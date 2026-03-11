use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;
use tokio::task::JoinHandle;

use super::util::{atomic_write, atomic_write_sync, truncate_str};

/// Runtime status snapshot, written periodically to status.json.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct RuntimeStatus {
    pub daemon_started_at: DateTime<Utc>,
    pub total_invocations: u64,
    pub queue_depth: usize,
    #[serde(default)]
    pub queue_full_rejections: u64,
    pub last_error: Option<ErrorRecord>,
    #[serde(default)]
    pub heartbeat_last_alert: Option<ErrorRecord>,
    pub heartbeat_last_ok: Option<DateTime<Utc>>,
    pub heartbeat_enabled: bool,
    pub scheduler_job_count: usize,
    pub session_id_short: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ErrorRecord {
    pub message: String,
    pub at: DateTime<Utc>,
}

/// Tracks runtime status. All methods are sync (lock is held for microseconds).
pub struct StatusTracker {
    data: Mutex<RuntimeStatus>,
}

impl StatusTracker {
    pub fn new(heartbeat_enabled: bool) -> Self {
        Self {
            data: Mutex::new(RuntimeStatus {
                daemon_started_at: Utc::now(),
                total_invocations: 0,
                queue_full_rejections: 0,
                queue_depth: 0,
                last_error: None,
                heartbeat_last_alert: None,
                heartbeat_last_ok: None,
                heartbeat_enabled,
                scheduler_job_count: 0,
                session_id_short: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RuntimeStatus> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn record_queue_full(&self) {
        self.lock().queue_full_rejections += 1;
    }

    pub fn record_invocations(&self, count: u64, error: Option<&str>) {
        if count == 0 {
            return;
        }
        let mut data = self.lock();
        data.total_invocations += count;
        if let Some(msg) = error {
            data.last_error = Some(ErrorRecord {
                message: truncate_str(msg, 500).to_string(),
                at: Utc::now(),
            });
        }
    }

    pub fn record_heartbeat_ok(&self) {
        self.lock().heartbeat_last_ok = Some(Utc::now());
    }

    pub fn record_heartbeat_alert(&self, msg: &str) {
        self.lock().heartbeat_last_alert = Some(ErrorRecord {
            message: truncate_str(msg, 500).to_string(),
            at: Utc::now(),
        });
    }

    pub fn record_error(&self, msg: &str) {
        self.lock().last_error = Some(ErrorRecord {
            message: truncate_str(msg, 500).to_string(),
            at: Utc::now(),
        });
    }

    pub fn update_queue_depth(&self, depth: usize) {
        self.lock().queue_depth = depth;
    }

    pub fn update_scheduler_jobs(&self, count: usize) {
        self.lock().scheduler_job_count = count;
    }

    pub fn update_session(&self, short_id: Option<String>) {
        self.lock().session_id_short = short_id;
    }

    pub fn snapshot(&self) -> RuntimeStatus {
        self.lock().clone()
    }
}

/// Write a final status snapshot to disk.
pub async fn flush_final(tracker: &StatusTracker, path: &Path) {
    let status = tracker.snapshot();
    let json = match serde_json::to_string(&status) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize final status");
            return;
        }
    };
    if let Err(e) = atomic_write(path, json.as_bytes()).await {
        tracing::warn!(error = %e, "Failed to write final status.json");
    }
}

/// Spawn a background task that writes status.json every 5 seconds.
pub fn spawn_writer(tracker: Arc<StatusTracker>, path: PathBuf) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;

            let status = tracker.snapshot();
            let json = match serde_json::to_string(&status) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to serialize status");
                    continue;
                }
            };

            if let Err(e) = atomic_write(&path, json.as_bytes()).await {
                tracing::warn!(error = %e, "Failed to write status.json");
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Statusline registration in .claude/settings.json
// ---------------------------------------------------------------------------

/// Resolve `~/.claude/settings.json`.
fn global_claude_settings_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let claude_dir = PathBuf::from(home).join(".claude");
    if let Err(e) = std::fs::create_dir_all(&claude_dir) {
        tracing::warn!(error = %e, "Failed to create ~/.claude dir for statusline");
        return None;
    }
    Some(claude_dir.join("settings.json"))
}

/// Write the `statusLine` entry into `~/.claude/settings.json`
/// so Claude Code displays our status bar globally.
///
/// Uses the stable binary path at `~/.crustyclaw/bin/crustyclaw` which is
/// placed by `install-binary.sh` and survives plugin cache version bumps.
pub fn install_statusline(data_dir: &Path) {
    let settings_path = match global_claude_settings_path() {
        Some(p) => p,
        None => return,
    };

    let mut settings: serde_json::Value = match std::fs::read_to_string(&settings_path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };

    let stable_bin = data_dir.join("bin").join("crustyclaw");
    if !stable_bin.exists() {
        tracing::warn!(path = %stable_bin.display(), "Stable binary not found, skipping statusline");
        return;
    }
    let escaped = stable_bin.display().to_string().replace('\'', "'\\''");
    let command = format!("'{escaped}' statusline");

    let entry = serde_json::json!({
        "type": "command",
        "command": command,
        "refresh": 5
    });

    // Skip rewrite if the entry already matches — avoids unnecessary
    // ConfigChange hook triggers in Claude Code on every daemon start.
    if settings.get("statusLine") == Some(&entry) {
        tracing::debug!("statusLine already up to date");
        return;
    }

    settings
        .as_object_mut()
        .unwrap()
        .insert("statusLine".into(), entry);

    let json = match serde_json::to_string_pretty(&settings) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize settings for statusline");
            return;
        }
    };

    if let Err(e) = atomic_write_sync(&settings_path, json.as_bytes()) {
        tracing::warn!(error = %e, "Failed to write statusline to settings.json");
    } else {
        tracing::info!(path = %settings_path.display(), "Installed statusLine");
    }
}

/// Remove the `statusLine` entry from the global `~/.claude/settings.json`.
pub fn remove_statusline() {
    let settings_path = match global_claude_settings_path() {
        Some(p) => p,
        None => return,
    };

    let mut settings: serde_json::Value = match std::fs::read_to_string(&settings_path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(v) => v,
            Err(_) => return,
        },
        Err(_) => return,
    };

    if let Some(obj) = settings.as_object_mut() {
        if obj.remove("statusLine").is_some() {
            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                let _ = atomic_write_sync(&settings_path, json.as_bytes());
                tracing::info!("Removed statusLine from settings.json");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CLI statusline subcommand
// ---------------------------------------------------------------------------

/// Max age of status.json before the daemon is considered dead.
const STALE_THRESHOLD_SECS: u64 = 15;

pub fn print_statusline(status_path: &Path) {
    let c = true; // Claude Code renders ANSI colors in status line
    let d = dim(c);
    let r = reset(c);

    let update_hint = check_update_available(status_path);

    let top = format!("{d}╭──────── 🦀 crustyclaw 🦀 ────────╮{r}");
    let bot = format!("{d}╰──────────────────────────────────╯{r}");

    match load_status(status_path) {
        None => {
            print!("{top}\n{d}│{r}  ○ offline\n{bot}");
        }
        Some(s) => {
            let uptime = format_uptime(&s);
            let jobs = s.scheduler_job_count;
            let job_label = if jobs == 1 { "job" } else { "jobs" };

            let (status_color, status_text) =
                if s.heartbeat_enabled && s.heartbeat_last_alert.is_some() {
                    (yellow(c), "alert")
                } else {
                    (green(c), "live")
                };

            print!(
                "{top}\n\
                 {d}│{r} 💓 {uptime}  {d}│{r}  📋 {jobs} {job_label}  {d}│{r}  {status_color}● {status_text}{r}  📡\n\
                 {bot}",
            );

            if let Some(latest) = &update_hint {
                print!(
                    "\n{d}│{r}  {cyan}⬆ {latest} available{r} {d}— /crustyclaw:update{r}",
                    cyan = cyan(c),
                );
            }
        }
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Check if an update is available by comparing the compiled-in version
/// against the cached latest-version file written by the background update checker.
fn check_update_available(status_path: &Path) -> Option<String> {
    // latest-version lives in the same data dir as status.json
    let data_dir = status_path.parent()?;
    let cache_path = data_dir.join("latest-version");
    let latest_tag = std::fs::read_to_string(cache_path).ok()?.trim().to_string();
    let latest = latest_tag.strip_prefix('v').unwrap_or(&latest_tag);
    let current = env!("CARGO_PKG_VERSION");
    if latest != current && !latest.is_empty() {
        Some(latest_tag)
    } else {
        None
    }
}

fn load_status(path: &Path) -> Option<RuntimeStatus> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = modified.elapsed().ok()?;
    if age.as_secs() > STALE_THRESHOLD_SECS {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn format_uptime(s: &RuntimeStatus) -> String {
    let elapsed = Utc::now() - s.daemon_started_at;
    let total_secs = elapsed.num_seconds().max(0);
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

fn dim(color: bool) -> &'static str {
    if color { "\x1b[90m" } else { "" }
}
fn green(color: bool) -> &'static str {
    if color { "\x1b[1;92m" } else { "" }
}
fn yellow(color: bool) -> &'static str {
    if color { "\x1b[1;93m" } else { "" }
}
fn cyan(color: bool) -> &'static str {
    if color { "\x1b[1;96m" } else { "" }
}
fn reset(color: bool) -> &'static str {
    if color { "\x1b[0m" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_invocation_increments_counter() {
        let tracker = StatusTracker::new(false);
        tracker.record_invocations(1, None);
        tracker.record_invocations(1, None);
        let snap = tracker.snapshot();
        assert_eq!(snap.total_invocations, 2);
        assert!(snap.last_error.is_none());
    }

    #[test]
    fn record_invocation_with_error_stores_message() {
        let tracker = StatusTracker::new(false);
        tracker.record_invocations(1, Some("something went wrong"));
        let snap = tracker.snapshot();
        assert_eq!(snap.total_invocations, 1);
        assert!(snap.last_error.is_some());
        assert_eq!(snap.last_error.unwrap().message, "something went wrong");
    }

    #[test]
    fn record_queue_full_increments_counter() {
        let tracker = StatusTracker::new(false);
        tracker.record_queue_full();
        tracker.record_queue_full();
        let snap = tracker.snapshot();
        assert_eq!(snap.queue_full_rejections, 2);
    }

}
