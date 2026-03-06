use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::common::util::atomic_write;

// ---- Types ----

/// Persisted session state.
///
/// `session_id` is `None` until the first successful Claude invocation returns
/// a session ID. When `None`, the queue invokes `claude -p` without `--resume`
/// so Claude starts a fresh conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used: chrono::DateTime<chrono::Utc>,
    pub invocation_count: u64,
}

// ---- Session manager ----

pub struct SessionManager {
    state: RwLock<SessionState>,
    path: PathBuf,
}

impl SessionManager {
    /// Load existing session from disk or create a fresh one.
    pub async fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("session.json");

        let (state, is_new) = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(s) => (s, false),
                Err(e) => {
                    tracing::warn!(error = %e, "Corrupted session file, creating new session");
                    (Self::new_state(), true)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::new_state(), true),
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("Failed to read session from {}", path.display())))
            }
        };

        let mgr = Self {
            state: RwLock::new(state),
            path,
        };

        if is_new {
            mgr.persist().await?;
        }

        Ok(mgr)
    }

    fn new_state() -> SessionState {
        let now = chrono::Utc::now();
        SessionState {
            session_id: None,
            created_at: now,
            last_used: now,
            invocation_count: 0,
        }
    }

    /// Return a snapshot of the current session state.
    pub async fn snapshot(&self) -> SessionState {
        self.state.read().await.clone()
    }

    /// Return the current Claude session ID.
    pub async fn session_id(&self) -> Option<Uuid> {
        self.state.read().await.session_id
    }

    /// Record one or more completed invocations, updating the session ID if
    /// Claude returned a new one, and persist to disk atomically.
    pub async fn record_invocations(
        &self,
        new_session_id: Option<Uuid>,
        count: u64,
    ) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        let mut state = self.state.write().await;

        state.last_used = chrono::Utc::now();
        state.invocation_count = state.invocation_count.saturating_add(count);

        if let Some(new_id) = new_session_id {
            if state.session_id != Some(new_id) {
                let old = state
                    .session_id
                    .map_or_else(|| "none".into(), |id| id.to_string());
                tracing::info!(old = %old, new = %new_id, "Session ID updated");
                state.session_id = Some(new_id);
            }
        }

        let json = serde_json::to_string(&*state).context("Failed to serialize session")?;
        atomic_write(&self.path, json.as_bytes()).await
    }

    /// Reset the session so the next invocation starts a fresh conversation.
    pub async fn reset(&self) -> Result<()> {
        let mut state = self.state.write().await;
        *state = Self::new_state();
        let json = serde_json::to_string(&*state).context("Failed to serialize session")?;
        atomic_write(&self.path, json.as_bytes()).await
    }

    /// Persist the current state to disk atomically.
    pub async fn persist(&self) -> Result<()> {
        let state = self.state.read().await;
        let json = serde_json::to_string(&*state).context("Failed to serialize session")?;
        atomic_write(&self.path, json.as_bytes()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_session() -> (SessionManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::load_or_create(dir.path()).await.unwrap();
        (mgr, dir)
    }

    #[tokio::test]
    async fn fresh_session_has_no_id() {
        let (mgr, _dir) = temp_session().await;
        assert!(mgr.session_id().await.is_none());
    }

    #[tokio::test]
    async fn record_invocation_sets_session_id() {
        let (mgr, _dir) = temp_session().await;
        let id = uuid::Uuid::new_v4();
        mgr.record_invocations(Some(id), 1).await.unwrap();
        let snap = mgr.snapshot().await;
        assert_eq!(snap.session_id, Some(id));
        assert_eq!(snap.invocation_count, 1);
    }

    #[tokio::test]
    async fn reset_clears_session() {
        let (mgr, _dir) = temp_session().await;
        let id = uuid::Uuid::new_v4();
        mgr.record_invocations(Some(id), 1).await.unwrap();

        mgr.reset().await.unwrap();

        assert!(mgr.session_id().await.is_none());
        assert_eq!(mgr.snapshot().await.invocation_count, 0);
    }

    #[tokio::test]
    async fn persist_and_reload_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4();
        {
            let mgr = SessionManager::load_or_create(dir.path()).await.unwrap();
            mgr.record_invocations(Some(id), 1).await.unwrap();
        }
        // Reload from disk.
        let mgr = SessionManager::load_or_create(dir.path()).await.unwrap();
        let snap = mgr.snapshot().await;
        assert_eq!(snap.session_id, Some(id));
        assert_eq!(snap.invocation_count, 1);
    }

    #[tokio::test]
    async fn reset_persists_empty_state_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mgr = SessionManager::load_or_create(dir.path()).await.unwrap();
            mgr.record_invocations(Some(uuid::Uuid::new_v4()), 1)
                .await
                .unwrap();
            mgr.reset().await.unwrap();
        }
        let mgr = SessionManager::load_or_create(dir.path()).await.unwrap();
        let snap = mgr.snapshot().await;
        assert!(snap.session_id.is_none());
        assert_eq!(snap.invocation_count, 0);
    }

    #[tokio::test]
    async fn record_invocations_adds_count() {
        let (mgr, _dir) = temp_session().await;
        mgr.record_invocations(None, 3).await.unwrap();
        let snap = mgr.snapshot().await;
        assert_eq!(snap.invocation_count, 3);
    }
}
