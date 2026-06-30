//! Text-to-speech backend abstraction and progress utterance cache.

pub(crate) mod chunks;
pub(crate) mod edge;
pub(crate) mod playback;

use crate::voice::config::{VoiceConfig, VoiceTtsBackendKind};
use crate::voice::utils::expand_tilde;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};
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
    // F15 (#2046): 너무 커지면 unused 엔트리(strong_count == 1, 즉 map 만 보유)를
    // 즉시 정리. 매번 hit 시 ~O(n) 비용이지만 entry 수는 캐시 텍스트 다양성에
    // 비례하므로 실무적으로 수백 단위에서 안정. 임계 초과 시에만 청소한다.
    if locks.len() >= PROGRESS_CACHE_LOCK_MAX_ENTRIES {
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    locks
        .entry(cache_path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

const PROGRESS_CACHE_LOCK_MAX_ENTRIES: usize = 1024;

/// Shared file-name prefix for edge-tts temp output. Defined here (instead of
/// inline in `edge.rs`) so the leak-E orphan sweep and the synthesizer agree
/// on exactly which files belong to AgentDesk. See [`sweep_edge_tts_temp_orphans`].
pub(crate) const EDGE_TTS_TEMP_PREFIX: &str = "agentdesk-edge-tts-";

/// #3909 (leak A) — default age cap for progress TTS cache files. Files whose
/// mtime is older than this are evicted on every sweep. The cache key is
/// `blake3(backend + text)`, but per-turn-unique LLM/template progress summaries
/// flow through this path, so without a cap a new file accrues every turn. 7
/// days keeps genuinely hot fixed-phrase entries warm across restarts while
/// bounding the long tail of one-shot summaries.
pub(crate) const DEFAULT_PROGRESS_CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// #3909 (leak A) — default capacity cap (bytes) for the progress TTS cache
/// dir. After the age pass, if the dir still exceeds this the oldest-mtime
/// files are evicted until it fits. 256 MiB is generous for short mp3
/// utterances yet bounds a runaway dir between sweeps.
pub(crate) const DEFAULT_PROGRESS_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// #3909 (leak A) — default grace window protecting very recent files from the
/// CAPACITY pass. The TTL pass only ever removes files older than `max_age`, but
/// the capacity (LRU) pass would otherwise evict the oldest survivor regardless
/// of how fresh — including a file still being written (atomic `.tmp` rename can
/// take up to the 60s synth timeout) or read for playback. 5 minutes
/// comfortably covers synth + playback so the cap never evicts an in-flight
/// file; the cap is soft (a later sweep reclaims the space once it ages out).
pub(crate) const DEFAULT_PROGRESS_CACHE_EVICT_GRACE: Duration = Duration::from_secs(5 * 60);

/// #3909 (leak E, belt-and-suspenders) — default age cap for orphaned edge-tts
/// temp mp3 files. Synthesis times out at 60s, so any `agentdesk-edge-tts-*`
/// file older than 30 min is a definite orphan (e.g. produced before the
/// drop-guard fix shipped, or by a crash). Conservative enough never to race a
/// live synthesis.
pub(crate) const DEFAULT_EDGE_TEMP_ORPHAN_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// Outcome of a [`sweep_progress_tts_cache`] pass, surfaced for logging/tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProgressCacheSweepOutcome {
    pub(crate) removed_aged: usize,
    pub(crate) removed_over_cap: usize,
    pub(crate) removed_bytes: u64,
    pub(crate) retained_files: usize,
    pub(crate) retained_bytes: u64,
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
            // F17 (#2046): `~/...` 같은 home-relative 경로가 와도 STT/Receiver 와
            // 동일하게 절대경로로 풀어 dcserver CWD 차이로 위치가 갈리는 문제를 막는다.
            progress_cache_dir: expand_tilde(&config.tts.progress_cache_dir),
        })
    }

    /// Re-read voice config after a voice-change command mutates backend
    /// settings, rebinding the backend and progress cache target together.
    // reason: voice runtime is wired only when voice config is enabled; no
    // compile target exercises it. See #3034.
    #[allow(dead_code)]
    pub(crate) fn rebind_from_voice_config(&mut self, config: &VoiceConfig) -> Result<()> {
        *self = Self::from_voice_config(config)?;
        Ok(())
    }

    // reason: voice runtime is wired only when voice config is enabled; no
    // compile target exercises it. See #3034.
    #[allow(dead_code)]
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

/// A finalized progress-cache file name is exactly `<64-hex blake3>.<ext>`
/// (see [`progress_tts_cache_file_name`]). Restricting the sweep to this pattern
/// keeps it from ever touching unrelated operator files OR the in-progress
/// atomic write temp (`.{name}.{uuid}.tmp`, which carries a leading dot and a
/// non-64-char stem). #3909.
fn is_progress_cache_file_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    !ext.is_empty()
        && stem.len() == 64
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// #3909 (leak A) — bound the progress TTS cache dir: evict cache files older
/// than `max_age` (TTL) then, if the dir still exceeds `max_bytes`, evict the
/// oldest-mtime survivors (capacity/LRU cap) — but never a file younger than
/// `evict_grace`, which may still be mid atomic-write or being read for
/// playback. Only finalized `<64-hex>.<ext>` cache files are considered, so the
/// in-progress `.tmp` write and any unrelated file are left untouched.
/// Synchronous (`std::fs`) so it can run inside a `spawn_blocking` from the
/// leader-only maintenance sweep. Missing dir is a no-op; a symlinked root or
/// symlinked entries are skipped so the sweep never follows a link into an
/// external tree.
pub(crate) fn sweep_progress_tts_cache(
    cache_dir: &Path,
    max_age: Duration,
    max_bytes: u64,
    evict_grace: Duration,
) -> ProgressCacheSweepOutcome {
    sweep_progress_tts_cache_at(
        cache_dir,
        max_age,
        max_bytes,
        evict_grace,
        SystemTime::now(),
    )
}

fn sweep_progress_tts_cache_at(
    cache_dir: &Path,
    max_age: Duration,
    max_bytes: u64,
    evict_grace: Duration,
    now: SystemTime,
) -> ProgressCacheSweepOutcome {
    let mut outcome = ProgressCacheSweepOutcome::default();

    match std::fs::symlink_metadata(cache_dir) {
        Ok(meta) if meta.file_type().is_symlink() => return outcome,
        Ok(_) => {}
        Err(_) => return outcome,
    }
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return outcome;
    };

    // (path, mtime, len) for the finalized cache files that survive the age
    // pass. Non-cache files (operator files, the `.tmp` in-progress write) are
    // skipped entirely — neither aged nor capacity-evicted.
    let mut survivors: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let matches_pattern = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_progress_cache_file_name);
        if !matches_pattern {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(now);
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age > max_age {
            if sweep_remove_file(&path) {
                outcome.removed_aged += 1;
                outcome.removed_bytes += len;
            }
            continue;
        }
        survivors.push((path, mtime, len));
    }

    let mut total: u64 = survivors.iter().map(|(_, _, len)| *len).sum();
    outcome.retained_files = survivors.len();
    outcome.retained_bytes = total;
    if total > max_bytes {
        // Oldest mtime first → evict the coldest entries until under the cap,
        // but never a grace-young file (still being written/played). If only
        // grace-young files remain, the cap is left softly exceeded until they
        // age out on a later sweep.
        survivors.sort_by(|a, b| a.1.cmp(&b.1));
        for (path, mtime, len) in &survivors {
            if total <= max_bytes {
                break;
            }
            let age = now.duration_since(*mtime).unwrap_or(Duration::ZERO);
            if age < evict_grace {
                continue;
            }
            if sweep_remove_file(path) {
                outcome.removed_over_cap += 1;
                outcome.removed_bytes += *len;
                outcome.retained_files -= 1;
                outcome.retained_bytes = outcome.retained_bytes.saturating_sub(*len);
                total = total.saturating_sub(*len);
            }
        }
    }

    outcome
}

/// #3909 (leak E, belt-and-suspenders) — remove orphaned `agentdesk-edge-tts-*`
/// temp files older than `max_age` from `temp_dir`. The drop guard in
/// [`edge`](super::edge) removes the temp file on barge-in abort at the source;
/// this sweep mops up any orphan that predates the guard or escaped it. Only
/// AgentDesk-prefixed regular files are touched; symlinks and the root link are
/// skipped. Returns the number of files removed.
pub(crate) fn sweep_edge_tts_temp_orphans(temp_dir: &Path, max_age: Duration) -> usize {
    sweep_edge_tts_temp_orphans_at(temp_dir, max_age, SystemTime::now())
}

fn sweep_edge_tts_temp_orphans_at(temp_dir: &Path, max_age: Duration, now: SystemTime) -> usize {
    match std::fs::symlink_metadata(temp_dir) {
        Ok(meta) if meta.file_type().is_symlink() => return 0,
        Ok(_) => {}
        Err(_) => return 0,
    }
    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return 0;
    };

    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(EDGE_TTS_TEMP_PREFIX) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(now);
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age > max_age && sweep_remove_file(&path) {
            removed += 1;
        }
    }
    removed
}

/// Best-effort unlink shared by the voice sweeps. `true` only when a file was
/// actually removed; a missing file or a removal error counts as "not removed"
/// so callers never over-count. Errors are logged at debug so a sweep never
/// fails the maintenance job.
fn sweep_remove_file(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::debug!(
                path = %path.display(),
                %error,
                "voice TTS cache sweep could not remove file"
            );
            false
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

    fn set_mtime(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    /// A valid finalized cache file name: `<64-hex>.mp3`. `seed` must be a hex
    /// digit so the whole 64-char stem is hex.
    fn cache_name(seed: char) -> String {
        format!("{}.mp3", std::iter::repeat_n(seed, 64).collect::<String>())
    }

    #[test]
    fn progress_cache_file_name_pattern_matches_only_finalized_entries() {
        // Real finalized names: `<64-hex>.<ext>`.
        assert!(is_progress_cache_file_name(&cache_name('a')));
        assert!(is_progress_cache_file_name(&format!(
            "{}.wav",
            "0".repeat(64)
        )));
        // Rejected: atomic `.tmp` write (leading dot, non-64 stem), no ext,
        // wrong stem length, and non-hex stems.
        assert!(!is_progress_cache_file_name(&format!(
            ".{}.{}.tmp",
            cache_name('b'),
            "uuid"
        )));
        assert!(!is_progress_cache_file_name(&"0".repeat(64)));
        assert!(!is_progress_cache_file_name(&format!(
            "{}.mp3",
            "0".repeat(63)
        )));
        assert!(!is_progress_cache_file_name("operator-notes.txt"));
        assert!(!is_progress_cache_file_name(&format!(
            "{}.mp3",
            "g".repeat(64)
        )));
    }

    #[test]
    fn sweep_missing_cache_dir_is_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist");
        let outcome = sweep_progress_tts_cache(
            &missing,
            Duration::from_secs(60),
            u64::MAX,
            DEFAULT_PROGRESS_CACHE_EVICT_GRACE,
        );
        assert_eq!(outcome, ProgressCacheSweepOutcome::default());
    }

    #[test]
    fn sweep_progress_cache_removes_aged_files_and_keeps_fresh() {
        // A (#3909): TTL pass evicts files older than `max_age`; fresh files
        // (per-turn cache writes within the window) survive.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let aged = dir.join(cache_name('a'));
        let fresh = dir.join(cache_name('b'));
        std::fs::write(&aged, b"old audio bytes").unwrap();
        std::fs::write(&fresh, b"new audio bytes").unwrap();
        // Backdate `aged` well beyond the 7-day TTL.
        set_mtime(
            &aged,
            SystemTime::now() - Duration::from_secs(10 * 24 * 60 * 60),
        );

        let outcome = sweep_progress_tts_cache(
            dir,
            Duration::from_secs(7 * 24 * 60 * 60),
            u64::MAX, // capacity unlimited: isolate the age pass
            DEFAULT_PROGRESS_CACHE_EVICT_GRACE,
        );

        assert!(!aged.exists(), "aged cache file must be evicted by the TTL");
        assert!(fresh.exists(), "fresh cache file must survive the TTL");
        assert_eq!(outcome.removed_aged, 1);
        assert_eq!(outcome.removed_over_cap, 0);
        assert_eq!(outcome.retained_files, 1);
    }

    #[test]
    fn sweep_progress_cache_evicts_oldest_over_capacity() {
        // A (#3909): with no file aged out, the capacity cap evicts the
        // oldest-mtime entries (LRU) until the dir fits under `max_bytes`.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let oldest = dir.join(cache_name('0'));
        let middle = dir.join(cache_name('1'));
        let newest = dir.join(cache_name('2'));
        for path in [&oldest, &middle, &newest] {
            std::fs::write(path, vec![0u8; 100]).unwrap();
        }
        // All well past the eviction grace so capacity can act on them.
        let base = SystemTime::now() - Duration::from_secs(3 * 60 * 60);
        set_mtime(&oldest, base);
        set_mtime(&middle, base + Duration::from_secs(60));
        set_mtime(&newest, base + Duration::from_secs(120));

        // 300 bytes total, cap 250 → exactly one (the oldest) must be evicted.
        let outcome = sweep_progress_tts_cache(
            dir,
            Duration::from_secs(365 * 24 * 60 * 60),
            250,
            DEFAULT_PROGRESS_CACHE_EVICT_GRACE,
        );

        assert!(!oldest.exists(), "oldest entry must be evicted first");
        assert!(middle.exists(), "newer entries must survive the cap");
        assert!(newest.exists(), "newest entry must survive the cap");
        assert_eq!(outcome.removed_aged, 0);
        assert_eq!(outcome.removed_over_cap, 1);
        assert_eq!(outcome.removed_bytes, 100);
        assert_eq!(outcome.retained_files, 2);
        assert_eq!(outcome.retained_bytes, 200);
    }

    #[test]
    fn sweep_progress_cache_skips_tmp_and_unrelated_files() {
        // Issue [2a] (#3909 codex review): the sweep must only target finalized
        // `<64-hex>.<ext>` cache files — never the atomic `.tmp` in-progress
        // write or unrelated operator files, even when they are aged.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let aged_cache = dir.join(cache_name('c'));
        // Realistic atomic-write temp: `.{name}.{uuid}.tmp`.
        let in_progress_tmp = dir.join(format!(".{}.{}.tmp", cache_name('d'), "deadbeef"));
        let operator_file = dir.join("operator-notes.txt");
        let extensionless = dir.join("README");
        for path in [
            &aged_cache,
            &in_progress_tmp,
            &operator_file,
            &extensionless,
        ] {
            std::fs::write(path, b"bytes").unwrap();
        }
        // Age ALL of them beyond the TTL — only the cache file may be removed.
        let old = SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60);
        for path in [
            &aged_cache,
            &in_progress_tmp,
            &operator_file,
            &extensionless,
        ] {
            set_mtime(path, old);
        }

        let outcome = sweep_progress_tts_cache(
            dir,
            Duration::from_secs(7 * 24 * 60 * 60),
            u64::MAX,
            DEFAULT_PROGRESS_CACHE_EVICT_GRACE,
        );

        assert!(!aged_cache.exists(), "aged finalized cache file is removed");
        assert!(
            in_progress_tmp.exists(),
            "in-progress .tmp write must never be swept"
        );
        assert!(
            operator_file.exists(),
            "unrelated operator file must never be swept"
        );
        assert!(extensionless.exists(), "extensionless file must be ignored");
        assert_eq!(outcome.removed_aged, 1);
    }

    #[test]
    fn sweep_progress_cache_capacity_grace_preserves_recent_files() {
        // Issue [2b] (#3909 codex review): the capacity (LRU) pass must not
        // evict a very recent file (possibly mid-write / being played) even
        // under cap pressure; it evicts the oldest grace-passed file instead and
        // leaves the cap softly exceeded.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let old = dir.join(cache_name('e'));
        let recent = dir.join(cache_name('f'));
        std::fs::write(&old, vec![0u8; 100]).unwrap();
        std::fs::write(&recent, vec![0u8; 100]).unwrap();
        // `old` is past the grace window; `recent` was just written (now).
        set_mtime(&old, SystemTime::now() - Duration::from_secs(60 * 60));

        // cap=50 forces eviction; total=200. Only `old` is grace-passed.
        let outcome = sweep_progress_tts_cache(
            dir,
            Duration::from_secs(365 * 24 * 60 * 60),
            50,
            Duration::from_secs(5 * 60),
        );

        assert!(!old.exists(), "the grace-passed oldest file is evicted");
        assert!(
            recent.exists(),
            "a within-grace file must survive capacity pressure (may be mid-write/playing)"
        );
        assert_eq!(outcome.removed_over_cap, 1);
        assert_eq!(outcome.retained_files, 1);
    }

    #[test]
    fn sweep_edge_temp_orphans_removes_only_aged_prefixed_files() {
        // E (#3909) belt-and-suspenders: aged AgentDesk temp mp3s are swept;
        // fresh ones and unrelated operator files are left untouched.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let aged_orphan = dir.join("agentdesk-edge-tts-123-abc.mp3");
        let fresh_orphan = dir.join("agentdesk-edge-tts-123-def.mp3");
        let unrelated = dir.join("operator-keepsake.mp3");
        for path in [&aged_orphan, &fresh_orphan, &unrelated] {
            std::fs::write(path, b"bytes").unwrap();
        }
        set_mtime(
            &aged_orphan,
            SystemTime::now() - Duration::from_secs(60 * 60),
        );

        let removed = sweep_edge_tts_temp_orphans(dir, DEFAULT_EDGE_TEMP_ORPHAN_MAX_AGE);

        assert_eq!(removed, 1);
        assert!(
            !aged_orphan.exists(),
            "aged edge-tts orphan must be removed"
        );
        assert!(fresh_orphan.exists(), "fresh temp output must be preserved");
        assert!(
            unrelated.exists(),
            "non-AgentDesk files must never be touched by the sweep"
        );
    }
}
