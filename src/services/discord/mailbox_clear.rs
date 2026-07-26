use poise::serenity_prelude::ChannelId;

use super::{SharedData, apply_queue_exit_feedback, queue_persistence_context};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ClearChannelResult;

fn clear_result_completes_recovery_latch(result: &ClearChannelResult) -> bool {
    !result.refused_resume_transition && result.persistence_error.is_none()
}

pub(super) async fn mailbox_clear_channel(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> ClearChannelResult {
    let result = shared
        .mailbox(channel_id)
        .clear(queue_persistence_context(shared, provider, channel_id))
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    complete_mailbox_clear_side_effects(shared, channel_id, &result);
    result
}

fn complete_mailbox_clear_side_effects(
    shared: &SharedData,
    channel_id: ChannelId,
    result: &ClearChannelResult,
) {
    // A refused resume reservation or failed durable queue clear still owns the
    // recovery latch. Only committed teardown frees recovery subscribers (#2443).
    if clear_result_completes_recovery_latch(result) {
        shared.mailboxes.recovery_done(channel_id).mark_done();
    }
}

pub(crate) async fn mailbox_prepare_channel_clear(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    transition_id: uuid::Uuid,
) -> crate::services::turn_orchestrator::PrepareChannelClearResult {
    shared
        .mailbox(channel_id)
        .prepare_clear(
            transition_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await
}

pub(crate) async fn mailbox_commit_prepared_channel_clear(
    shared: &SharedData,
    channel_id: ChannelId,
    key: crate::services::turn_orchestrator::ResumeTransitionKey,
) -> ClearChannelResult {
    let result = shared.mailbox(channel_id).commit_prepared_clear(key).await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    complete_mailbox_clear_side_effects(shared, channel_id, &result);
    result
}

pub(crate) async fn mailbox_abort_prepared_channel_clear(
    shared: &SharedData,
    channel_id: ChannelId,
    key: crate::services::turn_orchestrator::ResumeTransitionKey,
) -> crate::services::turn_orchestrator::AbortPreparedClearResult {
    shared.mailbox(channel_id).abort_prepared_clear(key).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(
        refused_resume_transition: bool,
        persistence_error: Option<&str>,
    ) -> ClearChannelResult {
        ClearChannelResult {
            removed_token: None,
            queue_exit_events: Vec::new(),
            persistence_error: persistence_error.map(str::to_string),
            refused_resume_transition,
        }
    }

    #[test]
    fn recovery_latch_requires_committed_clear() {
        assert!(clear_result_completes_recovery_latch(&result(false, None)));
        assert!(!clear_result_completes_recovery_latch(&result(true, None)));
        assert!(!clear_result_completes_recovery_latch(&result(
            false,
            Some("durable queue write failed")
        )));
    }
}
