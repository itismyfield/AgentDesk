//! Late adoption over the production boot seed, delegated PG worker and body gate.
//! The external TUI executor is recorded; all intake transitions and body claims stay real.

use super::*;
use crate::config::Config;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{InsertPendingPayload, insert_pending};
use crate::db::o_channel_activation::{activation_rows, fenced_activation_rows};
use crate::db::o_channel_homes::{
    ChannelHome, HeldHome, HomeError, HomeState, HomeWrite, adopt, delegate, finish_release,
};
use crate::services::cluster::channel_home::{self, HomeGate};
use crate::services::cluster::channel_home_boot::delegated_boot_ownership;
use crate::services::cluster::intake_worker::{TickOutcome, run_intake_worker_tick, test_executor};
use crate::services::tui_o::channel_policy::{self, Candidate};
use crate::services::tui_o::cutover::{
    BodyClaim, BodySend, claim_then_send, intake_route, peek_o_owns_tui_output_for_channel,
};
use crate::services::tui_o::writer::host::{FencedFacts, ManagedWriterHandle, start_managed};
use sqlx::PgPool;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::oneshot;

pub(super) const GATEWAY: &str = "late-adopt-gateway";
pub(super) const WORKER: &str = "late-adopt-worker";

pub(super) fn config(foreign: bool) -> Config {
    serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": {
            "enabled": true,
            "instance_id": if foreign { WORKER } else { GATEWAY },
            "gateway_preferred_instance_id": GATEWAY
        },
        "tui_o": {"writer": {"channels": [CHANNEL], "late_adopt": true}},
        "agents": [{"id": "late", "name": "Late", "channels": {
            "claude": {"id": CHANNEL.to_string(), "runtime": "tui"}
        }}]
    }))
    .unwrap()
}

/// Never installs a state by hand: seed the actual temporary store through production code.
pub(super) fn seed(harness: &Harness, foreign: bool) -> test_override::ChannelsGuard {
    let config = config(foreign);
    let snapshot = BootChannels::validate(&config)
        .unwrap()
        .seeded(true, &config, |channels, _| {
            Ok(channel_policy::stored(
                Some(harness._runtime.path()),
                channels,
            ))
        })
        .unwrap();
    crate::config_live_reload::install(config);
    test_override::force_boot(snapshot)
}

fn applied(write: Result<HomeWrite<ChannelHome>, HomeError>) -> ChannelHome {
    match write.unwrap() {
        HomeWrite::Applied(home) => home,
        HomeWrite::Stale => panic!("test home transition was stale"),
    }
}

pub(super) async fn home(pool: &PgPool) -> (Arc<HomeGate>, i64) {
    let channel = CHANNEL.to_string();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('late', 'Late', 'claude', $1)",
    )
    .bind(&channel)
    .execute(pool)
    .await
    .unwrap();
    let row = applied(delegate(pool, &channel, "claude", GATEWAY, WORKER).await);
    let row = applied(finish_release(pool, &channel, GATEWAY, row.epoch).await);
    let epoch = applied(adopt(pool, &channel, WORKER, row.epoch).await).epoch;
    let gate = Arc::new(HomeGate::new(&channel, WORKER));
    channel_home::register(Arc::clone(&gate));
    renew(&gate, epoch);
    (gate, epoch)
}

pub(super) fn renew(gate: &HomeGate, epoch: i64) {
    let held = HeldHome::for_test(&CHANNEL.to_string(), WORKER, epoch, HomeState::Worker);
    gate.confirm(&held, tokio::time::Instant::now()).unwrap();
}

pub(super) async fn pending(pool: &PgPool, epoch: i64) -> i64 {
    insert_pending(
        pool,
        &InsertPendingPayload {
            target_instance_id: WORKER.into(),
            forwarded_by_instance_id: GATEWAY.into(),
            required_labels: serde_json::json!([]),
            execution_requirements: serde_json::json!({}),
            attachment_refs: serde_json::json!([]),
            channel_id: CHANNEL.to_string(),
            user_msg_id: "6325".into(),
            request_owner_id: "100".into(),
            request_owner_name: None,
            user_text: "first late-adopt input".into(),
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: None,
            turn_kind: "foreground".into(),
            merge_consecutive: false,
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            preserve_on_cancel: false,
            agent_id: "late".into(),
            provider: "claude".into(),
            home_epoch: Some(epoch),
        },
        1,
        None,
    )
    .await
    .unwrap()
}

pub(super) struct Legacy {
    pub(super) path: PathBuf,
    pub(super) frontier: AtomicU64,
}

impl LegacyView for Legacy {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, _: &str) -> LegacyCursor {
        LegacyCursor::Bound {
            path: self.path.clone(),
            offset: std::fs::metadata(&self.path).unwrap().len(),
        }
    }

    fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
        Ok(self.cursor(tmux))
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        Some(self.frontier.load(Ordering::SeqCst))
    }

    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

struct FactsPause {
    entered: oneshot::Sender<i64>,
    release: oneshot::Receiver<()>,
}

pub(super) struct PgIo {
    pub(super) base: Arc<TestIo>,
    pub(super) pool: PgPool,
    pause: Mutex<Option<FactsPause>>,
    pub(super) facts: Mutex<Vec<i64>>,
    pub(super) fences: AtomicUsize,
    pub(super) enabled: AtomicBool,
}

impl HostIo for PgIo {
    type Port = FakePort;
    type Lease = Arc<FakeLease>;
    type Alarms = Raised;
    type Bindings = ChannelBindingLog;

    fn late_adopt_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn port(&self) -> impl Future<Output = Arc<FakePort>> + Send {
        self.base.port()
    }

    fn lease(&self) -> Self::Lease {
        self.base.lease()
    }

    fn alarms(&self) -> Raised {
        self.base.alarms()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        self.base.bindings(channel, provider)
    }

    async fn activation_facts(
        &self,
        channel: u64,
        _: ShadowProvider,
    ) -> Result<ActivationFacts, String> {
        let rows = activation_rows(&self.pool, &channel.to_string(), WORKER)
            .await
            .map_err(|error| error.to_string())?;
        self.facts.lock().unwrap().push(rows.open_intake);
        let pause = self.pause.lock().unwrap().take();
        if let Some(pause) = pause {
            pause.entered.send(rows.open_intake).unwrap();
            pause.release.await.unwrap();
        }
        Ok(ActivationFacts {
            open_intake: rows.open_intake,
            runner_sessions: rows.foreign_sessions,
            node_override: None,
        })
    }

    async fn intake_fence(&self, channel: u64, _: ShadowProvider) -> Result<FencedFacts, String> {
        let (hold, rows, queued_bodies) =
            fenced_activation_rows(&self.pool, &channel.to_string(), WORKER)
                .await
                .map_err(|error| error.to_string())?;
        self.facts.lock().unwrap().push(rows.open_intake);
        self.fences.fetch_add(1, Ordering::SeqCst);
        Ok(FencedFacts {
            hold,
            facts: ActivationFacts {
                open_intake: rows.open_intake,
                runner_sessions: rows.foreign_sessions,
                node_override: None,
            },
            queued_bodies,
        })
    }

    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.base.local_custody(channel, provider)
    }

    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.base.legacy()
    }

    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.base.legacy_busy(channel)
    }

    fn relaying(&self, channel: u64) -> bool {
        self.base.relaying(channel)
    }

    fn adopted(&self, channel: u64, provider: ShadowProvider) {
        self.base.adopted(channel, provider);
    }
}

pub(super) fn io(
    harness: &Harness,
    pool: &PgPool,
) -> (Arc<PgIo>, oneshot::Receiver<i64>, oneshot::Sender<()>) {
    let (entered, observed) = oneshot::channel();
    let (release, released) = oneshot::channel();
    (
        Arc::new(PgIo {
            base: TestIo::over(harness),
            pool: pool.clone(),
            pause: Mutex::new(Some(FactsPause {
                entered,
                release: released,
            })),
            facts: Mutex::default(),
            fences: AtomicUsize::default(),
            enabled: AtomicBool::new(true),
        }),
        observed,
        release,
    )
}

pub(super) fn start_delegated(
    harness: &Harness,
    io: &Arc<PgIo>,
) -> (ManagedWriterHandle, Candidate, Arc<Readiness>) {
    p5::set_test_root(Some(harness._runtime.path()));
    let owned = delegated_boot_ownership(|channel| channel == CHANNEL);
    assert_eq!(
        owned.len(),
        1,
        "production delegated resolver finds the boot candidate"
    );
    let candidate = owned[0].2.clone().unwrap();
    let ready = Arc::new(Readiness::default());
    let mut handles = start_managed(ShadowProvider::Claude, false, owned, || HostParts {
        io: Arc::clone(io),
        runtime_root: Some(root(harness)),
        gate: Arc::clone(&harness.gate),
        readiness: Arc::clone(&ready),
    });
    assert_eq!(handles.len(), 1);
    (handles.pop().unwrap(), candidate, ready)
}

pub(super) async fn wait(gate: &HomeGate, epoch: i64, why: &str, done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "{why}");
        renew(gate, epoch);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(super) fn closed() -> Vec<u8> {
    let row = serde_json::json!({"type":"system", "subtype":"turn_duration", "durationMs":5});
    format!("{row}\n").into_bytes()
}

pub(super) fn transcript(harness: &Harness, path: &Path) -> Arc<Legacy> {
    append(path, &[row("late-old", "old"), closed()].concat());
    let source = source_id_for("s1", path).unwrap();
    p5_log(harness._runtime.path(), CHANNEL, &startup(source));
    Arc::new(Legacy {
        path: path.to_path_buf(),
        frontier: AtomicU64::default(),
    })
}

pub(super) async fn assert_committed(
    harness: &Harness,
    path: &Path,
    io: &PgIo,
    gate: &HomeGate,
    epoch: i64,
    candidate: &Candidate,
    ready: &Readiness,
) {
    wait(gate, epoch, "same host committed and became ready", || {
        candidate.peek() == Adoption::Committed && ready.accepts(CHANNEL)
    })
    .await;
    let init = harness.store.read_init(CHANNEL).unwrap().unwrap();
    assert_eq!(init.sources.len(), 1);
    assert_eq!(
        init.sources[0].delivery_start,
        std::fs::metadata(path).unwrap().len()
    );
    assert!(
        io.fences.load(Ordering::SeqCst) > 0,
        "actual PG fence reached"
    );
    assert_eq!(
        harness.port.posts(),
        Vec::<String>::new(),
        "O never reposts old"
    );
    append(path, &row("late-new", "new"));
    wait(gate, epoch, "O delivered unique new once", || {
        harness.port.posts() == ["new"]
    })
    .await;
    let calls = AtomicUsize::new(0);
    let answer = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
        calls.fetch_add(1, Ordering::SeqCst);
    })
    .await;
    assert!(matches!(answer, Ok(BodySend::OwnedByO)));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "committed body executes no Legacy transport"
    );
    assert_eq!(harness.port.posts(), ["new"], "combined new count is one");
}

#[tokio::test(flavor = "current_thread")]
async fn late_adopt_delegated_first_pg_worker_executes_then_same_host_commits() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_delegated_first_pg_worker_executes_then_same_host_commits"
    )) {
        return;
    }
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let (harness, path) = fresh(|_| Vec::new());
    let _boot = seed(&harness, true);
    let (gate, epoch) = home(&pool).await;
    let row_id = pending(&pool, epoch).await;
    let (io, entered, release) = io(&harness, &pool);
    let (handle, candidate, ready) = start_delegated(&harness, &io);
    let generation = handle.generation();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .unwrap()
            .unwrap(),
        1
    );
    let (_registry, shared) =
        crate::services::discord::health::owner_runtime_for_tests::registered("claude").await;
    let recorder = test_executor::record();
    let outcome = run_intake_worker_tick(&pool, &shared, WORKER, "claude", "late", &|| false)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        TickOutcome::Processed,
        "P1-D1: actual delegated first input runs"
    );
    assert_eq!(
        recorder.channels(),
        [CHANNEL],
        "exactly one actual executor admission"
    );
    let status: String = sqlx::query_scalar("SELECT status::TEXT FROM intake_outbox WHERE id = $1")
        .bind(row_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        status, "done",
        "real PG transitions settled the first input"
    );
    assert_eq!(
        run_intake_worker_tick(&pool, &shared, WORKER, "claude", "late", &|| false)
            .await
            .unwrap(),
        TickOutcome::QueueEmpty
    );
    assert_eq!(recorder.channels(), [CHANNEL]);
    assert_eq!(
        intake_route::route_for_placement("claude", CHANNEL),
        intake_route::IntakeRoute::Unselected
    );
    let legacy = transcript(&harness, &path);
    *io.base.legacy.lock().unwrap() = Some(Arc::clone(&legacy) as Arc<dyn LegacyView>);
    let old_calls = AtomicUsize::new(0);
    let old = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
        old_calls.fetch_add(1, Ordering::SeqCst);
        "old"
    })
    .await
    .unwrap();
    assert_eq!(
        old,
        BodySend::Sent("old"),
        "Legacy old payload was actually sent once"
    );
    assert_eq!(old_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        candidate.sends(),
        (1, 1),
        "P1-D2: host counts the same body candidate"
    );
    legacy
        .frontier
        .store(std::fs::metadata(&path).unwrap().len(), Ordering::SeqCst);
    release.send(()).unwrap();
    assert_committed(&harness, &path, &io, &gate, epoch, &candidate, &ready).await;
    assert!(io.facts.lock().unwrap().contains(&1));
    assert!(
        io.facts.lock().unwrap().contains(&0),
        "open intake 1 becomes 0 on fresh query"
    );
    let stopped = handle.stop_and_join().await.unwrap();
    assert_eq!(
        stopped.generation, generation,
        "no restart produced the commit"
    );
    channel_home::unregister(&CHANNEL.to_string());
    pool.close().await;
    fixture.drop().await;
}

async fn body_candidate(foreign: bool) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let (harness, path) = fresh(|_| Vec::new());
    let _boot = seed(&harness, foreign);
    let (gate, epoch) = home(&pool).await;
    let (io, entered, release_facts) = io(&harness, &pool);
    let (handle, candidate, ready) = start_delegated(&harness, &io);
    let generation = handle.generation();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let legacy = transcript(&harness, &path);
    *io.base.legacy.lock().unwrap() = Some(Arc::clone(&legacy) as Arc<dyn LegacyView>);
    let (entered, at_transport) = oneshot::channel();
    let (release_body, body_released) = oneshot::channel();
    let old = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&old);
    let body = claim_then_send(
        Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
        || async move {
            entered.send(()).unwrap();
            body_released.await.unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
        },
    );
    tokio::pin!(body);
    tokio::select! {
        entered = at_transport => entered.unwrap(),
        answer = &mut body => panic!("Legacy transport must remain in flight: {answer:?}"),
        () = tokio::time::sleep(Duration::from_secs(5)) => panic!("Legacy transport never entered"),
    }
    let sends = {
        let _locked = candidate.lock();
        candidate.sends()
    };
    assert_eq!(
        sends,
        (1, 0),
        "P1-D2: transport and host share the exact sends Arc"
    );
    assert_ne!(candidate.peek(), Adoption::Committed);
    assert!(!harness.store.has_channel_dir(CHANNEL));
    assert_eq!(
        peek_o_owns_tui_output_for_channel(CHANNEL, Some(ClaudeTui)),
        Ok(false)
    );
    release_facts.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_ne!(
        candidate.peek(),
        Adoption::Committed,
        "no commit while body transport is in flight"
    );
    release_body.send(()).unwrap();
    assert!(matches!(body.await, Ok(BodySend::Sent(()))));
    assert_eq!(old.load(Ordering::SeqCst), 1);
    assert_eq!(candidate.sends(), (1, 1));
    legacy
        .frontier
        .store(std::fs::metadata(&path).unwrap().len(), Ordering::SeqCst);
    assert_committed(&harness, &path, &io, &gate, epoch, &candidate, &ready).await;
    for closed in [false, true] {
        if closed {
            gate.close();
        } else {
            gate.close_intake();
        }
        assert!(matches!(
            intake_route::route_for_placement("claude", CHANNEL),
            intake_route::IntakeRoute::Hold(_)
        ));
        assert_eq!(
            peek_o_owns_tui_output_for_channel(CHANNEL, Some(ClaudeTui)),
            Ok(true)
        );
        let calls = AtomicUsize::new(0);
        let answer = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
            calls.fetch_add(1, Ordering::SeqCst);
        })
        .await;
        assert!(
            matches!(answer, Ok(BodySend::OwnedByO)),
            "Closed/Lost keeps Committed output O-owned"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    assert_eq!(old.load(Ordering::SeqCst), 1);
    assert_eq!(harness.port.posts(), ["new"]);
    assert_eq!(handle.stop_and_join().await.unwrap().generation, generation);
    channel_home::unregister(&CHANNEL.to_string());
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn late_adopt_foreign_body_gate_and_host_share_candidate() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_foreign_body_gate_and_host_share_candidate"
    )) {
        body_candidate(true).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn late_adopt_home_body_before_first_decision_preserves_reservation() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_home_body_before_first_decision_preserves_reservation"
    )) {
        body_candidate(false).await;
    }
}
