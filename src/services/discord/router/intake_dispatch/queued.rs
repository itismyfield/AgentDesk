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
    /// Every source already started under a held request; that request keeps the original.
    ConsumedToHold,
}

pub(crate) struct AdmittedQueuedIntake {
    submission: IntakeSubmission,
    local_permit: Option<LocalAdmissionPermit>,
    restore_owner: Option<QueuedRestoreOwner>,
    #[cfg(test)]
    ledger_admission: Option<IntakeAdmission>,
}
struct QueuedRestoreOwner {
    original: Intervention,
    provider: ProviderKind,
    channel_id: serenity::ChannelId,
    dispatch_lease: std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
}
impl QueuedRestoreOwner {
    async fn restore(
        self,
        deps: &super::super::message_handler::IntakeDeps<'_>,
    ) -> super::super::IntakeExecution {
        loop {
            let restored = super::super::super::mailbox_restore_dequeued_head(
                deps.shared,
                &self.provider,
                self.channel_id,
                self.original.clone(),
                self.dispatch_lease.clone(),
            )
            .await;
            if restored.enqueued && restored.persistence_error.is_none() {
                break;
            }
            tracing::error!(provider = self.provider.as_str(), channel_id = self.channel_id.get(),
                refusal_reason = ?restored.refusal_reason, persistence_error = ?restored.persistence_error,
                "queued busy owner retains payload until authorized durable restore succeeds");
            tokio::time::sleep(super::super::super::queue_io::DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY)
                .await;
        }
        super::super::super::arm_slow_idle_queue_backstop_if_queue_nonempty(
            deps.shared,
            &self.provider,
            self.channel_id,
            "queued transition busy restored",
        )
        .await;
        super::super::IntakeExecution::Deferred
    }
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
            intake_outbox_id: None,
            channel_id,
            user_msg_id: intervention.message_id,
            source_message_ids: intervention.source_message_ids.clone(),
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
        has_nonportable_uploads: intervention
            .pending_uploads
            .iter()
            .any(|upload| upload.is_local()),
        attachments: Vec::new(),
        preloaded_uploads: intervention.pending_uploads.clone(),
        voice_announcement: None,
    };
    let admission = admit_text_intake(deps, &submission).await;
    #[cfg(test)]
    let admission = super::ledger::take_test_admission(admission);
    let local_permit = match admission {
        #[cfg(test)]
        admission @ (IntakeAdmission::LedgerReceived(_) | IntakeAdmission::LedgerDeferred(_)) => {
            let Some(dispatch_lease) = dispatch_lease else {
                return QueuedAdmissionDisposition::RejectedRestore;
            };
            let mut original = intervention.clone();
            if crate::services::discord::input_runtime::supervisor::mutant("c2c1_queued_ledger_drops_original") {
                original.pending_uploads.clear();
            }
            let restore_owner = QueuedRestoreOwner {
                original,
                provider: submission.provider.clone(),
                channel_id,
                dispatch_lease,
            };
            return QueuedAdmissionDisposition::Admitted(AdmittedQueuedIntake {
                submission,
                local_permit: None,
                restore_owner: Some(restore_owner),
                ledger_admission: Some(admission),
            });
        }
        IntakeAdmission::ConsumedToHold => {
            tracing::warn!(
                provider = submission.provider.as_str(),
                channel_id = channel_id.get(),
                user_msg_id = intervention.message_id.get(),
                "queued intake consumed to the hold of its already started request"
            );
            let _ = super::super::super::mailbox_abandon_pending_dispatch(
                deps.shared,
                &submission.provider,
                channel_id,
                intervention.message_id,
            )
            .await;
            return QueuedAdmissionDisposition::ConsumedToHold;
        }
        IntakeAdmission::Local(permit) => Some(permit),
        IntakeAdmission::Forwarded { .. } | IntakeAdmission::SkippedDuplicate => None,
        IntakeAdmission::Blocked {
            reason:
                reason @ (crate::services::cluster::intake_router_hook::IntakeBlockedReason::NonPortableAttachmentForeignOwner { .. }
                | crate::services::cluster::intake_router_hook::IntakeBlockedReason::NonPortableAttachmentRoutedTarget { .. }
                | crate::services::cluster::intake_router_hook::IntakeBlockedReason::AttachmentUnavailable { .. }),
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
        IntakeAdmission::DeferredOpenRoute { .. }
        | IntakeAdmission::ReplayDeferred(_)
        | IntakeAdmission::Blocked { .. } => {
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
    let restore_owner = if local_permit.is_some() {
        let Some(dispatch_lease) = dispatch_lease else {
            tracing::error!(
                channel_id = channel_id.get(),
                "local queued intake is missing its dispatch lease"
            );
            return QueuedAdmissionDisposition::RejectedRestore;
        };
        Some(QueuedRestoreOwner {
            original: intervention.clone(),
            provider: submission.provider.clone(),
            channel_id,
            dispatch_lease,
        })
    } else {
        None
    };
    QueuedAdmissionDisposition::Admitted(AdmittedQueuedIntake {
        submission,
        local_permit,
        restore_owner,
        #[cfg(test)]
        ledger_admission: None,
    })
}

#[cfg(test)]
#[derive(Debug)]
pub(in crate::services::discord::router) struct QueuedLedgerOutcome {
    pub(in crate::services::discord::router) admission: IntakeAdmission,
    pub(in crate::services::discord::router) original: Intervention,
    pub(in crate::services::discord::router) provider: ProviderKind,
    pub(in crate::services::discord::router) channel_id: serenity::ChannelId,
    pub(in crate::services::discord::router) dispatch_lease:
        std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
}
#[cfg(test)]
impl std::fmt::Display for QueuedLedgerOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("queued ledger admission retains its original and dispatch lease")
    }
}
#[cfg(test)]
impl std::error::Error for QueuedLedgerOutcome {}

pub(crate) async fn finish_admitted_queued_intake(
    deps: &super::super::message_handler::IntakeDeps<'_>,
    admitted: AdmittedQueuedIntake,
) -> Result<super::super::IntakeExecution, super::super::super::Error> {
    #[cfg(test)]
    if !crate::services::discord::input_runtime::supervisor::mutant(
        "c2c1_queued_ledger_means_executed",
    ) && let Some(admission) = admitted.ledger_admission
    {
        let owner = admitted
            .restore_owner
            .expect("queued ledger admission retains custody");
        if crate::services::discord::input_runtime::supervisor::mutant(
            "c2c1_queued_ledger_drops_lease",
        ) {
            return Err(Box::new(super::ledger::LedgerOutcome(admission)));
        }
        return Err(Box::new(QueuedLedgerOutcome {
            admission,
            original: owner.original,
            provider: owner.provider,
            channel_id: owner.channel_id,
            dispatch_lease: owner.dispatch_lease,
        }));
    }
    let Some(permit) = admitted.local_permit else {
        return Ok(super::super::IntakeExecution::Executed);
    };
    let mut owner = admitted
        .restore_owner
        .expect("local queued admission owns its original and lease");
    if let Some(announcement) = owner.original.voice_announcement.as_ref() {
        crate::voice::announce_meta::global_store()
            .insert_accepted_replay(owner.original.message_id, announcement.clone());
    }
    match finish_admitted_local(deps, permit, admitted.submission).await {
        Err(error) => match error.downcast::<super::QueuedTransitionBusyHandoff>() {
            Ok(handoff) => {
                // The handoff already includes the original queue uploads once.
                owner.original.pending_uploads = handoff.owned_uploads;
                Ok(owner.restore(deps).await)
            }
            Err(error) => Err(error),
        },
        Ok(result) => Ok(result),
    }
}

#[cfg(test)]
pub(in crate::services::discord::router) mod transition_busy_tests;

#[cfg(test)]
fn local_admission_config_after_shared_test_env_lock(
    root: &std::path::Path,
) -> crate::config::TestEnvVarGuard {
    crate::config::test_env_lock::assert_shared_test_env_lock_held();
    // Keep Postgres-less local admission independent of a host's cluster configuration.
    let config = crate::runtime_layout::config_file_path(root);
    std::fs::create_dir_all(config.parent().expect("config dir")).expect("create config dir");
    std::fs::write(&config, "server: {}\ndata: {dir: data}\n").expect("write config");
    crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_admission_carries_every_absorbed_source_message_id() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            std::ffi::OsStr::new("disabled"),
        );
        let root = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let _config = local_admission_config_after_shared_test_env_lock(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = std::sync::Arc::new(serenity::Http::new("Bot queued-source-id-test"));
        let deps = super::super::super::message_handler::IntakeDeps {
            http: &http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &shared,
            token: "queued-source-id-test",
        };
        let source_a = serenity::MessageId::new(5_071_201);
        let source_b = serenity::MessageId::new(5_071_202);
        let intervention = Intervention {
            author_id: serenity::UserId::new(5_071_203),
            author_is_bot: false,
            message_id: source_b,
            queued_generation: 1,
            source_message_ids: vec![source_a, source_b],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "absorbed source ids".to_string(),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: true,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };

        let channel = serenity::ChannelId::new(5_071_204);
        let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
            &ProviderKind::Claude,
            &shared.token_hash,
            None,
        );
        shared
            .mailbox(channel)
            .enqueue(intervention.clone(), persistence.clone())
            .await;
        let taken = shared.mailbox(channel).take_next_soft(persistence).await;
        let intervention = taken.intervention.unwrap();
        let admitted = admit_queued_intake(
            &deps,
            ProviderKind::Claude,
            channel,
            &intervention,
            intervention.author_id,
            "source-owner".to_string(),
            false,
            false,
            "queued_source_id_test",
            taken.dispatch_lease,
        )
        .await;
        let QueuedAdmissionDisposition::Admitted(admitted) = admitted else {
            panic!("disabled routing must admit queued intake locally");
        };

        assert_eq!(
            admitted.submission.request.source_message_ids,
            vec![source_a, source_b],
            "queued admission must carry every absorbed source message id"
        );
    }

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
