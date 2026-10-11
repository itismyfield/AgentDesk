//! A terminated Herdr execution's output stays O's: the real writer over its store, spool and
//! ledger posts the sealed tail once, a successor bind keeps that tail, and an announced unit stays
//! unsealed. The service writes none of O's state and posts nothing of its own.

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
    ExpectedExecution, HostedLocation, HostedOwner,
};
use crate::services::discord::herdr_terminate::{
    OperatorTerminate, TerminationResult, terminate_explicit_herdr, test_turn,
};
use crate::services::discord::recovery_engine::host_reconcile::{
    HerdrEndpointId, HerdrExecutionReader, HerdrPaneEvidence, HerdrPaneReading,
};
use crate::services::discord::tui_prompt_relay::herdr_source::{
    HerdrSourceAttach, attach_launched_herdr_source,
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
use crate::services::tui_o::store::{InitSource, Initialized, OStore, STORE_DIR_NAME, StoreConfig};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
    ChannelBindingLog,
};
use crate::services::tui_o::writer::deliver::ChannelWriter;
use crate::services::tui_o::writer::host::test_io::{Alarms, AnyLease, Posts};
use crate::services::tui_o::writer::host::{self, Custody, HostIo, HostParts, Readiness};
use crate::services::tui_o::writer::{WriterAlarm, WriterConfig, actor};
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding, binding_events};

const WAIT: Duration = Duration::from_secs(30);
const TAIL: &str = "the sealed tail written just before the termination";

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
fn m2t_t7_unsealed_survives_termination_and_rotation_stall_pg() {
    let path = concat!(
        module_path!(),
        "::m2t_t7_unsealed_survives_termination_and_rotation_stall_pg"
    );
    in_child(path, |rig, _| Box::pin(unsealed(rig)));
}

/// A Bound execution of the rig on the harness's bound channel, its turn running.
struct Terminating {
    harness: RelayE2eHarness,
    database: TestPostgresDb,
    pool: sqlx::PgPool,
    session_key: String,
    logical: String,
    owner: HostedOwner,
    location: HostedLocation,
    token: Arc<crate::services::provider::CancelToken>,
    _switches: herdr::LiveSwitches,
}

impl Terminating {
    async fn bind(rig: &herdr::P11TerminationRig) -> Self {
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
        let provider = ProviderKind::Claude;
        let session_key = crate::services::discord::adk_session::build_adk_session_key(
            &shared,
            harness.channel_id,
            &provider,
            None,
        )
        .await
        .expect("a namespaced session key");
        let bound = herdr::HostedState::Bound;
        let token_hash = shared.token_hash.clone();
        let switches = rig.bind(&pool, &token_hash, &session_key, bound).await;
        let record = rig.record(&token_hash, &session_key);
        let channel = harness.channel_id;
        let token = test_turn::start(&shared, channel, 51, "turn-a").await;
        Self {
            harness,
            database,
            pool,
            logical: record.owner.logical_key.clone(),
            owner: record.owner,
            location: record.location.expect("a recorded pane"),
            session_key,
            token,
            _switches: switches,
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
        assert!(!self.token.cancelled.load(Ordering::Relaxed));
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
            running: Vec::new(),
        }
    }

    fn start(&mut self) {
        assert!(self.running.is_empty());
        let root = crate::config::runtime_root().expect("the harness root");
        let (gateway, gate) = (Arc::clone(&self.gateway), Arc::clone(&self.gate));
        let parts = || HostParts {
            io: gateway,
            runtime_root: Some(root),
            gate,
            readiness: Arc::new(Readiness::default()),
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

    async fn posted(&self, expected: &[&str]) -> bool {
        let posts = Arc::clone(&self.gateway.posts);
        let expected: Vec<String> = expected.iter().map(|p| p.to_string()).collect();
        wait_until(WAIT, || {
            let done = posts.to(CHANNEL_ID).len() >= expected.len();
            Box::pin(async move { done })
        })
        .await
            && self.posts() == expected
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

/// A row that derives nothing to post, `size` bytes of thinking.
fn thinking(id: &str, size: usize) -> Vec<u8> {
    line(serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "message": {"id": id, "content": [{"type": "thinking", "thinking": "z".repeat(size)}]},
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
    assert!(
        o.posted(&["answered before the termination"]).await,
        "{:?} {:?}",
        o.posts(),
        o.halts()
    );

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
    if successor {
        let b_path = launch_and_attach_successor(&rig, &run).await;
        let (store, log) = o_state(&events);
        assert_eq!(store, before.0, "B's bind leaves O's store as it was");
        assert_ne!(log, before.1, "B's bind is logged for the writer");
        assert_eq!(cursor_of(&a_source), cursor, "B's bind leaves A's cursor");
        append(&b_path, &assistant("n1", "the successor's first answer"));
        expected.push("the successor's first answer");
    }

    o.start();
    assert!(o.posted(&expected).await, "{:?} {:?}", o.posts(), o.halts());
    o.stop().await;
    assert_eq!(posted_in_ledger(TAIL), 1, "the tail is delivered once");
    // A restart takes the store back and posts nothing again.
    o.start();
    tokio::time::sleep(actor::POLL_INTERVAL * 4).await;
    o.stop().await;
    assert_eq!(o.posts(), expected, "a restart reposts nothing");
    assert_eq!(posted_in_ledger(TAIL), 1);
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
/// returns B's transcript, now the pane's logged source.
async fn launch_and_attach_successor(rig: &herdr::P11TerminationRig, run: &Terminating) -> PathBuf {
    let b_session = uuid::Uuid::new_v4().to_string();
    let b_path = root().join("b.jsonl");
    std::fs::write(&b_path, b"").unwrap();
    let cwd = root().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let prepare =
        |prepared: &crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation| {
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
    let location = &run.location;
    let launch = HerdrLaunch {
        endpoint: Some(HerdrLaunchEndpoint {
            execution_node: location.execution_node.clone(),
            config_key: location.endpoint_config_key.clone(),
            socket_addr: location.socket_addr.clone(),
            herdr_session: location.named_session.clone(),
        }),
        owner: run.owner.clone(),
        channel_id: Some(CHANNEL_ID),
        expected_native_session_id: Some(b_session.clone()),
        resume: false,
        provider_root: None,
    };
    let outcome = launch_herdr_session(&run.pool, launch, prepare, Arc::new(Successor)).await;
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce,
        location,
        evidence: Ok(()),
    }) = outcome
    else {
        panic!("{outcome:?}");
    };
    let reader = SuccessorPane {
        endpoint: HerdrEndpointId::of(&location),
        evidence: HerdrPaneEvidence {
            binding_nonce: Some(execution_nonce.clone()),
            root: Some(successor_stamps(&execution_nonce).root),
            provider_process: Some(successor_stamps(&execution_nonce).provider_process),
            agent_session_id: Some(b_session.clone()),
        },
    };
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
    b_path
}

fn successor_stamps(nonce: &str) -> ExpectedExecution {
    crate::db::dispatched_sessions::hosted_execution::tests::expected(nonce, 300)
}

/// B's launch host: E7 off, one created pane and its evidence.
struct Successor;

impl HerdrLaunchHost for Successor {
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
        Ok(successor_stamps(&probe.execution_nonce))
    }
}

/// B's pane as its attach reads it: running B's provider.
struct SuccessorPane {
    endpoint: HerdrEndpointId,
    evidence: HerdrPaneEvidence,
}

impl HerdrExecutionReader for SuccessorPane {
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

/// T7: A, whose announced unit has no sealing record, is terminated while its full spool refuses
/// the tail; B's bind then stalls the rotation. The terminated execution stays as O left it.
///
/// The rig's Herdr fixture runs a Claude provider, so the terminated execution is Claude's while
/// the writer reads a Codex rollout: only a Codex rollout announces a unit before sealing it.
async fn unsealed(rig: herdr::P11TerminationRig) {
    let run = Terminating::bind(&rig).await;
    let a_path = root().join("rollout-a.jsonl");
    std::fs::write(&a_path, b"").unwrap();
    let a = source_id_for("codex-a", &a_path).unwrap();
    let (store, era) = begin_era(&a, "m2t-t7");
    let binds = LoggedBinds::new();
    binds.bind(None, &a, BindingCause::Startup);
    let settled = "x".repeat(1800);
    append(
        &a_path,
        &[codex_message("msg_s", &settled), announced("msg_k")].concat(),
    );
    let mut channel = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    let mut capture = SourceCapture::open(a.clone(), 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture failed");
    };
    channel
        .append_spool(&batch, &capture.prefix_hash())
        .unwrap();
    let spooled = capture.captured_through();
    channel.set_limits_for_test(1, channel.spool_bytes() + 1024);
    let tail = "y".repeat(1500);
    append(&a_path, &codex_message("msg_t", &tail));

    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let (posts, alarms) = (Arc::new(Posts::default()), Alarms::default());
    let start = |channel| {
        let writer = ChannelWriter::new(
            channel,
            Arc::clone(&gate),
            Arc::clone(&posts),
            AnyLease,
            alarms.clone(),
        );
        let (stop, stopped) = watch::channel(false);
        let config = WriterConfig { enabled: true };
        let resumed = watch::channel(false).0;
        let provider = ShadowProvider::Codex;
        let task =
            actor::spawn_if_enabled(&config, writer, provider, binds.clone(), stopped, resumed);
        (stop, task.expect("an enabled writer"))
    };
    let raised = |alarms: &Alarms| -> Vec<WriterAlarm> {
        alarms
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, alarm)| alarm.clone())
            .collect()
    };
    let (_stop, task) = start(channel);
    let refused = wait_until(WAIT, || {
        let ready = posts.to(CHANNEL_ID) == [settled.clone()]
            && raised(&alarms).contains(&WriterAlarm::SpoolFull);
        Box::pin(async move { ready })
    })
    .await;
    assert!(refused, "{:?} {:?}", posts.to(CHANNEL_ID), raised(&alarms));

    assert_eq!(run.terminate().await, TerminationResult::Retired);
    run.assert_settled().await;
    // The successor's bind: its rollout seals the announced unit only after A's refused tail.
    let b_path = a_path.with_file_name("rollout-b.jsonl");
    let b_body = [announced("msg_k"), codex_message("msg_k", "the answer")].concat();
    std::fs::write(&b_path, b_body).unwrap();
    let b = source_id_for("codex-b", &b_path).unwrap();
    binds.bind(Some(&a), &b, BindingCause::Resume);
    let stalled = tokio::time::timeout(WAIT, task).await;
    assert!(stalled.is_ok(), "a stalled rotation stops the channel");
    let stall = WriterAlarm::RotationStalled { source: a.clone() };
    assert_eq!(raised(&alarms), [WriterAlarm::SpoolFull, stall]);
    assert_eq!(
        posts.to(CHANNEL_ID),
        [settled.clone()],
        "nothing is posted past the stall"
    );
    let channel = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    assert_eq!(channel.cursor(&a).unwrap().captured_through, spooled);
    assert_eq!(
        channel
            .cursor(&b)
            .map_or(0, |cursor| cursor.captured_through),
        0
    );
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
        channel.retained_segments(&a) > 0,
        "the unsealed unit keeps A's spool from GC"
    );
    let unit = |native: &str| crate::services::tui_o::shadow::UnitKey {
        channel_id: CHANNEL_ID,
        provider: ShadowProvider::Codex,
        native_key: native.into(),
        kind: crate::services::tui_o::shadow::UnitKind::Body,
    };
    assert_eq!(ledger.excluded(&unit("msg_k")), None, "nor excluded");

    // The operator's restart: B's own record seals the unit, which is posted once, after A's tail.
    let (stop, task) = start(channel);
    let all = [settled.clone(), tail.clone(), "the answer".to_string()];
    let posted = wait_until(WAIT, || {
        let done = posts.to(CHANNEL_ID).len() >= all.len();
        Box::pin(async move { done })
    })
    .await;
    assert!(posted, "{:?} {:?}", posts.to(CHANNEL_ID), raised(&alarms));
    tokio::time::sleep(actor::POLL_INTERVAL * 2).await;
    assert_eq!(posts.to(CHANNEL_ID), all);
    stop.send_replace(true);
    let _ = task.await;
    run.finish().await;
}

// X1 runs this writer harness against real Herdr servers; ignored outside its runner.
#[path = "herdr_terminate_o_real_tests.rs"]
mod real;
