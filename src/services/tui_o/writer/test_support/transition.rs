//! Transition fixtures: a channel switched over a chain of unproven hops, the actor run over its
//! mailbox, and the requests and answers a test drives through it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use super::*;
use crate::config::session_hosts::ForcedSessionHosts;
use crate::services::claude_tui::source_verify::{SourceHistory, Visit};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::CURSOR_DIR;
use crate::services::tui_o::store::rotation::{Rotation, Successor};
use crate::services::tui_o::store::spool::{Cursor, ExecutionEnd, source_key};
use crate::services::tui_o::writer::actor::{
    Owing, force_execution_for_tests, spawn_projecting, transition_hooks as hooks,
};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::deliver::ChannelWriter;
use crate::services::tui_o::writer::transition_cleanup::scope::approve;
use crate::services::tui_o::writer::transition_cleanup::{Home, Request, Snapshot, Termination};
use crate::services::tui_o::writer::transition_port::{
    self, Context, Operation, Outcome, Reply, ScopeDigest, TransitionPort,
};
use crate::services::tui_o::writer::{AlarmSink, ReaderConfig, WriterConfig};
use crate::services::tui_prompt_dedupe::binding_events::SourceId;

/// A switched-over channel `a`, whose startup bind is seq 1, and an owned gateway.
pub fn started(body: &[u8]) -> (Harness, PathBuf, SourceId, Arc<FakeBindings>) {
    let (harness, path, source) = switched_over(body);
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(source.clone());
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    harness.gate.acquired();
    (harness, path, source, bindings)
}

pub fn transcript(
    beside: &Path,
    name: &str,
    session: &str,
    body: &[u8],
) -> (PathBuf, SourceId) {
    let path = beside.with_file_name(name);
    std::fs::write(&path, body).unwrap();
    let source = source_id_for(session, &path).unwrap();
    (path, source)
}

/// A bind of `new` in pane `tmux`, named by hook `hook` (empty for a record no hook made).
pub fn hop_at(
    seq: u64,
    tmux: &str,
    old: Option<&SourceId>,
    new: &SourceId,
    cause: BindingCause,
    hook: &str,
) -> BindingEvent {
    let target = BindingTarget::Source(new.clone());
    let mut event = bound(seq, old, target, cause, None);
    event.tmux_session = tmux.into();
    if let BindingRecord::Bound { evidence, .. } = &mut event.record {
        evidence.hook_event = hook.into();
    }
    event
}

pub const JOB: &str = "hrot-job";
/// A well-formed digest of no scope.
pub const UNSCOPED: &str =
    "hrot-scope-v1:0000000000000000000000000000000000000000000000000000000000000000";

/// A channel switched over source 0, then `hops` sources each bound from the one before in one
/// pane by a launch record no hook made, so no hop is proven. The last source is current.
pub struct Chain {
    pub harness: Harness,
    pub bindings: Arc<FakeBindings>,
    pub pane: String,
    pub sources: Vec<SourceId>,
    pub paths: Vec<PathBuf>,
    pub _hosts: Option<ForcedSessionHosts>,
}

pub fn chain(hops: usize) -> Chain {
    chain_on(hops, true)
}

/// `herdr` enables the channel's drain publications; both paths count POSTs.
pub fn chain_on(hops: usize, herdr: bool) -> Chain {
    let hop = |n: usize| (format!("h{n}"), row(&format!("n{n}"), &format!("h{n} one")));
    chain_named(hops, herdr, hop)
}

/// As `chain_on`, with hop `n`'s session and transcript body from `hop(n)`.
pub fn chain_named(
    hops: usize,
    herdr: bool,
    hop: impl Fn(usize) -> (String, Vec<u8>),
) -> Chain {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let pane = format!("hrot-{}", uuid::Uuid::new_v4().simple());
    let (mut sources, mut paths) = (vec![a], vec![a_path.clone()]);
    for n in 1..=hops {
        let (session, body) = hop(n);
        let (path, next) = transcript(&a_path, &format!("{session}.jsonl"), &session, &body);
        let cause = BindingCause::Startup;
        bindings.commit(hop_at(
            n as u64 + 1,
            &pane,
            sources.last(),
            &next,
            cause,
            "",
        ));
        sources.push(next);
        paths.push(path);
    }
    let hosts = || {
        let channels = [(CHANNEL, "mac-mini")];
        crate::config::session_hosts::force_for_test(Some("mac-mini"), &channels)
    };
    Chain {
        harness,
        bindings,
        pane,
        sources,
        paths,
        _hosts: herdr.then(hosts),
    }
}

impl Chain {
    pub(crate) fn hops(&self) -> u64 {
        self.sources.len() as u64 - 1
    }

    pub(crate) fn request(&self, job: &str) -> Request {
        let end = ExecutionEnd {
            host: "mac-mini".into(),
            pane: self.pane.clone(),
            first_seq: 2,
            last_seq: self.hops() + 1,
            basis: "the tmux pane is gone".into(),
        };
        Request {
            channel: CHANNEL,
            cutoff: self.hops() + 1,
            job_id: job.into(),
            operator: "operator".into(),
            at: Utc::now(),
            reason: "herdr transition".into(),
            termination: Some(Termination::Confirmed(end)),
        }
    }

    pub(crate) fn context(&self) -> Context {
        Context {
            codex: None,
            tmux: self.pane.clone(),
            nonce: "nonce".into(),
            lock: Arc::default(),
        }
    }

    /// The digest an Inspect of `request` gives the chain as it was built, every old source's hop
    /// at its seq in the pane; `None` where an Inspect gives none.
    pub(crate) fn digest(&self, request: &Request) -> Option<ScopeDigest> {
        let mut rotation = Rotation::default();
        let mut cursors = Vec::new();
        for (n, pair) in self.sources.windows(2).enumerate() {
            let next = Successor {
                seq: Some(n as u64 + 2),
                tmux_session: Some(self.pane.clone()),
                ..Successor::from(pair[1].clone())
            };
            rotation.successors.insert(self.key(n), next);
            let source = pair[0].clone();
            let (retired, transition_cleanup) = (false, None);
            let prefix_hash = String::new();
            cursors.push(Cursor {
                source,
                captured_through: 0,
                prefix_hash,
                retired,
                transition_cleanup,
            });
        }
        let snapshot = Snapshot {
            reading: 0,
            codex: None,
            channel: CHANNEL,
            provider: ShadowProvider::Claude,
            home: Home::Settled,
            rotation,
            cursors,
            pinned: Ok(None),
            execution: Ok(Vec::new()),
            sources: BTreeMap::new(),
            legacy_hops: Ok(BTreeMap::new()),
            obligations: Ok(Default::default()),
        };
        let approved = approve(&snapshot, request, &self.context());
        approved.ok().map(|scope| scope.digest)
    }

    /// An Apply of `request` under the digest its Inspect gives, or one no scope has.
    pub(crate) fn apply(&self, request: Request) -> Operation {
        let digest = self
            .digest(&request)
            .unwrap_or_else(|| ScopeDigest::parse(UNSCOPED).unwrap());
        Operation::Apply(request, digest)
    }

    /// The pane's execution holds only the current source.
    pub(crate) fn current(&self) -> impl Drop {
        let current = self.sources.last().unwrap();
        force_execution_for_tests(Ok(execution(current, Utc::now())))
    }

    pub(crate) fn cursor(&self, n: usize) -> Cursor {
        self.harness
            .channel()
            .cursor(&self.sources[n])
            .cloned()
            .unwrap()
    }

    pub(crate) fn key(&self, n: usize) -> String {
        source_key(&self.sources[n])
    }

    pub(crate) fn dir(&self) -> PathBuf {
        let runtime = self.paths[0].parent().unwrap();
        runtime.join("o_store").join(CHANNEL.to_string())
    }

    pub(crate) fn cursor_file(&self, n: usize) -> Option<Vec<u8>> {
        let name = format!("{}.json", self.key(n));
        std::fs::read(self.dir().join(CURSOR_DIR).join(name)).ok()
    }

    pub(crate) fn audit(&self, n: usize) -> Option<String> {
        let cursor = self.cursor(n);
        cursor.transition_cleanup.map(|audit| audit.job_id)
    }
}

/// Every file of the store directory `dir`, by path.
pub fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            found.insert(path.clone(), std::fs::read(&path).unwrap());
        }
    }
    found
}

pub fn execution(
    current: &SourceId,
    seen: DateTime<Utc>,
) -> (Option<SourceId>, SourceHistory) {
    let visit = Visit {
        session: current.session_id.clone(),
        pin: Some(current.clone()),
        pinned_at: None,
        since: None,
        seen,
    };
    let history = SourceHistory {
        current: Some(visit),
        awaiting: None,
        left: BTreeMap::new(),
        complete: true,
    };
    (Some(current.clone()), history)
}

pub struct Run {
    pub(crate) port: TransitionPort,
    pub(crate) stop: watch::Sender<bool>,
    /// The host's stop, which the actor's own stop follows only later.
    pub(crate) host: watch::Sender<bool>,
    pub(crate) task: Actor,
    pub(crate) unsettled: watch::Receiver<Option<usize>>,
}

pub fn run(chain: &Chain) -> Run {
    run_with(chain, None, Arc::default())
}

/// The actor of `chain` with its run's mailbox. Its probes are stamped with a wall clock that
/// follows tokio's paused time, read only once `held` is clear.
pub fn run_with(chain: &Chain, readers: Option<ReaderConfig>, held: Arc<AtomicBool>) -> Run {
    run_writer(
        chain,
        readers,
        held,
        chain.harness.writer(),
        ShadowProvider::Claude,
    )
}

pub fn run_writer<A: AlarmSink + 'static>(
    chain: &Chain,
    readers: Option<ReaderConfig>,
    held: Arc<AtomicBool>,
    writer: ChannelWriter<FakePort, Arc<FakeLease>, A>,
    provider: ShadowProvider,
) -> Run {
    let (t0, w0) = (tokio::time::Instant::now(), SystemTime::now());
    let wall: transition_port::Wall = Arc::new(move || {
        while held.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        w0 + (tokio::time::Instant::now() - t0)
    });
    let (host, stopping) = watch::channel(false);
    let gate = Arc::clone(&chain.harness.gate);
    let (port, receiver) = TransitionPort::new(1, gate, wall, stopping);
    let (stop, stopped) = watch::channel(false);
    let (published, unsettled) = watch::channel(None);
    let projections = (
        stopped,
        watch::channel(false).0,
        published,
        Owing::default(),
    );
    let config = WriterConfig {
        enabled: true,
        ..Default::default()
    };
    let bindings = Arc::clone(&chain.bindings);
    let spawn = || spawn_projecting(&config, readers, writer, provider, bindings, projections);
    let task = transition_port::spawning(receiver, spawn).unwrap();
    Run {
        port,
        stop,
        host,
        task,
        unsettled,
    }
}

/// Polls until every hop's row is posted.
pub async fn settle(chain: &Chain) {
    for _ in 0..40 {
        polls(1).await;
        if chain.harness.port.posts().len() as u64 >= chain.hops() {
            return;
        }
    }
    panic!("unsettled: {:?}", chain.harness.alarms.taken());
}

pub type Asked = tokio::task::JoinHandle<Result<Reply, String>>;

pub fn send(run: &Run, operation: Operation, context: Context) -> Asked {
    let port = run.port.clone();
    tokio::spawn(async move { port.request(operation, context).await })
}

pub async fn ask(run: &Run, operation: Operation, context: Context) -> Reply {
    answered(send(run, operation, context)).await
}

pub async fn answered(asked: Asked) -> Reply {
    answered_after(asked, 0, || {}).await.unwrap()
}

/// Runs the actor until it answers `asked`, letting each probe worker finish, and calls `between`
/// once `at` workers of the job have sent their probes.
pub async fn answered_after(
    asked: Asked,
    at: usize,
    mut between: impl FnMut(),
) -> Result<Reply, String> {
    let (base, mut called) = (hooks::started(), false);
    for _ in 0..300 {
        if !called && hooks::started() - base >= at {
            called = true;
            between();
        }
        if asked.is_finished() {
            break;
        }
        polls(1).await;
        hooks::settle_workers();
    }
    assert!(called, "the job ended before probe {at}");
    asked.await.unwrap()
}

pub fn outcomes(reply: &Reply) -> BTreeMap<String, Outcome> {
    let outcome = |hop: &transition_port::Hop| (hop.judged.key.clone(), hop.outcome.clone());
    reply.hops.iter().map(outcome).collect()
}

/// Writes the boundary record of each old source in `legacy` as a store from before hop fields
/// were kept: only its successor's identity.
pub fn strip_hop_fields(chain: &Chain, legacy: &[usize]) {
    use crate::services::tui_o::store::rotation::BOUNDARY_FILE;
    let file = chain.dir().join(BOUNDARY_FILE);
    let mut written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    for n in legacy {
        let next = written["successors"][chain.key(*n)]
            .as_object_mut()
            .unwrap();
        next.retain(|field, _| ["session_id", "path", "dev", "ino"].contains(&field.as_str()));
    }
    std::fs::write(&file, serde_json::to_vec(&written).unwrap()).unwrap();
}
