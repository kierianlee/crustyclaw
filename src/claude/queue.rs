use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::session::SessionManager;
use super::{invoke, ClaudeResponse, ResponseStatus};
use crate::common::config::DaemonConfig;
use crate::common::status::StatusTracker;
use crate::common::util;

// ---- Types owned by the queue ----

/// A request to invoke the Claude CLI, submitted to the invocation queue.
pub struct ClaudeRequest {
    pub prompt: String,
    pub origin: RequestOrigin,
    pub response_tx: tokio::sync::oneshot::Sender<ClaudeResponse>,
    /// Optional file to clean up after the subprocess finishes.
    /// Owned by the queue worker so cleanup is safe even if the caller times out.
    pub cleanup_path: Option<PathBuf>,
}

/// Where a request originated — used for logging and response routing.
pub enum RequestOrigin {
    Telegram { chat_id: i64 },
    Scheduler { job_name: String },
    Heartbeat,
    InternalScheduleParse,
}

impl fmt::Display for RequestOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Telegram { chat_id } => write!(f, "telegram:{chat_id}"),
            Self::Scheduler { job_name } => write!(f, "scheduler:{job_name}"),
            Self::Heartbeat => write!(f, "heartbeat"),
            Self::InternalScheduleParse => write!(f, "internal:schedule-parse"),
        }
    }
}

// ---- Queue implementation ----

pub struct InvocationQueue {
    tx: std::sync::Mutex<Option<mpsc::Sender<ClaudeRequest>>>,
    depth: Arc<AtomicUsize>,
    status: Arc<StatusTracker>,
}

impl InvocationQueue {
    /// Spawn the queue with a single worker that processes requests serially.
    ///
    /// Returns `(queue, worker_handle)`. The caller should `tokio::select!` on
    /// the worker handle to detect unexpected exits.
    pub fn spawn(
        config: Arc<DaemonConfig>,
        session: Arc<SessionManager>,
        data_dir: Arc<Path>,
        status: Arc<StatusTracker>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<ClaudeRequest>(32);
        let depth = Arc::new(AtomicUsize::new(0));

        let handle = tokio::spawn(queue_worker(
            rx,
            config,
            session,
            data_dir,
            depth.clone(),
            status.clone(),
        ));

        let queue = Self {
            tx: std::sync::Mutex::new(Some(tx)),
            depth,
            status,
        };

        (queue, handle)
    }

    /// Submit a prompt for Claude processing. Returns the response.
    pub async fn submit(
        &self,
        prompt: String,
        origin: RequestOrigin,
        cleanup_path: Option<PathBuf>,
    ) -> Result<ClaudeResponse> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = ClaudeRequest {
            prompt,
            origin,
            response_tx,
            cleanup_path,
        };

        {
            let guard = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(tx) => {
                    // Increment *before* send so the worker can never
                    // decrement before we increment, which would
                    // transiently wrap the usize counter.
                    let prev = self.depth.fetch_add(1, Ordering::Relaxed);
                    self.status.update_queue_depth(prev + 1);
                    match tx.try_send(request) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(req)) => {
                            let prev = self.depth.fetch_sub(1, Ordering::Relaxed);
                            self.status.update_queue_depth(prev.saturating_sub(1));
                            self.status.record_queue_full();
                            cleanup_path_background(req.cleanup_path);
                            anyhow::bail!("Queue is full — try again later");
                        }
                        Err(mpsc::error::TrySendError::Closed(req)) => {
                            let prev = self.depth.fetch_sub(1, Ordering::Relaxed);
                            self.status.update_queue_depth(prev.saturating_sub(1));
                            cleanup_path_background(req.cleanup_path);
                            anyhow::bail!("Queue closed");
                        }
                    }
                }
                None => {
                    cleanup_path_background(request.cleanup_path);
                    anyhow::bail!("Queue is shut down");
                }
            }
        }

        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("Worker dropped response channel (possible panic in request handler)"))
    }

    /// Current number of queued + in-flight requests.
    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    /// Stop accepting new requests. The worker will drain remaining items
    /// and exit once the channel closes.
    pub fn close(&self) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// Spawn a background task to remove a cleanup file. Used on submit error
/// paths where the request never entered the queue, so the worker will never
/// clean it up. Avoids blocking the async runtime with synchronous I/O.
fn cleanup_path_background(path: Option<PathBuf>) {
    if let Some(path) = path {
        tokio::spawn(async move {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                tracing::warn!(path = %path.display(), error = %e, "Failed to clean up file after submit error");
            }
        });
    }
}

async fn queue_worker(
    mut rx: mpsc::Receiver<ClaudeRequest>,
    config: Arc<DaemonConfig>,
    session: Arc<SessionManager>,
    data_dir: Arc<Path>,
    depth: Arc<AtomicUsize>,
    status: Arc<StatusTracker>,
) {
    while let Some(request) = rx.recv().await {
        // Skip requests whose callers have already timed out to avoid
        // wasting API calls on results nobody will receive.
        if request.response_tx.is_closed() {
            tracing::info!(origin = %request.origin, "Skipping cancelled request (caller timed out)");
            if let Some(path) = request.cleanup_path {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    tracing::warn!(path = %path.display(), error = %e, "Failed to clean up cancelled request file");
                }
            }
            let prev = depth.fetch_sub(1, Ordering::Relaxed);
            status.update_queue_depth(prev.saturating_sub(1));
            continue;
        }

        // Run the request in a spawned task so that a panic in invoke() or
        // session handling is caught by the JoinHandle instead of killing the
        // entire worker loop.  We .await immediately to preserve serial
        // execution — only one request is processed at a time.
        let config = config.clone();
        let session = session.clone();
        let data_dir = data_dir.clone();
        let status_clone = status.clone();
        let handle = tokio::spawn(async move {
            process_request(request, &config, &session, &data_dir, &status_clone).await
        });

        match handle.await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!(error = %e, "Request handler panicked — worker continuing");
            }
        }

        let prev = depth.fetch_sub(1, Ordering::Relaxed);
        status.update_queue_depth(prev.saturating_sub(1));
    }

    tracing::warn!("Queue worker exiting — channel closed");
}

/// Process a single queued request: invoke Claude, record the result, and
/// send the response back to the caller.
///
/// Extracted so it can run inside `tokio::spawn` for panic isolation.
async fn process_request(
    request: ClaudeRequest,
    config: &DaemonConfig,
    session: &SessionManager,
    data_dir: &Path,
    status: &StatusTracker,
) {
    tracing::info!(origin = %request.origin, "Processing request");

    let working_dir = config.effective_working_dir(data_dir);

    let is_internal_parse = matches!(request.origin, RequestOrigin::InternalScheduleParse);

    let (response, invocation_count) = if is_internal_parse {
        invoke_internal_no_session(config, &request.prompt, &working_dir).await
    } else {
        invoke_with_retries(config, session, &request.prompt, &working_dir).await
    };

    if !is_internal_parse {
        if let Err(e) = session
            .record_invocations(response.session_id, invocation_count)
            .await
        {
            tracing::error!(error = %e, "Failed to record/persist session state");
        }
        let error_msg = if response.status == ResponseStatus::Success {
            None
        } else {
            Some(response.text.as_str())
        };
        status.record_invocations(invocation_count, error_msg);
        if invocation_count == 0 {
            if let Some(msg) = error_msg {
                status.record_error(msg);
            }
        }
        if let Some(sid) = response.session_id {
            status.update_session(Some(util::short_id(sid)));
        }
    }

    tracing::info!(
        origin = %request.origin,
        duration_ms = response.duration_ms,
        cost_usd = response.cost_usd,
        error = (response.status != ResponseStatus::Success),
        "Request complete"
    );

    let _ = request.response_tx.send(response);

    // Clean up after the subprocess finishes, so the file isn't deleted
    // while Claude is still reading it (e.g. if the caller timed out).
    if let Some(path) = request.cleanup_path {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!(path = %path.display(), error = %e, "Failed to clean up request file");
        }
    }
}

// ---------------------------------------------------------------------------
// Retry state machine
// ---------------------------------------------------------------------------

/// Invoke Claude without using or mutating persisted session state.
///
/// Used for internal helper prompts (e.g. schedule parsing) that should not
/// pollute user conversation history.
async fn invoke_internal_no_session(
    config: &DaemonConfig,
    prompt: &str,
    working_dir: &Path,
) -> (ClaudeResponse, u64) {
    let timeout = config.subprocess_timeout_secs;
    match invoke(config, None, prompt, working_dir, timeout).await {
        Ok(response) => (response, 1),
        Err(e) => {
            tracing::error!(error = %e, "Internal Claude invocation failed");
            (ClaudeResponse::error(e.to_string()), 0)
        }
    }
}

/// Invoke Claude with automatic retry for rate-limiting and session expiry.
///
/// Retry policy (at most 2 retries total):
///   1. `SessionExpired` → reset session, retry with no session ID.
///   2. `RateLimited`    → sleep, retry with the same session ID.
///
/// Returns `(response, invocation_count)`.
async fn invoke_with_retries(
    config: &DaemonConfig,
    session: &SessionManager,
    prompt: &str,
    working_dir: &Path,
) -> (ClaudeResponse, u64) {
    let mut session_id = session.session_id().await;
    let timeout = config.subprocess_timeout_secs;
    let mut invocation_count: u64 = 0;
    let mut retries_left: u8 = 2;

    let mut response = match invoke(config, session_id, prompt, working_dir, timeout).await {
        Ok(r) => {
            invocation_count += 1;
            r
        }
        Err(e) => {
            tracing::error!(error = %e, "Claude invocation failed");
            return (ClaudeResponse::error(e.to_string()), invocation_count);
        }
    };

    loop {
        match response.status {
            ResponseStatus::Success | ResponseStatus::Error => break,
            ResponseStatus::SessionExpired if retries_left > 0 => {
                retries_left -= 1;
                let old = session_id.map_or_else(|| "none".into(), |id| id.to_string());
                tracing::warn!(old_session = %old, "Session expired, resetting and retrying");

                match session.reset().await {
                    Ok(()) => {
                        session_id = None;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to reset session");
                        response =
                            ClaudeResponse::error(format!("Session expired and reset failed: {e}"));
                        break;
                    }
                }
            }
            ResponseStatus::RateLimited if retries_left > 0 => {
                retries_left -= 1;
                // Add 0–999 ms of spread based on current time. Not truly random,
                // but sufficient for a single-worker queue to avoid retry
                // collisions with other processes.
                let spread_ms = u64::from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_millis(),
                );
                let sleep_ms = config
                    .rate_limit_retry_secs
                    .saturating_mul(1_000)
                    .saturating_add(spread_ms);
                tracing::warn!(retry_in_ms = sleep_ms, "Rate limited, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
            }
            // Exhausted retries or terminal transient status.
            ResponseStatus::SessionExpired => {
                response = ClaudeResponse::error("Session expired and retries exhausted");
                break;
            }
            ResponseStatus::RateLimited => {
                response =
                    ClaudeResponse::error("Still rate limited after retry — try again later");
                break;
            }
        }

        // Retry the invocation.
        response = match invoke(config, session_id, prompt, working_dir, timeout).await {
            Ok(r) => {
                invocation_count += 1;
                r
            }
            Err(e) => {
                tracing::error!(error = %e, "Claude invocation failed on retry");
                return (ClaudeResponse::error(e.to_string()), invocation_count);
            }
        };

        if matches!(
            response.status,
            ResponseStatus::Success | ResponseStatus::Error
        ) {
            break;
        }
    }

    (response, invocation_count)
}
