//! #5035 — the single enforcement point for destroying a channel's queued
//! placeholder card: one card can stand for several queue entries (newest
//! arrival owns it), so destroying it on one entry's exit can strand others.
//!
//! # Contract G
//!
//! Destroying channel `C`'s queued card `X` is permitted only when, under
//! `C`'s `queued_placeholders_persist_lock`: **(G1)** no queue entry still
//! maps to `X`, and **(G2)** every queue entry has some card. `G1` false, or
//! `G1 ∧ ¬G2` (re-key `X` onto a cardless entry) → preserved; only
//! `G1 ∧ G2` → [`QueuedCardDisposition::Released`].
//!
//! `departing` never affects the verdict, only which entry a re-key prefers —
//! error direction is always over-preservation, never deleting a live card.
//!
//! Enforcement is layered, each partial: [`QueuedCardTeardown`]'s private
//! fields block construction outside this module (not reuse of an issued
//! token); visibility narrows callers, not raw `serenity`/`http::*` calls; a
//! source-text ratchet in this module's tests catches raw Discord ops only
//! across an enumerated files/regions/spellings set (#5035 design note §6.2).

use std::collections::HashSet;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::turn_orchestrator::Intervention;

use super::super::http::{delete_channel_message, edit_channel_message};
use super::super::runtime_bootstrap::StalePlaceholderDeleter;
use super::super::{QueueExitVisibleCard, SharedData};

/// Outcome of [`release_or_rekey`] / [`release_or_rekey_locked`].
pub(in crate::services::discord) enum QueuedCardDisposition {
    /// Belongs to (or was just handed to) a live entry; `owner` is its key. No
    /// Discord action on the card is permitted.
    Preserved { owner: MessageId },
    /// Contract G holds; destruction is permitted through the token.
    Released(QueuedCardTeardown),
}

/// Permission token for destroying one queued card; private fields, so it
/// cannot be constructed outside this module.
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

/// Evaluate contract G for `card` on `channel_id`; caller must already hold
/// `channel_id`'s `queued_placeholders_persist_lock` (checked by the
/// debug-only assert below, since `_persist_guard` only proves *some* guard
/// is held at the type level).
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

    // Verdict already decided; `departing` only picks the recipient (newest
    // costs fewest gate round-trips) and only deprioritises, never excludes —
    // excluding would turn an over-estimated hint into a destructive verdict.
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
    let result = delete_channel_message(http, teardown.channel_id, teardown.card).await;
    detach(shared, &teardown);
    result
}

#[derive(Debug)]
pub(in crate::services::discord) enum QueueExitTeardownOutcome {
    Edited,
    Deleted {
        edit_error: String,
    },
    Parked {
        edit_error: String,
        delete_error: String,
    },
}

fn is_unknown_message(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response))
        if response.status_code.as_u16() == 404 && response.error.code == 10008)
}

/// Timer-only HTTP seam; the common pending drain re-gates before calling it.
/// Unlike the boot deleter, an already-gone message counts as cleared.
pub(in crate::services::discord) struct QueueExitRetryDeleter {
    pub(in crate::services::discord) http: Arc<serenity::http::Http>,
}

impl StalePlaceholderDeleter for QueueExitRetryDeleter {
    fn delete<'a>(
        &'a self,
        channel_id: ChannelId,
        placeholder_msg_id: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            match delete_channel_message(&self.http, channel_id, placeholder_msg_id).await {
                Ok(()) => Ok(()),
                Err(error) if is_unknown_message(&error) => Ok(()),
                Err(error) => Err(format!("{error:?}")),
            }
        })
    }
}

/// Rewrite the exit body or delete; persist double failures for a later re-gated drain.
pub(in crate::services::discord) async fn teardown_exit_body(
    http: &Arc<serenity::http::Http>,
    shared: &SharedData,
    teardown: QueuedCardTeardown,
    card: QueueExitVisibleCard,
) -> QueueExitTeardownOutcome {
    debug_assert_eq!(card.placeholder_msg_id, teardown.card());
    let (channel, message) = (teardown.channel_id, teardown.card);
    let body = super::super::queue_exit_card_body(card.kind);
    let outcome = match edit_channel_message(http, channel, message, body).await {
        Ok(_) => QueueExitTeardownOutcome::Edited,
        Err(edit_error) => {
            let edit_error = format!("{edit_error:?}");
            let result = delete_channel_message(http, channel, message).await;
            if let Some(error) = result.err().filter(|error| !is_unknown_message(error)) {
                let delete_error = format!("{error:?}");
                tracing::warn!(channel_id = channel.get(), placeholder_msg_id = message.get(),
                    user_msg_id = card.user_msg_id.get(), %edit_error, %delete_error,
                    "queue_exit: edit and delete failed; parking for retry");
                shared
                    .add_pending_queue_exit_placeholder_clear_one(
                        channel,
                        card.user_msg_id,
                        message,
                    )
                    .await;
                QueueExitTeardownOutcome::Parked {
                    edit_error,
                    delete_error,
                }
            } else {
                tracing::debug!(%edit_error, "queue_exit: edit failed; card deleted or already gone");
                QueueExitTeardownOutcome::Deleted { edit_error }
            }
        }
    };
    detach(shared, &teardown);
    outcome
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

/// Consume the token with no Discord HTTP source yet: drops the controller
/// row and hands back the card id to park in `queue_exit_placeholder_clears`,
/// whose deferred drain re-enters the gate (this verdict goes stale meanwhile).
pub(in crate::services::discord) fn teardown_defer(
    shared: &SharedData,
    teardown: QueuedCardTeardown,
) -> MessageId {
    detach(shared, &teardown);
    teardown.card
}

#[cfg(test)]
mod tests;
