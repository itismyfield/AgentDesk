//! Shared receipts read before a channel's actor starts: a receipted rejection neither stops the
//! writer nor sends its approval again, and the local ledger and PostgreSQL rows stay as they were.

use sqlx::PgPool;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
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
use crate::services::tui_o::repost::runner::{ChannelTurn, Runner, TurnWriter};
use crate::services::tui_o::store::ledger::PieceDisposition;
use crate::services::tui_o::writer::host::{FencedFacts, start_managed};
use std::sync::atomic::AtomicBool;

#[path = "pg_statements_tests.rs"]
mod pg_statements;
use pg_statements::{Statements, observed_pool};

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
        notice_origin: None,
        local_retry: None,
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

/// The production re-post turn over `pool`; it never opens a transport.
fn receipt_turn(root: &Path, pool: PgPool) -> ChannelTurn<RepostHttp> {
    let run = RunScope {
        holder: "node-a".into(),
        run: "d4d".into(),
        credentials: "creds".into(),
        evidence_generation: 1,
    };
    let sidecar = root.join("repost").join(format!("{CHANNEL}.jsonl"));
    let runner = Runner::new(pool, BOT, run);
    ChannelTurn::new(runner, Box::new(|| None), CHANNEL, sidecar)
}

fn unreachable_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/none")
        .unwrap()
}

/// A pool whose re-post statements are recorded once armed, beside the fixture's own pool.
struct Observed {
    pool: PgPool,
    statements: Arc<Statements>,
}

impl Observed {
    async fn over(db: &TestPostgresDb) -> Self {
        let (pool, statements, _proxy) = observed_pool(&db.database_url).await;
        Self { pool, statements }
    }

    fn unreachable() -> Self {
        let statements = Arc::default();
        let pool = unreachable_pool();
        Self { pool, statements }
    }

    /// The startup receipt reads recorded since the last arm, told apart from other re-post SQL.
    fn receipt_reads(&self) -> usize {
        let read = [
            "FROM o_piece_delivery d WHERE",
            "FROM o_piece_receipts r WHERE",
        ];
        self.statements.matching(&read)
    }
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

/// Starts the actor after reading its production turn's shared receipts, appends one row and
/// reports what that start came to.
async fn start_writer(harness: &Harness, path: &Path, pool: PgPool) -> Started {
    let mut hook = receipt_turn(harness._runtime.path(), pool);
    let receipts = <ChannelTurn<RepostHttp> as RepostHook<Writer>>::receipts(&mut hook).await;
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
/// or version, and an unreceipted 403 keep the stop.
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
        let started = start_writer(&harness, &path, reachable).await;
        let after = std::fs::read(ledger_path(&harness)).unwrap();
        assert!(
            after.starts_with(&before),
            "{native}: the ledger is only appended to"
        );
        observed.push((native, started));
    }
    let blocked = |approval| (vec![], Some(BLOCKED), 1, approval);
    let after = vec!["after".to_owned()];
    let expected = [
        ("w-approved", (after.clone(), None, 2, Some(None))),
        ("w-blocked", (after, None, 2, None)),
        ("w-missing", blocked(None)),
        ("w-down", blocked(None)),
        ("w-payload", blocked(None)),
        ("w-version", blocked(None)),
        ("w-two", (vec![], Some(BLOCKED), 2, None)),
    ];
    assert_eq!(observed, expected);
    pool.close().await;
    db.drop().await;
}

/// An unreadable shared receipt index holds the original stop and never creates a sidecar.
#[tokio::test]
async fn d4d_receipt_overlay_unreadable_pool_is_unknown() {
    let root = tempfile::tempdir().unwrap();
    let mut hook = receipt_turn(root.path(), unreachable_pool());
    let read = <ChannelTurn<RepostHttp> as RepostHook<Writer>>::receipts(&mut hook).await;
    assert!(matches!(read, ReceiptOverlay::Unknown(_)));
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
    let hook = receipt_turn(harness._runtime.path(), pool.clone());
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

/// The test host with the production re-post turn over a real pool; `held` holds its receipt
/// read until released.
struct ReceiptIo {
    base: Arc<TestIo>,
    pool: PgPool,
    root: PathBuf,
    held: Option<(Arc<AtomicBool>, watch::Receiver<bool>)>,
}

/// The production turn whose receipt read, once begun, waits for its release.
struct HeldRead {
    turn: ChannelTurn<RepostHttp>,
    entered: Arc<AtomicBool>,
    release: watch::Receiver<bool>,
}

impl<W: TurnWriter> RepostHook<W> for HeldRead {
    fn turn<'a>(&'a mut self, writer: &'a mut W) -> Turned<'a, ()> {
        <ChannelTurn<RepostHttp> as RepostHook<W>>::turn(&mut self.turn, writer)
    }

    fn original(&mut self, serial: u64, record: PieceRecord) -> Turned<'_, OriginalGate> {
        <ChannelTurn<RepostHttp> as RepostHook<W>>::original(&mut self.turn, serial, record)
    }

    fn receipts(&mut self) -> Turned<'_, ReceiptOverlay> {
        Box::pin(async move {
            self.entered.store(true, Ordering::SeqCst);
            let _ = self.release.wait_for(|released| *released).await;
            <ChannelTurn<RepostHttp> as RepostHook<W>>::receipts(&mut self.turn).await
        })
    }
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
        let turn = receipt_turn(&self.root, self.pool.clone());
        let Some((entered, release)) = self.held.clone() else {
            return Some(Box::new(turn));
        };
        Some(Box::new(HeldRead {
            turn,
            entered,
            release,
        }))
    }
}

/// What a hosted start of the channel came to: its posts, whether it stopped Blocked, the next
/// local serial and whether the approval at serial 0 was consumed.
type Hosted = (Vec<String>, bool, u64, Option<Option<u64>>);

/// The switched-over channel's startup binding, logged where the host reads it.
fn log_startup(harness: &Harness, source: &SourceId) {
    let startup = p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source.clone()));
    p5_log(harness._runtime.path(), CHANNEL, &startup);
    p5::set_test_root(Some(harness._runtime.path()));
}

/// Starts the channel's host with the real turn over `pg`, recording from just before `start`;
/// appends the row `out` once the actor is ready or stopped and reports what that start came to.
async fn host_receipts(
    harness: &Harness,
    path: &Path,
    source: &SourceId,
    pg: &Observed,
    out: &str,
) -> Hosted {
    log_startup(harness, source);
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let io = Arc::new(ReceiptIo {
        base: TestIo::over(harness),
        pool: pg.pool.clone(),
        root: root(harness),
        held: None,
    });
    let ready = Arc::new(Readiness::default());
    let parts = || HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(root(harness)),
        gate: Arc::clone(&harness.gate),
        readiness: Arc::clone(&ready),
    };
    pg.statements.reset();
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
    let pg = Observed::over(&db).await;
    let first = host_receipts(&harness, &path, &source, &pg, "first").await;
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
    let hosted = host_receipts(&harness, &path, &source, &pg, "after").await;
    assert!(pg.receipt_reads() >= 1, "the restart read the receipt");
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
        ("d4d-unreceipted", Observed::over(&db).await),
        ("d4d-receipted", Observed::unreachable()),
        ("d4d-receipted", Observed::over(&db).await),
    ];
    let mut observed = Vec::new();
    for (native, pg) in cases {
        let (harness, path, source) = switched_over(b"");
        seed_rejection(&harness, native, 0, false);
        harness.gate.acquired();
        let (posts, stopped, next, _) = host_receipts(&harness, &path, &source, &pg, "after").await;
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
    let pg = Observed::over(&db).await;
    let first = host_receipts(&harness, &path, &source, &pg, "first").await;
    let refused = harness.channel().ledger().piece(0).unwrap().clone();
    assert_eq!(refused.outcome, Some(PieceOutcome::Rejected(403)));
    let at_b = Shared {
        payload: "first",
        ..RECEIPTED
    };
    shared(&pool, &refused.unit_key.native_key, at_b, 5_003).await;
    let restarted = host_receipts(&harness, &path, &source, &pg, "after").await;
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

/// The default host reads shared receipts before its actor starts and leaves shared rows intact.
#[tokio::test]
async fn d4d_default_host_reads_shared_receipts_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    shared(&pool, "d4d-default", RECEIPTED, 5_004).await;
    let pg = Observed::over(&db).await;
    let rows = repost_rows(&pool).await;
    let (harness, path, source) = switched_over(b"");
    seed_rejection(&harness, "d4d-default", 0, true);
    harness.gate.acquired();
    let hosted = host_receipts(&harness, &path, &source, &pg, "after").await;
    assert_eq!(hosted, (vec!["after".to_owned()], false, 2, Some(None)));
    assert!(pg.statements.count() > 0 && pg.receipt_reads() >= 1);
    assert!(root(&harness).join("repost").exists());
    assert_eq!(repost_rows(&pool).await, rows);
    pool.close().await;
    db.drop().await;
}

/// A ledger an earlier build wrote: `d4d-legacy` refused 403, then approved by an operator.
const LEGACY_LEDGER: [&str; 3] = [
    r#"{"at":"2026-09-01T00:00:00Z","entry":{"type":"prepared","serial":0,"unit_key":{"channel_id":7,"provider":"claude","native_key":"d4d-legacy","kind":"body"},"piece_index":0,"payload":"receipted payload","anchor_id":100,"epoch":1}}"#,
    r#"{"at":"2026-09-01T00:00:01Z","entry":{"type":"rejected","serial":0,"status":403}}"#,
    r#"{"at":"2026-09-01T00:05:00Z","entry":{"type":"operator_resume","rejected_serial":0,"approval_id":"6325d408-0000-4000-8000-000000000008","operator":"operator","reason":"legacy approval","at":"2026-09-01T00:05:00Z"}}"#,
];

/// Every re-post row PostgreSQL holds, as text, in a stable order.
async fn repost_rows(pool: &PgPool) -> Vec<String> {
    let sql = "SELECT 'attempt ' || to_jsonb(attempt)::text FROM o_piece_attempts attempt \
               UNION ALL SELECT 'receipt ' || to_jsonb(receipt)::text FROM o_piece_receipts receipt \
               UNION ALL SELECT 'delivery ' || to_jsonb(delivery)::text \
               FROM o_piece_delivery delivery ORDER BY 1";
    sqlx::query_scalar(sql).fetch_all(pool).await.unwrap()
}

/// The channel's ledger bytes, its approval at serial 0 and the shared re-post rows.
async fn persisted(harness: &Harness, pool: &PgPool) -> (Vec<u8>, String, Vec<String>) {
    let ledger = std::fs::read(ledger_path(harness)).unwrap();
    let approval = format!("{:?}", harness.channel().ledger().approval(0));
    (ledger, approval, repost_rows(pool).await)
}

/// A legacy ledger's receipted approval: building the hosted writer writes nothing local or shared,
/// fresh output appends only its own piece, and a restart reads the receipt again.
#[tokio::test]
async fn d4d_legacy_ledger_receipt_overlay_is_read_only_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    shared(&pool, "d4d-legacy", RECEIPTED, 5_005).await;
    let (harness, path, source) = switched_over(b"");
    let mut legacy = LEGACY_LEDGER.join("\n");
    legacy.push('\n');
    append(&ledger_path(&harness), legacy.as_bytes());
    harness.gate.acquired();
    let pg = Observed::over(&db).await;

    let store = harness.channel();
    let before = persisted(&harness, &pool).await;
    pg.statements.reset();
    let hook = receipt_turn(harness._runtime.path(), pg.pool.clone());
    let hook: Box<dyn RepostHook<Writer>> = Box::new(hook);
    let (gate, port) = (Arc::clone(&harness.gate), Arc::clone(&harness.port));
    let (lease, alarms) = (Arc::clone(&harness.lease), harness.alarms.clone());
    let writer = ChannelWriter::hosted(store, gate, port, lease, alarms, Some(hook)).await;
    let applied = (writer.stop_cause().is_none(), writer.resume_pieces().len());
    drop(writer);
    let read = pg.receipt_reads();
    let after = persisted(&harness, &pool).await;
    assert_eq!((applied, read >= 1), ((true, 0), true));
    let unchanged = (
        after.0 == before.0,
        after.1 == before.1,
        after.2 == before.2,
    );
    assert_eq!(
        unchanged,
        (true, true, true),
        "ledger bytes, approval, shared rows"
    );

    let fresh = host_receipts(&harness, &path, &source, &pg, "fresh").await;
    let (ledger, approval, rows) = persisted(&harness, &pool).await;
    assert_eq!(fresh, (vec!["fresh".to_owned()], false, 2, Some(None)));
    let appended = ledger.strip_prefix(before.0.as_slice()).unwrap();
    let (activation, entries): (Vec<serde_json::Value>, Vec<serde_json::Value>) =
        String::from_utf8_lossy(appended)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .partition(|line: &serde_json::Value| line["entry"]["type"] == "input_notice");
    // The first actor start freezes the input notice migration once, beside the output pieces.
    assert_eq!(activation.len(), 1);
    assert_eq!(activation[0]["entry"]["entry"]["kind"], "migration_started");
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries[0]["entry"],
        serde_json::json!({
            "type": "excluded",
            "unit_key": {
                "channel_id": CHANNEL,
                "provider": "claude",
                "native_key": "fresh:0#turn",
                "kind": "tool"
            },
            "reason": "turn_summary_open"
        })
    );
    let serials: Vec<_> = entries[1..]
        .iter()
        .map(|line| {
            (
                line["entry"]["type"].clone(),
                line["entry"]["serial"].clone(),
            )
        })
        .collect();
    assert_eq!(
        serials,
        [("prepared".into(), 1.into()), ("posted".into(), 1.into())],
        "fresh output appends only its own piece"
    );
    assert_eq!((approval, rows), (before.1.clone(), before.2.clone()));

    let again = host_receipts(&harness, &path, &source, &pg, "again").await;
    let posts = vec!["fresh".to_owned(), "again".to_owned()];
    let restarted = (again, pg.receipt_reads() >= 1);
    assert_eq!(restarted, ((posts, false, 3, Some(None)), true));
    assert_eq!(repost_rows(&pool).await, before.2);
    pool.close().await;
    db.drop().await;
}

/// A stop while the receipt read is held ends the managed host before any actor exists: it joins
/// before the read is released, frees its claim, and the approved retry is never sent.
#[tokio::test]
async fn d4d_stop_during_receipt_read_starts_no_actor() {
    let (harness, _, source) = switched_over(b"");
    seed_rejection(&harness, "d4d-held", 0, true);
    harness.gate.acquired();
    log_startup(&harness, &source);
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (release, held) = watch::channel(false);
    let entered = Arc::new(AtomicBool::new(false));
    let io = Arc::new(ReceiptIo {
        base: TestIo::over(&harness),
        pool: unreachable_pool(),
        root: root(&harness),
        held: Some((Arc::clone(&entered), held)),
    });
    let ready = Arc::new(Readiness::default());
    let parts = || HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(root(&harness)),
        gate: Arc::clone(&harness.gate),
        readiness: Arc::clone(&ready),
    };
    let owned = cutover::boot_ownership();
    let mut writers = start_managed(ShadowProvider::Claude, true, owned, parts);
    assert_eq!(writers.len(), 1);
    until(|| entered.load(Ordering::SeqCst)).await;
    let read_began = entered.load(Ordering::SeqCst);
    let stopping = writers.pop().unwrap().stop_and_join();
    let bounded = std::time::Duration::from_secs(5);
    let joined = matches!(tokio::time::timeout(bounded, stopping).await, Ok(Ok(_)));
    let left = (
        ready.is_ready(CHANNEL),
        ready.is_hosted(CHANNEL),
        ready.accepts(CHANNEL),
    );
    let held_posts = harness.port.posts();
    release.send_replace(true);
    until(|| !ready.is_hosted(CHANNEL)).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let next = harness.channel().ledger().next_serial();
    let observed = (
        read_began,
        joined,
        left,
        held_posts,
        harness.port.posts(),
        next,
    );
    let expected = (true, true, (false, false, false), vec![], vec![], 1);
    assert_eq!(
        observed, expected,
        "the stop ended the start before any actor"
    );
}
