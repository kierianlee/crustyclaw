pub mod queue;
pub mod session;

pub use self::queue::{InvocationQueue, RequestOrigin};
pub use self::session::SessionManager;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::common::config::{DaemonConfig, PermissionMode};
use crate::common::util::truncate_str;

// ---- Types produced by invoke() ----

/// Outcome status of a Claude CLI invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseStatus {
    /// Invocation completed successfully.
    Success,
    /// Invocation completed but Claude reported the result as an error.
    Error,
    /// The API rate-limited the request.
    RateLimited,
    /// The `--resume` session ID was stale or invalid.
    SessionExpired,
}

/// The result of a Claude CLI invocation.
pub struct ClaudeResponse {
    pub text: String,
    pub session_id: Option<Uuid>,
    pub status: ResponseStatus,
    pub duration_ms: u64,
    pub cost_usd: Option<f64>,
}

impl ClaudeResponse {
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            text: msg.into(),
            session_id: None,
            status: ResponseStatus::Error,
            duration_ms: 0,
            cost_usd: None,
        }
    }

    /// Consume the response and return a user-facing display string,
    /// prefixing non-success statuses with a label.
    pub fn into_display_text(self) -> String {
        match self.status {
            ResponseStatus::Success => self.text,
            ResponseStatus::Error => format!("Error: {}", self.text),
            ResponseStatus::RateLimited => format!("Rate limited: {}", self.text),
            ResponseStatus::SessionExpired => format!("Session expired: {}", self.text),
        }
    }
}

// ---- Invoke implementation ----

/// Max bytes shown per progress line in the daemon terminal.
const PROGRESS_TRUNCATE_LEN: usize = 200;

fn stderr_is_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

const RATE_LIMIT_PATTERNS: &[&str] = &[
    "rate limit",
    "currently overloaded",
    "temporarily overloaded",
    "hit your limit",
    "too many requests",
];

const SESSION_EXPIRY_PATTERNS: &[&str] = &[
    "no conversation found with session",
    "session not found",
    "invalid session",
];
// NOTE: These patterns depend on CLI text and can drift across claude-code
// releases. Audit/update them when bumping claude-code to preserve retry/reset
// behavior for rate-limit and session-expiry errors.

fn matches_patterns(line: &str, patterns: &[&str]) -> bool {
    let lower = line.to_ascii_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}

/// A single line from `claude -p --output-format stream-json` (NDJSON).
#[derive(Default, Deserialize)]
struct StreamLine {
    #[serde(default, rename = "type")]
    line_type: Option<String>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    /// Present on `type: "assistant"` lines — contains the content blocks.
    #[serde(default)]
    message: Option<serde_json::Value>,
}

/// Invoke `claude -p` as a subprocess, streaming progress to the terminal.
///
/// Uses `--output-format stream-json` so thinking, tool use, and text blocks
/// are printed in real time. When `session_id` is `Some`, passes `--resume <id>`
/// to continue an existing conversation.
pub async fn invoke(
    config: &DaemonConfig,
    session_id: Option<Uuid>,
    prompt: &str,
    working_dir: &Path,
    timeout_secs: u64,
) -> Result<ClaudeResponse> {
    let start = Instant::now();

    let mut cmd = tokio::process::Command::new("claude");
    // Pipe the prompt via stdin so it is not visible in `ps` output.
    cmd.arg("-p");
    cmd.stdin(std::process::Stdio::piped());
    if let Some(sid) = session_id {
        cmd.arg("--resume").arg(sid.to_string());
    }
    cmd.arg("--output-format")
        .arg("stream-json")
        .arg("--verbose");
    cmd.arg("--model").arg(&config.model);

    if let Some(ref fallback) = config.fallback_model {
        cmd.arg("--fallback-model").arg(fallback);
    }
    if let Some(budget) = config.max_budget_usd {
        cmd.arg("--max-budget-usd").arg(budget.to_string());
    }

    cmd.arg("--append-system-prompt").arg(&config.system_prompt);

    match config.permission_mode {
        PermissionMode::DangerouslySkip => {
            cmd.arg("--dangerously-skip-permissions");
        }
        PermissionMode::AcceptEdits => {
            cmd.arg("--permission-mode").arg("acceptEdits");
        }
        PermissionMode::Interactive => {}
    }

    if let Some(ref tools) = config.allowed_tools {
        if !tools.is_empty() {
            cmd.arg("--allowedTools").args(tools);
        }
    }
    if let Some(ref tools) = config.disallowed_tools {
        if !tools.is_empty() {
            cmd.arg("--disallowedTools").args(tools);
        }
    }

    // Load hook settings (e.g. PreToolUse hook for Telegram approval) via
    // --settings so they're scoped to the crustyclaw subprocess only and don't
    // require modifying the user's global ~/.claude/settings.json.
    if config.telegram_approval {
        let settings_file = working_dir.join(".claude").join("settings.json");
        if tokio::fs::try_exists(&settings_file).await.unwrap_or(false) {
            cmd.arg("--settings").arg(&settings_file);
        }
    }

    // Prevent Claude CLI nesting detection.
    cmd.env_remove("CLAUDECODE");
    cmd.current_dir(working_dir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // Kill the subprocess if the timeout fires and drops the future.
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context("Failed to spawn claude process")?;

    let mut stdin_pipe = child.stdin.take().expect("stdin piped");
    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    let timeout = std::time::Duration::from_secs(timeout_secs);

    // Write stdin concurrently with reading stdout/stderr to avoid a pipe
    // deadlock: if the prompt exceeds the OS pipe buffer (16 KB on macOS),
    // the subprocess might need to drain stdout before consuming all stdin.
    let result = tokio::time::timeout(timeout, async {
        let (stdin_result, stream_result, stderr_buf, wait_result) = tokio::join!(
            async {
                let res = stdin_pipe.write_all(prompt.as_bytes()).await;
                // Explicitly drop to close the pipe fd and send EOF.
                // `shutdown()` is a no-op for pipes (it's a socket concept),
                // and tokio::join! keeps the async block's state alive until
                // all futures complete, so the fd won't close on its own.
                drop(stdin_pipe);
                res
            },
            read_stream(stdout_pipe),
            drain_stderr(stderr_pipe),
            child.wait(),
        );

        stdin_result.context("Failed to write prompt to claude stdin")?;
        let result_line = stream_result?;
        let stderr_buf = stderr_buf?;
        let status = wait_result?;

        Ok::<_, anyhow::Error>((result_line, stderr_buf, status))
    })
    .await;

    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    match result {
        Ok(Ok((result_line, stderr_buf, status))) => {
            if !status.success() {
                let stderr = String::from_utf8_lossy(&stderr_buf);
                return parse_error_output(&result_line, &stderr, status, duration_ms);
            }

            if result_line.line_type.as_deref() != Some("result") {
                anyhow::bail!(
                    "Claude exited successfully but produced no result message in stream"
                );
            }

            Ok(build_response(result_line, duration_ms))
        }
        Ok(Err(e)) => Err(e).context("Claude subprocess I/O failed"),
        Err(_) => anyhow::bail!("Claude subprocess timed out after {timeout_secs}s"),
    }
}

// ---------------------------------------------------------------------------
// Stream processing
// ---------------------------------------------------------------------------

/// Read stdout line-by-line as NDJSON, display progress, and return the
/// final result line.
///
/// Caps total bytes read from the pipe at 32 MiB to prevent OOM from excessive
/// subprocess output (e.g. a huge tool result in stream-json).
async fn read_stream(pipe: tokio::process::ChildStdout) -> std::io::Result<StreamLine> {
    const MAX_PIPE_READ: u64 = 32 * 1024 * 1024;
    let mut reader = tokio::io::BufReader::new(pipe.take(MAX_PIPE_READ));
    let mut lines = (&mut reader).lines();
    let mut result_line = StreamLine::default();
    let color = stderr_is_tty();

    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<StreamLine>(&line) {
            Ok(parsed) => {
                display_progress(&parsed, color);
                if parsed.line_type.as_deref() == Some("result") {
                    result_line = parsed;
                }
            }
            Err(e) => {
                tracing::trace!(error = %e, line = truncate_str(&line, 200), "Unparseable stream-json line");
            }
        }
    }

    // Detect if the safety cap truncated the stream before we saw a result line.
    drop(lines);
    let was_truncated = reader.into_inner().limit() == 0;
    if was_truncated {
        tracing::warn!("Claude stdout reached the 32 MiB safety cap — stream may be incomplete");
        if result_line.line_type.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Claude output exceeded 32 MiB safety cap before producing a result",
            ));
        }
    }

    Ok(result_line)
}

/// Stream stderr to the terminal (dimmed) and collect it for error analysis.
///
/// Caps the buffer at 128 KiB — generous enough to capture any CLI error
/// message while preventing OOM. The pipe is wrapped in `take()` so a
/// single unterminated line cannot cause unbounded memory use in BufReader.
async fn drain_stderr(pipe: tokio::process::ChildStderr) -> std::io::Result<Vec<u8>> {
    const MAX_STDERR_BYTES: usize = 128 * 1024;
    const MAX_PIPE_READ: u64 = 4 * 1024 * 1024;

    let reader = tokio::io::BufReader::new(pipe.take(MAX_PIPE_READ));
    let mut lines = reader.lines();
    let mut buf = Vec::with_capacity(4096);
    let mut capped = false;

    let color = stderr_is_tty();
    while let Some(line) = lines.next_line().await? {
        if !line.is_empty() {
            if color {
                eprintln!("\x1b[2m{line}\x1b[0m");
            } else {
                eprintln!("{line}");
            }
        }

        if !capped {
            let needed = line.len() + 1;
            if buf.len() + needed > MAX_STDERR_BYTES {
                tracing::debug!("stderr buffer capped at {MAX_STDERR_BYTES} bytes");
                capped = true;
            } else {
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
            }
        }
    }

    Ok(buf)
}

/// Print interesting content from a stream-json line to the daemon terminal.
fn display_progress(line: &StreamLine, color: bool) {
    let Some(msg) = &line.message else { return };
    let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
        return;
    };

    for block in content {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "thinking" => {
                if let Some(text) = block.get("thinking").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        let t = truncate_str(text, PROGRESS_TRUNCATE_LEN);
                        if color {
                            eprintln!("\x1b[2m[thinking] {t}\x1b[0m");
                        } else {
                            eprintln!("[thinking] {t}");
                        }
                    }
                }
            }
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        eprintln!("[response] {}", truncate_str(text, PROGRESS_TRUNCATE_LEN));
                    }
                }
            }
            "tool_use" => {
                if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                    if color {
                        eprintln!("\x1b[33m[tool] {name}\x1b[0m");
                    } else {
                        eprintln!("[tool] {name}");
                    }
                }
            }
            "tool_result" => {
                let is_error = block
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if is_error {
                    if color {
                        eprintln!("\x1b[31m[tool error]\x1b[0m");
                    } else {
                        eprintln!("[tool error]");
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Response construction
// ---------------------------------------------------------------------------

fn build_response(line: StreamLine, duration_ms: u64) -> ClaudeResponse {
    let session_id = line.session_id.as_deref().and_then(|s| {
        match Uuid::parse_str(s) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(raw = s, error = %e, "Failed to parse session_id from Claude — session resume may not work");
                None
            }
        }
    });

    let status = if line.is_error.unwrap_or(false) {
        ResponseStatus::Error
    } else {
        ResponseStatus::Success
    };

    ClaudeResponse {
        text: line.result.unwrap_or_default(),
        session_id,
        status,
        duration_ms,
        cost_usd: line.total_cost_usd,
    }
}

/// Handle non-zero exit: detect rate limiting, session expiry, or general failure.
///
/// Scans both the stream result line and stderr for known error patterns.
fn parse_error_output(
    result_line: &StreamLine,
    stderr: &str,
    status: std::process::ExitStatus,
    duration_ms: u64,
) -> Result<ClaudeResponse> {
    // Check both the result text from stdout and the stderr buffer.
    let result_text = result_line.result.as_deref().unwrap_or("");
    let saw_rate_limit =
        matches_patterns(result_text, RATE_LIMIT_PATTERNS)
        || matches_patterns(stderr, RATE_LIMIT_PATTERNS);
    let saw_session_expiry =
        matches_patterns(result_text, SESSION_EXPIRY_PATTERNS)
        || matches_patterns(stderr, SESSION_EXPIRY_PATTERNS);

    if saw_rate_limit {
        return Ok(ClaudeResponse {
            text: "Rate limited".into(),
            session_id: None,
            status: ResponseStatus::RateLimited,
            duration_ms,
            cost_usd: None,
        });
    }

    if saw_session_expiry {
        return Ok(ClaudeResponse {
            text: stderr.trim().to_string(),
            session_id: None,
            status: ResponseStatus::SessionExpired,
            duration_ms,
            cost_usd: None,
        });
    }

    anyhow::bail!("claude exited with {status}: {}", stderr.trim());
}

#[cfg(test)]
mod tests {
    use super::matches_patterns;

    #[test]
    fn matches_patterns_case_insensitive() {
        assert!(matches_patterns("Rate Limit", &["rate limit"]));
    }

    #[test]
    fn matches_patterns_no_match() {
        assert!(!matches_patterns("all good", &["rate limit"]));
    }
}
