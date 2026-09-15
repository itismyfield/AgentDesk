use super::*;

type DispatchLeaseHandle = Arc<crate::services::turn_orchestrator::DispatchLease>;

pub(super) fn persistence_context(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> crate::services::turn_orchestrator::QueuePersistenceContext {
    crate::services::turn_orchestrator::QueuePersistenceContext::new(
        provider,
        &shared.token_hash,
        shared
            .dispatch
            .role_overrides
            .get(&channel_id)
            .map(|override_id| override_id.value().get()),
    )
}

pub(super) fn log_kickoff_rejected_restore(provider: &ProviderKind, channel_id: ChannelId) {
    tracing::error!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        "KICKOFF: queued admission failed to restore dequeued head"
    );
}

/// Outcome of `mailbox_enqueue_intervention`: exposes both the enqueue
/// success and whether the incoming intervention was merged into the previous
/// queue entry, so callers can pick a different reaction emoji for merged
/// vs standalone queue entries (#1190 follow-up).
#[derive(Clone, Debug, Default)]
pub(in crate::services::discord) struct MailboxEnqueueOutcome {
    pub(super) enqueued: bool,
    pub(super) merged: bool,
    /// #2728: present iff `enqueued == false`. Identifies which guard
    /// (source-id dedup / last-item dedup / actor unreachable) produced the
    /// refusal so callers can surface it in producer-exit diagnostics.
    pub(super) refusal_reason: Option<crate::services::turn_orchestrator::EnqueueRefusalReason>,
    pub(super) persistence_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AutomaticQueueProgression {
    Empty,
    BlockedByCappedRetries,
    Eligible(MessageId),
}

fn busy_retry_is_capped(
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: &Intervention,
) -> bool {
    super::busy_followup_retry_store::state_is_capped(
        super::busy_followup_retry_store::resolve_identity(
            provider,
            channel_id.get(),
            intervention.message_id.get(),
            &intervention.source_message_ids,
        )
        .state,
    )
}

pub(super) fn automatic_progression(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> AutomaticQueueProgression {
    if !crate::services::agent_recovery::allows_cli_turn_for_provider(
        &channel_id.get().to_string(),
        provider,
    ) {
        return AutomaticQueueProgression::Empty;
    }
    if let Some(intervention) = snapshot.intervention_queue.iter().find(|intervention| {
        intervention.mode == InterventionMode::Soft
            && !busy_retry_is_capped(provider, channel_id, intervention)
    }) {
        return AutomaticQueueProgression::Eligible(intervention.message_id);
    }
    if let Some((marker, _)) =
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            provider,
            &shared.token_hash,
            channel_id,
        )
    {
        return if busy_retry_is_capped(provider, channel_id, &marker) {
            AutomaticQueueProgression::BlockedByCappedRetries
        } else {
            AutomaticQueueProgression::Eligible(marker.message_id)
        };
    }
    if snapshot.intervention_queue.is_empty() {
        AutomaticQueueProgression::Empty
    } else {
        AutomaticQueueProgression::BlockedByCappedRetries
    }
}

pub(super) fn intervention_became_capped(
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: &Intervention,
) -> bool {
    busy_retry_is_capped(provider, channel_id, intervention)
}

#[derive(Debug, Default)]
pub(super) struct MailboxTakeNextSoftOutcome {
    pub(super) intervention: Option<Intervention>,
    pub(super) dispatch_lease: Option<DispatchLeaseHandle>,
    pub(super) has_more: bool,
    pub(super) persistence_error: Option<String>,
}

impl MailboxTakeNextSoftOutcome {
    pub(super) fn into_intervention(
        self,
    ) -> Option<(Intervention, bool, Option<DispatchLeaseHandle>)> {
        self.intervention
            .map(|intervention| (intervention, self.has_more, self.dispatch_lease))
    }
}

pub(super) async fn mailbox_take_next_soft_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> MailboxTakeNextSoftOutcome {
    mailbox_take_soft_intervention(shared, provider, channel_id, None).await
}

pub(super) async fn mailbox_take_next_automatic_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> MailboxTakeNextSoftOutcome {
    let mut excluded_stale_primary_message_ids = std::collections::HashSet::new();
    loop {
        let mut snapshot = super::mailbox_snapshot(shared, channel_id).await;
        snapshot.intervention_queue.retain(|intervention| {
            !excluded_stale_primary_message_ids.contains(&intervention.message_id)
        });
        let (selected_message_id, result) =
            match automatic_progression(shared, provider, channel_id, &snapshot) {
                AutomaticQueueProgression::Eligible(message_id) => (
                    Some(message_id),
                    mailbox_take_soft_intervention(shared, provider, channel_id, Some(message_id))
                        .await,
                ),
                AutomaticQueueProgression::Empty => (
                    None,
                    mailbox_take_soft_intervention(shared, provider, channel_id, None).await,
                ),
                AutomaticQueueProgression::BlockedByCappedRetries => {
                    tracing::info!(
                        provider = provider.as_str(),
                        channel_id = channel_id.get(),
                        "AUTOMATIC-QUEUE-GUARD: preserving capped busy-retry entries"
                    );
                    return MailboxTakeNextSoftOutcome::default();
                }
            };
        let Some(intervention) = result.intervention.as_ref() else {
            if result.persistence_error.is_none()
                && result.has_more
                && let Some(message_id) = selected_message_id
            {
                excluded_stale_primary_message_ids.insert(message_id);
                continue;
            }
            return result;
        };
        if !intervention_became_capped(provider, channel_id, intervention) {
            return result;
        }

        let dispatch_lease = result
            .dispatch_lease
            .as_ref()
            .expect("dequeued automatic intervention must carry its dispatch lease")
            .clone();
        let restored = mailbox_restore_dequeued_head(
            shared,
            provider,
            channel_id,
            intervention.clone(),
            dispatch_lease,
        )
        .await;
        if !restored.enqueued {
            tracing::error!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                message_id = intervention.message_id.get(),
                "AUTOMATIC-QUEUE-GUARD: capped-after-selection intervention restore failed"
            );
            return MailboxTakeNextSoftOutcome::default();
        }
    }
}

async fn mailbox_take_soft_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    primary_message_id: Option<MessageId>,
) -> MailboxTakeNextSoftOutcome {
    loop {
        let result: TakeNextSoftResult = shared
            .mailbox(channel_id)
            .take_soft_matching(
                super::queue_persistence_context(shared, provider, channel_id),
                primary_message_id,
            )
            .await;
        let queue_len_after = result.queue_len_after;
        super::apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
        if let Some(error) = result.persistence_error {
            tracing::error!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                error = %error,
                "mailbox dequeue failed durable pending-queue persistence"
            );
            return MailboxTakeNextSoftOutcome {
                intervention: None,
                dispatch_lease: None,
                has_more: result.has_more,
                persistence_error: Some(error),
            };
        }
        super::maybe_schedule_catch_up_retry_after_queue_drain(
            shared,
            provider,
            channel_id,
            queue_len_after,
        );
        let Some(intervention) = result.intervention else {
            return MailboxTakeNextSoftOutcome {
                intervention: None,
                dispatch_lease: None,
                has_more: result.has_more,
                persistence_error: None,
            };
        };

        if let Some(stale) = super::stale_dispatch_turn_for_queued_intervention(
            shared.pg_pool.as_ref(),
            &intervention,
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ DISPATCH-GUARD: dropped queued terminal dispatch {} in channel {} (status={})",
                stale.dispatch_id,
                channel_id,
                stale.status
            );
            let queue_exit_events = [QueueExitEvent {
                intervention: intervention.clone(),
                kind: stale.queue_exit_kind,
            }];
            super::apply_queue_exit_feedback(shared, channel_id, &queue_exit_events).await;
            let _ = super::mailbox_abandon_pending_dispatch(
                shared,
                provider,
                channel_id,
                intervention.message_id,
            )
            .await;
            drop(result.dispatch_lease);
            if primary_message_id.is_some() {
                return MailboxTakeNextSoftOutcome {
                    intervention: None,
                    dispatch_lease: None,
                    has_more: result.has_more,
                    persistence_error: None,
                };
            }
            continue;
        }

        return MailboxTakeNextSoftOutcome {
            intervention: Some(intervention),
            dispatch_lease: result.dispatch_lease,
            has_more: result.has_more,
            persistence_error: None,
        };
    }
}

pub(super) async fn mailbox_requeue_intervention_front(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
) -> MailboxEnqueueOutcome {
    mailbox_front_requeue_outcome(
        shared,
        provider,
        channel_id,
        shared.mailbox(channel_id).requeue_front(
            intervention,
            super::queue_persistence_context(shared, provider, channel_id),
        ),
    )
    .await
}

pub(super) async fn mailbox_restore_dequeued_head(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
    dispatch_lease: DispatchLeaseHandle,
) -> MailboxEnqueueOutcome {
    mailbox_front_requeue_outcome(
        shared,
        provider,
        channel_id,
        shared.mailbox(channel_id).restore_dequeued_head(
            intervention,
            super::queue_persistence_context(shared, provider, channel_id),
            dispatch_lease,
        ),
    )
    .await
}

async fn mailbox_front_requeue_outcome(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    request: impl std::future::Future<
        Output = crate::services::turn_orchestrator::RequeueInterventionResult,
    >,
) -> MailboxEnqueueOutcome {
    let result = request.await;
    super::apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if let Some(error) = result.persistence_error.as_ref() {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            error = %error,
            "mailbox requeue-front failed durable pending-queue persistence; pending dispatch marker remains the durable backstop"
        );
    }
    MailboxEnqueueOutcome {
        enqueued: result.enqueued && result.persistence_error.is_none(),
        merged: false,
        refusal_reason: result.refusal_reason,
        persistence_error: result.persistence_error,
    }
}

pub(super) async fn mailbox_abandon_unclaimed_dispatch_after_success(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
    dispatch_lease: DispatchLeaseHandle,
) {
    shared
        .mailbox(channel_id)
        .abandon_pending_dispatch_if_lease_matches(
            user_message_id,
            dispatch_lease,
            super::queue_persistence_context(shared, provider, channel_id),
        )
        .await;
}
