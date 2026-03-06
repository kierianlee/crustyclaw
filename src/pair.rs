use anyhow::{Context, Result};
use teloxide::payloads::GetUpdatesSetters;
use teloxide::prelude::*;
use teloxide::types::UpdateKind;

use crate::common::{config, util};
use crate::telegram;

/// Run the `crustyclaw pair` interactive pairing flow.
pub async fn run() -> Result<()> {
    println!("crustyclaw pair\n");

    let data_dir = config::data_dir();
    let config_path = config::config_path(&data_dir);
    if !config_path.exists() {
        anyhow::bail!(
            "No config found at {}. Run `crustyclaw setup` first.",
            config_path.display()
        );
    }

    let config_raw: serde_json::Value = {
        let contents = tokio::fs::read_to_string(&config_path)
            .await
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?
    };
    let token = config_raw
        .get("telegram_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("telegram_token not found in config"))?;

    check_daemon_not_running(&data_dir)?;

    let bot = telegram::make_bot(token)?;
    let me = bot
        .get_me()
        .await
        .map_err(|e| anyhow::anyhow!("Invalid bot token: {e}"))?;
    let username = me.username.as_deref().unwrap_or("unknown");

    let code = generate_pairing_code();
    println!("  Send this code to @{username}: {code}\n");
    println!("  Waiting up to 120 seconds...\n");

    if let Err(e) = bot.delete_webhook().await {
        eprintln!("  Warning: failed to clear webhook ({e}). Detection may not work.");
    }

    let (chat_id, sender_name) = poll_for_code(&bot, &code, 120).await?;
    println!("  Matched! Message from {sender_name} (chat {chat_id})");

    add_chat_id_to_config(&config_path, chat_id).await?;

    let _ = telegram::send_text(&bot, chat_id, "Paired successfully.").await;

    println!("\n  Paired chat {chat_id}. Restart the daemon to pick up the change.");
    Ok(())
}

/// Generate a 6-character alphanumeric pairing code.
///
/// Uses an unambiguous alphabet (no I/O/0/1) to minimise transcription errors.
pub(crate) fn generate_pairing_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let bytes = uuid::Uuid::new_v4().into_bytes();
    (0..6)
        .map(|i| ALPHABET[(bytes[i] as usize) % ALPHABET.len()] as char)
        .collect()
}

/// Poll `getUpdates` for up to `timeout_secs`, looking for a message whose
/// trimmed text matches `expected_code` (case-insensitive).
///
/// Returns `(chat_id, sender_name)` on match.
pub(crate) async fn poll_for_code(
    bot: &Bot,
    expected_code: &str,
    timeout_secs: u64,
) -> Result<(i64, String)> {
    // Drain stale updates.
    let mut offset: i32 = 0;
    let last = bot
        .get_updates()
        .offset(-1)
        .limit(1)
        .timeout(0)
        .await
        .context("failed to drain stale updates")?;
    if let Some(update) = last.last() {
        offset = telegram::next_offset(update.id.0);
    }

    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        let now = tokio::time::Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            anyhow::bail!("Timed out waiting for pairing code");
        }

        let poll_timeout = remaining.as_secs().min(30) as u32;

        let updates = tokio::time::timeout(
            remaining,
            bot.get_updates()
                .offset(offset)
                .timeout(poll_timeout)
                .send(),
        )
        .await
        .context("timeout")?
        .context("getUpdates failed")?;

        for update in updates {
            offset = telegram::next_offset(update.id.0);

            if let UpdateKind::Message(msg) = update.kind {
                let text = msg.text().unwrap_or("").trim();
                if text.eq_ignore_ascii_case(expected_code) {
                    let name = msg
                        .from
                        .as_ref()
                        .map_or("unknown", |u| u.first_name.as_str())
                        .to_string();
                    return Ok((msg.chat.id.0, name));
                }
            }
        }
    }
}

/// Bail if the daemon is running (it consumes `getUpdates`, so pairing
/// cannot work alongside it).
///
/// Uses create + try_lock (matching main.rs) to avoid a TOCTOU race where
/// the daemon creates the lock file between our exists() check and open().
fn check_daemon_not_running(data_dir: &std::path::Path) -> Result<()> {
    let lock_path = data_dir.join("daemon.lock");
    let f = std::fs::File::create(&lock_path)
        .with_context(|| format!("Failed to create {}", lock_path.display()))?;
    match f.try_lock() {
        Ok(()) => {
            // We got the lock — daemon is not running. Drop releases it.
            drop(f);
            Ok(())
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!(
                "The daemon is currently running (holds {}).\n\
                 Stop it first, then re-run `crustyclaw pair`.",
                lock_path.display()
            );
        }
        Err(std::fs::TryLockError::Error(e)) => {
            eprintln!(
                "  Warning: could not check daemon lock ({}): {e}",
                lock_path.display()
            );
            Ok(())
        }
    }
}

/// Add `chat_id` to the config file.
///
/// If `admin_chat_id` is 0 (unset), sets it instead. Preserves all other
/// fields by round-tripping through `serde_json::Value`.
async fn add_chat_id_to_config(config_path: &std::path::Path, chat_id: i64) -> Result<()> {
    let contents = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    let mut config: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse {}", config_path.display()))?;

    let obj = config
        .as_object_mut()
        .context("config is not a JSON object")?;

    let admin_id = obj
        .get("admin_chat_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    if admin_id == 0 {
        obj.insert(
            "admin_chat_id".to_string(),
            serde_json::Value::Number(chat_id.into()),
        );
        println!("  Set as admin (admin_chat_id = {chat_id})");
    } else if admin_id == chat_id {
        println!("  Chat {chat_id} is already the admin.");
        return Ok(());
    } else {
        let arr = obj
            .entry("allowed_chat_ids")
            .or_insert_with(|| serde_json::json!([]));
        let ids = arr
            .as_array_mut()
            .context("allowed_chat_ids is not an array")?;

        if ids.iter().any(|v| v.as_i64() == Some(chat_id)) {
            println!("  Chat {chat_id} is already in allowed_chat_ids.");
            return Ok(());
        }

        ids.push(serde_json::json!(chat_id));
        println!("  Added {chat_id} to allowed_chat_ids");
    }

    let json = serde_json::to_string_pretty(&config)?;
    let path = config_path.to_path_buf();
    tokio::task::spawn_blocking(move || util::write_private(&path, json.as_bytes()))
        .await
        .context("write config task panicked")??;

    Ok(())
}
