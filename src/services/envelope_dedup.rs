//! Discord relay envelope dedup (#2662).
//!
//! Background: audit/2026-05-19 measured 95 user-message turns × ~2KB of
//! `[Authoritative Instructions]` envelope re-sent every turn = ~200KB /
//! session. The Codex+resumed-session path already suppresses repeats via
//! [`crate::services::provider::should_omit_repeated_system_prompt`]; this
//! module generalizes the idea so we can suppress per-session-key
//! independently of provider — useful for Claude resume sessions, Codex
//! threads, and any future provider that retains conversation state in its
//! own session store.
//!
//! Behavior is **infrastructure only by default**. A caller has to
//! explicitly look up `was_seen` and choose to omit the envelope; existing
//! callers continue to behave exactly as before. We ship the infrastructure
//! in this PR so a follow-up can flip the policy when all providers are
//! verified to respect the omission.
//!
//! Storage: in-memory only, keyed by `(session_key, envelope_blake3)`.
//! The blake3 hash collapses identical envelopes to a fixed-size key so
//! we are not pinning multi-KB payloads in the dedup table.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// How long to remember that an envelope was sent on a given session.
/// Long-running provider sessions (Claude `--resume`, Codex threads) live
/// for hours, so we keep the entry until eviction rather than expiring it
/// aggressively. Operators can override via env if needed.
const ENTRY_TTL: Duration = Duration::from_secs(60 * 60 * 24);

/// Hard cap on table size. Each entry holds a 32-byte hash + ~64 bytes of
/// session-key + timestamp — at the cap we use well under 1 MB.
const MAX_ENTRIES: usize = 8192;

/// Cache key used by the dedup store. Both fields are stack-friendly
/// (`String` + `[u8; 32]`); we intentionally hash with blake3 first so we
/// never store the raw envelope payload.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct DedupKey {
    session_key: String,
    envelope_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
struct DedupEntry {
    /// When the envelope was first recorded. Used for eviction and TTL.
    first_seen_at: Instant,
}

/// Stable hash of an envelope payload. `blake3` is fast enough that hashing
/// a ~3KB payload per turn is sub-microsecond on commodity hardware, so we
/// can call this on the hot path.
pub fn hash_envelope(envelope: &str) -> [u8; 32] {
    *blake3::hash(envelope.as_bytes()).as_bytes()
}

/// Shared dedup store. Cloning is cheap (`Arc`-backed map).
#[derive(Clone)]
pub struct EnvelopeDedupStore {
    inner: Arc<Inner>,
}

struct Inner {
    entries: DashMap<DedupKey, DedupEntry>,
    ttl: Duration,
    max_entries: usize,
}

impl Default for EnvelopeDedupStore {
    fn default() -> Self {
        Self::new(ENTRY_TTL, MAX_ENTRIES)
    }
}

impl EnvelopeDedupStore {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: DashMap::new(),
                ttl,
                // Always allow at least one entry — zero would silently
                // disable dedup which is almost never the operator's intent.
                max_entries: max_entries.max(1),
            }),
        }
    }

    /// Returns `true` if this `(session_key, envelope_hash)` was already
    /// recorded via [`mark_sent`] within the TTL window. Idempotent and
    /// side-effect-free.
    pub fn was_seen(&self, session_key: &str, envelope_hash: &[u8; 32]) -> bool {
        let key = DedupKey {
            session_key: session_key.to_string(),
            envelope_hash: *envelope_hash,
        };
        match self.inner.entries.get(&key) {
            Some(entry) => {
                Instant::now().saturating_duration_since(entry.first_seen_at) < self.inner.ttl
            }
            None => false,
        }
    }

    /// Record that an envelope was sent. Subsequent [`was_seen`] calls for
    /// the same key will return `true` until eviction or TTL expiry.
    pub fn mark_sent(&self, session_key: &str, envelope_hash: &[u8; 32]) {
        // Empty session keys are nonsense — silently drop rather than
        // unifying every callsite into the same bucket.
        if session_key.trim().is_empty() {
            return;
        }
        let key = DedupKey {
            session_key: session_key.to_string(),
            envelope_hash: *envelope_hash,
        };
        self.evict_if_needed();
        self.inner.entries.insert(
            key,
            DedupEntry {
                first_seen_at: Instant::now(),
            },
        );
    }

    /// Drop every entry for a given session. Called when the session is
    /// reset (provider crash, `/clear`, etc.) so the next turn re-sends the
    /// envelope.
    pub fn forget_session(&self, session_key: &str) {
        self.inner
            .entries
            .retain(|key, _| key.session_key != session_key);
    }

    /// Drop every entry. Used by tests and operator tooling.
    pub fn clear(&self) {
        self.inner.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    /// Evict a single oldest-by-first-seen entry when at capacity.
    /// O(n) — fine at cap=8k since dedup happens once per turn.
    fn evict_if_needed(&self) {
        if self.inner.entries.len() < self.inner.max_entries {
            return;
        }
        let mut oldest: Option<(DedupKey, Instant)> = None;
        for entry in self.inner.entries.iter() {
            let seen = entry.value().first_seen_at;
            match &oldest {
                None => oldest = Some((entry.key().clone(), seen)),
                Some((_, ts)) if seen < *ts => oldest = Some((entry.key().clone(), seen)),
                _ => {}
            }
        }
        if let Some((victim, _)) = oldest {
            self.inner.entries.remove(&victim);
        }
    }
}

/// Process-wide dedup store. Constructed lazily.
pub fn shared() -> &'static EnvelopeDedupStore {
    static STORE: OnceLock<EnvelopeDedupStore> = OnceLock::new();
    STORE.get_or_init(EnvelopeDedupStore::default)
}

/// Convenience: hash + lookup in one call. Returns `true` when the
/// envelope was already shown for the given session.
pub fn envelope_already_sent(session_key: &str, envelope: &str) -> bool {
    if session_key.trim().is_empty() {
        return false;
    }
    let hash = hash_envelope(envelope);
    shared().was_seen(session_key, &hash)
}

/// Convenience: hash + record. No-op if `session_key` is empty.
pub fn record_envelope_sent(session_key: &str, envelope: &str) {
    if session_key.trim().is_empty() {
        return;
    }
    let hash = hash_envelope(envelope);
    shared().mark_sent(session_key, &hash);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_across_calls() {
        let a = hash_envelope("hello world");
        let b = hash_envelope("hello world");
        assert_eq!(a, b);
        let c = hash_envelope("HELLO WORLD");
        assert_ne!(a, c);
    }

    #[test]
    fn mark_then_was_seen_returns_true() {
        let store = EnvelopeDedupStore::default();
        let h = hash_envelope("[Authoritative Instructions]\nfoo");
        assert!(!store.was_seen("session-x", &h));
        store.mark_sent("session-x", &h);
        assert!(store.was_seen("session-x", &h));
    }

    #[test]
    fn different_session_keys_isolated() {
        let store = EnvelopeDedupStore::default();
        let h = hash_envelope("env");
        store.mark_sent("session-A", &h);
        assert!(store.was_seen("session-A", &h));
        assert!(!store.was_seen("session-B", &h));
    }

    #[test]
    fn different_envelope_hash_isolated() {
        let store = EnvelopeDedupStore::default();
        let h1 = hash_envelope("env-1");
        let h2 = hash_envelope("env-2");
        store.mark_sent("session-A", &h1);
        assert!(store.was_seen("session-A", &h1));
        assert!(!store.was_seen("session-A", &h2));
    }

    #[test]
    fn empty_session_key_is_noop() {
        let store = EnvelopeDedupStore::default();
        let h = hash_envelope("env");
        store.mark_sent("", &h);
        // The convenience helper should also short-circuit.
        assert!(!envelope_already_sent("", "env"));
        assert!(!store.was_seen("", &h));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn forget_session_removes_only_that_sessions_entries() {
        let store = EnvelopeDedupStore::default();
        let h = hash_envelope("env");
        store.mark_sent("session-A", &h);
        store.mark_sent("session-B", &h);
        store.forget_session("session-A");
        assert!(!store.was_seen("session-A", &h));
        assert!(store.was_seen("session-B", &h));
    }

    #[test]
    fn ttl_expires_entry() {
        let store = EnvelopeDedupStore::new(Duration::ZERO, 8);
        let h = hash_envelope("env");
        store.mark_sent("session-A", &h);
        // Zero TTL means every subsequent lookup is stale.
        assert!(!store.was_seen("session-A", &h));
    }

    #[test]
    fn capacity_cap_evicts_oldest() {
        let store = EnvelopeDedupStore::new(Duration::from_secs(60), 2);
        let h1 = hash_envelope("env-1");
        let h2 = hash_envelope("env-2");
        let h3 = hash_envelope("env-3");
        store.mark_sent("s", &h1);
        store.mark_sent("s", &h2);
        // Both above are at-cap. Inserting #3 must evict one.
        store.mark_sent("s", &h3);
        assert!(store.len() <= 2);
        // The newest entry must survive.
        assert!(store.was_seen("s", &h3));
    }

    #[test]
    fn convenience_helpers_roundtrip() {
        // shared() is process-global so we use unique session keys to
        // avoid polluting / being polluted by other tests.
        let session_key = "test-envelope-dedup-convenience-roundtrip-xyz";
        let payload = "[Authoritative Instructions]\ndedup test";
        assert!(!envelope_already_sent(session_key, payload));
        record_envelope_sent(session_key, payload);
        assert!(envelope_already_sent(session_key, payload));
        // Clean up so we don't leak this entry across tests.
        shared().forget_session(session_key);
    }
}
