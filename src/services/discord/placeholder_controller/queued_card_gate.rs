//! #5035 — the single enforcement point for destroying a channel's queued
//! `📬 메시지 대기 중` placeholder card.
//!
//! One card stands for possibly several queue entries
//! (`reuse_any_queued_placeholder_for_channel` moves ownership to the newest
//! arrival, so earlier waiters hold no mapping), so a site that destroyed the
//! card when *its* entry left destroyed the card of users still waiting.
//!
//! # Contract G
//!
//! A destructive action on channel `C`'s queued card `X` — Discord `DELETE`,
//! an `EDIT` to a queue-exit body, `PlaceholderController::detach_by_message`,
//! or parking the card for deferred deletion — is permitted only when both
//! predicates hold on a mailbox snapshot read while holding `C`'s
//! `queued_placeholders_persist_lock`:
//!
//! * **Q** = the **whole** `intervention_queue` of that snapshot,
//!   oldest→newest; `Q` is not filtered by the departing hint.
//! * **ids(I)** = `{I.message_id} ∪ I.source_message_ids` (the union
//!   `queued_message_ids` uses); **cards(I)** =
//!   `{ queued_placeholders[(C, k)] : k ∈ ids(I) }`.
//! * **(G1)** no `I ∈ Q` has `X ∈ cards(I)`.
//! * **(G2)** every `I ∈ Q` has `cards(I) ≠ ∅`.
//!
//! `G1` false ⇒ nothing changes, [`QueuedCardDisposition::Preserved`].
//! `G1 ∧ ¬G2` ⇒ `X` is re-keyed onto one cardless `I ∈ Q`, `Preserved`.
//! Only `G1 ∧ G2` yields [`QueuedCardDisposition::Released`].
//!
//! # The departing hint `D`
//!
//! `departing` is whatever the site knows about the entries that are leaving.
//! **Neither predicate reads it**; it only orders re-keying candidates. A site
//! that supplies part of `D` (A3/A6/A8) or none (A7) therefore cannot change a
//! verdict. The gate's error direction is always over-preservation: a card may
//! survive that could have been deleted, or be re-keyed onto an entry that is
//! itself leaving (an orphan mapping, which
//! `reuse_any_queued_placeholder_for_channel` can still pick up). Deleting a
//! card a live entry owns is not reachable through the gate.
//!
//! # What is and is not enforced — by device
//!
//! * **Token (type system, no bypass for CONSTRUCTION).**
//!   [`QueuedCardTeardown`]'s fields are private to this module, so no other
//!   module — including the parent `placeholder_controller` — can construct
//!   one: calling a `teardown_*` helper without a gate verdict does not
//!   compile. What the token does NOT constrain is reuse of a verdict already
//!   granted: its `channel_id()` / `card()` accessors are
//!   `pub(in crate::services::discord)`, so anywhere in the `discord` module a
//!   holder can read the ids out and act on them by other means. The gate
//!   forbids destruction WITHOUT an issued teardown; it does not police an
//!   issued one. (The visibility is already the narrowest that compiles: the
//!   sole production consumer is `gateway.rs`'s
//!   `emit_relay_delete_result(.., placeholder_msg_id, ..)` observability call,
//!   in a different subtree from this module.)
//! * **Visibility (narrows, does not enforce).** `detach_by_message` moved from
//!   `pub(super)` to `pub(in …::placeholder_controller)`; that shrinks its
//!   callable set from all of `crate::services::discord` to this subtree. It
//!   does not make the gate the only caller.
//! * **Ratchet (asserts over an enumerated surface only).** Raw `serenity` and
//!   raw `http::*` calls cannot be restricted by visibility at all
//!   (`http::delete_channel_message` / `http::edit_channel_message` are
//!   `pub(in crate::services::discord)`), so the only defence is the
//!   source-text ratchet in this module's tests. It asserts over three
//!   enumerated axes and is blind outside each of them:
//!   - **files** — a fixed list; a destructive call in a NEW file passes. This
//!     has been demonstrated, not merely reasoned about: a scratch
//!     `queued_card_gate/bypass_test.rs` calling raw `http.delete_message()`
//!     passed the ratchet (rc=0).
//!   - **regions** — only the marked spans of the listed files; a destructive
//!     call elsewhere in a listed file passes. Two such spans are excluded on
//!     purpose (`QueueExitPendingPlaceholderDeleter::delete` and
//!     `SerenityStalePlaceholderDeleter::delete`), because those raw calls are
//!     the HTTP seam the token routes THROUGH.
//!   - **spellings** — only the substrings in `RAW_DISCORD_MESSAGE_OPS`,
//!     enumerated from the tree rather than from what the pre-#5035 code
//!     called. A newly introduced alias or wrapper name is not covered until it
//!     is added to that list.
//!   Within those three axes the assertion is real and independent: a listed
//!   region must both route through the gate AND hold no enumerated raw call,
//!   so a region carrying a gate call plus a raw call fails.
//!
//! Coordinates in this module are relative to `origin/main` = `ac0c8feba`; the
//! full non-guarantee list lives in the #5035 design note (§6.2).

use std::collections::HashSet;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::turn_orchestrator::{Intervention, QueueExitKind};

use super::super::SharedData;
use super::super::runtime_bootstrap::StalePlaceholderDeleter;

/// Outcome of [`release_or_rekey`] / [`release_or_rekey_locked`].
pub(in crate::services::discord) enum QueuedCardDisposition {
    /// The card belongs to (or was just handed to) a live queue entry. The
    /// caller must perform no Discord action on it. `owner` is the
    /// `queued_placeholders` key that now points at the card.
    Preserved { owner: MessageId },
    /// Contract G holds; destruction is permitted through the token.
    Released(QueuedCardTeardown),
}

/// Permission token for destroying one queued card. Its fields are private to
/// this module, so it cannot be constructed anywhere else — not even by the
/// parent `placeholder_controller` module.
#[must_use]
pub(in crate::services::discord) struct QueuedCardTeardown {
    channel_id: ChannelId,
    card: MessageId,
}

impl QueuedCardTeardown {
    pub(in crate::services::discord) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub(in crate::services::discord) fn card(&self) -> MessageId {
        self.card
    }
}

fn intervention_ids(item: &Intervention) -> impl Iterator<Item = MessageId> + '_ {
    std::iter::once(item.message_id).chain(item.source_message_ids.iter().copied())
}

/// `cards(I) ≠ ∅` — does any id of `I` own a queued card on this channel?
fn holds_any_card(shared: &SharedData, channel_id: ChannelId, item: &Intervention) -> bool {
    intervention_ids(item).any(|id| {
        shared
            .queued
            .queued_placeholders
            .contains_key(&(channel_id, id))
    })
}

/// Evaluate contract G for `card` on `channel_id`, assuming the caller already
/// holds that channel's `queued_placeholders_persist_lock`.
///
/// `_persist_guard` proves at the type level only that *some* tokio mutex guard
/// is held; the `debug_assert!` below is what checks it is this channel's
/// persist lock, and it is compiled out of release builds.
pub(in crate::services::discord) async fn release_or_rekey_locked(
    shared: &SharedData,
    channel_id: ChannelId,
    card: MessageId,
    departing: &[MessageId],
    _persist_guard: &tokio::sync::MutexGuard<'_, ()>,
) -> QueuedCardDisposition {
    debug_assert!(
        std::ptr::eq(
            Arc::as_ref(&shared.queued_placeholders_persist_lock(channel_id)),
            tokio::sync::MutexGuard::mutex(_persist_guard),
        ),
        "release_or_rekey_locked: guard must be the persist lock of `channel_id`",
    );
    let snapshot = super::super::mailbox_snapshot(shared, channel_id).await;
    // Collected once: both predicates borrow the same slice, and the
    // oldest→newest order is what the candidate preference below relies on.
    let queued: Vec<&Intervention> = snapshot.intervention_queue.iter().collect();

    // G1 — computed directly over Q. No inference from "no candidates".
    for item in &queued {
        for key in intervention_ids(item) {
            if shared
                .queued
                .queued_placeholders
                .get(&(channel_id, key))
                .map(|entry| *entry)
                == Some(card)
            {
                return QueuedCardDisposition::Preserved { owner: key };
            }
        }
    }

    // G2 — computed directly over Q.
    let candidates: Vec<&Intervention> = queued
        .iter()
        .copied()
        .filter(|item| !holds_any_card(shared, channel_id, item))
        .collect();
    let Some(newest_candidate) = candidates.last().copied() else {
        return QueuedCardDisposition::Released(QueuedCardTeardown { channel_id, card });
    };

    // Verdict is already decided; `departing` only picks the recipient. The
    // producer hands cards to the newest arrival and the dispatcher consumes
    // oldest-first, so newest costs the fewest further gate round-trips —
    // an argument about hops, not a safety requirement. Candidates named in
    // `departing` are deprioritised but never excluded: excluding them would
    // turn an over-estimated hint back into a destructive verdict.
    let departing: HashSet<MessageId> = departing.iter().copied().collect();
    let pick = candidates
        .iter()
        .rev()
        .copied()
        .find(|item| !intervention_ids(item).any(|id| departing.contains(&id)))
        .unwrap_or(newest_candidate);
    shared.insert_queued_placeholder_locked(channel_id, pick.message_id, card);
    QueuedCardDisposition::Preserved {
        owner: pick.message_id,
    }
}

/// Lock-acquiring variant of [`release_or_rekey_locked`].
pub(in crate::services::discord) async fn release_or_rekey(
    shared: &SharedData,
    channel_id: ChannelId,
    card: MessageId,
    departing: &[MessageId],
) -> QueuedCardDisposition {
    let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock().await;
    release_or_rekey_locked(shared, channel_id, card, departing, &persist_guard).await
}

fn detach(shared: &SharedData, teardown: &QueuedCardTeardown) {
    shared
        .ui
        .placeholder_controller
        .detach_by_message(teardown.channel_id, teardown.card);
}

/// Delete the card and drop its controller row.
pub(in crate::services::discord) async fn teardown_delete(
    http: &Arc<serenity::http::Http>,
    shared: &SharedData,
    teardown: QueuedCardTeardown,
) -> serenity::Result<()> {
    let result =
        super::super::http::delete_channel_message(http, teardown.channel_id, teardown.card).await;
    detach(shared, &teardown);
    result
}

/// Rewrite the card to its queue-exit body, falling back to delete, then drop
/// the controller row.
pub(in crate::services::discord) async fn teardown_exit_body(
    http: &Arc<serenity::http::Http>,
    shared: &SharedData,
    teardown: QueuedCardTeardown,
    kind: QueueExitKind,
) {
    let body = super::super::queue_exit_card_body(kind);
    if super::super::http::edit_channel_message(http, teardown.channel_id, teardown.card, body)
        .await
        .is_err()
    {
        let _ =
            super::super::http::delete_channel_message(http, teardown.channel_id, teardown.card)
                .await;
    }
    detach(shared, &teardown);
}

/// Delete through the `StalePlaceholderDeleter` seam (bootstrap + deferred
/// queue-exit drains).
pub(in crate::services::discord) async fn teardown_via_deleter(
    shared: &SharedData,
    deleter: &dyn StalePlaceholderDeleter,
    teardown: QueuedCardTeardown,
) -> Result<(), String> {
    let result = deleter.delete(teardown.channel_id, teardown.card).await;
    detach(shared, &teardown);
    result
}

/// Consume the token when no Discord HTTP source exists yet: the controller row
/// is dropped and the card id is handed back so the caller can park it in
/// `queue_exit_placeholder_clears`. That deferred drain re-enters the gate,
/// because the verdict taken here goes stale while it waits.
pub(in crate::services::discord) fn teardown_defer(
    shared: &SharedData,
    teardown: QueuedCardTeardown,
) -> MessageId {
    detach(shared, &teardown);
    teardown.card
}

#[cfg(test)]
mod tests;
