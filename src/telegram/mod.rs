pub mod transcribe;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use teloxide::net::Download;
use teloxide::payloads::{GetUpdatesSetters, SendMessageSetters};
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ParseMode, PhotoSize, UpdateKind, Voice};
use tokio::io::AsyncWriteExt;
use tokio::task::{JoinHandle, JoinSet};

use crate::claude::{InvocationQueue, RequestOrigin, ResponseStatus, SessionManager};
use crate::common::chatlog::{ChatDirection, ChatLog};
use crate::common::config::DaemonConfig;
use crate::common::util::{short_id, truncate_str};
#[cfg(unix)]
use crate::permission::PermissionServer;
use crate::scheduler::{JobAction, Scheduler};

/// RAII guard that aborts a spawned task on drop, preventing leaks if the
/// caller panics or returns early.
struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Permission server handle — only available on Unix (requires Unix sockets).
#[cfg(unix)]
pub(crate) type OptionalPermissionServer = Option<Arc<PermissionServer>>;
#[cfg(not(unix))]
pub(crate) type OptionalPermissionServer = Option<()>;

/// Long-polling timeout sent to Telegram (seconds).
const POLL_TIMEOUT_SECS: u32 = 30;

/// Max concurrent message handler tasks.
const MAX_CONCURRENT_HANDLERS: usize = 10;

/// Max total handler tasks (active + waiting for semaphore).
/// Prevents unbounded JoinSet growth during sustained bursts.
const MAX_TOTAL_HANDLERS: usize = 32;

/// Max concurrent callback-query handler tasks.  Separate from the message
/// semaphore to avoid a deadlock where all message slots are occupied waiting
/// for permission-approval callbacks.  Set generously since callbacks are
/// lightweight and short-lived.
const MAX_CONCURRENT_CALLBACKS: usize = 50;

/// Max consecutive failures before rebuilding the HTTP client.
const MAX_ERRORS_BEFORE_RECONNECT: u32 = 5;

/// Maximum prompt size in bytes. Prompts exceeding this are truncated
/// to prevent excessive memory use in the Claude subprocess.
const MAX_PROMPT_BYTES: usize = 32 * 1024;

/// Upper bound for downloaded Telegram photo size.
const MAX_PHOTO_BYTES: u32 = 20 * 1024 * 1024;
/// Upper bound for downloaded Telegram voice-note size.
const MAX_VOICE_BYTES: u32 = 20 * 1024 * 1024;
/// Upper bound for Telegram voice-note duration in seconds.
const MAX_VOICE_DURATION_SECS: u32 = 10 * 60;
/// Max time allowed for one schedule parsing attempt.
const SCHEDULE_PARSE_TIMEOUT_SECS: u64 = 90;

/// Create a Bot with an HTTP client whose timeout exceeds the long-polling timeout.
///
/// teloxide's default reqwest client has a 17 s timeout, but long-polling holds
/// the connection for `POLL_TIMEOUT_SECS`. We set the HTTP timeout to
/// `POLL_TIMEOUT_SECS + 15 s` so the client never kills a healthy long-poll.
pub(crate) fn make_bot(token: &str) -> anyhow::Result<Bot> {
    let client = teloxide::net::default_reqwest_settings()
        .timeout(Duration::from_secs(u64::from(POLL_TIMEOUT_SECS) + 15))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;
    Ok(Bot::with_client(token, client))
}

/// Spawn the Telegram long-polling loop.
pub fn spawn(
    config: Arc<DaemonConfig>,
    queue: Arc<InvocationQueue>,
    session: Arc<SessionManager>,
    scheduler: Arc<Scheduler>,
    data_dir: PathBuf,
    shared_bot: Arc<tokio::sync::RwLock<Bot>>,
    permission_server: OptionalPermissionServer,
    chat_log: Arc<ChatLog>,
) -> JoinHandle<()> {
    tokio::spawn(poll_loop(
        config,
        queue,
        session,
        scheduler,
        data_dir,
        shared_bot,
        permission_server,
        chat_log,
    ))
}

#[allow(clippy::too_many_lines)]
async fn poll_loop(
    config: Arc<DaemonConfig>,
    queue: Arc<InvocationQueue>,
    session: Arc<SessionManager>,
    scheduler: Arc<Scheduler>,
    data_dir: PathBuf,
    shared_bot: Arc<tokio::sync::RwLock<Bot>>,
    permission_server: OptionalPermissionServer,
    chat_log: Arc<ChatLog>,
) {
    const MAX_BACKOFF: u64 = 120;

    let token = &config.telegram_token;
    let admin_chat_id = config.admin_chat_id;

    let mut bot = shared_bot.read().await.clone();

    delete_webhook(&bot).await;

    match bot.get_me().await {
        Ok(me) => {
            let username = me.username.as_deref().unwrap_or("unknown");
            tracing::info!(bot = %username, "Telegram bot connected");
        }
        Err(e) => tracing::error!(error = %e, "Failed to get bot info"),
    }

    // Drain backlogged updates so messages sent while the daemon was offline
    // don't cause a burst of Claude invocations on startup.
    let mut offset: i32 = 0;
    match drain_stale_updates(&bot).await {
        Ok(new_offset) => {
            if new_offset != 0 {
                tracing::info!(offset = new_offset, "Drained stale Telegram updates");
            }
            offset = new_offset;
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to drain stale updates, starting from offset 0")
        }
    }

    if let Err(e) = send_text(&bot, admin_chat_id, "crustyclaw daemon started.").await {
        tracing::warn!(error = %e, "Failed to send startup message to admin");
    }

    let handler_state = Arc::new(HandlerState {
        config: config.clone(),
        queue: queue.clone(),
        session: session.clone(),
        scheduler: scheduler.clone(),
        data_dir: data_dir.clone(),
        chat_log,
    });
    let handler_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HANDLERS));
    let callback_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CALLBACKS));
    let mut handlers = JoinSet::new();
    let mut backoff_secs: u64 = 1;
    let mut consecutive_errors: u32 = 0;

    loop {
        // Reap completed handler tasks to free memory.
        while let Some(result) = handlers.try_join_next() {
            if let Err(e) = result {
                tracing::error!(error = %e, "Message handler panicked");
            }
        }

        let result = bot
            .get_updates()
            .offset(offset)
            .timeout(POLL_TIMEOUT_SECS)
            .await;

        match result {
            Ok(updates) => {
                if consecutive_errors > 0 {
                    tracing::info!(
                        after_errors = consecutive_errors,
                        "Telegram connection restored"
                    );
                }
                backoff_secs = 1;
                consecutive_errors = 0;

                for update in updates {
                    offset = next_offset(update.id.0);

                    match update.kind {
                        UpdateKind::Message(msg) => {
                            if handlers.len() >= MAX_TOTAL_HANDLERS {
                                tracing::warn!("Handler backlog full ({MAX_TOTAL_HANDLERS}), dropping message");
                                continue;
                            }
                            let state = handler_state.clone();
                            let bot = bot.clone();

                            // Acquire the semaphore *inside* the spawned task so the
                            // poll loop is never blocked.  This prevents a deadlock
                            // where all handler slots are occupied waiting for queue
                            // responses while a permission-approval CallbackQuery is
                            // stuck because the poll loop can't make progress.
                            let sem = handler_semaphore.clone();
                            handlers.spawn(async move {
                                let _permit = sem
                                    .acquire_owned()
                                    .await
                                    .expect("handler semaphore is never closed");
                                let ctx = HandlerCtx {
                                    bot: &bot,
                                    state: &state,
                                };
                                handle_message(&ctx, &msg).await;
                            });
                        }
                        UpdateKind::CallbackQuery(query) => {
                            // Callback queries (e.g. permission approval buttons)
                            // must not be gated by the *message* handler semaphore —
                            // they need to proceed even when all message slots are
                            // full, otherwise a permission-approval deadlock occurs.
                            // A separate, higher-capacity semaphore bounds task count.
                            let bot = bot.clone();
                            let cb_sem = callback_semaphore.clone();
                            #[cfg(unix)]
                            let perm = permission_server.clone();
                            handlers.spawn(async move {
                                let _permit = cb_sem.acquire_owned().await
                                    .expect("callback semaphore is never closed");
                                #[cfg(unix)]
                                if let Some(ref perm) = perm {
                                    perm.handle_callback(&bot, &query).await;
                                    return;
                                }
                                let _ = bot.answer_callback_query(query.id.clone()).await;
                            });
                        }
                        UpdateKind::EditedMessage(_msg) => {
                            // Intentionally ignored: edited messages should not
                            // retrigger Claude requests or command handling.
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;

                if matches!(
                    e,
                    teloxide::RequestError::Api(teloxide::ApiError::TerminatedByOtherGetUpdates)
                ) {
                    tracing::warn!(
                        consecutive_errors,
                        "Telegram poll conflict (another instance running?), \
                         retrying in {backoff_secs}s"
                    );
                } else {
                    tracing::error!(
                        error = %e,
                        retry_in = backoff_secs,
                        consecutive_errors,
                        "Telegram poll error"
                    );
                }

                if consecutive_errors >= MAX_ERRORS_BEFORE_RECONNECT {
                    tracing::warn!(consecutive_errors, "Rebuilding Telegram HTTP client");
                    match make_bot(token) {
                        Ok(new_bot) => {
                            *shared_bot.write().await = new_bot.clone();
                            bot = new_bot;
                            delete_webhook(&bot).await;
                            consecutive_errors = 0;
                            backoff_secs = 1;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to rebuild HTTP client");
                        }
                    }
                }

                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Shared handler state wrapped in a single `Arc` so per-message dispatch
/// needs only one `Arc::clone` instead of cloning each field individually.
struct HandlerState {
    config: Arc<DaemonConfig>,
    queue: Arc<InvocationQueue>,
    session: Arc<SessionManager>,
    scheduler: Arc<Scheduler>,
    data_dir: PathBuf,
    chat_log: Arc<ChatLog>,
}

/// Shared context for message and command handlers, avoiding repeated parameter lists.
struct HandlerCtx<'a> {
    bot: &'a Bot,
    state: &'a HandlerState,
}

/// Build (prompt, `cleanup_path`) from a message. Returns `None` if the message
/// is unsupported, empty, or on error (caller has already notified the user).
async fn build_prompt_from_message(
    bot: &Bot,
    msg: &Message,
    config: &DaemonConfig,
    data_dir: &Path,
    chat_id: i64,
) -> Option<(String, Option<PathBuf>)> {
    if let Some(photo_sizes) = msg.photo() {
        let inbox_dir = data_dir.join("inbox");
        match download_photo(bot, photo_sizes, &inbox_dir).await {
            Ok(path) => {
                let caption = msg.caption().unwrap_or("Describe this image");
                let prompt = format!(
                    "The user sent an image saved at: {}\n\n{caption}",
                    path.display(),
                );
                Some((prompt, Some(path)))
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to download photo");
                let _ = send_text(bot, chat_id, &format!("Failed to download image: {e}")).await;
                None
            }
        }
    } else if let Some(voice) = msg.voice() {
        if !config.voice_enabled {
            let _ = send_text(bot, chat_id, "Voice messages are not enabled.").await;
            return None;
        }
        let inbox_dir = data_dir.join("inbox");
        match download_voice(bot, voice, &inbox_dir).await {
            Ok(ogg_path) => {
                let Some(model_path) = config.whisper_model_path.as_deref() else {
                    let _ = send_text(
                        bot,
                        chat_id,
                        "Voice enabled but whisper_model_path not configured.",
                    )
                    .await;
                    let _ = tokio::fs::remove_file(&ogg_path).await;
                    return None;
                };
                match self::transcribe::transcribe(&ogg_path, model_path).await {
                    Ok(text) => {
                        tracing::info!(
                            duration_secs = voice.duration.seconds(),
                            chars = text.len(),
                            "Voice transcribed"
                        );
                        let prompt = format!("[Voice message transcription]\n\n{text}");
                        Some((prompt, Some(ogg_path)))
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Voice transcription failed");
                        let _ =
                            send_text(bot, chat_id, &format!("Transcription failed: {e}")).await;
                        let _ = tokio::fs::remove_file(&ogg_path).await;
                        None
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to download voice");
                let _ = send_text(bot, chat_id, &format!("Failed to download voice: {e}")).await;
                None
            }
        }
    } else if let Some(text) = msg.text() {
        Some((text.to_string(), None))
    } else {
        let _ = send_text(
            bot,
            chat_id,
            "Unsupported message type. Send text, a photo, or a voice message.",
        )
        .await;
        None
    }
}

async fn handle_message(ctx: &HandlerCtx<'_>, msg: &Message) {
    let bot = ctx.bot;
    let config = &ctx.state.config;
    let queue = &ctx.state.queue;
    let data_dir = &ctx.state.data_dir;
    let chat_id = msg.chat.id.0;
    let from = msg
        .from
        .as_ref()
        .map_or("unknown", |u| u.first_name.as_str());

    if !config.is_chat_allowed(chat_id) {
        tracing::warn!(chat_id, from, "Rejected message from unauthorized chat");
        return;
    }

    let Some((prompt, cleanup_path)) =
        build_prompt_from_message(bot, msg, config, data_dir, chat_id).await
    else {
        return;
    };

    if prompt.starts_with('/') {
        // Admin-only commands are bound to the admin's private chat ID.
        // In groups/supergroups chat_id is negative, so admin commands are
        // intentionally DM-only even if the group is in allowed_chat_ids.
        let is_admin = chat_id == config.admin_chat_id;
        handle_command(ctx, chat_id, is_admin, &prompt).await;
        if let Some(path) = cleanup_path {
            cleanup_inbox_file(path).await;
        }
        return;
    }

    // Build the tagged prompt first, then truncate, so the prefix is
    // included in the byte budget and we never overshoot MAX_PROMPT_BYTES.
    let tagged_prompt = format!("[Telegram chat_id={chat_id}] {prompt}");

    let tagged_prompt = if tagged_prompt.len() > MAX_PROMPT_BYTES {
        let original_len = tagged_prompt.len();
        let truncated = truncate_str(&tagged_prompt, MAX_PROMPT_BYTES).to_string();
        let _ = send_text(
            bot,
            chat_id,
            &format!("Message truncated ({original_len} → {MAX_PROMPT_BYTES} bytes)."),
        )
        .await;
        truncated
    } else {
        tagged_prompt
    };

    tracing::info!(
        chat_id,
        from,
        text = truncate_str(&tagged_prompt, 80),
        "Telegram message"
    );

    ctx.state
        .chat_log
        .push(ChatDirection::Incoming, chat_id, prompt.clone());

    // Keep "typing…" visible while Claude is working.
    let _ = bot
        .send_chat_action(ChatId(chat_id), ChatAction::Typing)
        .await;
    let typing_bot = bot.clone();
    let typing_guard = AbortOnDrop(tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(4)).await;
            let _ = typing_bot
                .send_chat_action(ChatId(chat_id), ChatAction::Typing)
                .await;
        }
    }));

    let depth = queue.depth();
    if depth > 0 {
        let _ = send_text(
            bot,
            chat_id,
            &format!("{depth} request(s) ahead — your request is queued."),
        )
        .await;
    }

    let origin = RequestOrigin::Telegram { chat_id };
    // Timeout covers queue wait + subprocess execution. Without this, a
    // handler could hold a semaphore permit indefinitely if the queue is
    // deep and each invocation takes the full subprocess timeout.
    // Worst case: all MAX_CONCURRENT_HANDLERS slots are occupied, each waiting
    // for the single queue worker to finish the one ahead of it. The timeout
    // must cover that many sequential subprocess durations, not just one.
    let submit_timeout = Duration::from_secs(
        config
            .subprocess_timeout_secs
            .saturating_mul(MAX_CONCURRENT_HANDLERS as u64)
            .saturating_add(60),
    );
    // Pass cleanup_path to the queue so the worker deletes the file after the
    // subprocess finishes — even if this handler times out waiting for a response.
    let result = tokio::time::timeout(
        submit_timeout,
        queue.submit(tagged_prompt, origin, cleanup_path),
    )
    .await;
    drop(typing_guard);

    match result {
        Ok(Ok(response)) => {
            let output = response.into_display_text();
            ctx.state
                .chat_log
                .push(ChatDirection::Outgoing, chat_id, output.clone());
            send_chunked(bot, chat_id, &output).await;
        }
        Ok(Err(e)) => {
            let msg = format!("Error: {e}");
            ctx.state
                .chat_log
                .push(ChatDirection::Outgoing, chat_id, msg.clone());
            let _ = send_text(bot, chat_id, &msg).await;
        }
        Err(_) => {
            let msg = "Request timed out waiting for a response. Try again later.";
            ctx.state
                .chat_log
                .push(ChatDirection::Outgoing, chat_id, msg.to_string());
            let _ = send_text(bot, chat_id, msg).await;
        }
    }
}

async fn cleanup_inbox_file(path: PathBuf) {
    if let Err(e) = tokio::fs::remove_file(&path).await {
        tracing::warn!(path = %path.display(), error = %e, "Failed to clean up inbox file");
    }
}

async fn handle_command(ctx: &HandlerCtx<'_>, chat_id: i64, is_admin: bool, text: &str) {
    let bot = ctx.bot;
    let queue = &ctx.state.queue;
    let session = &ctx.state.session;
    let scheduler = &ctx.state.scheduler;
    // Strip @botname suffix from commands like /start@mybotname.
    let cmd = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("");

    // Admin-only commands: /stop, /reset, /schedule, /jobs, /unschedule.
    if matches!(cmd, "/stop" | "/reset" | "/schedule" | "/jobs" | "/unschedule") && !is_admin {
        let _ = send_text(bot, chat_id, "This command is restricted to the admin.").await;
        return;
    }

    match cmd {
        "/start" => {
            let _ = send_text(
                bot,
                chat_id,
                "crustyclaw daemon active. Send a message to chat with Claude.",
            )
            .await;
        }
        "/stop" => {
            let _ = send_text(bot, chat_id, "Shutting down...").await;
            // Send SIGTERM to ourselves for a graceful shutdown.
            #[cfg(unix)]
            {
                unsafe { libc::kill(libc::getpid(), libc::SIGTERM); }
            }
            #[cfg(not(unix))]
            {
                let _ = send_text(bot, chat_id, "Remote stop not supported on this platform.").await;
            }
        }
        "/status" => {
            let state = session.snapshot().await;
            let age = chrono::Utc::now() - state.created_at;
            let sid = state
                .session_id
                .map_or_else(|| "none (fresh)".into(), short_id);
            let status = format!(
                "Session: {sid}\n\
                 Created: {}\n\
                 Invocations: {}\n\
                 Queue depth: {}\n\
                 Session age: {}d {}h",
                state.created_at.format("%Y-%m-%d %H:%M UTC"),
                state.invocation_count,
                queue.depth(),
                age.num_days(),
                age.num_hours() % 24,
            );
            let _ = send_text(bot, chat_id, &status).await;
        }
        "/reset" => match session.reset().await {
            Ok(()) => {
                let _ = send_text(
                    bot,
                    chat_id,
                    "Session reset. Next message starts a fresh conversation.",
                )
                .await;
            }
            Err(e) => {
                let _ = send_text(bot, chat_id, &format!("Failed to reset: {e}")).await;
            }
        },
        "/schedule" => {
            handle_schedule(bot, chat_id, text, queue, scheduler).await;
        }
        "/jobs" => {
            handle_jobs(bot, chat_id, scheduler).await;
        }
        "/unschedule" => {
            handle_unschedule(bot, chat_id, text, scheduler).await;
        }
        _ => {
            let _ = send_text(
                bot,
                chat_id,
                "Unknown command. Available: /start, /stop, /status, /reset, /schedule, /jobs, /unschedule",
            )
            .await;
        }
    }
}

/// `/schedule <description>` — create a recurring Claude prompt job.
///
/// The entire description is sent to Claude which extracts the job name,
/// schedule (as a cron expression), and prompt. This allows fully natural
/// input like: `/schedule every morning at 9am, summarize the top news`
async fn handle_schedule(
    bot: &Bot,
    chat_id: i64,
    text: &str,
    queue: &InvocationQueue,
    scheduler: &Arc<Scheduler>,
) {
    let description = text
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest)
        .trim();

    if description.is_empty() {
        let _ = send_text(
            bot,
            chat_id,
            "Usage: /schedule <description>\n\
             Example: /schedule every morning at 9am, summarize the top news",
        )
        .await;
        return;
    }

    let _ = send_text(bot, chat_id, "Parsing schedule...").await;

    let parsed = match parse_schedule_with_claude(description, queue, bot, chat_id).await {
        Ok(p) => p,
        Err(e) => {
            let _ = send_text(bot, chat_id, &format!("Failed to parse schedule: {e}")).await;
            return;
        }
    };

    // Validate cron expression: only allow characters that can appear in a
    // valid cron expression (digits, separators, wildcards, alpha for month/dow
    // names, and whitespace). Reject control characters, shell metacharacters,
    // etc. to prevent injection if the downstream parser is lenient.
    if !parsed
        .cron
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '*' | '/' | '-' | ',' | '?'))
    {
        let _ = send_text(
            bot,
            chat_id,
            "Schedule rejected — cron expression contains invalid characters.\n\
             Try rephrasing your schedule description.",
        )
        .await;
        return;
    }

    // Must be 7 fields (tokio-cron-scheduler format:
    // sec min hour dom month dow year) with seconds == "0" to prevent sub-minute firing.
    let cron_fields: Vec<&str> = parsed.cron.split_whitespace().collect();
    if cron_fields.len() != 7 {
        let _ = send_text(
            bot,
            chat_id,
            &format!(
                "Schedule rejected — expected 7-field cron expression (sec min hour dom month dow year), \
                 got {} fields.\nTry rephrasing your schedule description.",
                cron_fields.len(),
            ),
        )
        .await;
        return;
    }
    if cron_fields[0] != "0" {
        let _ = send_text(
            bot,
            chat_id,
            "Schedule rejected — seconds field must be '0' (fire at most once per minute).\n\
             Try rephrasing your schedule description.",
        )
        .await;
        return;
    }
    if cron_fires_too_often(cron_fields[1]) {
        let _ = send_text(
            bot,
            chat_id,
            "Schedule rejected — fires too frequently (minimum interval is 5 minutes).\n\
             Try rephrasing your schedule description.",
        )
        .await;
        return;
    }

    let prompt_preview = truncate_str(&parsed.prompt, 100).to_string();

    let action = JobAction::ClaudePrompt {
        prompt: parsed.prompt,
        chat_id,
    };

    match scheduler
        .add_job(parsed.name.clone(), parsed.cron.clone(), action, false)
        .await
    {
        Ok(id) => {
            let short_id = short_id(id);
            let _ = send_text(
                bot,
                chat_id,
                &format!(
                    "Scheduled '{name}' ({short_id})\nCron: {cron}\nPrompt: {prompt_preview}",
                    name = parsed.name,
                    cron = parsed.cron,
                ),
            )
            .await;
        }
        Err(e) => {
            let msg = format!(
                "Failed to schedule: {e}\n\
                 (Cron expression from Claude: `{}`)\n\
                 Try rephrasing your schedule description.",
                parsed.cron,
            );
            let _ = send_text(bot, chat_id, &msg).await;
        }
    }
}

/// Check if a cron minute field would fire more often than every 5 minutes.
///
/// Rejects `*` (every minute) and `*/N` where N < 5. Comma-separated lists
/// with more than 12 entries are also rejected. This catches the common
/// mistake patterns without fully parsing cron arithmetic.
fn cron_fires_too_often(minute_field: &str) -> bool {
    if minute_field == "*" {
        return true;
    }
    if let Some(step) = minute_field.strip_prefix("*/") {
        return step.parse::<u32>().map_or(false, |n| n < 5);
    }
    // Comma-separated list: reject if more than 12 values (avg < 5 min apart).
    if minute_field.contains(',') {
        return minute_field.split(',').count() > 12;
    }
    false
}

struct ParsedSchedule {
    name: String,
    cron: String,
    prompt: String,
}

/// Use Claude to extract name, cron, and prompt from a natural-language
/// schedule description.
///
/// Runs through the normal invocation queue so schedule parsing respects the
/// same serialization and backpressure limits as other requests.
/// Retries once on malformed output since Claude can occasionally deviate from
/// the expected format.
async fn parse_schedule_with_claude(
    description: &str,
    queue: &InvocationQueue,
    bot: &Bot,
    chat_id: i64,
) -> anyhow::Result<ParsedSchedule> {
    match try_parse_schedule(description, queue).await {
        Ok(parsed) => Ok(parsed),
        Err(first_err) => {
            tracing::warn!(error = %first_err, "Schedule parse attempt failed, retrying");
            let _ = send_text(bot, chat_id, "Parse failed, retrying...").await;
            try_parse_schedule(description, queue).await
        }
    }
}

async fn try_parse_schedule(
    description: &str,
    queue: &InvocationQueue,
) -> anyhow::Result<ParsedSchedule> {
    let sanitized = truncate_str(description, 500);
    let prompt = format!(
        "You are a schedule parser. Extract three fields from the user text below.\n\
         Reply with EXACTLY three lines, no other text:\n\
         NAME: <short-kebab-case-name>\n\
         CRON: <7-field cron expression for tokio-cron-scheduler: sec min hour dom month dow year>\n\
         Example CRON: 0 30 9 * * * * (every day at 9:30am)\n\
         PROMPT: <the prompt to send to Claude on each run>\n\n\
         User text:\n{sanitized}"
    );

    let response = tokio::time::timeout(
        Duration::from_secs(SCHEDULE_PARSE_TIMEOUT_SECS),
        queue.submit(prompt, RequestOrigin::InternalScheduleParse, None),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Schedule parsing timed out after {SCHEDULE_PARSE_TIMEOUT_SECS}s"))??;
    if response.status != ResponseStatus::Success {
        anyhow::bail!("Schedule parser failed: {}", response.into_display_text());
    }
    let result_text = response.text;
    parse_schedule_response(&result_text)
}

/// Case-insensitive prefix strip: if `line` starts with `prefix` (ASCII
/// case-insensitive), return the remainder; otherwise `None`.
fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    if line.len() >= prefix.len()
        && line.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

fn parse_schedule_response(result_text: &str) -> anyhow::Result<ParsedSchedule> {
    let mut name = None;
    let mut cron = None;
    let mut prompt_text = None;
    let mut collecting_prompt = false;

    for line in result_text.lines() {
        let trimmed = line.trim();
        // Accept only the first occurrence of NAME/CRON so that multiline
        // prompt text containing e.g. "NAME:" doesn't overwrite the real field.
        if name.is_none() {
            if let Some(v) = strip_prefix_ci(trimmed, "NAME:") {
                name = Some(v.trim().to_string());
                collecting_prompt = false;
                continue;
            }
        }
        if cron.is_none() {
            if let Some(v) = strip_prefix_ci(trimmed, "CRON:") {
                cron = Some(v.trim().to_string());
                collecting_prompt = false;
                continue;
            }
        }
        if prompt_text.is_none() {
            if let Some(v) = strip_prefix_ci(trimmed, "PROMPT:") {
                prompt_text = Some(v.trim_start().to_string());
                collecting_prompt = true;
                continue;
            }
        }
        if collecting_prompt {
            let p = prompt_text.get_or_insert_with(String::new);
            if !p.is_empty() {
                p.push('\n');
            }
            p.push_str(line.trim_start());
        }
    }

    // Trim trailing whitespace/blank lines from prompt (e.g. Claude commentary
    // that starts after a blank line at the end).
    if let Some(ref mut p) = prompt_text {
        let trimmed = p.trim_end().len();
        p.truncate(trimmed);
    }

    let name = name.ok_or_else(|| anyhow::anyhow!("Claude did not return a NAME field"))?;
    let cron = cron.ok_or_else(|| anyhow::anyhow!("Claude did not return a CRON field"))?;
    let prompt_text =
        prompt_text.ok_or_else(|| anyhow::anyhow!("Claude did not return a PROMPT field"))?;

    if name.is_empty() || cron.is_empty() || prompt_text.is_empty() {
        anyhow::bail!("Claude returned empty fields: name={name}, cron={cron}");
    }

    // Sanitize the job name: keep only alphanumeric, hyphens, and underscores
    // to prevent path traversal or other injection if the name is used in
    // filenames or shell contexts downstream. Truncate to a reasonable length.
    let name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if name.is_empty() {
        anyhow::bail!("Schedule name is empty after sanitization");
    }

    Ok(ParsedSchedule {
        name,
        cron,
        prompt: prompt_text,
    })
}

/// `/jobs` — list all scheduled jobs.
async fn handle_jobs(bot: &Bot, chat_id: i64, scheduler: &Scheduler) {
    let jobs = scheduler.list_jobs().await;
    if jobs.is_empty() {
        let _ = send_text(bot, chat_id, "No scheduled jobs.").await;
        return;
    }

    let mut lines = Vec::with_capacity(jobs.len());
    for job in &jobs {
        let sid = short_id(job.stable_id);
        let action_desc = match &job.action {
            JobAction::ClaudePrompt { prompt, .. } => {
                format!("claude: {}", truncate_str(prompt, 60))
            }
            JobAction::TelegramMessage { text, .. } => {
                format!("msg: {}", truncate_str(text, 60))
            }
            JobAction::TelegramAdmin { text } => {
                format!("admin: {}", truncate_str(text, 60))
            }
        };
        lines.push(format!(
            "{} ({sid})\n  cron: {}\n  {action_desc}",
            job.name, job.cron_expression,
        ));
    }

    let _ = send_text(bot, chat_id, &lines.join("\n\n")).await;
}

/// `/unschedule <name-or-id>` — remove a scheduled job by name or UUID prefix.
async fn handle_unschedule(bot: &Bot, chat_id: i64, text: &str, scheduler: &Arc<Scheduler>) {
    let query = text
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest)
        .trim();

    if query.is_empty() {
        let _ = send_text(bot, chat_id, "Usage: /unschedule <name or id>").await;
        return;
    }

    let jobs = scheduler.list_jobs().await;
    let matches: Vec<_> = jobs
        .iter()
        .filter(|j| j.name == query || j.stable_id.to_string().starts_with(query))
        .collect();

    match matches.as_slice() {
        [job] => {
            let name = job.name.clone();
            let id = job.stable_id;
            match scheduler.remove_job(id).await {
                Ok(()) => {
                    let _ = send_text(bot, chat_id, &format!("Removed job '{name}'.")).await;
                }
                Err(e) => {
                    let _ =
                        send_text(bot, chat_id, &format!("Failed to remove '{name}': {e}")).await;
                }
            }
        }
        [] => {
            let _ = send_text(bot, chat_id, &format!("No job found matching '{query}'.")).await;
        }
        many => {
            let options = many
                .iter()
                .map(|j| format!("{} ({})", j.name, short_id(j.stable_id)))
                .collect::<Vec<_>>()
                .join("\n");
            let _ = send_text(
                bot,
                chat_id,
                &format!(
                    "Ambiguous job match '{query}'. Use a longer ID prefix or exact name:\n{options}"
                ),
            )
            .await;
        }
    }
}

/// Download the largest available photo from a Telegram message.
///
/// Returns the local file path where the image was saved.
async fn download_photo(
    bot: &Bot,
    photo_sizes: &[PhotoSize],
    inbox_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let photo = photo_sizes
        .last()
        .ok_or_else(|| anyhow::anyhow!("Empty photo array"))?;

    if photo.file.size > MAX_PHOTO_BYTES {
        anyhow::bail!(
            "Photo too large: {} bytes (limit: {} bytes)",
            photo.file.size,
            MAX_PHOTO_BYTES,
        );
    }

    let file = bot
        .get_file(photo.file.id.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get file info: {e}"))?;

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("{timestamp}_{}.jpg", photo.file.unique_id);
    let dest_path = inbox_dir.join(&filename);

    let mut dest_file = create_inbox_file(&dest_path).await?;
    if let Err(e) = bot.download_file(&file.path, &mut dest_file).await {
        drop(dest_file);
        let _ = tokio::fs::remove_file(&dest_path).await;
        return Err(anyhow::anyhow!("Failed to download photo: {e}"));
    }
    if let Err(e) = dest_file.shutdown().await {
        let _ = tokio::fs::remove_file(&dest_path).await;
        return Err(anyhow::anyhow!("Failed to flush downloaded photo: {e}"));
    }

    tracing::info!(path = %dest_path.display(), "Downloaded Telegram photo");
    Ok(dest_path)
}

/// Download a voice message from Telegram.
///
/// Returns the local file path where the .ogg file was saved.
async fn download_voice(bot: &Bot, voice: &Voice, inbox_dir: &Path) -> anyhow::Result<PathBuf> {
    if voice.file.size > MAX_VOICE_BYTES {
        anyhow::bail!(
            "Voice message too large: {} bytes (limit: {} bytes)",
            voice.file.size,
            MAX_VOICE_BYTES
        );
    }
    if voice.duration.seconds() > MAX_VOICE_DURATION_SECS {
        anyhow::bail!(
            "Voice message too long: {}s (limit: {}s)",
            voice.duration.seconds(),
            MAX_VOICE_DURATION_SECS
        );
    }

    let file = bot
        .get_file(voice.file.id.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get voice file info: {e}"))?;

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("{timestamp}_{}.ogg", voice.file.unique_id);
    let dest_path = inbox_dir.join(&filename);

    let mut dest_file = create_inbox_file(&dest_path).await?;
    if let Err(e) = bot.download_file(&file.path, &mut dest_file).await {
        drop(dest_file);
        let _ = tokio::fs::remove_file(&dest_path).await;
        return Err(anyhow::anyhow!("Failed to download voice: {e}"));
    }
    if let Err(e) = dest_file.shutdown().await {
        let _ = tokio::fs::remove_file(&dest_path).await;
        return Err(anyhow::anyhow!("Failed to flush downloaded voice: {e}"));
    }

    tracing::info!(path = %dest_path.display(), duration = voice.duration.seconds(), "Downloaded Telegram voice");
    Ok(dest_path)
}

/// Create an inbox file with restrictive permissions, creating the parent
/// directory if it does not yet exist.
///
/// On Unix, the directory is set to 0700 and files to 0600 so downloaded
/// media is not world-readable even under a permissive process umask.
async fn create_inbox_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await
            {
                tracing::warn!(path = %parent.display(), error = %e, "Failed to set inbox directory permissions to 0700");
            }
        }
    }
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(path).await
}

/// Compute the next getUpdates offset from a u32 update ID.
pub(crate) fn next_offset(update_id: u32) -> i32 {
    (update_id as i32).saturating_add(1)
}

pub(crate) async fn send_text(
    bot: &Bot,
    chat_id: i64,
    text: &str,
) -> Result<(), teloxide::RequestError> {
    bot.send_message(ChatId(chat_id), text).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Markdown → Telegram HTML conversion
// ---------------------------------------------------------------------------

fn push_html_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

/// Convert common Markdown patterns to Telegram-compatible HTML.
///
/// Handles fenced code blocks (` ``` `), inline code (`` ` ``), and bold (`**`).
/// All content is HTML-escaped. Unclosed markers are emitted literally so the
/// output is always valid HTML that Telegram will accept.
fn markdown_to_telegram_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Fenced code block: ```
        if i + 2 < len && bytes[i] == b'`' && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
            let open = i;
            i += 3;
            // Skip optional language tag until newline.
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            if i < len {
                i += 1;
            }
            let content_start = i;
            // Scan for closing ```.
            loop {
                if i + 2 < len
                    && bytes[i] == b'`'
                    && bytes[i + 1] == b'`'
                    && bytes[i + 2] == b'`'
                {
                    let content = &text[content_start..i];
                    let content = content.strip_suffix('\n').unwrap_or(content);
                    out.push_str("<pre>");
                    push_html_escaped(&mut out, content);
                    out.push_str("</pre>");
                    i += 3;
                    break;
                }
                if i >= len {
                    // Unclosed — emit the remaining text literally.
                    push_html_escaped(&mut out, &text[open..]);
                    return out;
                }
                i += 1;
            }
            continue;
        }

        // Inline code: `
        if bytes[i] == b'`' {
            i += 1;
            let content_start = i;
            while i < len && bytes[i] != b'`' && bytes[i] != b'\n' {
                i += 1;
            }
            if i < len && bytes[i] == b'`' {
                out.push_str("<code>");
                push_html_escaped(&mut out, &text[content_start..i]);
                out.push_str("</code>");
                i += 1;
            } else {
                // Unclosed or hit newline — emit the backtick literally.
                push_html_escaped(&mut out, "`");
                push_html_escaped(&mut out, &text[content_start..i]);
            }
            continue;
        }

        // Bold: **
        if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            i += 2;
            let content_start = i;
            let mut found = false;
            while i + 1 < len {
                if bytes[i] == b'*' && bytes[i + 1] == b'*' {
                    out.push_str("<b>");
                    push_html_escaped(&mut out, &text[content_start..i]);
                    out.push_str("</b>");
                    i += 2;
                    found = true;
                    break;
                }
                i += 1;
            }
            if !found {
                push_html_escaped(&mut out, "**");
                push_html_escaped(&mut out, &text[content_start..]);
                return out;
            }
            continue;
        }

        // Regular character — HTML-escape and advance.
        match bytes[i] {
            b'&' => out.push_str("&amp;"),
            b'<' => out.push_str("&lt;"),
            b'>' => out.push_str("&gt;"),
            _ => {
                let ch = text[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
        i += 1;
    }

    out
}

/// Drain all pending updates so the poll loop starts fresh.
///
/// Fetches the last pending update (offset=-1), then acknowledges everything
/// up to it. Returns the offset to use for the first real poll.
async fn drain_stale_updates(bot: &Bot) -> anyhow::Result<i32> {
    let last = bot
        .get_updates()
        .offset(-1)
        .limit(1)
        .timeout(0)
        .await
        .map_err(|e| anyhow::anyhow!("drain stale updates: {e}"))?;

    if let Some(update) = last.last() {
        // The first real poll (using this offset) implicitly acknowledges all
        // stale updates — no extra API round-trip needed here.
        Ok(next_offset(update.id.0))
    } else {
        Ok(0)
    }
}

/// Delete any active webhook so getUpdates works.
async fn delete_webhook(bot: &Bot) {
    if let Err(e) = bot.delete_webhook().await {
        tracing::warn!(error = %e, "Failed to delete webhook — getUpdates may fail");
    }
}

/// Telegram counts message length in UTF-16 code units.
const MAX_UTF16_UNITS: usize = 4096;
/// Continuation chunks get a "…\n" prefix (2 UTF-16 code units).
const CONTINUATION_OVERHEAD: usize = 2;

/// Compute byte-range boundaries for splitting `text` into Telegram-sized chunks.
///
/// Returns `(start, end)` pairs (byte offsets into `text`). Each chunk fits
/// within `max_utf16_units` UTF-16 code units, and splits prefer newline
/// boundaries for cleaner output. Continuation chunks (index > 0) reserve
/// `continuation_overhead` units for a "…\n" prefix inserted by the caller.
///
/// Extracted as a pure function so it can be unit-tested independently of the
/// Telegram bot client.
fn chunk_boundaries(
    text: &str,
    max_utf16_units: usize,
    continuation_overhead: usize,
) -> Vec<(usize, usize)> {
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let budget = if chunks.is_empty() {
            max_utf16_units
        } else {
            max_utf16_units.saturating_sub(continuation_overhead)
        };
        if budget == 0 {
            break;
        }
        let mut end = start;
        let mut utf16_count: usize = 0;

        for ch in text[start..].chars() {
            let units = ch.len_utf16();
            if utf16_count + units > budget {
                break;
            }
            utf16_count += units;
            end += ch.len_utf8();
        }

        if end == start {
            break;
        }

        let chunk_end = if end < text.len() {
            match text[start..end].rfind('\n') {
                // Skip pos == 0: splitting on a leading newline would produce
                // an empty chunk, so fall through to the hard break at `end`.
                Some(pos) if pos > 0 => start + pos + 1,
                _ => end,
            }
        } else {
            end
        };

        chunks.push((start, chunk_end));
        start = chunk_end;
    }

    chunks
}

/// Send a response, splitting into chunks that respect Telegram's 4096-character
/// limit (measured in UTF-16 code units, which is how Telegram counts).
///
/// Converts Markdown to Telegram HTML for nicer formatting (bold, code blocks).
/// Falls back to plain text per-chunk if the HTML version exceeds the size limit
/// or Telegram rejects it.
pub(crate) async fn send_chunked(bot: &Bot, chat_id: i64, text: &str) {
    if text.trim().is_empty() {
        let _ = send_text(bot, chat_id, "(empty response)").await;
        return;
    }

    let boundaries = chunk_boundaries(text, MAX_UTF16_UNITS, CONTINUATION_OVERHEAD);

    for (i, (start, end)) in boundaries.into_iter().enumerate() {
        // Brief pause between chunks to avoid hitting Telegram rate limits.
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let owned;
        let chunk: &str = if i > 0 {
            owned = format!("…\n{}", &text[start..end]);
            &owned
        } else {
            &text[start..end]
        };

        // Try sending as HTML for nicer formatting.
        let html = markdown_to_telegram_html(chunk);
        let html_fits = html.encode_utf16().count() <= MAX_UTF16_UNITS;

        let sent_html = html_fits
            && bot
                .send_message(ChatId(chat_id), &html)
                .parse_mode(ParseMode::Html)
                .await
                .is_ok();

        if sent_html {
            continue;
        }

        // HTML failed or too large — fall back to plain text.
        if let Err(e) = send_text(bot, chat_id, chunk).await {
            tracing::warn!(
                chat_id,
                chunk_start = start,
                total_len = text.len(),
                error = %e,
                "Failed to send message chunk — remaining response dropped"
            );
            let _ = send_text(bot, chat_id, "(message truncated due to send error)").await;
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(admin: i64, allowed: &[i64]) -> DaemonConfig {
        DaemonConfig {
            telegram_token: "test".into(),
            admin_chat_id: admin,
            allowed_chat_ids: allowed.iter().copied().collect(),
            ..Default::default()
        }
    }

    // -- is_chat_allowed --

    #[test]
    fn is_chat_allowed_admin() {
        let cfg = test_config(42, &[]);
        assert!(cfg.is_chat_allowed(42));
    }

    #[test]
    fn is_chat_allowed_in_allowlist() {
        let cfg = test_config(42, &[100, 200]);
        assert!(cfg.is_chat_allowed(100));
        assert!(cfg.is_chat_allowed(200));
    }

    #[test]
    fn is_chat_allowed_rejects_unknown() {
        let cfg = test_config(42, &[100]);
        assert!(!cfg.is_chat_allowed(999));
    }

    #[test]
    fn is_chat_allowed_admin_not_in_allowlist() {
        // Admin is allowed even if not in the explicit allowlist
        let cfg = test_config(42, &[100]);
        assert!(cfg.is_chat_allowed(42));
    }

    // -- chunk_boundaries --

    fn collect_chunks(text: &str, max: usize) -> Vec<&str> {
        chunk_boundaries(text, max, CONTINUATION_OVERHEAD)
            .into_iter()
            .map(|(s, e)| &text[s..e])
            .collect()
    }

    #[test]
    fn chunk_short_text_no_split() {
        let text = "hello world";
        let chunks = collect_chunks(text, 4096);
        assert_eq!(chunks, vec!["hello world"]);
    }

    #[test]
    fn chunk_splits_on_newline() {
        // Two logical lines each under the limit but together over it.
        let line = "a".repeat(3000);
        let text = format!("{line}\n{line}");
        let chunks = collect_chunks(&text, 4096);
        assert_eq!(chunks.len(), 2, "should split into 2 chunks");
        assert!(
            chunks[0].ends_with('\n'),
            "first chunk should end with newline"
        );
    }

    #[test]
    fn chunk_hard_break_when_no_newline() {
        // One long line with no newlines — must hard-break at budget.
        let text = "x".repeat(8000);
        let chunks = collect_chunks(&text, 4096);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4096);
        assert_eq!(chunks[1].len(), 8000 - 4096);
    }

    #[test]
    fn chunk_multibyte_chars_respected() {
        // '€' is 3 UTF-8 bytes but 1 UTF-16 unit — chunking must use UTF-16 counts.
        // budget=3: first chunk fits 3×'€' (3 units).
        // budget=3-CONTINUATION_OVERHEAD(2)=1: each subsequent chunk fits 1×'€'.
        let text = "€€€€€"; // 5 × '€', 15 UTF-8 bytes, 5 UTF-16 units
        let chunks = collect_chunks(text, 3);
        assert_eq!(chunks.len(), 3); // 3 + 1 + 1
        assert_eq!(chunks[0], "€€€");
        assert_eq!(chunks[1], "€");
        assert_eq!(chunks[2], "€");
    }

    #[test]
    fn chunk_empty_text_produces_no_chunks() {
        assert!(chunk_boundaries("", 4096, CONTINUATION_OVERHEAD).is_empty());
    }

    #[test]
    fn chunk_exact_budget_no_split() {
        let text = "ab"; // 2 UTF-16 units
        let chunks = collect_chunks(text, 2);
        assert_eq!(chunks, vec!["ab"]);
    }

    #[test]
    fn chunk_zero_budget_after_continuation_overhead() {
        // First chunk can be sent, continuation budget becomes zero and should
        // stop cleanly (no panic/underflow).
        let text = "abc";
        let chunks = chunk_boundaries(text, 2, 2);
        assert_eq!(chunks, vec![(0, 2)]);
    }

    // -- markdown_to_telegram_html --

    #[test]
    fn md_plain_text_escapes_html() {
        assert_eq!(
            markdown_to_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn md_bold() {
        assert_eq!(
            markdown_to_telegram_html("hello **world**!"),
            "hello <b>world</b>!"
        );
    }

    #[test]
    fn md_inline_code() {
        assert_eq!(
            markdown_to_telegram_html("use `fmt::Display`"),
            "use <code>fmt::Display</code>"
        );
    }

    #[test]
    fn md_inline_code_escapes_html() {
        assert_eq!(
            markdown_to_telegram_html("try `a<b>`"),
            "try <code>a&lt;b&gt;</code>"
        );
    }

    #[test]
    fn md_fenced_code_block() {
        assert_eq!(
            markdown_to_telegram_html("```rust\nfn main() {}\n```"),
            "<pre>fn main() {}</pre>"
        );
    }

    #[test]
    fn md_fenced_code_block_no_lang() {
        assert_eq!(
            markdown_to_telegram_html("```\nhello\n```"),
            "<pre>hello</pre>"
        );
    }

    #[test]
    fn md_unclosed_bold_emits_literally() {
        assert_eq!(
            markdown_to_telegram_html("hello **world"),
            "hello **world"
        );
    }

    #[test]
    fn md_unclosed_backtick_emits_literally() {
        assert_eq!(
            markdown_to_telegram_html("hello `world"),
            "hello `world"
        );
    }

    #[test]
    fn md_unclosed_fence_emits_literally() {
        assert_eq!(
            markdown_to_telegram_html("```\nhello"),
            "```\nhello"
        );
    }

    #[test]
    fn md_mixed_formatting() {
        let input = "**Summary**: use `grep` to search\n```\ngrep -r \"foo\"\n```\ndone";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<b>Summary</b>: use <code>grep</code> to search\n<pre>grep -r \"foo\"</pre>\ndone"
        );
    }

    #[test]
    fn md_multibyte_chars_preserved() {
        assert_eq!(
            markdown_to_telegram_html("hello 🌍 **world**"),
            "hello 🌍 <b>world</b>"
        );
    }

    // -- schedule parser --

    #[test]
    fn parse_schedule_response_single_line_prompt() {
        let parsed = parse_schedule_response(
            "NAME: daily-summary\nCRON: 0 0 9 * * * *\nPROMPT: summarize calendar and priorities",
        )
        .unwrap();
        assert_eq!(parsed.name, "daily-summary");
        assert_eq!(parsed.cron, "0 0 9 * * * *");
        assert_eq!(parsed.prompt, "summarize calendar and priorities");
    }

    #[test]
    fn parse_schedule_response_multiline_prompt() {
        let parsed = parse_schedule_response(
            "NAME: daily-summary\nCRON: 0 0 9 * * * *\nPROMPT: summarize today\ninclude blockers\ninclude deadlines",
        )
        .unwrap();
        assert_eq!(
            parsed.prompt,
            "summarize today\ninclude blockers\ninclude deadlines"
        );
    }

    #[test]
    fn parse_schedule_response_missing_name_rejected() {
        let err = parse_schedule_response(
            "CRON: 0 0 9 * * * *\nPROMPT: summarize calendar and priorities",
        )
        .err()
        .expect("missing NAME should be rejected");
        assert!(err.to_string().contains("NAME"));
    }

    #[test]
    fn parse_schedule_response_accepts_cron_before_name() {
        let parsed = parse_schedule_response(
            "CRON: 0 0 9 * * * *\nNAME: daily-summary\nPROMPT: summarize calendar",
        )
        .unwrap();
        assert_eq!(parsed.name, "daily-summary");
        assert_eq!(parsed.cron, "0 0 9 * * * *");
    }

    #[test]
    fn parse_schedule_response_prompt_trims_leading_space() {
        let parsed = parse_schedule_response(
            "NAME: n\nCRON: 0 0 9 * * * *\nPROMPT:   leading spaces should trim",
        )
        .unwrap();
        assert_eq!(parsed.prompt, "leading spaces should trim");
    }

    #[test]
    fn parse_schedule_response_case_insensitive_prefixes() {
        let parsed = parse_schedule_response(
            "name: daily-summary\ncron: 0 0 9 * * * *\nprompt: summarize calendar",
        )
        .unwrap();
        assert_eq!(parsed.name, "daily-summary");
        assert_eq!(parsed.cron, "0 0 9 * * * *");
        assert_eq!(parsed.prompt, "summarize calendar");
    }

    // -- cron_fires_too_often --

    #[test]
    fn cron_wildcard_minute_rejected() {
        assert!(cron_fires_too_often("*"));
    }

    #[test]
    fn cron_step_below_5_rejected() {
        assert!(cron_fires_too_often("*/1"));
        assert!(cron_fires_too_often("*/2"));
        assert!(cron_fires_too_often("*/4"));
    }

    #[test]
    fn cron_step_5_or_above_accepted() {
        assert!(!cron_fires_too_often("*/5"));
        assert!(!cron_fires_too_often("*/10"));
        assert!(!cron_fires_too_often("*/30"));
    }

    #[test]
    fn cron_fixed_minute_accepted() {
        assert!(!cron_fires_too_often("0"));
        assert!(!cron_fires_too_often("30"));
    }

    #[test]
    fn cron_many_comma_values_rejected() {
        // 13 values → average gap < 5 min
        assert!(cron_fires_too_often("0,5,10,15,20,25,30,35,40,45,50,55,59"));
    }

    #[test]
    fn cron_few_comma_values_accepted() {
        assert!(!cron_fires_too_often("0,15,30,45"));
    }

    #[test]
    fn cron_range_not_checked() {
        // Ranges like 0-59 are not rejected — Claude typically generates
        // `*` or `*/N` instead. Keeping the check simple.
        assert!(!cron_fires_too_often("0-59"));
        assert!(!cron_fires_too_often("0-4"));
    }

    // -- parse_schedule_response field overwriting --

    #[test]
    fn parse_schedule_response_prompt_containing_name_prefix() {
        // Multiline prompt where a continuation line starts with "NAME:" —
        // should NOT overwrite the real name field.
        let parsed = parse_schedule_response(
            "NAME: my-job\nCRON: 0 0 9 * * * *\nPROMPT: explain why\nNAME: is important",
        )
        .unwrap();
        assert_eq!(parsed.name, "my-job");
        assert_eq!(parsed.prompt, "explain why\nNAME: is important");
    }

    #[test]
    fn parse_schedule_response_prompt_with_blank_lines() {
        let parsed = parse_schedule_response(
            "NAME: multi\nCRON: 0 0 9 * * * *\nPROMPT: step 1\n\nstep 2\n\nstep 3",
        )
        .unwrap();
        assert_eq!(parsed.prompt, "step 1\n\nstep 2\n\nstep 3");
    }

    #[test]
    fn parse_schedule_response_trailing_whitespace_trimmed() {
        let parsed = parse_schedule_response(
            "NAME: job\nCRON: 0 0 9 * * * *\nPROMPT: the prompt\n\n\n",
        )
        .unwrap();
        assert_eq!(parsed.prompt, "the prompt");
    }
}
