//! #4370: in-memory ledger of restart-re-adopted REAL-user mailbox owners.
//!
//! Extracted out of `mod.rs` so the discord giant does not re-inflate: the only
//! thing that stays in `SharedData` is the single `readopted_mailbox_ledger`
//! field; the ledger type, its entry, and the accessor methods live here.

use super::SharedData;
use crate::services::provider::ProviderKind;

/// #4370: in-memory record of a mailbox slot this process re-adopted from
/// persisted inflight state after a restart. Keyed in [`ReadoptedMailboxLedger`]
/// by `(provider, channel_id)`.
///
/// Only the process that performed the re-adopt can know that a mailbox belongs
/// to a restart-re-adopted REAL user turn: the on-disk `readopted_from_inflight`
/// marker cannot be relied upon on a DrainRestart-preserved row (the identity-
/// gated save refuses to write a row that still carries `restart_mode`, see
/// `inflight/save_store/identity_gate.rs`). This ledger is therefore the
/// authoritative source for the row-ABSENT ("Path B") stale-reclaim decision:
/// when the on-disk row was cleared but the mailbox slot is stuck owned by the
/// re-adopted real user, there is nothing left on disk to consult.
///
/// Its lifetime is exactly right. A fresh process re-derives the mailbox from
/// disk, so the ledger never needs to persist across restarts; and an entry that
/// outlives its own turn is INERT — a live successor turn owns a DIFFERENT
/// `active_user_message_id`, so a stale entry can never match it (that exact-id
/// requirement, plus the `>= 120s` age gate on the resulting `OwnerInflightAbsent`
/// reason, is the live-turn-theft guard).
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) struct ReadoptedMailboxOwner {
    /// The real Discord user id that owns the re-adopted mailbox turn.
    pub owner_user_id: u64,
    /// The mailbox `active_user_message_id` of the re-adopted turn (== the turn's
    /// effective finalizer id / `MessageId` the mailbox slot carries).
    pub active_user_message_id: u64,
}

/// #4370: the per-process ledger. Keyed by `(provider, channel_id)`; set at the
/// inflight re-adopt site and consulted by the TUI-direct synthetic
/// `stale_reclaim` path when the on-disk row is ABSENT (#4370 Path B).
#[derive(Default)]
pub(in crate::services::discord) struct ReadoptedMailboxLedger {
    entries: dashmap::DashMap<(ProviderKind, u64), ReadoptedMailboxOwner>,
}

impl SharedData {
    /// #4370: record that this process re-adopted `(provider, channel_id)`'s
    /// mailbox from persisted inflight state after a restart. A fresh re-adopt
    /// OVERWRITES any prior entry for the channel (the earlier turn can no longer
    /// own the mailbox once a new turn was re-adopted into it).
    pub(in crate::services::discord) fn record_readopted_mailbox_owner(
        &self,
        provider: &ProviderKind,
        channel_id: u64,
        owner_user_id: u64,
        active_user_message_id: u64,
    ) {
        self.readopted_mailbox_ledger.entries.insert(
            (provider.clone(), channel_id),
            ReadoptedMailboxOwner {
                owner_user_id,
                active_user_message_id,
            },
        );
    }

    /// #4370: `true` iff the ledger records `(provider, channel_id)` as a mailbox
    /// re-adopted from inflight whose owner AND `active_user_message_id` BOTH
    /// still match the live mailbox. This is the row-ABSENT ("Path B") reclaim
    /// authority: a live successor turn owns a different `active_user_message_id`,
    /// so it can never match — the exact-id requirement is the live-turn-theft
    /// guard, reinforced by the `>= 120s` age gate on the `OwnerInflightAbsent`
    /// reason that an absent row yields.
    pub(in crate::services::discord) fn is_readopted_mailbox_owner(
        &self,
        provider: &ProviderKind,
        channel_id: u64,
        owner_user_id: u64,
        active_user_message_id: u64,
    ) -> bool {
        self.readopted_mailbox_ledger
            .entries
            .get(&(provider.clone(), channel_id))
            .is_some_and(|entry| {
                entry.owner_user_id == owner_user_id
                    && entry.active_user_message_id == active_user_message_id
            })
    }

    /// #4370: drop the ledger entry once it can no longer be correct — after a
    /// successful reclaim frees the mailbox. (Stale entries are already inert, but
    /// evicting keeps the map bounded and makes the "reclaimed once" edge explicit.)
    pub(in crate::services::discord) fn evict_readopted_mailbox_owner(
        &self,
        provider: &ProviderKind,
        channel_id: u64,
    ) {
        self.readopted_mailbox_ledger
            .entries
            .remove(&(provider.clone(), channel_id));
    }
}
