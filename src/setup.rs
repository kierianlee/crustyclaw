use std::io::{self, Write};

use anyhow::{Context, Result};
use teloxide::prelude::*;

use crate::common::{config, util};
use crate::telegram;

/// Run the interactive setup wizard.
pub async fn run() -> Result<()> {
    println!("crustyclaw setup\n");

    // Step 1: Claude authentication
    let auth = check_claude_auth().await?;
    println!("  Claude auth: {auth}\n");

    // Step 2: Telegram configuration
    let (token, chat_id) = configure_telegram().await?;

    // Step 3: Write config
    write_config(&token, chat_id).await?;

    println!("\nSetup complete. Run `crustyclaw` to start the daemon.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 1: Claude auth
// ---------------------------------------------------------------------------

async fn check_claude_auth() -> Result<String> {
    println!("Step 1: Claude authentication\n");

    // Check current auth status via `claude auth status` (works regardless of
    // where credentials are stored — keychain, file, API key, etc.).
    if let Some(method) = check_auth_status().await {
        println!("  Already authenticated ({method}).");
        return Ok(method);
    }

    // Not authenticated — run interactive login.
    println!("  No valid Claude authentication found.");
    println!("  Running `claude auth login` (this will open your browser)...\n");

    let status = tokio::process::Command::new("claude")
        .args(["auth", "login"])
        .env_remove("CLAUDECODE")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context("Failed to run `claude auth login`. Is claude-code installed?")?;

    if !status.success() {
        anyhow::bail!("`claude auth login` exited with {status}");
    }

    // Verify login succeeded.
    print!("\n  Validating... ");
    io::stdout().flush()?;
    match check_auth_status().await {
        Some(method) => {
            println!("ok");
            Ok(method)
        }
        None => {
            println!("failed");
            anyhow::bail!("Authentication completed but validation failed");
        }
    }
}

/// Check `claude auth status` and return the auth method if logged in.
async fn check_auth_status() -> Option<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("claude")
            .args(["auth", "status"])
            .env_remove("CLAUDECODE")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    if json.get("loggedIn")?.as_bool()? {
        let method = json
            .get("authMethod")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        Some(method.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Step 2: Telegram
// ---------------------------------------------------------------------------

async fn configure_telegram() -> Result<(String, i64)> {
    println!("Step 2: Telegram bot\n");
    println!("  Create a bot via @BotFather on Telegram to get a token.\n");

    let token = prompt("  Bot token: ").await?;
    if token.is_empty() {
        anyhow::bail!("Bot token cannot be empty");
    }

    // Validate by calling getMe. Use make_bot so the HTTP client timeout
    // exceeds the long-polling timeout used in detect_chat_id.
    print!("  Validating... ");
    io::stdout().flush()?;

    let bot = telegram::make_bot(&token)?;
    let me = bot
        .get_me()
        .await
        .map_err(|e| anyhow::anyhow!("Invalid bot token: {e}"))?;
    let username = me.username.as_deref().unwrap_or("unknown");
    println!("ok (@{username})\n");

    if let Err(e) = bot.delete_webhook().await {
        println!("  Warning: failed to clear webhook ({e}). Chat detection may not work.");
    }

    let code = crate::pair::generate_pairing_code();
    println!("  Send this code to @{username}: {code}");
    println!("  Waiting up to 60 seconds...\n");

    let chat_id = match crate::pair::poll_for_code(&bot, &code, 60).await {
        Ok((id, name)) => {
            println!("  Received code from {name} (chat {id})");
            id
        }
        Err(_) => {
            println!("  Timed out waiting for the code.");
            prompt_chat_id().await?
        }
    };

    Ok((token, chat_id))
}


async fn prompt_chat_id() -> Result<i64> {
    let input = prompt("  Admin chat ID: ").await?;
    input
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid chat ID: must be an integer"))
}

// ---------------------------------------------------------------------------
// Step 3: Write config
// ---------------------------------------------------------------------------

async fn write_config(token: &str, chat_id: i64) -> Result<()> {
    println!("\nStep 3: Writing configuration\n");

    let data_dir = config::data_dir();
    config::ensure_data_dir(&data_dir).await?;

    let config_path = config::config_path(&data_dir);

    if tokio::fs::try_exists(&config_path).await.unwrap_or(false)
        && !prompt_yes_no(&format!(
            "  {} already exists. Overwrite?",
            config_path.display()
        ))
        .await?
    {
        println!("  Keeping existing config.");
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&serde_json::json!({
        "telegram_token": token,
        "admin_chat_id": chat_id,
    }))?;

    let path_display = config_path.display().to_string();
    let bytes = json.into_bytes();
    tokio::task::spawn_blocking(move || util::write_private(&config_path, &bytes))
        .await
        .context("write_config task panicked")??;

    println!("  Wrote {path_display}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn prompt(msg: &str) -> Result<String> {
    let prompt = msg.to_string();
    tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        Ok::<String, std::io::Error>(input.trim().to_string())
    })
    .await
    .map_err(|e| anyhow::anyhow!("prompt task failed: {e}"))?
    .map_err(anyhow::Error::from)
}

async fn prompt_yes_no(msg: &str) -> Result<bool> {
    let prompt = format!("{msg} [Y/n] ");
    tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        Ok::<bool, std::io::Error>(!matches!(input.trim().to_lowercase().as_str(), "n" | "no"))
    })
    .await
    .map_err(|e| anyhow::anyhow!("prompt task failed: {e}"))?
    .map_err(anyhow::Error::from)
}
