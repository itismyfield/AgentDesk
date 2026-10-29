//! A mailbox's original custody and database fence stay owned through one synchronous publication.
use super::*;
use crate::db::intake_outbox::migration_wait::FencedWaiting;
use crate::services::discord::LocalCustodyWitness;

type Publish = Box<dyn FnOnce() -> Result<(), String> + Send>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    Closed,
    Active,
    PendingDispatch,
    Recovery,
    Namespace,
    QueueChanged,
    Head,
    Database,
    Publication(String),
}

pub(crate) struct PublicationRequest {
    fence: FencedWaiting,
    witnesses: Vec<Arc<LocalCustodyWitness>>,
    persistence: QueuePersistenceContext,
    publish: Publish,
}

pub(crate) struct PublicationResult {
    fence: FencedWaiting,
    outcome: Result<(), Refusal>,
}

impl PublicationRequest {
    pub(crate) fn new(
        fence: FencedWaiting,
        witnesses: Vec<Arc<LocalCustodyWitness>>,
        persistence: QueuePersistenceContext,
        publish: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) -> Self {
        Self {
            fence,
            witnesses,
            persistence,
            publish: Box::new(publish),
        }
    }

    pub(super) fn refuse(self, reason: Refusal) -> PublicationResult {
        PublicationResult {
            fence: self.fence,
            outcome: Err(reason),
        }
    }

    fn check(&self, state: &ChannelMailboxState, channel: ChannelId) -> Result<(), Refusal> {
        let intent = self.fence.intent();
        let authority = &intent.authority;
        if authority.channel_id != channel.get()
            || authority.provider != self.persistence.provider.as_str()
            || authority.token_hash != self.persistence.token_hash
            || state.last_persistence.as_ref().is_some_and(|previous| {
                previous.provider != self.persistence.provider
                    || previous.token_hash != self.persistence.token_hash
                    || previous.dispatch_role_override != self.persistence.dispatch_role_override
            })
        {
            return Err(Refusal::Namespace);
        }
        if state.closed {
            return Err(Refusal::Closed);
        }
        if state.cancel_token.is_some()
            || state.active_user_message_id.is_some()
            || state.active_request_owner.is_some()
            || state.active_turn_nonce.is_some()
            || !state.active_absorbed_source_ids.is_empty()
        {
            return Err(Refusal::Active);
        }
        if state.pending_user_dispatch.is_some()
            || !state.pending_user_dispatch_source_ids.is_empty()
            || state.pending_user_dispatch_lease.is_some()
        {
            return Err(Refusal::PendingDispatch);
        }
        if state.recovery_started_at.is_some() {
            return Err(Refusal::Recovery);
        }
        if state.injection_reserved.is_some()
            || queued_head::migration_clear(state, channel, &self.persistence).is_err()
        {
            return Err(Refusal::Head);
        }
        if !self.fence.blockers().is_empty()
            || self.fence.foreign_sessions() != 0
            || self.fence.queued_bodies() != 0
        {
            return Err(Refusal::Database);
        }
        if state.intervention_queue.len() != self.witnesses.len() {
            return Err(Refusal::QueueChanged);
        }
        let mut sources = std::collections::BTreeSet::new();
        for request in self.fence.waiting().requests() {
            for id in &request.source_ids {
                if !sources.insert(id.clone()) {
                    return Err(Refusal::Database);
                }
            }
        }
        for (item, witness) in state.intervention_queue.iter().zip(&self.witnesses) {
            if !witness.intent_matches(intent)
                || !witness.namespace_matches(&self.persistence, channel)
                || !witness.matches_exact(item)
            {
                return Err(Refusal::QueueChanged);
            }
            for id in witness.sources() {
                if !sources.insert(id.get().to_string()) {
                    return Err(Refusal::QueueChanged);
                }
            }
        }
        Ok(())
    }

    pub(super) fn commit(
        self,
        state: &mut ChannelMailboxState,
        channel: ChannelId,
    ) -> PublicationResult {
        let checked = self.check(state, channel);
        let outcome = checked.and_then(|()| (self.publish)().map_err(Refusal::Publication));
        PublicationResult {
            fence: self.fence,
            outcome,
        }
    }
}

impl PublicationResult {
    pub(crate) fn outcome(&self) -> &Result<(), Refusal> {
        &self.outcome
    }
    pub(crate) async fn release(self) -> Result<Result<(), Refusal>, String> {
        self.fence.release().await?;
        Ok(self.outcome)
    }
}

impl ChannelMailboxHandle {
    pub(crate) async fn publish_migration(
        &self,
        request: PublicationRequest,
    ) -> Result<PublicationResult, MailboxUnreachable> {
        self.request(|reply| ChannelMailboxMsg::MigrationPublication {
            request: Box::new(request),
            reply,
        })
        .await
    }

    pub(crate) async fn enqueue_migration(
        &self,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) -> EnqueueInterventionResult {
        self.request(|reply| ChannelMailboxMsg::EnqueueMigration {
            intervention,
            persistence,
            reply,
        })
        .await
        .unwrap_or_else(|_| {
            EnqueueInterventionResult::refused(EnqueueRefusalReason::ActorUnreachable, Vec::new())
        })
    }
}

impl ChannelMailboxRegistry {
    pub(crate) async fn enqueue_migration_with_closed_retry(
        &self,
        channel: ChannelId,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) -> EnqueueInterventionResult {
        let mut result =
            EnqueueInterventionResult::refused(EnqueueRefusalReason::MailboxClosed, Vec::new());
        for _ in 0..3 {
            result = self
                .handle(channel)
                .enqueue_migration(intervention.clone(), persistence.clone())
                .await;
            if result.refusal_reason != Some(EnqueueRefusalReason::MailboxClosed) {
                break;
            }
            tokio::task::yield_now().await;
        }
        result
    }
}

#[path = "migration_wait_tests.rs"]
mod tests;
