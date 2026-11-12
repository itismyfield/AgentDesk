use super::*;
use crate::services::discord::queue_io;

#[derive(Debug)]
pub(in crate::services::discord::router) struct RaceLostReturn {
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
}

impl std::fmt::Display for RaceLostReturn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("queued start lost its claim; return through its lease owner")
    }
}

impl std::error::Error for RaceLostReturn {}

pub(in crate::services::discord::router) fn hand_back_race_lost(
    legacy_queued: bool,
    uploads: &mut crate::services::cluster::attachment_transfer::uploads::PendingUploads,
) -> Result<(), crate::services::discord::Error> {
    if legacy_queued {
        return Err(Box::new(RaceLostReturn {
            uploads: std::mem::take(uploads),
        }));
    }
    Ok(())
}

impl QueuedRestoreOwner {
    pub(super) async fn return_race_lost(
        mut self,
        deps: &super::super::super::message_handler::IntakeDeps<'_>,
        handoff: RaceLostReturn,
    ) -> Result<super::super::super::IntakeExecution, crate::services::discord::Error> {
        self.original.pending_uploads = handoff.uploads;
        let persistence = crate::services::discord::queue_dispatch::persistence_context(
            deps.shared,
            &self.provider,
            self.channel_id,
        );
        let outcome = deps
            .shared
            .mailbox(self.channel_id)
            .return_dequeued_head(self.original, persistence, self.dispatch_lease)
            .await;
        crate::services::discord::apply_queue_exit_feedback(
            deps.shared,
            self.channel_id,
            &outcome.queue_exit_events,
        )
        .await;
        if outcome.refusal_reason
            == Some(crate::services::turn_orchestrator::EnqueueRefusalReason::SupersededByClear)
        {
            return Ok(super::super::super::IntakeExecution::ConsumedProtected);
        }
        if !outcome.enqueued || outcome.persistence_error.is_some() {
            return Err(format!(
                "race-loss owner return failed: {:?} {:?}",
                outcome.refusal_reason, outcome.persistence_error
            )
            .into());
        }
        queue_io::schedule_race_loss_requeue_post_enqueue_idle_recheck(
            deps.shared.clone(),
            self.provider,
            self.channel_id,
        );
        Ok(super::super::super::IntakeExecution::Deferred)
    }
}
