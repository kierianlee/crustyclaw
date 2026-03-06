use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use super::util::{atomic_write, truncate_str};

/// Runtime status snapshot, written periodically to status.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
// CLI statusline subcommand
// ---------------------------------------------------------------------------

/// Max age of status.json before the daemon is considered dead.
const STALE_THRESHOLD_SECS: u64 = 15;

pub fn print_statusline(status_path: &Path) {
    let use_color = is_stdout_tty();

    let Ok(metadata) = std::fs::metadata(status_path) else {
        print!("{}crustyclaw offline{}", dim(use_color), reset(use_color));
        return;
    };

    if let Ok(modified) = metadata.modified() {
        if let Ok(age) = modified.elapsed() {
            if age.as_secs() > STALE_THRESHOLD_SECS {
                print!(
                    "{}crustyclaw offline (stale){}",
                    dim(use_color),
                    reset(use_color)
                );
                return;
            }
        }
    }

    let s = if let Ok(contents) = std::fs::read_to_string(status_path) {
        if let Ok(parsed) = serde_json::from_str::<RuntimeStatus>(&contents) {
            parsed
        } else {
            print!("{}crustyclaw offline{}", dim(use_color), reset(use_color));
            return;
        }
    } else {
        print!("{}crustyclaw offline{}", dim(use_color), reset(use_color));
        return;
    };

    let elapsed = Utc::now() - s.daemon_started_at;
    let total_secs = elapsed.num_seconds().max(0);
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let uptime = if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m")
    };

    let sid = s.session_id_short.as_deref().unwrap_or("\u{2014}");
    let q = s.queue_depth;
    let rejected = s.queue_full_rejections;

    let hb = if s.heartbeat_enabled {
        if s.heartbeat_last_alert.is_some() {
            format!("{}\u{2661} alert{}", yellow(use_color), reset(use_color))
        } else if s.heartbeat_last_ok.is_some() {
            format!("{}\u{2661} ok{}", green(use_color), reset(use_color))
        } else {
            format!("{}\u{2661} waiting{}", yellow(use_color), reset(use_color))
        }
    } else {
        format!("{}\u{2661} off{}", dim(use_color), reset(use_color))
    };

    let err_str = match &s.last_error {
        Some(e) => {
            let msg = truncate_str(&e.message, 80);
            format!("{}err: {msg}{}", red(use_color), reset(use_color))
        }
        None => format!("{}err: none{}", green(use_color), reset(use_color)),
    };

    let rejected_str = if rejected > 0 {
        format!(" {}(full\u{00d7}{rejected}){}", red(use_color), reset(use_color))
    } else {
        String::new()
    };
    print!(
        "{}crustyclaw{} {}\u{25cf}{} {uptime} \u{2502} Q:{q}{rejected_str} \u{2502} s:{sid}\n\
         {hb} \u{2502} \u{23f0} {} jobs \u{2502} inv:{} \u{2502} {err_str}",
        bold_cyan(use_color),
        reset(use_color),
        green(use_color),
        reset(use_color),
        s.scheduler_job_count,
        s.total_invocations,
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn is_stdout_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

fn dim(color: bool) -> &'static str {
    if color { "\x1b[90m" } else { "" }
}
fn green(color: bool) -> &'static str {
    if color { "\x1b[32m" } else { "" }
}
fn yellow(color: bool) -> &'static str {
    if color { "\x1b[33m" } else { "" }
}
fn red(color: bool) -> &'static str {
    if color { "\x1b[31m" } else { "" }
}
fn bold_cyan(color: bool) -> &'static str {
    if color { "\x1b[1;36m" } else { "" }
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
