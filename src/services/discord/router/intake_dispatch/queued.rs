use poise::serenity_prelude as serenity;

use super::{
    IntakeAdmission, IntakeOrigin, IntakeSubmission, LocalAdmissionPermit, admit_text_intake,
    finish_admitted_local,
};
use crate::services::discord::Intervention;
use crate::services::provider::ProviderKind;

#[allow(clippy::large_enum_variant)]
pub(crate) enum QueuedAdmissionDisposition {
    Admitted(AdmittedQueuedIntake),
    Deferred,
    RejectedNonPortableAttachment,
    RejectedRestore,
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
    dispatch_lease: Option<std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>>,
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
            preserve_on_cancel: intervention.preserve_on_cancel(),
        },
        origin: IntakeOrigin::QueuedDrain,
        preserve_on_cancel: intervention.preserve_on_cancel(),
        has_nonportable_uploads: !intervention.pending_uploads.is_empty(),
        preloaded_uploads: intervention.pending_uploads.clone(),
        voice_announcement: None,
    };
    let admission = admit_text_intake(deps, &submission).await;
    let local_permit = match admission {
        IntakeAdmission::Local(permit) => Some(permit),
        IntakeAdmission::Forwarded { .. } | IntakeAdmission::SkippedDuplicate => None,
        IntakeAdmission::Blocked {
            reason:
                reason @ (crate::services::cluster::intake_router_hook::IntakeBlockedReason::NonPortableAttachmentForeignOwner { .. }
                | crate::services::cluster::intake_router_hook::IntakeBlockedReason::NonPortableAttachmentRoutedTarget { .. }),
        } => {
            // A queued local-path upload can never become portable through
            // retry. Notify once and consume it instead of front-requeueing it
            // forever without user-visible recovery guidance.
            super::notice::notify_blocked_intake(deps, &submission, &reason).await;
            super::super::super::mailbox_abandon_pending_dispatch(
                deps.shared,
                &submission.provider,
                channel_id,
                intervention.message_id,
            )
            .await;
            return QueuedAdmissionDisposition::RejectedNonPortableAttachment;
        }
        IntakeAdmission::DeferredOpenRoute { .. } | IntakeAdmission::Blocked { .. } => {
            let Some(dispatch_lease) = dispatch_lease else {
                tracing::error!(
                    provider = submission.provider.as_str(),
                    channel_id = channel_id.get(),
                    "queued admission defer is missing its dispatch lease"
                );
                return QueuedAdmissionDisposition::RejectedRestore;
            };
            let restored = super::super::super::mailbox_restore_dequeued_head(
                deps.shared,
                &submission.provider,
                channel_id,
                intervention.clone(),
                dispatch_lease,
            )
            .await;
            if !restored.enqueued {
                tracing::error!(
                    provider = submission.provider.as_str(),
                    channel_id = channel_id.get(),
                    refusal_reason = restored
                        .refusal_reason
                        .map(|reason| reason.as_str())
                        .unwrap_or("none"),
                    persistence_error = restored.persistence_error.as_deref().unwrap_or("none"),
                    "queued admission defer rejected dequeued-head restore"
                );
                return QueuedAdmissionDisposition::RejectedRestore;
            }
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
