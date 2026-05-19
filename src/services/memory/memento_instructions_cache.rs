//! Memento server instructions delta cache.
//!
//! Issue #2664 — every Memento MCP re-authentication (e.g. when an access-key
//! expires and the client re-initializes) historically caused the server to
//! re-emit the same ~1.5–2 KB `# Memento MCP Server` instructions block. Clients
//! that follow the spec — including the Claude Code harness driving AgentDesk
//! — surface that text as a fresh `system-reminder` on every re-init, which
//! costs ~500 tokens per re-auth and pollutes the model's working context.
//!
//! AgentDesk is a *consumer* of Memento, not the system-prompt assembler, so
//! we cannot stop Claude Code from re-injecting the text. What we *can* do is:
//!
//! 1. Capture the instructions string from each `initialize` response.
//! 2. Hash it and compare against the last hash we saw on this process.
//! 3. Emit a structured "unchanged vs changed" delta signal so operators
//!    and downstream tooling (and any future system-prompt assembler we
//!    own) can decide whether to re-prepend the block.
//!
//! The cache is intentionally process-wide and lock-protected: every Memento
//! re-init in the same `agentdesk` process collapses onto the same dedup
//! state, which is what the audit's "1회만 prepend" requirement maps to from
//! the AgentDesk side.

use std::sync::{Mutex, OnceLock};

/// Outcome of comparing a freshly-received instructions block to the last
/// instructions block we observed on this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstructionsDelta {
    /// First instructions block we've seen on this process — the consumer
    /// *must* surface it (system prompt prepend, log breadcrumb, …).
    FirstSeen,
    /// Same hash as last time. The consumer should treat this as a no-op:
    /// downstream system-prompt state is unchanged.
    Unchanged,
    /// Hash differs from the previous block. The consumer must refresh
    /// whatever cached representation it built from the last block.
    Changed,
    /// Memento returned no instructions field — nothing to compare. We do
    /// not invalidate the previously-cached hash on `Missing`, so a
    /// degraded re-init won't trigger a spurious Changed afterwards.
    Missing,
}

#[derive(Clone, Debug, Default)]
struct CacheState {
    last_hash: Option<u64>,
    /// Total number of `record` calls — distinct from `last_hash` so we
    /// can distinguish "never observed" from "observed once and matched".
    observations: u64,
    /// How many of those observations matched the previous hash. Used by
    /// observability to report dedup savings.
    unchanged_count: u64,
    /// How many were genuine changes (excluding the FirstSeen).
    changed_count: u64,
    /// How many of those re-inits returned no instructions field.
    missing_count: u64,
}

fn cache() -> &'static Mutex<CacheState> {
    static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(CacheState::default()))
}

/// Stable, non-cryptographic 64-bit hash over `s`. We deliberately do *not*
/// use the standard library's `DefaultHasher` (which is randomized per
/// process) — a stable hash lets the cache survive process-wide module
/// re-initialization in tests, and lets two cooperating processes compare
/// notes if we ever wire this through observability.
fn stable_hash(s: &str) -> u64 {
    // FNV-1a 64. Tiny, deterministic, no extra dependency.
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Record an observation of a Memento `initialize` `result.instructions`
/// value (possibly `None` when the server omitted the field) and return the
/// delta classification.
///
/// This is the only public mutator — callers should not poke `CacheState`
/// directly.
pub(crate) fn record_instructions(observed: Option<&str>) -> InstructionsDelta {
    let mut state = cache().lock().unwrap_or_else(|err| err.into_inner());
    state.observations = state.observations.saturating_add(1);

    let Some(text) = observed else {
        state.missing_count = state.missing_count.saturating_add(1);
        return InstructionsDelta::Missing;
    };
    let hash = stable_hash(text);
    match state.last_hash {
        None => {
            state.last_hash = Some(hash);
            InstructionsDelta::FirstSeen
        }
        Some(prev) if prev == hash => {
            state.unchanged_count = state.unchanged_count.saturating_add(1);
            InstructionsDelta::Unchanged
        }
        Some(_) => {
            state.last_hash = Some(hash);
            state.changed_count = state.changed_count.saturating_add(1);
            InstructionsDelta::Changed
        }
    }
}

/// Public snapshot for observability / debug surfaces. Returns
/// `(observations, unchanged, changed, missing)`.
pub(crate) fn instructions_cache_stats() -> InstructionsCacheStats {
    let state = cache().lock().unwrap_or_else(|err| err.into_inner());
    InstructionsCacheStats {
        observations: state.observations,
        unchanged_count: state.unchanged_count,
        changed_count: state.changed_count,
        missing_count: state.missing_count,
        cached_hash: state.last_hash,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InstructionsCacheStats {
    pub observations: u64,
    pub unchanged_count: u64,
    pub changed_count: u64,
    pub missing_count: u64,
    pub cached_hash: Option<u64>,
}

/// Reset the process-wide cache. Test-only: real callers should never want
/// to clear the dedup state.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut state = cache().lock().unwrap_or_else(|err| err.into_inner());
    *state = CacheState::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All tests in this module share the process-wide cache via the
    /// `OnceLock`. Run them serially behind a private mutex so they don't
    /// race when `cargo test` runs them on multiple threads.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn stable_hash_is_deterministic() {
        let s = "# Memento MCP Server\n\n연결 성공.";
        assert_eq!(stable_hash(s), stable_hash(s));
        assert_ne!(stable_hash(s), stable_hash("different"));
    }

    #[test]
    fn first_seen_then_unchanged_then_changed() {
        let _g = lock();
        reset_for_tests();
        let first = "instructions-v1";
        assert_eq!(
            record_instructions(Some(first)),
            InstructionsDelta::FirstSeen
        );
        assert_eq!(
            record_instructions(Some(first)),
            InstructionsDelta::Unchanged
        );
        assert_eq!(
            record_instructions(Some(first)),
            InstructionsDelta::Unchanged
        );
        let second = "instructions-v2";
        assert_eq!(
            record_instructions(Some(second)),
            InstructionsDelta::Changed
        );
        assert_eq!(
            record_instructions(Some(second)),
            InstructionsDelta::Unchanged
        );
    }

    #[test]
    fn missing_does_not_invalidate_previous_hash() {
        let _g = lock();
        reset_for_tests();
        assert_eq!(
            record_instructions(Some("v1")),
            InstructionsDelta::FirstSeen
        );
        assert_eq!(record_instructions(None), InstructionsDelta::Missing);
        // After a Missing, re-receiving the same text must still report as
        // Unchanged — operators expect the cache to be sticky.
        assert_eq!(
            record_instructions(Some("v1")),
            InstructionsDelta::Unchanged
        );
    }

    #[test]
    fn first_seen_after_only_missing_observations() {
        // If the very first observation is Missing, the next real value
        // should still be classified as FirstSeen — we never had a hash to
        // compare against.
        let _g = lock();
        reset_for_tests();
        assert_eq!(record_instructions(None), InstructionsDelta::Missing);
        assert_eq!(
            record_instructions(Some("v1")),
            InstructionsDelta::FirstSeen
        );
    }

    #[test]
    fn stats_track_each_outcome() {
        let _g = lock();
        reset_for_tests();
        let _ = record_instructions(Some("v1"));
        let _ = record_instructions(Some("v1"));
        let _ = record_instructions(Some("v2"));
        let _ = record_instructions(None);

        let stats = instructions_cache_stats();
        assert_eq!(stats.observations, 4);
        assert_eq!(stats.unchanged_count, 1);
        assert_eq!(stats.changed_count, 1);
        assert_eq!(stats.missing_count, 1);
        assert!(stats.cached_hash.is_some());
    }
}
