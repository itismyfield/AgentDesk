//! Shared receipts read before a channel's actor starts: a receipted rejection neither stops the
//! writer nor sends its approval again, and the local ledger and PostgreSQL rows stay as they were.

use sqlx::PgPool;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::repost::config::{RepostConfig, RepostSwitch};
use crate::services::tui_o::repost::identity::{
    IDENTITY_VERSION, PieceKey, SPLIT_VERSION, payload_sha256,
};
use crate::services::tui_o::repost::io::RepostHttp;
use crate::services::tui_o::repost::io::probe::evidence::RunScope;
use crate::services::tui_o::repost::o_piece_attempts::AttemptResult;
use crate::services::tui_o::repost::o_piece_delivery::{
    self, AdmitOutcome, AdmittedBy, NewDelivery, Receipt, ReceiptMethod, ReceiptOutcome,
};
use crate::services::tui_o::repost::projection::ReceiptOverlay;
use crate::services::tui_o::repost::runner::{ChannelTurn, Runner};
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_o::store::ledger::PieceDisposition;
use crate::services::tui_o::writer::host::FencedFacts;

const PAYLOAD: &str = "receipted payload";
const BLOCKED: WriterAlarm = WriterAlarm::Blocked { status: 403 };

/// How another home left a piece in PostgreSQL.
#[derive(Clone, Copy)]
struct Shared {
    payload: &'static str,
    identity_version: i32,
    receipt: bool,
}

const RECEIPTED: Shared = Shared {
    payload: PAYLOAD,
    identity_version: IDENTITY_VERSION,
    receipt: true,
};

/// Another home's admission of `native` and, when asked, its recorded receipt.
async fn shared(pool: &PgPool, native: &str, shared: Shared, message_id: u64) -> PieceKey {
    let key = PieceKey::new(unit(native), 0).unwrap();
    let new = NewDelivery {
        key: key.clone(),
        payload: shared.payload.into(),
        payload_sha256: payload_sha256(shared.payload),
        identity_version: shared.identity_version,
        split_version: SPLIT_VERSION,
        original_anchor: 40,
        sender_id: BOT,
        admitted_by: AdmittedBy::Uncertain,
        origin_serial: 7,
        origin_node: "node-b".into(),
        original: AttemptResult::Uncertain,
        prior_retries: 0,
        failure: None,
    };
    let admitted = o_piece_delivery::admit(pool, &new).await.unwrap();
    assert!(matches!(admitted, AdmitOutcome::Admitted(_)));
    if shared.receipt {
        let receipt = Receipt {
            key: key.clone(),
            message_id,
            author_id: BOT,
            slot: Some(1),
            method: ReceiptMethod::Marker,
        };
        let recorded = o_piece_delivery::record_receipt(pool, &receipt).await;
        assert!(matches!(recorded.unwrap(), ReceiptOutcome::Recorded { .. }));
    }
    key
}

/// `native` prepared at `serial` with [`PAYLOAD`] and refused 403, optionally approved again.
fn seed_rejection(harness: &Harness, native: &str, serial: u64, approve: bool) {
    let mut channel = harness.channel();
    let anchor_id = channel.ledger().anchor();
    let prepared = LedgerEntry::Prepared {
        serial,
        unit_key: unit(native),
        piece_index: 0,
        payload: PAYLOAD.into(),
        anchor_id,
        epoch: 1,
    };
    channel.append_ledger(prepared).unwrap();
    let refused = LedgerEntry::Rejected {
        serial,
        status: 403,
    };
    channel.append_ledger(refused).unwrap();
    drop(channel);
    if approve {
        let resume = harness
            .store
            .record_operator_resume(CHANNEL, serial, "operator", "r");
        resume.unwrap();
    }
}

fn repost_config(enabled: bool, listed: &[u64]) -> RepostConfig {
    RepostConfig {
        enabled,
        channels: listed.iter().copied().collect(),
    }
}

fn tui_o(repost: RepostConfig) -> TuiOConfig {
    TuiOConfig {
        repost,
        ..TuiOConfig::default()
    }
}

/// The production re-post turn over `pool`; it never opens a transport.
fn receipt_turn(root: &Path, pool: PgPool, repost: RepostConfig) -> ChannelTurn<RepostHttp> {
    let switch = RepostSwitch::new(Some(&tui_o(repost)), watch::channel(None).1);
    turn_over(root, pool, switch)
}

fn turn_over(root: &Path, pool: PgPool, switch: RepostSwitch) -> ChannelTurn<RepostHttp> {
    let run = RunScope {
        holder: "node-a".into(),
        run: "d4d".into(),
        credentials: "creds".into(),
        evidence_generation: 1,
    };
    let sidecar = root.join("repost").join(format!("{CHANNEL}.jsonl"));
    let runner = Runner::new(switch, pool, BOT, run);
    ChannelTurn::new(runner, Box::new(|| None), CHANNEL, sidecar)
}

fn unreachable_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/none")
        .unwrap()
}

/// Polls in real time until `done` or about ten seconds.
async fn until(mut done: impl FnMut() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// What a recovered writer came to: its posts, the stop its constructor decided, the next local
/// serial and whether the approval at serial 0 was consumed.
type Started = (Vec<String>, Option<WriterAlarm>, u64, Option<Option<u64>>);

/// A live config naming `repost`, as a validated reload installs it.
fn live(repost: RepostConfig) -> Option<Arc<crate::config::Config>> {
    let mut config = crate::config::Config::default();
    config.tui_o = Some(tui_o(repost));
    Some(Arc::new(config))
}

/// Starts the actor over a writer built with the receipts its production turn read, appends one
/// row and reports what that start came to. `withdraw` unlists the channel after the read.
async fn start_writer(
    harness: &Harness,
    path: &Path,
    pool: PgPool,
    repost: RepostConfig,
    withdraw: bool,
) -> Started {
    let (reload, installed) = watch::channel(live(repost));
    let switch = RepostSwitch::new(None, installed);
    let mut hook = turn_over(harness._runtime.path(), pool, switch);
    let receipts = <ChannelTurn<RepostHttp> as RepostHook<Writer>>::receipts(&mut hook).await;
    if withdraw {
        reload.send_replace(live(repost_config(true, &[OTHER])));
    }
    let (gate, port) = (Arc::clone(&harness.gate), Arc::clone(&harness.port));
    let lease = Arc::clone(&harness.lease);
    let alarms = harness.alarms.clone();
    let writer =
        ChannelWriter::with_receipts(harness.channel(), gate, port, lease, alarms, receipts)
            .with_repost(Some(Box::new(hook)));
    let initially = writer.stop_cause().map(|cause| cause.alarm.clone());
    let (stop, task) = spawn(writer);
    append(path, &row("m1", "after"));
    until(|| {
        task.is_finished()
            || harness
                .port
                .posts()
                .last()
                .is_some_and(|last| last == "after")
    })
    .await;
    let _ = stop.send(true);
    task.await.unwrap();
    let channel = harness.channel();
    let ledger = channel.ledger();
    let approval = ledger.approval(0).map(|approval| approval.consumed_serial);
    (
        harness.port.posts(),
        initially,
        ledger.next_serial(),
        approval,
    )
}

/// A receipted 403 starts the writer with no POST of it; no receipt, a failed read, other payload
/// or version, an unreceipted 403 or a withdrawn listing keep the stop; off, approval goes once.
#[tokio::test]
async fn d4d_receipt_overlay_judges_the_recovered_writer_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let other_payload = Shared {
        payload: "another payload",
        ..RECEIPTED
    };
    let other_version = Shared {
        identity_version: IDENTITY_VERSION + 1,
        ..RECEIPTED
    };
    let none = Shared {
        receipt: false,
        ..RECEIPTED
    };
    let rows = [
        ("w-approved", Some(RECEIPTED), true),
        ("w-blocked", Some(RECEIPTED), false),
        ("w-missing", Some(none), false),
        ("w-down", None, false),
        ("w-payload", Some(other_payload), false),
        ("w-version", Some(other_version), false),
        ("w-two", Some(RECEIPTED), false),
        ("w-withdrawn", Some(RECEIPTED), false),
        ("w-off", Some(RECEIPTED), true),
    ];
    let mut observed = Vec::new();
    for (id, (native, stored, approve)) in (6_000..).zip(rows) {
        if let Some(stored) = stored {
            shared(&pool, native, stored, id).await;
        }
        let (harness, path, _) = switched_over(b"");
        seed_rejection(&harness, native, 0, approve);
        if native == "w-two" {
            seed_rejection(&harness, "w-two-unreceipted", 1, false);
        }
        let before = std::fs::read(ledger_path(&harness)).unwrap();
        harness.gate.acquired();
        let reachable = if stored.is_some() {
            pool.clone()
        } else {
            unreachable_pool()
        };
        let repost = repost_config(native != "w-off", &[CHANNEL]);
        let withdraw = native == "w-withdrawn";
        let started = start_writer(&harness, &path, reachable, repost, withdraw).await;
        let after = std::fs::read(ledger_path(&harness)).unwrap();
        assert!(
            after.starts_with(&before),
            "{native}: the ledger is only appended to"
        );
        observed.push((native, started));
    }
    let blocked = |approval| (vec![], Some(BLOCKED), 1, approval);
    let after = vec!["after".to_owned()];
    let sent = vec![PAYLOAD.to_owned(), "after".to_owned()];
    let expected = [
        ("w-approved", (after.clone(), None, 2, Some(None))),
        ("w-blocked", (after, None, 2, None)),
        ("w-missing", blocked(None)),
        ("w-down", blocked(None)),
        ("w-payload", blocked(None)),
        ("w-version", blocked(None)),
        ("w-two", (vec![], Some(BLOCKED), 2, None)),
        ("w-withdrawn", blocked(None)),
        ("w-off", (sent, None, 3, Some(Some(1)))),
    ];
    assert_eq!(observed, expected);
    pool.close().await;
    db.drop().await;
}

/// Off, with an empty allowlist or for an unlisted channel, the production turn answers
/// `Disabled` before PostgreSQL; the same unreachable pool reads as `Unknown` once listed.
#[tokio::test]
async fn d4d_receipt_overlay_reads_nothing_off_or_unlisted() {
    let configs = [
        (repost_config(false, &[CHANNEL]), "disabled"),
        (repost_config(true, &[]), "disabled"),
        (repost_config(true, &[OTHER]), "disabled"),
        (repost_config(true, &[CHANNEL]), "unknown"),
    ];
    let root = tempfile::tempdir().unwrap();
    for (repost, expected) in configs {
        let mut hook = receipt_turn(root.path(), unreachable_pool(), repost);
        let read = <ChannelTurn<RepostHttp> as RepostHook<Writer>>::receipts(&mut hook).await;
        let read = match read {
            ReceiptOverlay::Disabled => "disabled",
            ReceiptOverlay::Known { .. } => "known",
            ReceiptOverlay::Unknown(_) => "unknown",
        };
        assert_eq!(read, expected);
    }
    assert!(!root.path().join("repost").exists(), "no sidecar either");
}

/// Behind an unreceipted approval held by a busy lease, a receipted approval is no owed work: the
/// drain projection counts one piece, and once the lease frees only the unreceipted one posts.
#[tokio::test]
async fn d4d_receipted_approval_is_never_owed_pg() {
    use crate::services::tui_o::writer::actor::{Owing, spawn_projecting};
    let _hosts = crate::config::session_hosts::force_for_test(Some("book"), &[(CHANNEL, "book")]);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    shared(&pool, "w-owed-receipted", RECEIPTED, 6_100).await;
    let (harness, _, _) = switched_over(b"");
    seed_rejection(&harness, "w-owed-open", 0, true);
    seed_rejection(&harness, "w-owed-receipted", 1, true);
    harness.gate.acquired();
    harness.lease.busy.store(true, Ordering::SeqCst);
    let hook = receipt_turn(
        harness._runtime.path(),
        pool.clone(),
        repost_config(true, &[CHANNEL]),
    );
    let (gate, port) = (Arc::clone(&harness.gate), Arc::clone(&harness.port));
    let (lease, alarms) = (Arc::clone(&harness.lease), harness.alarms.clone());
    let hook: Box<dyn RepostHook<Writer>> = Box::new(hook);
    let mut writer =
        ChannelWriter::hosted(harness.channel(), gate, port, lease, alarms, Some(hook)).await;
    let bindings = startup_log(&mut writer);
    let (stop, stopped) = watch::channel(false);
    let owing = Owing::default();
    let projection = owing.published.subscribe();
    let _wanting = owing.demand.clone().want();
    let watches = (
        stopped,
        watch::channel(false).0,
        watch::channel(None).0,
        owing,
    );
    let config = WriterConfig { enabled: true };
    let provider = ShadowProvider::Claude;
    let task = spawn_projecting(&config, None, writer, provider, bindings, watches).unwrap();
    until(|| projection.borrow().is_some()).await;
    let owed = projection.borrow().map(|pending| pending.owed);
    harness.lease.busy.store(false, Ordering::SeqCst);
    until(|| !harness.port.posts().is_empty()).await;
    stop.send(true).unwrap();
    task.await.unwrap();
    let channel = harness.channel();
    let consumed = |serial| channel.ledger().approval(serial).unwrap().consumed_serial;
    let observed = (owed, harness.port.posts(), consumed(0), consumed(1));
    assert_eq!(observed, (Some(1), vec![PAYLOAD.to_owned()], Some(2), None));
    pool.close().await;
    db.drop().await;
}

/// The test host with the production re-post turn over a real pool.
struct ReceiptIo {
    base: Arc<TestIo>,
    pool: PgPool,
    repost: RepostConfig,
    root: PathBuf,
}

impl HostIo for ReceiptIo {
    type Port = FakePort;
    type Lease = Arc<FakeLease>;
    type Alarms = Raised;
    type Bindings = ChannelBindingLog;

    fn port(&self) -> impl Future<Output = Arc<FakePort>> + Send {
        self.base.port()
    }
    fn lease(&self) -> Arc<FakeLease> {
        self.base.lease()
    }
    fn alarms(&self) -> Raised {
        self.base.alarms()
    }
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        self.base.bindings(channel, provider)
    }
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.base.activation_facts(channel, provider)
    }
    fn intake_fence(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<FencedFacts, String>> + Send {
        self.base.intake_fence(channel, provider)
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
    fn repost(&self, _: u64, _: &Arc<FakePort>) -> Option<RepostTurn<Self>> {
        let turn = receipt_turn(&self.root, self.pool.clone(), self.repost.clone());
        Some(Box::new(turn))
    }
}

/// What a hosted start of the channel came to: its posts, whether it stopped Blocked, the next
/// local serial and whether the approval at serial 0 was consumed.
type Hosted = (Vec<String>, bool, u64, Option<Option<u64>>);

/// Hosts the switched-over channel through `start` with the real re-post turn, appends the row
/// `out` once the actor is ready or stopped, and reports what that start came to.
async fn host_receipts(
    harness: &Harness,
    path: &Path,
    source: &SourceId,
    pool: PgPool,
    repost: RepostConfig,
    out: &str,
) -> Hosted {
    let startup = p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source.clone()));
    p5_log(harness._runtime.path(), CHANNEL, &startup);
    p5::set_test_root(Some(harness._runtime.path()));
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let io = Arc::new(ReceiptIo {
        base: TestIo::over(harness),
        pool,
        repost,
        root: root(harness),
    });
    let ready = Arc::new(Readiness::default());
    let parts = || HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(root(harness)),
        gate: Arc::clone(&harness.gate),
        readiness: Arc::clone(&ready),
    };
    let tasks = start(ShadowProvider::Claude, true, parts);
    let stopped = || io.base.alarms.has(CHANNEL, &BLOCKED);
    until(|| ready.is_ready(CHANNEL) || stopped()).await;
    append(path, &row(out, out));
    until(|| stopped() || harness.port.posts().last().is_some_and(|last| last == out)).await;
    abort(tasks);
    let channel = harness.channel();
    let ledger = channel.ledger();
    let approval = ledger.approval(0).map(|approval| approval.consumed_serial);
    (
        harness.port.posts(),
        stopped(),
        ledger.next_serial(),
        approval,
    )
}

/// A's own POST is refused 403 and an operator approves it; another home's receipt for the same
/// piece then lets A restart without sending or consuming the approval, and later output posts.
#[tokio::test]
async fn d4d_shared_receipt_settles_local_authorized_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Refused(403));
    let on = || repost_config(true, &[CHANNEL]);
    let first = host_receipts(&harness, &path, &source, pool.clone(), on(), "first").await;
    assert_eq!((first.0, first.1), (vec!["first".to_owned()], true));
    let refused = harness.channel().ledger().piece(0).unwrap().clone();
    let resume = harness
        .store
        .record_operator_resume(CHANNEL, 0, "operator", "r");
    resume.unwrap();
    let elsewhere = Shared {
        payload: "first",
        ..RECEIPTED
    };
    shared(&pool, &refused.unit_key.native_key, elsewhere, 5_001).await;
    let before = std::fs::read(ledger_path(&harness)).unwrap();
    let hosted = host_receipts(&harness, &path, &source, pool.clone(), on(), "after").await;
    let posts = vec!["first".to_owned(), "after".to_owned()];
    let expected = (posts, false, 2, Some(None));
    assert_eq!(
        hosted, expected,
        "the receipted approval is neither sent nor consumed"
    );
    let channel = harness.channel();
    let local = channel.ledger().disposition(&refused.unit_key, 0);
    assert_eq!(local, PieceDisposition::Authorized { rejected_serial: 0 });
    let after = std::fs::read(ledger_path(&harness)).unwrap();
    assert!(after.starts_with(&before), "the ledger is only appended to");
    pool.close().await;
    db.drop().await;
}

/// A local 403 without approval at startup: a normal read without a receipt and a failed read
/// keep the original stop and send nothing; a receipt lets the actor start and post later output.
#[tokio::test]
async fn d4d_startup_blocked_reconciles_before_actor_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    shared(&pool, "d4d-receipted", RECEIPTED, 5_002).await;
    let none = Shared {
        receipt: false,
        ..RECEIPTED
    };
    shared(&pool, "d4d-unreceipted", none, 0).await;
    let cases = [
        ("d4d-unreceipted", pool.clone()),
        ("d4d-receipted", unreachable_pool()),
        ("d4d-receipted", pool.clone()),
    ];
    let mut observed = Vec::new();
    for (native, pool) in cases {
        let (harness, path, source) = switched_over(b"");
        seed_rejection(&harness, native, 0, false);
        harness.gate.acquired();
        let on = repost_config(true, &[CHANNEL]);
        let (posts, stopped, next, _) =
            host_receipts(&harness, &path, &source, pool, on, "after").await;
        observed.push((native, posts, stopped, next));
    }
    let after = || vec!["after".to_owned()];
    let expected = [
        ("d4d-unreceipted", vec![], true, 1),
        ("d4d-receipted", vec![], true, 1),
        ("d4d-receipted", after(), false, 2),
    ];
    assert_eq!(observed, expected, "missing, unreadable, receipted");
    pool.close().await;
    db.drop().await;
}

/// Home A's own POST is refused 403 and A stops; home B then records the piece's receipt under
/// its own origin serial. A's restart reads it and neither stops on nor resends the piece.
#[tokio::test]
async fn d4d_remote_receipt_overrides_local_serial_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Refused(403));
    let on = || repost_config(true, &[CHANNEL]);
    let first = host_receipts(&harness, &path, &source, pool.clone(), on(), "first").await;
    let refused = harness.channel().ledger().piece(0).unwrap().clone();
    assert_eq!(refused.outcome, Some(PieceOutcome::Rejected(403)));
    let at_b = Shared {
        payload: "first",
        ..RECEIPTED
    };
    shared(&pool, &refused.unit_key.native_key, at_b, 5_003).await;
    let restarted = host_receipts(&harness, &path, &source, pool.clone(), on(), "after").await;
    let observed = [(first.0, first.1), (restarted.0, restarted.1)];
    let both = vec!["first".to_owned(), "after".to_owned()];
    let expected = [(vec!["first".to_owned()], true), (both, false)];
    assert_eq!(observed, expected, "before and after B's receipt");
    let channel = harness.channel();
    let latest = channel.ledger().latest_piece(&refused.unit_key, 0).unwrap();
    assert_eq!(latest, (0, &refused), "A's own record stays as written");
    pool.close().await;
    db.drop().await;
}

/// Off, an empty allowlist and an unlisted channel keep the approval's one POST and the fresh
/// original's one POST over a receipted piece; only the listed channel reads the receipt.
#[tokio::test]
async fn d4d_dormant_host_positive_controls_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    shared(&pool, "d4d-dormant", RECEIPTED, 5_004).await;
    let cases = [
        ("off", repost_config(false, &[CHANNEL])),
        ("empty", repost_config(true, &[])),
        ("unlisted", repost_config(true, &[OTHER])),
        ("listed", repost_config(true, &[CHANNEL])),
    ];
    let mut observed = Vec::new();
    for (case, repost) in cases {
        let (harness, path, source) = switched_over(b"");
        seed_rejection(&harness, "d4d-dormant", 0, true);
        harness.gate.acquired();
        let hosted = host_receipts(&harness, &path, &source, pool.clone(), repost, "after").await;
        observed.push((case, hosted));
    }
    let sent = || vec![PAYLOAD.to_owned(), "after".to_owned()];
    let expected = [
        ("off", (sent(), false, 3, Some(Some(1)))),
        ("empty", (sent(), false, 3, Some(Some(1)))),
        ("unlisted", (sent(), false, 3, Some(Some(1)))),
        ("listed", (vec!["after".to_owned()], false, 2, Some(None))),
    ];
    assert_eq!(observed, expected);
    pool.close().await;
    db.drop().await;
}
