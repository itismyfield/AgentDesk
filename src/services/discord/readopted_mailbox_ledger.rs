//! #4370: in-memory ledger of restart-re-adopted REAL-user mailbox owners.
//!
//! Extracted out of `mod.rs` so the discord giant does not re-inflate: the only
//! thing that stays in `SharedData` is the single `readopted_mailbox_ledger`
//! field; the ledger type, its entry, and the accessor methods live here.

use super::SharedData;
use super::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;

/// Stable identity of one mailbox-owning turn. Unlike the recovery episode pin,
/// this deliberately excludes handoff authority that may advance while the same
/// turn is still running (`current_msg_id`, session/output paths, runtime kind,
/// and relay owner). The terminal-commit path must still be able to finish the
/// ledger entry after those legitimate progress writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct ReadoptedMailboxTurnPin {
    user_msg_id: u64,
    started_at: String,
    turn_start_offset: Option<u64>,
    born_generation: u64,
    turn_nonce: Option<String>,
}

impl ReadoptedMailboxTurnPin {
    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        Self {
            user_msg_id: state.user_msg_id,
            started_at: state.started_at.clone(),
            turn_start_offset: state.turn_start_offset,
            born_generation: state.born_generation,
            turn_nonce: state.turn_nonce.clone(),
        }
    }
}

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
/// `active_user_message_id`, so a stale entry can never match it. The
/// live-turn-theft guard is threefold: that exact-id requirement, the
/// `finished` bit (#4370 R3-1 — a still-live re-adopted turn is never reclaimed
/// even on an exact-id match), and the `>= 120s` age gate on the resulting
/// `OwnerInflightAbsent` reason.
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct ReadoptedMailboxOwner {
    /// The real Discord user id that owns the re-adopted mailbox turn.
    pub owner_user_id: u64,
    /// The mailbox `active_user_message_id` of the re-adopted turn (== the turn's
    /// effective finalizer id / `MessageId` the mailbox slot carries).
    pub active_user_message_id: u64,
    /// #4370 R3-1: `true` once this re-adopted turn's TERMINAL DELIVERY committed.
    /// It is the row-ABSENT ("Path B") analogue of the present row's on-disk
    /// `terminal_delivery_committed`, and is stamped at the watcher
    /// terminal-commit clear — the SAME production path that produces the
    /// row-ABSENT shape (`tmux_watcher/terminal_commit_epilogue.rs`).
    ///
    /// The absent-row reclaim REQUIRES it. Without it, safety rested on the
    /// UNENFORCED invariant "a cleared on-disk row implies the turn is not live":
    /// absence + exact-id + age alone could, in principle, steal a genuinely LIVE
    /// re-adopted turn whose durable row merely happened to be absent. With it,
    /// such a turn is `finished == false` and is refused — the invariant is now an
    /// enforced fact, not an assumption (#4370 R3-1).
    pub finished: bool,
    /// Stable turn identity for automatic recovery readoption. Legacy/restart
    /// callers that do not hold episode authority record `None`.
    turn_pin: Option<ReadoptedMailboxTurnPin>,
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
                // #4370 R3-1: a freshly re-adopted turn is LIVE — its terminal
                // delivery has not committed yet. The watcher terminal-commit
                // clear stamps `finished` when it does.
                finished: false,
                turn_pin: None,
            },
        );
    }

    pub(in crate::services::discord) fn record_readopted_mailbox_owner_for_episode(
        &self,
        provider: &ProviderKind,
        channel_id: u64,
        owner_user_id: u64,
        active_user_message_id: u64,
        state: &InflightTurnState,
    ) {
        self.readopted_mailbox_ledger.entries.insert(
            (provider.clone(), channel_id),
            ReadoptedMailboxOwner {
                owner_user_id,
                active_user_message_id,
                finished: false,
                turn_pin: Some(ReadoptedMailboxTurnPin::from_state(state)),
            },
        );
    }

    /// #4370 R3-1: stamp the ledger entry for `(provider, channel_id)` FINISHED —
    /// its re-adopted turn committed terminal delivery. Flips ONLY the entry whose
    /// `(owner_user_id, active_user_message_id)` still matches, so a newer re-adopt
    /// that overwrote the slot with a DIFFERENT turn is never marked finished on an
    /// older turn's commit (that newer turn is live). A no-op when no entry exists —
    /// the common non-re-adopted terminal commit — so the watcher terminal-commit
    /// path can call it unconditionally.
    pub(in crate::services::discord) fn mark_readopted_mailbox_owner_finished(
        &self,
        provider: &ProviderKind,
        channel_id: u64,
        owner_user_id: u64,
        active_user_message_id: u64,
    ) {
        if let Some(mut entry) = self
            .readopted_mailbox_ledger
            .entries
            .get_mut(&(provider.clone(), channel_id))
            && entry.owner_user_id == owner_user_id
            && entry.active_user_message_id == active_user_message_id
            && entry.turn_pin.is_none()
        {
            entry.finished = true;
        }
    }

    pub(in crate::services::discord) fn mark_readopted_mailbox_owner_finished_for_episode(
        &self,
        provider: &ProviderKind,
        channel_id: u64,
        owner_user_id: u64,
        active_user_message_id: u64,
        state: &InflightTurnState,
    ) {
        let turn_pin = ReadoptedMailboxTurnPin::from_state(state);
        if let Some(mut entry) = self
            .readopted_mailbox_ledger
            .entries
            .get_mut(&(provider.clone(), channel_id))
            && entry.owner_user_id == owner_user_id
            && entry.active_user_message_id == active_user_message_id
            && entry
                .turn_pin
                .as_ref()
                .is_none_or(|stored| stored == &turn_pin)
        {
            entry.finished = true;
        }
    }

    /// #4370: `true` iff the ledger records `(provider, channel_id)` as a mailbox
    /// re-adopted from inflight whose owner AND `active_user_message_id` BOTH
    /// still match the live mailbox AND whose turn is `finished` (its terminal
    /// delivery committed; #4370 R3-1). This is the row-ABSENT ("Path B") reclaim
    /// authority, and it now carries THREE independent live-turn-theft guards:
    ///   1. a live successor turn owns a different `active_user_message_id`, so it
    ///      can never match the exact-id requirement;
    ///   2. a re-adopted turn that is still LIVE (terminal delivery not committed)
    ///      is `finished == false`, so absence + exact-id alone cannot steal it;
    ///   3. the `OwnerInflightAbsent` reason an absent row yields still enforces
    ///      the `>= 120s` age gate on top (defense-in-depth, R3-4).
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
                    && entry.finished
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

    #[cfg(test)]
    pub(in crate::services::discord) fn readopted_mailbox_turn_pin_for_test(
        &self,
        provider: &ProviderKind,
        channel_id: u64,
    ) -> Option<ReadoptedMailboxTurnPin> {
        self.readopted_mailbox_ledger
            .entries
            .get(&(provider.clone(), channel_id))
            .and_then(|entry| entry.turn_pin.clone())
    }
}
