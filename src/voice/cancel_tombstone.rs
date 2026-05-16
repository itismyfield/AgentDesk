//! #2374 — voice cancel tombstone, keyed by voice-background handoff
//! `message_id`.
//!
//! Background
//! ----------
//! PR #2373 (#2335) added a cancel-during-handoff handler in
//! `VoiceBargeInRuntime::dispatch_voice_background_handoff` that, after
//! the background turn is created on the target channel, observes a
//! barge-in / explicit-stop that arrived DURING the await and
//! synchronously cancels the just-started target-channel turn.
//!
//! Codex round-3 review of that PR flagged a residual architectural
//! concern: the cancel propagation is in-memory only and is tied to the
//! cancel-token of the brand-new target-channel turn. A SECOND cancel
//! that arrives slightly later for the SAME handoff (e.g. a retried
//! voice utterance triggers `process_voice_foreground_request` a second
//! time for the same source channel, the prior turn already finished /
//! was finalized, and the mailbox issues a new cancel token) could
//! re-fire downstream actions (ack synthesis, spoken reply playback)
//! because the second caller does not see the in-memory cancel state
//! the first cancel already wrote.
//!
//! Fix
//! ---
//! Record a process-local tombstone keyed by the handoff prompt's
//! `message_id` whenever a cancel-during-handoff happens. The handoff
//! `message_id` is durable across both callers (it is the same posted
//! message on the background text channel), so the second caller can
//! consult the tombstone before re-firing actions and discard itself.
//!
//! Storage is in-memory with a TTL slightly longer than the typical
//! handoff dispatch window (`TOMBSTONE_TTL`). Pruning is opportunistic
//! on every write; reads also re-check the expiry so a stale read
//! returns `None`.
//!
//! This module is intentionally narrow: it does NOT persist tombstones
//! to PG. The race window it closes is process-local (both callers run
//! in the same dcserver), and a dcserver restart between the two cancel
//! attempts is already covered by the background turn's own
//! cancel-on-restart recovery in `runtime_bootstrap`.

use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
    time::{Duration, Instant},
};

use poise::serenity_prelude::MessageId;

/// Tombstones survive long enough to cover the typical handoff dispatch
/// window (seconds to a minute) plus generous slack for retry waves.
/// Five minutes matches the upper bound of legitimate "second cancel for
/// the same handoff" arrivals seen in production traces; anything older
/// than this almost certainly belongs to an unrelated handoff that
/// happens to reuse a `MessageId` (impossible in practice — Discord
/// message ids are monotonically increasing snowflakes — but the TTL
/// also bounds memory growth in case of pathological never-pruned
/// channels).
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
    /// Record (or refresh) a tombstone for `handoff_message_id`. Idempotent;
    /// a later record with a different reason overwrites the prior reason
    /// (last-cancel-wins for the label, but presence-of-tombstone is what
    /// downstream consumers branch on).
    pub(crate) fn record(&self, handoff_message_id: MessageId, reason: impl Into<String>) {
        if let Ok(mut entries) = self.entries.write() {
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
    }

    /// Return the recorded cancel reason if a non-expired tombstone exists.
    ///
    /// The tombstone is NOT consumed by lookup — multiple late callers may
    /// each need to observe it. The TTL bounds memory growth.
    pub(crate) fn lookup(&self, handoff_message_id: MessageId) -> Option<String> {
        let entries = self.entries.read().ok()?;
        let stored = entries.get(&handoff_message_id.get())?;
        if Instant::now() >= stored.expires_at {
            None
        } else {
            Some(stored.reason.clone())
        }
    }

    /// Explicit removal (test helper / future graceful-shutdown path).
    #[cfg(test)]
    pub(crate) fn forget(&self, handoff_message_id: MessageId) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(&handoff_message_id.get());
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.read().map(|guard| guard.len()).unwrap_or(0)
    }
}

fn prune_expired_locked(entries: &mut HashMap<u64, StoredTombstone>, now: Instant) {
    entries.retain(|_, stored| stored.expires_at > now);
}

static GLOBAL_STORE: OnceLock<VoiceCancelTombstoneStore> = OnceLock::new();

/// Process-wide tombstone store. Shared because the dispatch path and
/// the late-cancel path both run in the same dcserver process and need a
/// common view of "this handoff was already cancelled".
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
    fn global_store_is_process_wide_singleton() {
        let a = global_store() as *const _;
        let b = global_store() as *const _;
        assert_eq!(a, b, "global_store must return the same singleton");
    }
}
