//! The mailbox of one actor run for an operator's transition cleanup. Each envelope names the run
//! it was sent to, so another run of the same claim neither applies nor answers it.

use std::sync::Arc;
use std::time::SystemTime;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};

use super::rotation::transition::Watch;
use super::transition_cleanup::scope::ScopeApproval;
pub(crate) use super::transition_cleanup::scope::ScopeDigest;
use super::transition_cleanup::{Judged, Summary};
pub(crate) use super::transition_cleanup::{Request, Termination};
use crate::services::cluster::channel_home::CommandPermit;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::SourceId;

/// Envelopes a run's mailbox holds before its actor takes one.
const MAILBOX: usize = 4;

/// The wall clock the actor's probes stamp, read on the worker as each probe starts.
pub(crate) type Wall = Arc<dyn Fn() -> SystemTime + Send + Sync>;

#[derive(Clone, Debug)]
pub(crate) enum Operation {
    /// Judges after both probes and writes nothing.
    Inspect(Request),
    /// Retires each source the judgment passes, one per actor poll, while the scope is still the
    /// one whose digest the Inspect gave.
    #[cfg(test)]
    Apply(Request, ScopeDigest),
    /// The periodic retention policy applies an inspected scope, with a fresh idle check.
    AutomaticApply(Request, ScopeDigest, Idle),
}

type IdleFuture = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>;

/// A runtime-owned check; a missing or unreachable mailbox never counts as idle.
#[derive(Clone)]
pub(crate) struct Idle(pub Arc<dyn Fn() -> IdleFuture + Send + Sync>);

impl std::fmt::Debug for Idle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Idle")
    }
}

/// The operation a job runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Kind {
    Inspect,
    Apply,
}

/// What the requester vouches for beside the request.
pub(crate) struct Context {
    /// The pane and spawn nonce of the execution the request was checked against.
    pub tmux: String,
    pub nonce: String,
    /// The epoch of the registered home the request was admitted under; `None` without one.
    pub home_epoch: Option<i64>,
    /// The channel's session transition lock, taken only around each retirement.
    pub lock: Arc<tokio::sync::Mutex<()>>,
    /// The home's command admission, kept until the job ends however the requester left.
    pub permit: Option<CommandPermit>,
}

pub(crate) struct Envelope {
    generation: u64,
    run: Arc<()>,
    pub(super) operation: Operation,
    pub(super) context: Context,
    pub(super) reply: oneshot::Sender<Reply>,
}

/// Why a request was answered without a job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Refused {
    ForeignRun,
    CodexDeferred,
    /// Another job holds the channel.
    Busy,
    /// This job runs already; the reply is its receipt so far.
    InProgress,
    /// The job ID names a recorded cleanup, or the job in progress, of another operator, reason,
    /// cutoff or end.
    JobConflict,
    Unread(String),
}

/// Why nothing was written for a source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Hold {
    /// Its cursor, reader, successor, file, execution or rotation changed since its probes.
    Stale,
    TransitionBusy,
    SourceBusy,
    /// A cleanup is recorded for it already, by another job or before it grew back.
    PriorCleanup,
    /// Past the candidate, hashing or time limit of one job.
    Limit,
    /// An earlier write of this job failed, so this one was not tried.
    NotTried,
    /// The actor or its host was stopping, so neither this nor the rest of the job was written.
    Stopping,
    /// The scope is no longer the one the job accepted, so neither this nor the rest was written.
    InspectScopeChanged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Outcome {
    Unchanged,
    Held(Hold),
    /// Retired with its audit durably, then watched as `Watch` says.
    Retired(Watch),
    /// This job's audit was written already.
    AlreadyDone,
    /// The retirement is durable, but the file had grown and the source reads again.
    RetiredThenGrew,
    /// The write failed: whether the cursor on disk changed is known only once the store reopens.
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Hop {
    pub judged: Judged,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Reply {
    pub job_id: String,
    pub refused: Option<Refused>,
    /// The operation of the job answering; `None` when refused without one.
    pub operation: Option<Kind>,
    /// An Inspect's scope from its final snapshot, or the scope an Apply accepted; `None` without.
    pub scope: Option<Result<ScopeApproval, String>>,
    pub hops: Vec<Hop>,
    pub summary: Option<Summary>,
    /// The sources the current execution holds, which no candidate may be.
    pub execution: Option<Result<Vec<SourceId>, String>>,
    /// The channel's `rotation_unsettled` once the job ended; `None` after a failed write.
    pub unsettled: Option<usize>,
    /// The execution the answering judgment read, from the same snapshot; `None` without one.
    pub diagnostics: Option<Diagnostics>,
}

/// The pane's execution as the final snapshot read it, and the old sources it may be.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Diagnostics {
    pub nonce: String,
    pub history: Result<HistoryView, String>,
    pub overlaps: Vec<Overlap>,
    pub candidates: Vec<Candidate>,
}

/// An old source of the judgment by its spool key, with the pane its hop resolves to, if any.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Candidate {
    pub key: String,
    pub source: SourceId,
    pub pane: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct HistoryView {
    pub complete: bool,
    pub pinned: Option<SourceId>,
    /// The current, awaiting and left sessions in that order, each with its pin if it has one.
    pub sessions: Vec<(&'static str, String, Option<SourceId>)>,
}

/// An old source sharing a path, session ID or inode with a pin, as the judgment holds it, or
/// whose session ID an unpinned session has.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Overlap {
    pub key: String,
    pub held_by: Vec<SourceId>,
    pub unpinned: Vec<String>,
}

impl Reply {
    pub(crate) fn refused(job_id: &str, refused: Refused) -> Self {
        Self {
            job_id: job_id.to_owned(),
            refused: Some(refused),
            operation: None,
            scope: None,
            hops: Vec::new(),
            summary: None,
            execution: None,
            unsettled: None,
            diagnostics: None,
        }
    }
}

/// The sending side of one run's mailbox.
#[derive(Clone)]
pub(crate) struct TransitionPort {
    suspension: super::suspension::Suspension,
    generation: u64,
    run: Arc<()>,
    send: mpsc::Sender<Envelope>,
}

pub(crate) struct Receiver {
    generation: u64,
    run: Arc<()>,
    receive: mpsc::Receiver<Envelope>,
    /// The writer's gate, which owns a channel without a registered home.
    pub(super) gate: Arc<OwnershipGate>,
    pub(super) wall: Wall,
    /// The host's stop, raised before the actor's own stop reaches it.
    pub(super) stopping: watch::Receiver<bool>,
}

impl TransitionPort {
    pub(crate) fn with_suspension(mut self, suspension: super::suspension::Suspension) -> Self {
        self.suspension = suspension;
        self
    }

    pub(crate) fn sources_unsuspended(&self) -> bool {
        #[cfg(test)]
        if crate::services::claude_tui::source_verify::n2b_mutant("b1b-transition") {
            return true;
        }
        self.suspension.is_empty()
    }

    pub(crate) fn new(
        generation: u64,
        gate: Arc<OwnershipGate>,
        wall: Wall,
        stopping: watch::Receiver<bool>,
    ) -> (Self, Receiver) {
        let (run, (send, receive)) = (Arc::new(()), mpsc::channel(MAILBOX));
        let port = Self {
            suspension: Default::default(),
            generation,
            run: Arc::clone(&run),
            send,
        };
        let receiver = Receiver {
            generation,
            run,
            receive,
            gate,
            wall,
            stopping,
        };
        (port, receiver)
    }

    /// An actor still holds this run's mailbox.
    pub(crate) fn is_open(&self) -> bool {
        !self.send.is_closed()
    }

    pub(crate) fn same_run(&self, other: &Self) -> bool {
        self.generation == other.generation && Arc::ptr_eq(&self.run, &other.run)
    }

    /// Dropped before the mailbox takes the envelope, nothing is applied; dropped after, the job
    /// runs to its end with the permit, and only the reply is lost.
    pub(crate) async fn request(
        &self,
        operation: Operation,
        context: Context,
    ) -> Result<Reply, String> {
        self.request_as(self, operation, context).await
    }

    /// [`Self::request`] naming the run of `named` while it goes into this port's mailbox.
    pub(crate) async fn request_as(
        &self,
        named: &Self,
        operation: Operation,
        context: Context,
    ) -> Result<Reply, String> {
        if !self.sources_unsuspended() {
            return Err("suspended_sources".into());
        }
        let (reply, answer) = oneshot::channel();
        let envelope = Envelope {
            generation: named.generation,
            run: Arc::clone(&named.run),
            operation,
            context,
            reply,
        };
        let sent = self.send.send(envelope).await;
        sent.map_err(|_| "actor ended".to_owned())?;
        let reply = answer
            .await
            .map_err(|_| "actor reply unavailable".to_owned())?;
        if !self.sources_unsuspended() {
            return Err("suspended_sources".into());
        }
        Ok(reply)
    }
}

impl Operation {
    pub(super) fn request(&self) -> &Request {
        match self {
            Self::Inspect(request) | Self::AutomaticApply(request, _, _) => request,
            #[cfg(test)]
            Self::Apply(request, _) => request,
        }
    }
}

impl Receiver {
    /// The next envelope sent to this run; one sent to another run is answered as foreign.
    pub(super) fn try_next(&mut self) -> Result<Option<Envelope>, mpsc::error::TryRecvError> {
        let envelope = match self.receive.try_recv() {
            Ok(envelope) => envelope,
            Err(mpsc::error::TryRecvError::Empty) => return Ok(None),
            Err(closed) => return Err(closed),
        };
        if envelope.generation != self.generation || !Arc::ptr_eq(&envelope.run, &self.run) {
            let job = &envelope.operation.request().job_id;
            let _ = (envelope.reply).send(Reply::refused(job, Refused::ForeignRun));
            return Ok(None);
        }
        Ok(Some(envelope))
    }
}

thread_local! {
    static SPAWN: std::cell::RefCell<Option<Receiver>> = const { std::cell::RefCell::new(None) };
}

/// Hands `receiver` to the actor `spawn` starts; one it did not take is dropped with the scope.
pub(crate) fn spawning<T>(receiver: Receiver, spawn: impl FnOnce() -> T) -> T {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            SPAWN.with(|slot| slot.borrow_mut().take());
        }
    }
    SPAWN.with(|slot| *slot.borrow_mut() = Some(receiver));
    let _scope = Scope;
    spawn()
}

pub(super) fn take_spawn_receiver() -> Option<Receiver> {
    SPAWN.with(|slot| slot.borrow_mut().take())
}
