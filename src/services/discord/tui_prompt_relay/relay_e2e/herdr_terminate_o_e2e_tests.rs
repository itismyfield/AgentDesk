//! A terminated Herdr execution's output stays O's: its sealed tail posts once, a successor bind
//! keeps it, an announced unit stays unsealed, and the service writes or posts nothing of O's.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::{CHANNEL_ID, RelayE2eHarness, wait_until};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedExecution, HostedLocation, HostedOwner,
};
use crate::services::claude_tui::hook_server::adoption_retry::DurableKind;
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, observe_binding_hook,
};
use crate::services::discord::herdr_terminate::{
    OperatorTerminate, TerminationResult, terminate_explicit_herdr, test_turn,
};
use crate::services::discord::recovery_engine::host_reconcile::{
    HerdrEndpointId, HerdrExecutionReader, HerdrPaneEvidence, HerdrPaneReading,
};
use crate::services::discord::tui_prompt_relay::herdr_source::{
    HerdrSourceAttach, attach_launched_codex_herdr_source, attach_launched_herdr_source,
};
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunch, HerdrLaunchCommand,
    HerdrLaunchEndpoint, HerdrLaunchHost, HerdrLaunchOutcome, launch_herdr_session,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_termination_rig as herdr;
use crate::services::session_host::{EvidenceGap, RestoreResume, ServerWitness};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, MAX_READ_BYTES, ShadowProvider, SourceId,
};
use crate::services::tui_o::store::rotation::Successor as Rotated;
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::{
    ChannelStore, InitSource, Initialized, OEra, OStore, STORE_DIR_NAME, StoreConfig,
};
use crate::services::tui_o::writer::actor::{Owing, Undelivered};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
    ChannelBindingLog,
};
use crate::services::tui_o::writer::deliver::{ChannelWriter, StopCause};
use crate::services::tui_o::writer::host::test_io::{Alarms, AnyLease, Posts};
use crate::services::tui_o::writer::host::{self, Custody, HostIo, HostParts, OwedView, Readiness};
use crate::services::tui_o::writer::{WriterAlarm, WriterConfig, actor};
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, BindingContext, HookBindingEnvelope, PreparedIncarnation,
};
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding, binding_events};

/// Hang guard only: a wait that runs out fails the test without deciding it.
const WAIT: Duration = Duration::from_secs(30);
/// How many actor polls a positive wait observes before its assertion decides.
const POLLS: usize = 30;
const TAIL: &str = "the sealed tail written just before the termination";
const RESTART_OUTPUT: &str = "the restarted writer's own new output";

/// Runs `scenario` in a child owning its runtime: the termination rig on every runtime thread,
/// and the binding log under one root every thread reads.
fn in_child(path: &str, scenario: fn(herdr::P11TerminationRig, PathBuf) -> BoxedRun) {
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    let events = tempfile::tempdir().unwrap();
    let root = events.path().to_path_buf();
    let rig_install = rig.thread_installer();
    let events_root = root.clone();
    let install = Arc::new(move || {
        rig_install();
        binding_events::set_test_root(Some(&events_root));
    });
    install();
    let on_start = Arc::clone(&install);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(move || on_start())
        .build()
        .unwrap()
        .block_on(scenario(rig, root));
}

type BoxedRun = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

#[test]
fn m2t_t6_sealed_tail_delivered_once_after_termination_pg() {
    let path = concat!(
        module_path!(),
        "::m2t_t6_sealed_tail_delivered_once_after_termination_pg"
    );
    in_child(path, |rig, events| {
        Box::pin(sealed_tail(rig, events, false))
    });
}

#[test]
fn m2t_t6b_successor_launch_preserves_a_tail_and_cursor_pg() {
    let path = concat!(
        module_path!(),
        "::m2t_t6b_successor_launch_preserves_a_tail_and_cursor_pg"
    );
    in_child(path, |rig, events| Box::pin(sealed_tail(rig, events, true)));
}

#[test]
fn m2t_t7_codex_execution_unsealed_survives_its_own_termination_pg() {
    let path = concat!(
        module_path!(),
        "::m2t_t7_codex_execution_unsealed_survives_its_own_termination_pg"
    );
    in_child(path, |rig, _| Box::pin(unsealed(rig)));
}

#[test]
fn m2t_t7u_o_actor_unsealed_survives_rotation_stall_pg() {
    let path = concat!(
        module_path!(),
        "::m2t_t7u_o_actor_unsealed_survives_rotation_stall_pg"
    );
    in_child(path, |rig, _| Box::pin(unsealed_actor(rig)));
}

/// The harness's bound channel, its isolated database and the session key under `provider`.
type Opened = (RelayE2eHarness, TestPostgresDb, sqlx::PgPool, String);

/// An execution of the rig on the harness's bound channel, Bound once its test binds it.
struct Terminating {
    harness: RelayE2eHarness,
    database: TestPostgresDb,
    pool: sqlx::PgPool,
    session_key: String,
    logical: String,
    owner: HostedOwner,
    location: HostedLocation,
    expected: ExpectedExecution,
    /// The running turn the service captures; a Codex execution here runs none.
    token: Option<Arc<crate::services::provider::CancelToken>>,
    _switches: herdr::LiveSwitches,
}

impl Terminating {
    async fn open(provider: &ProviderKind) -> Opened {
        let mut database = None;
        let harness = RelayE2eHarness::start_bound_on(async {
            let fixture = TestPostgresDb::create().await;
            let pool = fixture.connect_and_migrate().await;
            database = Some(fixture);
            pool
        })
        .await;
        let database = database.expect("an isolated PostgreSQL database");
        let pool = harness.shared.pg_pool.clone().expect("PG-backed runtime");
        let shared = harness.shared.clone();
        harness.answer_placeholders_immediately();
        shared.restart.reconcile_done.store(true, Ordering::SeqCst);
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&harness.channel_id);
        let named = session
            .expect("bound fixture channel")
            .channel_name
            .replace("p11-m2t".into());
        assert!(named.is_none());
        drop(core);
        let session_key = crate::services::discord::adk_session::build_adk_session_key(
            &shared,
            harness.channel_id,
            provider,
            None,
        )
        .await
        .expect("a namespaced session key");
        (harness, database, pool, session_key)
    }

    fn new(
        (harness, database, pool, session_key): Opened,
        record: HostedExecution,
        token: Option<Arc<crate::services::provider::CancelToken>>,
        switches: herdr::LiveSwitches,
    ) -> Self {
        Self {
            harness,
            database,
            pool,
            logical: record.owner.logical_key.clone(),
            owner: record.owner,
            location: record.location.expect("a recorded pane"),
            expected: record.expected.expect("the launch evidence"),
            session_key,
            token,
            _switches: switches,
        }
    }

    /// The rig's Claude execution `EXECUTION`, Bound with its turn running.
    async fn bind(rig: &herdr::P11TerminationRig) -> Self {
        let opened = Self::open(&ProviderKind::Claude).await;
        let (harness, _, pool, session_key) = &opened;
        let bound = herdr::HostedState::Bound;
        let token_hash = harness.shared.token_hash.clone();
        let switches = rig.bind(pool, &token_hash, session_key, bound).await;
        let record = rig.record(&token_hash, session_key);
        let (shared, channel) = (&harness.shared, harness.channel_id);
        let token = test_turn::start(shared, channel, 51, "turn-a").await;
        Self::new(opened, record, Some(token), switches)
    }

    /// Codex execution `EXECUTION` Pending on the rig's pane as its launch and first prompt leave
    /// it: its launch context and spawn marker, its pane's placeholder, and its sessions root.
    async fn launch_codex(rig: &herdr::P11TerminationRig) -> (Self, PathBuf, PathBuf) {
        let opened = Self::open(&ProviderKind::Codex).await;
        let (harness, _, pool, session_key) = &opened;
        let sessions = root().join("codex-home/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let sessions = sessions.canonicalize().unwrap();
        let token_hash = harness.shared.token_hash.clone();
        let logical = session_key.rsplit_once(':').unwrap().1.to_owned();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: Utc::now(),
            execution_nonce: herdr::EXECUTION.into(),
            tmux_session: logical.clone(),
            channel_id: Some(CHANNEL_ID),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(sessions.clone()),
            first_prompt_digest: None,
            source_policy: None,
        };
        let prepared = PreparedIncarnation::create(context).unwrap();
        let key = (token_hash.as_str(), session_key.as_str());
        let pending = herdr::HostedState::Pending;
        let switches = rig
            .bind_as(pool, key, pending, "codex", &prepared.path)
            .await;
        dedupe::install_herdr_execution(&logical, herdr::EXECUTION);
        crate::services::discord::stamp_spawn_markers(&logical, Some(&prepared)).unwrap();
        dedupe::register_provider_session("codex", &logical, &logical);
        dedupe::register_codex_herdr_placeholder(&logical, CHANNEL_ID);
        let record = rig.record_as(key, herdr::HostedState::Bound, "codex");
        let path = prepared.path.clone();
        (Self::new(opened, record, None, switches), path, sessions)
    }

    /// A launch onto the execution's endpoint for the same owner.
    fn launch(&self, native: Option<String>, provider_root: Option<PathBuf>) -> HerdrLaunch {
        let location = &self.location;
        HerdrLaunch {
            endpoint: Some(HerdrLaunchEndpoint {
                execution_node: location.execution_node.clone(),
                config_key: location.endpoint_config_key.clone(),
                socket_addr: location.socket_addr.clone(),
                herdr_session: location.named_session.clone(),
            }),
            owner: self.owner.clone(),
            channel_id: Some(CHANNEL_ID),
            expected_native_session_id: native,
            resume: false,
            provider_root,
        }
    }

    /// The execution's own pane as its attach reads it, running `session`.
    fn own_pane(&self, session: &str) -> LaunchedPane {
        LaunchedPane {
            endpoint: HerdrEndpointId::of(&self.location),
            evidence: HerdrPaneEvidence {
                binding_nonce: Some(herdr::EXECUTION.into()),
                root: Some(self.expected.root.clone()),
                provider_process: Some(self.expected.provider_process.clone()),
                agent_session_id: Some(session.into()),
            },
        }
    }

    async fn terminate(&self) -> TerminationResult {
        let request = OperatorTerminate {
            session_key: self.session_key.clone(),
            execution_nonce: herdr::EXECUTION.into(),
        };
        let shared = self.harness.shared.clone();
        terminate_explicit_herdr(request, shared, &self.pool).await
    }

    /// The service settled the captured turn as a host termination and released nothing else.
    async fn assert_settled(&self) {
        let shared = &self.harness.shared;
        assert!(
            test_turn::active(shared, self.harness.channel_id)
                .await
                .is_none()
        );
        if let Some(token) = &self.token {
            assert!(!token.cancelled.load(Ordering::Relaxed));
        }
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(!herdr::held(), "the execution's hold is released");
    }

    async fn finish(self) {
        let Self {
            harness,
            database,
            pool,
            ..
        } = self;
        drop(harness);
        pool.close().await;
        database.drop().await;
    }
}

/// The gateway side of an O writer host over the real binding log; posts outlive restarts.
struct Gateway {
    posts: Arc<Posts>,
    alarms: Alarms,
}

impl HostIo for Gateway {
    type Port = Posts;
    type Lease = AnyLease;
    type Alarms = Alarms;
    type Bindings = ChannelBindingLog;

    fn port(&self) -> impl std::future::Future<Output = Arc<Posts>> + Send {
        std::future::ready(Arc::clone(&self.posts))
    }

    fn lease(&self) -> AnyLease {
        AnyLease
    }

    fn alarms(&self) -> Alarms {
        self.alarms.clone()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        Arc::new(ChannelBindingLog::new(channel, provider))
    }

    fn activation_facts(
        &self,
        _: u64,
        _: ShadowProvider,
    ) -> impl std::future::Future<
        Output = Result<crate::services::tui_o::writer::activation::ActivationFacts, String>,
    > + Send {
        std::future::ready(Ok(Default::default()))
    }

    fn local_custody(&self, _: u64, _: ShadowProvider) -> Result<Custody, String> {
        Ok(Custody::Free)
    }

    fn legacy(&self) -> Arc<dyn crate::services::tui_o::writer::adoption::LegacyView> {
        Arc::new(crate::services::tui_o::writer::adoption::NoLegacy)
    }

    fn legacy_busy(&self, _: u64) -> impl std::future::Future<Output = bool> + Send {
        std::future::ready(false)
    }

    fn relaying(&self, _: u64) -> bool {
        false
    }
}

/// The channel's O writer host as the gateway runs it, started and stopped whole.
struct OHost {
    gateway: Arc<Gateway>,
    gate: Arc<OwnershipGate>,
    readiness: Arc<Readiness>,
    running: Vec<host::ManagedWriterHandle>,
}

impl OHost {
    fn new() -> Self {
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let gateway = Arc::new(Gateway {
            posts: Arc::default(),
            alarms: Alarms::default(),
        });
        Self {
            gateway,
            gate,
            readiness: Arc::default(),
            running: Vec::new(),
        }
    }

    fn start(&mut self) {
        assert!(self.running.is_empty());
        self.readiness = Arc::default();
        let root = crate::config::runtime_root().expect("the harness root");
        let (gateway, gate) = (Arc::clone(&self.gateway), Arc::clone(&self.gate));
        let readiness = Arc::clone(&self.readiness);
        let parts = || HostParts {
            io: gateway,
            runtime_root: Some(root),
            gate,
            readiness,
        };
        let owned = crate::services::tui_o::cutover::boot_ownership();
        self.running = host::start_managed(ShadowProvider::Claude, true, owned, parts);
        assert_eq!(self.running.len(), 1, "the writer hosts the channel");
    }

    /// Stops the host, its actor and any admitted POST, and waits until all ended.
    async fn stop(&mut self) {
        for handle in self.running.drain(..) {
            handle
                .stop_and_join()
                .await
                .expect("the writer stopped cleanly");
        }
    }

    fn posts(&self) -> Vec<String> {
        self.gateway.posts.to(CHANNEL_ID)
    }

    /// The running actor's poll view, once the host started it.
    async fn view(&self) -> OwedView {
        let readiness = Arc::clone(&self.readiness);
        let started = wait_until(WAIT, || {
            let started = readiness.undelivered(CHANNEL_ID).is_some();
            Box::pin(async move { started })
        })
        .await;
        assert!(started, "hang guard: the host started no actor");
        self.readiness
            .undelivered(CHANNEL_ID)
            .expect("a running actor")
    }

    /// Polls of the running actor until `done` holds of its posts, at most `POLLS` of them.
    async fn until(&self, done: impl Fn(&[String]) -> bool) -> bool {
        let view = self.view().await;
        polls_until(&view, || done(&self.posts())).await
    }

    /// Polls until `expected` many posts arrived; then they are exactly `expected`.
    async fn posted(&self, expected: &[&str]) {
        let reached = self.until(|posts| posts.len() >= expected.len()).await;
        assert!(reached, "{:?} {:?}", self.posts(), self.halts());
        assert_eq!(self.posts(), expected, "{:?}", self.halts());
    }

    fn halts(&self) -> Vec<WriterAlarm> {
        let raised = self.gateway.alarms.0.lock().unwrap();
        let halted = raised.iter().filter(|(_, alarm)| {
            matches!(
                alarm,
                WriterAlarm::Halted { .. } | WriterAlarm::LedgerViolation { .. }
            )
        });
        halted.map(|(_, alarm)| alarm.clone()).collect()
    }
}

/// What the last of `n` polls published after this call asked, or `None` once the actor ended;
/// an unreadable store publishes `Some(None)`. The first poll may have begun before the ask.
async fn polled(view: &OwedView, n: usize) -> Option<Option<Undelivered>> {
    let mut published = view.published.clone();
    let wanting = view.demand.want();
    published.borrow_and_update();
    let mut last = None;
    let mut seen = 0;
    while seen < n {
        let changed = tokio::time::timeout(WAIT, published.changed()).await;
        changed
            .expect("hang guard: the actor published no poll")
            .ok()?;
        let poll = published.borrow_and_update().clone();
        if poll
            .as_ref()
            .is_none_or(|poll| poll.asked >= wanting.asked())
        {
            (last, seen) = (Some(poll), seen + 1);
        }
    }
    last
}

/// Polls of the actor behind `view` until `done` holds or it ends, at most `POLLS` of them.
async fn polls_until(view: &OwedView, done: impl Fn() -> bool) -> bool {
    for _ in 0..POLLS {
        if done() || polled(view, 1).await.is_none() {
            break;
        }
    }
    done()
}

fn root() -> PathBuf {
    crate::config::runtime_root().expect("the harness root")
}

/// Every file under `dir` with its bytes; a missing dir is empty.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(tree(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            files.insert(path, bytes);
        }
    }
    files
}

/// O's state as the writer recovers it: its store and the binding log it follows.
fn o_state(events: &Path) -> (BTreeMap<PathBuf, Vec<u8>>, BTreeMap<PathBuf, Vec<u8>>) {
    let store = tree(&root().join(STORE_DIR_NAME));
    assert!(!store.is_empty(), "the writer created its store");
    (
        store,
        tree(&events.join(binding_events::BINDING_EVENTS_DIR)),
    )
}

fn append(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn line(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

fn assistant(id: &str, text: &str) -> Vec<u8> {
    line(serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "message": {"id": id, "content": [{"type": "text", "text": text}]},
    }))
}

/// A signature-only row of `size` bytes that derives nothing to post.
fn thinking(id: &str, size: usize) -> Vec<u8> {
    line(serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "message": {"id": id, "content": [{"type": "thinking", "thinking": "", "signature": "z".repeat(size)}]},
    }))
}

/// Registers `transcript` as `logical`'s source the way a launch handoff does; it is logged.
fn bind_source(logical: &str, session: &str, transcript: &Path) {
    dedupe::register_tmux_channel(logical, CHANNEL_ID);
    let binding = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: transcript.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_owned()),
        last_offset: 0,
        relay_last_offset: None,
    };
    dedupe::register_launched_tmux_runtime_binding(logical, binding);
    let logged = binding_events::binding_events_since(CHANNEL_ID, 0).unwrap();
    assert!(!logged.is_empty(), "the source bind is logged");
}

/// How many times a piece with `payload` was posted, as the channel's ledger records it.
fn posted_in_ledger(payload: &str) -> usize {
    let store = OStore::existing(&root()).expect("the store");
    let era = store.read_era().unwrap().expect("the era");
    let channel = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    let ledger = channel.ledger();
    let pieces = (0..ledger.next_serial()).filter_map(|serial| ledger.piece(serial));
    let posted = pieces.filter(|piece| {
        piece.payload == payload
            && matches!(
                piece.outcome,
                Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
                    _
                ))
            )
    });
    posted.count()
}

/// The channel's store as the switch leaves it, with `source` bound from its empty start.
fn begin_era(source: &SourceId, build: &str) -> (OStore, crate::services::tui_o::store::OEra) {
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root())
        .unwrap()
        .unwrap();
    let source = InitSource {
        source_id: source.clone(),
        delivery_start: 0,
        prefix_hash: hex::encode(Sha256::digest(b"")),
    };
    let init = |channel| {
        Ok(Initialized {
            channel,
            sources: vec![source.clone()],
            initial_anchor: 100,
            build_digest: build.into(),
            at: Utc::now(),
        })
    };
    let era = store.begin_era(&[CHANNEL_ID], Utc::now(), init).unwrap();
    (store, era)
}

fn cursor_of(source: &SourceId) -> Option<u64> {
    let store = OStore::existing(&root())?;
    let era = store.read_era().ok()??;
    let channel = store.open_channel(&era, CHANNEL_ID).ok()??;
    channel.cursor(source).map(|cursor| cursor.captured_through)
}

/// A's rotation as the store keeps it: the successor B's bind left for A, if any.
fn successor_of(source: &SourceId) -> Option<Rotated> {
    let store = OStore::existing(&root())?;
    let era = store.read_era().ok()??;
    let channel = store.open_channel(&era, CHANNEL_ID).ok()??;
    let rotation = channel.rotation().ok()?;
    rotation.successors.get(&source_key(source)).cloned()
}

/// `source`'s spool segment files on disk with their bytes, read without opening the store.
fn segments_of(source: &SourceId) -> BTreeMap<PathBuf, Vec<u8>> {
    let prefix = format!("{}-", source_key(source));
    let mut files = tree(&root().join(STORE_DIR_NAME));
    files.retain(|path, _| {
        let name = path.file_name().and_then(|name| name.to_str());
        let spool = path.parent().and_then(Path::file_name);
        spool.is_some_and(|dir| dir == "spool")
            && name.is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".seg"))
    });
    files
}

/// T6 and T6b: A is terminated with its sealed tail unread, with or without a successor B bound
/// before the writer reads that tail.
async fn sealed_tail(rig: herdr::P11TerminationRig, events: PathBuf, successor: bool) {
    let run = Terminating::bind(&rig).await;
    let a_session = uuid::Uuid::new_v4().to_string();
    let a_path = root().join("a.jsonl");
    std::fs::write(&a_path, b"").unwrap();
    bind_source(&run.logical, &a_session, &a_path);
    let a_source = source_id_for(&a_session, &a_path).unwrap();
    // The channel reads as adopted, so its store already holds A from the switch.
    begin_era(&a_source, "m2t-t6");
    let mut o = OHost::new();
    o.start();
    append(&a_path, &assistant("m1", "answered before the termination"));
    o.posted(&["answered before the termination"]).await;

    // The tail lands while the writer is down, so the termination meets it unread.
    o.stop().await;
    let mut backlog = Vec::new();
    if successor {
        // More than one read: the writer must come back to A after B is bound.
        for index in 0..6 {
            backlog.extend(thinking(&format!("t{index}"), 200 * 1024));
        }
    }
    backlog.extend(assistant("m2", TAIL));
    append(&a_path, &backlog);
    assert!(std::fs::metadata(&a_path).unwrap().len() > MAX_READ_BYTES || !successor);
    let cursor = cursor_of(&a_source);
    let written = std::fs::metadata(&a_path).unwrap().len();
    assert!(
        cursor.is_some_and(|through| through < written),
        "unread tail: {cursor:?}"
    );
    let messages = run.harness.messages();
    let before = o_state(&events);
    assert_eq!(run.terminate().await, TerminationResult::Retired);
    assert_eq!(
        o_state(&events),
        before,
        "the service wrote none of O's state"
    );
    assert_eq!(
        run.harness.messages(),
        messages,
        "the service posted nothing of its own"
    );
    run.assert_settled().await;

    let mut expected = vec!["answered before the termination", TAIL];
    let (mut current, mut b) = (a_path.clone(), None);
    if successor {
        let (b_path, b_source) = launch_and_attach_successor(&rig, &run).await;
        let (store, log) = o_state(&events);
        assert_eq!(store, before.0, "B's bind leaves O's store as it was");
        assert_ne!(log, before.1, "B's bind is logged for the writer");
        assert_eq!(cursor_of(&a_source), cursor, "B's bind leaves A's cursor");
        assert_eq!(
            successor_of(&a_source),
            None,
            "no rotation before the writer applies it"
        );
        append(&b_path, &assistant("n1", "the successor's first answer"));
        expected.push("the successor's first answer");
        (current, b) = (b_path, Some(b_source));
    }

    o.start();
    let reached = o.until(|posts| posts.len() >= expected.len()).await;
    o.stop().await;
    if let Some(b_source) = &b {
        // The applied bind keeps A's backlog end as A's boundary, and A's cursor reached it.
        let next = successor_of(&a_source).expect("the writer rotated A to B");
        assert_eq!((&next.source, next.drain_to), (b_source, Some(written)));
        assert_eq!(next.tmux_session.as_deref(), Some(run.logical.as_str()));
        assert_eq!(
            cursor_of(&a_source),
            Some(written),
            "A drained to its boundary"
        );
    }
    assert!(reached, "{:?} {:?}", o.posts(), o.halts());
    assert_eq!(o.posts(), expected, "{:?}", o.halts());
    assert_eq!(posted_in_ledger(TAIL), 1, "the tail is delivered once");
    // The restart's positive control: its own new output posts once, and nothing else again.
    o.start();
    append(&current, &assistant("c1", RESTART_OUTPUT));
    expected.push(RESTART_OUTPUT);
    let reached = o
        .until(|posts| posts.iter().any(|p| p == RESTART_OUTPUT))
        .await;
    o.stop().await;
    let tails = o.posts().iter().filter(|post| *post == TAIL).count();
    assert_eq!(tails, 1, "the restart reposts no tail: {:?}", o.posts());
    assert!(reached, "the restart posts its new output: {:?}", o.halts());
    assert_eq!(o.posts(), expected, "a restart reposts nothing");
    assert_eq!(posted_in_ledger(TAIL), 1);
    assert_eq!(posted_in_ledger(RESTART_OUTPUT), 1);
    assert_eq!(o.halts(), Vec::<WriterAlarm>::new());
    assert_eq!(
        run.harness.provider_starts(),
        0,
        "no provider was faked or started"
    );
    assert_eq!(run.harness.messages(), messages);
    run.finish().await;
}

/// B's launch over the retired row and its attach, through the production launch and attach;
/// returns B's transcript, now the pane's logged source, and that source.
async fn launch_and_attach_successor(
    rig: &herdr::P11TerminationRig,
    run: &Terminating,
) -> (PathBuf, SourceId) {
    let b_session = uuid::Uuid::new_v4().to_string();
    let b_path = root().join("b.jsonl");
    std::fs::write(&b_path, b"").unwrap();
    let cwd = root().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let prepare = |prepared: &PreparedIncarnation| {
        let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: prepared.context.tmux_session.clone(),
            working_dir: cwd.clone(),
            claude_bin: crate::services::claude_command::ClaudeBinary::from_tmux_wrapper_argv(
                "/usr/local/bin/claude",
            ),
            agentdesk_exe: PathBuf::from("/usr/local/bin/agentdesk"),
            hook_endpoint: "http://127.0.0.1:1".into(),
            session_id: b_session.clone(),
            system_prompt: None,
            model: None,
            resume: false,
        };
        let files = crate::services::claude_tui::session::prepare_claude_tui_launch(&config)?;
        Ok(HerdrLaunchCommand {
            cwd: cwd.clone(),
            command: format!("bash {}", files.launch_script_path.display()),
        })
    };
    let launch = run.launch(Some(b_session.clone()), None);
    let host = Arc::new(Successor("claude"));
    let outcome = launch_herdr_session(&run.pool, launch, prepare, host).await;
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce,
        location,
        evidence: Ok(()),
    }) = outcome
    else {
        panic!("{outcome:?}");
    };
    let reader = LaunchedPane::successor(&location, &execution_nonce, "claude", &b_session);
    let attached = attach_launched_herdr_source(
        &run.pool,
        &run.owner,
        CHANNEL_ID,
        &execution_nonce,
        &b_session,
        &b_path,
        &reader,
    )
    .await;
    assert!(
        matches!(attached, HerdrSourceAttach::Published { bound: true, .. }),
        "{attached:?}"
    );
    let row = herdr::row(&run.pool, &run.session_key).await;
    assert_eq!(row, Some((herdr::HostedState::Bound, execution_nonce)));
    assert_eq!(
        rig.writes().len(),
        1,
        "B's launch went through its own host"
    );
    let b_source = source_id_for(&b_session, &b_path).unwrap();
    (b_path, b_source)
}

fn successor_stamps(nonce: &str, provider: &str) -> ExpectedExecution {
    let mut stamps = crate::db::dispatched_sessions::hosted_execution::tests::expected(nonce, 300);
    stamps.binding_provider = provider.into();
    stamps
}

/// B's launch host for `provider`: E7 off, one created pane and its evidence.
struct Successor(&'static str);

impl HerdrLaunchHost for Successor {
    fn mode(&self, _: &HerdrLaunchEndpoint) -> crate::config::session_hosts::EndpointMode {
        crate::config::session_hosts::EndpointMode::LegacyDedicated
    }

    fn restore_resume(&self, _: &HerdrLaunchEndpoint) -> RestoreResume {
        RestoreResume::Off {
            witness: ServerWitness::for_test(1),
        }
    }

    fn create(&self, _: &HerdrCreateRequest) -> HerdrCreateOutcome {
        HerdrCreateOutcome::Created {
            pane_id: herdr::LAUNCHED_PANE.into(),
        }
    }

    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        Ok(successor_stamps(&probe.execution_nonce, self.0))
    }
}

/// A launched pane as its attach reads it: running its execution's provider.
struct LaunchedPane {
    endpoint: HerdrEndpointId,
    evidence: HerdrPaneEvidence,
}

impl LaunchedPane {
    /// B's pane at `location`, as `Successor` stamped execution `nonce`, running `session`.
    fn successor(location: &HostedLocation, nonce: &str, provider: &str, session: &str) -> Self {
        let stamps = successor_stamps(nonce, provider);
        Self {
            endpoint: HerdrEndpointId::of(location),
            evidence: HerdrPaneEvidence {
                binding_nonce: Some(nonce.into()),
                root: Some(stamps.root),
                provider_process: Some(stamps.provider_process),
                agent_session_id: Some(session.into()),
            },
        }
    }
}

impl HerdrExecutionReader for LaunchedPane {
    fn endpoint(&self) -> Option<&HerdrEndpointId> {
        Some(&self.endpoint)
    }

    fn read_pane(&self, _: &str) -> HerdrPaneReading {
        HerdrPaneReading::Present(self.evidence.clone())
    }
}

/// A Codex channel's binding log as a list the test commits to.
struct LoggedBinds {
    events: Mutex<Vec<BindingEvent>>,
    notice: watch::Sender<u64>,
}

impl LoggedBinds {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::default(),
            notice: watch::channel(0).0,
        })
    }

    fn bind(&self, old: Option<&SourceId>, new: &SourceId, cause: BindingCause) {
        let mut events = self.events.lock().unwrap();
        let seq = events.len() as u64 + 1;
        let evidence = BindingEvidence {
            hook_event: "SessionStart".into(),
            received_at: Utc::now(),
            reclaims: false,
        };
        events.push(BindingEvent {
            seq,
            channel_id: CHANNEL_ID,
            provider: ShadowProvider::Codex,
            tmux_session: "AgentDesk-codex-p11-m2t".into(),
            execution_nonce: herdr::EXECUTION.into(),
            record: BindingRecord::Bound {
                old: old.cloned(),
                new: BindingTarget::Source(new.clone()),
                cause,
                parent_hint: old.cloned(),
                evidence,
            },
            committed_at: Utc::now(),
        });
        self.notice.send_replace(seq);
    }
}

impl BindingEvents for LoggedBinds {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        let events = self.events.lock().unwrap();
        let due = events
            .iter()
            .filter(|e| e.channel_id == channel && e.seq > after);
        Ok(due.cloned().collect())
    }

    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.notice.subscribe()
    }
}

fn codex_message(id: &str, text: &str) -> Vec<u8> {
    line(
        serde_json::json!({"type": "response_item", "timestamp": Utc::now().to_rfc3339(),
        "payload": {"type": "message", "role": "assistant", "id": id,
            "content": [{"type": "output_text", "text": text}]}}),
    )
}

/// A unit announced by `item_completed` before the record that seals it.
fn announced(id: &str) -> Vec<u8> {
    line(serde_json::json!({"type": "event_msg", "payload": {
        "type": "item_completed", "item": {"type": "AgentMessage", "id": id}}}))
}

/// A new Codex rollout of `session` under `sessions`, holding only its header.
fn rollout(sessions: &Path, session: &str) -> PathBuf {
    let path = sessions.join(format!("rollout-2026-10-11T07-00-00-{session}.jsonl"));
    let header = serde_json::json!({"timestamp": "2026-10-11T07:00:00.000Z",
        "type": "session_meta", "payload": {"id": session, "session_id": session,
            "cwd": sessions, "originator": "codex-tui", "cli_version": "0.160.0",
            "source": "cli"}});
    std::fs::write(&path, line(header)).unwrap();
    path
}

/// Codex's SessionStart of `session` as its hook relays it, under the launch context `context`.
fn session_start(logical: &str, context: &Path, session: &str, path: &Path) -> IngressOutcome {
    let envelope = HookBindingEnvelope::capture_from_env("codex", |name| {
        (name == "AGENTDESK_BINDING_CONTEXT").then(|| context.as_os_str().to_owned())
    });
    let mut headers = axum::http::HeaderMap::new();
    let encoded = envelope.encode().unwrap();
    headers.insert(BINDING_HEADER, encoded.parse().unwrap());
    let payload = serde_json::json!({"session_id": session, "source": "startup",
        "hook_event_name": "SessionStart", "transcript_path": path});
    let ingress = observe_binding_hook;
    ingress(
        "codex",
        "SessionStart",
        Some(logical),
        Some(session),
        &payload,
        &headers,
    )
}

/// The Codex writer's side: the gate it posts under, its posts and its raised alarms.
struct Out {
    gate: Arc<OwnershipGate>,
    posts: Arc<Posts>,
    alarms: Alarms,
}

impl Out {
    fn new() -> Self {
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        Self {
            gate,
            posts: Arc::default(),
            alarms: Alarms::default(),
        }
    }

    fn posts(&self) -> Vec<String> {
        self.posts.to(CHANNEL_ID)
    }

    fn raised(&self) -> Vec<WriterAlarm> {
        let raised = self.alarms.0.lock().unwrap();
        raised.iter().map(|(_, alarm)| alarm.clone()).collect()
    }

    /// The channel's Codex actor over `channel` following `bindings`, and its poll view.
    fn run<B: BindingEvents>(&self, channel: ChannelStore, bindings: Arc<B>) -> Running {
        let (gate, posts) = (Arc::clone(&self.gate), Arc::clone(&self.posts));
        let writer = ChannelWriter::new(channel, gate, posts, AnyLease, self.alarms.clone());
        let (stop, stopped) = watch::channel(false);
        let owing = Owing::default();
        let view = OwedView {
            published: owing.published.subscribe(),
            demand: owing.demand.clone(),
        };
        let watches = (
            stopped,
            watch::channel(false).0,
            watch::channel(None).0,
            owing,
        );
        let config = WriterConfig { enabled: true };
        let provider = ShadowProvider::Codex;
        let task = actor::spawn_projecting(&config, None, writer, provider, bindings, watches);
        let task = task.expect("an enabled writer");
        Running { stop, task, view }
    }
}

struct Running {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Option<StopCause>>,
    view: OwedView,
}

impl Running {
    async fn stop(self) -> Option<StopCause> {
        self.stop.send_replace(true);
        let ended = tokio::time::timeout(WAIT, self.task).await;
        ended
            .expect("hang guard: the writer did not stop")
            .expect("the writer task")
    }
}

/// T7's A: a settled message and a unit announced without its sealing record, spooled before
/// the writer starts; the tail written after them no longer fits the spool.
struct Unsealed {
    store: OStore,
    era: OEra,
    a: SourceId,
    /// A's spool segments as spooled before the writer started.
    segments: BTreeMap<PathBuf, Vec<u8>>,
    spooled: u64,
    settled: String,
    tail: String,
}

impl Unsealed {
    fn spool(a: &SourceId, build: &str) -> (Self, ChannelStore) {
        let (store, era) = begin_era(a, build);
        let settled = "x".repeat(1800);
        let body = [codex_message("msg_s", &settled), announced("msg_k")].concat();
        append(&a.path, &body);
        let mut channel = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
        let mut capture = SourceCapture::open(a.clone(), 0).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
            panic!("capture failed");
        };
        channel
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
        let spooled = capture.captured_through();
        let segments = segments_of(a);
        assert!(!segments.is_empty(), "A is spooled");
        channel.set_limits_for_test(1, channel.spool_bytes() + 1024);
        let tail = "y".repeat(1500);
        append(&a.path, &codex_message("msg_t", &tail));
        let a = a.clone();
        let unsealed = Self {
            store,
            era,
            a,
            segments,
            spooled,
            settled,
            tail,
        };
        (unsealed, channel)
    }

    /// The writer's first polls: the settled message posts and the tail is refused, while the
    /// announced unit keeps A's spool from GC.
    async fn refused(&self, out: &Out, running: &Running) {
        let settled = [self.settled.clone()];
        let refused = || out.posts() == settled && out.raised().contains(&WriterAlarm::SpoolFull);
        polls_until(&running.view, refused).await;
        let after = polled(&running.view, 2).await;
        let (posts, raised) = (out.posts(), out.raised());
        let kept = segments_of(&self.a) == self.segments;
        assert!(
            kept,
            "the unsealed unit keeps A's spool from GC: {posts:?} {raised:?}"
        );
        let after = after.expect("the writer runs on with the tail refused");
        let after = after.expect("the writer reads its store");
        assert_eq!(
            after.unsealed, 1,
            "A's announced unit is unsealed: {after:?}"
        );
        assert_eq!(after.uncaptured, 1, "A's tail is unread: {after:?}");
        assert_eq!(raised, [WriterAlarm::SpoolFull]);
        assert_eq!(posts, settled);
    }

    /// The termination of A's execution while its writer runs: A's spool, unit and alarms stay
    /// as O left them, and the log gains no bind.
    async fn survives(&self, run: &Terminating, out: &Out, running: &Running) {
        let logged = binding_events::binding_events_since(CHANNEL_ID, 0).unwrap();
        assert_eq!(run.terminate().await, TerminationResult::Retired);
        run.assert_settled().await;
        let after = polled(&running.view, 2).await;
        let kept = segments_of(&self.a) == self.segments;
        assert!(
            kept,
            "A's spool is as O left it: {:?}",
            segments_of(&self.a).keys()
        );
        let after = after.expect("the writer runs on past the termination");
        let after = after.expect("the writer reads its store");
        assert_eq!(after.unsealed, 1, "A's unit is still unsealed: {after:?}");
        assert_eq!(out.raised(), [WriterAlarm::SpoolFull]);
        assert_eq!(out.posts(), std::slice::from_ref(&self.settled));
        let now = binding_events::binding_events_since(CHANNEL_ID, 0).unwrap();
        assert_eq!(now, logged, "the service logs no bind");
    }

    /// B bound: the rotation stalls on A's refused tail and stops the writer before anything
    /// else posts; A keeps its cursor, ledger and spool, and B's boundary waits on A's end.
    async fn stalled(&self, out: &Out, running: Running, b: &SourceId) -> ChannelStore {
        // The actor's view closes when it ends; a running one keeps publishing its polls.
        let mut ended = false;
        for _ in 0..POLLS {
            if polled(&running.view, 1).await.is_none() {
                ended = true;
                break;
            }
        }
        let cause = running.stop().await;
        let stall = WriterAlarm::RotationStalled {
            source: self.a.clone(),
        };
        assert_eq!(out.raised(), [WriterAlarm::SpoolFull, stall], "{cause:?}");
        let settled = [self.settled.clone()];
        assert_eq!(out.posts(), settled, "nothing is posted past the stall");
        assert!(ended, "the stalled rotation stops the writer: {cause:?}");
        let channel = self
            .store
            .open_channel(&self.era, CHANNEL_ID)
            .unwrap()
            .unwrap();
        assert_eq!(
            channel.cursor(&self.a).unwrap().captured_through,
            self.spooled
        );
        let b_read = channel
            .cursor(b)
            .map_or(0, |cursor| cursor.captured_through);
        assert_eq!(b_read, 0);
        let ledger = channel.ledger();
        let pieces: Vec<_> = (0..ledger.next_serial())
            .filter_map(|serial| ledger.piece(serial))
            .map(|piece| piece.unit_key.native_key.clone())
            .collect();
        assert_eq!(
            pieces,
            ["msg_s"],
            "the announced unit was neither sealed nor posted"
        );
        assert!(
            channel.retained_segments(&self.a) > 0,
            "the unsealed unit keeps A's spool from GC"
        );
        assert!(
            segments_of(&self.a) == self.segments,
            "A's spool is as O left it"
        );
        let unit = |native: &str| crate::services::tui_o::shadow::UnitKey {
            channel_id: CHANNEL_ID,
            provider: ShadowProvider::Codex,
            native_key: native.into(),
            kind: crate::services::tui_o::shadow::UnitKind::Body,
        };
        assert_eq!(ledger.excluded(&unit("msg_k")), None, "nor excluded");
        let rotation = channel.rotation().unwrap();
        let next = rotation.successors.get(&source_key(&self.a));
        let a_end = std::fs::metadata(&self.a.path).unwrap().len();
        let boundary = next.map(|next| (&next.source, next.drain_to));
        assert_eq!(boundary, Some((b, Some(a_end))), "B succeeds A at A's end");
        channel
    }

    /// The operator's restart: B's own record seals the unit, posted once after A's tail; then a
    /// new output of B posts once, the restarted writer's positive control.
    async fn restarted<B>(&self, out: &Out, channel: ChannelStore, binds: Arc<B>, b_path: &Path)
    where
        B: BindingEvents,
    {
        let running = out.run(channel, binds);
        let answer = "the answer".to_string();
        let mut all = vec![self.settled.clone(), self.tail.clone(), answer];
        polls_until(&running.view, || out.posts().len() >= all.len()).await;
        assert_eq!(out.posts(), all, "{:?}", out.raised());
        append(b_path, &codex_message("msg_c", RESTART_OUTPUT));
        all.push(RESTART_OUTPUT.into());
        let posted = polls_until(&running.view, || out.posts().len() >= all.len()).await;
        let cause = running.stop().await;
        assert!(posted, "{:?} {:?} {cause:?}", out.posts(), out.raised());
        assert_eq!(out.posts(), all);
        assert_eq!(
            posted_in_ledger(&self.tail),
            1,
            "A's tail is delivered once"
        );
    }
}

/// T7: Codex execution A, attached to the rollout its own SessionStart logged, leaves an unsealed
/// unit and a refused tail; its own termination keeps them as O left them, and B's launch stalls.
async fn unsealed(rig: herdr::P11TerminationRig) {
    let (run, context, sessions) = Terminating::launch_codex(&rig).await;
    let a_session = uuid::Uuid::new_v4().to_string();
    let a_path = rollout(&sessions, &a_session);
    let a = own_source(&run, &context, &a_session, &a_path).await;
    let (unsealed, channel) = Unsealed::spool(&a, "m2t-t7");
    let out = Out::new();
    let bindings = Arc::new(ChannelBindingLog::new(CHANNEL_ID, ShadowProvider::Codex));
    let running = out.run(channel, Arc::clone(&bindings));
    unsealed.refused(&out, &running).await;
    unsealed.survives(&run, &out, &running).await;
    let row = herdr::row(&run.pool, &run.session_key).await;
    let retired = Some((herdr::HostedState::Retired, herdr::EXECUTION.to_string()));
    assert_eq!(row, retired, "the terminated execution is A's");
    // B's rollout seals A's announced unit, read only after A's refused tail.
    let b_body = [announced("msg_k"), codex_message("msg_k", "the answer")].concat();
    let (b, b_path) = launch_codex_successor(&run, &sessions, &a, &b_body).await;
    let channel = unsealed.stalled(&out, running, &b).await;
    unsealed.restarted(&out, channel, bindings, &b_path).await;
    run.finish().await;
}

/// A's own SessionStart through the hook ingress, then its attach: one logged source naming this
/// execution, provider and channel, which O's log reader binds as A.
async fn own_source(run: &Terminating, context: &Path, session: &str, path: &Path) -> SourceId {
    let started = session_start(&run.logical, context, session, path);
    assert_eq!(started, IngressOutcome::Durable(DurableKind::Adopted));
    let logged = binding_events::binding_events_since(CHANNEL_ID, 0).unwrap();
    let [event] = logged.as_slice() else {
        panic!("one logged bind: {logged:?}");
    };
    let binding_events::BindingTarget::Source(source) = &event.new else {
        panic!("a logged source: {event:?}");
    };
    let named = (event.provider.as_str(), event.channel_id);
    assert_eq!(
        named,
        (run.owner.provider.as_str(), CHANNEL_ID),
        "{event:?}"
    );
    assert_eq!(run.owner.channel_id, CHANNEL_ID.to_string());
    assert_eq!(event.execution_nonce.as_deref(), Some(herdr::EXECUTION));
    assert_eq!(event.tmux_session, run.logical);
    assert_eq!(
        (source.session_id.as_str(), source.path.as_path()),
        (session, path)
    );
    let reader = run.own_pane(session);
    let nonce = herdr::EXECUTION;
    let attach = attach_launched_codex_herdr_source;
    let attached = attach(&run.pool, &run.owner, CHANNEL_ID, nonce, source, &reader).await;
    assert!(
        matches!(attached, HerdrSourceAttach::Published { bound: true, .. }),
        "{attached:?}"
    );
    let row = herdr::row(&run.pool, &run.session_key).await;
    assert_eq!(row, Some((herdr::HostedState::Bound, nonce.to_string())));
    let read = ChannelBindingLog::new(CHANNEL_ID, ShadowProvider::Codex);
    let read = read.binding_events_since(CHANNEL_ID, 0).unwrap();
    let [read] = read.as_slice() else {
        panic!("O reads one bind: {read:?}");
    };
    assert_eq!(read.execution_nonce, nonce);
    let names = |new: &SourceId| new == source;
    assert!(
        matches!(&read.record, BindingRecord::Bound { new: BindingTarget::Source(new), .. } if names(new)),
        "{read:?}"
    );
    source.clone()
}

/// B's launch over A's retired row through the production launch, its own SessionStart and its
/// attach; the log names B after A for B's execution. Returns B's source and rollout.
async fn launch_codex_successor(
    run: &Terminating,
    sessions: &Path,
    a: &SourceId,
    body: &[u8],
) -> (SourceId, PathBuf) {
    let b_session = uuid::Uuid::new_v4().to_string();
    let b_path = rollout(sessions, &b_session);
    append(&b_path, body);
    let cwd = root().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let context = Mutex::new(None);
    let prepare = |prepared: &PreparedIncarnation| {
        *context.lock().unwrap() = Some(prepared.path.clone());
        let command = "codex".to_string();
        Ok(HerdrLaunchCommand { cwd, command })
    };
    let launch = run.launch(None, Some(sessions.to_path_buf()));
    let host = Arc::new(Successor("codex"));
    let outcome = launch_herdr_session(&run.pool, launch, prepare, host).await;
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce,
        location,
        evidence: Ok(()),
    }) = outcome
    else {
        panic!("{outcome:?}");
    };
    let context = context.lock().unwrap().clone().expect("B's launch context");
    let started = session_start(&run.logical, &context, &b_session, &b_path);
    assert_eq!(started, IngressOutcome::Durable(DurableKind::Adopted));
    let logged = binding_events::binding_events_since(CHANNEL_ID, 0).unwrap();
    let event = logged.last().expect("B's logged bind");
    let binding_events::BindingTarget::Source(source) = &event.new else {
        panic!("a logged source: {event:?}");
    };
    assert_eq!(
        event.execution_nonce.as_deref(),
        Some(execution_nonce.as_str())
    );
    assert_eq!(event.old.as_ref(), Some(a), "B succeeds A in the log");
    let reader = LaunchedPane::successor(&location, &execution_nonce, "codex", &b_session);
    let nonce = execution_nonce.as_str();
    let attach = attach_launched_codex_herdr_source;
    let attached = attach(&run.pool, &run.owner, CHANNEL_ID, nonce, source, &reader).await;
    assert!(
        matches!(attached, HerdrSourceAttach::Published { bound: true, .. }),
        "{attached:?}"
    );
    let row = herdr::row(&run.pool, &run.session_key).await;
    assert_eq!(
        row,
        Some((herdr::HostedState::Bound, execution_nonce.clone()))
    );
    (source.clone(), b_path)
}

/// T7u, the O actor's own regression: the rig's Claude execution and a listed log, unlinked to the
/// Codex rollout whose unsealed unit keeps its spool through the termination and the stall.
async fn unsealed_actor(rig: herdr::P11TerminationRig) {
    let run = Terminating::bind(&rig).await;
    let a_path = root().join("rollout-a.jsonl");
    std::fs::write(&a_path, b"").unwrap();
    let a = source_id_for("codex-a", &a_path).unwrap();
    let binds = LoggedBinds::new();
    binds.bind(None, &a, BindingCause::Startup);
    let (unsealed, channel) = Unsealed::spool(&a, "m2t-t7u");
    let out = Out::new();
    let running = out.run(channel, Arc::clone(&binds));
    unsealed.refused(&out, &running).await;
    unsealed.survives(&run, &out, &running).await;
    // The successor's bind: its rollout seals the announced unit only after A's refused tail.
    let b_path = a_path.with_file_name("rollout-b.jsonl");
    let b_body = [announced("msg_k"), codex_message("msg_k", "the answer")].concat();
    std::fs::write(&b_path, b_body).unwrap();
    let b = source_id_for("codex-b", &b_path).unwrap();
    binds.bind(Some(&a), &b, BindingCause::Resume);
    let channel = unsealed.stalled(&out, running, &b).await;
    unsealed.restarted(&out, channel, binds, &b_path).await;
    run.finish().await;
}

// X1 runs this writer harness against real Herdr servers; ignored outside its runner.
#[path = "herdr_terminate_o_real_tests.rs"]
mod real;

// A launched execution's source reaching O through the writer host's first activation.
#[path = "herdr_terminate_o_activation_tests.rs"]
mod activation;
