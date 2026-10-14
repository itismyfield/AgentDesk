//! The actor's side of an operator's transition cleanup. It takes one job at a time from its run's
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
use crate::services::tui_o::writer::transition_cleanup::{
    Judged, Request, Termination, Unclear, Verdict, judge,
};
use crate::services::tui_o::writer::transition_port::{
    Context, Envelope, Hold, Hop, Operation, Outcome, Receiver, Refused, Reply, Wall,
};

/// Candidates one job probes; the rest wait for a later job.
const CANDIDATES: usize = 64;
/// Sources one worker probes before the actor takes its results.
const BATCH: usize = 4;
/// Probing past this holds every source not probed twice by then.
const DEADLINE: Duration = Duration::from_secs(60);
/// Prefix bytes one job may hash: two probes of every candidate at 32 MiB each.
const HASH_BUDGET: u64 = 2 * CANDIDATES as u64 * (32 << 20);

pub(super) struct Transitions {
    receiver: Option<Receiver>,
    job: Option<Job>,
}

impl Transitions {
    pub(super) fn new(receiver: Option<Receiver>) -> Self {
        let job = None;
        Self { receiver, job }
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

type Worker = mpsc::Receiver<Vec<(String, Observation)>>;

struct Job {
    request: Request,
    apply: bool,
    context: Context,
    reply: oneshot::Sender<Reply>,
    gate: Arc<OwnershipGate>,
    wall: Wall,
    common: Common,
    /// The judgment the job started from.
    first: Vec<Judged>,
    probes: BTreeMap<String, Probe>,
    queue: VecDeque<String>,
    stage: Stage,
    worker: Option<Worker>,
    deadline: Instant,
    hashed: u64,
    outcomes: BTreeMap<String, Outcome>,
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
            ..Reply::refused(&self.request.job_id, Refused::InProgress)
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
    pub(super) fn poll_transition(&mut self, resumed: bool, unsettled: &Unsettled) {
        if !resumed || self.writer.is_stopped() {
            return;
        }
        self.take_request();
        let Some(mut job) = self.transitions.job.take() else {
            return;
        };
        if self.step(&mut job, unsettled) {
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
        let Envelope {
            operation,
            context,
            reply,
            ..
        } = envelope;
        let (apply, request) = match operation {
            Operation::Inspect(request) => (false, request),
            Operation::Apply(request) => (true, request),
        };
        let refused = match &self.transitions.job {
            Some(job) if job.request.job_id == request.job_id => {
                let _ = reply.send(job.receipt());
                return;
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
        let first = judge(&snapshot, &request).hops;
        let unprobed = |hop: &&Judged| hop.verdict == Verdict::Unknown(Unclear::NotObserved);
        let (mut probes, mut outcomes) = (BTreeMap::new(), BTreeMap::new());
        for (index, hop) in first.iter().filter(unprobed).enumerate() {
            let cursor = (snapshot.cursors.iter()).find(|c| source_key(&c.source) == hop.key);
            let Some(cursor) = cursor.filter(|_| index < CANDIDATES) else {
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
                },
            );
        }
        self.transitions.job = Some(Job {
            request,
            apply,
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
            deadline: Instant::now() + DEADLINE,
            hashed: 0,
            outcomes,
        });
    }

    /// One step of `job`; true once it has nothing left to do.
    fn step(&mut self, job: &mut Job, unsettled: &Unsettled) -> bool {
        if let Stage::Apply(queue) = &mut job.stage {
            let Some(key) = queue.pop_front() else {
                return true;
            };
            let outcome = self.retire_one(job, &key, unsettled);
            let failed = outcome == Outcome::Unknown;
            job.outcomes.insert(key, outcome);
            let Stage::Apply(queue) = &mut job.stage else {
                return true;
            };
            if failed {
                for key in queue.drain(..) {
                    job.outcomes.insert(key, Outcome::Held(Hold::NotTried));
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
            let seen = match worker.try_recv() {
                Ok(seen) => seen,
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => Vec::new(),
            };
            job.worker = None;
            let first = matches!(job.stage, Stage::First);
            for (key, seen) in seen {
                if let Some(probe) = job.probes.get_mut(&key) {
                    *(if first {
                        &mut probe.earlier
                    } else {
                        &mut probe.later
                    }) = Some(seen);
                }
            }
        }
        let now = Instant::now();
        if now >= job.deadline {
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
            if job.hashed + cursor.captured_through > HASH_BUDGET {
                continue;
            }
            job.hashed += cursor.captured_through;
            batch.push((key, cursor));
        }
        if batch.is_empty() {
            return;
        }
        let (send, worker) = mpsc::channel();
        let wall = Arc::clone(&job.wall);
        #[cfg(test)]
        let done = test_hooks::worker_started();
        tokio::task::spawn_blocking(move || {
            let seen = batch
                .into_iter()
                .map(|(key, cursor)| (key, observe(&cursor, wall())));
            let _ = send.send(seen.collect());
            #[cfg(test)]
            done.store(true, std::sync::atomic::Ordering::Release);
        });
        job.worker = Some(worker);
    }

    /// Holds the session transition lock and the pane's source authority across one retirement,
    /// both taken and released within this call.
    fn retire_one(&mut self, job: &Job, key: &str, unsettled: &Unsettled) -> Outcome {
        let Ok(_transition) = Arc::clone(&job.context.lock).try_lock_owned() else {
            return Outcome::Held(Hold::TransitionBusy);
        };
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
        let Job {
            request,
            context,
            reply,
            gate,
            probes,
            first,
            outcomes,
            ..
        } = job;
        let (hops, summary, execution) = match self.build_snapshot(&context, &gate, &probes) {
            Ok((snapshot, _)) => {
                let judgment = judge(&snapshot, &request);
                let hops = Job::hops(judgment.hops, &outcomes);
                (hops, Some(judgment.summary), Some(snapshot.execution))
            }
            Err(detail) => (Job::hops(first, &outcomes), None, Some(Err(detail))),
        };
        let stopped = self.writer.is_stopped();
        let _ = reply.send(Reply {
            job_id: request.job_id,
            refused: None,
            hops,
            summary,
            execution,
            unsettled: if stopped { None } else { self.unsettled() },
        });
        drop(context.permit);
    }
}

#[cfg(test)]
pub(crate) mod test_hooks {
    //! Lets a test wait for the probe workers, count the snapshots, and look in just before a
    //! retirement is written.
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    thread_local! {
        static WORKERS: RefCell<Vec<Arc<AtomicBool>>> = const { RefCell::new(Vec::new()) };
        static SNAPSHOTS: Cell<usize> = const { Cell::new(0) };
        static AT_RETIRE: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
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
