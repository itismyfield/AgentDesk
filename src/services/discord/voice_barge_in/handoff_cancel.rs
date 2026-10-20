//! Handoff tombstones follow confirmed native stops; other voice cancellations stay unchanged.
use super::*;

pub(super) async fn record_and_cancel_voice_handoff_if_observed(
    shared: &Arc<SharedData>,
    source_channel_id: ChannelId,
    target_channel_id: ChannelId,
    turn_id: &str,
    handoff_message_id: Option<MessageId>,
    observation: VoiceHandoffCancelObservation,
) -> crate::services::turn_orchestrator::CancelActiveTurnResult {
    let native =
        super::super::mailbox_stop::active_native(shared, target_channel_id, handoff_message_id)
            .await;
    if let Some(handoff_id) = handoff_message_id.filter(|_| !native) {
        super::super::record_voice_handoff_cancel_tombstone(
            handoff_id,
            observation.cancel_reason.clone(),
        );
    }

    let result = if let Some(handoff_id) = handoff_message_id {
        super::super::mailbox_cancel_active_turn_if_handoff_user_message_with_reason(
            shared,
            target_channel_id,
            handoff_id,
            &observation.cancel_reason,
        )
        .await
    } else if observation.local_cancel {
        super::super::mailbox_cancel_active_turn_with_reason(
            shared,
            target_channel_id,
            &observation.cancel_reason,
        )
        .await
    } else {
        // Tombstone-only observation without a message id to identity-guard on:
        // do not issue a blind cancel; just suppress the foreground ack.
        crate::services::turn_orchestrator::CancelActiveTurnResult {
            token: None,
            already_stopping: false,
        }
    };

    if native
        && result.token.is_some()
        && let Some(id) = handoff_message_id
    {
        super::super::record_voice_handoff_cancel_tombstone(id, observation.cancel_reason.clone());
    }
    tracing::info!(
        source_channel_id = source_channel_id.get(),
        target_channel_id = target_channel_id.get(),
        turn_id,
        target_cancelled = result.token.is_some(),
        already_stopping = result.already_stopping,
        cancel_source = %observation.cancel_reason,
        observed_via_tombstone = observation.observed_via_tombstone,
        handoff_message_id = ?handoff_message_id.map(|m| m.get()),
        "voice background handoff cancellation observed"
    );

    result
}
