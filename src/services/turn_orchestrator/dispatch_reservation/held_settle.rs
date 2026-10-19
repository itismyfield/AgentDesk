//! The actor's half of settling a boot restore whose merge failed: it runs its own reservation and
//! marker upkeep first, then reports which ids are live, still undecided, or queued.

use std::collections::HashSet;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};
use tokio::sync::oneshot;

use super::super::pending_queue_persistence::load_channel_pending_dispatch_marker_checked;
use super::super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxState, MailboxUnreachable,
    QueuePersistenceContext,
};
use super::{
    clear_stale_pending_dispatch_reservation, hydrate_pending_queue_from_disk_if_present,
    intervention_dedup_ids, marker_trusted,
};

/// What a held channel may decide on once the actor has done its own upkeep.
#[derive(Debug, Default)]
pub(crate) struct HeldSettleProbe {
    /// The queue or a trusted marker could not be read; nothing is decided then.
    pub(crate) error: Option<String>,
    /// Queued, running, or reserved by a caller that still holds the lease.
    pub(crate) live: HashSet<MessageId>,
    /// Reserved without a caller yet too young to clear, or named by a trusted marker.
    pub(crate) undecided: HashSet<MessageId>,
    /// Every queued primary and source id.
    pub(crate) queued: HashSet<MessageId>,
}

pub(crate) struct HeldSettleMsg {
    persistence: QueuePersistenceContext,
    reply: oneshot::Sender<HeldSettleProbe>,
}

impl HeldSettleMsg {
    pub(in crate::services::turn_orchestrator) fn refuse(self, error: String) {
        let probe = HeldSettleProbe {
            error: Some(error),
            ..Default::default()
        };
        let _ = self.reply.send(probe);
    }

    pub(in crate::services::turn_orchestrator) fn step(
        self,
        state: &mut ChannelMailboxState,
        channel_id: ChannelId,
    ) {
        let _ = self.reply.send(probe(state, channel_id, &self.persistence));
    }
}

/// Clears an orphaned reservation and restores the disk queue and a trusted marker the way a take
/// would, but regardless of whether anything could be dispatched now.
fn probe(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> HeldSettleProbe {
    let _ = clear_stale_pending_dispatch_reservation(state, channel_id);
    let hydrated = hydrate_pending_queue_from_disk_if_present(state, channel_id, persistence);
    let mut probe = HeldSettleProbe {
        error: hydrated.persistence_error,
        ..Default::default()
    };
    if probe.error.is_some() {
        return probe;
    }
    probe.queued = state
        .intervention_queue
        .iter()
        .flat_map(intervention_dedup_ids)
        .collect();
    probe.live.extend(probe.queued.iter().copied());
    probe.live.extend(state.active_user_message_id);
    probe
        .live
        .extend(state.active_absorbed_source_ids.iter().copied());
    let reserved = state.pending_user_dispatch.into_iter();
    let reserved = reserved.chain(state.pending_user_dispatch_source_ids.iter().copied());
    let held_by_caller = (state.pending_user_dispatch_lease.as_ref())
        .is_some_and(|lease| Arc::strong_count(lease) > 1);
    match held_by_caller {
        true => probe.live.extend(reserved),
        false => probe.undecided.extend(reserved),
    }
    // A marker a clear could not remove was discarded by that clear; it decides nothing.
    if marker_trusted(state, persistence) {
        let (provider, token_hash) = (&persistence.provider, persistence.token_hash.as_str());
        match load_channel_pending_dispatch_marker_checked(provider, token_hash, channel_id) {
            Ok(Some((marker, _))) => probe.undecided.extend(intervention_dedup_ids(&marker)),
            Ok(None) => {}
            Err(error) => probe.error = Some(error),
        }
    }
    let live = &probe.live;
    probe.undecided.retain(|id| !live.contains(id));
    probe
}

impl ChannelMailboxHandle {
    /// The actor's reservation and marker upkeep, then what a held boot restore may decide on.
    pub(crate) async fn held_settle_probe(
        &self,
        persistence: QueuePersistenceContext,
    ) -> Result<HeldSettleProbe, MailboxUnreachable> {
        self.request(|reply| {
            let msg = HeldSettleMsg { persistence, reply };
            ChannelMailboxMsg::Injection(super::super::injected_inputs::InjectionMsg::Held(msg))
        })
        .await
    }
}
