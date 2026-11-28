//! The mailbox half of a before-start policy close: one actor step claims the exact episode, clears
//! its receipt without waiting, then releases, debits, fences and witnesses before reading on.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::{ChannelMailboxState, FinishTurnResult, InterventionMode, QueuePersistenceContext};
use crate::services::provider::CancelToken;

#[path = "policy_tail.rs"]
pub(crate) mod tail;

/// Test seam at a typed stage of the close, on the request's own probe; gone outside tests.
macro_rules! at {
    ($request:expr, $stage:ident) => {
        #[cfg(any(test, feature = "test-support"))]
        $request.trace.fire(test_hooks::Stage::$stage);
    };
}

/// The close's one durable effect: the non-blocking exact clear of its acked receipt row.
pub(crate) trait ControlCustody: Send + Sync {
    fn input(&self) -> &str;
    fn owns(
        &self,
        channel: ChannelId,
        actor: Option<&Arc<crate::services::provider::CancelToken>>,
        message: Option<MessageId>,
        nonce: Option<&str>,
    ) -> bool;
    fn admit(&self, operator: bool) -> Result<(), String>;
}

pub(super) struct InstallCustody {
    pub(crate) custody: Option<Arc<dyn ControlCustody>>,
    pub(crate) remove: Option<String>,
    pub(crate) reply: tokio::sync::oneshot::Sender<bool>,
}

impl super::ChannelMailboxHandle {
    pub(crate) async fn install_control_custody(&self, custody: Arc<dyn ControlCustody>) -> bool {
        self.request(|reply| {
            super::ChannelMailboxMsg::ControlCustody(Box::new(InstallCustody {
                custody: Some(custody),
                remove: None,
                reply,
            }))
        })
        .await
        .unwrap_or(false)
    }
}

impl super::ChannelMailboxHandle {
    pub(crate) async fn remove_control_custody(&self, input: String) -> bool {
        self.request(|reply| {
            super::ChannelMailboxMsg::ControlCustody(Box::new(InstallCustody {
                custody: None,
                remove: Some(input),
                reply,
            }))
        })
        .await
        .unwrap_or(false)
    }
}

pub(super) fn admit_control(
    state: &ChannelMailboxState,
    channel: ChannelId,
    operator: bool,
) -> Result<(), String> {
    if let Some(custody) = &state.control_custody {
        if !custody.owns(
            channel,
            state.cancel_token.as_ref(),
            state.active_user_message_id,
            state.active_turn_nonce.as_deref(),
        ) {
            return Err("native control custody superseded".into());
        }
        custody.admit(operator)?;
    }
    Ok(())
}

pub(crate) trait PolicyReceipt: Send {
    fn channel(&self) -> u64;
    fn try_clear(&self) -> PolicyClear;
    fn try_clear_admitted(&self, admission: &dyn Fn() -> Result<(), String>) -> PolicyClear {
        let _ = admission;
        PolicyClear::Skipped
    }
}

/// How the receipt clear ended; only `Cleared` lets the step release anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyClear {
    Cleared,
    LockBusy,
    Missing,
    Mismatch,
    Io,
    Skipped,
}

/// Why a close released nothing; the turn stays held with its receipt.
#[derive(Debug)]
pub(crate) enum PolicyRetain {
    Clear(PolicyClear),
    ActorClosed,
    Panicked,
}

/// Where a close lost sight of its result; nothing after that point is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyStage {
    FinalizerSend,
    FinalizerAck,
    MailboxSend,
    MailboxReply,
    FinalizerPanic,
}

/// The step's answer. `Committed` carries the removed token and its debit is already done.
pub(crate) enum PolicyMailboxOutcome {
    Committed(FinishTurnResult),
    Duplicate,
    /// The episode was already released exactly; only its receipt was cleared.
    Reconciled,
    Superseded,
    Retained(PolicyRetain),
}

pub(crate) struct PolicyCloseRequest {
    pub(crate) actor: Weak<CancelToken>,
    pub(crate) user_message_id: MessageId,
    pub(crate) turn_nonce: String,
    pub(crate) interrupt_generation: u64,
    pub(crate) observed_before: Instant,
    pub(crate) receipt: Box<dyn PolicyReceipt>,
    pub(crate) active: Arc<AtomicUsize>,
    pub(crate) persistence: QueuePersistenceContext,
    /// Set only by the finalizer, when its ledger already holds this exact episode as finalized.
    pub(crate) release_reconcile_allowed: bool,
    /// The finalizer's ledger-owned ticket; a commit seals it before the step answers.
    pub(crate) ticket: Option<Arc<tail::PolicyCommitTicket>>,
    pub(crate) trace: Trace,
}

/// Where a policy close was seen. The counts are process-wide totals, never per request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    SubmitAttempt,
    SendAccepted,
    Dequeued,
    MailboxCommitted,
    AckObserved,
    QueueExit,
    FinalizeEntered,
    FinalizeReturned,
}

const POINTS: usize = 8;
static COUNTS: [AtomicU64; POINTS] = [const { AtomicU64::new(0) }; POINTS];
static REGISTERED: AtomicU32 = AtomicU32::new(0);

/// A request's observation handle; in tests it also carries the request's own probe.
#[derive(Clone, Default)]
pub(crate) struct Trace {
    #[cfg(any(test, feature = "test-support"))]
    probe: Option<Arc<test_hooks::Probe>>,
}

impl Trace {
    /// Adds `count` at `point` and marks the point reached, so a zero there is a real reading.
    pub(crate) fn note(&self, point: Point, count: u64) {
        REGISTERED.fetch_or(1 << point as u32, Ordering::Relaxed);
        COUNTS[point as usize].fetch_add(count, Ordering::Relaxed);
        #[cfg(any(test, feature = "test-support"))]
        if let Some(probe) = &self.probe {
            probe.note(point, count);
        }
    }
}

/// The latest close this actor incarnation committed; one entry, so it never grows.
pub(super) struct PolicyWitness {
    user_message_id: MessageId,
    turn_nonce: String,
    actor: Weak<CancelToken>,
}

impl PolicyCloseRequest {
    fn witnessed(&self, witness: Option<&PolicyWitness>) -> bool {
        witness.is_some_and(|seen| {
            seen.user_message_id == self.user_message_id
                && seen.turn_nonce == self.turn_nonce
                && Weak::ptr_eq(&seen.actor, &self.actor)
        })
    }

    /// The exact episode still holds the slot: its token, message, nonce and interrupt
    /// generation, started before the bridge observed it.
    fn claims(&self, state: &ChannelMailboxState, channel_id: ChannelId) -> bool {
        let Some(actor) = self.actor.upgrade() else {
            return false;
        };
        !self.turn_nonce.is_empty()
            && self.receipt.channel() == channel_id.get()
            && state
                .cancel_token
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &actor))
            && state.active_user_message_id == Some(self.user_message_id)
            && state.active_turn_nonce.as_deref() == Some(self.turn_nonce.as_str())
            && actor.claude_interrupt_generation() == self.interrupt_generation
            && state
                .turn_started_instant
                .is_some_and(|started| started < self.observed_before)
    }

    /// Only the receipt may go: the finalizer finalized this exact episode and the shared fence,
    /// which notes every start into an empty slot, saw it released. A closed actor admits no more.
    pub(super) fn reconciles(&self, state: &ChannelMailboxState, channel_id: ChannelId) -> bool {
        self.release_reconcile_allowed
            && !self.turn_nonce.is_empty()
            && self.receipt.channel() == channel_id.get()
            && (state.remint_fence).refuses(self.user_message_id, Some(&self.turn_nonce))
    }
}

/// The mailbox arm of a close or its tail claim: a close runs the step and answers; a tail claim
/// is decided against the commit's stamp.
pub(super) fn answer(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    msg: super::ChannelMailboxMsg,
    recovery_done: &super::RecoveryDoneSignal,
) {
    let (request, reply) = match msg {
        super::ChannelMailboxMsg::PolicyClose { request, reply } => (*request, reply),
        super::ChannelMailboxMsg::ControlCustody(request) => {
            let accepted = if let Some(custody) = request.custody {
                let owns = custody.owns(
                    channel_id,
                    state.cancel_token.as_ref(),
                    state.active_user_message_id,
                    state.active_turn_nonce.as_deref(),
                );
                if owns {
                    state.control_custody = Some(custody);
                }
                owns
            } else if state
                .control_custody
                .as_ref()
                .is_some_and(|c| Some(c.input()) == request.remove.as_deref())
            {
                state.control_custody = None;
                true
            } else {
                true
            };
            let _ = request.reply.send(accepted);
            return;
        }
        super::ChannelMailboxMsg::ClaimPolicyTail(claim) => {
            return tail::answer(state, claim, recovery_done);
        }
        _ => unreachable!("only policy messages reach this arm"),
    };
    #[cfg(any(test, feature = "test-support"))]
    let lost = request.trace.lost(test_hooks::Loss::MailboxReply);
    let outcome = step(state, channel_id, request);
    // Test seam: the step ran and sealed; only its reply is lost.
    #[cfg(any(test, feature = "test-support"))]
    if lost {
        return;
    }
    let _ = reply.send(outcome);
}

/// A purged actor commits nothing; the close stays held for restart.
pub(super) fn refuse_closed(
    reply: tokio::sync::oneshot::Sender<PolicyMailboxOutcome>,
) -> &'static str {
    let _ = reply.send(PolicyMailboxOutcome::Retained(PolicyRetain::ActorClosed));
    "PolicyClose"
}

/// Runs one close inside the actor. A panic stops here; one after
/// the receipt clear still answers `Committed`, as the completion guard finished while unwinding.
pub(super) fn step(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    request: PolicyCloseRequest,
) -> PolicyMailboxOutcome {
    #[cfg(any(test, feature = "test-support"))]
    let _settlement = request.trace.settlement();
    let mut removed = None;
    let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
        close(state, channel_id, &request, &mut removed)
    }));
    let outcome = match (run, removed) {
        (Ok(outcome), _) => outcome,
        (Err(_), Some(token)) => {
            at!(request, Unwound);
            super::mark_turn_finished_signal_done(channel_id);
            PolicyMailboxOutcome::Committed(FinishTurnResult {
                removed_token: Some(token),
                has_pending: state
                    .intervention_queue
                    .iter()
                    .any(|item| item.mode == InterventionMode::Soft),
                mailbox_online: true,
                queue_exit_events: Vec::new(),
                persistence_error: Some("policy close panicked after its commit".into()),
            })
        }
        (Err(_), None) => PolicyMailboxOutcome::Retained(PolicyRetain::Panicked),
    };
    // A commit, unwound or not, seals the ticket before the actor answers or reads on.
    let outcome = match (outcome, request.ticket.as_ref()) {
        (PolicyMailboxOutcome::Committed(finish), Some(ticket)) => {
            PolicyMailboxOutcome::Committed(ticket.seal(state, finish))
        }
        (outcome, _) => outcome,
    };
    let committed = matches!(outcome, PolicyMailboxOutcome::Committed(_));
    request
        .trace
        .note(Point::MailboxCommitted, u64::from(committed));
    outcome
}

fn close(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    request: &PolicyCloseRequest,
    removed: &mut Option<Arc<CancelToken>>,
) -> PolicyMailboxOutcome {
    if request.witnessed(state.policy_witness.as_ref()) {
        return PolicyMailboxOutcome::Duplicate;
    }
    #[cfg(any(test, feature = "test-support"))]
    request.trace.note_started(state.turn_started_instant);
    at!(request, BeforeClaim);
    if !request.claims(state, channel_id) {
        return reconcile(state, channel_id, request);
    }
    // Built before the clear, so after it the guard only moves this value into the state.
    let witness = PolicyWitness {
        user_message_id: request.user_message_id,
        turn_nonce: request.turn_nonce.clone(),
        actor: request.actor.clone(),
    };
    at!(request, BeforeClear);
    let cleared = if state.control_custody.is_some() {
        request
            .receipt
            .try_clear_admitted(&|| admit_control(state, channel_id, false))
    } else {
        request.receipt.try_clear()
    };
    let refused = match cleared {
        PolicyClear::Cleared => None,
        cleared => Some(PolicyRetain::Clear(cleared)),
    };
    if let Some(refused) = refused {
        at!(request, ClearRefused);
        return PolicyMailboxOutcome::Retained(refused);
    }
    let mut commit = Commit {
        state: &mut *state,
        removed: &mut *removed,
        request,
        witness: Some(witness),
        held: None,
        detached: false,
        debited: false,
        fenced: false,
    };
    at!(request, AfterClear);
    commit.detach();
    at!(request, AfterDetach);
    commit.debit();
    at!(request, AfterDebit);
    commit.fence();
    at!(request, AfterFence);
    commit.witness();
    let held = commit.held;
    drop(commit);
    // After the commit: the queue, marker and latch work a matching finish does.
    super::pause_inbound_stall_for_turn(state, held);
    state.last_persistence = Some(request.persistence.clone());
    at!(request, AfterCommit);
    let mut finish =
        super::finalize_turn_state(state, channel_id, Some(&request.persistence), false);
    finish.removed_token = removed.clone();
    super::consume_pending_dispatch_marker_if_matches(
        state,
        channel_id,
        request.user_message_id,
        "policy_close",
    );
    super::mark_turn_finished_signal_done(channel_id);
    at!(request, BeforeSeal);
    PolicyMailboxOutcome::Committed(finish)
}

/// The receipt-only arm: no token, count, queue, marker, witness or signal is touched.
fn reconcile(
    state: &ChannelMailboxState,
    channel_id: ChannelId,
    request: &PolicyCloseRequest,
) -> PolicyMailboxOutcome {
    if !request.reconciles(state, channel_id) {
        return PolicyMailboxOutcome::Superseded;
    }
    // A row already gone is this receipt already cleared; any other answer keeps the hold.
    match request.receipt.try_clear() {
        PolicyClear::Cleared | PolicyClear::Missing => PolicyMailboxOutcome::Reconciled,
        refused => PolicyMailboxOutcome::Retained(PolicyRetain::Clear(refused)),
    }
}

/// Armed once the receipt row is gone: whichever stage a panic interrupts, dropping it finishes
/// the release, the one debit, the remint fence and the witness before the actor reads on.
struct Commit<'a> {
    state: &'a mut ChannelMailboxState,
    removed: &'a mut Option<Arc<CancelToken>>,
    request: &'a PolicyCloseRequest,
    witness: Option<PolicyWitness>,
    held: Option<Instant>,
    detached: bool,
    debited: bool,
    fenced: bool,
}

impl Commit<'_> {
    fn detach(&mut self) {
        if !std::mem::replace(&mut self.detached, true) {
            (*self.removed, self.held) =
                super::lease_release::detach_active_turn_anchor(self.state);
        }
    }

    fn debit(&mut self) {
        if !std::mem::replace(&mut self.debited, true) {
            let _ = self
                .request
                .active
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        }
    }

    fn fence(&mut self) {
        if !std::mem::replace(&mut self.fenced, true) {
            let request = self.request;
            (self.state.remint_fence)
                .note_exact_release(request.user_message_id, &request.turn_nonce);
        }
    }

    fn witness(&mut self) {
        if let Some(witness) = self.witness.take() {
            self.state.policy_witness = Some(witness);
        }
    }
}

impl Drop for Commit<'_> {
    fn drop(&mut self) {
        self.detach();
        self.debit();
        self.fence();
        self.witness();
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_hooks;
