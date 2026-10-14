use poise::serenity_prelude as serenity;

use super::{
    IntakeAdmission, IntakeOrigin, IntakeSubmission, LocalAdmissionPermit, admit_text_intake,
    finish_admitted_local,
};
use crate::services::discord::Intervention;
use crate::services::discord::input_runtime::{fence, intake, supervisor as runtime};
use crate::services::provider::ProviderKind;
use std::sync::Arc;

#[allow(clippy::large_enum_variant)]
pub(crate) enum QueuedAdmissionDisposition {
    Admitted(AdmittedQueuedIntake),
    Deferred,
    RejectedNonPortableAttachment,
    RejectedRestore,
    /// Every source already started under a held request; that request keeps the original.
    ConsumedToHold,
}

impl QueuedAdmissionDisposition {
    pub(in crate::services::discord) fn ledger_candidate(
        provider: &ProviderKind,
        channel_id: serenity::ChannelId,
    ) -> bool {
        fence::any_protected()
            && !matches!(
                intake::client::lookup(queued_registry(), provider, channel_id.get()),
                intake::client::Access::Legacy
            )
    }
}

#[cfg(test)]
mod custody_support_tests;
#[cfg(test)]
use custody_support_tests::TEST_FALLBACK_ADMISSION;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
use custody_support_tests::with_registry_for_test;

pub(crate) struct AdmittedQueuedIntake {
    submission: Option<IntakeSubmission>,
    local_permit: Option<LocalAdmissionPermit>,
    restore_owner: Option<QueuedRestoreOwner>,
    ledger_outcome: Option<Box<QueuedLedgerOutcome>>,
}
impl AdmittedQueuedIntake {
    pub(in crate::services::discord) fn is_ledger(&self) -> bool {
        self.ledger_outcome.is_some()
    }
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
    if let Some(outcome) = queued_ledger_lookup(&provider, channel_id, intervention).await {
        let mut outcome = outcome;
        outcome.dispatch_lease = dispatch_lease;
        if outcome.dispatch_lease.is_none() {
            outcome.failure = "MissingDispatchLease".into();
        }
        #[cfg(test)]
        if runtime::mutant("c2c2_missing_lease_erases_proof") && outcome.dispatch_lease.is_none() {
            return QueuedAdmissionDisposition::RejectedRestore;
        }
        return QueuedAdmissionDisposition::Admitted(AdmittedQueuedIntake {
            submission: None,
            local_permit: None,
            restore_owner: None,
            ledger_outcome: Some(outcome),
        });
    }
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
    let admission = TEST_FALLBACK_ADMISSION
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or(admission);
    let local_permit = match admission {
        admission @ (IntakeAdmission::LedgerReceived(_) | IntakeAdmission::LedgerDeferred(_)) => {
            let mut outcome = QueuedLedgerOutcome::pending(
                submission.provider.clone(), channel_id, intervention.clone(), dispatch_lease,
            );
            outcome.blocked_proof = !admission_covers_queued_original(&admission, intervention, channel_id);
            if outcome.blocked_proof {
                outcome.failure = "CanonicalProofDoesNotCoverQueuedOriginal".into();
            }
            outcome.admission = Some(admission);
            outcome.requires_lookup = false;
            return QueuedAdmissionDisposition::Admitted(AdmittedQueuedIntake {
                submission: None,
                local_permit: None,
                restore_owner: None,
                ledger_outcome: Some(outcome),
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
        submission: Some(submission),
        local_permit,
        restore_owner,
        ledger_outcome: None,
    })
}

#[derive(Debug)]
pub(in crate::services::discord) struct QueuedLedgerOutcome {
    admission: Option<IntakeAdmission>,
    original: Intervention,
    provider: ProviderKind,
    channel_id: serenity::ChannelId,
    dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    failure: String,
    requires_lookup: bool,
    reservation_released: bool,
    restore_committed: bool,
    blocked_proof: bool,
}
impl std::fmt::Display for QueuedLedgerOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "queued ledger custody unresolved: {}", self.failure)
    }
}
impl std::error::Error for QueuedLedgerOutcome {}

impl QueuedLedgerOutcome {
    pub(in crate::services::discord) fn pending(
        provider: ProviderKind,
        channel_id: serenity::ChannelId,
        original: Intervention,
        dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> Box<Self> {
        Box::new(Self {
            admission: None,
            original,
            provider,
            channel_id,
            dispatch_lease,
            failure: "PendingResponsibilityLookup".into(),
            requires_lookup: true,
            reservation_released: false,
            restore_committed: false,
            blocked_proof: false,
        })
    }
    pub(in crate::services::discord) fn provider(&self) -> &ProviderKind {
        &self.provider
    }
    pub(in crate::services::discord) fn channel_id(&self) -> serenity::ChannelId {
        self.channel_id
    }
    pub(in crate::services::discord) fn failure(&self) -> &str {
        &self.failure
    }
    pub(in crate::services::discord) fn original(&self) -> &Intervention {
        &self.original
    }
    pub(in crate::services::discord) fn dispatch_lease(
        &self,
    ) -> Option<&Arc<crate::services::turn_orchestrator::DispatchLease>> {
        self.dispatch_lease.as_ref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum QueuedCustody {
    Received,
    Restored,
}

fn queued_registry() -> &'static runtime::Registry {
    #[cfg(test)]
    if let Ok(registry) = custody_support_tests::TEST_REGISTRY.try_with(|registry| *registry) {
        return registry;
    }
    runtime::runtime::registry()
}

async fn queued_ledger_lookup(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    original: &Intervention,
) -> Option<Box<QueuedLedgerOutcome>> {
    #[cfg(test)]
    if runtime::mutant("c2c2_queued_bypass_ledger") {
        return None;
    }
    #[cfg(test)]
    if let Some(admission) = super::ledger::take_injected_admission() {
        let mut outcome =
            QueuedLedgerOutcome::pending(provider.clone(), channel_id, original.clone(), None);
        outcome.admission = Some(admission);
        outcome.requires_lookup = false;
        return Some(outcome);
    }
    if !QueuedAdmissionDisposition::ledger_candidate(provider, channel_id) {
        return None;
    }
    let mut outcome =
        QueuedLedgerOutcome::pending(provider.clone(), channel_id, original.clone(), None);
    outcome.requires_lookup = false;
    // A control channel can differ from the announcement source; the queue stores no separate origin.
    if original
        .voice_announcement
        .as_ref()
        .and_then(|voice| voice.control_channel_id)
        .is_some_and(|source| source != channel_id.get())
    {
        outcome.failure = "UncertainOriginalChannel".into();
        return Some(outcome);
    }
    let prepared = match intake::prepare_source(intake::SourceDraft {
        origin: intake::Origin::Message {
            message_id: original.message_id.get(),
            source_ids: original
                .source_message_ids
                .iter()
                .map(|id| id.get())
                .collect(),
            author_id: original.author_id.get(),
            original_channel_id: channel_id.get(),
            execution_channel_id: channel_id.get(),
        },
        author_is_bot: original.author_is_bot,
        original_text: original.text.clone(),
        prompt: original.text.clone(),
        reply_context: original.reply_context.clone(),
        attachments: Vec::new(),
        voice_announcement: original.voice_announcement.clone(),
    }) {
        Ok(prepared) => prepared,
        Err(error) => {
            outcome.failure = format!("InvalidOriginal: {error}");
            return Some(outcome);
        }
    };
    let lookup = match intake::client::lookup(queued_registry(), provider, channel_id.get()) {
        intake::client::Access::Legacy => {
            outcome.failure = "ProtectionLost".into();
            outcome.requires_lookup = true;
            return Some(outcome);
        }
        intake::client::Access::Deferred(reason) => Err(reason),
        intake::client::Access::Ready(client) => {
            #[cfg(test)]
            let identity = custody_support_tests::lookup_identity(&prepared);
            #[cfg(not(test))]
            let identity = prepared.identity().clone();
            client.lookup_responsibility(identity).await
        }
    };
    outcome.admission = Some(super::ledger::export_lookup(prepared, lookup));
    Some(outcome)
}

fn same_queued_original(original: &Intervention, restored: &Intervention) -> bool {
    use crate::services::turn_orchestrator::QueueIdentity;
    let generations = |item: &Intervention| {
        item.source_message_queued_generations()
            .into_iter()
            .map(|source| {
                (
                    source.message_id,
                    source.queued_generation,
                    source.enqueued_at_epoch_us,
                    source.preserve_on_cancel,
                )
            })
            .collect::<Vec<_>>()
    };
    let texts = |item: &Intervention| {
        item.source_text_segments()
            .into_iter()
            .map(|source| (source.message_id, source.text))
            .collect::<Vec<_>>()
    };
    let age_delta = if original.created_at >= restored.created_at {
        original.created_at.duration_since(restored.created_at)
    } else {
        restored.created_at.duration_since(original.created_at)
    };
    QueueIdentity::of(original) == QueueIdentity::of(restored)
        && original.author_id == restored.author_id
        && original.author_is_bot == restored.author_is_bot
        && original.text == restored.text
        && original.mode == restored.mode
        && original.reply_context == restored.reply_context
        && original.has_reply_boundary == restored.has_reply_boundary
        && original.merge_consecutive == restored.merge_consecutive
        && original.pending_uploads == restored.pending_uploads
        && original.voice_announcement == restored.voice_announcement
        && generations(original) == generations(restored)
        && texts(original) == texts(restored)
        && age_delta <= std::time::Duration::from_millis(2)
}

fn admission_covers_queued_original(
    admission: &IntakeAdmission,
    original: &Intervention,
    channel: serenity::ChannelId,
) -> bool {
    let prepared = match admission {
        IntakeAdmission::LedgerReceived(proof) => &proof.original,
        IntakeAdmission::LedgerDeferred(proof) => &proof.original,
        _ => return false,
    };
    let identity = prepared.identity();
    identity.author_id == original.author_id.get()
        && identity.original_channel_id == channel.get()
        && identity.execution_channel_id == channel.get()
        && identity.source_ids.contains(&original.message_id.get())
        && original
            .source_message_ids
            .iter()
            .all(|id| identity.source_ids.contains(&id.get()))
        && !original
            .voice_announcement
            .as_ref()
            .and_then(|voice| voice.control_channel_id)
            .is_some_and(|source| source != channel.get())
}

async fn queued_marker_absent(
    outcome: &QueuedLedgerOutcome,
    token_hash: &str,
) -> Result<bool, String> {
    let (provider, token_hash, channel) = (
        outcome.provider.clone(),
        token_hash.to_owned(),
        outcome.channel_id,
    );
    tokio::task::spawn_blocking(move || {
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker_checked(
            &provider,
            &token_hash,
            channel,
        )
    })
    .await
    .map_err(|error| format!("DispatchMarkerWorkerFailed: {error}"))?
    .map(|marker| marker.is_none())
    .map_err(|error| format!("DispatchMarkerReadbackFailed: {error}"))
}

async fn queued_original_persisted(
    outcome: &QueuedLedgerOutcome,
    token_hash: &str,
) -> Result<bool, String> {
    let (provider, token_hash, channel) = (
        outcome.provider.clone(),
        token_hash.to_owned(),
        outcome.channel_id,
    );
    let (items, _) = tokio::task::spawn_blocking(move || {
        crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &provider,
            &token_hash,
            channel,
        )
    })
    .await
    .map_err(|error| format!("RestoreReadbackWorkerFailed: {error}"))?
    .map_err(|error| format!("RestoreReadbackFailed: {error}"))?;
    Ok(items
        .iter()
        .any(|item| same_queued_original(&outcome.original, item)))
}

pub(in crate::services::discord) fn consume_queued_ledger_outcome<'a>(
    deps: &'a super::super::message_handler::IntakeDeps<'_>,
    outcome: &'a mut QueuedLedgerOutcome,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<QueuedCustody, ()>> + Send + 'a>> {
    Box::pin(async move {
        let blocked = outcome.blocked_proof;
        #[cfg(test)]
        let blocked =
            custody_support_tests::guard(blocked, "c2c2_queued_unblocks_mismatched_proof");
        if blocked {
            outcome.failure = "CanonicalProofDoesNotCoverQueuedOriginal".into();
            return Err(());
        }
        if outcome.requires_lookup {
            let Some(lookup) =
                queued_ledger_lookup(&outcome.provider, outcome.channel_id, &outcome.original)
                    .await
            else {
                outcome.failure = "ProtectionLost".into();
                return Err(());
            };
            outcome.admission = lookup.admission;
            outcome.requires_lookup = lookup.requires_lookup;
            outcome.failure = lookup.failure;
            if outcome.requires_lookup {
                return Err(());
            }
        }
        let Some(lease) = outcome.dispatch_lease.as_ref() else {
            outcome.failure = "MissingDispatchLease".into();
            return Err(());
        };
        #[cfg(test)]
        let lease = custody_support_tests::dispatch_lease(lease);
        #[cfg(not(test))]
        let lease = lease.clone();
        #[cfg(test)]
        custody_support_tests::before_custody(deps, outcome).await;
        if matches!(outcome.admission, Some(IntakeAdmission::LedgerReceived(_))) {
            let recover = !outcome.reservation_released;
            #[cfg(test)]
            let recover =
                custody_support_tests::guard(recover, "c2c2_queued_received_ack_recovery_removed");
            if recover {
                outcome.reservation_released = deps
                    .shared
                    .mailbox(outcome.channel_id)
                    .try_snapshot()
                    .await
                    .is_ok_and(|snapshot| snapshot.pending_user_dispatch.is_none())
                    && queued_marker_absent(outcome, &deps.shared.token_hash)
                        .await
                        .unwrap_or(false);
            }
            let released = outcome.reservation_released;
            #[cfg(test)]
            let released =
                custody_support_tests::guard(released, "c2c2_queued_received_stage_ignored");
            if !released {
                let authorized = deps
                    .shared
                    .mailbox(outcome.channel_id)
                    .abandon_pending_dispatch_if_lease_matches(
                        outcome.original.message_id,
                        lease.clone(),
                        super::super::super::queue_dispatch::persistence_context(
                            deps.shared,
                            &outcome.provider,
                            outcome.channel_id,
                        ),
                    )
                    .await;
                if !authorized {
                    outcome.failure = "DispatchLeaseNotAuthorized".into();
                    return Err(());
                }
                outcome.reservation_released = true;
            }
            match queued_marker_absent(outcome, &deps.shared.token_hash).await {
                Ok(true) => {}
                Ok(false) => {
                    outcome.failure = "PendingDispatchMarkerStillPresent".into();
                    return Err(());
                }
                Err(error) => {
                    outcome.failure = error;
                    return Err(());
                }
            }
            super::super::super::queue_dispatch::settle_received_queued_custody(
                deps.shared,
                deps.http,
                &outcome.provider,
                outcome.channel_id,
                &outcome.original,
                "queued ledger received custody",
            )
            .await;
            return Ok(QueuedCustody::Received);
        }
        let recover = !outcome.restore_committed;
        #[cfg(test)]
        let recover =
            custody_support_tests::guard(recover, "c2c2_queued_deferred_ack_recovery_removed");
        if recover {
            outcome.restore_committed = deps
                .shared
                .mailbox(outcome.channel_id)
                .try_snapshot()
                .await
                .is_ok_and(|snapshot| {
                    snapshot
                        .intervention_queue
                        .iter()
                        .any(|item| same_queued_original(&outcome.original, item))
                })
                && queued_original_persisted(outcome, &deps.shared.token_hash)
                    .await
                    .unwrap_or(false);
        }
        let restored = outcome.restore_committed;
        #[cfg(test)]
        let restored = custody_support_tests::guard(restored, "c2c2_queued_deferred_stage_ignored");
        if !restored {
            #[cfg(test)]
            let original = custody_support_tests::original_for_restore(&outcome.original);
            #[cfg(not(test))]
            let original = outcome.original.clone();
            let restored = super::super::super::mailbox_restore_dequeued_head(
                deps.shared,
                &outcome.provider,
                outcome.channel_id,
                original,
                lease.clone(),
            )
            .await;
            #[cfg(test)]
            if runtime::mutant("c2c2_queued_restore_result_ignored") {
                return Ok(QueuedCustody::Restored);
            }
            if !restored.enqueued || restored.persistence_error.is_some() {
                outcome.failure = format!(
                    "RestoreFailed: refusal={:?} persistence={:?}",
                    restored.refusal_reason, restored.persistence_error
                );
                return Err(());
            }
            outcome.restore_committed = true;
        }
        match queued_original_persisted(outcome, &deps.shared.token_hash).await {
            Ok(true) => Ok(QueuedCustody::Restored),
            Ok(false) => {
                outcome.failure = "RestoredOriginalMissingOrChanged".into();
                Err(())
            }
            Err(error) => {
                outcome.failure = error;
                Err(())
            }
        }
    })
}

pub(crate) async fn finish_admitted_queued_intake(
    deps: &super::super::message_handler::IntakeDeps<'_>,
    admitted: AdmittedQueuedIntake,
) -> Result<super::super::IntakeExecution, super::super::super::Error> {
    if let Some(outcome) = admitted.ledger_outcome {
        #[cfg(test)]
        return custody_support_tests::finish_ledger(outcome);
        #[cfg(not(test))]
        return Err(outcome);
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
    match finish_admitted_local(
        deps,
        permit,
        admitted
            .submission
            .expect("local admission owns its submission"),
    )
    .await
    {
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
            admitted
                .submission
                .as_ref()
                .unwrap()
                .request
                .source_message_ids,
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
