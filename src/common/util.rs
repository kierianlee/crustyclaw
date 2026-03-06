use anyhow::{Context, Result};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Generate a unique temp file path in the same directory as `path`.
///
/// Pattern: `<path>.tmp.<pid>.<uuid>` — same directory guarantees `rename()`
/// stays on the same filesystem, and the unique suffix avoids races.
fn tmp_path_for(path: &Path) -> PathBuf {
    PathBuf::from(format!(
        "{}.tmp.{}.{}",
        path.display(),
        std::process::id(),
        Uuid::new_v4().as_simple()
    ))
}

/// Write `contents` to `path` atomically via write-to-temp-then-rename.
///
/// Uses a unique temp file per call to avoid races when multiple tasks
/// write to the same path concurrently. Calls `sync_all()` before the
/// rename so data is durable on disk — without this, a power loss after
/// rename could leave a zero-length or partial file.
pub async fn atomic_write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    let tmp = tmp_path_for(path);
    {
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("Failed to create {}", tmp.display()))?;
        tokio::io::AsyncWriteExt::write_all(&mut f, contents.as_ref())
            .await
            .with_context(|| format!("Failed to write to {}", tmp.display()))?;
        f.sync_all()
            .await
            .with_context(|| format!("Failed to fsync {}", tmp.display()))?;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow::Error::new(e).context(format!(
            "Failed to rename {} -> {}",
            tmp.display(),
            path.display()
        )));
    }
    Ok(())
}

/// Synchronous version of [`atomic_write`] for use in `spawn_blocking` contexts.
pub fn atomic_write_sync(path: &Path, contents: &[u8]) -> Result<()> {
    atomic_write_sync_inner(path, contents, None)
}

/// Write a file atomically with mode 0600 (owner-only read/write).
///
/// On Unix the temp file is created with mode 0600 from the start so the
/// contents are never world-readable (avoids TOCTOU between write and chmod).
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    atomic_write_sync_inner(path, contents, Some(0o600))
}

/// Shared implementation for synchronous atomic writes.
///
/// When `mode` is `Some`, the temp file is created with that Unix mode.
/// On non-Unix platforms the mode is ignored.
fn atomic_write_sync_inner(path: &Path, contents: &[u8], #[allow(unused)] mode: Option<u32>) -> Result<()> {
    let tmp = tmp_path_for(path);
    {
        use std::io::Write;

        #[cfg(unix)]
        let mut f = if let Some(m) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(m)
                .open(&tmp)
                .with_context(|| format!("Failed to create {}", tmp.display()))?
        } else {
            std::fs::File::create(&tmp)
                .with_context(|| format!("Failed to create {}", tmp.display()))?
        };

        #[cfg(not(unix))]
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("Failed to create {}", tmp.display()))?;

        f.write_all(contents)
            .with_context(|| format!("Failed to write to {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("Failed to fsync {}", tmp.display()))?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context(format!(
            "Failed to rename {} -> {}",
            tmp.display(),
            path.display()
        )));
    }
    Ok(())
}

/// Format a UUID as its first 8 hex characters for display.
pub fn short_id(id: Uuid) -> String {
    let mut s = String::with_capacity(8);
    for byte in &id.as_bytes()[..4] {
        write!(s, "{byte:02x}").expect("writing to String cannot fail");
    }
    s
}

/// Truncate a string to at most `max_bytes`, respecting UTF-8 char boundaries.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Remove stale temp files left behind by `atomic_write` crashes.
///
/// Matches the specific pattern produced by `atomic_write`:
/// `<basename>.tmp.<pid>.<uuid>` (e.g. `session.json.tmp.12345.a1b2c3...`).
pub fn cleanup_stale_tmp_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_atomic_write_tmp(&name) {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                tracing::debug!(path = %entry.path().display(), error = %e, "Failed to clean stale tmp file");
            } else {
                tracing::debug!(path = %entry.path().display(), "Cleaned stale tmp file");
            }
        }
    }
}

/// Remove stale files from `inbox/` left behind by daemon crashes.
///
/// Downloaded photos and voice messages are transient — they only need to
/// exist while Claude processes them. If the daemon crashes mid-processing,
/// these files accumulate. This function removes inbox files older than
/// `max_age` on startup.
pub fn cleanup_stale_inbox_files(data_dir: &Path, max_age: std::time::Duration) {
    let inbox = data_dir.join("inbox");
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_stale = path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if is_stale {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::debug!(path = %path.display(), error = %e, "Failed to clean stale inbox file");
            } else {
                tracing::info!(path = %path.display(), "Cleaned stale inbox file");
            }
        }
    }
}

/// Check whether a filename looks like an `atomic_write` temp file.
fn is_atomic_write_tmp(name: &str) -> bool {
    name.contains(".tmp.")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_atomic_write_tmp --

    #[test]
    fn tmp_pattern_valid() {
        assert!(is_atomic_write_tmp(
            "session.json.tmp.12345.a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"
        ));
    }

    #[test]
    fn tmp_pattern_no_marker() {
        assert!(!is_atomic_write_tmp("session.json"));
    }

    // -- truncate_str --

    #[test]
    fn truncate_no_op_short_string() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_boundary() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_ascii() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_multibyte_boundary() {
        // '€' is 3 bytes; truncating at byte 2 should back up to byte 0
        assert_eq!(truncate_str("€abc", 2), "");
        assert_eq!(truncate_str("€abc", 3), "€");
        assert_eq!(truncate_str("€abc", 4), "€a");
    }

    // -- short_id --

    #[test]
    fn short_id_length() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(short_id(id), "550e8400");
    }

    // -- atomic_write --

    #[tokio::test]
    async fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        atomic_write(&path, b"hello").await.unwrap();
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "hello");
    }

    #[tokio::test]
    async fn atomic_write_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        atomic_write(&path, b"first").await.unwrap();
        atomic_write(&path, b"second").await.unwrap();
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "second");
    }

    #[tokio::test]
    async fn atomic_write_no_leftover_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        atomic_write(&path, b"data").await.unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name().to_string_lossy(), "test.txt");
    }
}
