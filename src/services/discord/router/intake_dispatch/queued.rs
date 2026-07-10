use poise::serenity_prelude as serenity;

use super::{
    IntakeAdmission, IntakeOrigin, IntakeSubmission, LocalAdmissionPermit, admit_text_intake,
    finish_admitted_local,
};
use crate::services::discord::Intervention;
use crate::services::provider::ProviderKind;

pub(crate) enum QueuedAdmissionDisposition {
    Admitted(AdmittedQueuedIntake),
    Deferred,
}

pub(crate) struct AdmittedQueuedIntake {
    submission: IntakeSubmission,
    local_permit: Option<LocalAdmissionPermit>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn admit_queued_intake(
    deps: &super::super::message_handler::IntakeDeps<'_>,
    provider: ProviderKind,
    channel_id: serenity::ChannelId,
    intervention: &Intervention,
    request_owner: serenity::UserId,
    request_owner_name: String,
    defer_watcher_resume: bool,
    wait_for_completion: bool,
    backstop_reason: &'static str,
) -> QueuedAdmissionDisposition {
    let submission = IntakeSubmission {
        provider,
        request: super::super::message_handler::IntakeRequest {
            channel_id,
            user_msg_id: intervention.message_id,
            request_owner,
            request_owner_name,
            user_text: intervention.text.clone(),
            reply_to_user_message: true,
            defer_watcher_resume,
            wait_for_completion,
            merge_consecutive: intervention.merge_consecutive,
            reply_context: intervention.reply_context.clone(),
            has_reply_boundary: intervention.has_reply_boundary,
            dm_hint: None,
            turn_kind: super::super::TurnKind::Foreground,
        },
        origin: IntakeOrigin::QueuedDrain,
        has_nonportable_uploads: !intervention.pending_uploads.is_empty(),
        preloaded_uploads: intervention.pending_uploads.clone(),
        voice_announcement: None,
    };
    let admission = admit_text_intake(deps, &submission).await;
    let local_permit = match admission {
        IntakeAdmission::Local(permit) => Some(permit),
        IntakeAdmission::Forwarded { .. } | IntakeAdmission::SkippedDuplicate => None,
        IntakeAdmission::DeferredOpenRoute { .. } | IntakeAdmission::Blocked { .. } => {
            // `requeue_front` also consumes the actor's pending-dispatch
            // reservation, matching the existing hosted-TUI pre-drain defer.
            // Both queue entrypoints call this before marker/card teardown.
            super::super::super::mailbox_requeue_intervention_front(
                deps.shared,
                &submission.provider,
                channel_id,
                intervention.clone(),
            )
            .await;
            super::super::super::arm_slow_idle_queue_backstop_if_queue_nonempty(
                deps.shared,
                &submission.provider,
                channel_id,
                backstop_reason,
            )
            .await;
            return QueuedAdmissionDisposition::Deferred;
        }
    };
    QueuedAdmissionDisposition::Admitted(AdmittedQueuedIntake {
        submission,
        local_permit,
    })
}

pub(crate) async fn finish_admitted_queued_intake(
    deps: &super::super::message_handler::IntakeDeps<'_>,
    admitted: AdmittedQueuedIntake,
    intervention: &Intervention,
) -> Result<(), super::super::super::Error> {
    let Some(permit) = admitted.local_permit else {
        return Ok(());
    };
    if let Some(announcement) = intervention.voice_announcement.as_ref() {
        crate::voice::announce_meta::global_store()
            .insert_accepted_replay(intervention.message_id, announcement.clone());
    }
    finish_admitted_local(deps, permit, admitted.submission).await
}
