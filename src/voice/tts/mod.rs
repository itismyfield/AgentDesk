//! Text-to-speech backend abstraction and progress utterance cache.

pub(crate) mod edge;

use crate::voice::config::{VoiceConfig, VoiceTtsBackendKind};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::{fs, sync::Mutex};
use tracing::debug;

pub(crate) use edge::EdgeTtsBackend;

type ProgressCacheLockMap = Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>;

fn progress_cache_locks() -> &'static ProgressCacheLockMap {
    static LOCKS: OnceLock<ProgressCacheLockMap> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn progress_cache_lock(cache_path: &Path) -> Arc<Mutex<()>> {
    let mut locks = progress_cache_locks().lock().await;
    locks
        .entry(cache_path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

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

    let cache_lock = progress_cache_lock(&cache_path).await;
    let _cache_guard = cache_lock.lock().await;
    if is_non_empty_file(&cache_path).await? {
        debug!(
            path = %cache_path.display(),
            "voice progress TTS cache hit after single-flight wait; synthesis skipped"
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
        copy_to_cache_atomically(&synthesized, &cache_path).await?;
        fs::remove_file(&synthesized).await.with_context(|| {
            format!(
                "remove synthesized TTS temp output {}",
                synthesized.display()
            )
        })?;
    }
    ensure_non_empty_file(&cache_path).await?;

    Ok(TtsSynthesisOutput {
        path: cache_path,
        cache_status: ProgressTtsCacheStatus::Miss,
    })
}

async fn copy_to_cache_atomically(synthesized: &Path, cache_path: &Path) -> Result<()> {
    let temp_path = cache_write_temp_path(cache_path);
    let result = async {
        fs::copy(synthesized, &temp_path).await.with_context(|| {
            format!(
                "copy synthesized TTS output {} to cache temp {}",
                synthesized.display(),
                temp_path.display()
            )
        })?;
        ensure_non_empty_file(&temp_path).await?;
        fs::rename(&temp_path, cache_path).await.with_context(|| {
            format!(
                "rename TTS cache temp {} to {}",
                temp_path.display(),
                cache_path.display()
            )
        })?;
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    result
}

fn cache_write_temp_path(cache_path: &Path) -> PathBuf {
    let file_name = cache_path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "tts-cache".into());
    cache_path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()))
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[derive(Clone)]
    struct CountingBackend {
        calls: Arc<AtomicUsize>,
        output_dir: PathBuf,
        delay: Duration,
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
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
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
            delay: Duration::ZERO,
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
        assert!(
            !temp.path().join("tmp").join("mock-1.mp3").exists(),
            "synthesized temp output should be removed after cache write"
        );
    }

    #[tokio::test]
    async fn concurrent_progress_cache_call_singleflights_backend() {
        let temp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = CountingBackend {
            calls: calls.clone(),
            output_dir: temp.path().join("tmp"),
            delay: Duration::from_millis(50),
        };
        fs::create_dir_all(&backend.output_dir).await.unwrap();
        let cache_dir = temp.path().join("progress-cache");

        let backend_a = backend.clone();
        let cache_dir_a = cache_dir.clone();
        let first = tokio::spawn(async move {
            synthesize_with_progress_cache(
                &backend_a,
                "동시 진행 안내",
                TtsSynthesisKind::Progress,
                &cache_dir_a,
            )
            .await
        });
        let backend_b = backend.clone();
        let cache_dir_b = cache_dir.clone();
        let second = tokio::spawn(async move {
            synthesize_with_progress_cache(
                &backend_b,
                "동시 진행 안내",
                TtsSynthesisKind::Progress,
                &cache_dir_b,
            )
            .await
        });

        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap().unwrap();
        let second = second.unwrap().unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.path, second.path);
        assert_ne!(first.cache_status, second.cache_status);
        assert!([first.cache_status, second.cache_status].contains(&ProgressTtsCacheStatus::Miss));
        assert!([first.cache_status, second.cache_status].contains(&ProgressTtsCacheStatus::Hit));
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
