use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct DaemonConfig {
    // Telegram
    pub telegram_token: String,
    pub admin_chat_id: i64,
    #[serde(default)]
    pub allowed_chat_ids: HashSet<i64>,

    // Claude CLI
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_tools: Option<Vec<String>>,

    // Daemon
    #[serde(default = "default_subprocess_timeout")]
    pub subprocess_timeout_secs: u64,
    #[serde(default = "default_rate_limit_retry")]
    pub rate_limit_retry_secs: u64,

    // Heartbeat
    #[serde(default = "default_true")]
    pub heartbeat_enabled: bool,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_heartbeat_prompt")]
    pub heartbeat_prompt: String,
    #[serde(default = "default_heartbeat_notify")]
    pub heartbeat_notify_telegram: bool,

    // Web UI
    #[serde(default = "default_true")]
    pub web_enabled: bool,
    #[serde(default = "default_web_port")]
    pub web_port: u16,

    // Voice transcription
    #[serde(default)]
    pub voice_enabled: bool,
    #[serde(default)]
    pub whisper_model_path: Option<PathBuf>,

    // Telegram tool approval (PreToolUse hooks)
    #[serde(default = "default_true")]
    pub telegram_approval: bool,
    #[serde(default = "default_approval_timeout")]
    pub approval_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    DangerouslySkip,
    #[default]
    AcceptEdits,
    /// Use Claude's default interactive permission mode.
    ///
    /// Serializes as `"default"` for config-file backward compatibility;
    /// `"interactive"` is accepted as an alias.
    #[serde(rename = "default", alias = "interactive")]
    Interactive,
}

fn default_model() -> String {
    "sonnet".into()
}

fn default_system_prompt() -> String {
    "You are crustyclaw, a personal AI assistant daemon. You are being controlled remotely via Telegram. \
     Be concise in your responses. You have access to MCP tools in your environment."
        .into()
}

fn default_subprocess_timeout() -> u64 {
    300
}

fn default_rate_limit_retry() -> u64 {
    30
}

fn default_heartbeat_interval() -> u64 {
    900 // 15 minutes
}

fn default_heartbeat_prompt() -> String {
    "Review any pending context. If something needs attention, summarize it briefly. \
     Otherwise reply with HEARTBEAT_OK."
        .into()
}

fn default_heartbeat_notify() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_web_port() -> u16 {
    11111
}

fn default_approval_timeout() -> u64 {
    120
}

impl DaemonConfig {
    /// Load config from file, then apply env var overrides.
    pub async fn load(path: &Path) -> Result<Self> {
        let mut config: Self = match tokio::fs::read_to_string(path).await {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("Failed to parse config from {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(
                    "Config file not found at {}. Create it with telegram_token and admin_chat_id.",
                    path.display()
                );
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("Failed to read config from {}", path.display())));
            }
        };

        config.apply_env_overrides();
        config.validate()?;

        if let Some(path) = &config.working_directory {
            let meta = tokio::fs::metadata(path)
                .await
                .with_context(|| format!("working_directory does not exist: {}", path.display()))?;
            anyhow::ensure!(
                meta.is_dir(),
                "working_directory is not a directory: {}",
                path.display()
            );
        }

        if config.voice_enabled {
            // validate() already ensures whisper_model_path.is_some() when voice_enabled.
            let path = config.whisper_model_path.as_ref().unwrap();
            let exists = tokio::fs::try_exists(path)
                .await
                .with_context(|| format!("Failed to check whisper path {}", path.display()))?;
            anyhow::ensure!(exists, "whisper model not found at {}", path.display());
        }

        Ok(config)
    }

    /// Apply environment variable overrides for security-sensitive fields.
    ///
    /// Only token and admin chat ID are overridable via env vars (useful for
    /// Docker secrets). Everything else goes through the config file.
    fn apply_env_overrides(&mut self) {
        if let Some(v) = std::env::var("CRUSTYCLAW_TELEGRAM_TOKEN").ok().filter(|s| !s.is_empty()) {
            self.telegram_token = v;
        }
        if let Some(v) = std::env::var("CRUSTYCLAW_ADMIN_CHAT_ID")
            .ok()
            .and_then(|s| match s.parse() {
                Ok(id) => Some(id),
                Err(_) => {
                    tracing::warn!(value = %s, "CRUSTYCLAW_ADMIN_CHAT_ID is not a valid integer, ignoring");
                    None
                }
            })
        {
            self.admin_chat_id = v;
        }
    }

    /// Validate that required fields are present and sensible.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.model.is_empty(), "model cannot be empty");
        anyhow::ensure!(
            !self.telegram_token.is_empty(),
            "telegram_token cannot be empty"
        );
        anyhow::ensure!(
            self.admin_chat_id != 0,
            "admin_chat_id is missing or zero — set it to your Telegram user ID \
             (send a message to @userinfobot to find it)"
        );
        if self.admin_chat_id < 0 {
            tracing::warn!(
                admin_chat_id = self.admin_chat_id,
                "admin_chat_id is negative (group chat). Permission approval callbacks \
                 require a positive user ID — use your personal chat ID instead"
            );
        }
        anyhow::ensure!(
            self.subprocess_timeout_secs >= 10,
            "subprocess_timeout_secs must be at least 10 (got {}, which would immediately \
             time out most Claude invocations)",
            self.subprocess_timeout_secs,
        );
        anyhow::ensure!(
            self.rate_limit_retry_secs >= 1,
            "rate_limit_retry_secs must be at least 1 (got {}, zero would hammer the API)",
            self.rate_limit_retry_secs,
        );
        if let Some(budget) = self.max_budget_usd {
            anyhow::ensure!(
                budget > 0.0,
                "max_budget_usd must be positive (got {budget})"
            );
        }
        anyhow::ensure!(
            self.approval_timeout_secs >= 5,
            "approval_timeout_secs must be at least 5 (got {}, too short to approve anything)",
            self.approval_timeout_secs,
        );
        if self.heartbeat_enabled {
            anyhow::ensure!(
                self.heartbeat_interval_secs >= 30,
                "heartbeat_interval_secs must be at least 30"
            );
        }
        if self.telegram_approval {
            if matches!(self.permission_mode, PermissionMode::DangerouslySkip) {
                anyhow::bail!(
                    "telegram_approval requires permission_mode != dangerously_skip \
                     (dangerously_skip disables hooks entirely)"
                );
            }
            anyhow::ensure!(
                self.admin_chat_id > 0,
                "telegram_approval requires a positive admin_chat_id (personal user ID). \
                 Group chat IDs are negative and cannot receive permission callbacks"
            );
        }
        if self.voice_enabled {
            anyhow::ensure!(
                self.whisper_model_path.is_some(),
                "whisper_model_path must be set when voice_enabled is true"
            );
        }
        if let (Some(allowed), Some(disallowed)) = (&self.allowed_tools, &self.disallowed_tools) {
            let disallowed_set: HashSet<&str> = disallowed.iter().map(String::as_str).collect();
            let overlap: Vec<&str> = allowed
                .iter()
                .map(String::as_str)
                .filter(|tool| disallowed_set.contains(tool))
                .collect();
            if !overlap.is_empty() {
                anyhow::bail!(
                    "Tools cannot be present in both allowed_tools and disallowed_tools: {:?}",
                    overlap
                );
            }
        }
        Ok(())
    }

    /// Check whether a chat ID is authorized to interact with the daemon.
    pub fn is_chat_allowed(&self, chat_id: i64) -> bool {
        chat_id == self.admin_chat_id || self.allowed_chat_ids.contains(&chat_id)
    }

    /// Resolve the effective working directory for child claude processes.
    pub fn effective_working_dir(&self, data_dir: &Path) -> PathBuf {
        self.working_directory
            .as_deref()
            .unwrap_or(data_dir)
            .to_path_buf()
    }
}

/// Resolve the data directory. Precedence: `CRUSTYCLAW_DATA_DIR` env > `~/.crustyclaw`
pub fn data_dir() -> PathBuf {
    if let Ok(v) = std::env::var("CRUSTYCLAW_DATA_DIR") {
        PathBuf::from(v)
    } else if let Some(home) = dirs::home_dir() {
        home.join(".crustyclaw")
    } else {
        eprintln!("Warning: cannot determine home directory, falling back to ./.crustyclaw");
        PathBuf::from(".crustyclaw")
    }
}

/// Ensure the data directory and its subdirectories exist.
pub async fn ensure_data_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("Failed to create data directory {}", dir.display()))?;
    let inbox = dir.join("inbox");
    tokio::fs::create_dir_all(&inbox)
        .await
        .with_context(|| format!("Failed to create inbox directory {}", inbox.display()))?;
    let prompts = dir.join("prompts");
    tokio::fs::create_dir_all(&prompts)
        .await
        .with_context(|| format!("Failed to create prompts directory {}", prompts.display()))?;
    Ok(())
}

/// Path to the config file within the data directory.
pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.json")
}

/// Read just the `approval_timeout_secs` value from config.json synchronously.
///
/// Used by the hook-handler subprocess to derive its own timeout without
/// loading and validating the full `DaemonConfig`. Returns `None` if the
/// config file is missing, unreadable, or doesn't contain the field.
pub fn read_approval_timeout(data_dir: &Path) -> Option<u64> {
    let path = config_path(data_dir);
    let contents = std::fs::read_to_string(&path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&contents).ok()?;
    val.get("approval_timeout_secs")?.as_u64()
}

/// Load all `.md` files from `data_dir/prompts/`, sorted alphabetically,
/// and concatenate their contents.
///
/// Returns an empty string if the directory doesn't exist or contains no files.
/// Caps total size at 64 KiB — the combined system prompt is passed as a CLI
/// argument to `claude`, so it must stay well within OS `ARG_MAX` limits
/// (typically 256 KiB on macOS, 2 MiB on Linux, shared with the environment).
pub async fn load_soul_prompts(data_dir: &Path) -> String {
    const MAX_TOTAL_BYTES: usize = 64 * 1024;

    let prompts_dir = data_dir.join("prompts");
    let mut read_dir = match tokio::fs::read_dir(&prompts_dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return String::new(),
        Err(e) => {
            tracing::warn!(path = %prompts_dir.display(), error = %e, "Failed to read prompts directory");
            return String::new();
        }
    };

    let mut files: Vec<PathBuf> = Vec::new();
    loop {
        match read_dir.next_entry().await {
            Ok(Some(entry)) => {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    files.push(path);
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(
                    path = %prompts_dir.display(),
                    error = %e,
                    "Error while scanning prompts directory; using files discovered so far"
                );
                break;
            }
        }
    }

    files.sort();

    let mut parts = Vec::new();
    let mut total_bytes: usize = 0;
    for path in &files {
        match tokio::fs::read_to_string(path).await {
            Ok(contents) => {
                let trimmed = contents.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let separator_cost = if parts.is_empty() { 0 } else { 2 }; // "\n\n"
                if total_bytes + separator_cost + trimmed.len() > MAX_TOTAL_BYTES {
                    tracing::warn!(
                        file = %path.display(),
                        total_bytes,
                        limit = MAX_TOTAL_BYTES,
                        "Soul prompts exceeded {MAX_TOTAL_BYTES} bytes, skipping remaining files"
                    );
                    break;
                }
                total_bytes += separator_cost + trimmed.len();
                tracing::info!(file = %path.display(), "Loaded soul prompt");
                parts.push(trimmed.to_string());
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "Failed to read prompt file");
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config_json() -> String {
        serde_json::json!({
            "telegram_token": "123:ABC",
            "admin_chat_id": 42
        })
        .to_string()
    }

    #[tokio::test]
    async fn load_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        tokio::fs::write(&path, minimal_config_json())
            .await
            .unwrap();
        let cfg = DaemonConfig::load(&path).await.unwrap();
        assert_eq!(cfg.admin_chat_id, 42);
        assert_eq!(cfg.model, "sonnet");
        assert_eq!(cfg.subprocess_timeout_secs, 300);
    }

    #[tokio::test]
    async fn validate_rejects_zero_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let json = serde_json::json!({
            "telegram_token": "123:ABC",
            "admin_chat_id": 42,
            "subprocess_timeout_secs": 0
        })
        .to_string();
        tokio::fs::write(&path, json).await.unwrap();
        let err = DaemonConfig::load(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("subprocess_timeout_secs"),
            "expected timeout error, got: {err}"
        );
    }

    #[tokio::test]
    async fn validate_rejects_zero_rate_limit_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let json = serde_json::json!({
            "telegram_token": "123:ABC",
            "admin_chat_id": 42,
            "rate_limit_retry_secs": 0
        })
        .to_string();
        tokio::fs::write(&path, json).await.unwrap();
        let err = DaemonConfig::load(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("rate_limit_retry_secs"),
            "expected rate limit error, got: {err}"
        );
    }

    #[tokio::test]
    async fn validate_rejects_negative_admin_with_approval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let json = serde_json::json!({
            "telegram_token": "123:ABC",
            "admin_chat_id": -100123,
            "telegram_approval": true,
            "permission_mode": "accept_edits"
        })
        .to_string();
        tokio::fs::write(&path, json).await.unwrap();
        let err = DaemonConfig::load(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("positive admin_chat_id"),
            "expected admin_chat_id error, got: {err}"
        );
    }

    #[tokio::test]
    async fn validate_rejects_missing_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let json = serde_json::json!({
            "telegram_token": "123:ABC",
            "admin_chat_id": 42,
            "working_directory": "/definitely/missing/crustyclaw-working-dir"
        })
        .to_string();
        tokio::fs::write(&path, json).await.unwrap();
        let err = DaemonConfig::load(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("working_directory"),
            "expected working_directory error, got: {err}"
        );
    }

}
