//! #3909 — voice TTS cache / temp sweep.
//!
//! Two unbounded-growth leaks share one root (no sweeper for the voice
//! temp/cache dirs):
//!
//!   * **Leak A** — the progress TTS cache key is `blake3(backend + text)`, but
//!     per-turn-unique LLM/template progress summaries flow through it, so
//!     without a cap a new mp3 accrues every turn → partition exhaustion on a
//!     long-lived dcserver. [`sweep_progress_tts_cache`] bounds the dir by TTL
//!     (mtime age) and a capacity cap (oldest-mtime LRU eviction).
//!   * **Leak E** — edge-tts temp mp3s orphan when a barge-in aborts the synth
//!     future mid-`.await`. The `tts::edge::EdgeTtsTempGuard` drop guard removes
//!     them at the source; [`sweep_edge_tts_temp_orphans`] is the
//!     belt-and-suspenders that mops up any that predate the guard or escape it.
//!
//! Wrapped by the pool-less `server::maintenance::ProgressTtsCacheSweepJob` and
//! run through the leader-only `worker_registry::MaintenanceScheduler` — like
//! `voice.turn_link_gc`, voice-runtime housekeeping belongs on the leader so N
//! cluster nodes do not each spin a redundant sweeper.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::voice::VoiceConfig;
use crate::voice::tts::{
    DEFAULT_EDGE_TEMP_ORPHAN_MAX_AGE, DEFAULT_PROGRESS_CACHE_EVICT_GRACE,
    DEFAULT_PROGRESS_CACHE_MAX_AGE, DEFAULT_PROGRESS_CACHE_MAX_BYTES, sweep_edge_tts_temp_orphans,
    sweep_progress_tts_cache,
};

/// Resolved sweep targets + caps. Built via [`Config::from_voice_config`] in
/// production; tests inject temp dirs directly.
#[derive(Debug, Clone)]
pub struct Config {
    pub progress_cache_dir: PathBuf,
    pub edge_temp_dir: PathBuf,
    pub progress_max_age: Duration,
    pub progress_max_bytes: u64,
    pub progress_evict_grace: Duration,
    pub edge_temp_orphan_max_age: Duration,
}

impl Config {
    /// Resolve the sweep targets from the **loaded runtime** `VoiceConfig`, the
    /// exact same source of truth the TTS write path uses
    /// (`TtsRuntime::from_voice_config` for `voice.tts.progress_cache_dir`,
    /// `EdgeTtsConfig::from_voice_config` for `voice.audio.temp_dir`, both
    /// tilde-expanded). This keeps the sweep dir == the write dir even when an
    /// operator overrides those paths — otherwise the sweep would clean the
    /// default dir while writes pile into the override dir (#3909 review [1]).
    pub fn from_voice_config(voice: &VoiceConfig) -> Self {
        Self {
            progress_cache_dir: crate::voice::utils::expand_tilde(&voice.tts.progress_cache_dir),
            edge_temp_dir: crate::voice::utils::expand_tilde(&voice.audio.temp_dir),
            progress_max_age: DEFAULT_PROGRESS_CACHE_MAX_AGE,
            progress_max_bytes: DEFAULT_PROGRESS_CACHE_MAX_BYTES,
            progress_evict_grace: DEFAULT_PROGRESS_CACHE_EVICT_GRACE,
            edge_temp_orphan_max_age: DEFAULT_EDGE_TEMP_ORPHAN_MAX_AGE,
        }
    }

    /// Default-deployment dirs, used only by the pool-less status/registry path
    /// (`static_registry`) that never executes the job. The live scheduler
    /// always builds via [`Config::from_voice_config`] off the loaded config.
    pub fn default_runtime() -> Self {
        Self::from_voice_config(&VoiceConfig::default())
    }
}

/// Run one sweep pass. The sweeps are synchronous `std::fs`, so they run on a
/// `spawn_blocking` thread to stay off the scheduler's async worker. Errors in
/// the sweeps themselves are swallowed (logged at debug) so a single bad entry
/// never fails the maintenance job; only a join failure is surfaced.
pub async fn run(config: Config) -> Result<()> {
    let (cache, temp_orphans) = tokio::task::spawn_blocking(move || {
        let cache = sweep_progress_tts_cache(
            &config.progress_cache_dir,
            config.progress_max_age,
            config.progress_max_bytes,
            config.progress_evict_grace,
        );
        let temp_orphans =
            sweep_edge_tts_temp_orphans(&config.edge_temp_dir, config.edge_temp_orphan_max_age);
        (cache, temp_orphans)
    })
    .await
    .map_err(|error| anyhow::anyhow!("voice progress TTS cache sweep task: {error}"))?;

    if cache.removed_aged + cache.removed_over_cap + temp_orphans > 0 {
        tracing::info!(
            removed_aged = cache.removed_aged,
            removed_over_cap = cache.removed_over_cap,
            removed_bytes = cache.removed_bytes,
            retained_files = cache.retained_files,
            retained_bytes = cache.retained_bytes,
            removed_temp_orphans = temp_orphans,
            "[maintenance] voice progress TTS cache sweep"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_name(seed: char) -> String {
        format!("{}.mp3", std::iter::repeat_n(seed, 64).collect::<String>())
    }

    #[tokio::test]
    async fn run_bounds_cache_and_temp_dirs_end_to_end() {
        // #3909 — end-to-end: the maintenance job wiring evicts aged cache
        // files + orphaned temp mp3s and keeps fresh ones, via the real
        // spawn_blocking path.
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path().join("progress-cache");
        let temp_dir = temp.path().join("tmp");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();

        let fresh_cache = cache_dir.join(cache_name('a'));
        let aged_cache = cache_dir.join(cache_name('b'));
        let aged_orphan = temp_dir.join("agentdesk-edge-tts-1-2.mp3");
        std::fs::write(&fresh_cache, b"fresh").unwrap();
        std::fs::write(&aged_cache, b"aged").unwrap();
        std::fs::write(&aged_orphan, b"orphan").unwrap();

        let old = std::time::SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        for path in [&aged_cache, &aged_orphan] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }

        let config = Config {
            progress_cache_dir: cache_dir.clone(),
            edge_temp_dir: temp_dir.clone(),
            progress_max_age: Duration::from_secs(24 * 60 * 60),
            progress_max_bytes: u64::MAX,
            progress_evict_grace: DEFAULT_PROGRESS_CACHE_EVICT_GRACE,
            edge_temp_orphan_max_age: Duration::from_secs(60 * 60),
        };
        run(config).await.unwrap();

        assert!(fresh_cache.exists(), "fresh cache entry must survive");
        assert!(!aged_cache.exists(), "aged cache entry must be evicted");
        assert!(!aged_orphan.exists(), "aged temp orphan must be swept");
    }

    #[tokio::test]
    async fn from_voice_config_sweeps_operator_overridden_dirs() {
        // Issue [1] (#3909 codex review): when an operator overrides
        // `voice.tts.progress_cache_dir` / `voice.audio.temp_dir`, the sweep
        // must target THOSE dirs (the same ones the TTS write path resolves) —
        // not the defaults. Otherwise the override dir leaks forever.
        let temp = tempfile::tempdir().unwrap();
        let override_cache = temp.path().join("custom-cache");
        let override_temp = temp.path().join("custom-tmp");
        std::fs::create_dir_all(&override_cache).unwrap();
        std::fs::create_dir_all(&override_temp).unwrap();

        let aged_cache = override_cache.join(cache_name('c'));
        let aged_orphan = override_temp.join("agentdesk-edge-tts-9-9.mp3");
        std::fs::write(&aged_cache, b"aged").unwrap();
        std::fs::write(&aged_orphan, b"orphan").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        for path in [&aged_cache, &aged_orphan] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }

        let mut voice = VoiceConfig::default();
        voice.tts.progress_cache_dir = override_cache.clone();
        voice.audio.temp_dir = override_temp.clone();
        let mut config = Config::from_voice_config(&voice);
        // Shorten the caps so the 48h-old fixtures are swept by this run.
        config.progress_max_age = Duration::from_secs(24 * 60 * 60);
        config.edge_temp_orphan_max_age = Duration::from_secs(60 * 60);

        assert_eq!(config.progress_cache_dir, override_cache);
        assert_eq!(config.edge_temp_dir, override_temp);
        run(config).await.unwrap();

        assert!(
            !aged_cache.exists(),
            "sweep must clean the OVERRIDDEN cache dir, not the default"
        );
        assert!(
            !aged_orphan.exists(),
            "sweep must clean the OVERRIDDEN temp dir, not the default"
        );
    }

    #[test]
    fn default_runtime_resolves_absolute_voice_dirs() {
        let config = Config::default_runtime();
        assert!(
            config.progress_cache_dir.is_absolute() || cfg!(not(unix)),
            "progress cache dir should be tilde-expanded to an absolute path"
        );
        assert_eq!(config.progress_max_age, DEFAULT_PROGRESS_CACHE_MAX_AGE);
        assert_eq!(config.progress_max_bytes, DEFAULT_PROGRESS_CACHE_MAX_BYTES);
        assert_eq!(
            config.progress_evict_grace,
            DEFAULT_PROGRESS_CACHE_EVICT_GRACE
        );
    }
}
