//! #8398: an idle owner puts ⏳ on a person's input as soon as it accepts it, before the start
//! is decided; the return of that input's owner handoff settles the ⏳ that no start took over.
//!
//! Every way a granted front can end without starting (busy pane, race loss, replay refusal or
//! deferral, blocked or writer-held admission, injection) returns through the one handoff, so
//! the settle there covers them all: back to 📬 while the owner still holds the original, cleared
//! once it left the queue. A start re-notes Pending over the early ⏳ with a fresh attempt, which
//! turns the attempt-scoped settle into a no-op.

use std::future::Future;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::discord::turn_view_reconciler::{TurnViewIdentity, TurnViewTarget};
use crate::services::discord::{Intervention, SharedData};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};

/// Text intake treats as a dispatch, never as a person's typed input.
const DISPATCH_PREFIX: &str = "DISPATCH:";

/// A person's real Discord message: no bot, background trigger, dispatch or internal id.
pub(crate) fn eligible(raw: &Intervention) -> bool {
    !raw.author_is_bot
        && !raw.text.starts_with(DISPATCH_PREFIX)
        && crate::services::discord::reaction_lifecycle::is_real_discord_message_id(raw.message_id)
        && !crate::services::discord::is_internal_message_id(raw.message_id)
}

/// No turn runs and the owner holds no input yet, so the input admitted next starts next.
pub(crate) async fn idle(handle: &ChannelMailboxHandle) -> bool {
    let empty = handle
        .v2_request(|slot| {
            Ok(slot.as_ref().is_some_and(|owner| {
                owner.snapshot.items.is_empty() && owner.snapshot.head_injection.is_none()
            }))
        })
        .await
        .unwrap_or(false);
    empty && matches!(handle.has_active_turn().await, Ok(false))
}

/// Puts the early ⏳ on an accepted input; a target that already has a turn view keeps it.
pub(crate) async fn note(shared: &SharedData, channel: ChannelId, message: MessageId) -> bool {
    let target = TurnViewTarget::intake_user_message(channel, message);
    shared
        .turn_view_reconciler
        .note_accept_pending(
            shared,
            target,
            TurnViewIdentity::IntakeShared,
            "owner_accept_pending",
        )
        .await
        .is_some()
}

/// Runs one owner handoff, then settles the early ⏳ of its front that no start took over.
pub(crate) async fn after_handoff<F>(
    runtime: Option<Arc<SharedData>>,
    channel: u64,
    message: u64,
    handoff: F,
) -> Result<(), String>
where
    F: Future<Output = Result<(), String>>,
{
    let result = handoff.await;
    if let Some(shared) = runtime.as_deref()
        && channel != 0
        && message != 0
    {
        settle(shared, ChannelId::new(channel), MessageId::new(message)).await;
    }
    result
}

/// Settles `message`'s untaken early ⏳; returns whether it rolled back or cleared one.
pub(crate) async fn settle(shared: &SharedData, channel: ChannelId, message: MessageId) -> bool {
    let target = TurnViewTarget::intake_user_message(channel, message);
    let handle = shared.mailbox(channel);
    shared
        .turn_view_reconciler
        .settle_accept_pending(
            shared,
            target,
            owner_holds(&handle, message.get()),
            "owner_handoff_accept_pending_settle",
        )
        .await
}

/// Whether the owner still holds `message`, queued or merged as a source, or in its head.
async fn owner_holds(handle: &ChannelMailboxHandle, message: u64) -> bool {
    handle
        .v2_request(move |slot| {
            let holds = |item: &PendingQueueItem| {
                item.message_id == message || item.source_message_ids.contains(&message)
            };
            Ok(slot.as_ref().is_some_and(|owner| {
                owner.snapshot.items.iter().any(holds)
                    || owner
                        .snapshot
                        .head_injection
                        .as_ref()
                        .is_some_and(|head| head.members().any(holds))
            }))
        })
        .await
        .unwrap_or(false)
}

#[cfg(all(test, unix))]
#[path = "early_pending_tests.rs"]
mod tests;
