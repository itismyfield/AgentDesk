//! Text-to-speech backend abstraction and progress utterance cache.

pub(crate) mod edge;

use crate::voice::config::{VoiceConfig, VoiceTtsBackendKind};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::debug;

pub(crate) use edge::EdgeTtsBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TtsSynthesisKind {
    Final,
    Progress,
}

impl TtsSynthesisKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Final => "final",
            Self::Progress => "progress",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressTtsCacheStatus {
    Hit,
    Miss,
    Bypassed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TtsSynthesisOutput {
    pub(crate) path: PathBuf,
    pub(crate) cache_status: ProgressTtsCacheStatus,
}

/// Backend contract for all voice synthesis engines.
///
/// `cache_key_parts` must include every audio-affecting backend setting such as
/// backend name, voice identity, reference voice, style, model, and speaking
/// rate. That keeps the progress cache valid when future OpenVoice or
/// Supertonic implementations are added without changing the cache layer.
#[allow(async_fn_in_trait)]
pub(crate) trait TtsBackend: Send + Sync {
    fn cache_key_parts(&self) -> Vec<String>;
    fn output_extension(&self) -> &'static str;
    async fn synthesize(&self, text: &str, kind: TtsSynthesisKind) -> Result<PathBuf>;
}

#[derive(Clone)]
pub(crate) enum ConfiguredTtsBackend {
    Edge(EdgeTtsBackend),
}

impl ConfiguredTtsBackend {
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Result<Self> {
        match config.tts.backend {
            VoiceTtsBackendKind::Edge => Ok(Self::Edge(EdgeTtsBackend::from_voice_config(config))),
        }
    }
}

impl TtsBackend for ConfiguredTtsBackend {
    fn cache_key_parts(&self) -> Vec<String> {
        match self {
            Self::Edge(backend) => backend.cache_key_parts(),
        }
    }

    fn output_extension(&self) -> &'static str {
        match self {
            Self::Edge(backend) => backend.output_extension(),
        }
    }

    async fn synthesize(&self, text: &str, kind: TtsSynthesisKind) -> Result<PathBuf> {
        match self {
            Self::Edge(backend) => backend.synthesize(text, kind).await,
        }
    }
}

#[derive(Clone)]
pub(crate) struct TtsRuntime {
    backend: ConfiguredTtsBackend,
    progress_cache_dir: PathBuf,
}

impl TtsRuntime {
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Result<Self> {
        Ok(Self {
            backend: ConfiguredTtsBackend::from_voice_config(config)?,
            progress_cache_dir: config.tts.progress_cache_dir.clone(),
        })
    }

    /// Re-read voice config after a voice-change command mutates backend
    /// settings, rebinding the backend and progress cache target together.
    pub(crate) fn rebind_from_voice_config(&mut self, config: &VoiceConfig) -> Result<()> {
        *self = Self::from_voice_config(config)?;
        Ok(())
    }

    pub(crate) fn cache_key_parts(&self) -> Vec<String> {
        self.backend.cache_key_parts()
    }

    pub(crate) async fn synthesize(
        &self,
        text: &str,
        kind: TtsSynthesisKind,
    ) -> Result<TtsSynthesisOutput> {
        synthesize_with_progress_cache(&self.backend, text, kind, &self.progress_cache_dir).await
    }
}

pub(crate) async fn synthesize_with_progress_cache<B>(
    backend: &B,
    text: &str,
    kind: TtsSynthesisKind,
    progress_cache_dir: &Path,
) -> Result<TtsSynthesisOutput>
where
    B: TtsBackend + ?Sized,
{
    if kind != TtsSynthesisKind::Progress {
        let path = backend.synthesize(text, kind).await?;
        ensure_non_empty_file(&path).await?;
        return Ok(TtsSynthesisOutput {
            path,
            cache_status: ProgressTtsCacheStatus::Bypassed,
        });
    }

    let cache_path = progress_tts_cache_path(
        progress_cache_dir,
        &backend.cache_key_parts(),
        text,
        backend.output_extension(),
    );
    if is_non_empty_file(&cache_path).await? {
        debug!(
            path = %cache_path.display(),
            "voice progress TTS cache hit; synthesis skipped"
        );
        return Ok(TtsSynthesisOutput {
            path: cache_path,
            cache_status: ProgressTtsCacheStatus::Hit,
        });
    }

    debug!(
        path = %cache_path.display(),
        "voice progress TTS cache miss; running backend"
    );
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create TTS progress cache dir {}", parent.display()))?;
    }

    let synthesized = backend.synthesize(text, kind).await?;
    ensure_non_empty_file(&synthesized).await?;
    if synthesized != cache_path {
        fs::copy(&synthesized, &cache_path).await.with_context(|| {
            format!(
                "copy synthesized TTS output {} to cache {}",
                synthesized.display(),
                cache_path.display()
            )
        })?;
        let _ = fs::remove_file(&synthesized).await;
    }
    ensure_non_empty_file(&cache_path).await?;

    Ok(TtsSynthesisOutput {
        path: cache_path,
        cache_status: ProgressTtsCacheStatus::Miss,
    })
}

pub(crate) fn progress_tts_cache_path(
    progress_cache_dir: &Path,
    backend_key_parts: &[String],
    text: &str,
    extension: &str,
) -> PathBuf {
    progress_cache_dir.join(progress_tts_cache_file_name(
        backend_key_parts,
        text,
        extension,
    ))
}

pub(crate) fn progress_tts_cache_file_name(
    backend_key_parts: &[String],
    text: &str,
    extension: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in backend_key_parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(text.as_bytes());

    let extension = normalize_extension(extension);
    format!("{}.{}", hasher.finalize().to_hex(), extension)
}

fn normalize_extension(extension: &str) -> String {
    let trimmed = extension.trim().trim_start_matches('.');
    if trimmed.is_empty() {
        "mp3".to_string()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

async fn ensure_non_empty_file(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("stat synthesized TTS output {}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        anyhow::bail!("TTS backend produced empty output: {}", path.display());
    }
    Ok(())
}

async fn is_non_empty_file(path: &Path) -> Result<bool> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("stat TTS progress cache file {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingBackend {
        calls: Arc<AtomicUsize>,
        output_dir: PathBuf,
    }

    impl TtsBackend for CountingBackend {
        fn cache_key_parts(&self) -> Vec<String> {
            vec!["mock".to_string(), "voice-a".to_string()]
        }

        fn output_extension(&self) -> &'static str {
            "mp3"
        }

        async fn synthesize(&self, text: &str, _kind: TtsSynthesisKind) -> Result<PathBuf> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let path = self.output_dir.join(format!("mock-{call}.mp3"));
            fs::write(&path, format!("audio:{text}:{call}")).await?;
            Ok(path)
        }
    }

    #[test]
    fn progress_cache_filename_uses_blake3_hex_and_extension() {
        let name = progress_tts_cache_file_name(
            &["edge".to_string(), "ko-KR-SunHiNeural".to_string()],
            "작업 중입니다",
            ".MP3",
        );

        assert!(name.ends_with(".mp3"));
        assert_eq!(name.len(), 64 + ".mp3".len());
    }

    #[tokio::test]
    async fn progress_cache_hit_skips_second_backend_call() {
        let temp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = CountingBackend {
            calls: calls.clone(),
            output_dir: temp.path().join("tmp"),
        };
        fs::create_dir_all(&backend.output_dir).await.unwrap();
        let cache_dir = temp.path().join("progress-cache");

        let first = synthesize_with_progress_cache(
            &backend,
            "잠시만 기다려 주세요",
            TtsSynthesisKind::Progress,
            &cache_dir,
        )
        .await
        .unwrap();
        let second = synthesize_with_progress_cache(
            &backend,
            "잠시만 기다려 주세요",
            TtsSynthesisKind::Progress,
            &cache_dir,
        )
        .await
        .unwrap();

        assert_eq!(first.cache_status, ProgressTtsCacheStatus::Miss);
        assert_eq!(second.cache_status, ProgressTtsCacheStatus::Hit);
        assert_eq!(first.path, second.path);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn runtime_rebinds_backend_voice_from_config() {
        let mut config = VoiceConfig::default();
        config.tts.edge.voice = "ko-KR-SunHiNeural".to_string();
        let mut runtime = TtsRuntime::from_voice_config(&config).unwrap();
        assert!(
            runtime
                .cache_key_parts()
                .contains(&"ko-KR-SunHiNeural".to_string())
        );

        config.tts.edge.voice = "ko-KR-InJoonNeural".to_string();
        runtime.rebind_from_voice_config(&config).unwrap();

        assert!(
            runtime
                .cache_key_parts()
                .contains(&"ko-KR-InJoonNeural".to_string())
        );
    }
}
