use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

/// Timeout for each subprocess (ffmpeg and whisper-cpp).
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(60);

/// Async-safe cleanup: remove the WAV file without blocking the tokio runtime.
async fn cleanup_wav(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

/// Transcribe an OGG Opus audio file using whisper-cpp.
///
/// Pipeline: ogg → ffmpeg (16kHz mono WAV) → whisper-cpp → text.
/// The intermediate WAV file is cleaned up after transcription.
pub async fn transcribe(ogg_path: &Path, model_path: &Path) -> Result<String> {
    let wav_path = ogg_path.with_extension("wav");

    // Step 1: Convert OGG Opus → 16kHz mono WAV (whisper-cpp requirement).
    let ffmpeg = tokio::time::timeout(SUBPROCESS_TIMEOUT, {
        let wav = wav_path.clone();
        async move {
            tokio::process::Command::new("ffmpeg")
                .arg("-i")
                .arg(ogg_path)
                .args(["-ar", "16000", "-ac", "1", "-acodec", "pcm_s16le", "-y"])
                .arg(&wav)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
        }
    })
    .await
    .context("ffmpeg timed out")?
    .context("Failed to spawn ffmpeg")?;

    if !ffmpeg.status.success() {
        let stderr = String::from_utf8_lossy(&ffmpeg.stderr);
        // wav_path may or may not exist at this point; ignore the error.
        let _ = tokio::fs::remove_file(&wav_path).await;
        anyhow::bail!("ffmpeg failed: {}", stderr.trim());
    }

    // Run whisper and always clean up the WAV afterward, even on error/timeout.
    let result = transcribe_wav(&wav_path, model_path).await;
    cleanup_wav(&wav_path).await;
    result
}

/// Step 2: Transcribe WAV → text via whisper-cpp.
async fn transcribe_wav(wav_path: &Path, model_path: &Path) -> Result<String> {
    let whisper = tokio::time::timeout(SUBPROCESS_TIMEOUT, {
        let wav = wav_path.to_path_buf();
        async move {
            tokio::process::Command::new("whisper-cpp")
                .arg("-m")
                .arg(model_path)
                .arg("-f")
                .arg(&wav)
                .arg("--no-timestamps")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
        }
    })
    .await
    .context("whisper-cpp timed out")?
    .context("Failed to spawn whisper-cpp")?;

    if !whisper.status.success() {
        let stderr = String::from_utf8_lossy(&whisper.stderr);
        anyhow::bail!("whisper-cpp failed: {}", stderr.trim());
    }

    let text = String::from_utf8_lossy(&whisper.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if text.is_empty() {
        anyhow::bail!("whisper-cpp produced no output");
    }

    Ok(text)
}
