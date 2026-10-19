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
        #[cfg(test)]
        $request.trace.fire(test_hooks::Stage::$stage);
    };
}

/// The close's one durable effect: the non-blocking exact clear of its acked receipt row.
pub(crate) trait PolicyReceipt: Send {
    fn channel(&self) -> u64;
    fn try_clear(&self) -> PolicyClear;
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
    #[cfg(test)]
    probe: Option<Arc<test_hooks::Probe>>,
}

impl Trace {
    /// Adds `count` at `point` and marks the point reached, so a zero there is a real reading.
    pub(crate) fn note(&self, point: Point, count: u64) {
        REGISTERED.fetch_or(1 << point as u32, Ordering::Relaxed);
        COUNTS[point as usize].fetch_add(count, Ordering::Relaxed);
        #[cfg(test)]
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
        super::ChannelMailboxMsg::ClaimPolicyTail(claim) => {
            return tail::answer(state, claim, recovery_done);
        }
        _ => unreachable!("only policy messages reach this arm"),
    };
    #[cfg(test)]
    let lost = request.trace.lost(test_hooks::Loss::MailboxReply);
    let outcome = step(state, channel_id, request);
    // Test seam: the step ran and sealed; only its reply is lost.
    #[cfg(test)]
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
    #[cfg(test)]
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
    #[cfg(test)]
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
    let refused = match request.receipt.try_clear() {
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

#[cfg(test)]
pub(crate) mod test_hooks {
    use super::{COUNTS, POINTS, Point, PolicyCloseRequest, REGISTERED, Trace};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    type Hook = Box<dyn FnMut(Stage) + Send>;

    /// Where a close's step, and the finalize after it, can be seen and interrupted.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Stage {
        BeforeClaim,
        BeforeClear,
        ClearRefused,
        AfterClear,
        AfterDetach,
        AfterDebit,
        AfterFence,
        AfterCommit,
        BeforeSeal,
        Unwound,
        BeforeFinalize,
    }

    /// A branch that, once taken, rules out the points it names for this request.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Branch {
        /// The finalizer refused the message; nothing downstream ran.
        SendFailed,
        /// The finalizer dequeued and dropped it; no mailbox step or finalize ran.
        Lost,
        /// The mailbox did not commit, so the finalize is skipped.
        NotCommitted,
        /// The commit is in and the finalize has not been entered yet.
        BeforeFinalize,
    }

    impl Branch {
        fn rules_out(self) -> &'static [Point] {
            use Point::*;
            match self {
                Self::SendFailed => &[
                    Dequeued,
                    MailboxCommitted,
                    AckObserved,
                    QueueExit,
                    FinalizeEntered,
                    FinalizeReturned,
                ],
                Self::Lost => &[
                    MailboxCommitted,
                    QueueExit,
                    FinalizeEntered,
                    FinalizeReturned,
                ],
                Self::NotCommitted | Self::BeforeFinalize => &[FinalizeEntered, FinalizeReturned],
            }
        }
    }

    /// The one message the normal finalizer actor loses for a request.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Loss {
        /// Dropped as the arm dequeues it, before any processing.
        Dequeued,
        /// Processed, then only its ACK sender is dropped.
        Ack,
        /// The mailbox step ran; only its reply is dropped.
        MailboxReply,
    }

    /// One request's observations; it travels with the request to whatever thread runs it.
    pub(crate) struct Probe {
        pub(crate) channel: u64,
        pub(crate) request: u64,
        settlement: bool,
        stages: Mutex<Vec<Stage>>,
        counts: [AtomicU64; POINTS],
        seen: AtomicU32,
        not_run: AtomicU32,
        started: Mutex<Option<Instant>>,
        hook: Mutex<Option<Hook>>,
        loss: Mutex<Option<Loss>>,
        tail: Mutex<Option<super::tail::PolicyTailDecision>>,
        ticket: Mutex<Option<std::sync::Weak<super::tail::PolicyCommitTicket>>>,
    }

    thread_local! { static ARMED: RefCell<Option<Hook>> = const { RefCell::new(None) }; }

    /// The next request built on this thread carries `hook` on its probe.
    pub(crate) fn arm(hook: Hook) {
        ARMED.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    /// Whether a hook was still armed, that is no request was built on this thread since.
    pub(crate) fn disarm() -> bool {
        ARMED.with(|slot| slot.borrow_mut().take()).is_some()
    }

    /// Binds a fresh probe to `request` with this thread's settlement switch and armed hook.
    pub(crate) fn bind(mut request: PolicyCloseRequest) -> PolicyCloseRequest {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        request.trace.probe = Some(Arc::new(Probe {
            channel: request.receipt.channel(),
            request: NEXT.fetch_add(1, Ordering::Relaxed),
            settlement: settlement_now(),
            stages: Mutex::default(),
            counts: [const { AtomicU64::new(0) }; POINTS],
            seen: AtomicU32::new(0),
            not_run: AtomicU32::new(0),
            started: Mutex::default(),
            hook: Mutex::new(ARMED.with(|slot| slot.borrow_mut().take())),
            loss: Mutex::default(),
            tail: Mutex::default(),
            ticket: Mutex::default(),
        }));
        request
    }

    fn bit(point: Point) -> u32 {
        1 << point as u32
    }

    impl Probe {
        pub(super) fn note(&self, point: Point, count: u64) {
            self.seen.fetch_or(bit(point), Ordering::Relaxed);
            self.counts[point as usize].fetch_add(count, Ordering::Relaxed);
        }

        /// Replaces the hook that sees, and may panic at, each stage this request reaches.
        pub(crate) fn on_stage(&self, hook: Hook) {
            *lock(&self.hook) = Some(hook);
        }

        pub(crate) fn lose(&self, loss: Loss) {
            *lock(&self.loss) = Some(loss);
        }

        pub(crate) fn stages(&self) -> Vec<Stage> {
            lock(&self.stages).clone()
        }

        /// The slot's start instant this request's step compared its cutoff with.
        pub(crate) fn started(&self) -> Option<Instant> {
            *lock(&self.started)
        }

        /// How this request's tail claim was decided, if its tail ran.
        pub(crate) fn tail(&self) -> Option<super::tail::PolicyTailDecision> {
            *lock(&self.tail)
        }

        /// The ledger ticket this request ran under, held weakly so its release is observable.
        pub(crate) fn ticket(&self) -> Option<std::sync::Weak<super::tail::PolicyCommitTicket>> {
            lock(&self.ticket).clone()
        }

        /// This request's own readings: its counts, the points it reached, and the ones a
        /// branch it took ruled out.
        pub(crate) fn snapshot(&self) -> Snapshot {
            Snapshot {
                run: self.request,
                seen: self.seen.load(Ordering::Relaxed),
                not_run: self.not_run.load(Ordering::Relaxed),
                counts: std::array::from_fn(|i| self.counts[i].load(Ordering::Relaxed)),
            }
        }
    }

    fn lock<T>(slot: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    impl Trace {
        pub(crate) fn probe(&self) -> Arc<Probe> {
            self.probe.clone().expect("a bound request")
        }

        pub(crate) fn fire(&self, stage: Stage) {
            let Some(probe) = &self.probe else {
                return;
            };
            lock(&probe.stages).push(stage);
            let hook = lock(&probe.hook).take();
            if let Some(hook) = hook {
                let mut restore = Restore(probe, Some(hook));
                if let Some(hook) = restore.1.as_mut() {
                    hook(stage);
                }
            }
        }

        /// Records that this request took `branch`, so the points it rules out read as not run.
        pub(crate) fn not_run(&self, branch: Branch) {
            if let Some(probe) = &self.probe {
                let mask = branch.rules_out().iter().fold(0, |mask, p| mask | bit(*p));
                probe.not_run.fetch_or(mask, Ordering::Relaxed);
            }
        }

        pub(crate) fn note_tail(&self, decision: super::tail::PolicyTailDecision) {
            if let Some(probe) = &self.probe {
                *lock(&probe.tail) = Some(decision);
            }
        }

        pub(crate) fn note_ticket(&self, ticket: &Arc<super::tail::PolicyCommitTicket>) {
            if let Some(probe) = &self.probe {
                *lock(&probe.ticket) = Some(Arc::downgrade(ticket));
            }
        }

        pub(super) fn note_started(&self, started: Option<Instant>) {
            if let Some(probe) = &self.probe {
                *lock(&probe.started) = started;
            }
        }

        pub(crate) fn lost(&self, loss: Loss) -> bool {
            self.probe
                .as_ref()
                .is_some_and(|probe| *lock(&probe.loss) == Some(loss))
        }

        /// The request's settlement switch on this thread until the guard drops.
        pub(crate) fn settlement(&self) -> Option<Settlement> {
            self.probe
                .as_ref()
                .map(|probe| Settlement::set(probe.settlement))
        }

        /// `work` polled under the request's settlement switch, whichever thread polls it.
        pub(crate) fn settled<F: std::future::Future>(
            &self,
            work: F,
        ) -> impl std::future::Future<Output = F::Output> + use<F> {
            let on = self.probe.as_ref().map(|probe| probe.settlement);
            let mut work = Box::pin(work);
            std::future::poll_fn(move |cx| {
                let _settlement = on.map(Settlement::set);
                work.as_mut().poll(cx)
            })
        }
    }

    /// Puts a fired hook back on its probe, also when the hook panics.
    struct Restore<'a>(&'a Probe, Option<Hook>);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let mut slot = lock(&self.0.hook);
            if slot.is_none() {
                *slot = self.1.take();
            }
        }
    }

    /// Holds a settlement switch on this thread and restores the prior one when dropped.
    pub(crate) struct Settlement {
        #[cfg(unix)]
        prior: bool,
    }

    impl Settlement {
        pub(crate) fn set(on: bool) -> Self {
            #[cfg(unix)]
            let prior =
                crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE
                    .replace(on);
            #[cfg(not(unix))]
            let _ = on;
            Self {
                #[cfg(unix)]
                prior,
            }
        }
    }

    impl Drop for Settlement {
        fn drop(&mut self) {
            #[cfg(unix)]
            crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE
                .set(self.prior);
        }
    }

    fn settlement_now() -> bool {
        crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available()
    }

    /// Readings under one run: this process for the totals, one request for a probe.
    #[derive(Clone, Debug)]
    pub(crate) struct Snapshot {
        pub(crate) run: u64,
        seen: u32,
        not_run: u32,
        counts: [u64; POINTS],
    }

    /// Why a snapshot cannot be read as counts, or as a finished run.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Judged {
        Foreign,
        Missing(Point),
        Incomplete,
    }

    const ALL: [Point; POINTS] = [
        Point::SubmitAttempt,
        Point::SendAccepted,
        Point::Dequeued,
        Point::MailboxCommitted,
        Point::AckObserved,
        Point::QueueExit,
        Point::FinalizeEntered,
        Point::FinalizeReturned,
    ];

    impl Snapshot {
        /// The raw counts at `required`: a reached point reads its count, one a taken branch
        /// ruled out reads 0, and any other point is `Missing`.
        pub(crate) fn judge(&self, run: u64, required: &[Point]) -> Result<Vec<u64>, Judged> {
            if self.run != run {
                return Err(Judged::Foreign);
            }
            let read = |point: Point| {
                if self.seen & bit(point) != 0 {
                    Ok(self.counts[point as usize])
                } else if self.not_run & bit(point) != 0 {
                    Ok(0)
                } else {
                    Err(Judged::Missing(point))
                }
            };
            required.iter().map(|point| read(*point)).collect()
        }

        /// Whether the run finished: every point read, and no commit without its queue exit and
        /// finalize, and no finalize without its return.
        pub(crate) fn complete(&self, run: u64) -> Result<(), Judged> {
            let counts = self.judge(run, &ALL)?;
            let count = |point: Point| counts[point as usize];
            if count(Point::MailboxCommitted) > count(Point::QueueExit)
                || count(Point::MailboxCommitted) > count(Point::FinalizeEntered)
                || count(Point::FinalizeEntered) != count(Point::FinalizeReturned)
            {
                return Err(Judged::Incomplete);
            }
            Ok(())
        }

        pub(crate) fn with_run(mut self, run: u64) -> Self {
            self.run = run;
            self
        }

        pub(crate) fn unseen(mut self, point: Point) -> Self {
            self.seen &= !bit(point);
            self
        }
    }

    /// The process-wide totals under this process's run; a point is read once any call site
    /// in the process reached it.
    pub(crate) fn totals() -> Snapshot {
        static RUN: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
            u64::from(std::process::id()) << 32
                | u64::from(
                    std::time::UNIX_EPOCH
                        .elapsed()
                        .map_or(0, |d| d.subsec_nanos()),
                )
        });
        Snapshot {
            run: *RUN,
            seen: REGISTERED.load(Ordering::Relaxed),
            not_run: 0,
            counts: std::array::from_fn(|i| COUNTS[i].load(Ordering::Relaxed)),
        }
    }

    /// A captured handle whose actor is gone, so every send to it fails.
    pub(crate) fn closed_mailbox() -> super::super::ChannelMailboxHandle {
        super::super::mailbox_unreachable_tests::closed_handle()
    }
}
