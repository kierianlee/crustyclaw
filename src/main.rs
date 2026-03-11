use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

mod claude;
mod common;
mod pair;
#[cfg(unix)]
mod permission;
mod scheduler;
mod setup;
mod telegram;
mod web;

use common::{config, status, util};

/// Cap for system prompt size (passed as CLI arg; must stay within OS `ARG_MAX`).
const MAX_SYSTEM_PROMPT_BYTES: usize = 128 * 1024;

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("crustyclaw=info,warn")),
        )
        .init();

    // Subcommands that exit before daemon init.
    match std::env::args().nth(1).as_deref() {
        Some("statusline") => {
            let data_dir = config::data_dir();
            status::print_statusline(&data_dir.join("status.json"));
            return Ok(());
        }
        Some("setup") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let opts = parse_setup_args(&args);
            return setup::run_with_opts(opts).await;
        }
        Some("pair") => {
            return pair::run().await;
        }
        #[cfg(unix)]
        Some("hook-handler") => {
            let data_dir = config::data_dir();
            let socket_path = data_dir.join("permission.sock");
            let file_timeout =
                tokio::task::spawn_blocking(move || config::read_approval_timeout(&data_dir))
                    .await
                    .unwrap_or(None);
            let env_timeout = std::env::var("CRUSTYCLAW_APPROVAL_TIMEOUT")
                .ok()
                .and_then(|v| match v.parse::<u64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        eprintln!(
                            "Warning: CRUSTYCLAW_APPROVAL_TIMEOUT is invalid, falling back to config/default"
                        );
                        None
                    }
                });
            let approval_timeout = env_timeout.or(file_timeout).unwrap_or(120);
            return permission::handle_hook(&socket_path, approval_timeout.saturating_add(30))
                .await;
        }
        #[cfg(not(unix))]
        Some("hook-handler") => {
            eprintln!("Error: hook-handler requires Unix (uses Unix domain sockets)");
            std::process::exit(1);
        }
        Some("update") => {
            return run_update().await;
        }
        Some("start") => {
            return start_daemon().await;
        }
        Some("help" | "--help" | "-h") => {
            eprintln!("Usage: crustyclaw [command]");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  (none)         Start the daemon (foreground)");
            eprintln!("  start          Start the daemon (background)");
            eprintln!("  setup          Interactive first-time setup");
            eprintln!("                   --token <T>    Provide bot token non-interactively");
            eprintln!("                   --yes          Auto-confirm overwrites");
            eprintln!("  pair           Pair a new Telegram user");
            eprintln!("  update         Update to the latest release");
            eprintln!("  statusline     Print daemon status (for shell integration)");
            #[cfg(unix)]
            eprintln!("  hook-handler   Handle Claude Code PreToolUse hooks (internal)");
            return Ok(());
        }
        Some(cmd) => {
            eprintln!("Unknown command: {cmd}");
            eprintln!("Run `crustyclaw help` for usage.");
            std::process::exit(1);
        }
        None => {}
    }

    tracing::info!("Starting crustyclaw daemon v{}", env!("CARGO_PKG_VERSION"));

    let data_dir = config::data_dir();
    config::ensure_data_dir(&data_dir).await?;

    // Acquire exclusive lock to prevent multiple daemon instances from
    // racing on shared state files (session.json, scheduler.json, etc.)
    // and causing duplicate Telegram message processing.
    // Held until end of main to keep the exclusive file lock active.
    // On lock failure we exit(1) without running shutdown — no other
    // resources have been acquired yet, so this is safe.
    let _daemon_lock = {
        let lock_path = data_dir.join("daemon.lock");
        let f = std::fs::File::create(&lock_path).unwrap_or_else(|e| {
            eprintln!(
                "Error: failed to create lock file {}: {e}",
                lock_path.display()
            );
            std::process::exit(1);
        });
        if let Err(e) = f.try_lock() {
            match e {
                std::fs::TryLockError::WouldBlock => {
                    eprintln!("Error: another crustyclaw instance is already running");
                    eprintln!(
                        "(could not acquire exclusive lock on {})",
                        lock_path.display()
                    );
                }
                std::fs::TryLockError::Error(io_err) => {
                    eprintln!(
                        "Error: could not acquire lock on {}: {io_err}",
                        lock_path.display()
                    );
                }
            }
            std::process::exit(1);
        }
        // Write our PID so `signal_stop()` can find and SIGTERM us.
        use std::io::Write;
        let _ = (&f).write_all(format!("{}", std::process::id()).as_bytes());
        f
    };

    {
        let dd = data_dir.clone();
        tokio::task::spawn_blocking(move || {
            util::cleanup_stale_tmp_files(&dd);
            util::cleanup_stale_inbox_files(&dd, std::time::Duration::from_secs(3600));
        })
        .await?;
    }

    tracing::info!(data_dir = %data_dir.display(), "Data directory");

    let config_path = config::config_path(&data_dir);
    if !config_path.exists() {
        tracing::info!("No config found — launching first-time setup");
        setup::run().await?;
        // If setup didn't create the config (e.g. user aborted), exit cleanly.
        if !config_path.exists() {
            anyhow::bail!("Setup did not create config. Run `crustyclaw setup` to try again.");
        }
    }
    let mut config = match config::DaemonConfig::load(&config_path).await {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(error = %e, "Failed to load config");
            anyhow::bail!("{e}");
        }
    };

    tracing::info!(model = %config.model, admin = config.admin_chat_id, permission_mode = ?config.permission_mode, "Config loaded");

    // Quick sanity check that the claude CLI is reachable.
    match tokio::process::Command::new("claude")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
    {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(exit = %s, "`claude --version` failed — invocations may fail"),
        Err(e) => tracing::warn!(error = %e, "`claude` not found in PATH — invocations will fail"),
    }

    // Soul prompts are always prepended to the system prompt, even when
    // CRUSTYCLAW_SYSTEM_PROMPT overrides the default. This is intentional:
    // soul prompts define persistent personality/instructions that survive
    // env-var overrides.
    let soul_prompts = config::load_soul_prompts(&data_dir).await;
    if !soul_prompts.is_empty() {
        tracing::info!(chars = soul_prompts.len(), "Soul prompts loaded");
        config.system_prompt = format!("{}\n\n{}", soul_prompts.trim(), config.system_prompt);
    }

    // Cap the final system prompt to stay well within OS ARG_MAX limits
    // (typically 256 KiB on macOS, 2 MiB on Linux, shared with environment).
    // The prompt is passed as a CLI argument via --append-system-prompt.
    if config.system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
        let truncated = util::truncate_str(&config.system_prompt, MAX_SYSTEM_PROMPT_BYTES);
        tracing::warn!(
            original_bytes = config.system_prompt.len(),
            truncated_bytes = truncated.len(),
            "System prompt exceeds {MAX_SYSTEM_PROMPT_BYTES} bytes, truncating to avoid ARG_MAX"
        );
        config.system_prompt = truncated.to_string();
    }

    let session = Arc::new(claude::SessionManager::load_or_create(&data_dir).await?);
    {
        let state = session.snapshot().await;
        let sid = state
            .session_id
            .map_or_else(|| "none (fresh)".into(), util::short_id);
        tracing::info!(session_id = %sid, invocations = state.invocation_count, "Session loaded");
    }

    let config = Arc::new(config);

    // Check for updates periodically in the background.
    // Writes latest version to data_dir/latest-version for the statusline.
    if config.update_check_interval_secs > 0 {
        let dd = data_dir.clone();
        let interval_secs = config.update_check_interval_secs;
        tokio::task::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if let Err(e) = check_for_update(&dd).await {
                    tracing::debug!(error = %e, "Background update check failed");
                }
            }
        });
    }

    let working_dir = config.effective_working_dir(&data_dir);

    // Register statusLine in ~/.claude/settings.json so Claude Code shows our status bar.
    status::install_statusline(&data_dir);

    let status_tracker = Arc::new(status::StatusTracker::new(config.heartbeat_enabled));
    let status_writer_handle =
        status::spawn_writer(status_tracker.clone(), data_dir.join("status.json"));

    let data_dir_arc: Arc<Path> = Arc::from(data_dir.as_path());
    let (queue, queue_worker_handle) = claude::InvocationQueue::spawn(
        config.clone(),
        session.clone(),
        data_dir_arc.clone(),
        status_tracker.clone(),
    );
    let queue = Arc::new(queue);

    let chat_log = Arc::new(common::chatlog::ChatLog::new());

    let shared_bot = Arc::new(tokio::sync::RwLock::new(
        telegram::make_bot(&config.telegram_token)?,
    ));

    let scheduler = scheduler::Scheduler::new(
        data_dir.join("scheduler.json"),
        queue.clone(),
        shared_bot.clone(),
        config.clone(),
        status_tracker.clone(),
    )
    .await?;

    #[cfg(unix)]
    let (permission_server, permission_handle) = if config.telegram_approval {
        // Auto-install the hook into {working_dir}/.claude/settings.json.
        // Uses absolute binary path and is loaded via --settings, so it's
        // scoped to the crustyclaw subprocess only.
        let wd = working_dir.clone();
        match tokio::task::spawn_blocking(move || permission::install_hook(&wd)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "Failed to install PreToolUse hook"),
            Err(e) => tracing::error!(error = %e, "install_hook task panicked"),
        }

        let wd = working_dir.clone();
        let hook_installed =
            match tokio::task::spawn_blocking(move || permission::is_hook_installed(&wd)).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "is_hook_installed task panicked");
                    false
                }
            };
        if !hook_installed {
            tracing::warn!(
                "PreToolUse hook not detected after installation attempt. \
                 Permission approval may not work."
            );
        }

        let server = permission::PermissionServer::new(
            data_dir.join("permission.sock"),
            shared_bot.clone(),
            config.admin_chat_id,
            config.approval_timeout_secs,
        );
        let handle = server.spawn()?;
        (Some(server), Some(handle))
    } else {
        (None, None)
    };
    #[cfg(not(unix))]
    let permission_server: Option<()> = {
        if config.telegram_approval {
            tracing::warn!("telegram_approval requires Unix — disabled on this platform");
        }
        None
    };
    #[cfg(not(unix))]
    let permission_handle: Option<tokio::task::JoinHandle<()>> = None;

    let status_path = data_dir.join("status.json");

    let heartbeat_handle =
        scheduler::heartbeat::spawn(
            config.clone(),
            queue.clone(),
            shared_bot.clone(),
            status_tracker.clone(),
        );

    let web_handle = if config.web_enabled {
        Some(web::spawn(
            config.web_port,
            status_tracker.clone(),
            scheduler.clone(),
            chat_log.clone(),
            config.clone(),
            data_dir_arc.clone(),
        ))
    } else {
        None
    };

    let telegram_handle = telegram::spawn(
        config.clone(),
        queue.clone(),
        session.clone(),
        scheduler.clone(),
        data_dir,
        shared_bot,
        permission_server.clone(),
        chat_log,
    );

    tracing::info!("All subsystems started. Waiting for shutdown signal...");

    tokio::select! {
        () = shutdown_signal() => {}
        result = telegram_handle => {
            match result {
                Ok(()) => tracing::warn!("Telegram poll loop exited unexpectedly"),
                Err(e) => tracing::error!(error = %e, "Telegram poll loop panicked"),
            }
        }
        result = queue_worker_handle => {
            match result {
                Ok(()) => tracing::error!("Queue worker exited unexpectedly — initiating shutdown"),
                Err(e) => tracing::error!(error = %e, "Queue worker panicked — initiating shutdown"),
            }
        }
        result = async {
            if let Some(h) = heartbeat_handle {
                h.await
            } else {
                std::future::pending().await
            }
        } => {
            match result {
                Ok(()) => tracing::warn!("Heartbeat loop exited unexpectedly"),
                Err(e) => tracing::error!(error = %e, "Heartbeat loop panicked"),
            }
        }
        result = async {
            if let Some(h) = permission_handle {
                h.await
            } else {
                std::future::pending().await
            }
        } => {
            match result {
                Ok(()) => tracing::error!("Permission server exited — initiating shutdown"),
                Err(e) => tracing::error!(error = %e, "Permission server panicked — initiating shutdown"),
            }
        }
        result = status_writer_handle => {
            match result {
                Ok(()) => tracing::warn!("Status writer exited unexpectedly"),
                Err(e) => tracing::error!(error = %e, "Status writer panicked"),
            }
        }
        result = async {
            if let Some(h) = web_handle {
                h.await
            } else {
                std::future::pending().await
            }
        } => {
            match result {
                Ok(()) => tracing::warn!("Web server exited unexpectedly"),
                Err(e) => tracing::error!(error = %e, "Web server panicked"),
            }
        }
    }

    tracing::info!("Shutting down gracefully — waiting for in-flight requests...");
    scheduler.shutdown().await;
    queue.close();

    #[cfg(unix)]
    if let Some(ref perm) = permission_server {
        perm.cleanup();
    }

    if let Err(e) = session.persist().await {
        tracing::error!(error = %e, "Failed to persist session on shutdown");
    }

    status::flush_final(&status_tracker, &status_path).await;
    status::remove_statusline();

    tracing::info!("Shutdown complete");
    Ok(())
}

/// Parse `crustyclaw setup` flags: `--token <T>`, `--yes`.
fn parse_setup_args(args: &[String]) -> setup::SetupOpts {
    let mut opts = setup::SetupOpts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--token" => {
                i += 1;
                if i < args.len() {
                    opts.token = Some(args[i].clone());
                }
            }
            "--yes" | "-y" => {
                opts.yes = true;
            }
            other => {
                eprintln!("Warning: unknown setup flag: {other}");
            }
        }
        i += 1;
    }
    opts
}

/// Update crustyclaw to the latest release using install-binary.sh.
async fn run_update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    eprintln!("Current version: v{current}");

    // Find install-binary.sh relative to the running binary.
    // Expected layout: plugin/bin/crustyclaw + plugin/scripts/install-binary.sh
    let exe = std::env::current_exe()?;
    let script = exe
        .parent()
        .and_then(|bin_dir| {
            let s = bin_dir.parent()?.join("scripts/install-binary.sh");
            if s.exists() { Some(s) } else { None }
        });

    let script = match script {
        Some(s) => s,
        None => {
            anyhow::bail!(
                "Could not find install-binary.sh. \
                 If you built from source, run: ./build.sh"
            );
        }
    };

    // Stop daemon if running before replacing the binary
    let was_running = is_daemon_running();
    if was_running {
        eprintln!("Stopping daemon for update...");
        signal_stop();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    let status = std::process::Command::new("bash")
        .arg(&script)
        .arg("--force")
        .status()?;

    if !status.success() {
        anyhow::bail!("Update failed");
    }

    // Clear the latest-version cache so statusline stops showing the hint
    let data_dir = config::data_dir();
    let _ = std::fs::remove_file(data_dir.join("latest-version"));

    // Sync plugin files (commands, scripts, hooks) to the active Claude plugin
    // cache directory. When the plugin version bumps, Claude Code creates a new
    // cache dir from the repo — but install-binary.sh only updates the binary.
    // This ensures the cached plugin files match the new version.
    if let Some(plugin_root) = exe.parent().and_then(|b| b.parent()) {
        sync_plugin_cache(plugin_root);
    }

    if was_running {
        eprintln!("Restarting daemon...");
        start_daemon().await?;
    }

    Ok(())
}

/// Copy plugin files (commands, scripts, hooks) from the current plugin root
/// to the active Claude Code plugin cache directory. This handles the case
/// where the marketplace version bumps and Claude creates a new cache dir
/// that only has repo files (no binary, possibly outdated commands).
fn sync_plugin_cache(current_plugin_root: &std::path::Path) {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    let cache_base = std::path::PathBuf::from(&home)
        .join(".claude/plugins/cache/crustyclaw/crustyclaw");

    if !cache_base.exists() {
        return;
    }

    // Find the newest version directory in the cache
    let newest = match std::fs::read_dir(&cache_base) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .max_by_key(|e| e.file_name()),
        Err(_) => return,
    };

    let target = match newest {
        Some(entry) => entry.path(),
        None => return,
    };

    // Don't sync to ourselves
    if target == current_plugin_root {
        return;
    }

    // Sync commands, scripts, hooks directories
    for dir_name in &["commands", "scripts", "hooks"] {
        let src = current_plugin_root.join(dir_name);
        let dst = target.join(dir_name);
        if src.is_dir() {
            let _ = std::fs::create_dir_all(&dst);
            if let Ok(entries) = std::fs::read_dir(&src) {
                for entry in entries.flatten() {
                    let dest_file = dst.join(entry.file_name());
                    let _ = std::fs::copy(entry.path(), &dest_file);
                }
            }
        }
    }

    // Copy binary if it exists
    let src_bin = current_plugin_root.join("bin/crustyclaw");
    if src_bin.exists() {
        let dst_bin_dir = target.join("bin");
        let _ = std::fs::create_dir_all(&dst_bin_dir);
        let dst_bin = dst_bin_dir.join("crustyclaw");
        if std::fs::copy(&src_bin, &dst_bin).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dst_bin, std::fs::Permissions::from_mode(0o755));
            }
        }
    }

    eprintln!("Synced plugin files to cache: {}", target.display());
}

/// Check GitHub for the latest release and cache it for the statusline.
async fn check_for_update(data_dir: &std::path::Path) -> Result<()> {
    let cache_path = data_dir.join("latest-version");

    let output = tokio::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "5",
               "https://api.github.com/repos/kierianlee/crustyclaw/releases/latest"])
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!("curl failed");
    }

    let body: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if let Some(tag) = body.get("tag_name").and_then(|v| v.as_str()) {
        tokio::fs::write(&cache_path, tag).await?;
    }

    Ok(())
}

fn is_daemon_running() -> bool {
    let data_dir = config::data_dir();
    let lock_path = data_dir.join("daemon.lock");
    if lock_path.exists() {
        if let Ok(f) = std::fs::File::open(&lock_path) {
            return f.try_lock().is_err();
        }
    }
    false
}

fn signal_stop() {
    #[cfg(unix)]
    {
        use std::io::Read;
        let data_dir = config::data_dir();
        let lock_path = data_dir.join("daemon.lock");
        if let Ok(mut f) = std::fs::File::open(&lock_path) {
            let mut pid_str = String::new();
            if f.read_to_string(&mut pid_str).is_ok() {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    unsafe { libc::kill(pid, libc::SIGTERM); }
                }
            }
        }
    }
}

async fn start_daemon() -> Result<()> {
    let data_dir = config::data_dir();

    // Check config exists
    let config_path = config::config_path(&data_dir);
    if !config_path.exists() {
        anyhow::bail!("No config found. Run `crustyclaw setup` first.");
    }

    // Check if already running via lock file
    let lock_path = data_dir.join("daemon.lock");
    if lock_path.exists() {
        if let Ok(f) = std::fs::File::open(&lock_path) {
            if f.try_lock().is_err() {
                eprintln!("crustyclaw is already running");
                return Ok(());
            }
        }
    }

    let exe = std::env::current_exe()?;
    let log = std::fs::File::create("/tmp/crustyclaw.log")?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.env_remove("CLAUDECODE");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(log.try_clone()?);
    cmd.stderr(log);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Safety: setsid is async-signal-safe and we call no other functions.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = cmd.spawn()?;
    eprintln!("crustyclaw started (pid: {})", child.id());
    Ok(())
}

/// Wait for a shutdown signal (SIGINT or SIGTERM).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("Received SIGINT, shutting down...");
                    }
                    _ = sigterm.recv() => {
                        tracing::info!("Received SIGTERM, shutting down...");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to install SIGTERM handler, using SIGINT only");
                tokio::signal::ctrl_c().await.ok();
                tracing::info!("Received SIGINT, shutting down...");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("Received SIGINT, shutting down...");
    }
}
