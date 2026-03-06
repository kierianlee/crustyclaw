use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use teloxide::payloads::{EditMessageTextSetters, SendMessageSetters};
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::common::util::atomic_write_sync;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// JSON received from the Claude Code `PreToolUse` hook (via stdin).
#[derive(Deserialize)]
struct HookInput {
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

/// JSON sent back to Claude Code from the `PreToolUse` hook.
///
/// Uses the structured `hookSpecificOutput` format so Claude Code interprets
/// the decision correctly (allow/deny/ask) instead of falling through to its
/// default permission flow.
///
/// See: <https://code.claude.com/docs/en/hooks>
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HookOutput {
    hook_specific_output: HookSpecificOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HookSpecificOutput {
    hook_event_name: &'static str,
    permission_decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_decision_reason: Option<String>,
}

impl HookOutput {
    fn allow() -> Self {
        Self {
            hook_specific_output: HookSpecificOutput {
                hook_event_name: "PreToolUse",
                permission_decision: "allow",
                permission_decision_reason: None,
            },
        }
    }

    fn deny(reason: impl Into<String>) -> Self {
        Self {
            hook_specific_output: HookSpecificOutput {
                hook_event_name: "PreToolUse",
                permission_decision: "deny",
                permission_decision_reason: Some(reason.into()),
            },
        }
    }
}

/// Callback decision from Telegram buttons.
enum Decision {
    Allow,
    AlwaysAllow,
    Deny,
}

// Sent as `permission_decision_reason` for denied tool calls. This message is
// intentionally phrased as assistant guidance because Claude may surface it to
// the model when deciding how to continue after a deny.
const USER_DENY_REASON: &str =
    "The user denied this tool call. Do not retry it or mention hooks. Acknowledge the denial and move on.";

struct PendingRequest {
    tx: oneshot::Sender<Decision>,
    tool_name: String,
    /// `None` until the Telegram send completes and we know the real message ID.
    /// A fast callback arriving in that narrow window will skip the edit — the
    /// decision is still delivered via the oneshot channel, so the hook responds
    /// correctly; only the Telegram message update is skipped.
    message_id: Option<MessageId>,
    message_text: String,
}

/// Max bytes to read from a hook connection or stdin (64 KiB).
/// Shared by both the server (daemon) and client (hook-handler).
const MAX_HOOK_INPUT: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// Permission server (daemon side)
// ---------------------------------------------------------------------------

/// Unix socket server that bridges Claude Code `PreToolUse` hooks to Telegram
/// inline keyboard approval.
///
/// Flow: hook script → socket → Telegram message with [Allow]/[Deny] buttons
///       → admin taps button → callback query → socket response → hook exits
pub struct PermissionServer {
    pending: Mutex<HashMap<String, PendingRequest>>,
    /// Tools the admin has marked "Always Allow" this session. Intentionally
    /// in-memory only — cleared on daemon restart as a security measure so
    /// stale blanket approvals don't persist across restarts.
    auto_approved: RwLock<HashSet<String>>,
    bot: Arc<RwLock<Bot>>,
    admin_chat_id: i64,
    timeout: Duration,
    socket_path: PathBuf,
}

impl PermissionServer {
    pub fn new(socket_path: PathBuf, bot: Arc<RwLock<Bot>>, admin_chat_id: i64, timeout_secs: u64) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(HashMap::new()),
            auto_approved: RwLock::new(HashSet::new()),
            bot,
            admin_chat_id,
            timeout: Duration::from_secs(timeout_secs),
            socket_path,
        })
    }

    /// Start listening for hook connections. Removes any stale socket file first.
    ///
    /// Restricts the socket to mode 0600 immediately after bind via
    /// `set_permissions`, so only the owning user can connect.
    pub fn spawn(self: &Arc<Self>) -> Result<JoinHandle<()>> {
        let _ = std::fs::remove_file(&self.socket_path);

        // Narrow umask while creating the socket to avoid a brief world-writable
        // window between bind() and set_permissions(), then restore it.
        let std_listener = {
            // SAFETY: umask is process-global; we immediately restore the prior
            // value before returning from this scope.
            let old_umask = unsafe { libc::umask(0o177) };
            let result = std::os::unix::net::UnixListener::bind(&self.socket_path);
            // SAFETY: restore the saved umask unconditionally.
            unsafe { libc::umask(old_umask) };
            result
        }
        .with_context(|| format!("Failed to bind {}", self.socket_path.display()))?;

        std::fs::set_permissions(
            &self.socket_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .with_context(|| {
            format!(
                "Failed to set permissions on {}",
                self.socket_path.display()
            )
        })?;
        std_listener
            .set_nonblocking(true)
            .context("Failed to set socket non-blocking")?;

        let listener = tokio::net::UnixListener::from_std(std_listener)
            .context("Failed to create async UnixListener")?;

        tracing::info!(path = %self.socket_path.display(), "Permission server listening");

        let server = self.clone();
        Ok(tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let server = server.clone();
                        tokio::spawn(async move {
                            if let Err(e) = server.handle_connection(stream).await {
                                tracing::warn!(error = %e, "Permission request handler error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Permission server accept failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }))
    }

    /// Remove the socket file on shutdown.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }

    async fn handle_connection(&self, stream: tokio::net::UnixStream) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = tokio::io::BufReader::new(reader.take(MAX_HOOK_INPUT));
        let mut line = String::new();

        // Timeout the initial read so a hung or misbehaving client can't hold
        // a connection open indefinitely. Use the approval timeout + 10s as a
        // generous upper bound — the client should send data immediately.
        let read_timeout = self.timeout.saturating_add(Duration::from_secs(10));
        match tokio::time::timeout(read_timeout, buf_reader.read_line(&mut line)).await {
            Ok(result) => result.context("read hook input")?,
            Err(_) => anyhow::bail!("hook client timed out sending input"),
        };

        let input: HookInput = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Malformed hook input — treating as unknown tool");
                HookInput {
                    tool_name: None,
                    tool_input: None,
                }
            }
        };

        let tool_name = input.tool_name.as_deref().unwrap_or("unknown");

        // Fast path: tool was previously "Always Allowed" this session.
        // Never auto-approve "unknown" — it's a catch-all for malformed inputs.
        if tool_name != "unknown" && self.auto_approved.read().await.contains(tool_name) {
            tracing::debug!(tool = tool_name, "Auto-approved (always allow)");
            let json = serde_json::to_string(&HookOutput::allow())?;
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.shutdown().await?;
            return Ok(());
        }

        let details = format_tool_details(tool_name, input.tool_input.as_ref());
        let display_name = html_escape(&prettify_tool_name(tool_name));
        let msg_text = fit_telegram_limit(&format!("🔧 <b>{display_name}</b>\n{details}"));

        let request_id = uuid::Uuid::new_v4().to_string();

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("✅ Allow", format!("perm:{request_id}:a")),
            InlineKeyboardButton::callback("✅ Always", format!("perm:{request_id}:aa")),
            InlineKeyboardButton::callback("❌ Deny", format!("perm:{request_id}:d")),
        ]]);

        // Insert into pending map *before* sending the Telegram message so an
        // extremely fast callback can't race ahead of the insertion. The
        // message_id (None) is updated once the send succeeds; if the callback
        // fires before the update, the edit is a harmless no-op.
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            request_id.clone(),
            PendingRequest {
                tx,
                tool_name: tool_name.to_string(),
                message_id: None,
                message_text: msg_text.clone(),
            },
        );

        let bot = self.bot.read().await.clone();

        let sent = match bot
            .send_message(ChatId(self.admin_chat_id), &msg_text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await
        {
            Ok(msg) => msg,
            Err(e) => {
                tracing::error!(error = %e, "Failed to send permission prompt to Telegram");
                self.pending.lock().await.remove(&request_id);
                let json =
                    serde_json::to_string(&HookOutput::deny(format!("Telegram unavailable: {e}")))?;
                writer.write_all(json.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.shutdown().await?;
                return Ok(());
            }
        };

        // Update the message_id now that we know the real one.
        if let Some(req) = self.pending.lock().await.get_mut(&request_id) {
            req.message_id = Some(sent.id);
        }

        let decision = if let Ok(Ok(d)) = tokio::time::timeout(self.timeout, rx).await {
            d
        } else {
            // Timeout or channel dropped — only edit the Telegram message
            // if we still own the pending request. At the timeout boundary,
            // whichever path removes the request from `pending` first wins the
            // message edit; the loser sees None and performs no edit.
            if self.pending.lock().await.remove(&request_id).is_some() {
                let _ = bot
                    .edit_message_text(
                        ChatId(self.admin_chat_id),
                        sent.id,
                        format!("{msg_text}\n\n⏰ Timed out — denied"),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
            }
            Decision::Deny
        };

        let output = match decision {
            Decision::Allow | Decision::AlwaysAllow => HookOutput::allow(),
            Decision::Deny => HookOutput::deny(USER_DENY_REASON),
        };

        let json = serde_json::to_string(&output).context("serialize hook output")?;
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await?;

        Ok(())
    }

    /// Handle a Telegram callback query for a permission button press.
    pub async fn handle_callback(&self, bot: &Bot, query: &teloxide::types::CallbackQuery) {
        // Auth: only the admin can approve/deny tool calls. In private chats,
        // chat_id equals the user id, so we store the admin's chat_id and
        // compare it to query.from.id here.
        let is_admin = match i64::try_from(query.from.id.0) {
            Ok(id) => id == self.admin_chat_id,
            Err(_) => {
                tracing::warn!(
                    user_id = query.from.id.0,
                    "Callback user ID exceeds i64 range; rejecting callback"
                );
                false
            }
        };
        if !is_admin {
            let _ = bot.answer_callback_query(query.id.clone()).await;
            return;
        }

        let Some(data) = &query.data else {
            let _ = bot.answer_callback_query(query.id.clone()).await;
            return;
        };

        let parts: Vec<&str> = data.split(':').collect();
        if parts.len() != 3 || parts[0] != "perm" {
            let _ = bot.answer_callback_query(query.id.clone()).await;
            return;
        }

        let request_id = parts[1];
        // "a" = allow, "aa" = always allow, "d" = deny
        let action = parts[2];

        let Some(req) = self.pending.lock().await.remove(request_id) else {
            // Already resolved (timeout or duplicate click).
            let _ = bot.answer_callback_query(query.id.clone()).await;
            return;
        };

        let (decision, status) = match action {
            "a" => (Decision::Allow, "✅ Allowed"),
            "aa" => {
                if req.tool_name == "unknown" {
                    // Refuse to auto-approve the catch-all "unknown" tool.
                    (
                        Decision::Allow,
                        "✅ Allowed (always-allow ignored for unknown tool)",
                    )
                } else {
                    self.auto_approved
                        .write()
                        .await
                        .insert(req.tool_name.clone());
                    tracing::info!(tool = %req.tool_name, "Tool added to always-allow list");
                    (Decision::AlwaysAllow, "✅ Always allowed")
                }
            }
            _ => (Decision::Deny, "❌ Denied"),
        };

        let _ = req.tx.send(decision);

        // message_id is None until the Telegram send completes. If the callback
        // arrives in that narrow window, skip the edit — the decision is still
        // delivered via the oneshot channel so the hook responds correctly.
        if let Some(mid) = req.message_id {
            let _ = bot
                .edit_message_text(
                    ChatId(self.admin_chat_id),
                    mid,
                    format!("{}\n\n{status}", req.message_text),
                )
                .parse_mode(ParseMode::Html)
                .await;
        } else {
            tracing::warn!(
                tool = %req.tool_name,
                status,
                "Callback arrived before Telegram message_id was set — message not updated"
            );
        }
        let _ = bot.answer_callback_query(query.id.clone()).await;
    }
}

// ---------------------------------------------------------------------------
// Hook handler subcommand (client side)
// ---------------------------------------------------------------------------

/// `crustyclaw hook-handler` — called by Claude Code's `PreToolUse` hook.
///
/// Reads tool info from stdin, forwards it to the daemon's permission server
/// via unix socket, and prints the approval decision to stdout.
///
/// `timeout_secs` should exceed the daemon's `approval_timeout_secs` so the
/// hook client doesn't give up before the server does.
pub async fn handle_hook(socket_path: &Path, timeout_secs: u64) -> Result<()> {
    let mut input = String::new();
    tokio::io::BufReader::new(tokio::io::stdin().take(MAX_HOOK_INPUT))
        .read_line(&mut input)
        .await
        .context("read hook input from stdin")?;

    if input.trim().is_empty() {
        // No input — pass through (no opinion).
        return Ok(());
    }

    let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await else {
        // Daemon not running — exit silently with no output.
        // Claude Code treats this as "no opinion" and falls back to its
        // normal permission behavior, so regular Claude usage is unaffected.
        return Ok(());
    };

    let (reader, mut writer) = stream.into_split();

    writer.write_all(input.trim().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;

    let mut buf_reader = tokio::io::BufReader::new(reader.take(MAX_HOOK_INPUT));
    let mut response = String::new();

    match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        buf_reader.read_line(&mut response),
    )
    .await
    {
        Ok(Ok(_)) if !response.trim().is_empty() => {
            print!("{response}");
        }
        Ok(Ok(_)) => {
            eprintln!("crustyclaw: server closed connection without a decision");
            print!(
                "{}",
                serde_json::to_string(&HookOutput::deny("Server closed without response"))
                    .unwrap_or_default()
            );
        }
        Ok(Err(e)) => {
            eprintln!("crustyclaw: read error from daemon ({e})");
            print!(
                "{}",
                serde_json::to_string(&HookOutput::deny("Communication error"))?
            );
        }
        Err(_) => {
            eprintln!("crustyclaw: timed out waiting for permission decision");
            print!("{}", serde_json::to_string(&HookOutput::deny("Timed out"))?);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether the `PreToolUse` hook is installed in project settings.
///
/// This intentionally checks only `{working_dir}/.claude/settings.json`.
/// crustyclaw passes that file via `claude --settings`, so global
/// `~/.claude/settings.json` is irrelevant for crustyclaw subprocesses.
pub fn is_hook_installed(working_dir: &Path) -> bool {
    let project_settings = working_dir.join(".claude").join("settings.json");
    settings_has_hook(&project_settings)
}

fn settings_has_hook(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };

    let Ok(settings): Result<serde_json::Value, _> = serde_json::from_str(&contents) else {
        return false;
    };

    settings_value_has_hook(&settings)
}

fn settings_value_has_hook(settings: &serde_json::Value) -> bool {
    settings
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|p| p.as_array())
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .is_some_and(is_crustyclaw_hook_command)
                        })
                    })
            })
        })
}

/// Install the `PreToolUse` hook into `{working_dir}/.claude/settings.json`.
///
/// Uses the absolute path to the current binary so the hook works regardless
/// of the subprocess's PATH. The settings file is loaded by the Claude
/// subprocess via `--settings`, keeping it scoped to crustyclaw only.
pub fn install_hook(working_dir: &Path) -> Result<()> {
    let claude_dir = working_dir.join(".claude");
    std::fs::create_dir_all(&claude_dir)
        .with_context(|| format!("Failed to create {}", claude_dir.display()))?;

    let settings_path = claude_dir.join("settings.json");

    // Load existing settings or start fresh.
    let mut settings: serde_json::Value = match std::fs::read_to_string(&settings_path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|_| {
            tracing::warn!(
                "Existing {} is invalid JSON, overwriting",
                settings_path.display()
            );
            serde_json::json!({})
        }),
        Err(_) => serde_json::json!({}),
    };

    // Check if already installed.
    if settings_value_has_hook(&settings) {
        tracing::debug!(
            "PreToolUse hook already installed in {}",
            settings_path.display()
        );
        return Ok(());
    }

    // Use the absolute path to the running binary so the hook works
    // regardless of the Claude subprocess's PATH.  Quote the path so
    // directories with spaces (common on macOS) don't break shell splitting.
    let exe_path = std::env::current_exe().context("Cannot determine path to crustyclaw binary")?;
    let exe_str = exe_path.display().to_string();
    // Always single-quote the path to handle spaces and special chars.
    // Escape embedded single quotes with the shell '\'' idiom.
    let escaped = exe_str.replace('\'', "'\\''");
    let hook_command = format!("'{escaped}' hook-handler");

    let hook_entry = serde_json::json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": hook_command
        }]
    });

    // Merge into existing hooks.PreToolUse array, or create it.
    let hooks = settings
        .as_object_mut()
        .context("settings is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let pre_tool_use = hooks
        .as_object_mut()
        .context("hooks is not an object")?
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));
    pre_tool_use
        .as_array_mut()
        .context("PreToolUse is not an array")?
        .push(hook_entry);

    let json = serde_json::to_string_pretty(&settings)?;
    atomic_write_sync(&settings_path, json.as_bytes())?;

    tracing::info!(path = %settings_path.display(), "Installed PreToolUse hook");
    Ok(())
}

/// Check whether a command string refers to the crustyclaw hook-handler binary.
///
/// Matches commands written by `install_hook`: `'<path>/crustyclaw' hook-handler`
/// or `<path>/crustyclaw hook-handler`. Strips quotes from the binary path before
/// checking the basename.
fn is_crustyclaw_hook_command(cmd: &str) -> bool {
    let Some(rest) = cmd.strip_suffix("hook-handler") else {
        return false;
    };
    let rest = rest.trim_end();
    if rest.is_empty() {
        return false;
    }
    // Strip surrounding quotes from the binary path.
    let binary = rest
        .trim()
        .trim_start_matches('\'')
        .trim_end_matches('\'')
        .trim_start_matches('"')
        .trim_end_matches('"');
    std::path::Path::new(binary)
        .file_name()
        .and_then(|s| s.to_str())
        == Some("crustyclaw")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Telegram text limit (UTF-16 code units).
const TELEGRAM_TEXT_LIMIT: usize = 4096;
/// Reserve for the "\n\n✅ Allowed" / "⏰ Timed out — denied" suffix.
const STATUS_SUFFIX_RESERVE: usize = 30;

/// Truncate an HTML message to fit within Telegram's text limit, leaving room
/// for the status suffix appended after approval. Closes any `<pre>`, `<code>`,
/// or `<b>` tags left open by the truncation.
fn fit_telegram_limit(msg: &str) -> String {
    let budget = TELEGRAM_TEXT_LIMIT - STATUS_SUFFIX_RESERVE;
    if msg.encode_utf16().count() <= budget {
        return msg.to_string();
    }

    // Leave room for closing tags + ellipsis that we may append.
    let truncate_at = budget.saturating_sub(25);
    let mut count = 0usize;
    let mut end = 0usize;
    for ch in msg.chars() {
        let units = ch.len_utf16();
        if count + units > truncate_at {
            break;
        }
        count += units;
        end += ch.len_utf8();
    }

    let mut truncated = msg[..end].to_string();

    // If we cut inside an HTML entity (&amp; &lt; &gt;), back up to before it.
    if let Some(amp_pos) = truncated.rfind('&') {
        if !truncated[amp_pos..].contains(';') {
            truncated.truncate(amp_pos);
        }
    }

    truncated.push('…');

    // Close any unclosed HTML tags.
    let balance = |open: &str, close: &str, s: &str| -> usize {
        s.matches(open).count().saturating_sub(s.matches(close).count())
    };
    for _ in 0..balance("<b>", "</b>", &truncated) {
        truncated.push_str("</b>");
    }
    for _ in 0..balance("<code>", "</code>", &truncated) {
        truncated.push_str("</code>");
    }
    for _ in 0..balance("<pre>", "</pre>", &truncated) {
        truncated.push_str("</pre>");
    }

    truncated
}

/// Shorten MCP tool names for display.
///
/// `mcp__claude_ai_Slack__slack_search_public` → `Slack / slack_search_public`
/// `mcp__myserver__do_thing`                   → `myserver / do_thing`
/// `Bash`                                      → `Bash`
fn prettify_tool_name(name: &str) -> String {
    let rest = name
        .strip_prefix("mcp__claude_ai_")
        .or_else(|| name.strip_prefix("mcp__"));
    if let Some(rest) = rest {
        if let Some(pos) = rest.find("__") {
            let provider = &rest[..pos];
            let tool = &rest[pos + 2..];
            return format!("{provider} / {tool}");
        }
    }
    name.to_string()
}

/// Format a simple inline diff: lines from `old` prefixed with `- `,
/// lines from `new` prefixed with `+ `.
fn format_inline_diff(old: &str, new: &str) -> String {
    let mut diff = String::new();
    for line in old.lines() {
        diff.push_str("- ");
        diff.push_str(line);
        diff.push('\n');
    }
    for line in new.lines() {
        diff.push_str("+ ");
        diff.push_str(line);
        diff.push('\n');
    }
    if diff.ends_with('\n') {
        diff.pop();
    }
    diff
}

fn format_tool_details(tool_name: &str, tool_input: Option<&serde_json::Value>) -> String {
    let Some(input) = tool_input else {
        return String::new();
    };

    match tool_name {
        "Bash" => {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("(no command)");
            format!("<pre>{}</pre>", html_escape(cmd))
        }
        "Edit" => {
            let file = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown file)");
            let old = input.get("old_string").and_then(|v| v.as_str());
            let new = input.get("new_string").and_then(|v| v.as_str());
            match (old, new) {
                (Some(o), Some(n)) => {
                    let diff = format_inline_diff(o, n);
                    format!(
                        "<code>{}</code>\n<pre>{}</pre>",
                        html_escape(file),
                        html_escape(&diff)
                    )
                }
                _ => format!("<code>{}</code>", html_escape(file)),
            }
        }
        "Write" => {
            let file = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown file)");
            let content = input.get("content").and_then(|v| v.as_str());
            match content {
                Some(c) => format!(
                    "<code>{}</code>\n<pre>{}</pre>",
                    html_escape(file),
                    html_escape(c)
                ),
                None => format!("<code>{}</code>", html_escape(file)),
            }
        }
        _ => {
            let json = serde_json::to_string_pretty(input).unwrap_or_default();
            format!("<pre>{}</pre>", html_escape(&json))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_crustyclaw_hook_command --

    #[test]
    fn hook_command_single_quoted_path() {
        assert!(is_crustyclaw_hook_command(
            "'/usr/local/bin/crustyclaw' hook-handler"
        ));
    }

    #[test]
    fn hook_command_unquoted_bare_name() {
        assert!(is_crustyclaw_hook_command("crustyclaw hook-handler"));
    }

    #[test]
    fn hook_command_path_with_spaces_single_quoted() {
        assert!(is_crustyclaw_hook_command(
            "'/home/user/my apps/crustyclaw' hook-handler"
        ));
    }

    #[test]
    fn hook_command_double_quoted_path() {
        assert!(is_crustyclaw_hook_command(
            "\"/usr/local/bin/crustyclaw\" hook-handler"
        ));
    }

    #[test]
    fn hook_command_slash_prefix() {
        assert!(is_crustyclaw_hook_command("/usr/local/bin/crustyclaw hook-handler"));
    }

    #[test]
    fn hook_command_rejects_not_crustyclaw() {
        assert!(!is_crustyclaw_hook_command("not-crustyclaw hook-handler"));
    }

    #[test]
    fn hook_command_rejects_missing_hook_handler() {
        assert!(!is_crustyclaw_hook_command("'/usr/bin/crustyclaw'"));
        assert!(!is_crustyclaw_hook_command("crustyclaw"));
    }

    #[test]
    fn hook_command_rejects_wrong_subcommand() {
        assert!(!is_crustyclaw_hook_command("crustyclaw statusline"));
        assert!(!is_crustyclaw_hook_command("crustyclaw setup"));
    }

    #[test]
    fn hook_command_rejects_no_crustyclaw() {
        assert!(!is_crustyclaw_hook_command("something-else hook-handler"));
        assert!(!is_crustyclaw_hook_command(""));
    }

    #[test]
    fn hook_command_rejects_prefixed_shell_command() {
        assert!(!is_crustyclaw_hook_command("echo crustyclaw hook-handler"));
    }

    // -- prettify_tool_name --

    #[test]
    fn prettify_mcp_claude_ai() {
        assert_eq!(
            prettify_tool_name("mcp__claude_ai_Slack__slack_search_public"),
            "Slack / slack_search_public"
        );
    }

    #[test]
    fn prettify_mcp_custom_server() {
        assert_eq!(
            prettify_tool_name("mcp__myserver__do_thing"),
            "myserver / do_thing"
        );
    }

    #[test]
    fn prettify_plain_tool() {
        assert_eq!(prettify_tool_name("Bash"), "Bash");
    }

    // -- html_escape --

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(html_escape("<b>&</b>"), "&lt;b&gt;&amp;&lt;/b&gt;");
    }

    // -- format_tool_details --

    #[test]
    fn format_bash_command() {
        let input = serde_json::json!({ "command": "ls -la" });
        assert_eq!(
            format_tool_details("Bash", Some(&input)),
            "<pre>ls -la</pre>"
        );
    }

    #[test]
    fn format_bash_missing_command() {
        let input = serde_json::json!({});
        assert_eq!(
            format_tool_details("Bash", Some(&input)),
            "<pre>(no command)</pre>"
        );
    }

    #[test]
    fn format_bash_escapes_html() {
        let input = serde_json::json!({ "command": "echo <script>" });
        assert_eq!(
            format_tool_details("Bash", Some(&input)),
            "<pre>echo &lt;script&gt;</pre>"
        );
    }

    #[test]
    fn format_edit_shows_diff() {
        let input = serde_json::json!({
            "file_path": "/src/main.rs",
            "old_string": "let x = 1;",
            "new_string": "let x = 2;"
        });
        let result = format_tool_details("Edit", Some(&input));
        assert!(result.starts_with("<code>/src/main.rs</code>\n<pre>"));
        assert!(result.contains("- let x = 1;"));
        assert!(result.contains("+ let x = 2;"));
    }

    #[test]
    fn format_edit_file_only_without_strings() {
        let input = serde_json::json!({ "file_path": "/src/main.rs" });
        assert_eq!(
            format_tool_details("Edit", Some(&input)),
            "<code>/src/main.rs</code>"
        );
    }

    #[test]
    fn format_write_shows_content() {
        let input = serde_json::json!({
            "file_path": "/tmp/out.txt",
            "content": "hello world"
        });
        let result = format_tool_details("Write", Some(&input));
        assert!(result.starts_with("<code>/tmp/out.txt</code>\n<pre>"));
        assert!(result.contains("hello world"));
    }

    #[test]
    fn format_write_file_only_without_content() {
        let input = serde_json::json!({ "file_path": "/tmp/out.txt" });
        assert_eq!(
            format_tool_details("Write", Some(&input)),
            "<code>/tmp/out.txt</code>"
        );
    }

    #[test]
    fn format_unknown_tool_shows_json() {
        let input = serde_json::json!({ "key": "value" });
        let result = format_tool_details("SomeOtherTool", Some(&input));
        assert!(result.starts_with("<pre>"));
        assert!(result.ends_with("</pre>"));
        assert!(result.contains("key"));
        assert!(result.contains("value"));
    }

    #[test]
    fn format_no_input() {
        assert_eq!(format_tool_details("Bash", None), "");
    }

    // -- fit_telegram_limit --

    #[test]
    fn fit_short_message_unchanged() {
        let msg = "🔧 <b>Edit</b>\n<code>/src/main.rs</code>\n<pre>- old\n+ new</pre>";
        assert_eq!(fit_telegram_limit(msg), msg);
    }

    #[test]
    fn fit_long_message_truncated_with_closed_tags() {
        // Build a message that exceeds the Telegram limit.
        let big_content = "x".repeat(5000);
        let msg = format!("🔧 <b>Edit</b>\n<pre>{big_content}</pre>");
        let result = fit_telegram_limit(&msg);
        let utf16_len: usize = result.encode_utf16().count();
        assert!(
            utf16_len <= TELEGRAM_TEXT_LIMIT - STATUS_SUFFIX_RESERVE,
            "result is {utf16_len} UTF-16 units, exceeds budget"
        );
        assert!(result.contains("…"), "should have ellipsis");
        assert!(result.ends_with("</pre>"), "should close <pre> tag");
    }

    #[test]
    fn fit_truncation_avoids_partial_entity() {
        // Build a message where truncation would land inside &amp;
        // Pad to just under the limit, then add &amp; entities right at the boundary.
        let padding = "x".repeat(TELEGRAM_TEXT_LIMIT - STATUS_SUFFIX_RESERVE - 30);
        let msg = format!("<pre>{padding}&amp;&amp;&amp;</pre>");
        let result = fit_telegram_limit(&msg);
        // Should not contain a bare & (partial entity).
        assert!(
            !result.contains("&a…") && !result.contains("&am…"),
            "should not cut inside an entity"
        );
    }
}
