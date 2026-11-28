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
    super::super::test_support::closed_handle()
}
