//! Explicit clear changes the V2 owner's durable snapshot and memory in the same actor turn.
use super::*;
use crate::db::turns::TerminalOutcome;
use crate::services::turn_orchestrator::{
    ChannelMailboxState, ClearChannelResult, QueuePersistenceContext,
    clear_channel::ActivePolicy,
    clear_pending_user_dispatch,
    pending_queue_persistence::{
        load_channel_pending_dispatch_marker_checked, remove_channel_pending_dispatch_marker,
    },
    release_active_turn_anchor,
};
use poise::serenity_prelude::{ChannelId, MessageId};

/// Legacy and unreadable files keep their existing replacement path. A parsed V2 file always
/// clears through its owner, including the scratch state used when no mailbox is registered.
pub(crate) fn clear_channel(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    persistence: &QueuePersistenceContext,
    active: ActivePolicy,
) -> Option<ClearChannelResult> {
    let (path, namespace) = actor::v2_queue(persistence, channel).ok()?;
    let adoption = if state.queued_head.v2.is_none() {
        let raw = std::fs::read_to_string(&path).ok()?;
        if !matches!(
            super::super::compat::inspect(&raw),
            Ok(super::super::compat::SnapshotRead::ReadOnlyV2(_))
        ) {
            return None;
        }
        Owner::adopt(path.clone(), namespace.clone())
            .map(|owner| state.queued_head.v2 = Some(owner))
    } else {
        Ok(())
    };
    state.last_persistence = Some(persistence.clone());
    let mut result = ClearChannelResult::default();
    let protected: Vec<_> = state
        .active_user_message_id
        .into_iter()
        .chain(state.active_absorbed_source_ids.iter().copied())
        .collect();
    result.persistence.not_waiting = protected.clone();
    if active == ActivePolicy::Release {
        result
            .persistence
            .not_waiting
            .extend(state.pending_user_dispatch);
        result
            .persistence
            .not_waiting
            .extend(state.pending_user_dispatch_source_ids.iter().copied());
    }
    let keep = if active == ActivePolicy::Preserve {
        protected.as_slice()
    } else {
        &[]
    };
    let cleared = adoption.and_then(|()| {
        state
            .queued_head
            .v2
            .as_mut()
            .ok_or("no V2 owner")?
            .clear_all(keep)
    });
    let cleared: Result<Vec<MessageId>, String> = cleared.or_else(|error| {
        tracing::warn!(%error, "V2 clear will readopt the queue and retry once");
        // An explicit clear may replace a stale owner, but still publishes through the same CAS.
        let (path, namespace) = state
            .queued_head
            .v2
            .take()
            .map_or((path, namespace), |owner| {
                (owner.path, owner.snapshot.namespace.unwrap_or_default())
            });
        let owner = Owner::adopt(path, namespace)?;
        state.queued_head.v2 = Some(owner);
        let discarded = state
            .queued_head
            .v2
            .as_mut()
            .ok_or("no V2 owner")?
            .clear_all(keep)?;
        result.persistence.retried.push("queue_publish");
        Ok(discarded)
    });
    match cleared {
        Ok(mut discarded) => {
            let (provider, token) = (&persistence.provider, persistence.token_hash.as_str());
            match load_channel_pending_dispatch_marker_checked(provider, token, channel) {
                Ok(Some((marker, _))) => discarded
                    .extend(std::iter::once(marker.message_id).chain(marker.source_message_ids)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "clear could not read the dispatch marker");
                    result.persistence.count_unknown = true;
                }
            }
            for item in state.intervention_queue.drain(..) {
                discarded.extend(std::iter::once(item.message_id).chain(item.source_message_ids));
            }
            discarded.extend(state.pending_user_dispatch);
            discarded.extend(state.pending_user_dispatch_source_ids.iter().copied());
            clear_pending_user_dispatch(state);
            state.recently_valve_cleared_dispatch = None;
            discarded.sort_unstable();
            discarded.dedup();
            if active == ActivePolicy::Preserve {
                let running: Vec<_> = state
                    .queued_head
                    .v2
                    .as_ref()
                    .into_iter()
                    .flat_map(|owner| owner.snapshot.head_injection.iter())
                    .flat_map(|head| head.members())
                    .flat_map(|item| sources::identity(item).sources)
                    .map(|source| MessageId::new(source.message_id))
                    .collect();
                discarded.retain(|id| !running.contains(id));
                result
                    .persistence
                    .not_waiting
                    .retain(|id| !discarded.contains(id));
                state.remint_fence.note_discarded(&discarded);
            }
            result.discarded_message_ids = discarded;
            super::super::release_for_clear(state, persistence);
            state.remint_fence.begin_clear();
            let namespace =
                crate::services::turn_orchestrator::remint_fence::namespace(persistence);
            state.remint_fence.trust_queue(&namespace);
            if result.persistence.attempt("marker_unlink", || {
                remove_channel_pending_dispatch_marker(provider, token, channel)
            }) {
                state.remint_fence.trust_marker(&namespace);
            }
        }
        Err(error) => {
            tracing::warn!(%error, "V2 clear could not commit; nothing was discarded");
            result.persistence.failed.push("queue_publish");
            result.persistence.count_unknown = true;
        }
    }
    if active == ActivePolicy::Release {
        result.removed_token =
            release_active_turn_anchor(state, channel, Some(&persistence.provider));
    }
    Some(result)
}

impl Owner {
    fn clear_all(&mut self, protected: &[MessageId]) -> Result<Vec<MessageId>, String> {
        self.recover()?;
        let mut next = self.snapshot.clone();
        let at = origin::now_ms();
        let head = next.head_injection.take().filter(|head| {
            head.phase != HeadPhase::Reserved
                && head.members().all(|item| {
                    sources::identity(item)
                        .sources
                        .iter()
                        .all(|source| protected.contains(&MessageId::new(source.message_id)))
                })
        });
        let kept = head.as_ref().map_or(0, |head| head.members().count());
        if let Some(input) = self
            .snapshot
            .head_injection
            .as_ref()
            .filter(|head| head.phase == HeadPhase::Reserved)
        {
            next.owe_attempt(input, TerminalOutcome::Cancelled, at);
        }
        next.owe_queued(
            control::untried(self)..next.items.len(),
            TerminalOutcome::Cancelled,
            at,
        );
        let inputs: Vec<_> = next
            .retained_inputs
            .iter()
            .filter(|entry| control::owes_cancelled(entry))
            .map(|entry| entry.input.clone())
            .collect();
        for input in inputs {
            next.owe_attempt(&input, TerminalOutcome::Cancelled, at);
        }
        let mut discarded: Vec<_> = next
            .items
            .iter()
            .skip(kept)
            .chain(
                next.retained_inputs
                    .iter()
                    .flat_map(|entry| entry.input.members()),
            )
            .map(sources::identity)
            .collect();
        if let Some(pending) = next
            .effort_restart
            .as_ref()
            .and_then(|intent| intent.pending())
        {
            discarded.push(sources::identity(&pending.original));
        }
        // An armed, protected input keeps all members and their exact attempt for late settlement.
        next.items.truncate(kept);
        next.head_injection = head;
        next.retained_inputs.clear();
        next.unfinished_handoff = None;
        next.effort_restart = None;
        next.admission_binds.retain(|bind| match &bind.at {
            BindAt::Settled { .. } => true,
            BindAt::Attempt { identity, attempt } => {
                next.head_injection.as_ref().is_some_and(|head| {
                    head.members()
                        .any(|item| sources::identity(item) == *identity)
                        && head.attempt.as_ref() == Some(attempt)
                })
            }
            _ => false,
        });
        self.commit(next)?;
        self.revoke_front();
        self.held = None;
        self.unacknowledged.clear();
        self.project_slots();
        Ok(discarded
            .into_iter()
            .flat_map(|identity| identity.sources)
            .map(|source| MessageId::new(source.message_id))
            .collect())
    }
}
