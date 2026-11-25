//! An idle owner puts ⏳ on a person's input at acceptance; its handoff, a queue exit or a boot
//! disposition that removed it later rolls it back to 📬 or clears it unless a start took it.

use std::future::Future;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::discord::turn_view_reconciler::{
    AcceptSettle, TurnViewIdentity, TurnViewTarget,
};
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

/// Puts the early ⏳ on the input the owner just opened its idle head with. The record lands only
/// after the reaction, so a cancel during that call is caught here by reading custody again.
pub(crate) async fn note(shared: &SharedData, channel: ChannelId, message: MessageId) -> bool {
    let target = TurnViewTarget::intake_user_message(channel, message);
    let noted = shared
        .turn_view_reconciler
        .note_accept_pending(
            shared,
            target,
            TurnViewIdentity::IntakeShared,
            "owner_accept_pending",
        )
        .await
        .is_some();
    if noted {
        clear_if_gone(shared, channel, message, "owner_accept_pending_left").await;
    }
    noted
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

/// Settles `message`'s untaken early ⏳ after its handoff: 📬 while held, else cleared.
pub(crate) async fn settle(
    shared: &SharedData,
    channel: ChannelId,
    message: MessageId,
) -> AcceptSettle {
    let target = TurnViewTarget::intake_user_message(channel, message);
    let handle = shared.mailbox(channel);
    let held = owner_holds(&handle, message.get());
    let source = "owner_handoff_accept_pending_settle";
    let settled = shared
        .turn_view_reconciler
        .settle_accept_pending(shared, target, held, source)
        .await;
    warn_kept(channel, message, settled, source);
    settled
}

/// Clears the untaken early ⏳ of each id a queue exit or a boot disposition removed.
pub(crate) async fn clear_left(
    shared: &SharedData,
    channel: ChannelId,
    ids: impl IntoIterator<Item = MessageId>,
    source: &'static str,
) {
    for message in ids {
        clear_if_gone(shared, channel, message, source).await;
    }
}

/// At boot, clears every persisted early ⏳ of `channel` whose input the owner no longer holds:
/// dropped by a boot disposition, or gone before a restart cut its settle short.
pub(crate) async fn sweep_boot(shared: &SharedData, channel: ChannelId) {
    let early = shared
        .turn_view_reconciler
        .persisted_accept_pending(shared, channel);
    clear_left(shared, channel, early, "boot_accept_pending_sweep").await;
}

async fn clear_if_gone(
    shared: &SharedData,
    channel: ChannelId,
    message: MessageId,
    source: &'static str,
) {
    if message.get() == 0 || owner_holds(&shared.mailbox(channel), message.get()).await {
        return;
    }
    let target = TurnViewTarget::intake_user_message(channel, message);
    let settled = shared
        .turn_view_reconciler
        .settle_accept_pending(shared, target, std::future::ready(false), source)
        .await;
    warn_kept(channel, message, settled, source);
}

fn warn_kept(channel: ChannelId, message: MessageId, settled: AcceptSettle, source: &str) {
    if settled == AcceptSettle::Kept {
        tracing::warn!(
            channel_id = channel.get(),
            message = message.get(),
            source,
            "early ⏳ settle failed; the record stays for the next settle"
        );
    }
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
