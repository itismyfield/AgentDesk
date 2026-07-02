use super::*;

type DispatchLeaseHandle = Arc<crate::services::turn_orchestrator::DispatchLease>;

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

pub(super) async fn mailbox_abandon_unclaimed_dispatch_after_success(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
) {
    let snapshot = super::mailbox_snapshot(shared, channel_id).await;
    let active_identity_matches =
        snapshot.cancel_token.is_some() && snapshot.active_user_message_id == Some(user_message_id);
    if snapshot.pending_user_dispatch == Some(user_message_id) && !active_identity_matches {
        super::mailbox_abandon_pending_dispatch(shared, provider, channel_id, user_message_id)
            .await;
    }
}
