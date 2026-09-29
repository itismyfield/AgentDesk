//! Cancel tombstones keyed by the voice background handoff's `message_id`.
//!
//! A cancel observed for a handoff records one; a later caller for the same handoff
//! looks it up and discards itself instead of re-firing the spoken ack or reply.
//! In-memory only: both callers run in the same dcserver, so nothing survives a restart.

use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
    time::{Duration, Instant},
};

use poise::serenity_prelude::MessageId;

/// Covers the handoff dispatch window plus slack for retry waves, and bounds memory
/// for tombstones nobody looks up again.
const TOMBSTONE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
struct StoredTombstone {
    reason: String,
    expires_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct VoiceCancelTombstoneStore {
    entries: RwLock<HashMap<u64, StoredTombstone>>,
}

impl VoiceCancelTombstoneStore {
    /// Write guard that recovers from poisoning: the map stays consistent after a
    /// writer panic, and dropping writes instead would silently disable the guard.
    fn write_entries(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<u64, StoredTombstone>> {
        self.entries.write().unwrap_or_else(|poisoned| {
            tracing::warn!(
                "voice cancel-tombstone lock was poisoned; recovering in place so re-fire \
                 protection keeps working (#3914)"
            );
            poisoned.into_inner()
        })
    }

    /// Record (or refresh) a tombstone. The last reason wins; callers branch only on
    /// whether a tombstone exists.
    pub(crate) fn record(&self, handoff_message_id: MessageId, reason: impl Into<String>) {
        let mut entries = self.write_entries();
        let now = Instant::now();
        prune_expired_locked(&mut entries, now);
        entries.insert(
            handoff_message_id.get(),
            StoredTombstone {
                reason: reason.into(),
                expires_at: now + TOMBSTONE_TTL,
            },
        );
    }

    /// Reason of a live tombstone. Not consumed, since several late callers may each
    /// need it; also prunes, so eviction does not wait for another `record`.
    pub(crate) fn lookup(&self, handoff_message_id: MessageId) -> Option<String> {
        let mut entries = self.write_entries();
        let now = Instant::now();
        prune_expired_locked(&mut entries, now);
        entries
            .get(&handoff_message_id.get())
            .map(|stored| stored.reason.clone())
    }

    /// Remove a tombstone (tests only).
    #[cfg(test)]
    pub(crate) fn forget(&self, handoff_message_id: MessageId) {
        self.write_entries().remove(&handoff_message_id.get());
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.write_entries().len()
    }
}

fn prune_expired_locked(entries: &mut HashMap<u64, StoredTombstone>, now: Instant) {
    entries.retain(|_, stored| stored.expires_at > now);
}

static GLOBAL_STORE: OnceLock<VoiceCancelTombstoneStore> = OnceLock::new();

/// Process-wide store shared by the cancel path and the handoff dispatch path.
pub(crate) fn global_store() -> &'static VoiceCancelTombstoneStore {
    GLOBAL_STORE.get_or_init(VoiceCancelTombstoneStore::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u64) -> MessageId {
        MessageId::new(id)
    }

    #[test]
    fn record_then_lookup_returns_recorded_reason() {
        let store = VoiceCancelTombstoneStore::default();
        store.record(msg(42), "voice_foreground_cancel_during_handoff");
        assert_eq!(
            store.lookup(msg(42)).as_deref(),
            Some("voice_foreground_cancel_during_handoff"),
            "fresh tombstone must be visible to a subsequent lookup"
        );
    }

    #[test]
    fn lookup_does_not_consume_so_multiple_callers_see_tombstone() {
        let store = VoiceCancelTombstoneStore::default();
        store.record(msg(100), "voice_barge_in_live_cut");
        assert!(store.lookup(msg(100)).is_some(), "first lookup observes");
        assert!(
            store.lookup(msg(100)).is_some(),
            "second lookup still observes — tombstones are not consumed by lookup so a \
             retried second cancel attempt for the same handoff can also discard itself"
        );
    }

    #[test]
    fn unrelated_message_id_returns_none() {
        let store = VoiceCancelTombstoneStore::default();
        store.record(msg(1), "explicit_stop");
        assert!(
            store.lookup(msg(2)).is_none(),
            "tombstone is keyed by handoff message id and must NOT alias to other ids"
        );
    }

    #[test]
    fn record_refreshes_reason_for_same_handoff() {
        let store = VoiceCancelTombstoneStore::default();
        store.record(msg(7), "first_reason");
        store.record(msg(7), "second_reason");
        assert_eq!(
            store.lookup(msg(7)).as_deref(),
            Some("second_reason"),
            "later record overwrites the reason — presence-of-tombstone is what callers \
             branch on; label is best-effort attribution"
        );
        assert_eq!(store.len(), 1, "same handoff id must dedupe");
    }

    #[test]
    fn forget_clears_tombstone() {
        let store = VoiceCancelTombstoneStore::default();
        store.record(msg(9), "x");
        store.forget(msg(9));
        assert!(store.lookup(msg(9)).is_none());
    }

    #[test]
    fn lookup_prunes_expired_entries() {
        let store = VoiceCancelTombstoneStore::default();
        {
            // `record()` always sets a future expiry, so seed the entries directly.
            let mut entries = store.write_entries();
            entries.insert(
                1,
                StoredTombstone {
                    reason: "stale".to_string(),
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
            entries.insert(
                2,
                StoredTombstone {
                    reason: "fresh".to_string(),
                    expires_at: Instant::now() + TOMBSTONE_TTL,
                },
            );
        }
        assert_eq!(store.len(), 2);

        // Querying id 2 still prunes the expired id 1.
        assert_eq!(store.lookup(msg(2)).as_deref(), Some("fresh"));
        assert_eq!(store.len(), 1, "expired tombstone must be pruned on lookup");
        assert!(store.lookup(msg(1)).is_none());
    }

    #[test]
    fn recovers_from_a_poisoned_lock() {
        let store = std::sync::Arc::new(VoiceCancelTombstoneStore::default());
        store.record(msg(1), "before");

        let poison_store = store.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison_store.write_entries();
            panic!("intentional poison for test");
        })
        .join();

        store.record(msg(2), "after");
        assert_eq!(store.lookup(msg(2)).as_deref(), Some("after"));
        assert_eq!(
            store.lookup(msg(1)).as_deref(),
            Some("before"),
            "pre-poison tombstones must survive lock recovery"
        );
    }

    #[test]
    fn global_store_is_process_wide_singleton() {
        let a = global_store() as *const _;
        let b = global_store() as *const _;
        assert_eq!(a, b, "global_store must return the same singleton");
    }
}
