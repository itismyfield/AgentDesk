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
use crate::services::cluster::channel_home_boot::{
    self, Boot, BootPort, ChannelWriters, ResetRefused,
};
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
    /// This process's spawn name, unique within the scene.
    pub(super) spawn: String,
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
        Self::spawn_at(scene.dir(), scene.url(), n, scenario, role)
    }

    /// As [`Node::spawn`] in a bare directory: the scenario must open no database.
    pub(super) fn spawn_without_db(
        dir: &Path,
        n: usize,
        scenario: &str,
        role: &'static str,
    ) -> Self {
        Self::spawn_at(dir, "", n, scenario, role)
    }

    fn spawn_at(dir: &Path, db: &str, n: usize, scenario: &str, role: &'static str) -> Self {
        let spawn = format!("{role}-{n}");
        let root = dir.join(format!("root-{role}"));
        std::fs::create_dir_all(&root).unwrap();
        let log = dir.join(format!("{spawn}.log"));
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
            .env(ENV_DIR, dir)
            .env(ENV_DB, db)
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
            dir: dir.to_path_buf(),
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

    /// Fails at once with this node's log if it exited before sending `name`.
    pub(super) fn alive_before(&mut self, name: &str) {
        let exited = self.child.as_mut().expect("running").try_wait().unwrap();
        if let Some(status) = exited.filter(|_| !signalled(&self.dir, name)) {
            let tail = self.log_tail();
            panic!(
                "node {} exited ({status}) before signal {name}\n{tail}",
                self.role
            );
        }
    }

    /// Waits for `name` from this node; its exit before the signal fails at once with its log.
    pub(super) async fn wait_signal(&mut self, name: &str, within: Duration) {
        eprintln!("[s3] waiting for signal {name} from {}", self.role);
        let deadline = tokio::time::Instant::now() + within;
        while !signalled(&self.dir, name) {
            self.alive_before(name);
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

/// The scene file every recording node appends its POST and history steps to, one JSON per line.
pub(super) const LIFECYCLE: &str = "lifecycle.jsonl";

/// Where a step's epoch came from: the scene's fixed epoch, or the node's gate read just then.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EpochSource {
    Scene,
    Gate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LifeStep {
    /// A POST in its first poll before its first await, with the scene's fixed epoch.
    Started,
    /// The home epoch the POST read just after its first poll, outside the admission lock.
    Observed,
    Accepted,
    Ended,
    Dropped,
    /// A history read as asked, and as answered.
    Asked,
    Answered,
}

/// One line of [`LIFECYCLE`]: step `step` of POST or history read `id`, made by process `spawn`
/// of `node` on `channel`; `line` is the step's line in the file, kept through any filtering.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct LifeEvent {
    #[serde(skip)]
    pub(super) line: usize,
    pub(super) node: String,
    pub(super) spawn: String,
    pub(super) channel: u64,
    pub(super) id: String,
    pub(super) step: LifeStep,
    /// The POST payload, or the anchor a history read asked after.
    pub(super) detail: String,
    pub(super) epoch: Option<i64>,
    pub(super) epoch_source: Option<EpochSource>,
    /// What a history read answered: `messages <n>` or `error: <reason>`.
    pub(super) answer: Option<String>,
}

/// Appends this process's steps to the lifecycle file; ids are `post/<spawn>/<n>` and
/// `history/<spawn>/<n>`.
struct Recorder {
    file: PathBuf,
    node: String,
    spawn: String,
    scene_epoch: Option<i64>,
    posts: AtomicU64,
    reads: AtomicU64,
}

impl Recorder {
    /// Creates `file` if needed, so a reader that finds none fails instead of reading no steps.
    fn new(file: PathBuf, node: &str, spawn: &str, scene_epoch: Option<i64>) -> Self {
        let options = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file);
        drop(options.unwrap());
        Self {
            file,
            node: node.into(),
            spawn: spawn.into(),
            scene_epoch,
            posts: AtomicU64::new(0),
            reads: AtomicU64::new(0),
        }
    }

    fn event(&self, channel: u64, id: &str, step: LifeStep, detail: &str) -> LifeEvent {
        LifeEvent {
            line: 0,
            node: self.node.clone(),
            spawn: self.spawn.clone(),
            channel,
            id: id.into(),
            step,
            detail: detail.into(),
            epoch: None,
            epoch_source: None,
            answer: None,
        }
    }

    fn write(&self, event: &LifeEvent) {
        use std::io::Write;
        let mut bytes = serde_json::to_vec(event).unwrap();
        bytes.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.file)
            .unwrap();
        // The lock spans this one append only, never the request it records; closing releases it.
        file.lock().unwrap();
        file.write_all(&bytes).unwrap();
    }

    fn start(self: &Arc<Self>, channel: u64, content: &str) -> PostRecord {
        let n = self.posts.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("post/{}/{n}", self.spawn);
        let started = LifeEvent {
            epoch: self.scene_epoch,
            epoch_source: Some(EpochSource::Scene),
            ..self.event(channel, &id, LifeStep::Started, content)
        };
        self.write(&started);
        PostRecord {
            recorder: Arc::clone(self),
            channel,
            id,
            content: content.into(),
            accepted: false,
        }
    }

    /// Records one history read as asked, then as answered with `answer`.
    fn read(&self, channel: u64, after: u64, answer: impl FnOnce() -> Result<usize, String>) {
        let n = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        let (id, after) = (format!("history/{}/{n}", self.spawn), after.to_string());
        self.write(&self.event(channel, &id, LifeStep::Asked, &after));
        let answer = match answer() {
            Ok(messages) => format!("messages {messages}"),
            Err(error) => format!("error: {error}"),
        };
        let answered = LifeEvent {
            answer: Some(answer),
            ..self.event(channel, &id, LifeStep::Answered, &after)
        };
        self.write(&answered);
    }
}

/// A recorded POST from its Started on; going, it writes Ended once accepted and Dropped before.
struct PostRecord {
    recorder: Arc<Recorder>,
    channel: u64,
    id: String,
    content: String,
    accepted: bool,
}

impl PostRecord {
    fn note(&self, step: LifeStep) -> LifeEvent {
        (self.recorder).event(self.channel, &self.id, step, &self.content)
    }

    /// The gate's epoch as read, kept with its source even when the gate held none.
    fn observed(&self, epoch: Option<i64>) {
        let observed = LifeEvent {
            epoch,
            epoch_source: Some(EpochSource::Gate),
            ..self.note(LifeStep::Observed)
        };
        self.recorder.write(&observed);
    }

    fn accepted(&mut self) {
        self.recorder.write(&self.note(LifeStep::Accepted));
        self.accepted = true;
    }
}

impl Drop for PostRecord {
    fn drop(&mut self) {
        let end = if self.accepted {
            LifeStep::Ended
        } else {
            LifeStep::Dropped
        };
        self.recorder.write(&self.note(end));
    }
}

/// Every line of `dir`'s [`LIFECYCLE`], read under the writers' lock; a missing file, a torn last
/// line or a line that does not parse is an error, never fewer steps.
pub(super) fn lifecycle(dir: &Path) -> Result<Vec<LifeEvent>, String> {
    use std::io::Read;
    let path = dir.join(LIFECYCLE);
    let failed = |error: std::io::Error| format!("{}: {error}", path.display());
    let mut file = std::fs::File::open(&path).map_err(failed)?;
    file.lock_shared().map_err(failed)?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(failed)?;
    parse_lifecycle(&text)
}

fn parse_lifecycle(text: &str) -> Result<Vec<LifeEvent>, String> {
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("the last line is torn".into());
    }
    let parse = |(line, json): (usize, &str)| {
        let event: Result<LifeEvent, _> = serde_json::from_str(json);
        let event = event.map_err(|error| format!("line {line}: {error}"))?;
        Ok(LifeEvent { line, ..event })
    };
    text.lines().enumerate().map(parse).collect()
}

/// Whether `step` may follow `last` for one id: a POST is `Started, Observed?` then `Accepted,
/// Ended` or `Dropped`; a history read is `Asked, Answered`.
fn may_follow(last: Option<LifeStep>, step: LifeStep) -> bool {
    use LifeStep::{Accepted, Answered, Asked, Dropped, Ended, Observed, Started};
    matches!(
        (last, step),
        (None, Started | Asked)
            | (Some(Started), Observed | Dropped)
            | (Some(Observed), Accepted | Dropped)
            | (Some(Accepted), Ended)
            | (Some(Asked), Answered)
    )
}

fn ended(step: LifeStep) -> bool {
    matches!(
        step,
        LifeStep::Ended | LifeStep::Dropped | LifeStep::Answered
    )
}

/// A step carries the epoch and answer its kind does and no others: Started the scene's epoch,
/// Observed the gate's (which may be none), Answered an answer.
fn well_formed(event: &LifeEvent) -> bool {
    let source = event.epoch_source;
    match event.step {
        LifeStep::Started => source == Some(EpochSource::Scene) && event.answer.is_none(),
        LifeStep::Observed => source == Some(EpochSource::Gate) && event.answer.is_none(),
        LifeStep::Answered => event.answer.is_some() && event.epoch.is_none() && source.is_none(),
        _ => event.epoch.is_none() && source.is_none() && event.answer.is_none(),
    }
}

/// Folds the steps per id in file order, failing on a malformed step, a step out of order or
/// after the id ended, and a step whose process, channel or detail is not its id's.
pub(super) fn tracks(events: &[LifeEvent]) -> Result<BTreeMap<String, Vec<LifeEvent>>, String> {
    let mut tracks: BTreeMap<String, Vec<LifeEvent>> = BTreeMap::new();
    for event in events {
        let steps = tracks.entry(event.id.clone()).or_default();
        let (line, id, step) = (event.line, &event.id, event.step);
        if !well_formed(event) {
            return Err(format!(
                "line {line}: {id} {step:?} is malformed: {event:?}"
            ));
        }
        let last = steps.last().map(|last| last.step);
        if !may_follow(last, step) {
            return Err(format!("line {line}: {id} {step:?} after {last:?}"));
        }
        let owner = |e: &LifeEvent| (e.node.clone(), e.spawn.clone(), e.channel, e.detail.clone());
        if steps
            .first()
            .is_some_and(|first| owner(first) != owner(event))
        {
            return Err(format!("line {line}: {id} {step:?} from another request"));
        }
        steps.push(event.clone());
    }
    Ok(tracks)
}

/// The ids not yet ended, by the rules [`post_spans`] judges with; a broken record is an error.
pub(super) fn open_ids(events: &[LifeEvent]) -> Result<Vec<String>, String> {
    let tracks = tracks(events)?;
    let open = tracks.into_iter().filter(|(_, steps)| {
        let last = steps.last().expect("a track has a step");
        !ended(last.step)
    });
    Ok(open.map(|(id, _)| id).collect())
}

/// A POST's span in the lifecycle file: the file lines of its Started and of its terminal step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PostSpan {
    pub(super) node: String,
    pub(super) spawn: String,
    pub(super) channel: u64,
    pub(super) id: String,
    pub(super) content: String,
    pub(super) started: usize,
    pub(super) ended: usize,
    pub(super) accepted: bool,
    pub(super) observed: Option<(Option<i64>, EpochSource)>,
}

/// Every POST's span, failing on any id not yet ended and on two processes' POSTs whose spans
/// overlap; lines are the file's own.
pub(super) fn post_spans(events: &[LifeEvent]) -> Result<Vec<PostSpan>, String> {
    let mut spans = Vec::new();
    for (id, steps) in tracks(events)? {
        let (first, last) = (&steps[0], &steps[steps.len() - 1]);
        if !ended(last.step) {
            return Err(format!("{id} is still open after {:?}", last.step));
        }
        if first.step != LifeStep::Started {
            continue;
        }
        let observed = steps.iter().find(|e| e.step == LifeStep::Observed);
        spans.push(PostSpan {
            node: first.node.clone(),
            spawn: first.spawn.clone(),
            channel: first.channel,
            id,
            content: first.detail.clone(),
            started: first.line,
            ended: last.line,
            accepted: steps.iter().any(|e| e.step == LifeStep::Accepted),
            observed: observed.map(|e| (e.epoch, e.epoch_source.expect("well formed"))),
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

/// The file line of the first history read of `channel` asked after file line `line`.
pub(super) fn first_read_after(events: &[LifeEvent], channel: u64, line: usize) -> Option<usize> {
    let asked = events.iter().filter(|e| e.step == LifeStep::Asked);
    let after = asked.filter(|e| e.channel == channel && e.line > line);
    after.map(|e| e.line).next()
}

fn life(id: &str, step: LifeStep) -> LifeEvent {
    let spawn = id.split('/').nth(1).unwrap().to_string();
    let (epoch, epoch_source, answer) = match step {
        LifeStep::Started => (Some(7), Some(EpochSource::Scene), None),
        LifeStep::Observed => (Some(7), Some(EpochSource::Gate), None),
        LifeStep::Answered => (None, None, Some("messages 0".into())),
        _ => (None, None, None),
    };
    LifeEvent {
        line: 0,
        node: spawn.clone(),
        spawn,
        channel: C,
        id: id.into(),
        step,
        detail: format!("{id} payload"),
        epoch,
        epoch_source,
        answer,
    }
}

/// `events` as their lines in a file, in order.
fn numbered(events: impl IntoIterator<Item = LifeEvent>) -> Vec<LifeEvent> {
    let lines = events.into_iter().enumerate();
    lines
        .map(|(line, event)| LifeEvent { line, ..event })
        .collect()
}

#[test]
fn post_spans_pass_apart_and_name_each_broken_record() {
    use LifeStep::{Accepted, Answered, Asked, Dropped, Ended, Observed, Started};
    let posted = |id| [Started, Observed, Accepted, Ended].map(|step| life(id, step));
    let read = [Asked, Answered].map(|step| life("history/a/1", step));
    let apart = numbered(
        posted("post/a/1")
            .into_iter()
            .chain(read)
            .chain(posted("post/b/1")),
    );
    let spans = post_spans(&apart).unwrap();
    assert_eq!(
        spans
            .iter()
            .map(|s| (s.started, s.ended))
            .collect::<Vec<_>>(),
        [(0, 3), (6, 9)]
    );
    assert_eq!(
        first_read_after(&apart, C, 3),
        Some(4),
        "the file's own line, not the POSTs'"
    );
    let crossed = numbered([
        life("post/a/1", Started),
        life("post/b/1", Started),
        life("post/a/1", Dropped),
        life("post/b/1", Dropped),
    ]);
    assert!(post_spans(&crossed).unwrap_err().contains("overlap"));
    let channels = numbered([
        life("post/a/1", Started),
        LifeEvent {
            channel: C + 1,
            detail: "post/a/1 payload".into(),
            ..life("post/a/2", Started)
        },
        life("post/a/1", Dropped),
        LifeEvent {
            channel: C + 1,
            detail: "post/a/1 payload".into(),
            ..life("post/a/2", Dropped)
        },
    ]);
    let spans = post_spans(&channels).unwrap();
    let apart: Vec<_> = spans
        .iter()
        .map(|s| (s.id.as_str(), s.channel, s.content.as_str()))
        .collect();
    let payload = "post/a/1 payload";
    assert_eq!(
        apart,
        [("post/a/1", C, payload), ("post/a/2", C + 1, payload)],
        "same payload"
    );
    let same_process = numbered([
        life("post/a/1", Started),
        life("post/a/2", Started),
        life("post/a/1", Dropped),
        life("post/a/2", Dropped),
    ]);
    assert!(
        post_spans(&same_process).is_ok(),
        "overlap is judged across processes"
    );
    let open = numbered([life("post/a/1", Started), life("post/a/1", Observed)]);
    assert_eq!(open_ids(&open), Ok(vec!["post/a/1".to_string()]));
    assert!(post_spans(&open).unwrap_err().contains("still open"));
    let other_channel = LifeEvent {
        channel: C + 1,
        ..life("post/a/1", Dropped)
    };
    let gate_scene = LifeEvent {
        epoch_source: Some(EpochSource::Scene),
        ..life("post/a/1", Observed)
    };
    let accepted_epoch = LifeEvent {
        epoch: Some(7),
        ..life("post/a/1", Accepted)
    };
    let broken: [Vec<LifeEvent>; 7] = [
        vec![
            life("post/a/1", Started),
            life("post/b/1", Started),
            life("post/b/1", Dropped),
            life("post/b/1", Dropped),
        ],
        vec![life("post/a/1", Dropped)],
        vec![life("post/a/1", Accepted), life("post/a/1", Started)],
        vec![
            life("post/a/1", Started),
            life("post/a/1", Observed),
            life("post/a/1", Ended),
        ],
        vec![life("post/a/1", Started), other_channel],
        vec![life("post/a/1", Started), gate_scene],
        vec![
            life("post/a/1", Started),
            life("post/a/1", Observed),
            accepted_epoch,
        ],
    ];
    for events in broken {
        let events = numbered(events);
        assert!(open_ids(&events).is_err(), "{events:#?}");
        assert!(post_spans(&events).is_err(), "{events:#?}");
    }
}

#[test]
fn lifecycle_reads_whole_lines_or_fails() {
    let lost_gate = LifeEvent {
        epoch: None,
        ..life("post/a/1", LifeStep::Observed)
    };
    let mut text = String::new();
    for event in [life("post/a/1", LifeStep::Started), lost_gate.clone()] {
        text += &format!("{}\n", serde_json::to_string(&event).unwrap());
    }
    let read = parse_lifecycle(&text).unwrap();
    assert_eq!(
        read[1],
        LifeEvent {
            line: 1,
            ..lost_gate
        },
        "a Lost gate keeps its source"
    );
    assert_eq!(read[1].epoch_source, Some(EpochSource::Gate));
    assert!(
        parse_lifecycle(&text[..text.len() - 1])
            .unwrap_err()
            .contains("torn")
    );
    let dir = tempfile::tempdir().unwrap();
    assert!(lifecycle(dir.path()).is_err(), "no file is not no steps");
}

/// A recording Discord appending to `dir`'s lifecycle file as process `spawn`.
fn recording_in(dir: &Path, spawn: &str) -> Discord {
    let recorder = Recorder::new(dir.join(LIFECYCLE), "node", spawn, Some(7));
    Discord::default().with_recorder(recorder)
}

/// A POST records nothing unpolled, Started and Dropped when dropped after its first poll,
/// Observed too when dropped while held, and one terminal when it completes.
#[tokio::test(flavor = "current_thread")]
async fn recorded_steps_follow_how_far_each_post_ran() {
    use LifeStep::{Accepted, Dropped, Ended, Observed, Started};
    let dir = tempfile::tempdir().unwrap();
    let discord = recording_in(dir.path(), "p");
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    drop(discord.post(C, "never polled".into()));
    let mut first = Box::pin(discord.post(C, "first poll".into()));
    assert!(first.as_mut().poll(&mut cx).is_pending());
    drop(first);
    discord.hold();
    let mut held = Box::pin(discord.post(C, "held".into()));
    for _ in 0..2 {
        assert!(held.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(
        discord.running(),
        1,
        "past its first yield, held at the valve"
    );
    drop(held);
    discord.release();
    let done = discord.post(C, "done".into()).await;
    assert!(matches!(done, PostOutcome::Created(_)), "{done:?}");
    let events = lifecycle(dir.path()).unwrap();
    let steps = |content: &str| {
        let ours = events.iter().filter(|e| e.detail == content);
        ours.map(|e| e.step).collect::<Vec<_>>()
    };
    assert_eq!(steps("never polled"), Vec::<LifeStep>::new());
    assert_eq!(steps("first poll"), [Started, Dropped]);
    assert_eq!(steps("held"), [Started, Observed, Dropped]);
    assert_eq!(steps("done"), [Started, Observed, Accepted, Ended]);
    assert_eq!(open_ids(&events), Ok(vec![]));
    let gate = events.iter().find(|e| e.step == Observed).unwrap();
    assert_eq!(
        (gate.epoch, gate.epoch_source),
        (None, Some(EpochSource::Gate))
    );
}

/// Two processes append to one lifecycle file at once: both POSTs start before either ends, every
/// line is whole, and the judge names their overlap.
#[tokio::test(flavor = "current_thread")]
async fn two_processes_append_whole_steps_to_one_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let within = Duration::from_secs(30);
    let mut nodes = [(0, MINI), (1, GW)]
        .map(|(n, role)| Node::spawn_without_db(dir.path(), n, "lifecycle_writer", role));
    for node in &mut nodes {
        let started = format!("in_flight.{}", node.role());
        node.wait_signal(&started, within).await;
    }
    signal(dir.path(), "release");
    for node in nodes {
        node.finish(within).await;
    }
    let events = lifecycle(dir.path()).unwrap();
    let lines = |done: fn(LifeStep) -> bool| {
        let ours = events.iter().filter(|e| done(e.step));
        ours.map(|e| e.line).collect::<Vec<_>>()
    };
    let (starts, ends) = (lines(|s| s == LifeStep::Started), lines(ended));
    assert_eq!((starts.len(), ends.len()), (2, 2), "{events:#?}");
    assert!(starts.iter().max() < ends.iter().min(), "{events:#?}");
    assert_eq!(open_ids(&events), Ok(vec![]));
    assert!(post_spans(&events).unwrap_err().contains("overlap"));
}

/// One node's side of the two-writer test: a recorded POST held in flight until the parent
/// releases both.
pub(super) async fn lifecycle_writer_node(env: &Env) {
    let within = Duration::from_secs(30);
    let discord = Discord::default().recording(env);
    discord.hold();
    let post = tokio::spawn(discord.post(C, format!("{} payload", env.role)));
    until("the POST is in flight", within, || discord.running() == 1).await;
    signal(&env.dir, &format!("in_flight.{}", env.role));
    wait_signal(&env.dir, "release", within).await;
    discord.release();
    let done = post.await.unwrap();
    assert!(matches!(done, PostOutcome::Created(_)), "{done:?}");
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
    recorder: Option<Arc<Recorder>>,
    reads: Mutex<Vec<HistoryRead>>,
}

/// Whether samples at `offsets` from a watch's start cover `window`: the first right at the
/// start, the last at or past its end, and no gap a whole renewal period long.
pub(super) fn observed_throughout(offsets: &[Duration], window: Duration) -> Result<(), String> {
    let renewal = crate::services::cluster::channel_home::RENEW_EVERY;
    let (first, last) = match offsets {
        [first, .., last] => (*first, *last),
        _ => return Err(format!("too few samples: {offsets:?}")),
    };
    let gap = offsets
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .max()
        .unwrap();
    if first >= renewal || last < window || gap >= renewal {
        let seen = format!(
            "{} samples, {first:?}..{last:?}, gap {gap:?}",
            offsets.len()
        );
        return Err(format!("too thin a watch of {window:?}: {seen}"));
    }
    Ok(())
}

#[test]
fn a_watch_with_a_window_long_gap_is_too_thin() {
    let window = Duration::from_secs(12);
    let even: Vec<_> = (0..=120).map(|n| Duration::from_millis(100 * n)).collect();
    assert_eq!(observed_throughout(&even, window), Ok(()));
    let gapped = [Duration::ZERO, window];
    assert!(
        observed_throughout(&gapped, window)
            .unwrap_err()
            .contains("too thin")
    );
    let short = &even[..even.len() - 1];
    assert!(
        observed_throughout(short, window).is_err(),
        "ends before the window does"
    );
    assert!(observed_throughout(&even[..1], window).is_err());
}

/// One history read as this Discord answered it, on this process's monotonic clock.
#[derive(Clone, Debug)]
pub(super) struct HistoryRead {
    pub(super) channel: u64,
    pub(super) after: u64,
    pub(super) at: std::time::Instant,
    pub(super) answer: Result<usize, String>,
}

/// A fake Discord that keeps each request's lifecycle and can hold requests open.
pub(super) struct Discord {
    state: Arc<DiscordState>,
    valve: watch::Sender<bool>,
    history_unavailable: bool,
    /// Lists another author's message after every anchor.
    chatter: bool,
}

/// What every history read of [`Discord::history_unavailable`] fails with.
pub(super) const HISTORY_UNAVAILABLE: &str = "test history unavailable";

impl Default for Discord {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            valve: watch::channel(true).0,
            history_unavailable: false,
            chatter: false,
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

    /// As this Discord, with another author's message listed after every anchor, which shows the
    /// history readable.
    pub(super) fn with_chatter(mut self) -> Self {
        self.chatter = true;
        self
    }

    /// As this Discord, appending each POST's and history read's steps to the scene's
    /// [`LIFECYCLE`].
    pub(super) fn recording(self, env: &Env) -> Self {
        let epoch = get(&env.dir, "epoch").map(|epoch| epoch.parse().unwrap());
        let file = env.dir.join(LIFECYCLE);
        self.with_recorder(Recorder::new(file, &env.role, &env.spawn, epoch))
    }

    fn with_recorder(mut self, recorder: Recorder) -> Self {
        let state = Arc::get_mut(&mut self.state).expect("a Discord not yet shared");
        state.recorder = Some(Arc::new(recorder));
        self
    }

    /// Every history read so far, in order.
    pub(super) fn history_reads(&self) -> Vec<HistoryRead> {
        locked(&self.state.reads).clone()
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
            let mut record = state.recorder.as_ref().map(|r| r.start(channel, &content));
            // The first poll runs under the home gate's admission lock; read its epoch after it.
            tokio::task::yield_now().await;
            let home_epoch = home_epoch_now(channel);
            if let Some(record) = &record {
                record.observed(home_epoch);
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
                record.accepted();
            }
            PostOutcome::Created(message)
        }
    }

    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        let mut answer = None;
        let mut read = || {
            let page = if self.history_unavailable {
                Err(HISTORY_UNAVAILABLE.to_string())
            } else {
                let created = locked(&self.state.created);
                let page = (self.state.kept.iter().chain(created.iter()))
                    .filter(|(c, m)| *c == channel && m.id > after);
                let mut page: Vec<_> = page.map(|(_, m)| m.clone()).collect();
                if self.chatter {
                    let (id, author_id, content) = (after + 1, BOT + 1, "chatter".to_owned());
                    page.push(SeenMessage {
                        id,
                        author_id,
                        content,
                    });
                }
                Ok(page)
            };
            let read = HistoryRead {
                channel,
                after,
                at: std::time::Instant::now(),
                answer: page.as_ref().map(Vec::len).map_err(Clone::clone),
            };
            locked(&self.state.reads).push(read.clone());
            answer = Some(page);
            read.answer
        };
        match &self.state.recorder {
            Some(recorder) => recorder.read(channel, after, &mut read),
            None => {
                let _ = read();
            }
        }
        std::future::ready(answer.expect("the history was read"))
    }

    fn message(
        &self,
        channel: u64,
        id: u64,
    ) -> impl Future<Output = Result<Option<SeenMessage>, String>> + Send {
        let found = if self.history_unavailable {
            Err(HISTORY_UNAVAILABLE.to_string())
        } else {
            let created = locked(&self.state.created);
            let mut known = self.state.kept.iter().chain(created.iter());
            Ok(known
                .find(|(c, m)| *c == channel && m.id == id)
                .map(|(_, m)| m.clone()))
        };
        std::future::ready(found)
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
    /// Per publish, whether the event list was still locked as it went out.
    publishes: Mutex<Vec<bool>>,
}

impl EventLog {
    fn new(events: Vec<BindingEvent>) -> Self {
        let latest = events.last().map_or(0, |event| event.seq);
        Self {
            events: Mutex::new(events),
            notice: watch::channel(latest).0,
            served: AtomicU64::new(0),
            slow_publish: AtomicU64::new(0),
            publishes: Mutex::default(),
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
        let held = matches!(
            self.events.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        );
        locked(&self.publishes).push(held);
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

fn startup_log() -> EventLog {
    let source = SourceId {
        session_id: "s1".into(),
        path: PathBuf::from("/s1.jsonl"),
        dev: 1,
        ino: 1,
    };
    let startup = TestHost::new([(C, source)]).bindings(C, ShadowProvider::Claude);
    EventLog::new(startup.binding_events_since(C, 0).unwrap())
}

fn rejected(detail: &str) -> BindingRecord {
    BindingRecord::Rejected {
        closes: None,
        detail: detail.into(),
    }
}

/// One commit publishes while its own lock on the events is still held.
#[test]
fn a_commit_publishes_before_it_unlocks_the_events() {
    let log = startup_log();
    assert_eq!(log.commit(rejected("a")), 2);
    assert_eq!(
        *locked(&log.publishes),
        [true],
        "published after the events unlocked"
    );
    assert_eq!(*log.subscribe(C).borrow(), 2);
}

/// The commits start together and seq 2's publish waits; outside the lock it would end on 2.
#[test]
fn racing_commits_publish_the_latest_seq_and_every_event() {
    let log = startup_log();
    log.slow_publish.store(2, Ordering::SeqCst);
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for detail in ["a", "b"] {
            let (log, start) = (&log, &start);
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

/// A held step of the writer host: counts the waits it entered and the ones it let through.
#[derive(Default)]
pub(super) struct Park {
    entered: AtomicUsize,
    passed: AtomicUsize,
    go: tokio::sync::Notify,
}

impl Park {
    async fn wait(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.go.notified().await;
        self.passed.fetch_add(1, Ordering::SeqCst);
    }

    /// Lets the next wait through, also one not entered yet.
    pub(super) fn release(&self) {
        self.go.notify_one();
    }

    pub(super) fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    pub(super) fn passed(&self) -> usize {
        self.passed.load(Ordering::SeqCst)
    }
}

/// How a node's writer host runs: not started, or with its facts reads or port waits held.
#[derive(Default)]
pub(super) struct HostKnobs {
    pub(super) unhosted: bool,
    pub(super) facts: Option<Arc<Park>>,
    pub(super) port: Option<Arc<Park>>,
}

/// The real writer host's gateway IO with the recording Discord in place of [`TestHost`]'s.
pub(super) struct S3Host {
    inner: Arc<TestHost>,
    pub(super) discord: Arc<Discord>,
    pub(super) lease: Lease,
    logs: Mutex<BTreeMap<u64, Arc<EventLog>>>,
    facts_park: Option<Arc<Park>>,
    port_park: Option<Arc<Park>>,
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
        let (park, discord) = (self.port_park.clone(), Arc::clone(&self.discord));
        async move {
            if let Some(park) = park {
                park.wait().await;
            }
            discord
        }
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
        let (park, facts) = (
            self.facts_park.clone(),
            self.inner.activation_facts(channel, provider),
        );
        async move {
            if let Some(park) = park {
                park.wait().await;
            }
            facts.await
        }
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

/// What a scripted reset call answers once its `go` signal comes.
#[derive(Clone, Copy, Debug)]
pub(super) enum ResetAnswer {
    Refuse(&'static str),
    Apply,
}

/// An injected `LegacyReset`: call n of `tag` signals `reset.<tag>.<n>.entered`, holds until `.go`,
/// then records its answer. Applied is its `Ok(())`; no managed-session reset or cleanup runs.
pub(super) struct ScriptedReset {
    dir: PathBuf,
    tag: String,
    script: Vec<ResetAnswer>,
    calls: Mutex<usize>,
}

impl ScriptedReset {
    /// Tagged with the child's spawn, so one generation's calls never stand for another's.
    pub(super) fn in_child(env: &Env, script: Vec<ResetAnswer>) -> channel_home_boot::LegacyReset {
        Self::tagged(&env.dir, &env.spawn, script)
    }

    fn tagged(dir: &Path, tag: &str, script: Vec<ResetAnswer>) -> channel_home_boot::LegacyReset {
        let reset = Arc::new(Self {
            dir: dir.to_path_buf(),
            tag: tag.into(),
            script,
            calls: Mutex::new(0),
        });
        Arc::new(move || {
            let reset = Arc::clone(&reset);
            Box::pin(async move { reset.call().await })
        })
    }

    /// A call past the script answers an explicit refusal at once rather than succeeding.
    async fn call(&self) -> Result<(), ResetRefused> {
        let (n, scripted) = {
            let mut calls = locked(&self.calls);
            *calls += 1;
            (*calls, self.script.get(*calls - 1).copied())
        };
        let key = format!("reset.{}.{n}", self.tag);
        signal(&self.dir, &format!("{key}.entered"));
        let answer = match scripted {
            Some(answer) => {
                while !signalled(&self.dir, &format!("{key}.go")) {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                answer
            }
            None => ResetAnswer::Refuse("unscripted call"),
        };
        let (result, record) = match answer {
            ResetAnswer::Refuse(why) => (
                Err(ResetRefused::Refused(why.into())),
                format!("refused: {why}"),
            ),
            ResetAnswer::Apply => (Ok(()), "applied".to_string()),
        };
        put(&self.dir, &key, record);
        signal(&self.dir, &format!("{key}.returned"));
        result
    }
}

pub(super) fn reset_entered(dir: &Path, tag: &str, n: usize) -> bool {
    signalled(dir, &format!("reset.{tag}.{n}.entered"))
}

/// What call n of `tag` returned, once it recorded that; `None` while held or never called.
pub(super) fn reset_returned(dir: &Path, tag: &str, n: usize) -> Option<String> {
    let returned = signalled(dir, &format!("reset.{tag}.{n}.returned"));
    returned.then(|| get(dir, &format!("reset.{tag}.{n}")).expect("recorded before returned"))
}

pub(super) fn reset_go(dir: &Path, tag: &str, n: usize) {
    signal(dir, &format!("reset.{tag}.{n}.go"));
}

/// How many calls of `tag` entered.
pub(super) fn reset_calls(dir: &Path, tag: &str) -> usize {
    (1..).take_while(|n| reset_entered(dir, tag, *n)).count()
}

/// Each scripted call holds until its `go` and answers as scripted; one past the script refuses
/// at once, and another tag's calls count apart.
#[tokio::test(flavor = "current_thread")]
async fn scripted_reset_holds_each_call_and_never_succeeds_past_its_script() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let script = vec![ResetAnswer::Refuse("busy"), ResetAnswer::Apply];
    let reset = ScriptedReset::tagged(dir, "a", script);
    for (n, expected, record) in [
        (
            1,
            Err(ResetRefused::Refused("busy".into())),
            "refused: busy",
        ),
        (2, Ok(()), "applied"),
    ] {
        let call = tokio::spawn(reset());
        until("the call enters", Duration::from_secs(5), || {
            reset_entered(dir, "a", n)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!call.is_finished(), "call {n} returned before its go");
        assert_eq!(reset_returned(dir, "a", n), None);
        reset_go(dir, "a", n);
        assert_eq!(call.await.unwrap(), expected);
        assert_eq!(reset_returned(dir, "a", n).as_deref(), Some(record));
    }
    let past = Err(ResetRefused::Refused("unscripted call".into()));
    assert_eq!(reset().await, past, "no go was sent for it");
    assert_eq!(
        reset_returned(dir, "a", 3).as_deref(),
        Some("refused: unscripted call")
    );
    let other = ScriptedReset::tagged(dir, "b", Vec::new());
    assert_eq!(other().await, past);
    assert_eq!((reset_calls(dir, "a"), reset_calls(dir, "b")), (3, 1));
}

/// The row as a refused reset must leave it; `renewed_at` and `updated_at` are left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HomeFacts {
    pub(super) channel_id: String,
    pub(super) provider: String,
    pub(super) state: HomeState,
    pub(super) holder: Option<String>,
    pub(super) target: Option<String>,
    pub(super) epoch: i64,
    pub(super) detail: Option<String>,
}

pub(super) async fn home_facts(pool: &PgPool) -> Option<HomeFacts> {
    let row = o_channel_homes::read_home(pool, &C.to_string())
        .await
        .unwrap()?;
    Some(HomeFacts {
        channel_id: row.channel_id,
        provider: row.provider,
        state: row.state,
        holder: row.holder,
        target: row.target,
        epoch: row.epoch,
        detail: row.detail,
    })
}

/// Intake rows a release must not touch: a terminal one of `C` and a pending one of another channel.
pub(super) async fn seed_outbox_samples(pool: &PgPool) {
    let done = seed(pool, "5001", None).await;
    sqlx::query("UPDATE intake_outbox SET status = 'done', completed_at = NOW() WHERE id = $1")
        .bind(done)
        .execute(pool)
        .await
        .unwrap();
    seed_on(pool, C + 1, "5002", None).await;
}

/// Every intake row, whole, by id: the samples and anything a run added.
pub(super) async fn outbox_samples(pool: &PgPool) -> Vec<String> {
    let rows = "SELECT row_to_json(io)::TEXT FROM intake_outbox io ORDER BY id";
    let rows: Vec<String> = sqlx::query_scalar(rows).fetch_all(pool).await.unwrap();
    assert_eq!(rows.len(), 2, "the two samples and nothing else: {rows:?}");
    rows
}

/// At a reset call's entry the channel has no open intake and the samples are as seeded.
pub(super) async fn quiet_at_reset(pool: &PgPool, samples: &[String]) {
    let rows = crate::db::o_channel_activation::activation_rows(pool, &C.to_string(), GW).await;
    assert_eq!(
        rows.unwrap().open_intake,
        0,
        "no open intake of the channel"
    );
    assert_eq!(outbox_samples(pool).await, samples, "the samples moved");
}

/// How long a holder may be ready before a releasing drain tries its reset: eight renewal periods.
const RESET_WINDOW: Duration = Duration::from_secs(40);

/// How long `ready` has held without a break at each sample; a false sample ends the stretch.
#[derive(Default)]
struct ReadyStretch(Option<std::time::Instant>);

impl ReadyStretch {
    fn sample(&mut self, ready: bool, at: std::time::Instant) -> Duration {
        if !ready {
            self.0 = None;
            return Duration::ZERO;
        }
        at.saturating_duration_since(*self.0.get_or_insert(at))
    }
}

#[test]
fn a_ready_stretch_restarts_after_any_false_sample() {
    let (t0, s) = (std::time::Instant::now(), Duration::from_secs);
    let mut stretch = ReadyStretch::default();
    assert_eq!(stretch.sample(true, t0), Duration::ZERO);
    assert_eq!(stretch.sample(false, t0 + s(1)), Duration::ZERO);
    assert_eq!(
        stretch.sample(true, t0 + s(41)),
        Duration::ZERO,
        "a new stretch"
    );
    assert_eq!(stretch.sample(true, t0 + s(80)), s(39));
}

#[test]
fn an_unbroken_ready_stretch_reaches_the_window() {
    let (t0, s) = (std::time::Instant::now(), Duration::from_secs);
    let mut stretch = ReadyStretch::default();
    let held: Vec<_> = [0, 20, 40]
        .map(|at| stretch.sample(true, t0 + s(at)))
        .into();
    assert_eq!(held, [Duration::ZERO, s(20), RESET_WINDOW]);
}

/// Waits for reset call `n` of `tag`, the row still `facts` when it enters. The row moving first
/// fails at once, and so does a holder `ready` without a break through [`RESET_WINDOW`] with no call.
pub(super) async fn until_reset_entered(
    pool: &PgPool,
    dir: &Path,
    (tag, n): (&str, usize),
    facts: &HomeFacts,
    mut ready: impl FnMut() -> bool,
) {
    eprintln!("[s3] until reset call {n} of {tag}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut stretch = ReadyStretch::default();
    loop {
        let entered = reset_entered(dir, tag, n);
        let row = home_facts(pool).await;
        if entered {
            let moved = format!("the row moved before reset call {n} of {tag} returned");
            assert_eq!(row.as_ref(), Some(facts), "{moved}");
            return;
        }
        let moved = format!("the row moved with reset call {n} of {tag} never entered");
        assert_eq!(row.as_ref(), Some(facts), "{moved}");
        let held = stretch.sample(ready(), std::time::Instant::now());
        assert!(
            held < RESET_WINDOW,
            "the holder stayed ready at epoch {} for {held:?} and tried no reset call {n} of {tag}",
            facts.epoch
        );
        let now = tokio::time::Instant::now();
        assert!(now < deadline, "never reached: reset call {n} of {tag}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
/// (`runtime_bootstrap.rs` `HomeRole::RestWorker`); a standby restores per channel once held,
/// as [`Booted::boot_restoring`] boots it.
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

    /// As [`Booted::boot_with`], with each drain's port running `reset` as the runtime's would.
    pub(super) async fn boot_with_reset<R>(
        env: &Env,
        reads: impl Fn(u64) -> R,
        reset: channel_home_boot::LegacyReset,
    ) -> Self
    where
        R: DrainPort + Send + Sync + 'static,
    {
        let (discord, provider) = (Discord::default(), ShadowProvider::Claude);
        Self::boot_reset_on(env, reads, reset, discord, provider, GW).await
    }

    /// As [`Booted::boot_on`], its watch adopting a released row as a restored runtime's does.
    pub(super) async fn boot_adopting(env: &Env, discord: Discord) -> Self {
        let port = |id| BootPort::reading(id, worker_restored(), no_reset());
        Self::boot_port_on(env, port, discord, ShadowProvider::Claude, GW).await
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
        Self::boot_reset_on(env, reads, no_reset(), discord, provider, gateway).await
    }

    async fn boot_reset_on<R>(
        env: &Env,
        reads: impl Fn(u64) -> R,
        reset: channel_home_boot::LegacyReset,
        discord: Discord,
        provider: ShadowProvider,
        gateway: &str,
    ) -> Self
    where
        R: DrainPort + Send + Sync + 'static,
    {
        let port = move |id| BootPort::new(reads(id), Arc::clone(&reset));
        Self::boot_port_on(env, port, discord, provider, gateway).await
    }

    /// As [`Booted::boot`], each port built by the runtime's own `BootPort::reading` with `restored`
    /// as the runtime's "persisted turns restored" flag.
    pub(super) async fn boot_reading(env: &Env, restored: Arc<AtomicBool>) -> Self {
        Self::boot_reading_with(env, restored, HostKnobs::default()).await
    }

    /// As [`Booted::boot_reading`], its writer host run as `knobs` say.
    pub(super) async fn boot_reading_with(
        env: &Env,
        restored: Arc<AtomicBool>,
        knobs: HostKnobs,
    ) -> Self {
        let port = move |id| BootPort::reading(id, Arc::clone(&restored), no_reset());
        let (discord, provider) = (Discord::default(), ShadowProvider::Claude);
        Self::boot_port_knobs(env, port, (discord, provider, GW), knobs).await
    }

    /// As [`Booted::boot_reading`] for a standby: each port restores its channel with `run` once
    /// held, its home taking no new intake until then.
    pub(super) async fn boot_restoring(env: &Env, run: channel_home_boot::ChannelRestore) -> Self {
        let port = move |id| {
            let port = BootPort::reading(id, Arc::default(), no_reset());
            port.restoring("claude", Arc::clone(&run))
        };
        let on = (Discord::default(), ShadowProvider::Claude, GW);
        Self::boot_port_knobs(env, port, on, HostKnobs::default()).await
    }

    async fn boot_port_on<R>(
        env: &Env,
        port: impl Fn(u64) -> BootPort<R>,
        discord: Discord,
        provider: ShadowProvider,
        gateway: &str,
    ) -> Self
    where
        R: DrainPort + Send + Sync + 'static,
    {
        let on = (discord, provider, gateway);
        Self::boot_port_knobs(env, port, on, HostKnobs::default()).await
    }

    async fn boot_port_knobs<R>(
        env: &Env,
        port: impl Fn(u64) -> BootPort<R>,
        (discord, provider, gateway): (Discord, ShadowProvider, &str),
        knobs: HostKnobs,
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
        let prepare = move || {
            Some(Boot {
                provider: provider_name(provider).into(),
                local,
                pool: boot_pool,
                rows,
                candidates: vec![C],
                port,
                writers: None,
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
            facts_park: knobs.facts,
            port_park: knobs.port,
        });
        let owned = channel_home_boot::delegated_boot_ownership(|_| true);
        let io = Arc::clone(&host);
        let parts_root = root.clone();
        let writers = if knobs.unhosted {
            Vec::new()
        } else {
            host::start_delegated(provider, owned, move || HostParts {
                io,
                runtime_root: Some(parts_root),
                gate: Arc::new(OwnershipGate::default()),
                readiness: host::process_readiness(),
            })
        };
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

    /// A gateway node with no home row: its writers run under its acquired gateway authority and
    /// are supervised as the runtime's, so its candidate watch can return a reclaimed channel.
    pub(super) async fn boot_gateway(env: &Env, discord: Discord) -> (Self, Arc<ChannelWriters>) {
        let (node, writers, first_spawn) = Self::boot_gateway_homes(env, discord).await;
        first_spawn();
        (node, writers)
    }

    /// [`Booted::boot_gateway`] with its homes started and the runtime's first writer spawn
    /// (`ChannelWriters::start_boot`) left for the caller to run.
    pub(super) async fn boot_gateway_homes(
        env: &Env,
        discord: Discord,
    ) -> (Self, Arc<ChannelWriters>, Box<dyn FnOnce()>) {
        let (role, provider) = (env.role.clone(), ShadowProvider::Claude);
        let root = crate::config::runtime_root().expect("node root");
        let hosts = crate::config::session_hosts::force_for_test(Some(&role), &[(C, &role)]);
        let config = node_config(&role, provider, &role);
        crate::services::tui_o::channel_policy::install(&config).expect("boot snapshot");
        let pool = env.pool().await;
        let gateway = Arc::new(OwnershipGate::default());
        gateway.acquired();
        let writers = ChannelWriters::new(Arc::clone(&gateway), host::process_readiness());
        let writers = Arc::new(writers);
        // A gateway whose Herdr endpoint runs here keeps its pane, so its release reset applies.
        let reset: channel_home_boot::LegacyReset = Arc::new(|| Box::pin(async { Ok(()) }));
        let reads = |id| ChannelHomePort::new(id, host::process_readiness(), worker_restored());
        let (local, boot_pool, boot_writers) = (role.clone(), pool.clone(), Arc::clone(&writers));
        let rows = channel_home_boot::listed(&pool);
        let prepare = move || {
            Some(Boot {
                provider: provider_name(provider).into(),
                local,
                pool: boot_pool,
                rows,
                candidates: vec![C],
                port: move |id| BootPort::new(reads(id), Arc::clone(&reset)),
                writers: Some(boot_writers),
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
            facts_park: None,
            port_park: None,
        });
        let (io, parts_root) = (Arc::clone(&host), root.clone());
        let parts = move || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(parts_root.clone()),
            gate: Arc::clone(&gateway),
            readiness: host::process_readiness(),
        };
        let spawned = Arc::clone(&writers);
        let first_spawn = Box::new(move || spawned.start_boot(provider, true, parts));
        let node = Self {
            pool,
            root,
            transcript,
            gates,
            host,
            writers: Vec::new(),
            _hosts: hosts,
        };
        (node, writers, first_spawn)
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
