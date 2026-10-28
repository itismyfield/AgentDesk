//! What the V1 paths may know of a V2 queue: a boot/catalog plan, read-only state views, and the
//! checked purge's compare-and-commit. Reading never writes; boot wiring that acts on a plan is not here.
use super::*;
#[cfg(unix)]
use crate::services::turn_orchestrator::checked_purge::V2PurgeStop as PurgeStop;
use crate::services::turn_orchestrator::pending_queue_persistence::{
    load_channel_pending_dispatch_marker_checked, pending_queue_file_path,
    queued_recovery::read_catalog,
};
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, ChannelMailboxState, QueuePersistenceContext,
};
use poise::serenity_prelude::{ChannelId, MessageId};

/// Boot recovery runs every channel's queue before any inflight, marker or placeholder step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    Queue,
    Inflight,
    Marker,
    Placeholder,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    /// Adopt the channel's file, metadata-only included.
    Adopt(ChannelId),
    /// Unreadable or not migratable: nothing adopts it and its raw bytes stay as they are.
    Hold(ChannelId, String),
    /// The attempted head an inflight restore settles; it is never dispatched again.
    Inflight(ChannelId, QueueIdentity),
    /// A dispatch marker the owner keeps; it is not merged into a V1 queue.
    Marker(ChannelId),
    /// Retained inputs whose notice is still owed.
    Placeholder(ChannelId, Vec<QueueIdentity>),
}

impl Step {
    pub(crate) fn phase(&self) -> Phase {
        match self {
            Self::Adopt(_) | Self::Hold(..) => Phase::Queue,
            Self::Inflight(..) => Phase::Inflight,
            Self::Marker(_) => Phase::Marker,
            Self::Placeholder(..) => Phase::Placeholder,
        }
    }

    fn channel(&self) -> ChannelId {
        match self {
            Self::Adopt(c) | Self::Hold(c, _) | Self::Inflight(c, _) => *c,
            Self::Marker(c) | Self::Placeholder(c, _) => *c,
        }
    }
}

/// Every catalog channel as the adoption under `namespace` would see it, phase by phase.
pub(crate) fn plan(persistence: &QueuePersistenceContext, namespace: &str) -> Vec<Step> {
    let (provider, token_hash) = (&persistence.provider, persistence.token_hash.as_str());
    let catalog = read_catalog(provider, token_hash);
    let mut steps: Vec<Step> = catalog
        .quarantined
        .into_iter()
        .map(|(channel, error)| Step::Hold(channel, error))
        .collect();
    for channel in catalog.snapshots.into_keys() {
        let restored = pending_queue_file_path(provider, token_hash, channel)
            .ok_or_else(|| "pending queue root unavailable".to_string())
            .and_then(|path| std::fs::read_to_string(path).map_err(|e| e.to_string()))
            .and_then(|raw| sources::restore(&raw, namespace));
        let snapshot = match restored {
            Ok(snapshot) => snapshot,
            Err(error) => {
                steps.push(Step::Hold(channel, error));
                continue;
            }
        };
        steps.push(Step::Adopt(channel));
        if let Some(head) = snapshot.head_injection {
            steps.push(Step::Inflight(channel, head.identity));
        }
        if !matches!(
            load_channel_pending_dispatch_marker_checked(provider, token_hash, channel),
            Ok(None)
        ) {
            steps.push(Step::Marker(channel));
        }
        let owed: Vec<QueueIdentity> = (snapshot.retained_inputs.into_iter())
            .filter(|entry| !entry.notice.enqueued)
            .map(|entry| entry.input.identity)
            .collect();
        if !owed.is_empty() {
            steps.push(Step::Placeholder(channel, owed));
        }
    }
    steps.sort_by_key(|step| (step.phase(), step.channel()));
    steps
}

/// Adopts every planned channel whose actor holds no owner, so a mailbox the registry dropped
/// keeps its retained inputs and notices; returns the channels adopted.
pub(crate) async fn readopt(
    registry: &ChannelMailboxRegistry,
    persistence: &QueuePersistenceContext,
    namespace: &str,
) -> Vec<Result<ChannelId, String>> {
    let (provider, token_hash) = (&persistence.provider, persistence.token_hash.as_str());
    let mut adopted = Vec::new();
    for step in plan(persistence, namespace) {
        let Step::Adopt(channel) = step else {
            continue;
        };
        let handle = registry.handle(channel);
        if handle.v2_request(|slot| Ok(slot.is_some())).await == Ok(true) {
            continue;
        }
        let Some(path) = pending_queue_file_path(provider, token_hash, channel) else {
            continue;
        };
        let result = handle.v2_adopt(path, namespace.into()).await;
        adopted.push(result.map(|()| channel));
    }
    adopted
}

impl Owner {
    /// The committed snapshot a checked purge compares, namespace included.
    pub(crate) fn fingerprint(&self) -> Result<String, String> {
        serde_json::to_string(&self.snapshot).map_err(|e| e.to_string())
    }
}

impl ChannelMailboxState {
    pub(crate) fn v2_len(&self) -> Option<usize> {
        (self.queued_head.v2.as_ref()).map(|owner| owner.snapshot.items.len())
    }

    /// A queued V2 input other than `claimant`; retained inputs never hold the slot.
    pub(crate) fn v2_queued_ahead_of(&self, claimant: MessageId) -> bool {
        let Some(owner) = &self.queued_head.v2 else {
            return false;
        };
        let other = |id: &u64| *id != claimant.get();
        (owner.snapshot.items.iter())
            .any(|item| other(&item.message_id) || item.source_message_ids.iter().any(other))
    }

    /// Discards the untried items only while the owner still holds `expected`; the attempted head,
    /// retained inputs and terminals stay, and the file is replaced only through the owner's CAS.
    #[cfg(unix)]
    pub(crate) fn v2_purge_if_unchanged(
        &mut self,
        expected: &str,
    ) -> Result<Vec<PendingQueueItem>, PurgeStop> {
        let owner =
            (self.queued_head.v2.as_mut()).ok_or(PurgeStop::Refused("no V2 owner".into()))?;
        if owner.fingerprint().map_err(PurgeStop::Refused)? != expected {
            return Err(PurgeStop::Refused(
                "the V2 queue or namespace changed since the capture".into(),
            ));
        }
        let mut next = owner.snapshot.clone();
        let first = usize::from(next.head_injection.is_some());
        let drained: Vec<PendingQueueItem> =
            next.items.drain(first.min(next.items.len())..).collect();
        match owner.commit(next) {
            Ok(()) => Ok(drained),
            Err(error) if owner.pending.is_some() => Err(PurgeStop::SaveUnknown(error)),
            Err(error) => Err(PurgeStop::NotSaved(error)),
        }
    }
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;
