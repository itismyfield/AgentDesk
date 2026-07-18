use serenity::model::id::ChannelId;
use tokio::sync::broadcast;

use crate::services::turn_orchestrator::FinishTurnResult;

use super::SharedData;

/// Small bounded in-process bus for canonical turn-finalize completion edges.
///
/// Capacity 64 is deliberately modest: normal traffic is consumed immediately by
/// one idle-queue listener, while a burst that exceeds the buffer trips
/// `RecvError::Lagged`; the listener treats that as lost events and reconciles
/// every queued channel from mailbox snapshots.
pub(in crate::services::discord) const TURN_COMPLETION_EVENT_BUS_CAPACITY: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TurnCompletionEvent {
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) turn_id: Option<u64>,
}

impl TurnCompletionEvent {
    pub(in crate::services::discord) fn new(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            turn_id: None,
        }
    }

    pub(in crate::services::discord) fn for_turn(channel_id: ChannelId, turn_id: u64) -> Self {
        Self {
            channel_id,
            turn_id: Some(turn_id),
        }
    }
}

#[cfg(test)]
pub(in crate::services::discord) fn turn_completion_event_bus()
-> broadcast::Sender<TurnCompletionEvent> {
    broadcast::channel(TURN_COMPLETION_EVENT_BUS_CAPACITY).0
}

pub(in crate::services::discord) fn publish_turn_completion_event(
    shared: &SharedData,
    event: TurnCompletionEvent,
) {
    let channel_id = event.channel_id.get();
    match shared.turn_completion_events.send(event) {
        Ok(subscribers) => {
            tracing::debug!(
                target: "agentdesk::discord::turn_completion_events",
                channel_id,
                subscribers,
                "published turn completion event"
            );
        }
        Err(_no_receivers) => {
            tracing::debug!(
                target: "agentdesk::discord::turn_completion_events",
                channel_id,
                "turn completion event had no subscribers"
            );
        }
    }
}

pub(in crate::services::discord) fn subscribe_turn_completion_events(
    shared: &SharedData,
) -> broadcast::Receiver<TurnCompletionEvent> {
    shared.turn_completion_events.subscribe()
}

pub(in crate::services::discord) fn publish_mailbox_release_completion_event(
    shared: &SharedData,
    channel_id: ChannelId,
    turn_id: Option<u64>,
    finish: &FinishTurnResult,
) {
    if finish.removed_token.is_some() {
        let event = turn_id.map_or_else(
            || TurnCompletionEvent::new(channel_id),
            |turn_id| TurnCompletionEvent::for_turn(channel_id, turn_id),
        );
        publish_turn_completion_event(shared, event);
    }
}

pub(in crate::services::discord) fn warn_unresolvable_hard_stop_pending_backlog(
    channel_id: ChannelId,
    has_pending: bool,
    source: &'static str,
) {
    tracing::warn!(
        target: "agentdesk::discord::idle_queue_backstop",
        channel_id = channel_id.get(),
        has_pending,
        source,
        "raw hard_stop fallback could not resolve the owning runtime; pending mailbox backlog may be stranded without a completion event"
    );
}
