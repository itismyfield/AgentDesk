//! The mailbox half of a policy close's continuation. The commit seals its ticket before the actor
//! answers, and one later step on the same actor decides whether the close's channel tail may run.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use poise::serenity_prelude::MessageId;
use tokio::sync::oneshot;

use super::super::claim_observation::ClaimObservation;
use super::super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxState, FinishTurnResult,
    InterventionMode, QueueExitEvent, RecoveryDoneSignal,
};

/// One exact close's commit record: sealed once by the mailbox step that committed it, then
/// decided once by the tail claim. The finalizer's ledger entry owns it until the close finalizes.
pub(crate) struct PolicyCommitTicket {
    user_message_id: MessageId,
    turn_nonce: String,
    seal: OnceLock<PolicyCommitSeal>,
    decision: OnceLock<PolicyTailDecision>,
}

struct PolicyCommitSeal {
    /// The commit's finish without its queue exits; the token goes when the ledger drops the ticket.
    finish: FinishTurnResult,
    /// Taken by the first tail that gets this far, so the queue feedback runs at most once.
    exits: Mutex<Option<Vec<QueueExitEvent>>>,
    stamp: OwnerStamp,
}

/// The mailbox right after the commit: any claim, slot, dispatch or injection change since means a
/// successor owns the channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OwnerStamp {
    claim: ClaimObservation,
    active_message: Option<MessageId>,
    token: bool,
    pending_dispatch: Option<MessageId>,
    dispatch_lease: bool,
    injection: bool,
}

impl OwnerStamp {
    fn of(state: &mut ChannelMailboxState) -> Self {
        Self {
            claim: state.claim_log.observation(),
            active_message: state.active_user_message_id,
            token: state.cancel_token.is_some(),
            pending_dispatch: state.pending_user_dispatch,
            dispatch_lease: state.pending_user_dispatch_lease.is_some(),
            injection: super::super::injected_inputs::holds_order(state),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyTailDecision {
    Granted,
    Skipped(PolicyTailSkip),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyTailSkip {
    SuccessorObserved,
    ActorClosed,
    ActorUnreachable,
}

impl PolicyCommitTicket {
    pub(crate) fn new(user_message_id: MessageId, turn_nonce: &str) -> Arc<Self> {
        Arc::new(Self {
            user_message_id,
            turn_nonce: turn_nonce.to_owned(),
            seal: OnceLock::new(),
            decision: OnceLock::new(),
        })
    }

    pub(crate) fn names(&self, user_message_id: MessageId, turn_nonce: &str) -> bool {
        self.user_message_id == user_message_id && self.turn_nonce == turn_nonce
    }

    pub(crate) fn sealed(&self) -> bool {
        self.seal.get().is_some()
    }

    /// The commit's finish, without its queue exits.
    pub(crate) fn finish(&self) -> Option<FinishTurnResult> {
        self.seal.get().map(|seal| without_exits(&seal.finish))
    }

    /// The commit's queue exits, handed out once.
    pub(crate) fn take_queue_exits(&self) -> Option<Vec<QueueExitEvent>> {
        let seal = self.seal.get()?;
        seal.exits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    pub(crate) fn decision(&self) -> Option<PolicyTailDecision> {
        self.decision.get().copied()
    }

    /// Installs `decision` unless one is already in; returns the one that holds.
    pub(crate) fn decide(&self, decision: PolicyTailDecision) -> PolicyTailDecision {
        *self.decision.get_or_init(|| decision)
    }

    /// Seals the commit inside the committing step, before its reply: the post-commit stamp and
    /// the finish; the reply keeps a copy without the queue exits, which only the ticket hands out.
    pub(super) fn seal(
        &self,
        state: &mut ChannelMailboxState,
        mut finish: FinishTurnResult,
    ) -> FinishTurnResult {
        let exits = std::mem::take(&mut finish.queue_exit_events);
        let reply = without_exits(&finish);
        let stamp = OwnerStamp::of(state);
        let _ = self.seal.set(PolicyCommitSeal {
            finish,
            exits: Mutex::new(Some(exits)),
            stamp,
        });
        reply
    }
}

fn without_exits(finish: &FinishTurnResult) -> FinishTurnResult {
    FinishTurnResult {
        removed_token: finish.removed_token.clone(),
        has_pending: finish.has_pending,
        mailbox_online: finish.mailbox_online,
        queue_exit_events: Vec::new(),
        persistence_error: finish.persistence_error.clone(),
    }
}

/// Runs inside the actor only when the tail is granted: takes the episode's channel state while
/// no successor can claim; told whether soft input is queued.
pub(crate) type TailCapture = Box<dyn FnOnce(bool) + Send>;

/// One tail claim on the actor that sealed `ticket`.
pub(crate) struct TailClaim {
    ticket: Arc<PolicyCommitTicket>,
    capture: TailCapture,
    reply: oneshot::Sender<PolicyTailDecision>,
}

impl TailClaim {
    /// A purged actor holds nothing of the episode's channel: it skips and changes nothing.
    pub(in crate::services::turn_orchestrator) fn closed(self) -> &'static str {
        let skip = PolicyTailDecision::Skipped(PolicyTailSkip::ActorClosed);
        let _ = self.reply.send(self.ticket.decide(skip));
        "ClaimPolicyTail"
    }
}

/// The actor step: grant only while the mailbox is exactly as the commit left it. A ticket already
/// decided answers again without capturing twice.
pub(in crate::services::turn_orchestrator) fn answer(
    state: &mut ChannelMailboxState,
    claim: TailClaim,
    recovery_done: &RecoveryDoneSignal,
) {
    let TailClaim {
        ticket,
        capture,
        reply,
    } = claim;
    let decision = match ticket.decision() {
        Some(decided) => decided,
        None => {
            let current = OwnerStamp::of(state);
            if ticket.seal.get().is_some_and(|seal| seal.stamp == current) {
                recovery_done.mark_done();
                let queued = (state.intervention_queue.iter())
                    .any(|item| item.mode == InterventionMode::Soft);
                capture(queued);
                ticket.decide(PolicyTailDecision::Granted)
            } else {
                ticket.decide(PolicyTailDecision::Skipped(
                    PolicyTailSkip::SuccessorObserved,
                ))
            }
        }
    };
    let _ = reply.send(decision);
}

impl ChannelMailboxHandle {
    /// Claims the tail on this captured actor. An actor that cannot be reached skips the tail.
    pub(crate) async fn claim_policy_tail(
        &self,
        ticket: Arc<PolicyCommitTicket>,
        capture: TailCapture,
    ) -> PolicyTailDecision {
        let unreachable = PolicyTailDecision::Skipped(PolicyTailSkip::ActorUnreachable);
        let (reply, answer) = oneshot::channel();
        let claim = TailClaim {
            ticket: ticket.clone(),
            capture,
            reply,
        };
        let permit = crate::services::discord::input_runtime::fence::effect::current();
        if (self.sender)
            .send((ChannelMailboxMsg::ClaimPolicyTail(claim), permit))
            .is_err()
        {
            return ticket.decide(unreachable);
        }
        match answer.await {
            Ok(decision) => decision,
            Err(_) => ticket.decide(unreachable),
        }
    }
}
