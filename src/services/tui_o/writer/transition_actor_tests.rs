//! An operator's transition cleanup sent to a running actor through its run's mailbox, over a
//! real store, real readers and the actor's own loop.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, TimeDelta, Utc};

use super::*;
use crate::config::session_hosts::ForcedSessionHosts;
use crate::services::claude_tui::source_verify::{SourceHistory, Visit};
use crate::services::tui_o::store::CURSOR_DIR;
use crate::services::tui_o::store::fault::{self, Step};
use crate::services::tui_o::store::rotation::{Rotation, Successor};
use crate::services::tui_o::store::spool::{Cursor, ExecutionEnd};
use crate::services::tui_o::writer::actor::{
    Owing, force_execution_for_tests, spawn_projecting, transition_hooks as hooks,
};
use crate::services::tui_o::writer::rotation::RETIRED_WATCH;
use crate::services::tui_o::writer::rotation::transition::Watch;
use crate::services::tui_o::writer::transition_cleanup::scope::approve;
use crate::services::tui_o::writer::transition_cleanup::{
    Home, Owed, Refusal, Request, Snapshot, Termination, Unclear, Verdict,
};
use crate::services::tui_o::writer::transition_port::{
    self, Context, Hold, Kind, Operation, Outcome, Refused, Reply, ScopeDigest, TransitionPort,
};
use crate::services::tui_o::writer::{ReaderConfig, WriterConfig};

const JOB: &str = "hrot-job";
/// A well-formed digest of no scope.
const UNSCOPED: &str =
    "hrot-scope-v1:0000000000000000000000000000000000000000000000000000000000000000";

/// A channel switched over source 0, then `hops` sources each bound from the one before in one
/// pane by a launch record no hook made, so no hop is proven. The last source is current.
struct Chain {
    harness: Harness,
    bindings: Arc<FakeBindings>,
    pane: String,
    sources: Vec<SourceId>,
    paths: Vec<PathBuf>,
    _hosts: Option<ForcedSessionHosts>,
}

fn chain(hops: usize) -> Chain {
    chain_on(hops, true)
}

/// `herdr` configures the channel, so its POSTs are counted and its rotation count published.
fn chain_on(hops: usize, herdr: bool) -> Chain {
    let hop = |n: usize| (format!("h{n}"), row(&format!("n{n}"), &format!("h{n} one")));
    chain_named(hops, herdr, hop)
}

/// As `chain_on`, with hop `n`'s session and transcript body from `hop(n)`.
fn chain_named(hops: usize, herdr: bool, hop: impl Fn(usize) -> (String, Vec<u8>)) -> Chain {
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
    fn hops(&self) -> u64 {
        self.sources.len() as u64 - 1
    }

    fn request(&self, job: &str) -> Request {
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

    fn context(&self) -> Context {
        Context {
            codex: None,
            tmux: self.pane.clone(),
            nonce: "nonce".into(),
            lock: Arc::default(),
        }
    }

    /// The digest an Inspect of `request` gives the chain as it was built, every old source's hop
    /// at its seq in the pane; `None` where an Inspect gives none.
    fn digest(&self, request: &Request) -> Option<ScopeDigest> {
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
    fn apply(&self, request: Request) -> Operation {
        let digest = self
            .digest(&request)
            .unwrap_or_else(|| ScopeDigest::parse(UNSCOPED).unwrap());
        Operation::Apply(request, digest)
    }

    /// The pane's execution holds only the current source.
    fn current(&self) -> impl Drop {
        let current = self.sources.last().unwrap();
        force_execution_for_tests(Ok(execution(current, Utc::now())))
    }

    fn cursor(&self, n: usize) -> Cursor {
        self.harness
            .channel()
            .cursor(&self.sources[n])
            .cloned()
            .unwrap()
    }

    fn key(&self, n: usize) -> String {
        source_key(&self.sources[n])
    }

    fn dir(&self) -> PathBuf {
        let runtime = self.paths[0].parent().unwrap();
        runtime.join("o_store").join(CHANNEL.to_string())
    }

    fn cursor_file(&self, n: usize) -> Option<Vec<u8>> {
        let name = format!("{}.json", self.key(n));
        std::fs::read(self.dir().join(CURSOR_DIR).join(name)).ok()
    }

    fn audit(&self, n: usize) -> Option<String> {
        let cursor = self.cursor(n);
        cursor.transition_cleanup.map(|audit| audit.job_id)
    }
}

/// Every file of the store directory `dir`, by path.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
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

fn execution(current: &SourceId, seen: DateTime<Utc>) -> (Option<SourceId>, SourceHistory) {
    let visit = Visit {
        session: current.session_id.clone(),
        pin: Some(current.clone()),
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

struct Run {
    port: TransitionPort,
    stop: watch::Sender<bool>,
    /// The host's stop, which the actor's own stop follows only later.
    host: watch::Sender<bool>,
    task: Actor,
    unsettled: watch::Receiver<Option<usize>>,
}

fn run(chain: &Chain) -> Run {
    run_with(chain, None, Arc::default())
}

/// The actor of `chain` with its run's mailbox. Its probes are stamped with a wall clock that
/// follows tokio's paused time, read only once `held` is clear.
fn run_with(chain: &Chain, readers: Option<ReaderConfig>, held: Arc<AtomicBool>) -> Run {
    run_writer(
        chain,
        readers,
        held,
        chain.harness.writer(),
        ShadowProvider::Claude,
    )
}

fn run_writer(
    chain: &Chain,
    readers: Option<ReaderConfig>,
    held: Arc<AtomicBool>,
    writer: Writer,
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
    let config = WriterConfig { enabled: true };
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
async fn settle(chain: &Chain) {
    for _ in 0..40 {
        polls(1).await;
        if chain.harness.port.posts().len() as u64 >= chain.hops() {
            return;
        }
    }
    panic!("unsettled: {:?}", chain.harness.alarms.taken());
}

type Asked = tokio::task::JoinHandle<Result<Reply, String>>;

fn send(run: &Run, operation: Operation, context: Context) -> Asked {
    let port = run.port.clone();
    tokio::spawn(async move { port.request(operation, context).await })
}

async fn ask(run: &Run, operation: Operation, context: Context) -> Reply {
    answered(send(run, operation, context)).await
}

async fn answered(asked: Asked) -> Reply {
    answered_after(asked, 0, || {}).await.unwrap()
}

/// Runs the actor until it answers `asked`, letting each probe worker finish, and calls `between`
/// once `at` workers of the job have sent their probes.
async fn answered_after(
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

fn outcomes(reply: &Reply) -> BTreeMap<String, Outcome> {
    let outcome = |hop: &transition_port::Hop| (hop.judged.key.clone(), hop.outcome.clone());
    reply.hops.iter().map(outcome).collect()
}

fn grown(harness: &Harness) -> Vec<WriterAlarm> {
    harness.alarms.taken().into_iter().filter(grew).collect()
}

// Once both probes held, one cursor write per hop retires it with its audit, the init-only source
// included; the lock is held and the count closed only around the writes, which touch nothing else.
#[tokio::test(start_paused = true)]
async fn transition_apply_retires_each_old_hop_with_its_audit_once_both_probes_held() {
    let chain = chain(2);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    assert_eq!(*actor.unsettled.borrow(), Some(2), "[T1:before]");
    assert_eq!(chain.cursor_file(0), None, "[T8:init_only]");
    let (origin, before) = (chain.cursor(0), files(&chain.dir()));
    let context = chain.context();
    let lock = Arc::clone(&context.lock);
    type Seen = (Option<usize>, BTreeMap<PathBuf, Vec<u8>>, bool);
    let seen: Rc<RefCell<Vec<Seen>>> = Rc::default();
    let (record, dir, projection) = (Rc::clone(&seen), chain.dir(), actor.unsettled.clone());
    let held = Arc::clone(&lock);
    let _hook = hooks::on_retire(move || {
        let locked = held.try_lock().is_err();
        record
            .borrow_mut()
            .push((*projection.borrow(), files(&dir), locked));
    });
    let asked = send(&actor, chain.apply(chain.request(JOB)), context);
    let free = Rc::new(RefCell::new(None));
    let probing = Rc::clone(&free);
    let reply = answered_after(asked, 1, move || {
        *probing.borrow_mut() = Some(lock.try_lock().is_ok());
    })
    .await
    .unwrap();
    for n in 0..2 {
        let cursor = chain.cursor(n);
        assert!(cursor.retired, "[T1:retired] {n}");
        let audit = cursor.transition_cleanup.clone().expect("[T1:audit]");
        let verified = (audit.job_id.as_str(), audit.hop_seq, audit.verified.offset);
        assert_eq!(
            verified,
            (JOB, n as u64 + 2, cursor.captured_through),
            "[T1:audit] {n}"
        );
        let written: Cursor = serde_json::from_slice(&chain.cursor_file(n).unwrap()).unwrap();
        assert_eq!(written, cursor, "[T1:one_cursor] {n}");
    }
    let reopened = chain.cursor(0);
    let kept = (reopened.captured_through, &reopened.prefix_hash);
    assert_eq!(
        kept,
        (origin.captured_through, &origin.prefix_hash),
        "[T8:origin]"
    );
    let seen = seen.borrow().clone();
    assert_eq!(seen.len(), 2, "[T1:writes]");
    assert_eq!(seen[0].1, before, "[T1:nothing_written_while_probing]");
    let closed = seen
        .iter()
        .all(|(projected, _, locked)| projected.is_none() && *locked);
    assert!(closed, "[T4:closed_while_applying]");
    assert_eq!(*free.borrow(), Some(true), "[T4:free_while_probing]");
    let after = files(&chain.dir());
    let changed: Vec<&PathBuf> = (after.iter())
        .filter(|(path, bytes)| seen[0].1.get(*path) != Some(*bytes))
        .map(|(path, _)| path)
        .collect();
    let cursors = [0, 1].map(|n| {
        chain
            .dir()
            .join(CURSOR_DIR)
            .join(format!("{}.json", chain.key(n)))
    });
    let mut expected: Vec<&PathBuf> = cursors.iter().collect();
    expected.sort();
    assert_eq!(changed, expected, "[T1:only_the_two_cursors]");
    let done = BTreeMap::from([
        (chain.key(0), Outcome::Retired(Watch::Fd)),
        (chain.key(1), Outcome::Retired(Watch::Fd)),
    ]);
    assert_eq!(outcomes(&reply), done, "[T1:outcomes]");
    assert_eq!(
        (reply.refused.clone(), reply.unsettled),
        (None, Some(0)),
        "[T1:reply]"
    );
    let summary = reply.summary.clone().unwrap();
    assert_eq!(
        (summary.unsettled, summary.already_done),
        (0, 2),
        "[T1:summary]"
    );
    let held = vec![chain.sources[2].clone()];
    assert_eq!(reply.execution, Some(Ok(held)), "[T1:execution]");
    polls(1).await;
    assert_eq!(*actor.unsettled.borrow(), Some(0), "[T1:projected]");
    assert_eq!(chain.harness.alarms.readers(), Some(1), "[T1:not_read]");
    halt(actor.stop, actor.task).await;
    let actor = run(&chain);
    polls(3).await;
    assert_eq!(*actor.unsettled.borrow(), Some(0), "[T8:reopened]");
    assert_eq!(chain.audit(0).as_deref(), Some(JOB), "[T8:reopened]");
    halt(actor.stop, actor.task).await;
}

/// Writes the boundary record of each old source in `legacy` as a store from before hop fields
/// were kept: only its successor's identity.
fn strip_hop_fields(chain: &Chain, legacy: &[usize]) {
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

// The channel's 59 hops, also with three seq-less successors: an unchecked end writes nothing, a
// checked one retires all 59 through the actor and leaves nothing unsettled.
#[tokio::test(start_paused = true)]
async fn transition_apply_settles_all_fifty_nine_hops_of_both_fixtures() {
    for legacy in [&[][..], &[30, 31, 32][..]] {
        let chain = chain(59);
        let _current = chain.current();
        let mut actor = run(&chain);
        settle(&chain).await;
        if !legacy.is_empty() {
            halt(actor.stop, actor.task).await;
            strip_hop_fields(&chain, legacy);
            actor = run(&chain);
            polls(3).await;
        }
        let before = files(&chain.dir());
        let mut unchecked = chain.request("hrot-unchecked");
        unchecked.termination = None;
        let refused = ask(&actor, chain.apply(unchecked.clone()), chain.context()).await;
        let reply = ask(&actor, Operation::Inspect(unchecked), chain.context()).await;
        assert_eq!(
            files(&chain.dir()),
            before,
            "[59:unchecked_writes] {legacy:?}"
        );
        let unscoped =
            Refused::Unread("inspect_scope_changed: no checked end: inspect only".into());
        assert_eq!(refused.refused, Some(unscoped), "[59:unchecked_apply]");
        assert!(matches!(reply.scope, Some(Err(_))), "[59:unchecked_scope]");
        let unknown = Verdict::Unknown(Unclear::NoTermination);
        let refused = (reply.hops.iter())
            .all(|hop| hop.judged.verdict == unknown && hop.outcome == Outcome::Unchanged);
        assert!(
            refused && reply.hops.len() == 59,
            "[59:unchecked] {:?}",
            reply.hops
        );
        let reply = ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
        let retired = (0..59).filter(|n| chain.cursor(*n).retired && chain.audit(*n).is_some());
        assert_eq!(retired.count(), 59, "[59:retired] {legacy:?}");
        for n in legacy {
            let audit = chain.cursor(*n).transition_cleanup.unwrap();
            assert_eq!(audit.hop_seq, *n as u64 + 2, "[59:legacy_seq] {n}");
        }
        let all = (reply.hops.iter()).all(|hop| hop.outcome == Outcome::Retired(Watch::Fd));
        assert!(all, "[59:outcomes] {legacy:?}");
        assert_eq!(reply.unsettled, Some(0), "[59:unsettled] {legacy:?}");
        polls(1).await;
        assert_eq!(
            *actor.unsettled.borrow(),
            Some(0),
            "[59:projected] {legacy:?}"
        );
        halt(actor.stop, actor.task).await;
    }
}

// A job probes at most 64 candidates, sources still to retire first, so a second job retires
// what the first held at the limit.
#[tokio::test(start_paused = true)]
async fn transition_job_probes_at_most_sixty_four_candidates() {
    let chain = chain(65);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let base = hooks::started();
    let reply = ask(
        &actor,
        Operation::Inspect(chain.request(JOB)),
        chain.context(),
    )
    .await;
    let held = (reply.hops.iter()).filter(|hop| hop.outcome == Outcome::Held(Hold::Limit));
    assert_eq!(held.count(), 1, "[B4:candidates]");
    let probed =
        (reply.hops.iter()).filter(|hop| matches!(hop.judged.verdict, Verdict::Eligible(_)));
    assert_eq!(probed.count(), 64, "[B4:probed]");
    assert_eq!(hooks::started() - base, 32, "[B4:batches]");
    assert!(
        (0..65).all(|n| !chain.cursor(n).retired),
        "[B4:inspect_writes]"
    );
    let apply = || ask(&actor, chain.apply(chain.request(JOB)), chain.context());
    apply().await;
    let reply = apply().await;
    let done = (0..65).filter(|n| chain.cursor(*n).retired && chain.audit(*n).is_some());
    assert_eq!(done.count(), 65, "[B4:second_job_retires_the_rest]");
    assert_eq!(reply.unsettled, Some(0), "[B4:second_job_settles]");
    halt(actor.stop, actor.task).await;
}

/// Moves paused time on by `count` polls by hand, as a probe worker still running holds it.
async fn steps(count: u32) {
    for _ in 0..count {
        tokio::time::advance(POLL_INTERVAL).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }
}

/// Clears the hold on the probe workers however the test ends.
struct Release(Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// A slow probe keeps one worker, past the deadline too, while requests are answered and posts go
// on; probes back after the deadline hold every source unapplied.
#[tokio::test(start_paused = true)]
async fn transition_probes_run_on_one_worker_however_slow_it_is() {
    let chain = chain(5);
    let _current = chain.current();
    let held = Arc::new(AtomicBool::new(false));
    let release = Release(Arc::clone(&held));
    let actor = run_with(&chain, None, Arc::clone(&held));
    settle(&chain).await;
    held.store(true, Ordering::Release);
    let base = hooks::started();
    let asked = send(&actor, chain.apply(chain.request(JOB)), chain.context());
    steps(5).await;
    assert_eq!(hooks::started() - base, 1, "[B4:one_worker]");
    append(&chain.paths[5], &row("n9", "still posting"));
    let again = send(&actor, chain.apply(chain.request(JOB)), chain.context());
    let other = send(
        &actor,
        Operation::Inspect(chain.request("other")),
        chain.context(),
    );
    steps(3).await;
    let last = chain.harness.port.posts().last().cloned();
    assert_eq!(last.as_deref(), Some("still posting"), "[B4:posting]");
    let again = again.await.unwrap().unwrap();
    assert_eq!(again.refused, Some(Refused::InProgress), "[B4:repeat]");
    assert_eq!(again.hops.len(), 5, "[B4:receipt]");
    let other = other.await.unwrap().unwrap();
    assert_eq!(other.refused, Some(Refused::Busy), "[B4:other_job]");
    steps(61).await;
    assert_eq!(hooks::started() - base, 1, "[B4:past_deadline]");
    drop(release);
    let reply = answered(asked).await;
    assert_eq!(hooks::started() - base, 1, "[B4:no_second_round]");
    assert!((0..5).all(|n| !chain.cursor(n).retired), "[B4:unapplied]");
    let limit = (reply.hops.iter()).all(|hop| hop.outcome == Outcome::Held(Hold::Limit));
    assert!(limit, "[B4:deadline] {:?}", reply.hops);
    halt(actor.stop, actor.task).await;
}

// A second probe back only past the deadline stays unrecorded: the source is held at the limit
// and nothing is written.
#[tokio::test(start_paused = true)]
async fn transition_second_probe_back_past_the_deadline_writes_nothing() {
    let chain = chain(1);
    let _current = chain.current();
    let held = Arc::new(AtomicBool::new(false));
    let release = Release(Arc::clone(&held));
    let actor = run_with(&chain, None, Arc::clone(&held));
    settle(&chain).await;
    let (base, before) = (hooks::started(), files(&chain.dir()));
    let asked = send(&actor, chain.apply(chain.request(JOB)), chain.context());
    for _ in 0..5 {
        if hooks::started() > base {
            break;
        }
        polls(1).await;
    }
    hooks::settle_workers();
    held.store(true, Ordering::Release);
    steps(75).await;
    assert_eq!(hooks::started() - base, 2, "[late:second_probe]");
    drop(release);
    let reply = answered(asked).await;
    assert_eq!(files(&chain.dir()), before, "[late:bytes]");
    assert_eq!(chain.audit(0), None, "[late:audit]");
    let outcome = outcomes(&reply)[&chain.key(0)].clone();
    assert_eq!(outcome, Outcome::Held(Hold::Limit), "[late:limit]");
    halt(actor.stop, actor.task).await;
}

// A failed second write keeps the first retirement, stops the actor and leaves the third untried;
// the same job asked again writes only the two it did not finish.
#[tokio::test(start_paused = true)]
async fn transition_apply_keeps_what_it_retired_before_a_failed_write_and_resumes_the_rest() {
    let chain = chain(3);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let (dir, mut writes, plants) = (chain.dir(), 0, Rc::new(RefCell::new(Vec::new())));
    let planted = Rc::clone(&plants);
    let hook = hooks::on_retire(move || {
        writes += 1;
        if writes == 2 {
            let full = std::io::ErrorKind::StorageFull;
            planted
                .borrow_mut()
                .push(fault::plant(&dir, Step::Write, full, Some(1)));
        }
    });
    let reply = ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
    drop(hook);
    let done = outcomes(&reply);
    let with = |wanted: Outcome| (0..3).find(|n| done[&chain.key(*n)] == wanted);
    let retired: Vec<usize> = (0..3).filter(|n| chain.cursor(*n).retired).collect();
    let first = with(Outcome::Retired(Watch::Fd));
    assert_eq!(retired.first().copied(), first, "[T2:kept]");
    assert_eq!(retired.len(), 1, "[T2:kept]");
    let cause = actor.task.await.unwrap().unwrap();
    assert_eq!(
        cause.io,
        Some(std::io::ErrorKind::StorageFull),
        "[T2:stopped]"
    );
    let (failed, untried) = (with(Outcome::Unknown), with(Outcome::Held(Hold::NotTried)));
    assert!(
        failed.is_some() && untried.is_some(),
        "[T2:outcomes] {done:?}"
    );
    assert_eq!(reply.unsettled, None, "[T2:unsettled]");
    let first = first.unwrap();
    let kept = chain.cursor_file(first);
    let actor = run(&chain);
    polls(3).await;
    let reply = ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
    assert!((0..3).all(|n| chain.cursor(n).retired), "[T2:resumed]");
    assert_eq!(chain.cursor_file(first), kept, "[T2:not_rewritten]");
    let done = outcomes(&reply);
    assert_eq!(
        done[&chain.key(first)],
        Outcome::AlreadyDone,
        "[T2:receipt]"
    );
    assert_eq!(reply.unsettled, Some(0), "[T2:settled]");
    halt(actor.stop, actor.task).await;
}

// A failed directory sync after the rename is no retirement though the file shows it: the actor
// stops, nothing projects 0, and only a reopened store answers the job as done.
#[tokio::test(start_paused = true)]
async fn transition_apply_whose_directory_sync_fails_is_unknown_until_the_store_reopens() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let (dir, plants) = (chain.dir(), Rc::new(RefCell::new(Vec::new())));
    let planted = Rc::clone(&plants);
    let hook = hooks::on_retire(move || {
        let full = std::io::ErrorKind::StorageFull;
        planted
            .borrow_mut()
            .push(fault::plant(&dir, Step::ReplaceDirSync, full, Some(1)));
    });
    let reply = ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
    drop(hook);
    assert_eq!(*actor.unsettled.borrow(), None, "[T5:projection]");
    let cause = actor.task.await.unwrap().unwrap();
    let halted = matches!(cause.alarm, WriterAlarm::Halted { .. });
    assert!(
        halted && cause.io == Some(std::io::ErrorKind::StorageFull),
        "[T5:stopped] {cause:?}"
    );
    let on_disk: Cursor = serde_json::from_slice(&chain.cursor_file(0).unwrap()).unwrap();
    let audit = on_disk.transition_cleanup.map(|audit| audit.job_id);
    assert!(
        on_disk.retired && audit.as_deref() == Some(JOB),
        "[T5:disk]"
    );
    assert_eq!(
        outcomes(&reply)[&chain.key(0)],
        Outcome::Unknown,
        "[T5:outcome]"
    );
    assert_eq!(reply.unsettled, None, "[T5:unsettled]");
    let memory = reply.hops[0].judged.verdict.clone();
    assert!(
        !matches!(memory, Verdict::AlreadyDone),
        "[T5:memory] {memory:?}"
    );
    let actor = run(&chain);
    polls(3).await;
    let reply = ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
    assert_eq!(
        outcomes(&reply)[&chain.key(0)],
        Outcome::AlreadyDone,
        "[T5:reopened]"
    );
    assert_eq!(reply.unsettled, Some(0), "[T5:reopened]");
    halt(actor.stop, actor.task).await;
}

// A change after the probes to the cursor, reader, successor, checkpoint, execution or file holds
// the source unwritten while the actor goes on.
#[tokio::test(start_paused = true)]
async fn transition_apply_holds_a_source_whose_inputs_changed_after_its_probes() {
    let cases = [
        ("append", 1),
        ("torn", 1),
        ("proven", 1),
        ("checkpoint", 1),
        ("execution", 1),
        ("path", 2),
    ];
    for (case, at) in cases {
        let chain = chain(1);
        let current = chain.current();
        let actor = run(&chain);
        settle(&chain).await;
        let asked = send(&actor, chain.apply(chain.request(JOB)), chain.context());
        let swapped = Rc::new(RefCell::new(None));
        let swap = Rc::clone(&swapped);
        let (old, new) = (chain.paths[0].clone(), chain.sources[1].clone());
        let (bindings, pane) = (Arc::clone(&chain.bindings), chain.pane.clone());
        let reply = answered_after(asked, at, move || match case {
            "append" => append(&old, &row("m9", "old late")),
            "torn" => append(&old, br#"{"type":"assistant""#),
            "proven" => bindings.commit(resumed(3, &pane, &new)),
            "checkpoint" => {
                let detail = "regression".to_string();
                bindings.commit(event(
                    3,
                    BindingRecord::Rejected {
                        detail,
                        closes: None,
                    },
                    Utc::now(),
                ));
            }
            "execution" => {
                let later = Utc::now() + TimeDelta::seconds(1);
                *swap.borrow_mut() = Some(force_execution_for_tests(Ok(execution(&new, later))));
            }
            _ => {
                let past = filetime::FileTime::from_unix_time(1_700_000_000, 0);
                filetime::set_file_mtime(&old, past).unwrap();
            }
        })
        .await;
        let alarms = chain.harness.alarms.taken();
        let reply = reply.unwrap_or_else(|error| panic!("[T6:{case}] {error}: {alarms:?}"));
        assert_eq!(chain.audit(0), None, "[T6:{case}] written");
        let outcome = outcomes(&reply)[&chain.key(0)].clone();
        assert_eq!(outcome, Outcome::Held(Hold::Stale), "[T6:{case}]");
        if case == "append" {
            let last = chain.harness.port.posts().last().cloned();
            assert_eq!(last.as_deref(), Some("old late"), "[T6:{case}] posted");
        }
        drop(swapped.borrow_mut().take());
        drop(current);
        halt(actor.stop, actor.task).await;
    }
}

// A request dropped before mailbox admission changes nothing; an admitted job continues
// after its requester leaves, until the work completes or its actor ends.
#[tokio::test(start_paused = true)]
async fn transition_job_runs_to_completion_after_the_requester_leaves() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let base = hooks::started();
    drop(
        actor
            .port
            .request(chain.apply(chain.request(JOB)), chain.context()),
    );
    polls(3).await;
    assert_eq!(hooks::started() - base, 0, "[T4:dropped_before]");
    let asked = send(&actor, chain.apply(chain.request(JOB)), chain.context());
    for _ in 0..5 {
        polls(1).await;
        hooks::settle_workers();
        if hooks::started() > base {
            break;
        }
    }
    assert_eq!(hooks::started() - base, 1, "[T4:taken]");
    asked.abort();
    for _ in 0..40 {
        polls(1).await;
        hooks::settle_workers();
    }
    assert_eq!(chain.audit(0).as_deref(), Some(JOB), "[T4:ran_to_its_end]");
    let asked = send(
        &actor,
        Operation::Inspect(chain.request("next")),
        chain.context(),
    );
    polls(1).await;
    hooks::settle_workers();
    halt(actor.stop, actor.task).await;
    let ended = asked.await.unwrap().err();
    assert_eq!(
        ended.as_deref(),
        Some("actor reply unavailable"),
        "[T4:actor_ended]"
    );
}

// A stop, the actor's or only its host's so far, raised while the retiring poll awaits a POST is
// read again before the write: nothing is written and the job answers with Stopping.
#[tokio::test(start_paused = true)]
async fn transition_apply_writes_nothing_once_a_stop_comes_while_delivery_is_held() {
    for host in [false, true] {
        let chain = chain(1);
        let _current = chain.current();
        let held = Arc::new(AtomicBool::new(false));
        let release = Release(Arc::clone(&held));
        let actor = run_with(&chain, None, Arc::clone(&held));
        settle(&chain).await;
        let context = chain.context();
        let (base, kept) = (hooks::started(), chain.cursor_file(0));
        let asked = send(&actor, chain.apply(chain.request(JOB)), context);
        for _ in 0..5 {
            if hooks::started() > base {
                break;
            }
            polls(1).await;
        }
        hooks::settle_workers();
        held.store(true, Ordering::Release);
        let (port, lease) = (&chain.harness.port, &chain.harness.lease);
        lease.busy.store(true, Ordering::SeqCst);
        append(&chain.paths[1], &row("n9", "held back"));
        for _ in 0..30 {
            if hooks::started() - base == 2 {
                break;
            }
            steps(1).await;
        }
        assert_eq!(hooks::started() - base, 2, "[stop:{host}:second_probe]");
        drop(release);
        hooks::settle_workers();
        for _ in 0..5 {
            if actor.unsettled.borrow().is_none() {
                break;
            }
            steps(1).await;
        }
        assert_eq!(*actor.unsettled.borrow(), None, "[stop:{host}:applying]");
        let (unhold, hold) = tokio::sync::oneshot::channel();
        port.lazy.store(true, Ordering::SeqCst);
        *port.hold.lock().unwrap() = Some(hold);
        lease.busy.store(false, Ordering::SeqCst);
        for _ in 0..5 {
            if !port.started.lock().unwrap().is_empty() {
                break;
            }
            steps(1).await;
        }
        assert_eq!(
            *port.started.lock().unwrap(),
            ["http"],
            "[stop:{host}:posting]"
        );
        let stop = if host { &actor.host } else { &actor.stop };
        stop.send_replace(true);
        unhold.send(()).unwrap();
        let reply = asked.await.unwrap().unwrap();
        assert_eq!(chain.audit(0), None, "[stop:{host}:audit]");
        assert_eq!(chain.cursor_file(0), kept, "[stop:{host}:cursor]");
        let outcome = outcomes(&reply)[&chain.key(0)].clone();
        assert_eq!(
            outcome,
            Outcome::Held(Hold::Stopping),
            "[stop:{host}:outcome]"
        );
        let last = port.posts().last().cloned();
        assert_eq!(
            last.as_deref(),
            Some("held back"),
            "[stop:{host}:delivered]"
        );
        actor.stop.send_replace(true);
        actor.task.await.unwrap();
    }
}

// Output the actor still queues refuses every hop until it is delivered.
#[tokio::test(start_paused = true)]
async fn transition_judges_what_the_actor_still_owes_from_its_own_state() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    chain.harness.lease.busy.store(true, Ordering::SeqCst);
    append(&chain.paths[1], &row("n9", "held back"));
    polls(3).await;
    let reply = ask(
        &actor,
        Operation::Inspect(chain.request(JOB)),
        chain.context(),
    )
    .await;
    let queued = Verdict::Refused(Refusal::OutputOwed(Owed::Queue));
    assert_eq!(reply.hops[0].judged.verdict, queued, "[T7:queue]");
    chain.harness.lease.busy.store(false, Ordering::SeqCst);
    polls(3).await;
    let reply = ask(
        &actor,
        Operation::Inspect(chain.request(JOB)),
        chain.context(),
    )
    .await;
    let eligible = matches!(reply.hops[0].judged.verdict, Verdict::Eligible(_));
    assert!(eligible, "[T7:delivered] {:?}", reply.hops);
    halt(actor.stop, actor.task).await;
}

// A channel whose POSTs this process does not count is unknown, never nothing owed.
#[tokio::test(start_paused = true)]
async fn transition_judges_uncounted_posts_unknown() {
    let chain = chain_on(1, false);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    crate::services::tui_o::writer::deliver::forget_posts_for_test(CHANNEL);
    let reply = ask(
        &actor,
        Operation::Inspect(chain.request(JOB)),
        chain.context(),
    )
    .await;
    let unread = &reply.hops[0].judged.verdict;
    let unknown = matches!(unread, Verdict::Unknown(Unclear::ObligationsUnread(_)));
    assert!(unknown, "[T7:posting_unread] {unread:?}");
    halt(actor.stop, actor.task).await;
}

// A job's audit is compared whole after a reopen; a cleaned source that grows reads again with an
// alarm naming the job, and the same job asked again writes nothing for it.
#[tokio::test(start_paused = true)]
async fn transition_job_is_compared_whole_and_never_applied_again_to_a_source_read_again() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
    assert!(chain.cursor(0).retired);
    halt(actor.stop, actor.task).await;
    let actor = run(&chain);
    polls(3).await;
    type Change = (&'static str, fn(&mut Request));
    let changes: [Change; 4] = [
        ("operator", |r| r.operator = "another".into()),
        ("reason", |r| r.reason = "another".into()),
        ("cutoff", |r| r.cutoff += 1),
        ("termination", |r| r.termination = None),
    ];
    let kept = chain.cursor_file(0);
    for (case, change) in changes {
        let mut request = chain.request(JOB);
        change(&mut request);
        let reply = ask(&actor, chain.apply(request), chain.context()).await;
        assert_eq!(chain.cursor_file(0), kept, "[B3:{case}] written");
        assert_eq!(reply.refused, Some(Refused::JobConflict), "[B3:{case}]");
    }
    let mut later = chain.request(JOB);
    later.at += TimeDelta::hours(1);
    let reply = ask(&actor, chain.apply(later), chain.context()).await;
    assert_eq!(chain.cursor_file(0), kept, "[B3:retry] written");
    assert_eq!(
        outcomes(&reply)[&chain.key(0)],
        Outcome::AlreadyDone,
        "[B3:retry]"
    );
    let posted = chain.harness.port.posts().len();
    append(&chain.paths[0], &row("m5", "old grew"));
    polls(3).await;
    assert!(!chain.cursor(0).retired, "[A1:read_again]");
    assert_eq!(chain.audit(0).as_deref(), Some(JOB), "[A1:audit_kept]");
    assert_eq!(
        chain.harness.port.posts()[posted..],
        ["old grew"],
        "[A1:from_old_end]"
    );
    let alarm = WriterAlarm::RetiredSourceGrew {
        source: chain.sources[0].clone(),
        cleanup_history: Some(JOB.into()),
    };
    assert_eq!(grown(&chain.harness), [alarm], "[A3:alarm]");
    assert_eq!(*actor.unsettled.borrow(), Some(1), "[A1:unsettled]");
    let kept = chain.cursor_file(0);
    let reply = ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
    assert!(!chain.cursor(0).retired, "[B3:read_again_not_retired]");
    assert_eq!(chain.cursor_file(0), kept, "[B3:read_again_not_retired]");
    let outcome = outcomes(&reply)[&chain.key(0)].clone();
    assert_eq!(outcome, Outcome::Held(Hold::PriorCleanup), "[B3:prior]");
    halt(actor.stop, actor.task).await;
}

// The job in progress gives its receipt only to the same operation and request, `at` and scope
// aside; another operation, operator, reason, cutoff or end is a conflict that writes nothing.
#[tokio::test(start_paused = true)]
async fn transition_job_in_progress_refuses_its_id_asked_with_another_request() {
    let chain = chain(8);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    type Change = (&'static str, fn(&mut Request));
    let changes: [Change; 5] = [
        ("operator", |r| r.operator = "another".into()),
        ("reason", |r| r.reason = "another".into()),
        ("cutoff", |r| r.cutoff += 1),
        ("termination", |r| r.termination = None),
        ("at", |r| r.at += TimeDelta::hours(1)),
    ];
    let mut asks: Vec<_> = (changes.iter())
        .map(|(case, change)| {
            let mut request = chain.request(JOB);
            change(&mut request);
            (*case, chain.apply(request), chain.context())
        })
        .collect();
    let unscoped = ScopeDigest::parse(UNSCOPED).unwrap();
    asks.push((
        "operation",
        Operation::Inspect(chain.request(JOB)),
        chain.context(),
    ));
    let asked = Operation::Apply(chain.request(JOB), unscoped);
    asks.push(("digest", asked, chain.context()));
    let (port, writes, sent) = (actor.port.clone(), Rc::new(Cell::new(0)), Rc::default());
    let (count, record): (_, Rc<RefCell<Vec<_>>>) = (Rc::clone(&writes), Rc::clone(&sent));
    let _hook = hooks::on_retire(move || {
        count.set(count.get() + 1);
        for (case, operation, context) in asks.drain(..) {
            let port = port.clone();
            let asked = tokio::spawn(async move { port.request(operation, context).await });
            record.borrow_mut().push((case, asked));
        }
    });
    ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
    assert_eq!(writes.get(), 8, "[B3:in_progress] writes");
    let audited = |n: usize| {
        let audit = chain.cursor(n).transition_cleanup;
        audit.is_some_and(|a| a.operator == "operator" && a.reason == "herdr transition")
    };
    assert!((0..8).all(audited), "[B3:in_progress] audits");
    let accepted = chain.digest(&chain.request(JOB));
    for (case, asked) in sent.take() {
        let reply = asked.await.unwrap().unwrap();
        let wanted = match case {
            "at" | "digest" => Refused::InProgress,
            _ => Refused::JobConflict,
        };
        assert_eq!(reply.refused, Some(wanted), "[B3:in_progress:{case}]");
        if case == "digest" {
            let receipt = (
                reply.operation,
                reply.scope.map(|scope| scope.unwrap().digest),
            );
            assert_eq!(receipt, (Some(Kind::Apply), accepted), "[B3:receipt]");
        }
    }
    halt(actor.stop, actor.task).await;
}

// The file grown between its last check and the write: the receipt is durable, but the source
// reads again at once and the reply counts it unsettled.
#[tokio::test(start_paused = true)]
async fn transition_apply_that_finds_its_file_grown_after_the_write_reports_it_read_again() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let path = chain.paths[0].clone();
    let hook = hooks::on_retire(move || append(&path, &row("m8", "grew at the write")));
    let reply = ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
    drop(hook);
    assert!(!chain.cursor(0).retired, "[regrew:read_again]");
    assert_eq!(chain.audit(0).as_deref(), Some(JOB), "[regrew:receipt]");
    let outcome = outcomes(&reply)[&chain.key(0)].clone();
    assert_eq!(outcome, Outcome::RetiredThenGrew, "[regrew:outcome]");
    assert_eq!(reply.unsettled, Some(1), "[regrew:unsettled]");
    assert_eq!(grown(&chain.harness).len(), 1, "[regrew:alarm]");
    polls(3).await;
    let last = chain.harness.port.posts().last().cloned();
    assert_eq!(
        last.as_deref(),
        Some("grew at the write"),
        "[regrew:posted]"
    );
    halt(actor.stop, actor.task).await;
}

// A cleaned source that grows, live, past its watch, after a restart or bound back, reads again
// from its old end with its audit; only a legacy FD past its watch sees nothing, as it would.
#[tokio::test(start_paused = true)]
async fn transition_cleaned_source_that_grows_reads_again_as_a_provider_retirement_does() {
    let shapes = [
        ("legacy", None),
        ("configured", Some(ReaderConfig { dormant: false })),
        ("dormant", Some(ReaderConfig { dormant: true })),
    ];
    for (shape, config) in shapes {
        for when in ["live", "past_watch", "restart", "rebind"] {
            let case = format!("{shape}:{when}");
            let chain = chain(1);
            let _current = chain.current();
            let mut actor = run_with(&chain, config, Arc::default());
            settle(&chain).await;
            let reply = ask(&actor, chain.apply(chain.request(JOB)), chain.context()).await;
            let watch = config.map_or(Watch::Fd, |_| Watch::Path);
            let outcome = outcomes(&reply)[&chain.key(0)].clone();
            assert_eq!(outcome, Outcome::Retired(watch), "[{case}] retired");
            match when {
                "past_watch" => {
                    tokio::time::advance(RETIRED_WATCH).await;
                    polls(2).await;
                }
                "restart" => {
                    halt(actor.stop, actor.task).await;
                    actor = run_with(&chain, config, Arc::default());
                    polls(3).await;
                }
                "rebind" => {
                    let (seq, current) = (chain.hops() + 2, chain.sources.last());
                    let back = &chain.sources[0];
                    let hook = session_start();
                    let cause = BindingCause::Resume;
                    chain
                        .bindings
                        .commit(hop_at(seq, &chain.pane, current, back, cause, &hook));
                    polls(3).await;
                }
                _ => {}
            }
            chain.harness.alarms.taken();
            let posted = chain.harness.port.posts().len();
            append(&chain.paths[0], &row("m7", "old grew"));
            polls(3).await;
            let cursor = chain.cursor(0);
            let posts = chain.harness.port.posts();
            assert_eq!(chain.audit(0).as_deref(), Some(JOB), "[{case}] audit kept");
            if (shape, when) == ("legacy", "past_watch") {
                assert!(cursor.retired, "[{case}] unwatched");
                assert_eq!(posts.len(), posted, "[{case}] unwatched");
                assert_eq!(grown(&chain.harness), [], "[{case}] unwatched");
            } else {
                assert!(!cursor.retired, "[{case}] read again");
                assert_eq!(posts[posted..], ["old grew"], "[{case}] from its old end");
                let alarm = WriterAlarm::RetiredSourceGrew {
                    source: chain.sources[0].clone(),
                    cleanup_history: Some(JOB.into()),
                };
                let expected = if when == "rebind" {
                    vec![]
                } else {
                    vec![alarm]
                };
                assert_eq!(grown(&chain.harness), expected, "[{case}] alarm");
            }
            halt(actor.stop, actor.task).await;
        }
    }
}

#[path = "transition_route_tests.rs"]
mod route_tests;

#[path = "transition_apply_tests.rs"]
mod apply_tests;

#[path = "transition_automatic_tests.rs"]
mod automatic_tests;

#[tokio::test(start_paused = true)]
async fn codex_observation_real_not_found_retry_is_owed_before_due_and_after_restore() {
    let chain = chain(0);
    let _current = chain.current();
    chain.harness.gate.acquired();
    chain.harness.port.say(BOT + 1, "history readable");
    chain
        .harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(super::Reply::Unsent);
    let mut writer = chain.harness.writer();
    assert_eq!(
        writer
            .deliver(&piece("retry-observation", "owed retry"))
            .await,
        super::Step::Done
    );
    assert!(writer.has_local_retry());
    assert_eq!(writer.store().ledger().unresolved(), None);
    for reopen in 0..2 {
        let actor = run_writer(&chain, None, Arc::default(), writer, ShadowProvider::Claude);
        polls(2).await;
        tokio::time::sleep(std::time::Duration::from_secs(11)).await;
        let reply = ask(
            &actor,
            Operation::Inspect(chain.request(JOB)),
            chain.context(),
        )
        .await;
        assert_eq!(
            reply.summary.unwrap().obligations.unwrap().owed,
            1,
            "retry survives queue0 and reopen {reopen}"
        );
        assert_eq!(chain.harness.port.posts().len(), 1, "retry not due");
        halt(actor.stop, actor.task).await;
        writer = chain.harness.writer();
    }
}

#[path = "transition_actor_bo_tests.rs"]
pub(crate) mod bo;
