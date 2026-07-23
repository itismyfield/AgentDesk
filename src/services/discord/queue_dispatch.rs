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
