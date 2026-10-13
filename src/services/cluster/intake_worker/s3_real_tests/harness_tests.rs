//! Two simulated nodes over one PG. Each node is a child test process with its own runtime root,
//! home-gate registry, writer readiness and Discord recorder; the parent owns the database.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::{self, ChannelHome, HomeError, HomeState, HomeWrite};
use crate::services::cluster::channel_home::{self, HomeGate, HomeOwnership};
use crate::services::cluster::channel_home_boot::{self, Boot, BootPort, ResetRefused};
use crate::services::cluster::channel_home_drain::DrainPort;
use crate::services::cluster::channel_home_port::ChannelHomePort;
use crate::services::cluster::intake_router_hook::{
    IntakeRouterContext, IntakeRouterDecision, try_route_intake,
};
use crate::services::cluster::intake_routing_config::{
    IntakeRoutingMode, OwnerAuthorityChannelOptIn,
};
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, binding_reader::source_id_for};
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::adoption::LegacyView;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents, BindingRecord};
use crate::services::tui_o::writer::host::test_io::{Alarms, TestHost};
use crate::services::tui_o::writer::host::{self, Custody, HostIo, HostParts};
use crate::services::tui_o::writer::{
    DeliveryLease, DiscordPort, PostOutcome, SeenMessage, WriterAlarm,
};

pub(super) const C: u64 = 4_380_901;
pub(super) const GW: &str = "gw-s3";
pub(super) const MINI: &str = "mini-s3";
const BOT: u64 = 42;

const ENV_SCENARIO: &str = "ADK_S3_SCENARIO";
const ENV_ROLE: &str = "ADK_S3_ROLE";
const ENV_DIR: &str = "ADK_S3_DIR";
const ENV_DB: &str = "ADK_S3_DB";
const ENV_SPAWN: &str = "ADK_S3_SPAWN";

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The provider name the DB rows, config and intake router use for `provider`.
pub(super) fn provider_name(provider: ShadowProvider) -> &'static str {
    match provider {
        ShadowProvider::Claude => "claude",
        ShadowProvider::Codex => "codex",
    }
}

pub(super) fn applied(write: Result<HomeWrite<ChannelHome>, HomeError>) -> ChannelHome {
    match write.expect("home write") {
        HomeWrite::Applied(home) => home,
        HomeWrite::Stale => panic!("expected the home write to apply"),
    }
}

/// What a child node reads from its parent.
pub(super) struct Env {
    pub(super) scenario: String,
    pub(super) role: String,
    pub(super) dir: PathBuf,
    pub(super) db: String,
    spawn: String,
}

impl Env {
    /// `None` outside a child spawned by [`Node::spawn`]; such a run proves nothing and returns.
    pub(super) fn read() -> Option<Self> {
        let var = |key| std::env::var(key).ok();
        Some(Self {
            scenario: var(ENV_SCENARIO)?,
            role: var(ENV_ROLE)?,
            dir: PathBuf::from(var(ENV_DIR)?),
            db: var(ENV_DB)?,
            spawn: var(ENV_SPAWN)?,
        })
    }

    /// The marker the parent requires before it counts this child as run.
    pub(super) fn finished(&self) {
        std::fs::write(self.dir.join(format!("done.{}", self.spawn)), b"ok").unwrap();
    }

    pub(super) async fn pool(&self) -> PgPool {
        let name = format!("s3 node {}", self.role);
        crate::db::postgres::connect_test_pool_with_max_connections(&self.db, &name, 4)
            .await
            .expect("node pool")
    }
}

pub(super) fn put(dir: &Path, key: &str, value: impl std::fmt::Display) {
    std::fs::write(dir.join(format!("obs.{key}")), value.to_string()).unwrap();
}

pub(super) fn get(dir: &Path, key: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(format!("obs.{key}"))).ok()
}

pub(super) fn signal(dir: &Path, name: &str) {
    std::fs::write(dir.join(format!("sig.{name}")), b"").unwrap();
}

pub(super) fn signalled(dir: &Path, name: &str) -> bool {
    dir.join(format!("sig.{name}")).exists()
}

/// Waits for `name` from the other side; a missing signal fails instead of passing on.
pub(super) async fn wait_signal(dir: &Path, name: &str, within: Duration) {
    eprintln!("[s3] waiting for signal {name}");
    let deadline = tokio::time::Instant::now() + within;
    while !signalled(dir, name) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "signal {name} never came"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Polls `done` until it holds; failing after `within` rather than passing on.
pub(super) async fn until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    eprintln!("[s3] until {what}");
    let deadline = tokio::time::Instant::now() + within;
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never reached: {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(super) async fn until_async<F: Future<Output = bool>>(
    what: &str,
    within: Duration,
    mut done: impl FnMut() -> F,
) {
    eprintln!("[s3] until {what}");
    let deadline = tokio::time::Instant::now() + within;
    while !done().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never reached: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The parent's database and the directory its nodes share.
pub(super) struct Scene {
    pub(super) dir: tempfile::TempDir,
    db: Option<TestPostgresDb>,
    pub(super) pool: PgPool,
    spawned: AtomicUsize,
    /// The provider of the channel's agent, worker capability and home row.
    pub(super) provider: ShadowProvider,
}

impl Scene {
    /// A migrated database with the channel's agent and both nodes registered as live workers.
    pub(super) async fn new() -> Self {
        Self::for_provider(ShadowProvider::Claude).await
    }

    /// As [`Scene::new`], with the channel's agent and both workers on `provider`.
    pub(super) async fn for_provider(provider: ShadowProvider) -> Self {
        let name = provider_name(provider);
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for node in [GW, MINI] {
            sqlx::query(
                "INSERT INTO worker_nodes (instance_id, status, role, effective_role, labels,
                 capabilities, last_heartbeat_at, started_at, updated_at)
                 VALUES ($1, 'online', 'worker', 'worker', '[]', $2, NOW(), NOW(), NOW())",
            )
            .bind(node)
            .bind(serde_json::json!({"intake_worker": {"enabled": true, "providers": [name]}}))
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ($1, 'Writer', $3, $2)",
        )
        .bind(format!("writer-{C}"))
        .bind(C.to_string())
        .bind(name)
        .execute(&pool)
        .await
        .unwrap();
        Self {
            dir: tempfile::tempdir().unwrap(),
            db: Some(db),
            pool,
            spawned: AtomicUsize::new(0),
            provider,
        }
    }

    pub(super) fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub(super) fn url(&self) -> &str {
        &self.db.as_ref().unwrap().database_url
    }

    /// A worker-owned home held by MINI: the gateway released it and MINI adopted it.
    pub(super) async fn worker_home(&self) -> i64 {
        let (channel, provider) = (C.to_string(), provider_name(self.provider));
        let home =
            applied(o_channel_homes::delegate(&self.pool, &channel, provider, GW, MINI).await);
        let home =
            applied(o_channel_homes::finish_release(&self.pool, &channel, GW, home.epoch).await);
        applied(o_channel_homes::adopt(&self.pool, &channel, MINI, home.epoch).await).epoch
    }

    pub(super) async fn row(&self) -> Option<ChannelHome> {
        o_channel_homes::read_home(&self.pool, &C.to_string())
            .await
            .unwrap()
    }

    pub(super) async fn drop_db(mut self) {
        self.pool.close().await;
        if let Some(db) = self.db.take() {
            db.drop().await;
        }
    }
}

/// One node as a child test process; dropped unfinished, it is killed.
pub(super) struct Node {
    role: &'static str,
    spawn: String,
    dir: PathBuf,
    child: Option<Child>,
    log: PathBuf,
}

const CHILD: &str = concat!(module_path!(), "::s3_node_child");

impl Node {
    pub(super) fn spawn(scene: &Scene, scenario: &str, role: &'static str) -> Self {
        let n = scene.spawned.fetch_add(1, Ordering::SeqCst);
        let spawn = format!("{role}-{n}");
        let root = scene.dir().join(format!("root-{role}"));
        std::fs::create_dir_all(&root).unwrap();
        let log = scene.dir().join(format!("{spawn}.log"));
        let out = std::fs::File::create(&log).unwrap();
        // The entry sits beside this module; libtest names drop the crate segment.
        let parent = CHILD.rsplit_once("::harness::").unwrap().0;
        let name = format!("{parent}::s3_node_child");
        let name = name.split_once("::").unwrap().1.to_string();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &name,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ENV_SCENARIO, scenario)
            .env(ENV_ROLE, role)
            .env(ENV_DIR, scene.dir())
            .env(ENV_DB, scene.url())
            .env(ENV_SPAWN, &spawn)
            .env("AGENTDESK_ROOT_DIR", &root)
            .env(test_override::CHILD_ENV, "1")
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .expect("spawn node");
        Self {
            role,
            spawn,
            dir: scene.dir().to_path_buf(),
            child: Some(child),
            log,
        }
    }

    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let start = log.len().saturating_sub(6_000);
        let start = (start..log.len())
            .find(|i| log.is_char_boundary(*i))
            .unwrap_or(0);
        log[start..].to_string()
    }

    /// Waits for the child to end on its own; it must have run its one test and finished it.
    pub(super) async fn finish(mut self, within: Duration) {
        let deadline = tokio::time::Instant::now() + within;
        let mut child = self.child.take().unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "node {} timed out (watchdog)\n{}",
                    self.role,
                    self.log_tail()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        assert!(
            status.success(),
            "node {} failed: {status}\n{}",
            self.role,
            self.log_tail()
        );
        assert!(
            log.contains("1 passed; 0 failed; 0 ignored"),
            "node {} ran no test\n{}",
            self.role,
            self.log_tail()
        );
        let done = self.dir.join(format!("done.{}", self.spawn));
        assert!(done.exists(), "node {} returned before its end", self.role);
    }

    /// Waits for `name` from this node; its exit before the signal fails at once with its log.
    pub(super) async fn wait_signal(&mut self, name: &str, within: Duration) {
        eprintln!("[s3] waiting for signal {name} from {}", self.role);
        let deadline = tokio::time::Instant::now() + within;
        while !signalled(&self.dir, name) {
            let exited = self.child.as_mut().expect("running").try_wait().unwrap();
            if let Some(status) = exited.filter(|_| !signalled(&self.dir, name)) {
                let tail = self.log_tail();
                panic!(
                    "node {} exited ({status}) before signal {name}\n{tail}",
                    self.role
                );
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "signal {name} never came from {}\n{}",
                self.role,
                self.log_tail()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The spawn this node is, unique within the scene: a generation's identity across restarts.
    pub(super) fn name(&self) -> &str {
        &self.spawn
    }

    pub(super) fn role(&self) -> &'static str {
        self.role
    }

    /// The child's process id, for a node that must be the same process across a phase.
    pub(super) fn pid(&self) -> u32 {
        self.child.as_ref().expect("running").id()
    }

    /// Ends the node at once, as a crash would.
    pub(super) fn kill(mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One POST as the fake Discord saw it: its start (with the home epoch read just after the
/// admitting first poll), acceptance, and whether it ended after acceptance or was dropped before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Seen {
    Started {
        content: String,
        home_epoch: Option<i64>,
    },
    Accepted(String),
    Ended(String),
    Dropped(String),
}

/// The scene file every recording node appends its POSTs' lifecycles to.
pub(super) const POST_LIFECYCLE: &str = "post-lifecycle.jsonl";

/// One line of [`POST_LIFECYCLE`]: a step of request `request` made by process `spawn` of `node`,
/// with an epoch and where that epoch came from.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct PostEvent {
    pub(super) node: String,
    pub(super) spawn: String,
    pub(super) request: String,
    pub(super) step: PostStep,
    pub(super) content: String,
    pub(super) epoch: Option<i64>,
    pub(super) epoch_source: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PostStep {
    /// Written in the first poll before its first await, with the scene's fixed epoch.
    Started,
    /// The home epoch read just after the first poll, outside the admission lock.
    Observed,
    Accepted,
    Ended,
    Dropped,
}

/// Appends this process's POST steps to the scene file; request ids are `<spawn>/<n>`.
struct PostRecorder {
    file: PathBuf,
    node: String,
    spawn: String,
    scene_epoch: Option<i64>,
    next: AtomicU64,
}

impl PostRecorder {
    fn write(&self, request: &str, step: PostStep, content: &str, epoch: (Option<i64>, &str)) {
        use std::io::Write;
        let line = PostEvent {
            node: self.node.clone(),
            spawn: self.spawn.clone(),
            request: request.into(),
            step,
            content: content.into(),
            epoch: epoch.0,
            epoch_source: epoch.0.map(|_| epoch.1.to_string()),
        };
        let mut bytes = serde_json::to_vec(&line).unwrap();
        bytes.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
            .unwrap();
        // The lock spans this one append only, never the request it records; closing releases it.
        file.lock().unwrap();
        file.write_all(&bytes).unwrap();
    }

    fn start(self: &Arc<Self>, content: &str) -> PostRecord {
        let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let request = format!("{}/{n}", self.spawn);
        let epoch = (self.scene_epoch, "scene");
        self.write(&request, PostStep::Started, content, epoch);
        PostRecord {
            recorder: Arc::clone(self),
            request,
            content: content.into(),
            accepted: false,
        }
    }
}

/// A recorded POST from its Started on; going, it writes Ended once accepted and Dropped before.
struct PostRecord {
    recorder: Arc<PostRecorder>,
    request: String,
    content: String,
    accepted: bool,
}

impl PostRecord {
    fn note(&self, step: PostStep, epoch: (Option<i64>, &str)) {
        let recorder = &self.recorder;
        recorder.write(&self.request, step, &self.content, epoch);
    }
}

impl Drop for PostRecord {
    fn drop(&mut self) {
        let end = if self.accepted {
            PostStep::Ended
        } else {
            PostStep::Dropped
        };
        self.note(end, (None, ""));
    }
}

/// Every line of the scene's [`POST_LIFECYCLE`] in file order; no file is no POST.
pub(super) fn post_lifecycle(dir: &Path) -> Vec<PostEvent> {
    let text = std::fs::read_to_string(dir.join(POST_LIFECYCLE)).unwrap_or_default();
    let lines = text.lines().map(|line| serde_json::from_str(line).unwrap());
    lines.collect()
}

/// Requests started and not yet Ended or Dropped.
pub(super) fn open_posts(events: &[PostEvent]) -> usize {
    let started = events.iter().filter(|e| e.step == PostStep::Started);
    let ended = events
        .iter()
        .filter(|e| matches!(e.step, PostStep::Ended | PostStep::Dropped));
    started.count().saturating_sub(ended.count())
}

/// A request's span in the lifecycle file: the line of its Started and of its terminal step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PostSpan {
    pub(super) node: String,
    pub(super) spawn: String,
    pub(super) content: String,
    pub(super) started: usize,
    pub(super) ended: usize,
    pub(super) accepted: bool,
    pub(super) observed: Option<i64>,
}

/// Pairs each request's steps in file order, failing on a missing, repeated or misordered step,
/// and on any two requests of different processes whose spans overlap.
pub(super) fn post_spans(events: &[PostEvent]) -> Result<Vec<PostSpan>, String> {
    use PostStep::{Accepted, Dropped, Ended, Observed, Started};
    let mut requests: BTreeMap<&str, Vec<(usize, &PostEvent)>> = BTreeMap::new();
    for (line, event) in events.iter().enumerate() {
        requests
            .entry(&event.request)
            .or_default()
            .push((line, event));
    }
    let mut spans = Vec::new();
    for (request, steps) in requests {
        let order: Vec<PostStep> = steps.iter().map(|(_, event)| event.step).collect();
        let whole = matches!(
            order[..],
            [Started, Observed, Accepted, Ended]
                | [Started, Observed, Dropped]
                | [Started, Dropped]
        );
        if !whole {
            return Err(format!("{request}: steps {order:?}"));
        }
        let (first, last) = (steps[0], steps[steps.len() - 1]);
        let same = |(_, event): &(usize, &PostEvent)| {
            (&event.spawn, &event.content) == (&first.1.spawn, &first.1.content)
        };
        if !steps.iter().all(same) {
            return Err(format!("{request}: steps from different requests"));
        }
        let observed = steps.iter().find(|(_, event)| event.step == Observed);
        spans.push(PostSpan {
            node: first.1.node.clone(),
            spawn: first.1.spawn.clone(),
            content: first.1.content.clone(),
            started: first.0,
            ended: last.0,
            accepted: order.contains(&Accepted),
            observed: observed.and_then(|(_, event)| event.epoch),
        });
    }
    spans.sort_by_key(|span| span.started);
    for (i, a) in spans.iter().enumerate() {
        for b in spans[i + 1..].iter().filter(|b| b.spawn != a.spawn) {
            if b.started < a.ended {
                return Err(format!("{a:?} and {b:?} overlap"));
            }
        }
    }
    Ok(spans)
}

#[test]
fn post_spans_pass_apart_and_name_each_broken_record() {
    use PostStep::{Accepted, Dropped, Ended, Observed, Started};
    let event = |request: &str, step| {
        let spawn = request.split('/').next().unwrap().to_string();
        let (node, content) = (spawn.clone(), format!("{request} payload"));
        let (epoch, epoch_source) = (None, None);
        let request = request.to_string();
        PostEvent {
            node,
            spawn,
            request,
            step,
            content,
            epoch,
            epoch_source,
        }
    };
    let posted = |request| [Started, Observed, Accepted, Ended].map(|step| event(request, step));
    let apart: Vec<_> = posted("a/1").into_iter().chain(posted("b/1")).collect();
    assert_eq!(post_spans(&apart).map(|spans| spans.len()), Ok(2));
    let crossed = [
        event("a/1", Started),
        event("b/1", Started),
        event("a/1", Dropped),
        event("b/1", Dropped),
    ];
    assert!(post_spans(&crossed).unwrap_err().contains("overlap"));
    let same_process = [
        event("a/1", Started),
        event("a/2", Started),
        event("a/1", Dropped),
        event("a/2", Dropped),
    ];
    assert!(
        post_spans(&same_process).is_ok(),
        "overlap is judged across processes"
    );
    let open = [event("a/1", Started), event("a/1", Observed)];
    assert_eq!(open_posts(&open), 1);
    let broken: [&[PostEvent]; 4] = [
        &open,
        &[
            event("a/1", Started),
            event("a/1", Dropped),
            event("a/1", Dropped),
        ],
        &[
            event("a/1", Accepted),
            event("a/1", Started),
            event("a/1", Ended),
        ],
        &[
            event("a/1", Started),
            event("a/1", Observed),
            event("a/1", Ended),
        ],
    ];
    for events in broken {
        let error = post_spans(events).unwrap_err();
        assert!(error.contains("a/1: steps"), "{error}");
    }
}

#[derive(Default)]
struct DiscordState {
    created: Mutex<Vec<(u64, SeenMessage)>>,
    seen: Mutex<Vec<Seen>>,
    running: AtomicUsize,
    most_running: AtomicUsize,
    /// Messages accepted in an earlier generation: history shows them, `posts` never counts them.
    kept: Vec<(u64, SeenMessage)>,
    /// Where each accepted message is also written, for the next generation's Discord to keep.
    file: Option<PathBuf>,
    recorder: Option<Arc<PostRecorder>>,
}

/// A fake Discord that keeps each request's lifecycle and can hold requests open.
pub(super) struct Discord {
    state: Arc<DiscordState>,
    valve: watch::Sender<bool>,
    history_unavailable: bool,
}

/// What every history read of [`Discord::history_unavailable`] fails with.
pub(super) const HISTORY_UNAVAILABLE: &str = "test history unavailable";

impl Default for Discord {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            valve: watch::channel(true).0,
            history_unavailable: false,
        }
    }
}

impl Discord {
    /// A Discord that keeps what earlier generations accepted into `file` and adds to it.
    pub(super) fn kept_in(file: &Path) -> Self {
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file);
        drop(opened.unwrap());
        let text = std::fs::read_to_string(file).unwrap();
        let kept = text.lines().map(|line| {
            let line: serde_json::Value = serde_json::from_str(line).unwrap();
            let message = SeenMessage {
                id: line["id"].as_u64().unwrap(),
                author_id: line["author_id"].as_u64().unwrap(),
                content: line["content"].as_str().unwrap().to_string(),
            };
            (line["channel"].as_u64().unwrap(), message)
        });
        let state = DiscordState {
            kept: kept.collect(),
            file: Some(file.to_path_buf()),
            ..DiscordState::default()
        };
        Self {
            state: Arc::new(state),
            ..Self::default()
        }
    }

    /// As this Discord, with every history read failing as an outage would.
    pub(super) fn history_unavailable(mut self) -> Self {
        self.history_unavailable = true;
        self
    }

    /// As this Discord, appending each POST's lifecycle to the scene's [`POST_LIFECYCLE`].
    pub(super) fn recording(mut self, env: &Env) -> Self {
        let recorder = PostRecorder {
            file: env.dir.join(POST_LIFECYCLE),
            node: env.role.clone(),
            spawn: env.spawn.clone(),
            scene_epoch: get(&env.dir, "epoch").map(|epoch| epoch.parse().unwrap()),
            next: AtomicU64::new(0),
        };
        let state = Arc::get_mut(&mut self.state).expect("a Discord not yet shared");
        state.recorder = Some(Arc::new(recorder));
        self
    }

    /// What earlier generations accepted.
    pub(super) fn kept(&self) -> Vec<SeenMessage> {
        self.state.kept.iter().map(|(_, m)| m.clone()).collect()
    }

    /// Requests started from now wait until [`Discord::release`].
    pub(super) fn hold(&self) {
        self.valve.send_replace(false);
    }

    pub(super) fn release(&self) {
        self.valve.send_replace(true);
    }

    /// Accepted contents, in order.
    pub(super) fn posts(&self) -> Vec<String> {
        let created = locked(&self.state.created);
        created.iter().map(|(_, m)| m.content.clone()).collect()
    }

    pub(super) fn seen(&self) -> Vec<Seen> {
        locked(&self.state.seen).clone()
    }

    pub(super) fn running(&self) -> usize {
        self.state.running.load(Ordering::SeqCst)
    }

    pub(super) fn started(&self, content: &str) -> bool {
        self.seen()
            .iter()
            .any(|seen| matches!(seen, Seen::Started { content: c, .. } if c == content))
    }

    /// The home epoch each request started under.
    pub(super) fn started_epochs(&self) -> Vec<(String, Option<i64>)> {
        let seen = self.seen();
        let started = seen.into_iter().filter_map(|seen| match seen {
            Seen::Started {
                content,
                home_epoch,
            } => Some((content, home_epoch)),
            _ => None,
        });
        started.collect()
    }
}

struct Request {
    state: Arc<DiscordState>,
    content: String,
    accepted: bool,
}

impl Drop for Request {
    fn drop(&mut self) {
        let content = std::mem::take(&mut self.content);
        let seen = if self.accepted {
            Seen::Ended(content)
        } else {
            Seen::Dropped(content)
        };
        locked(&self.state.seen).push(seen);
        self.state.running.fetch_sub(1, Ordering::SeqCst);
    }
}

fn home_epoch_now(channel: u64) -> Option<i64> {
    match channel_home::registered_channel(channel)?.ownership() {
        HomeOwnership::Owned { home_epoch, .. } => Some(home_epoch),
        HomeOwnership::Lost => None,
    }
}

impl DiscordPort for Discord {
    fn bot_id(&self) -> u64 {
        BOT
    }

    fn post(
        &self,
        channel: u64,
        content: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        let state = Arc::clone(&self.state);
        let mut valve = self.valve.subscribe();
        async move {
            // Before the first await: a request dropped at its first yield still records its end.
            let mut record = state.recorder.as_ref().map(|r| r.start(&content));
            // The first poll runs under the home gate's admission lock; read its epoch after it.
            tokio::task::yield_now().await;
            let home_epoch = home_epoch_now(channel);
            if let Some(record) = &record {
                record.note(PostStep::Observed, (home_epoch, "gate"));
            }
            let started = Seen::Started {
                content: content.clone(),
                home_epoch,
            };
            locked(&state.seen).push(started);
            let now = state.running.fetch_add(1, Ordering::SeqCst) + 1;
            state.most_running.fetch_max(now, Ordering::SeqCst);
            let mut request = Request {
                state: Arc::clone(&state),
                content: content.clone(),
                accepted: false,
            };
            let _ = valve.wait_for(|open| *open).await;
            let message = {
                let mut created = locked(&state.created);
                // Ids rise across node restarts, as Discord's do.
                let micros = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as u64;
                let last = created.iter().chain(&state.kept).map(|(_, m)| m.id).max();
                let id = micros.max(last.unwrap_or(0) + 1);
                let message = SeenMessage {
                    id,
                    author_id: BOT,
                    content: content.clone(),
                };
                created.push((channel, message.clone()));
                if let Some(file) = &state.file {
                    let line = serde_json::json!({"channel": channel, "id": id,
                        "author_id": BOT, "content": content});
                    append(file, format!("{line}\n").as_bytes());
                }
                message
            };
            locked(&state.seen).push(Seen::Accepted(content));
            request.accepted = true;
            if let Some(record) = &mut record {
                record.note(PostStep::Accepted, (None, ""));
                record.accepted = true;
            }
            PostOutcome::Created(message)
        }
    }

    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        if self.history_unavailable {
            return std::future::ready(Err(HISTORY_UNAVAILABLE.into()));
        }
        let created = locked(&self.state.created);
        let page = (self.state.kept.iter().chain(created.iter()))
            .filter(|(c, m)| *c == channel && m.id > after);
        std::future::ready(Ok(page.map(|(_, m)| m.clone()).collect()))
    }

    fn history_readable(&self, _: u64) -> bool {
        true
    }
}

/// The channel's delivery lease; withheld, the actor keeps its pieces owed and keeps polling.
#[derive(Clone, Default)]
pub(super) struct Lease(Arc<AtomicBool>);

impl Lease {
    pub(super) fn withhold(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(super) fn grant(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl DeliveryLease for Lease {
    type Held = ();
    fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
        (!self.0.load(Ordering::SeqCst)).then_some(())
    }
}

/// A channel's binding event log, one per host: [`TestHost`]'s Startup bind as seq 1, then what
/// a scenario commits.
pub(super) struct EventLog {
    events: Mutex<Vec<BindingEvent>>,
    notice: watch::Sender<u64>,
    /// The highest seq any read returned.
    served: AtomicU64,
    /// A seq whose publish waits a moment after it is stored, so a racing commit could pass it.
    slow_publish: AtomicU64,
}

impl EventLog {
    fn new(events: Vec<BindingEvent>) -> Self {
        let latest = events.last().map_or(0, |event| event.seq);
        Self {
            events: Mutex::new(events),
            notice: watch::channel(latest).0,
            served: AtomicU64::new(0),
            slow_publish: AtomicU64::new(0),
        }
    }

    /// Seq 1, whose channel, provider and pane every committed event carries.
    pub(super) fn first(&self) -> BindingEvent {
        locked(&self.events)[0].clone()
    }

    /// Stores `record` as the next seq and publishes it under the same lock, so a notified reader
    /// finds it and the notice never moves back.
    pub(super) fn commit(&self, record: BindingRecord) -> u64 {
        let mut events = locked(&self.events);
        let first = events[0].clone();
        let seq = events.last().unwrap().seq + 1;
        let committed_at = chrono::Utc::now();
        events.push(BindingEvent {
            seq,
            record,
            committed_at,
            ..first
        });
        if seq == self.slow_publish.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        self.notice.send_replace(seq);
        seq
    }

    pub(super) fn served(&self) -> u64 {
        self.served.load(Ordering::SeqCst)
    }
}

impl BindingEvents for EventLog {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        let events = locked(&self.events);
        let due = events
            .iter()
            .filter(|e| e.channel_id == channel && e.seq > after);
        let due: Vec<_> = due.cloned().collect();
        if let Some(last) = due.last() {
            self.served.fetch_max(last.seq, Ordering::SeqCst);
        }
        Ok(due)
    }

    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.notice.subscribe()
    }
}

/// The commits start together and seq 2's publish waits; outside the lock it would end on 2.
#[test]
fn racing_commits_publish_the_latest_seq_and_every_event() {
    let source = SourceId {
        session_id: "s1".into(),
        path: PathBuf::from("/s1.jsonl"),
        dev: 1,
        ino: 1,
    };
    let startup = TestHost::new([(C, source)]).bindings(C, ShadowProvider::Claude);
    let log = EventLog::new(startup.binding_events_since(C, 0).unwrap());
    let rejected = |detail: &str| BindingRecord::Rejected {
        detail: detail.into(),
    };
    log.slow_publish.store(2, Ordering::SeqCst);
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for detail in ["a", "b"] {
            let (log, start, rejected) = (&log, &start, &rejected);
            scope.spawn(move || {
                start.wait();
                log.commit(rejected(detail))
            });
        }
    });
    assert_eq!(
        *log.subscribe(C).borrow(),
        3,
        "the notice names the last commit"
    );
    let read = log.binding_events_since(C, 0).unwrap();
    let seqs: Vec<_> = read.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, [1, 2, 3]);
}

/// The real writer host's gateway IO with the recording Discord in place of [`TestHost`]'s.
pub(super) struct S3Host {
    inner: Arc<TestHost>,
    pub(super) discord: Arc<Discord>,
    pub(super) lease: Lease,
    logs: Mutex<BTreeMap<u64, Arc<EventLog>>>,
}

impl S3Host {
    /// Whether the writer host asked for `channel`'s binding log, as it does once its store is open.
    pub(super) fn bound(&self, channel: u64) -> bool {
        locked(&self.logs).contains_key(&channel)
    }
}

impl HostIo for S3Host {
    type Port = Discord;
    type Lease = Lease;
    type Alarms = Alarms;
    type Bindings = EventLog;

    fn port(&self) -> impl Future<Output = Arc<Discord>> + Send {
        std::future::ready(Arc::clone(&self.discord))
    }

    fn lease(&self) -> Lease {
        self.lease.clone()
    }

    fn alarms(&self) -> Alarms {
        self.inner.alarms()
    }

    /// Every caller, the host and the actor alike, gets the channel's one log.
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<EventLog> {
        let mut logs = locked(&self.logs);
        let log = logs.entry(channel).or_insert_with(|| {
            let startup = self.inner.bindings(channel, provider);
            Arc::new(EventLog::new(
                startup.binding_events_since(channel, 0).unwrap(),
            ))
        });
        Arc::clone(log)
    }

    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.inner.activation_facts(channel, provider)
    }

    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.inner.local_custody(channel, provider)
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.inner.legacy()
    }

    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.inner.legacy_busy(channel)
    }

    fn relaying(&self, channel: u64) -> bool {
        self.inner.relaying(channel)
    }
}

/// A transcript row the Claude deriver turns into one piece with `text`.
pub(super) fn row(id: &str, text: &str) -> Vec<u8> {
    let row = serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "message": {"id": id, "content": [{"type": "text", "text": text}]},
    });
    let mut line = serde_json::to_vec(&row).unwrap();
    line.push(b'\n');
    line
}

pub(super) fn append(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

/// The boot config a node reads: clustered, the gateway as O home, `C` selected for the writer.
fn node_config(role: &str, provider: ShadowProvider, gateway: &str) -> crate::config::Config {
    serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": {"enabled": true, "instance_id": role, "gateway_preferred_instance_id": gateway},
        "runtime": {"channel_home_delegation_enabled": true},
        "tui_o": {"writer": {"channels": [C]}},
        "agents": [{"id": format!("writer-{C}"), "name": "Writer",
            "channels": {provider_name(provider): {"id": C.to_string(), "runtime": "tui"}}}],
    }))
    .unwrap()
}

/// The reset a releasing drain would run; a worker-owned drain never asks for it.
fn no_reset() -> channel_home_boot::LegacyReset {
    Arc::new(|| Box::pin(async { Err(ResetRefused::NotWired) }))
}

/// A node as its provider runtime boots it: the boot snapshot read from its own O store, the
/// delegated homes its rows name with their watch and lease, and the writer host for them.
pub(super) struct Booted {
    pub(super) pool: PgPool,
    pub(super) root: PathBuf,
    pub(super) transcript: PathBuf,
    pub(super) gates: Vec<Arc<HomeGate>>,
    pub(super) host: Arc<S3Host>,
    pub(super) writers: Vec<tokio::task::JoinHandle<()>>,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
}

/// The REST worker role restored its persisted turns before its homes start
/// (`runtime_bootstrap.rs` `HomeRole::RestWorker`); a standby does not, and is not booted here.
pub(super) fn worker_restored() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(true))
}

impl Booted {
    pub(super) async fn boot(env: &Env) -> Self {
        Self::boot_on(env, Discord::default()).await
    }

    /// As [`Booted::boot`], posting to `discord`.
    pub(super) async fn boot_on(env: &Env, discord: Discord) -> Self {
        let reads = |id| ChannelHomePort::new(id, host::process_readiness(), worker_restored());
        Self::boot_with_on(env, reads, discord, ShadowProvider::Claude, GW).await
    }

    /// As [`Booted::boot`], with `gateway` as the cluster's gateway role (and so the O home).
    pub(super) async fn boot_with_gateway(env: &Env, gateway: &str) -> Self {
        let reads = |id| ChannelHomePort::new(id, host::process_readiness(), worker_restored());
        Self::boot_with_on(
            env,
            reads,
            Discord::default(),
            ShadowProvider::Claude,
            gateway,
        )
        .await
    }

    /// As [`Booted::boot`], with the config, boot and writer host on `provider`.
    pub(super) async fn boot_as(env: &Env, provider: ShadowProvider) -> Self {
        let reads = |id| ChannelHomePort::new(id, host::process_readiness(), worker_restored());
        Self::boot_with_on(env, reads, Discord::default(), provider, GW).await
    }

    /// As [`Booted::boot`], with the drain reading through `reads`.
    pub(super) async fn boot_with<R>(env: &Env, reads: impl Fn(u64) -> R) -> Self
    where
        R: DrainPort + Send + Sync + 'static,
    {
        Self::boot_with_on(env, reads, Discord::default(), ShadowProvider::Claude, GW).await
    }

    async fn boot_with_on<R>(
        env: &Env,
        reads: impl Fn(u64) -> R,
        discord: Discord,
        provider: ShadowProvider,
        gateway: &str,
    ) -> Self
    where
        R: DrainPort + Send + Sync + 'static,
    {
        let role = env.role.clone();
        let root = crate::config::runtime_root().expect("node root");
        let hosts = crate::config::session_hosts::force_for_test(Some(&role), &[(C, &role)]);
        let config = node_config(&role, provider, gateway);
        crate::services::tui_o::channel_policy::install(&config).expect("boot snapshot");
        let pool = env.pool().await;
        let rows = channel_home_boot::listed(&pool);
        let (local, boot_pool) = (role.clone(), pool.clone());
        let port = move |id| BootPort::new(reads(id), no_reset());
        let prepare = move || {
            Some(Boot {
                provider: provider_name(provider).into(),
                local,
                pool: boot_pool,
                rows,
                candidates: vec![C],
                port,
            })
        };
        let gates = channel_home_boot::start(Some(true), prepare).await;
        let transcript = root.join("c.jsonl");
        if !transcript.exists() {
            std::fs::write(&transcript, b"").unwrap();
        }
        let source = source_id_for("s1", &transcript).unwrap();
        let host = Arc::new(S3Host {
            inner: TestHost::new([(C, source)]),
            discord: Arc::new(discord),
            lease: Lease::default(),
            logs: Mutex::default(),
        });
        let owned = channel_home_boot::delegated_boot_ownership(|_| true);
        let io = Arc::clone(&host);
        let parts_root = root.clone();
        let writers = host::start_delegated(provider, owned, move || HostParts {
            io,
            runtime_root: Some(parts_root),
            gate: Arc::new(OwnershipGate::default()),
            readiness: host::process_readiness(),
        });
        Self {
            pool,
            root,
            transcript,
            gates,
            host,
            writers,
            _hosts: hosts,
        }
    }

    pub(super) fn discord(&self) -> &Discord {
        &self.host.discord
    }

    /// Waits for exactly `expected` to be accepted, failing with what was accepted instead.
    pub(super) async fn until_posts(&self, expected: &[&str]) {
        eprintln!("[s3] until posts {expected:?}");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while self.discord().posts() != expected && tokio::time::Instant::now() < deadline {
            if self.stopped_for_good() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if self.discord().posts() != expected {
            let alarms = locked(&self.host.inner.alarms.0).clone();
            let owed = self.owed().await;
            let transcript = std::fs::read_to_string(&self.transcript).unwrap_or_default();
            panic!(
                "posts {:?} != {expected:?}\nhalted for good {}\nseen {:?}\nalarms {alarms:?}\nowed {owed:?}\n{transcript}",
                self.discord().posts(),
                self.stopped_for_good(),
                self.discord().seen()
            );
        }
    }

    pub(super) fn lease(&self) -> &Lease {
        &self.host.lease
    }

    /// What the drain's port reads from the channel's actor now.
    pub(super) async fn owed(&self) -> Option<crate::services::cluster::channel_home_drain::Owed> {
        let port = ChannelHomePort::new(C, host::process_readiness(), worker_restored());
        port.owed().await
    }

    pub(super) fn gate(&self) -> Option<Arc<HomeGate>> {
        channel_home::registered_channel(C)
    }

    pub(super) fn home_epoch(&self) -> Option<i64> {
        home_epoch_now(C)
    }

    pub(super) fn accepts(&self) -> bool {
        host::process_readiness().accepts(C)
    }

    pub(super) fn halted(&self) -> bool {
        let raised = locked(&self.host.inner.alarms.0);
        let halted = |(channel, alarm): &&(u64, WriterAlarm)| {
            *channel == C && matches!(alarm, WriterAlarm::Halted { .. })
        };
        raised.iter().any(|entry| halted(&entry))
    }

    /// A Halt with every writer host ended: no resume follows, so no later POST can come.
    fn stopped_for_good(&self) -> bool {
        self.halted()
            && self
                .writers
                .iter()
                .all(tokio::task::JoinHandle::is_finished)
    }

    /// The writer alarms raised for `C`, in order, as the host's sink received them.
    pub(super) fn raised(&self) -> Vec<WriterAlarm> {
        let raised = locked(&self.host.inner.alarms.0);
        let ours = raised.iter().filter(|(channel, _)| *channel == C);
        ours.map(|(_, alarm)| alarm.clone()).collect()
    }

    /// What the drain waits on now, as health shows it.
    pub(super) fn drain_blocker(&self) -> Option<String> {
        let health = channel_home::health()?;
        let draining = health["home_draining"].as_array()?.first()?.clone();
        draining["blocker"].as_str().map(str::to_owned)
    }

    pub(super) async fn state(&self) -> Option<HomeState> {
        let row = o_channel_homes::read_home(&self.pool, &C.to_string()).await;
        row.unwrap().map(|row| row.state)
    }

    pub(super) async fn stop(self) {
        self.writers.iter().for_each(|writer| writer.abort());
        channel_home_boot::stop(&C.to_string()).await;
        channel_home::unregister(&C.to_string());
        self.pool.close().await;
    }
}

/// One router decision for message `message` of `C` as node `leader` runs it.
pub(super) async fn route(pool: &PgPool, leader: &str, message: &str) -> IntakeRouterDecision {
    route_as(pool, leader, ShadowProvider::Claude, message).await
}

/// As [`route`], for an intake of `provider`.
pub(super) async fn route_as(
    pool: &PgPool,
    leader: &str,
    provider: ShadowProvider,
    message: &str,
) -> IntakeRouterDecision {
    let channel = C.to_string();
    try_route_intake(pool, &router_context(leader, provider, &channel, message)).await
}

/// The intake [`route_as`] asks the router about: message `message` of `channel` on `provider`.
fn router_context<'a>(
    leader: &'a str,
    provider: ShadowProvider,
    channel: &'a str,
    message: &'a str,
) -> IntakeRouterContext<'a> {
    IntakeRouterContext {
        mode: IntakeRoutingMode::Enforce,
        leader_instance_id: leader,
        provider: provider_name(provider),
        channel_id: channel,
        policy_channel_id: channel,
        user_msg_id: message,
        replay_source_message_ids: None,
        request_owner_id: "100",
        request_owner_name: Some("Tester"),
        user_text: "hello",
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "foreground",
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        node_override_instance_id: None,
        owner_authority: OwnerAuthorityChannelOptIn::NotOptedIn,
        has_nonportable_uploads: false,
        attachment_refs: &[],
    }
}

#[test]
fn route_as_asks_for_the_given_provider() {
    let channel = C.to_string();
    for (provider, name) in [
        (ShadowProvider::Claude, "claude"),
        (ShadowProvider::Codex, "codex"),
    ] {
        assert_eq!(router_context(GW, provider, &channel, "1").provider, name);
    }
}

/// A pending row for MINI routed at `home_epoch`, as the gateway's router would insert it.
pub(super) async fn seed(pool: &PgPool, message: &str, home_epoch: Option<i64>) -> i64 {
    seed_on(pool, C, message, home_epoch).await
}

/// As [`seed`] for `channel`.
pub(super) async fn seed_on(
    pool: &PgPool,
    channel: u64,
    message: &str,
    home_epoch: Option<i64>,
) -> i64 {
    seed_for(pool, channel, MINI, message, home_epoch).await
}

/// As [`seed_on`], routed to `target`.
pub(super) async fn seed_for(
    pool: &PgPool,
    channel: u64,
    target: &str,
    message: &str,
    home_epoch: Option<i64>,
) -> i64 {
    let payload = crate::db::intake_outbox::InsertPendingPayload {
        target_instance_id: target.into(),
        forwarded_by_instance_id: GW.into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: channel.to_string(),
        user_msg_id: message.into(),
        replay_source_message_ids: None,
        request_owner_id: "100".into(),
        request_owner_name: None,
        user_text: "hello".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: None,
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: format!("writer-{C}"),
        provider: "claude".into(),
        home_epoch,
    };
    crate::db::intake_outbox::insert_pending(pool, &payload, 1, None)
        .await
        .unwrap()
}
