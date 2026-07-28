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
    let retry_identity = super::super::super::busy_followup_retry_store::resolve_identity(
        &provider,
        channel_id.get(),
        intervention.message_id.get(),
        &intervention.source_message_ids,
    );
    let submission = IntakeSubmission {
        provider,
        request: super::super::message_handler::IntakeRequest {
            channel_id,
            user_msg_id: intervention.message_id,
            busy_followup_retry_user_msg_id: serenity::MessageId::new(retry_identity.user_msg_id),
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
        attachments: Vec::new(),
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
            let _ = super::super::super::mailbox_abandon_pending_dispatch(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_merge_persistence_restores_canonical_retry_lineage_4888() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let provider = ProviderKind::Claude;
        let channel_id = serenity::ChannelId::new(4_888_201);
        let source_a = serenity::MessageId::new(4_888_202);
        let merged_head_b = serenity::MessageId::new(4_888_203);
        let notice = serenity::MessageId::new(4_888_204);
        let token_hash = "busy-retry-lineage-4888";

        crate::services::discord::busy_followup_retry_store::bind_notice_if_absent(
            &provider,
            channel_id.get(),
            source_a.get(),
            notice.get(),
        )
        .expect("bind source retry notice");
        for _ in 0..crate::services::discord::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT {
            crate::services::discord::busy_followup_retry_store::record_busy_retry(
                &provider,
                channel_id.get(),
                source_a.get(),
                notice.get(),
            )
            .expect("record source retry");
        }

        let intervention = |message_id: serenity::MessageId, text: &str| Intervention {
            author_id: serenity::UserId::new(4_888_205),
            author_is_bot: false,
            message_id,
            queued_generation: 7,
            source_message_ids: vec![message_id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: true,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let mut queue = Vec::new();
        assert!(
            crate::services::turn_orchestrator::enqueue_intervention(
                &mut queue,
                intervention(source_a, "source A"),
                None,
            )
            .enqueued
        );
        let merged = crate::services::turn_orchestrator::enqueue_intervention(
            &mut queue,
            intervention(merged_head_b, "head B"),
            None,
        );
        assert!(merged.enqueued && merged.merged);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].message_id, merged_head_b);
        assert_eq!(queue[0].source_message_ids, vec![source_a, merged_head_b]);

        crate::services::turn_orchestrator::save_channel_queue(
            &provider, token_hash, channel_id, &queue, None,
        )
        .expect("persist merged queue");
        let (restored, failures) =
            crate::services::turn_orchestrator::load_pending_queues(&provider, token_hash);
        assert!(failures.is_empty());
        let restored = &restored[&channel_id][0];
        assert_eq!(restored.message_id, merged_head_b);
        assert_eq!(restored.source_message_ids, vec![source_a, merged_head_b]);

        let retry_identity = crate::services::discord::busy_followup_retry_store::resolve_identity(
            &provider,
            channel_id.get(),
            restored.message_id.get(),
            &restored.source_message_ids,
        );
        assert_eq!(retry_identity.user_msg_id, source_a.get());
        let retry_state = retry_identity.state.expect("source A retry state");
        assert_eq!(retry_state.notice_message_id, notice.get());
        assert!(
            crate::services::discord::busy_followup_retry_store::state_is_capped(Some(retry_state)),
            "merged head B must inherit source A's aggregate retry cap"
        );
    }
}
