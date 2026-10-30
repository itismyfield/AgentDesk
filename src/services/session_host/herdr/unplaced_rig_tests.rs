//! Unplaced Pending resolution through the production launch, the socket host and the gate's
//! residue removal, against a server that keeps workspaces, panes and their processes.
use std::collections::HashMap;
use std::sync::mpsc;

use sqlx::PgPool;

use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedOwner, HostedRecord, HostedState,
};
use crate::services::discord::{UnplacedLease, make_shared_data_for_tests};
use crate::services::herdr_launch::unplaced::{Resolution, Unplaced, resolve_unplaced};
use crate::services::herdr_launch::{
    HerdrLaunch, HerdrLaunchCommand, HerdrLaunchOutcome, launch_herdr_session, workspace_marker,
};
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use poise::serenity_prelude::ChannelId;

const CHANNEL: u64 = 4_207_563;
const KEY: &str = "AgentDesk-claude-unplaced";
const READS: [&str; 4] = [
    "pane.get",
    "pane.read",
    "pane.process_info",
    "session.snapshot",
];

struct Proc {
    parent: u32,
    start: ProcessStart,
    environ: Vec<String>,
    alive: bool,
}

struct Workspace {
    id: String,
    label: String,
    panes: Vec<String>,
}

/// What the server holds; a pane write other than input removes its pane.
#[derive(Default)]
struct Herdr {
    workspaces: Vec<Workspace>,
    shells: HashMap<String, u32>,
    procs: HashMap<u32, Proc>,
    next: u32,
    lose_create_reply: bool,
    /// Labels arrive as a launch from before the marker sent them.
    bare_labels: bool,
    /// Removal requests are answered but change nothing.
    ignore_removals: bool,
    creates: usize,
    inputs: usize,
    removed: Vec<String>,
    /// Signalled when an input arrives; its reply then waits a moment.
    on_input: Option<mpsc::Sender<()>>,
}

type Shared = Arc<Mutex<Herdr>>;

fn now_start() -> ProcessStart {
    let wall_clock = SystemTime::now();
    let since = wall_clock.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    ProcessStart {
        identity: StartIdentity::Darwin {
            seconds: since.as_secs(),
            micros: u64::from(since.subsec_micros()),
        },
        wall_clock,
    }
}

impl Herdr {
    fn spawn(&mut self, parent: u32, environ: Vec<String>) -> u32 {
        self.next += 1;
        let pid = 1_000 + self.next;
        let start = now_start();
        let alive = true;
        self.procs.insert(
            pid,
            Proc {
                parent,
                start,
                environ,
                alive,
            },
        );
        pid
    }

    /// A workspace with one pane whose shell is alone in the foreground.
    fn workspace(&mut self, label: &str) -> String {
        self.next += 1;
        let id = format!("w{}", self.next);
        let pane = format!("{id}:p1");
        let shell = self.spawn(1, vec!["HOME=/x".into()]);
        self.shells.insert(pane.clone(), shell);
        let (label, panes) = (label.to_owned(), vec![pane.clone()]);
        self.workspaces.push(Workspace { id, label, panes });
        pane
    }

    /// Another pane in `pane`'s workspace, as a person splits it.
    fn split(&mut self, pane: &str) -> String {
        let shell = self.spawn(1, vec!["HOME=/x".into()]);
        let workspace = self
            .workspaces
            .iter_mut()
            .find(|w| w.panes.iter().any(|p| p == pane));
        let workspace = workspace.unwrap();
        let split = format!("{}:p{}", workspace.id, workspace.panes.len() + 1);
        workspace.panes.push(split.clone());
        self.shells.insert(split.clone(), shell);
        split
    }

    /// The pane's shell runs a provider whose environment names `context`.
    fn run(&mut self, pane: &str, context: &Path) {
        let shell = self.shells[pane];
        let environ = vec![format!("AGENTDESK_BINDING_CONTEXT={}", context.display())];
        self.spawn(shell, environ);
    }

    fn drop_pane(&mut self, pane: &str) {
        for workspace in &mut self.workspaces {
            workspace.panes.retain(|p| p != pane);
        }
        self.workspaces.retain(|w| !w.panes.is_empty());
        if let Some(shell) = self.shells.remove(pane) {
            for proc in self
                .procs
                .iter_mut()
                .filter(|(pid, p)| **pid == shell || p.parent == shell)
            {
                proc.1.alive = false;
            }
        }
    }

    fn foreground(&self, pane: &str) -> Vec<u32> {
        let shell = self.shells[pane];
        let children: Vec<u32> = self
            .procs
            .iter()
            .filter(|(_, p)| p.alive && p.parent == shell)
            .map(|(pid, _)| *pid)
            .collect();
        if children.is_empty() {
            vec![shell]
        } else {
            children
        }
    }

    fn labels(&self) -> Vec<String> {
        self.workspaces.iter().map(|w| w.label.clone()).collect()
    }

    fn reply(&mut self, request: &Value) -> Value {
        let method = request["method"].as_str().unwrap_or("");
        let params = &request["params"];
        let pane = params["pane_id"].as_str().unwrap_or("").to_owned();
        match method {
            "session.snapshot" => {
                let panes: Vec<Value> = self
                    .workspaces
                    .iter()
                    .flat_map(|w| {
                        w.panes.iter().map(|p| {
                            json!({"pane_id": p, "workspace_id": w.id,
                        "tab_id": format!("{}:t1", w.id), "revision": 0, "cwd": "/tmp"})
                        })
                    })
                    .collect();
                let workspaces: Vec<Value> = self.workspaces.iter().map(|w| json!({
                    "workspace_id": w.id, "label": w.label, "pane_count": w.panes.len(), "tab_count": 1
                })).collect();
                json!({"type": "session_snapshot", "snapshot": {"version": "0.9.3", "protocol": 22,
                    "panes": panes, "workspaces": workspaces}})
            }
            "pane.process_info" => {
                let foreground: Vec<Value> = self
                    .foreground(&pane)
                    .iter()
                    .map(|pid| json!({"pid": pid}))
                    .collect();
                json!({"type": "pane_process_info", "process_info": {"pane_id": pane,
                    "shell_pid": self.shells[&pane], "foreground_processes": foreground}})
            }
            "workspace.create" => {
                self.creates += 1;
                let label = params["label"].as_str().unwrap();
                let label = match self.bare_labels {
                    true => label.split('#').next().unwrap(),
                    false => label,
                };
                let created = self.workspace(label);
                let cwd = params["cwd"].clone();
                json!({"type": "workspace_created", "root_pane": {"pane_id": created,
                    "workspace_id": created.split(':').next(), "tab_id": "t1", "revision": 0, "cwd": cwd}})
            }
            "pane.send_input" => {
                self.inputs += 1;
                let text = params["text"].as_str().unwrap_or("");
                if let Some(context) = text.strip_prefix("provider ") {
                    self.run(&pane, Path::new(context));
                }
                json!({"type": "ok"})
            }
            method if !READS.contains(&method) && !pane.is_empty() => {
                if !self.ignore_removals {
                    self.removed.push(pane.clone());
                    self.drop_pane(&pane);
                }
                json!({"type": "ok"})
            }
            _ => json!({"type": "ok"}),
        }
    }
}

/// The process table behind the server's panes.
struct RigOs(Shared);

impl ProcessOs for RigOs {
    fn exists(&self, pid: u32) -> Result<bool, RestoreUnverified> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .procs
            .get(&pid)
            .is_some_and(|p| p.alive))
    }
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        let herdr = self.0.lock().unwrap();
        herdr
            .procs
            .get(&pid)
            .map(|p| p.parent)
            .ok_or(RestoreUnverified::ProcessUnreadable)
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let herdr = self.0.lock().unwrap();
        herdr
            .procs
            .get(&pid)
            .map(|p| p.start)
            .ok_or(RestoreUnverified::ProcessUnreadable)
    }
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        let herdr = self.0.lock().unwrap();
        let found = herdr.procs.get(&pid).map(|p| p.environ.clone());
        found.ok_or(RestoreUnverified::ProcessUnreadable)
    }
    fn exec_path(&self, _pid: u32) -> Option<String> {
        None
    }
}

/// A channel's row on PG, the stateful server and a host over it.
struct Rig {
    _root: crate::config::TestRuntimeRootGuard,
    db: crate::db::auto_queue::test_support::TestPostgresDb,
    pool: PgPool,
    owner: HostedOwner,
    server: Server,
    herdr: Shared,
}

impl Rig {
    async fn new() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared = make_shared_data_for_tests();
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: CHANNEL.to_string(),
            logical_key: KEY.into(),
            owner_node: "mac-mini".into(),
            runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
        };
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                   discord_token_hash, channel_id, hosted_execution)
             VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, NULL)",
        )
        .bind(format!(
            "claude/{}/mac-mini:{KEY}",
            owner.discord_token_hash
        ))
        .bind(&owner.discord_token_hash)
        .bind(&owner.channel_id)
        .execute(&pool)
        .await
        .unwrap();
        let herdr = Shared::default();
        let state = herdr.clone();
        let server = serve(Box::new(move |request, _| {
            let lost =
                request["method"] == "workspace.create" && state.lock().unwrap().lose_create_reply;
            let input = request["method"] == "pane.send_input";
            let reply = state.lock().unwrap().reply(request);
            let signal = input
                .then(|| state.lock().unwrap().on_input.take())
                .flatten();
            if let (true, Some(signal)) = (input, signal) {
                let _ = signal.send(());
                std::thread::sleep(Duration::from_millis(300));
            }
            if lost {
                std::thread::sleep(Duration::from_millis(800));
            }
            reply
        }));
        Self {
            _root: root,
            db,
            pool,
            owner,
            server,
            herdr,
        }
    }

    fn host(&self) -> Arc<dyn HerdrLaunchHost> {
        let mut host = launch_host(&self.server);
        host.os = Box::new(RigOs(self.herdr.clone()));
        host.provider_window = Duration::from_millis(500);
        Arc::new(host)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Herdr> {
        self.herdr.lock().unwrap()
    }

    /// The production launch; its command starts a provider naming the launch's own context.
    async fn launch(
        &self,
        host: &Arc<dyn HerdrLaunchHost>,
    ) -> Result<HerdrLaunchOutcome, crate::services::herdr_launch::HerdrLaunchError> {
        let (_dir, cwd) = workdir();
        let launch = HerdrLaunch {
            endpoint: Some(launch_endpoint(&self.server)),
            owner: self.owner.clone(),
            channel_id: Some(CHANNEL),
            expected_native_session_id: None,
            resume: false,
            provider_root: None,
        };
        let prepare = |prepared: &PreparedIncarnation| {
            Ok(HerdrLaunchCommand {
                cwd: cwd.clone(),
                command: format!("provider {}", prepared.path.display()),
            })
        };
        launch_herdr_session(&self.pool, launch, prepare, host.clone()).await
    }

    async fn resolve(&self, host: &Arc<dyn HerdrLaunchHost>, nonce: &str) -> Resolution {
        let shared = make_shared_data_for_tests();
        let lease = UnplacedLease::try_acquire(&shared, ChannelId::new(CHANNEL)).unwrap();
        let endpoint = launch_endpoint(&self.server);
        let target = Unplaced {
            pool: &self.pool,
            owner: &self.owner,
            nonce,
            endpoint: &endpoint,
            host: host.clone(),
            quiet: Duration::ZERO,
        };
        resolve_unplaced(&lease, &target).await
    }

    async fn record(&self) -> HostedExecution {
        let raw: Option<Value> =
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
                .bind(CHANNEL.to_string())
                .fetch_one(&self.pool)
                .await
                .unwrap();
        match HostedRecord::decode(raw.as_ref()) {
            HostedRecord::Known(record) => record,
            other => panic!("{other:?}"),
        }
    }

    /// A launch whose create reply is lost after the server made its pane.
    async fn lost_create(&self, host: &Arc<dyn HerdrLaunchHost>) -> String {
        self.state().lose_create_reply = true;
        let outcome = self.launch(host).await;
        self.state().lose_create_reply = false;
        let Ok(HerdrLaunchOutcome::Indeterminate {
            execution_nonce, ..
        }) = outcome
        else {
            panic!("{outcome:?}");
        };
        // The server's late reply has gone out before anything reads it again.
        tokio::time::sleep(Duration::from_millis(400)).await;
        execution_nonce
    }

    async fn end(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

/// Another execution of this channel, launched earlier, with its own context file.
fn other_execution(rig: &Rig) -> (String, PathBuf) {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let context = BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.clone(),
        tmux_session: KEY.into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: rig.owner.runtime_root.clone(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
    };
    (nonce, PreparedIncarnation::create(context).unwrap().path)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_create_reply_leaves_one_shell_that_is_removed_before_one_fresh_launch_pg() {
    let rig = Rig::new().await;
    let host = rig.host();
    let n = rig.lost_create(&host).await;
    let residue = {
        let herdr = rig.state();
        assert_eq!(
            (herdr.inputs, herdr.labels()),
            (0, vec![workspace_marker(KEY, &n)])
        );
        herdr.workspaces[0].panes[0].clone()
    };
    let pending = rig.record().await;

    // A removal the server does not carry out is not confirmed: the row stays and nothing launches.
    rig.state().ignore_removals = true;
    let held = rig.resolve(&host, &n).await;
    assert!(
        matches!(held, Resolution::Held(ref why) if why.contains("not confirmed")),
        "{held:?}"
    );
    assert_eq!(rig.record().await, pending);
    rig.state().ignore_removals = false;

    assert_eq!(rig.resolve(&host, &n).await, Resolution::Retired);
    assert_eq!(rig.state().removed, vec![residue]);
    assert!(rig.state().workspaces.is_empty());
    assert_eq!(
        (rig.record().await.state, rig.record().await.execution_nonce),
        (HostedState::Retired, n.clone())
    );

    let fresh = rig.launch(&host).await;
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce: m,
        evidence: Ok(()),
        ..
    }) = fresh
    else {
        panic!("{fresh:?}");
    };
    {
        let herdr = rig.state();
        assert_eq!(
            herdr.labels(),
            vec![workspace_marker(KEY, &m)],
            "one pane for the channel"
        );
        assert_eq!((herdr.creates, herdr.inputs), (2, 1));
    }
    rig.end().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pre_marker_launch_whose_provider_started_is_adopted_after_a_restart_pg() {
    let rig = Rig::new().await;
    rig.state().bare_labels = true;
    let (started, input) = mpsc::channel();
    rig.state().on_input = Some(started);
    let first = rig.host();
    let launch = {
        let (pool, owner, endpoint) = (
            rig.pool.clone(),
            rig.owner.clone(),
            launch_endpoint(&rig.server),
        );
        let host = first.clone();
        tokio::spawn(async move {
            let (_dir, cwd) = workdir();
            let launch = HerdrLaunch {
                endpoint: Some(endpoint),
                owner,
                channel_id: Some(CHANNEL),
                expected_native_session_id: None,
                resume: false,
                provider_root: None,
            };
            let prepare = |prepared: &PreparedIncarnation| {
                Ok(HerdrLaunchCommand {
                    cwd: cwd.clone(),
                    command: format!("provider {}", prepared.path.display()),
                })
            };
            launch_herdr_session(&pool, launch, prepare, host).await
        })
    };
    // The process ends after the provider started and before its pane is recorded.
    tokio::task::spawn_blocking(move || input.recv().unwrap())
        .await
        .unwrap();
    launch.abort();
    assert!(launch.await.is_err());
    tokio::time::sleep(Duration::from_millis(600)).await;
    let pending = rig.record().await;
    assert_eq!(
        (pending.state, &pending.location),
        (HostedState::Pending, &None)
    );
    assert_eq!(rig.state().labels(), vec![KEY.to_string()]);

    let restarted = rig.host();
    assert_eq!(
        rig.resolve(&restarted, &pending.execution_nonce).await,
        Resolution::Adopted
    );
    let adopted = rig.record().await;
    let pane = rig.state().workspaces[0].panes[0].clone();
    assert_eq!(adopted.state, HostedState::Pending);
    assert_eq!(
        adopted.location.map(|location| location.pane_id),
        Some(pane)
    );
    assert!(adopted.expected.is_some());
    {
        let herdr = rig.state();
        assert_eq!(
            (herdr.creates, herdr.removed.len()),
            (1, 0),
            "no retire, removal or fresh launch"
        );
    }
    rig.end().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn panes_the_resolver_cannot_attribute_hold_the_row_and_are_never_written_pg() {
    let rig = Rig::new().await;
    let host = rig.host();
    let n = rig.lost_create(&host).await;
    let own = rig.state().workspaces[0].panes[0].clone();
    let contexts = PathBuf::from(&rig.owner.runtime_root).join("runtime/binding_contexts/claude");
    let own_context = contexts.join(format!("{n}.json"));
    let (m, m_context) = other_execution(&rig);
    let (orphan, earlier, person, user, split) = {
        let mut herdr = rig.state();
        herdr.run(&own, &own_context);
        let orphan = herdr.workspace(&workspace_marker(KEY, &m));
        herdr.run(&orphan, &m_context);
        let elsewhere = herdr.workspace(&workspace_marker("AgentDesk-claude-elsewhere", &m));
        herdr.run(&elsewhere, &m_context);
        let earlier = herdr.workspace(&workspace_marker(
            KEY,
            &uuid::Uuid::new_v4().simple().to_string(),
        ));
        let person = herdr.workspace("notes");
        let user = herdr.workspace(KEY);
        let split = herdr.split(&own);
        (orphan, earlier, person, user, split)
    };
    let pending = rig.record().await;
    let runs = [
        ("residue_touched", Some(split)),
        ("legacy_unattributed", Some(user)),
        ("unrecorded executions", None),
    ];
    for (why, removed) in runs {
        let held = rig.resolve(&host, &n).await;
        assert!(
            matches!(held, Resolution::Held(ref reason) if reason.contains(why)),
            "{why}: {held:?}"
        );
        assert_eq!(rig.record().await, pending, "{why}");
        let herdr = rig.state();
        assert!(herdr.removed.is_empty() && herdr.creates == 1, "{why}");
        drop(herdr);
        if let Some(pane) = removed {
            rig.state().drop_pane(&pane);
        }
    }
    // A context this runtime cannot read is not someone else's: the row stays held.
    std::fs::write(&m_context, b"{").unwrap();
    let held = rig.resolve(&host, &n).await;
    assert!(matches!(held, Resolution::Held(_)), "{held:?}");
    assert_eq!(rig.record().await, pending);
    rig.state().drop_pane(&orphan);
    assert_eq!(rig.resolve(&host, &n).await, Resolution::Adopted);
    assert!(rig.state().removed.is_empty());
    let left: Vec<String> = rig
        .state()
        .workspaces
        .iter()
        .flat_map(|w| w.panes.clone())
        .collect();
    assert!(
        left.contains(&earlier) && left.contains(&person) && left.contains(&own),
        "{left:?}"
    );
    let person_reads = rig
        .server
        .conns
        .lock()
        .unwrap()
        .iter()
        .filter(|(request, _)| {
            request
                .as_ref()
                .is_some_and(|r| r["params"]["pane_id"] == person.as_str())
        })
        .count();
    assert_eq!(
        person_reads, 0,
        "a pane outside the channel's labels is never read"
    );
    rig.end().await;
}

/// Delegates to the socket host, pausing before the channel's panes are read.
struct Paused {
    inner: Arc<dyn HerdrLaunchHost>,
    reached: Mutex<Option<mpsc::Sender<()>>>,
    resume: Mutex<Option<mpsc::Receiver<()>>>,
}

impl HerdrLaunchHost for Paused {
    fn mode(&self, endpoint: &HerdrLaunchEndpoint) -> EndpointMode {
        self.inner.mode(endpoint)
    }
    fn restore_resume(&self, endpoint: &HerdrLaunchEndpoint) -> RestoreResume {
        self.inner.restore_resume(endpoint)
    }
    fn create(
        &self,
        request: &crate::services::herdr_launch::HerdrCreateRequest,
    ) -> HerdrCreateOutcome {
        self.inner.create(request)
    }
    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        self.inner.launch_evidence(probe)
    }
    fn unplaced_witness(
        &self,
        endpoint: &HerdrLaunchEndpoint,
        nonce: &str,
    ) -> Result<ServerWitness, String> {
        self.inner.unplaced_witness(endpoint, nonce)
    }
    fn channel_panes(
        &self,
        endpoint: &HerdrLaunchEndpoint,
        key: &str,
        witness: &ServerWitness,
    ) -> Result<Vec<ChannelPane>, String> {
        if let Some(reached) = self.reached.lock().unwrap().take() {
            reached.send(()).unwrap();
            let resume = self.resume.lock().unwrap().take().unwrap();
            resume.recv().unwrap();
        }
        self.inner.channel_panes(endpoint, key, witness)
    }
    fn dispose_residue(
        &self,
        endpoint: &HerdrLaunchEndpoint,
        nonce: &str,
        residue: &Residue,
    ) -> Result<(), String> {
        self.inner.dispose_residue(endpoint, nonce, residue)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resolver_that_holds_the_lease_excludes_the_next_until_it_retires_pg() {
    let rig = Rig::new().await;
    let host = rig.host();
    let n = rig.lost_create(&host).await;
    let (reached, paused) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    let paused_host: Arc<dyn HerdrLaunchHost> = Arc::new(Paused {
        inner: host.clone(),
        reached: Mutex::new(Some(reached)),
        resume: Mutex::new(Some(resumed)),
    });
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(CHANNEL);
    let lease = UnplacedLease::try_acquire(&shared, channel).unwrap();
    let holder = {
        let (pool, owner, endpoint, nonce) = (
            rig.pool.clone(),
            rig.owner.clone(),
            launch_endpoint(&rig.server),
            n.clone(),
        );
        tokio::spawn(async move {
            let target = Unplaced {
                pool: &pool,
                owner: &owner,
                nonce: &nonce,
                endpoint: &endpoint,
                host: paused_host,
                quiet: Duration::ZERO,
            };
            resolve_unplaced(&lease, &target).await
        })
    };
    tokio::task::spawn_blocking(move || paused.recv().unwrap())
        .await
        .unwrap();
    let requests = rig.server.methods().len();
    let next = UnplacedLease::acquire(&shared, channel).await;
    assert!(
        next.is_err(),
        "R1 entered the critical section while R2 held the lease"
    );
    assert_eq!(
        rig.server.methods().len(),
        requests,
        "the excluded resolver read nothing"
    );
    assert_eq!(rig.record().await.state, HostedState::Pending);
    resume.send(()).unwrap();
    assert_eq!(holder.await.unwrap(), Resolution::Retired);

    let fresh = rig.launch(&host).await;
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce: m, ..
    }) = fresh
    else {
        panic!("{fresh:?}");
    };
    let held = rig.resolve(&host, &n).await;
    assert!(matches!(held, Resolution::Held(_)), "{held:?}");
    let current = rig.record().await;
    assert_eq!(
        (current.state, current.execution_nonce.as_str()),
        (HostedState::Pending, m.as_str())
    );
    assert_eq!(rig.state().labels(), vec![workspace_marker(KEY, &m)]);
    assert_eq!(
        rig.state().removed.len(),
        1,
        "only the lost create's shell was removed"
    );
    {
        let herdr = rig.state();
        let pane = &herdr.workspaces[0].panes[0];
        assert!(
            herdr
                .foreground(pane)
                .iter()
                .all(|pid| herdr.procs[pid].alive),
            "M lives"
        );
    }
    rig.end().await;
}
