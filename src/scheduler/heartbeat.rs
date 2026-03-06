use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::claude::{InvocationQueue, RequestOrigin, ResponseStatus};
use crate::common::config::DaemonConfig;
use crate::common::status::StatusTracker;
use crate::common::util::truncate_str;
use crate::telegram;

/// Spawn the heartbeat loop if enabled in config.
///
/// Returns `None` if heartbeat is disabled.
pub fn spawn(
    config: Arc<DaemonConfig>,
    queue: Arc<InvocationQueue>,
    bot: Arc<tokio::sync::RwLock<teloxide::Bot>>,
    status: Arc<StatusTracker>,
) -> Option<JoinHandle<()>> {
    if !config.heartbeat_enabled {
        tracing::info!("Heartbeat disabled");
        return None;
    }

    tracing::info!(
        interval_secs = config.heartbeat_interval_secs,
        notify = config.heartbeat_notify_telegram,
        "Heartbeat enabled"
    );

    Some(tokio::spawn(heartbeat_loop(config, queue, bot, status)))
}

async fn heartbeat_loop(
    config: Arc<DaemonConfig>,
    queue: Arc<InvocationQueue>,
    bot: Arc<tokio::sync::RwLock<teloxide::Bot>>,
    status: Arc<StatusTracker>,
) {
    let interval = Duration::from_secs(config.heartbeat_interval_secs);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // First tick fires immediately — skip it so the first heartbeat
    // happens after one full interval.
    ticker.tick().await;

    loop {
        ticker.tick().await;

        tracing::debug!("Heartbeat firing");

        let bot = bot.read().await.clone();
        let origin = RequestOrigin::Heartbeat;

        let response = match queue.submit(config.heartbeat_prompt.clone(), origin, None).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "Heartbeat invocation failed");
                status.record_error(&e.to_string());
                continue;
            }
        };

        match response.status {
            ResponseStatus::Success => {
                let trimmed = response.text.trim();
                let is_ok = trimmed
                    .trim_end_matches(|c: char| c.is_ascii_punctuation())
                    .eq_ignore_ascii_case("HEARTBEAT_OK");
                if is_ok {
                    tracing::debug!("Heartbeat OK — nothing needs attention");
                    status.record_heartbeat_ok();
                } else {
                    status.record_heartbeat_alert(truncate_str(&response.text, 200));
                    tracing::info!(text = truncate_str(&response.text, 200), "Heartbeat response");
                    if config.heartbeat_notify_telegram {
                        let msg = format!("[Heartbeat]\n{}", response.text);
                        telegram::send_chunked(&bot, config.admin_chat_id, &msg).await;
                    }
                }
            }
            ResponseStatus::RateLimited => {
                tracing::warn!("Heartbeat rate limited, skipping");
                status.record_heartbeat_alert("Heartbeat rate limited; will retry next interval");
                if config.heartbeat_notify_telegram {
                    let _ = telegram::send_text(
                        &bot,
                        config.admin_chat_id,
                        "[Heartbeat] Rate limited; will retry next interval.",
                    )
                    .await;
                }
            }
            ResponseStatus::Error => {
                tracing::warn!(error = %response.text, "Heartbeat got error response");
                status.record_error(&response.text);
                if config.heartbeat_notify_telegram {
                    let msg = format!("[Heartbeat]\n{}", response.text);
                    telegram::send_chunked(&bot, config.admin_chat_id, &msg).await;
                }
            }
            ResponseStatus::SessionExpired => {
                tracing::warn!(error = %response.text, "Heartbeat hit session expiry");
                status.record_error("Session expired during heartbeat");
                if config.heartbeat_notify_telegram {
                    let msg = format!("[Heartbeat]\n{}", response.text);
                    telegram::send_chunked(&bot, config.admin_chat_id, &msg).await;
                }
            }
        }
    }
}
