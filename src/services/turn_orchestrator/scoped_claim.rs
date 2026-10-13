#![cfg(test)]
//! A scoped restore's claim of its message in one mailbox step: the queue without that message,
//! one debt per exit, the actor and what it still owes land together, or nothing changes.

use super::installed_actor_handoff::Installation;
use super::pending_queue_persistence::{
    ExitWithDebt, PreparedQueueExitTransition, QueueExitCommitError, QueueExitPrepareError,
    commit_queue_exit,
};
use super::*;
use crate::services::discord::input_runtime::fence::Failure;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;

/// Exit feedback the installed actor owes; a reservation is not a durable acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingFeedback {
    Owed,
    Reserved,
}

/// Whether the installed actor's driver was scheduled; none of these acknowledges a debt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingDriver {
    NotScheduled,
    Reserved,
    Ready,
}

/// What the actor a scoped claim installed still owes, held by that actor's own state.
#[derive(Clone)]
pub(crate) struct InstalledPending {
    pub(crate) token: Weak<CancelToken>,
    pub(crate) episode: String,
    pub(crate) birth: String,
    pub(crate) owner: UserId,
    pub(crate) message: MessageId,
    pub(crate) generation: u64,
    pub(crate) current: Arc<AtomicU64>,
    pub(crate) feedback: PendingFeedback,
    pub(crate) driver: PendingDriver,
    pub(crate) exits: Vec<ExitWithDebt>,
}

#[derive(Debug)]
pub(crate) enum ScopedClaimRefusal {
    ActorClosed,
    InputFence(Failure),
    WrongChannel,
    Adopt(String),
    ForeignActor,
    PendingHeld,
    ReleasedEpisode,
    Stale,
    HeadBlocks,
    Yields,
    CallerLeft,
    Prepare(String),
    NotCommitted(String),
    Indeterminate(String),
}

impl ScopedClaimRefusal {
    /// The error an adopt, prepare or commit refusal carries.
    pub(crate) fn detail(&self) -> Option<&str> {
        match self {
            Self::Adopt(error)
            | Self::Prepare(error)
            | Self::NotCommitted(error)
            | Self::Indeterminate(error) => Some(error),
            _ => None,
        }
    }
}

pub(crate) enum ScopedClaimOutcome {
    /// The actor and its pending debts were installed; the event ids recorded on disk.
    Installed(Vec<String>),
    /// The exact actor already holds the slot; its pending record follows the newer generation.
    Inherited,
    Refused(ScopedClaimRefusal),
}

pub(crate) enum ScopedMsg {
    Claim {
        proof: Installation,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<ScopedClaimOutcome>,
    },
    Pending {
        reply: oneshot::Sender<Option<InstalledPending>>,
    },
}

impl ScopedMsg {
    pub(super) fn persistence(&self) -> Option<&QueuePersistenceContext> {
        match self {
            Self::Claim { persistence, .. } => Some(persistence),
            Self::Pending { .. } => None,
        }
    }

    pub(super) fn is_read(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }

    pub(super) fn refuse(self, refusal: ScopedClaimRefusal) {
        if let Self::Claim { reply, .. } = self {
            let _ = reply.send(ScopedClaimOutcome::Refused(refusal));
        }
    }
}

impl ChannelMailboxHandle {
    pub(crate) async fn claim_scoped(
        &self,
        proof: Installation,
        persistence: QueuePersistenceContext,
    ) -> Option<ScopedClaimOutcome> {
        let build = |reply| {
            ChannelMailboxMsg::ScopedClaim(ScopedMsg::Claim {
                proof,
                persistence,
                reply,
            })
        };
        self.request(build).await.ok()
    }

    pub(crate) async fn installed_pending(&self) -> Option<InstalledPending> {
        let build = |reply| ChannelMailboxMsg::ScopedClaim(ScopedMsg::Pending { reply });
        self.request(build).await.ok().flatten()
    }
}

pub(super) fn step(state: &mut ChannelMailboxState, channel: ChannelId, msg: ScopedMsg) {
    let (proof, persistence, reply) = match msg {
        ScopedMsg::Pending { reply } => {
            let _ = reply.send(state.installed_pending.clone());
            return;
        }
        ScopedMsg::Claim {
            proof,
            persistence,
            reply,
        } => (proof, persistence, reply),
    };
    if reply.is_closed() {
        return;
    }
    let outcome = claim(state, channel, &proof, &persistence, &reply)
        .unwrap_or_else(ScopedClaimOutcome::Refused);
    let _ = reply.send(outcome);
}

/// Same birth, owner and message on the same live token; a new episode needs a newer
/// generation, and the generation is still the current one.
fn exact(pending: &InstalledPending, proof: &Installation) -> bool {
    let token = pending.token.upgrade();
    token.is_some_and(|token| Arc::ptr_eq(&token, &proof.token))
        && (pending.birth == proof.birth)
        && (pending.owner, pending.message) == (proof.owner, proof.message)
        && (proof.episode == pending.episode || proof.generation > pending.generation)
        && proof.generation >= pending.generation
        && proof.current.load(Ordering::Acquire) == proof.generation
}

fn claim(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    proof: &Installation,
    persistence: &QueuePersistenceContext,
    reply: &oneshot::Sender<ScopedClaimOutcome>,
) -> Result<ScopedClaimOutcome, ScopedClaimRefusal> {
    use ScopedClaimRefusal as R;
    if channel != proof.channel {
        return Err(R::WrongChannel);
    }
    queued_head::adopt(state, channel, persistence).map_err(R::Adopt)?;
    if let Some(active) = &state.cancel_token {
        let holds = Arc::ptr_eq(active, &proof.token)
            && state.active_request_owner == Some(proof.owner)
            && state.active_user_message_id == Some(proof.message);
        let pending = state.installed_pending.as_mut();
        let Some(pending) = pending.filter(|pending| holds && exact(pending, proof)) else {
            return Err(R::ForeignActor);
        };
        pending.episode = proof.episode.clone();
        pending.generation = proof.generation;
        pending.current = proof.current.clone();
        return Ok(ScopedClaimOutcome::Inherited);
    }
    if state.installed_pending.is_some() {
        return Err(R::PendingHeld);
    }
    // The fence judges the token's nonce, which a policy close of this episode released.
    if (state.remint_fence).refuses(proof.message, proof.token.turn_nonce()) {
        return Err(R::ReleasedEpisode);
    }
    if proof.token.cancelled.load(Ordering::Acquire)
        || proof.current.load(Ordering::Acquire) != proof.generation
    {
        return Err(R::Stale);
    }
    if queued_head::blocks_claims(state, channel, Some(persistence)) {
        return Err(R::HeadBlocks);
    }
    let (kind, order) = (ActiveTurnKind::UserOrAgent, TurnAdmissionOrder::Immediate);
    if claim_yields(state, kind, proof.message, order) {
        return Err(R::Yields);
    }
    let (queue, message, episode) = (&state.intervention_queue, proof.message, &proof.episode);
    let transition =
        PreparedQueueExitTransition::from_scoped_purge(queue, message, episode, channel)
            .map_err(|error: QueueExitPrepareError| R::Prepare(error.0))?;
    if reply.is_closed() {
        return Err(R::CallerLeft);
    }
    if !transition.exits().is_empty() {
        commit_queue_exit(state, channel, persistence, &transition).map_err(
            |error| match error {
                QueueExitCommitError::NotCommitted(error) => R::NotCommitted(error),
                QueueExitCommitError::Indeterminate(error) => R::Indeterminate(error),
            },
        )?;
    }
    install(state, channel, proof);
    let exits = transition.exits().to_vec();
    let recorded = exits
        .iter()
        .map(|exit| exit.debt.event_id.clone())
        .collect();
    state.installed_pending = Some(InstalledPending {
        token: Arc::downgrade(&proof.token),
        episode: proof.episode.clone(),
        birth: proof.birth.clone(),
        owner: proof.owner,
        message: proof.message,
        generation: proof.generation,
        current: proof.current.clone(),
        feedback: PendingFeedback::Owed,
        driver: PendingDriver::NotScheduled,
        exits,
    });
    Ok(ScopedClaimOutcome::Installed(recorded))
}

/// The slot fields a started user claim sets.
fn install(state: &mut ChannelMailboxState, channel: ChannelId, proof: &Installation) {
    let token = &proof.token;
    reset_turn_finished_signal(channel);
    state.active_turn_nonce = token.turn_nonce().map(str::to_owned);
    (state.remint_fence).note_started(Some(proof.message), token.turn_nonce());
    state.cancel_token = Some(token.clone());
    state.active_request_owner = Some(proof.owner);
    state.active_user_message_id = Some(proof.message);
    state.active_turn_kind = ActiveTurnKind::UserOrAgent;
    settle_pending_dispatch_on_claim(state, channel, proof.message);
    state.record_claim();
    state.recovery_started_at = None;
    state.turn_started_at = Some(Utc::now());
    state.turn_started_instant = Some(Instant::now());
}

#[cfg(test)]
#[path = "scoped_claim_tests.rs"]
mod tests;
