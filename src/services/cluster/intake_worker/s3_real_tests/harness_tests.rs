//! Two simulated nodes over one PG. Each node is a child test process with its own runtime root,
//! home-gate registry, writer readiness and Discord recorder; the parent owns the database.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::adoption::LegacyView;
use crate::services::tui_o::writer::host::test_io::{Alarms, Startup, TestHost};
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
}

impl Scene {
    /// A migrated database with the channel's agent and both nodes registered as live workers.
    pub(super) async fn new() -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for node in [GW, MINI] {
            sqlx::query(
                "INSERT INTO worker_nodes (instance_id, status, role, effective_role, labels,
                 capabilities, last_heartbeat_at, started_at, updated_at)
                 VALUES ($1, 'online', 'worker', 'worker', '[]', $2, NOW(), NOW(), NOW())",
            )
            .bind(node)
            .bind(serde_json::json!({"intake_worker": {"enabled": true, "providers": ["claude"]}}))
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ($1, 'Writer', 'claude', $2)",
        )
        .bind(format!("writer-{C}"))
        .bind(C.to_string())
        .execute(&pool)
        .await
        .unwrap();
        Self {
            dir: tempfile::tempdir().unwrap(),
            db: Some(db),
            pool,
            spawned: AtomicUsize::new(0),
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
        let channel = C.to_string();
        let home =
            applied(o_channel_homes::delegate(&self.pool, &channel, "claude", GW, MINI).await);
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

#[derive(Default)]
struct DiscordState {
    created: Mutex<Vec<(u64, SeenMessage)>>,
    seen: Mutex<Vec<Seen>>,
    running: AtomicUsize,
    most_running: AtomicUsize,
}

/// A fake Discord that keeps each request's lifecycle and can hold requests open.
pub(super) struct Discord {
    state: Arc<DiscordState>,
    valve: watch::Sender<bool>,
}

impl Default for Discord {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            valve: watch::channel(true).0,
        }
    }
}

impl Discord {
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
            // The first poll runs under the home gate's admission lock; read its epoch after it.
            tokio::task::yield_now().await;
            let home_epoch = home_epoch_now(channel);
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
                let last = created.last().map_or(0, |(_, m)| m.id);
                let id = micros.max(last + 1);
                let message = SeenMessage {
                    id,
                    author_id: BOT,
                    content: content.clone(),
                };
                created.push((channel, message.clone()));
                message
            };
            locked(&state.seen).push(Seen::Accepted(content));
            request.accepted = true;
            PostOutcome::Created(message)
        }
    }

    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        let created = locked(&self.state.created);
        let page = created
            .iter()
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

/// The real writer host's gateway IO with the recording Discord in place of [`TestHost`]'s.
pub(super) struct S3Host {
    inner: Arc<TestHost>,
    pub(super) discord: Arc<Discord>,
    pub(super) lease: Lease,
}

impl HostIo for S3Host {
    type Port = Discord;
    type Lease = Lease;
    type Alarms = Alarms;
    type Bindings = Startup;

    fn port(&self) -> impl Future<Output = Arc<Discord>> + Send {
        std::future::ready(Arc::clone(&self.discord))
    }

    fn lease(&self) -> Lease {
        self.lease.clone()
    }

    fn alarms(&self) -> Alarms {
        self.inner.alarms()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Startup> {
        self.inner.bindings(channel, provider)
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
fn node_config(role: &str) -> crate::config::Config {
    serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": {"enabled": true, "instance_id": role, "gateway_preferred_instance_id": GW},
        "runtime": {"channel_home_delegation_enabled": true},
        "tui_o": {"writer": {"channels": [C]}},
        "agents": [{"id": format!("writer-{C}"), "name": "Writer",
            "channels": {"claude": {"id": C.to_string(), "runtime": "tui"}}}],
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
        Self::boot_with(env, |id| {
            let readiness = host::process_readiness();
            ChannelHomePort::new(id, readiness, worker_restored())
        })
        .await
    }

    /// As [`Booted::boot`], with the drain reading through `reads`.
    pub(super) async fn boot_with<R>(env: &Env, reads: impl Fn(u64) -> R) -> Self
    where
        R: DrainPort + Send + Sync + 'static,
    {
        let role = env.role.clone();
        let root = crate::config::runtime_root().expect("node root");
        let hosts = crate::config::session_hosts::force_for_test(Some(&role), &[(C, &role)]);
        let config = node_config(&role);
        crate::services::tui_o::channel_policy::install(&config).expect("boot snapshot");
        let pool = env.pool().await;
        let rows = channel_home_boot::listed(&pool);
        let (local, boot_pool) = (role.clone(), pool.clone());
        let port = move |id| BootPort::new(reads(id), no_reset());
        let prepare = move || {
            Some(Boot {
                provider: "claude".into(),
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
            discord: Arc::default(),
            lease: Lease::default(),
        });
        let owned = channel_home_boot::delegated_boot_ownership(|_| true);
        let io = Arc::clone(&host);
        let parts_root = root.clone();
        let writers = host::start_delegated(ShadowProvider::Claude, owned, move || HostParts {
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
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if self.discord().posts() != expected {
            let alarms = locked(&self.host.inner.alarms.0).clone();
            let owed = self.owed().await;
            let transcript = std::fs::read_to_string(&self.transcript).unwrap_or_default();
            panic!(
                "posts {:?} != {expected:?}\nseen {:?}\nalarms {alarms:?}\nowed {owed:?}\n{transcript}",
                self.discord().posts(),
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
    let channel = C.to_string();
    let ctx = IntakeRouterContext {
        mode: IntakeRoutingMode::Enforce,
        leader_instance_id: leader,
        provider: "claude",
        channel_id: &channel,
        policy_channel_id: &channel,
        user_msg_id: message,
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
    };
    try_route_intake(pool, &ctx).await
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
    let payload = crate::db::intake_outbox::InsertPendingPayload {
        target_instance_id: MINI.into(),
        forwarded_by_instance_id: GW.into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: channel.to_string(),
        user_msg_id: message.into(),
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
