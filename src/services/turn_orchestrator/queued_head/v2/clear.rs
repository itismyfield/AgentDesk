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
        Owner::adopt(path, namespace).map(|owner| state.queued_head.v2 = Some(owner))
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
    let cleared = adoption.and_then(|()| {
        state
            .queued_head
            .v2
            .as_mut()
            .ok_or("no V2 owner")?
            .clear_all(if active == ActivePolicy::Preserve {
                &protected
            } else {
                &[]
            })
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
                discarded.retain(|id| !protected.contains(id));
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
        next.owe_queued(0..next.items.len(), TerminalOutcome::Cancelled, at);
        next.owe_opaque(TerminalOutcome::Cancelled, at);
        let inputs: Vec<_> = next
            .head_injection
            .iter()
            .chain(next.retained_inputs.iter().map(|entry| &entry.input))
            .cloned()
            .collect();
        let mut discarded: Vec<_> = next.items.iter().map(sources::identity).collect();
        for input in inputs {
            next.owe_attempt(&input, TerminalOutcome::Cancelled, at);
            discarded.extend(input.members().map(sources::identity));
        }
        if let Some(pending) = next
            .effort_restart
            .as_ref()
            .and_then(|intent| intent.pending())
        {
            discarded.push(sources::identity(&pending.original));
        }
        let running: Vec<_> = discarded
            .iter()
            .filter(|identity| {
                identity
                    .sources
                    .iter()
                    .any(|source| protected.contains(&MessageId::new(source.message_id)))
            })
            .map(|identity| identity.primary)
            .collect();
        // A preserved turn still owns its result; only waiting origins become cancelled.
        next.owed_drop_receipts.retain(|owed| {
            !running.contains(&owed.message_id) || self.snapshot.owed_drop_receipts.contains(owed)
        });
        next.items.clear();
        next.head_injection = None;
        next.retained_inputs.clear();
        next.unfinished_handoff = None;
        next.effort_restart = None;
        next.admission_binds
            .retain(|bind| matches!(bind.at, BindAt::Settled { .. }));
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
