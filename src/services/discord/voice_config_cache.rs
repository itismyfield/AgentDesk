//! F6 (#2046) `Config` snapshot hot-cache extracted from `VoiceBargeInRuntime`
//! (#3038 god-object split, config-cache slice).
//!
//! Hosts [`ConfigSnapshotCache`], the single `Mutex`-guarded snapshot slot that
//! previously lived directly on `VoiceBargeInRuntime`. Moving it (together with
//! its sole TTL constant `VOICE_CONFIG_CACHE_TTL`) into this sibling module both
//! isolates the concern and physically shrinks the `voice_barge_in.rs` giant
//! rather than re-inflating it. The lock discipline, TTL fallback, and
//! side-effect ordering are byte-for-byte unchanged.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// F6 (#2046): voice config 핫캐시 TTL. 5초 안 utterance 는 캐시 재사용.
const VOICE_CONFIG_CACHE_TTL: Duration = Duration::from_secs(5);

/// Cohesive sub-concern of `VoiceBargeInRuntime`: the F6 (#2046) `Config`
/// snapshot hot-cache (#3038 god-object split, config-cache slice).
///
/// Wraps the single `Mutex<Option<(Instant, Arc<Config>)>>` slot that used to
/// live directly on `VoiceBargeInRuntime`. The accessors below preserve the
/// original lock discipline and TTL fallback exactly:
/// - `lookup_within_ttl` returns the cached `Arc<Config>` only while the entry
///   is younger than `VOICE_CONFIG_CACHE_TTL`, silently treating a poisoned /
///   contended lock as a miss (`Ok` guard required, never blocking-on-panic),
/// - `store` overwrites the slot with a freshly stamped entry, also ignoring a
///   failed lock,
/// - the spawn_blocking reload itself stays on the runtime so the async control
///   flow and side-effect ordering are byte-for-byte unchanged.
pub(in crate::services::discord) struct ConfigSnapshotCache {
    slot: std::sync::Mutex<Option<(Instant, Arc<crate::config::Config>)>>,
}

impl ConfigSnapshotCache {
    pub(in crate::services::discord) fn new() -> Self {
        Self {
            slot: std::sync::Mutex::new(None),
        }
    }

    /// Return the cached snapshot iff it is still within the TTL window as of
    /// `now`. A poisoned or contended lock is treated as a cache miss, matching
    /// the original `if let Ok(guard) = ... .lock()` fallback.
    pub(in crate::services::discord) fn lookup_within_ttl(
        &self,
        now: Instant,
    ) -> Option<Arc<crate::config::Config>> {
        if let Ok(guard) = self.slot.lock()
            && let Some((loaded_at, cached)) = guard.as_ref()
            && now.duration_since(*loaded_at) < VOICE_CONFIG_CACHE_TTL
        {
            return Some(cached.clone());
        }
        None
    }

    /// Stamp and store a freshly loaded snapshot. A failed lock is ignored,
    /// matching the original `if let Ok(mut guard) = ... .lock()` write.
    pub(in crate::services::discord) fn store(
        &self,
        loaded_at: Instant,
        config: Arc<crate::config::Config>,
    ) {
        if let Ok(mut guard) = self.slot.lock() {
            *guard = Some((loaded_at, config));
        }
    }

    /// Test-only seed of the cache slot (mirrors the previous direct
    /// `*runtime.config_cache.lock().unwrap() = Some(...)` writes).
    #[cfg(test)]
    pub(in crate::services::discord) fn seed(
        &self,
        loaded_at: Instant,
        config: Arc<crate::config::Config>,
    ) {
        *self.slot.lock().unwrap() = Some((loaded_at, config));
    }
}
