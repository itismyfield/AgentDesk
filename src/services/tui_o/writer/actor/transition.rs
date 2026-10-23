//! Scoped transition cleanup takes one job at a time from the running actor's
//! mailbox, probes each candidate twice off the actor, and retires at most one source per poll.

use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc;

use tokio::sync::oneshot;
use tokio::time::Instant;

use super::transition_snapshot::{Common, Probe};
use super::*;
use crate::services::tmux_common::try_with_tmux_source_authority;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::shadow::{SourceAnomalyKind, SourceId};
use crate::services::tui_o::store::StoreError;
use crate::services::tui_o::store::spool::Cursor;
use crate::services::tui_o::writer::rotation::RETIRE_QUIET;
use crate::services::tui_o::writer::rotation::transition::Watch;
use crate::services::tui_o::writer::transition_cleanup::observe::{FileSeen, Observation, observe};
use crate::services::tui_o::writer::transition_cleanup::scope::{ScopeApproval, approve};
use crate::services::tui_o::writer::transition_cleanup::{
    Judged, Request, Termination, Unclear, Verdict, judge,
};
use crate::services::tui_o::writer::transition_port::{
    Context, Envelope, Hold, Hop, Kind, Operation, Outcome, Receiver, Refused, Reply, Wall,
};

/// Candidates one inspection probes; automatic jobs inspect every pending source.
const CANDIDATES: usize = 64;
/// Sources one worker probes before the actor takes its results.
const BATCH: usize = 4;
/// Inspections hold every source not probed twice by this deadline.
const DEADLINE: Duration = Duration::from_secs(60);
/// Prefix bytes one inspection may hash: two probes of every candidate at 32 MiB each.
const HASH_BUDGET: u64 = 2 * CANDIDATES as u64 * (32 << 20);

pub(super) struct Transitions {
    receiver: Option<Receiver>,
    job: Option<Job>,
    /// The actor's stop, read again just before each retirement write.
    stop: watch::Receiver<bool>,
    /// Rotates the reserved receipt across inspection jobs.
    turn: usize,
}

impl Transitions {
    pub(super) fn has_job(&self) -> bool {
        self.job.is_some()
    }
    pub(super) fn new(receiver: Option<Receiver>, stop: watch::Receiver<bool>) -> Self {
        let job = None;
        Self {
            receiver,
            job,
            stop,
            turn: 0,
        }
    }

    /// A job is retiring sources, so the channel's unsettled count is not final.
    pub(super) fn applying(&self) -> bool {
        (self.job.as_ref()).is_some_and(|job| matches!(job.stage, Stage::Apply(_)))
    }
}

enum Stage {
    First,
    /// The second probes wait until this long after the first ones ended.
    Quiet(Instant),
    Second,
    Apply(VecDeque<String>),
}

type Worker = mpsc::Receiver<Vec<(String, Observation, Option<std::time::SystemTime>)>>;

pub(super) struct Job {
    pub(super) request: Request,
    apply: bool,
    pub(super) automatic: Option<super::super::transition_port::Idle>,
    /// The scope an Apply was admitted for, never replaced while it runs.
    scope: Option<ScopeApproval>,
    /// The host's stop, read with the actor's before each retirement write.
    stopping: watch::Receiver<bool>,
    context: Context,
    reply: oneshot::Sender<Reply>,
    gate: Arc<OwnershipGate>,
    pub(super) wall: Wall,
    common: Common,
    /// The judgment the job started from.
    first: Vec<Judged>,
    pub(super) probes: BTreeMap<String, Probe>,
    queue: VecDeque<String>,
    stage: Stage,
    worker: Option<Worker>,
    deadline: Instant,
    hashed: u64,
    pub(super) outcomes: BTreeMap<String, Outcome>,
}

impl Job {
    fn hops(judged: Vec<Judged>, outcomes: &BTreeMap<String, Outcome>) -> Vec<Hop> {
        let outcome = |key: &str| outcomes.get(key).cloned().unwrap_or(Outcome::Unchanged);
        let hop = |judged: Judged| Hop {
            outcome: outcome(&judged.key),
            judged,
        };
        judged.into_iter().map(hop).collect()
    }

    fn receipt(&self) -> Reply {
        Reply {
            hops: Self::hops(self.first.clone(), &self.outcomes),
            operation: Some(self.kind()),
            scope: self.scope.clone().map(Ok),
            ..Reply::refused(&self.request.job_id, Refused::InProgress)
        }
    }

    fn kind(&self) -> Kind {
        if self.apply {
            Kind::Apply
        } else {
            Kind::Inspect
        }
    }
}

/// A recorded cleanup under the request's ID that another operator, reason, cutoff or end made.
fn conflicts<'a>(mut cursors: impl Iterator<Item = &'a Cursor>, request: &Request) -> bool {
    let ended = match &request.termination {
        Some(Termination::Confirmed(end)) => Some(end),
        _ => None,
    };
    cursors.any(|cursor| {
        cursor.transition_cleanup.as_ref().is_some_and(|done| {
            done.job_id == request.job_id
                && (done.operator != request.operator
                    || done.reason != request.reason
                    || done.cutoff != request.cutoff
                    || Some(&done.ended) != ended)
        })
    })
}

/// The actor's clock; a test may move it on just as a worker's probes are taken.
fn now() -> Instant {
    let now = Instant::now();
    #[cfg(test)]
    let now = now + test_hooks::skew();
    now
}

/// The path still shows the file the second probe saw.
fn path_unchanged(source: &SourceId, later: Option<&Observation>) -> bool {
    let path = Fingerprint::path(source);
    match later.map(|seen| &seen.file) {
        Some(FileSeen::Present(seen)) => path.ok() == Some(seen.fingerprint.clone()),
        Some(FileSeen::Missing) => matches!(path, Err((SourceAnomalyKind::Missing, _))),
        _ => false,
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    /// Takes requests only once the sources are restored: at most one envelope and one step a poll.
    pub(super) async fn poll_transition(&mut self, resumed: bool, unsettled: &Unsettled) {
        let suspended = !self.writer.suspension.is_empty();
        #[cfg(test)]
        let suspended = suspended
            && !crate::services::claude_tui::source_verify::n2b_mutant("b1b-poll-transition");
        if suspended {
            if let Some(receiver) = &mut self.transitions.receiver
                && let Ok(Some(envelope)) = receiver.try_next()
            {
                let id = &envelope.operation.request_ref().job_id;
                let _ = envelope
                    .reply
                    .send(Reply::refused(id, Refused::CodexDeferred));
            }
            return;
        }
        if !resumed || self.writer.is_stopped() {
            return;
        }
        self.take_request();
        let Some(mut job) = self.transitions.job.take() else {
            return;
        };
        if self.step(&mut job, unsettled).await {
            self.finish(job);
        } else {
            self.transitions.job = Some(job);
        }
    }

    fn take_request(&mut self) {
        let Some(receiver) = &mut self.transitions.receiver else {
            return;
        };
        let envelope = match receiver.try_next() {
            Ok(Some(envelope)) => envelope,
            Ok(None) => return,
            Err(_) => {
                self.transitions.receiver = None;
                return;
            }
        };
        let Some(receiver) = &self.transitions.receiver else {
            return;
        };
        let (gate, wall) = (Arc::clone(&receiver.gate), Arc::clone(&receiver.wall));
        let stopping = receiver.stopping.clone();
        let Envelope {
            operation,
            context,
            reply,
            ..
        } = envelope;
        let (apply, request, expected, automatic) = match operation {
            Operation::Inspect(request) => (false, request, None, None),
            #[cfg(test)]
            Operation::Apply(request, digest) => (true, request, Some(digest), None),
            Operation::AutomaticApply(request, digest, idle) => {
                (true, request, Some(digest), Some(idle))
            }
        };
        let refused = match &self.transitions.job {
            Some(job) if job.request.job_id == request.job_id => {
                // Only the same operation and request, `at` aside, with no audit against it gets
                // the receipt; the scope it asks for neither joins nor changes the job.
                let asked = Request {
                    at: job.request.at,
                    ..request.clone()
                };
                let same = apply == job.apply
                    && automatic.is_some() == job.automatic.is_some()
                    && asked == job.request;
                if same && !conflicts(self.writer.store().cursors(), &request) {
                    let _ = reply.send(job.receipt());
                    return;
                }
                Some(Refused::JobConflict)
            }
            Some(_) => Some(Refused::Busy),
            None if self.sources.transition_provider() == ShadowProvider::Codex => {
                Some(Refused::CodexDeferred)
            }
            None if conflicts(self.writer.store().cursors(), &request) => {
                Some(Refused::JobConflict)
            }
            None => None,
        };
        if let Some(refused) = refused {
            let _ = reply.send(Reply::refused(&request.job_id, refused));
            return;
        }
        let (snapshot, common) = match self.build_snapshot(&context, &gate, &BTreeMap::new()) {
            Ok(built) => built,
            Err(detail) => {
                let _ = reply.send(Reply::refused(&request.job_id, Refused::Unread(detail)));
                return;
            }
        };
        // A new Apply runs only on the scope its Inspect approved, compared on this snapshot.
        let scope = match expected.map(|digest| (digest, approve(&snapshot, &request, &context))) {
            None => None,
            Some((digest, Ok(scope))) if scope.digest == digest => Some(scope),
            Some((_, scoped)) => {
                let mut detail = "inspect_scope_changed".to_owned();
                if let Err(error) = scoped {
                    detail = format!("{detail}: {error}");
                }
                let _ = reply.send(Reply::refused(&request.job_id, Refused::Unread(detail)));
                return;
            }
        };
        let first = judge(&snapshot, &request).hops;
        let cursors: BTreeMap<String, &Cursor> = (snapshot.cursors.iter())
            .map(|c| (source_key(&c.source), c))
            .collect();
        let unprobed = |hop: &&Judged| hop.verdict == Verdict::Unknown(Unclear::NotObserved);
        // Pending sources come first; inspections reserve room for an existing receipt.
        let pending = |hop: &&Judged| {
            cursors
                .get(&hop.key)
                .is_some_and(|c| !c.retired && c.transition_cleanup.is_none())
        };
        let (mut unprobed, mut receipts): (Vec<&Judged>, Vec<&Judged>) =
            first.iter().filter(unprobed).partition(pending);
        let limited = automatic.is_none();
        #[cfg(test)]
        let limited = limited
            || test_hooks::retention_mutant("budget_receipt")
            || test_hooks::retention_mutant("budget_panes");
        if !limited {
            receipts.clear();
        } else if unprobed.len() >= CANDIDATES && !receipts.is_empty() {
            let turn = self.transitions.turn % receipts.len();
            self.transitions.turn = self.transitions.turn.wrapping_add(1);
            unprobed.insert(CANDIDATES - 1, receipts.remove(turn));
        }
        unprobed.extend(receipts);
        let (mut probes, mut outcomes) = (BTreeMap::new(), BTreeMap::new());
        for (index, hop) in unprobed.into_iter().enumerate() {
            let Some(cursor) = cursors
                .get(&hop.key)
                .filter(|_| !limited || index < CANDIDATES)
            else {
                outcomes.insert(hop.key.clone(), Outcome::Held(Hold::Limit));
                continue;
            };
            let (token, earlier, later) = (self.token(cursor), None, None);
            probes.insert(
                hop.key.clone(),
                Probe {
                    token,
                    earlier,
                    later,
                    last_dialogue: None,
                },
            );
        }
        self.transitions.job = Some(Job {
            request,
            apply,
            automatic,
            scope,
            stopping,
            context,
            reply,
            gate,
            wall,
            common,
            first,
            queue: probes.keys().cloned().collect(),
            probes,
            stage: Stage::First,
            worker: None,
            deadline: now() + DEADLINE,
            hashed: 0,
            outcomes,
        });
    }

    /// One step of `job`; true once it has nothing left to do.
    async fn step(&mut self, job: &mut Job, unsettled: &Unsettled) -> bool {
        if let Stage::Apply(queue) = &mut job.stage {
            let Some(key) = queue.pop_front() else {
                return true;
            };
            let outcome = self.retire_one(job, &key, unsettled).await;
            let rest = match outcome {
                Outcome::Unknown => Some(Outcome::Held(Hold::NotTried)),
                Outcome::Held(Hold::Stopping | Hold::InspectScopeChanged) => Some(outcome.clone()),
                _ => None,
            };
            job.outcomes.insert(key, outcome);
            let Stage::Apply(queue) = &mut job.stage else {
                return true;
            };
            if let Some(rest) = rest {
                for key in queue.drain(..) {
                    job.outcomes.insert(key, rest.clone());
                }
            }
            return queue.is_empty();
        }
        if !self.probe(job) {
            return false;
        }
        let probed = (job.probes.iter()).filter(|(_, probe)| probe.later.is_some());
        let probed: VecDeque<String> = probed.map(|(key, _)| key.clone()).collect();
        for key in job.probes.keys().filter(|key| !probed.contains(key)) {
            job.outcomes
                .entry(key.clone())
                .or_insert(Outcome::Held(Hold::Limit));
        }
        if !job.apply || probed.is_empty() {
            return true;
        }
        job.stage = Stage::Apply(probed);
        false
    }

    /// Takes a finished worker's probes and starts the next batch; true once probing is over.
    fn probe(&mut self, job: &mut Job) -> bool {
        if job.probes.is_empty() {
            return true;
        }
        if let Some(worker) = &job.worker {
            #[cfg(test)]
            test_hooks::receiving();
            let seen = match worker.try_recv() {
                Ok(seen) => seen,
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => Vec::new(),
            };
            job.worker = None;
            let first = matches!(job.stage, Stage::First);
            // Inspection probes taken past their deadline stay unrecorded.
            let late = job.automatic.is_none() && now() >= job.deadline;
            for (key, seen, last_dialogue) in seen {
                if !first && let Some(probe) = job.probes.get_mut(&key).filter(|_| !late) {
                    probe.last_dialogue = last_dialogue;
                }
                if let Some(probe) = job.probes.get_mut(&key).filter(|_| !late) {
                    *(if first {
                        &mut probe.earlier
                    } else {
                        &mut probe.later
                    }) = Some(seen);
                }
            }
        }
        let now = now();
        if job.automatic.is_none() && now >= job.deadline {
            return true;
        }
        match job.stage {
            Stage::First | Stage::Second if !job.queue.is_empty() => {}
            Stage::First => {
                job.stage = Stage::Quiet(now + RETIRE_QUIET);
                return false;
            }
            Stage::Quiet(until) if now >= until => {
                let probed = job
                    .probes
                    .iter()
                    .filter(|(_, probe)| probe.earlier.is_some());
                job.queue = probed.map(|(key, _)| key.clone()).collect();
                job.stage = Stage::Second;
            }
            Stage::Quiet(_) => return false,
            Stage::Second | Stage::Apply(_) => return true,
        }
        self.dispatch(job);
        false
    }

    /// Probes the next batch on a blocking worker; the actor never waits on it.
    fn dispatch(&mut self, job: &mut Job) {
        let mut batch = Vec::new();
        while batch.len() < BATCH
            && let Some(key) = job.queue.pop_front()
        {
            let cursor = job.probes[&key].token.cursor.clone();
            if job.automatic.is_none() {
                if job.hashed + cursor.captured_through > HASH_BUDGET {
                    continue;
                }
                job.hashed += cursor.captured_through;
            }
            batch.push((key, cursor));
        }
        if batch.is_empty() {
            return;
        }
        let (send, worker) = mpsc::channel();
        let wall = Arc::clone(&job.wall);
        let automatic = job.automatic.is_some();
        #[cfg(test)]
        let done = test_hooks::worker_started();
        tokio::task::spawn_blocking(move || {
            let seen = batch.into_iter().map(|(key, cursor)| {
                let age = automatic
                    .then(|| super::automatic_age::last_dialogue(&cursor))
                    .flatten();
                (key, observe(&cursor, wall()), age)
            });
            let _ = send.send(seen.collect());
            #[cfg(test)]
            done.store(true, std::sync::atomic::Ordering::Release);
        });
        job.worker = Some(worker);
    }

    /// Holds the session transition lock and the pane's source authority across one retirement,
    /// both taken and released within this call.
    async fn retire_one(&mut self, job: &mut Job, key: &str, unsettled: &Unsettled) -> Outcome {
        let Ok(_transition) = Arc::clone(&job.context.lock).try_lock_owned() else {
            return Outcome::Held(Hold::TransitionBusy);
        };
        if let Err(reason) = super::automatic::ready(job, key).await {
            return Outcome::Held(Hold::Policy(reason));
        }
        let tmux = job.context.tmux.clone();
        let admitted =
            try_with_tmux_source_authority(&tmux, |_| self.retire_admitted(job, key, unsettled));
        admitted.unwrap_or(Outcome::Held(Hold::SourceBusy))
    }

    fn retire_admitted(&mut self, job: &Job, key: &str, unsettled: &Unsettled) -> Outcome {
        let Some(probe) = job.probes.get(key) else {
            return Outcome::Unchanged;
        };
        let Ok((snapshot, common)) = self.build_snapshot(&job.context, &job.gate, &job.probes)
        else {
            return Outcome::Held(Hold::Stale);
        };
        let approved = |scope: &ScopeApproval| {
            let now = approve(&snapshot, &job.request, &job.context);
            now.is_ok_and(|now| now.digest == scope.digest)
        };
        if !job.scope.as_ref().is_some_and(approved) {
            return Outcome::Held(Hold::InspectScopeChanged);
        }
        let cursor = (snapshot.cursors.iter()).find(|c| source_key(&c.source) == key);
        let Some(cursor) = cursor.filter(|cursor| self.token(cursor) == probe.token) else {
            return Outcome::Held(Hold::Stale);
        };
        if common != job.common || !path_unchanged(&cursor.source, probe.later.as_ref()) {
            return Outcome::Held(Hold::Stale);
        }
        let mut judged = judge(&snapshot, &job.request).hops.into_iter();
        let audit = match judged.find(|hop| hop.key == key).map(|hop| hop.verdict) {
            Some(Verdict::AlreadyDone) => return Outcome::AlreadyDone,
            Some(Verdict::Eligible(audit)) if cursor.transition_cleanup.is_none() => *audit,
            Some(Verdict::Eligible(_)) => return Outcome::Held(Hold::PriorCleanup),
            _ => return Outcome::Unchanged,
        };
        // A stop raised while this poll awaited delivery is read again here, before the write; the
        // host's is read too, as its own stop may not have reached the actor yet.
        if *self.transitions.stop.borrow() || *job.stopping.borrow() {
            return Outcome::Held(Hold::Stopping);
        }
        #[cfg(test)]
        test_hooks::at_retire();
        unsettled.send_replace(None);
        match self.writer.store().retire_transition(cursor, audit) {
            Ok(()) => {}
            Err(StoreError::Rejected(_)) => return Outcome::Held(Hold::Stale),
            Err(error) => {
                let detail = format!("transition cleanup: {error:?}");
                self.writer.stop(WriterAlarm::Halted { detail });
                return Outcome::Unknown;
            }
        }
        let watch = self.sources.arm_transition_watch(&cursor.source);
        match self
            .sources
            .regrown_after_transition(&mut self.writer, &cursor.source)
        {
            Ok(true) => Outcome::RetiredThenGrew,
            Ok(false) => Outcome::Retired(watch.unwrap_or(Watch::Unwatched)),
            Err(alarm) => {
                self.writer.stop(alarm);
                Outcome::Unknown
            }
        }
    }

    /// Answers the job with the judgment of the actor's state now and the projection after it.
    fn finish(&mut self, job: Job) {
        super::automatic::log(&job);
        let operation = Some(job.kind());
        let Job {
            request,
            scope: accepted,
            context,
            reply,
            gate,
            probes,
            first,
            outcomes,
            ..
        } = job;
        // An Apply answers with the scope it accepted; only an Inspect's final snapshot issues one.
        let accepted = accepted.map(Ok);
        let (hops, summary, execution, diagnostics, scope) =
            match self.build_snapshot(&context, &gate, &probes) {
                Ok((snapshot, common)) => {
                    let judgment = judge(&snapshot, &request);
                    let hops = Job::hops(judgment.hops, &outcomes);
                    let diagnostics = common.diagnostics(&snapshot, &context.nonce);
                    let scope = accepted.or_else(|| Some(approve(&snapshot, &request, &context)));
                    let execution = Some(snapshot.execution);
                    (
                        hops,
                        Some(judgment.summary),
                        execution,
                        Some(diagnostics),
                        scope,
                    )
                }
                Err(detail) => {
                    let hops = Job::hops(first, &outcomes);
                    (hops, None, Some(Err(detail)), None, accepted)
                }
            };
        let stopped = self.writer.is_stopped();
        let _ = reply.send(Reply {
            job_id: request.job_id,
            refused: None,
            operation,
            scope,
            hops,
            summary,
            execution,
            unsettled: if stopped { None } else { self.unsettled() },
            diagnostics,
        });
    }
}

#[cfg(all(test, unix))]
#[path = "scoped_transition_tests.rs"]
pub(super) mod scoped_tests;

#[cfg(test)]
pub(crate) mod test_hooks {
    //! Lets a test wait for the probe workers, count the snapshots, and look in just before a
    //! retirement is written.
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    thread_local! {
        static WORKERS: RefCell<Vec<Arc<AtomicBool>>> = const { RefCell::new(Vec::new()) };
        static SNAPSHOTS: Cell<usize> = const { Cell::new(0) };
        static AT_RETIRE: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
        static SKEW: Cell<(Option<Duration>, Duration)> = const { Cell::new((None, Duration::ZERO)) };
    }

    pub(crate) fn retention_mutant(tag: &str) -> bool {
        std::env::var("AGENTDESK_OTRANSITION_MUTANT").is_ok_and(|value| value == tag)
    }

    pub(super) fn receiving() {
        SKEW.with(|skew| match skew.get() {
            (Some(armed), _) => skew.set((None, armed)),
            (None, _) => {}
        });
    }

    pub(super) fn skew() -> Duration {
        SKEW.with(|skew| skew.get().1)
    }

    /// Moves the actors' clock on this thread by `by` just as one next takes a worker's probes,
    /// until the guard drops.
    pub(crate) fn skew_at_receipt(by: Duration) -> impl Drop {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                SKEW.with(|skew| skew.set((None, Duration::ZERO)));
            }
        }
        SKEW.with(|skew| skew.set((Some(by), Duration::ZERO)));
        Clear
    }

    pub(super) fn worker_started() -> Arc<AtomicBool> {
        let done = Arc::new(AtomicBool::new(false));
        WORKERS.with(|workers| workers.borrow_mut().push(Arc::clone(&done)));
        done
    }

    /// Probe workers the actors on this thread started so far.
    pub(crate) fn started() -> usize {
        WORKERS.with(|workers| workers.borrow().len())
    }

    /// Blocks this thread until every probe worker its actors started has sent its probes.
    pub(crate) fn settle_workers() {
        let pending = || WORKERS.with(|w| w.borrow().iter().any(|d| !d.load(Ordering::Acquire)));
        while pending() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub(in super::super) fn snapshot_built() {
        SNAPSHOTS.with(|built| built.set(built.get() + 1));
    }

    /// Snapshots the actors on this thread built so far, each one a read of the store and history.
    pub(crate) fn snapshots() -> usize {
        SNAPSHOTS.with(Cell::get)
    }

    pub(super) fn at_retire() {
        AT_RETIRE.with(|hook| hook.borrow_mut().as_mut().map(|hook| hook()));
    }

    /// Runs `hook` just before each retirement write on this thread until the guard drops.
    pub(crate) fn on_retire(hook: impl FnMut() + 'static) -> impl Drop {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                AT_RETIRE.with(|slot| slot.borrow_mut().take());
            }
        }
        AT_RETIRE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Clear
    }
}
