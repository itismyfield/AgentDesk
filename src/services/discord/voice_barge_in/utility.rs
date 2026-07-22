use super::*;

pub(super) fn pcm_i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

pub(super) fn discord_pcm_i16_stereo_48k_to_mono_f32_16k(samples: &[i16]) -> Vec<f32> {
    samples
        .chunks_exact(6)
        .map(|chunk| {
            let mut sum = 0.0f32;
            for frame in 0..3 {
                let left = chunk[frame * 2] as f32;
                let right = chunk[frame * 2 + 1] as f32;
                sum += (left + right) * 0.5;
            }
            (sum / 3.0) / i16::MAX as f32
        })
        .collect()
}

/// #2156: 일반 wav/segment 삭제. NotFound 는 무시, 그 외 에러는 debug 로그.
pub(super) async fn remove_file_quietly(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::debug!(
            error = %error,
            path = %path.display(),
            "voice utterance cleanup could not remove file (#2156)"
        ),
    }
}

/// #2156: transcript sidecar 정리. 후보 다수 중 대부분은 존재하지 않으므로
/// 모든 에러를 trace 로 낮춰 로그 노이즈를 줄인다.
pub(super) async fn remove_file_quietly_silent(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::trace!(
            error = %error,
            path = %path.display(),
            "voice transcript sidecar cleanup skipped (#2156)"
        );
    }
}

pub(super) fn transcript_dirs_from_config(config: &VoiceConfig) -> Vec<PathBuf> {
    vec![expand_tilde(&config.audio.transcripts_dir)]
}

fn expand_tilde(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

pub(super) fn lock_monitor(
    monitor: &std::sync::Mutex<LiveBargeInMonitor>,
) -> std::sync::MutexGuard<'_, LiveBargeInMonitor> {
    monitor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(in crate::services::discord) const INTERNAL_VOICE_MESSAGE_ID_START: u64 =
    9_000_000_000_000_000_000;

/// `true` iff `msg_id` is a synthetic voice-originated id (≥
/// `INTERNAL_VOICE_MESSAGE_ID_START`). Real Discord snowflakes encode
/// timestamps and worker/process/sequence fields and stay well below 2^63
/// for the foreseeable future, so the 9e18 prefix is safely above them.
/// Used by the message intake to skip ⏳/📬 reactions, placeholder POSTs,
/// and `message_reference` lookups that would fail with "Unknown message"
/// for a non-existent Discord message id.
pub(in crate::services::discord) fn is_synthetic_voice_message_id(
    msg_id: poise::serenity_prelude::MessageId,
) -> bool {
    msg_id.get() >= INTERNAL_VOICE_MESSAGE_ID_START
}
