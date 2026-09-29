//! Claim-epoch CAS: a catch-up enqueue replays the claim observation of the snapshot
//! that classified it, so the actor refuses work a later claim may have spoken for.

use std::collections::VecDeque;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use poise::serenity_prelude::{ChannelId, MessageId};

use super::active_source_dedup::active_turn_enqueue_refusal;
use super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxSnapshot, ChannelMailboxState,
    EnqueueInterventionResult, EnqueueRefusalReason, Intervention, QueuePersistenceContext,
};

/// Claims retained after an observation. More claims than this since a
/// snapshot refuses its enqueue rather than guessing what was evicted.
pub(crate) const RECENT_CLAIMS_CAP: usize = 64;

/// Incarnation 0 is never minted, so a defaulted observation always refuses.
static NEXT_INCARNATION: AtomicU64 = AtomicU64::new(1);
/// Bumped when a purge tombstones a channel's actor, so a no-actor snapshot
/// does not trust a fresh actor when an intermediate one may have claimed.
static PURGE_EPOCHS: LazyLock<DashMap<ChannelId, u64>> = LazyLock::new(DashMap::new);

fn purge_epoch(channel_id: ChannelId) -> u64 {
    PURGE_EPOCHS.get(&channel_id).map_or(0, |epoch| *epoch)
}

pub(super) fn note_purge(channel_id: ChannelId) {
    *PURGE_EPOCHS.entry(channel_id).or_insert(0) += 1;
}

/// What a snapshot proved about the claims made so far on its channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimObservation {
    /// The snapshot read this actor after `claim_seq` claims.
    Actor { incarnation: u64, claim_seq: u64 },
    /// No actor existed; any actor spawned later saw every claim since.
    NoActor { minted: u64, purge_epoch: u64 },
}

impl Default for ClaimObservation {
    /// Proves nothing (actor unreachable, synthetic snapshot): always refuses.
    fn default() -> Self {
        Self::Actor {
            incarnation: 0,
            claim_seq: 0,
        }
    }
}

impl ClaimObservation {
    fn no_actor(channel_id: ChannelId) -> Self {
        // Epoch first: a purge between the two reads can only make it refuse.
        let purge_epoch = purge_epoch(channel_id);
        let minted = NEXT_INCARNATION.load(Ordering::SeqCst);
        Self::NoActor {
            minted,
            purge_epoch,
        }
    }
}

impl ChannelMailboxSnapshot {
    /// The snapshot of a channel with no registered actor.
    pub(crate) fn no_actor(channel_id: ChannelId) -> Self {
        Self {
            claim_observation: ClaimObservation::no_actor(channel_id),
            ..Self::default()
        }
    }
}

/// Per-actor claim history: each claim bumps `seq` and records the primary +
/// absorbed ids, so a rebind or re-mint is covered like a first claim.
pub(super) struct ClaimLog {
    incarnation: u64,
    purge_epoch: u64,
    seq: u64,
    recent: VecDeque<(u64, Vec<MessageId>)>,
}

impl Default for ClaimLog {
    /// An unbound log (unit-test state) behaves as a purged channel's actor.
    fn default() -> Self {
        Self {
            incarnation: NEXT_INCARNATION.fetch_add(1, Ordering::SeqCst),
            purge_epoch: u64::MAX,
            seq: 0,
            recent: VecDeque::new(),
        }
    }
}

impl ClaimLog {
    /// The log of a freshly spawned actor for `channel_id`.
    pub(super) fn spawned(channel_id: ChannelId) -> Self {
        let purge_epoch = purge_epoch(channel_id);
        Self {
            purge_epoch,
            ..Self::default()
        }
    }

    pub(super) fn observation(&self) -> ClaimObservation {
        ClaimObservation::Actor {
            incarnation: self.incarnation,
            claim_seq: self.seq,
        }
    }

    fn record(&mut self, ids: Vec<MessageId>) {
        self.seq += 1;
        self.recent.push_back((self.seq, ids));
        if self.recent.len() > RECENT_CLAIMS_CAP {
            self.recent.pop_front();
        }
    }

    /// Whether a claim after `observed` may have spoken for any of `sources`.
    fn claimed_since(&self, observed: ClaimObservation, sources: &[MessageId]) -> bool {
        let since = match observed {
            ClaimObservation::Actor {
                incarnation,
                claim_seq,
            } if incarnation == self.incarnation => claim_seq,
            ClaimObservation::NoActor {
                minted,
                purge_epoch,
            } if self.incarnation >= minted && self.purge_epoch == purge_epoch => 0,
            _ => return true,
        };
        if since >= self.seq {
            return since > self.seq;
        }
        let Some(&(oldest, _)) = self.recent.front() else {
            return true;
        };
        // Past the retained window an evicted claim may overlap, so refuse; the retry reclassifies
        // from a fresh snapshot, so this only delays while >CAP claims land before each enqueue.
        oldest > since + 1
            || (self.recent.iter())
                .any(|(seq, ids)| *seq > since && ids.iter().any(|id| sources.contains(id)))
    }
}

impl ChannelMailboxState {
    /// Called after every claim that sets `active_user_message_id`; records the set
    /// `AbsorbedByActiveTurn` refuses, so a restored absorbed set must precede it.
    pub(super) fn record_claim(&mut self) {
        let mut ids: Vec<MessageId> = self.active_user_message_id.into_iter().collect();
        ids.extend(self.active_absorbed_source_ids.iter().copied());
        self.claim_log.record(ids);
    }

    /// Pre-hydrate refusal: active-turn duplicates first, then the claim CAS.
    pub(super) fn enqueue_refusal(
        &self,
        intervention: &Intervention,
        observed: Option<ClaimObservation>,
    ) -> Option<EnqueueRefusalReason> {
        active_turn_enqueue_refusal(self, intervention).or_else(|| {
            let sources = &intervention.source_message_ids;
            observed
                .filter(|observed| self.claim_log.claimed_since(*observed, sources))
                .map(|_| EnqueueRefusalReason::ClaimedSinceObservation)
        })
    }
}

impl ChannelMailboxHandle {
    /// Live-intake enqueue (no CAS); production goes through the registry.
    #[cfg(test)]
    pub(crate) async fn enqueue(
        &self,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
    ) -> EnqueueInterventionResult {
        self.enqueue_observed(intervention, persistence, None).await
    }

    /// `observed` is the claim observation of the snapshot that classified the
    /// intervention; `None` (live intake) skips the CAS.
    pub(crate) async fn enqueue_observed(
        &self,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
        observed: Option<ClaimObservation>,
    ) -> EnqueueInterventionResult {
        self.request(|reply| ChannelMailboxMsg::Enqueue {
            intervention,
            persistence,
            observed,
            reply,
        })
        .await
        .unwrap_or_else(|_| {
            EnqueueInterventionResult::refused(EnqueueRefusalReason::ActorUnreachable, Vec::new())
        })
    }
}
