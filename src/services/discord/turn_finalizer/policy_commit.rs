//! The finalizer half of a policy close's continuation. The ledger entry owns one commit per exact
//! episode until its tail runs, so a lost reply or a caught panic resumes that same tail.

use std::sync::{Mutex, PoisonError};

use super::super::*;
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use crate::services::turn_orchestrator::policy_close::PolicyMailboxOutcome as Mailbox;
use crate::services::turn_orchestrator::policy_close::tail::{
    PolicyCommitTicket, PolicyTailDecision, PolicyTailSkip, TailCapture,
};

type DeferredVoice = Arc<tokio::sync::Mutex<crate::voice::barge_in::DeferredBargeInBuffer>>;

/// One exact close as the ledger holds it: the ticket, the actor that commits it and the channel
/// state its granted claim took. Every strong ref goes once the close finalizes.
pub(in crate::services::discord::turn_finalizer) struct PolicyCommit {
    ticket: Arc<PolicyCommitTicket>,
    actor: Mutex<Option<ChannelMailboxHandle>>,
    plan: Arc<Mutex<Option<TailPlan>>>,
}

/// What the granted claim took from the channel inside the actor, before any successor could.
#[derive(Default)]
struct TailPlan {
    parents: Vec<ChannelId>,
    voice: Option<DeferredVoice>,
}

impl PolicyCommit {
    pub(super) fn new(intent: &super::super::episode::PolicyCloseIntent) -> Arc<Self> {
        let request = &intent.request;
        Arc::new(Self {
            ticket: PolicyCommitTicket::new(request.user_message_id, &request.turn_nonce),
            actor: Mutex::new(Some(intent.mailbox.clone())),
            plan: Arc::default(),
        })
    }

    pub(super) fn ticket(&self) -> Arc<PolicyCommitTicket> {
        self.ticket.clone()
    }

    fn actor(&self) -> Option<ChannelMailboxHandle> {
        self.actor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// An unsealed ticket goes to the actor this attempt's bridge captured; one that seals it
    /// there is the actor its tail claims.
    fn submit_to(&self, mailbox: ChannelMailboxHandle) -> ChannelMailboxHandle {
        let mut actor = self.actor.lock().unwrap_or_else(PoisonError::into_inner);
        actor.insert(mailbox).clone()
    }

    /// Drops the actor and plan; the ledger drops the commit itself in the same step.
    pub(super) fn release(&self) {
        *self.actor.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.plan.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Runs inside the granted actor step: takes the episode's thread parents and deferred voice,
    /// and drops its role override when nothing will run after it.
    fn capture(&self, shared: &Arc<SharedData>, channel: ChannelId, voice: bool) -> TailCapture {
        let (shared, plan) = (shared.clone(), self.plan.clone());
        Box::new(move |queued| {
            let parents = super::super::cleanup::collect_and_clear_thread_parents(&shared, channel);
            let voice = voice
                .then(|| shared.voice_barge_in.detach_deferred(channel))
                .flatten();
            let silent = (voice.as_ref())
                .is_none_or(|buffer| buffer.try_lock().is_ok_and(|buffer| buffer.is_empty()));
            if !queued && silent {
                shared.dispatch.role_overrides.remove(&channel);
            }
            let taken = TailPlan { parents, voice };
            *plan.lock().unwrap_or_else(PoisonError::into_inner) = Some(taken);
        })
    }
}

/// The mailbox commit unless the ticket already holds one, then the tail. Only a sealed ticket
/// proves a commit: neither a lost reply nor an empty slot does.
pub(super) async fn settle(
    intent: super::super::episode::PolicyCloseIntent,
    commit: &PolicyCommit,
    shared: &Arc<SharedData>,
) -> (PolicyCloseOutcome, Option<FinalizeOutcome>) {
    let super::super::episode::PolicyCloseIntent {
        key,
        provider,
        mailbox,
        request,
        snapshot,
    } = intent;
    let trace = request.trace.clone();
    #[cfg(test)]
    trace.note_ticket(&commit.ticket);
    let reply = match commit.ticket.sealed() {
        // A sealed ticket resumes its tail: this request commits nothing in the mailbox.
        true => {
            trace.note(Point::MailboxCommitted, 0);
            None
        }
        false => {
            let actor = commit.submit_to(mailbox);
            Some(actor.commit_policy_close(request).await)
        }
    };
    #[cfg(test)]
    use crate::services::turn_orchestrator::policy_close::test_hooks::{Branch, Stage};
    if !commit.ticket.sealed() {
        #[cfg(test)]
        trace.not_run(Branch::NotCommitted);
        trace.note(Point::QueueExit, 0);
        let outcome = match reply {
            Some(Err(stage)) => PolicyCloseOutcome::Unconfirmed(stage),
            // A commit seals its ticket before it answers, so an unsealed one confirms nothing.
            None | Some(Ok(Mailbox::Committed(_))) => {
                PolicyCloseOutcome::Unconfirmed(PolicyStage::MailboxReply)
            }
            // An exactly released episode with its receipt now gone is a release already confirmed.
            Some(Ok(Mailbox::Duplicate | Mailbox::Reconciled)) => {
                PolicyCloseOutcome::DuplicateConfirmed
            }
            Some(Ok(Mailbox::Superseded)) => PolicyCloseOutcome::Superseded,
            Some(Ok(Mailbox::Retained(reason))) => PolicyCloseOutcome::Retained(reason),
        };
        return (outcome, None);
    }
    // The tail has not run its queue exit yet; a zero here is a reading, not a missing point.
    trace.note(Point::QueueExit, 0);
    // Test seam between the commit and its tail, which has consumed nothing yet.
    #[cfg(test)]
    {
        trace.not_run(Branch::BeforeFinalize);
        trace.fire(Stage::BeforeFinalize);
    }
    trace.note(Point::FinalizeEntered, 1);
    let released = run_tail(commit, key, &provider, &snapshot, shared, &trace).await;
    trace.note(Point::FinalizeReturned, 1);
    (PolicyCloseOutcome::Settled, released)
}

/// The episode's own cleanup always; the channel's only when the claim was granted. Returns the
/// release a granted tail publishes.
async fn run_tail(
    commit: &PolicyCommit,
    key: TurnKey,
    provider: &ProviderKind,
    snapshot: &SyntheticClaimSnapshot,
    shared: &Arc<SharedData>,
    trace: &crate::services::turn_orchestrator::policy_close::Trace,
) -> Option<FinalizeOutcome> {
    let channel = key.channel_id;
    let decision = match (commit.ticket.decision(), commit.actor()) {
        (Some(decided), _) => decided,
        (None, Some(actor)) => {
            let voice = shared.voice_barge_in.deferred_drainable();
            let capture = commit.capture(shared, channel, voice);
            actor.claim_policy_tail(commit.ticket(), capture).await
        }
        (None, None) => (commit.ticket).decide(PolicyTailDecision::Skipped(
            PolicyTailSkip::ActorUnreachable,
        )),
    };
    let finish = commit.ticket.finish();
    let exits = commit.ticket.take_queue_exits();
    if let Some(exits) = exits.as_ref() {
        super::super::super::apply_queue_exit_feedback(shared, channel, exits).await;
    }
    trace.note(Point::QueueExit, u64::from(exits.is_some()));
    #[cfg(test)]
    {
        trace.note_tail(decision);
        super::super::test_panic_hook::maybe_panic_in_finalize();
    }
    let removed_token = finish.as_ref().and_then(|f| f.removed_token.clone());
    if let Some(token) = removed_token.as_ref() {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let event = TerminalEvent::Cancel;
    super::super::cleanup::ensure_synthetic_claim_marker_before_clear(
        key,
        provider,
        Some(snapshot),
    );
    super::super::cleanup::enqueue_terminal_status_panel_reconcile(
        key,
        provider,
        &event,
        Some(snapshot),
        shared.as_ref(),
    );
    let skip_reaction =
        super::super::cleanup::relay_ownership_only_for_finalize(key, provider, Some(snapshot));
    let relay_owner =
        super::super::cleanup::relay_owner_kind_for_finalize(key, provider, Some(snapshot));
    let ctx = FinalizeContext {
        clear_inflight: false,
        ..FinalizeContext::bridge()
    };
    super::super::cleanup::finalized_reaction_lifecycle(
        key,
        &event,
        ctx,
        shared,
        "finalized",
        skip_reaction,
        relay_owner,
    );
    if decision != PolicyTailDecision::Granted {
        return None;
    }
    let plan = commit
        .plan
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let plan = plan.unwrap_or_default();
    super::super::cleanup::kickoff_thread_parents_after_finalize(shared, provider, plan.parents);
    let voice_enqueued = match plan.voice {
        Some(buffer) => {
            (shared.voice_barge_in)
                .drain_detached(shared, provider, channel, buffer)
                .await
        }
        None => false,
    };
    if removed_token.is_some() {
        super::super::super::turn_completion_events::publish_turn_completion_event(
            shared,
            super::super::super::turn_completion_events::TurnCompletionEvent::mailbox_released(
                channel,
                Some(key.user_msg_id),
            ),
        );
    }
    let has_pending = finish.as_ref().is_some_and(|f| f.has_pending) || voice_enqueued;
    super::super::cleanup::rearm_queue_backstop_after_mailbox_release(
        shared,
        provider,
        channel,
        has_pending,
        "turn_finalizer_mailbox_release",
    )
    .await;
    Some(FinalizeOutcome::Finalized {
        removed_token,
        has_pending,
        mailbox_online: finish.is_none_or(|f| f.mailbox_online),
    })
}
