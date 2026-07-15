use super::*;

pub(super) async fn allows(
    shared: &SharedData,
    target: TurnViewTarget,
    current: Option<TurnViewState>,
    source: &'static str,
) -> bool {
    if target.kind != TurnViewTargetKind::IntakeUserMessage {
        return false;
    }
    let queued = super::super::mailbox_snapshot(shared, target.channel_id)
        .await
        .intervention_queue
        .iter()
        .any(|intervention| {
            intervention.message_id == target.message_id
                || intervention.source_message_ids.contains(&target.message_id)
        });
    if queued {
        tracing::info!(
            channel_id = target.channel_id.get(),
            message = target.message_id.get(),
            target_kind = ?target.kind,
            source,
            current_state = ?current,
            "turn view queue marker repaired over stale view state because mailbox still owns the source message"
        );
    }
    queued
}
