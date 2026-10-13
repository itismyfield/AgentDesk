//! The mailbox half of a before-start policy close: one actor step claims the exact episode, clears
//! its receipt without waiting, then releases, debits, fences and witnesses before reading on.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::{ChannelMailboxState, FinishTurnResult, InterventionMode, QueuePersistenceContext};
use crate::services::discord::input_runtime::fence::{Failure, PopulationScope};
use crate::services::provider::{CancelToken, ProviderKind};

/// The close's one durable effect: the non-blocking exact clear of its acked receipt row, under
/// the authority the step holds over its channel.
pub(crate) trait PolicyReceipt: Send {
    fn channel(&self) -> u64;
    fn try_clear(&self, authority: &PolicyAuthority<'_>) -> PolicyClear;
}

/// The step's write authority over its channel, borrowed from its input fence for the whole step:
/// a legacy step holds none to lend, a protected one lends its held population scope.
pub(crate) struct PolicyAuthority<'a>(Held<'a>);

enum Held<'a> {
    Legacy,
    Protected(&'a PopulationScope),
}

impl<'a> PolicyAuthority<'a> {
    pub(super) fn legacy() -> Self {
        Self(Held::Legacy)
    }

    pub(super) fn protected(scope: &'a PopulationScope) -> Self {
        Self(Held::Protected(scope))
    }

    pub(crate) fn is_protected(&self) -> bool {
        matches!(self.0, Held::Protected(_))
    }

    /// Runs `work` under the borrowed scope, only for the exact row that scope holds.
    pub(crate) fn borrowed<T>(
        &self,
        root: &std::path::Path,
        provider: &ProviderKind,
        channel: u64,
        work: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        match self.0 {
            Held::Protected(scope) => scope.borrowed(root, provider, channel, work),
            Held::Legacy => Err(std::io::Error::other("a legacy step lends no scope")),
        }
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
    /// The input fence refused the row's lock; never an IO failure.
    InputFence(Failure),
}

/// Why a close released nothing; the turn stays held with its receipt.
#[derive(Debug)]
pub(crate) enum PolicyRetain {
    Clear(PolicyClear),
    InputFence(Failure),
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
}

/// Runs one close inside the actor under the step's own authority. A panic stops here; one after
/// the receipt clear still answers `Committed`, as the completion guard finished while unwinding.
pub(super) fn step(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    request: PolicyCloseRequest,
    authority: PolicyAuthority<'_>,
) -> PolicyMailboxOutcome {
    #[cfg(test)]
    let _settlement = request.trace.settlement();
    let mut removed = None;
    let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
        close(state, channel_id, &request, &authority, &mut removed)
    }));
    let outcome = match (run, removed) {
        (Ok(outcome), _) => outcome,
        (Err(_), Some(token)) => {
            at(&request, "unwound");
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
    authority: &PolicyAuthority<'_>,
    removed: &mut Option<Arc<CancelToken>>,
) -> PolicyMailboxOutcome {
    if request.witnessed(state.policy_witness.as_ref()) {
        return PolicyMailboxOutcome::Duplicate;
    }
    #[cfg(test)]
    request.trace.note_started(state.turn_started_instant);
    at(request, "before_claim");
    if !request.claims(state, channel_id) {
        return PolicyMailboxOutcome::Superseded;
    }
    // Built before the clear, so after it the guard only moves this value into the state.
    let witness = PolicyWitness {
        user_message_id: request.user_message_id,
        turn_nonce: request.turn_nonce.clone(),
        actor: request.actor.clone(),
    };
    at(request, "before_clear");
    let refused = match request.receipt.try_clear(authority) {
        PolicyClear::Cleared => None,
        PolicyClear::InputFence(failure) => Some(PolicyRetain::InputFence(failure)),
        cleared => Some(PolicyRetain::Clear(cleared)),
    };
    if let Some(refused) = refused {
        at(request, "clear_refused");
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
    at(request, "after_clear");
    commit.detach();
    at(request, "after_detach");
    commit.debit();
    at(request, "after_debit");
    commit.fence();
    at(request, "after_fence");
    commit.witness();
    let held = commit.held;
    drop(commit);
    // After the commit: the queue, marker and latch work a matching finish does.
    super::pause_inbound_stall_for_turn(state, held);
    state.last_persistence = Some(request.persistence.clone());
    at(request, "after_commit");
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
    PolicyMailboxOutcome::Committed(finish)
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

/// Test seam at each stage of the close, on the request's own probe; nothing outside tests.
fn at(_request: &PolicyCloseRequest, _stage: &'static str) {
    #[cfg(test)]
    _request.trace.fire(_stage);
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use super::{COUNTS, POINTS, Point, PolicyCloseRequest, REGISTERED, Trace};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;
    use std::time::Instant;

    type Hook = Box<dyn FnMut(&'static str) + Send>;

    /// The one message the normal finalizer actor loses for a request.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Loss {
        /// Dropped as the arm dequeues it, before any processing.
        Dequeued,
        /// Processed, then only its ACK sender is dropped.
        Ack,
    }

    /// One request's observations; it travels with the request to whatever thread runs it.
    pub(crate) struct Probe {
        pub(crate) channel: u64,
        pub(crate) episode: String,
        pub(crate) request: u64,
        settlement: bool,
        stages: Mutex<Vec<(&'static str, ThreadId)>>,
        counts: [AtomicU64; POINTS],
        started: Mutex<Option<Instant>>,
        hook: Mutex<Option<Hook>>,
        loss: Mutex<Option<Loss>>,
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
            episode: request.turn_nonce.clone(),
            request: NEXT.fetch_add(1, Ordering::Relaxed),
            settlement: settlement_now(),
            stages: Mutex::default(),
            counts: [const { AtomicU64::new(0) }; POINTS],
            started: Mutex::default(),
            hook: Mutex::new(ARMED.with(|slot| slot.borrow_mut().take())),
            loss: Mutex::default(),
        }));
        request
    }

    impl Probe {
        pub(super) fn note(&self, point: Point, count: u64) {
            self.counts[point as usize].fetch_add(count, Ordering::Relaxed);
        }

        /// Replaces the hook that sees, and may panic at, each stage this request reaches.
        pub(crate) fn on_stage(&self, hook: Hook) {
            *lock(&self.hook) = Some(hook);
        }

        pub(crate) fn lose(&self, loss: Loss) {
            *lock(&self.loss) = Some(loss);
        }

        pub(crate) fn stages(&self) -> Vec<&'static str> {
            lock(&self.stages).iter().map(|(stage, _)| *stage).collect()
        }

        /// The threads the stages ran on.
        pub(crate) fn threads(&self) -> Vec<ThreadId> {
            lock(&self.stages)
                .iter()
                .map(|(_, thread)| *thread)
                .collect()
        }

        /// The slot's start instant this request's step compared its cutoff with.
        pub(crate) fn started(&self) -> Option<Instant> {
            *lock(&self.started)
        }

        /// This request's counts, read against the points this process ever reached.
        pub(crate) fn snapshot(&self) -> Snapshot {
            Snapshot {
                run: self.request,
                registered: REGISTERED.load(Ordering::Relaxed),
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

        pub(crate) fn fire(&self, stage: &'static str) {
            let Some(probe) = &self.probe else {
                return;
            };
            lock(&probe.stages).push((stage, std::thread::current().id()));
            let hook = lock(&probe.hook).take();
            if let Some(hook) = hook {
                let mut restore = Restore(probe, Some(hook));
                if let Some(hook) = restore.1.as_mut() {
                    hook(stage);
                }
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

    /// Counts read under one run: this process for the totals, one request for a probe.
    #[derive(Clone, Debug)]
    pub(crate) struct Snapshot {
        pub(crate) run: u64,
        registered: u32,
        counts: [u64; POINTS],
    }

    /// Why a snapshot cannot be read as counts.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Judged {
        Foreign,
        Missing(Point),
        Incomplete,
    }

    impl Snapshot {
        /// The counts at `required`. Another run, a point no call site reached, and a commit or
        /// finalize that never returned are refused, since their zeros say nothing.
        pub(crate) fn judge(&self, run: u64, required: &[Point]) -> Result<Vec<u64>, Judged> {
            if self.run != run {
                return Err(Judged::Foreign);
            }
            if let Some(point) = required
                .iter()
                .find(|point| self.registered & (1 << **point as u32) == 0)
            {
                return Err(Judged::Missing(*point));
            }
            let count = |point: Point| self.counts[point as usize];
            if count(Point::FinalizeEntered) != count(Point::FinalizeReturned)
                || count(Point::MailboxCommitted) > count(Point::QueueExit)
            {
                return Err(Judged::Incomplete);
            }
            Ok(required.iter().map(|point| count(*point)).collect())
        }

        pub(crate) fn with_run(mut self, run: u64) -> Self {
            self.run = run;
            self
        }

        pub(crate) fn unregistered(mut self, point: Point) -> Self {
            self.registered &= !(1 << point as u32);
            self
        }
    }

    /// The process-wide totals under this process's run.
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
            registered: REGISTERED.load(Ordering::Relaxed),
            counts: std::array::from_fn(|i| COUNTS[i].load(Ordering::Relaxed)),
        }
    }

    /// A captured handle whose actor is gone, so every send to it fails.
    pub(crate) fn closed_mailbox() -> super::super::ChannelMailboxHandle {
        super::super::mailbox_unreachable_tests::closed_handle()
    }

    /// The authority of a step without an input fence.
    pub(crate) fn legacy() -> super::PolicyAuthority<'static> {
        super::PolicyAuthority::legacy()
    }

    /// The authority a protected step lends, for capability tests outside the actor.
    pub(crate) fn authority(
        scope: &crate::services::discord::input_runtime::fence::PopulationScope,
    ) -> super::PolicyAuthority<'_> {
        super::PolicyAuthority::protected(scope)
    }
}
