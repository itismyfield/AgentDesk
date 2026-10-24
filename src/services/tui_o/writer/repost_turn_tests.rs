//! The re-post turn sends through the channel writer's lease, gate and ledger, never beside
//! another post of the channel.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::{Notify, watch};

use super::*;

#[path = "local_retry_repost_tests.rs"]
mod local_retry;
#[path = "scoped_repost_tests.rs"]
mod scoped;
#[path = "tool_result_repost_tests.rs"]
mod tool_result;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::repost::admission::{Admitter, OriginalAttempt, OriginalGate};
use crate::services::tui_o::repost::dispatch::{self, Admission, RepostWriter};
use crate::services::tui_o::repost::identity::PieceKey;
use crate::services::tui_o::repost::io::probe::ProbeRead;
use crate::services::tui_o::repost::io::probe::evidence::{EvidenceScope, RunScope};
use crate::services::tui_o::repost::io::probe::matcher::ObservedMessage;
use crate::services::tui_o::repost::io::probe::tests::absent_evidence;
use crate::services::tui_o::repost::o_piece_attempts::{AttemptResult, attempts};
use crate::services::tui_o::repost::o_piece_delivery::{
    Receipt, ReceiptMethod, ReceiptOutcome, load, record_receipt,
};
use crate::services::tui_o::repost::provenance::ProvenanceLog;
use crate::services::tui_o::repost::runner::{
    ChannelTurn, DispatchIntent, RepostHook, Runner, TurnWriter, Turned,
};
use crate::services::tui_o::repost::send::{
    AttemptGuard, BoundedTransport, CreatedMessage, RepostEnvelope, WireOutcome,
};
use crate::services::tui_o::store::ledger::PieceRecord;

fn key(native_key: &str) -> PieceKey {
    PieceKey::new(unit(native_key), 0).unwrap()
}

/// Admits `native_key` as an uncertain on original of `payload`, slot 0 spent.
async fn admitted(pool: &PgPool, native_key: &str, payload: &str) -> PieceKey {
    let dir = tempfile::tempdir().unwrap();
    let mut log = ProvenanceLog::open(&dir.path().join("s.jsonl")).unwrap();
    let admitter = Admitter::new(pool, "node-a", BOT);
    let original = OriginalAttempt {
        serial: 3,
        key: key(native_key),
        payload: payload.into(),
        anchor: 40,
    };
    admitter.adopt_uncertain(&mut log, &original).await.unwrap();
    original.key
}

fn run_scope() -> RunScope {
    RunScope {
        holder: "node-a".into(),
        run: "run-f6".into(),
        credentials: "creds".into(),
        evidence_generation: 1,
    }
}

/// Re-post reads answer from the writer port's history. A send counts each request that leaves;
/// it may draw one 429 first, and it may stay in flight until `release`.
#[derive(Default)]
struct Io {
    port: Option<Arc<FakePort>>,
    reads: AtomicUsize,
    wire: Arc<AtomicUsize>,
    /// Requests in flight now.
    active: Arc<AtomicUsize>,
    throttle_first: AtomicBool,
    hold: AtomicBool,
    /// Notified once a request waits, after its 429 or while held.
    waiting: Arc<Notify>,
    release: Arc<Notify>,
    outcome: Mutex<Option<WireOutcome>>,
    drop_count: Mutex<Option<Arc<AtomicUsize>>>,
    dropped_count: Arc<Mutex<Vec<usize>>>,
}

struct ObserveRequestDrop(Option<Arc<AtomicUsize>>, Arc<Mutex<Vec<usize>>>);

impl Drop for ObserveRequestDrop {
    fn drop(&mut self) {
        if let Some(count) = &self.0 {
            self.1.lock().unwrap().push(count.load(Ordering::SeqCst));
        }
    }
}

impl Io {
    fn over(port: &Arc<FakePort>) -> Arc<Self> {
        Arc::new(Self {
            port: Some(Arc::clone(port)),
            ..Self::default()
        })
    }

    fn wire(&self) -> usize {
        self.wire.load(Ordering::SeqCst)
    }

    fn seen(&self, channel: u64) -> Vec<ObservedMessage> {
        let Some(port) = &self.port else {
            return Vec::new();
        };
        let history = port.history.lock().unwrap();
        let observed = history.iter().map(|message| ObservedMessage {
            id: message.id,
            channel_id: channel,
            author_id: message.author_id,
            content: message.content.clone(),
            rich_embeds: 0,
            footers: Vec::new(),
            nonce: None,
        });
        observed.collect()
    }
}

impl ProbeRead for Io {
    fn credentials(&self) -> String {
        "creds".into()
    }

    fn message(
        &self,
        channel: u64,
        id: u64,
    ) -> impl Future<Output = Result<Option<ObservedMessage>, String>> + Send {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let found = self.seen(channel).into_iter().find(|seen| seen.id == id);
        async move { Ok(found) }
    }

    fn history(
        &self,
        channel: u64,
        before: Option<u64>,
        limit: u8,
    ) -> impl Future<Output = Result<Vec<ObservedMessage>, String>> + Send {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mut page: Vec<_> = (self.seen(channel).into_iter())
            .filter(|seen| before.is_none_or(|before| seen.id < before))
            .collect();
        page.sort_by_key(|seen| std::cmp::Reverse(seen.id));
        page.truncate(limit.into());
        async move { Ok(page) }
    }
}

impl BoundedTransport for Io {
    fn create(
        &self,
        envelope: &RepostEnvelope,
        guard: AttemptGuard,
    ) -> impl Future<Output = WireOutcome> + Send + 'static {
        let body = serde_json::to_value(envelope.message()).unwrap();
        let footer = body["embeds"][0]["footer"]["text"]
            .as_str()
            .map(str::to_owned);
        let content = body["content"].as_str().unwrap_or_default().to_owned();
        let (wire, active) = (Arc::clone(&self.wire), Arc::clone(&self.active));
        let (waiting, release) = (Arc::clone(&self.waiting), Arc::clone(&self.release));
        let throttle = self.throttle_first.load(Ordering::SeqCst);
        let hold = self.hold.load(Ordering::SeqCst);
        let port = self.port.clone();
        let outcome = self.outcome.lock().unwrap().clone();
        let drop_count = self.drop_count.lock().unwrap().clone();
        let dropped_count = Arc::clone(&self.dropped_count);
        async move {
            let _drop = ObserveRequestDrop(drop_count, dropped_count);
            if let Some(outcome @ WireOutcome::Unsent(_)) = outcome.clone() {
                return outcome;
            }
            let leave = || guard.begin().map(|()| wire.fetch_add(1, Ordering::SeqCst));
            if let Err(reason) = leave() {
                return WireOutcome::Unsent(reason);
            }
            if throttle {
                guard.throttled();
                if outcome == Some(WireOutcome::Throttled) {
                    return WireOutcome::Throttled;
                }
                waiting.notify_one();
                release.notified().await;
                if let Err(reason) = leave() {
                    return WireOutcome::Unsent(reason);
                }
            }
            active.fetch_add(1, Ordering::SeqCst);
            if hold {
                waiting.notify_one();
                release.notified().await;
            }
            active.fetch_sub(1, Ordering::SeqCst);
            if let Some(outcome) = outcome {
                return outcome;
            }
            // Discord ids grow, so the re-post lands after everything the channel already holds.
            let id = port.map_or(9_000, |port| port.say(BOT, &content).id);
            WireOutcome::Created(CreatedMessage {
                id,
                author_id: BOT,
                content,
                footers: footer.into_iter().collect(),
            })
        }
    }
}

fn sidecar(harness: &Harness) -> std::path::PathBuf {
    harness._runtime.path().join("repost").join("7.jsonl")
}

fn turn(harness: &Harness, pool: PgPool, io: &Arc<Io>) -> ChannelTurn<Io> {
    let runner = Runner::new(pool, BOT, run_scope());
    let io = Arc::clone(io);
    ChannelTurn::new(
        runner,
        Box::new(move || Some(Arc::clone(&io))),
        CHANNEL,
        sidecar(harness),
    )
}

fn unreachable() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/none")
        .unwrap()
}

/// Counts the turns the actor asks of `inner`.
struct Counted<H> {
    inner: H,
    turns: Arc<AtomicUsize>,
}

impl<W, H: RepostHook<W>> RepostHook<W> for Counted<H> {
    fn turn<'a>(&'a mut self, writer: &'a mut W) -> Turned<'a, ()> {
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.inner.turn(writer)
    }

    fn original(&mut self, serial: u64, record: PieceRecord) -> Turned<'_, OriginalGate> {
        self.inner.original(serial, record)
    }

    fn receipts(
        &mut self,
    ) -> Turned<'_, crate::services::tui_o::repost::projection::ReceiptOverlay> {
        self.inner.receipts()
    }
}

/// Evidence for `key` as this run would gather it now.
async fn evidence(pool: &PgPool, key: &PieceKey) -> DispatchIntent<'static> {
    let row = load(pool, key).await.unwrap().unwrap();
    let spent = attempts(pool, key).await.unwrap();
    let scope = EvidenceScope::of(&row, &spent, &run_scope()).unwrap();
    DispatchIntent::Auto(Box::new(absent_evidence(scope, run_scope())))
}

async fn results(pool: &PgPool, key: &PieceKey) -> Vec<Option<AttemptResult>> {
    let spent = attempts(pool, key).await.unwrap();
    spent.into_iter().map(|attempt| attempt.result).collect()
}

/// Whether message `id` already is `key`'s receipt, read back by recording it once more.
async fn is_receipt(pool: &PgPool, key: &PieceKey, id: u64) -> bool {
    let receipt = Receipt {
        key: key.clone(),
        message_id: id,
        author_id: BOT,
        slot: None,
        method: ReceiptMethod::PostResponse,
    };
    let outcome = record_receipt(pool, &receipt).await.unwrap();
    outcome == ReceiptOutcome::Known
}

async fn grant(runner: &mut Runner, writer: &mut Writer, pool: &PgPool, key: &PieceKey) -> bool {
    let intent = evidence(pool, key).await;
    let admitted = runner.try_next_dispatch(writer, intent);
    matches!(admitted.await.unwrap(), Admission::Granted { .. })
}

/// An operator resume, an ordinary piece and an Auto re-post of one channel never overlap their
/// POSTs; the re-post goes once at the gate's epoch, and an excluded unit gets no slot.
#[tokio::test]
async fn f6_general_auto_and_resume_never_overlap_posts_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let excluded = admitted(&pool, "f6-excluded", "excluded piece").await;
    let (harness, path, _) = switched_over(b"");
    let epoch = harness.gate.acquired();
    let mut channel = harness.channel();
    let resumed = LedgerEntry::Prepared {
        local_retry: None,
        serial: 0,
        unit_key: unit("resumed"),
        piece_index: 0,
        payload: "resumed payload".into(),
        anchor_id: 100,
        epoch,
    };
    channel.append_ledger(resumed).unwrap();
    let refused = LedgerEntry::Rejected {
        serial: 0,
        status: 403,
    };
    channel.append_ledger(refused).unwrap();
    let excluding = LedgerEntry::Excluded {
        unit_key: excluded.unit().clone(),
        reason: "operator".into(),
    };
    channel.append_ledger(excluding).unwrap();
    drop(channel);
    let resume = harness
        .store
        .record_operator_resume(CHANNEL, 0, "operator", "restored");
    resume.unwrap();
    let io = Io::over(&harness.port);
    io.hold.store(true, Ordering::SeqCst);
    let overlaps = Arc::new(AtomicUsize::new(0));
    let (active, seen) = (Arc::clone(&io.active), Arc::clone(&overlaps));
    let on_post = move || {
        if active.load(Ordering::SeqCst) > 0 {
            seen.fetch_add(1, Ordering::SeqCst);
        }
    };
    *harness.port.on_post.lock().unwrap() = Some(Box::new(on_post));
    let hook = turn(&harness, pool.clone(), &io);
    let (stop, task) = spawn(harness.writer().with_repost(Some(Box::new(hook))));
    // Admitted later, so the excluded piece's evidence comes first, alone.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let auto = admitted(&pool, "f6-auto", "auto piece").await;
    // Two probe passes, 10 s and then 20 s apart, come before any re-post.
    let waiting = tokio::time::timeout(Duration::from_secs(60), io.waiting.notified());
    waiting.await.expect("the re-post leaves");
    assert_eq!(harness.port.posts(), ["resumed payload"]);
    append(&path, &row("m1", "ordinary"));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(harness.port.posts(), ["resumed payload"], "it waits");
    io.release.notify_one();
    for _ in 0..50 {
        if harness.port.posts().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), ["resumed payload", "ordinary"]);
    assert_eq!((overlaps.load(Ordering::SeqCst), io.wire()), (0, 1));
    let channel = harness.channel();
    let ledger = channel.ledger();
    assert!(ledger.violation().is_none());
    let (_, latest) = ledger.latest_piece(auto.unit(), 0).unwrap();
    let history = harness.port.history.lock().unwrap().clone();
    let reposted = history.iter().find(|seen| seen.content == "auto piece");
    let reposted = reposted.unwrap().id;
    let posted = Some(PieceOutcome::Posted(reposted));
    assert_eq!((latest.epoch, &latest.outcome), (epoch, &posted));
    let created = [Some(AttemptResult::Uncertain), Some(AttemptResult::Created)];
    assert_eq!(results(&pool, &auto).await, created);
    assert!(is_receipt(&pool, &auto, reposted).await);
    assert_eq!(results(&pool, &excluded).await.len(), 1);
    pool.close().await;
    db.drop().await;
}

/// Each writer guard keeps Auto from a slot: a disabled writer, a stopped writer, a busy lease
/// and a gateway not owned. The same writer with nothing in the way grants.
#[tokio::test]
async fn f6_t07_all_writer_guards_block_auto_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = admitted(&pool, "f6-t07", "piece").await;
    let harness = Harness::new();
    let mut runner = Runner::new(pool.clone(), BOT, run_scope());
    for case in 0..4 {
        harness.gate.acquired();
        let mut writer = harness.writer();
        writer.set_live(case != 0);
        if case == 1 {
            writer.stop(WriterAlarm::Halted { detail: "x".into() });
        }
        harness.lease.busy.store(case == 2, Ordering::SeqCst);
        if case == 3 {
            harness.gate.lost();
        }
        assert!(
            !grant(&mut runner, &mut writer, &pool, &key).await,
            "{case}"
        );
    }
    assert_eq!(results(&pool, &key).await.len(), 1, "no slot was spent");
    harness.lease.busy.store(false, Ordering::SeqCst);
    harness.gate.acquired();
    let mut writer = harness.writer();
    writer.set_live(true);
    assert!(grant(&mut runner, &mut writer, &pool, &key).await);
    pool.close().await;
    db.drop().await;
}

/// A Prepared the channel left open is settled from history before Auto takes a slot, so the
/// re-post's Prepared is never a second open one.
#[tokio::test]
async fn f6_t09_open_prepared_blocks_every_sender_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = admitted(&pool, "f6-t09", "piece").await;
    let harness = Harness::new();
    let epoch = harness.gate.acquired();
    let open = LedgerEntry::Prepared {
        local_retry: None,
        serial: 0,
        unit_key: unit("left-open"),
        piece_index: 0,
        payload: "left open".into(),
        anchor_id: 100,
        epoch,
    };
    harness.channel().append_ledger(open).unwrap();
    let landed = harness.port.say(BOT, "left open").id;
    let mut writer = harness.writer();
    writer.set_live(true);
    let io = Io::over(&harness.port);
    let mut runner = Runner::new(pool.clone(), BOT, run_scope());
    let intent = evidence(&pool, &key).await;
    let admitted = runner.try_next_dispatch(&mut writer, intent);
    let Admission::Granted { permit, lease } = admitted.await.unwrap() else {
        panic!("granted once the open one is settled");
    };
    let started = dispatch::start(&mut writer, permit, lease, &*io).await;
    started.finish(&mut writer, &pool).await.unwrap();
    assert!(!writer.is_stopped());
    let ledger = TurnWriter::ledger(&mut writer);
    let settled = ledger.piece(0).unwrap().outcome.clone();
    assert_eq!(settled, Some(PieceOutcome::Posted(landed)));
    let (serial, latest) = ledger.latest_piece(key.unit(), 0).unwrap();
    assert_eq!(
        (serial, &latest.outcome),
        (1, &Some(PieceOutcome::Posted(landed + 1)))
    );
    assert_eq!(io.wire(), 1);
    pool.close().await;
    db.drop().await;
}

/// After a restart an admitted piece never posts as a fresh original; an unreadable admission
/// holds it until PostgreSQL can confirm its ownership.
#[tokio::test]
async fn f6_t10_recovered_admitted_piece_never_becomes_new_original_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = admitted(&pool, "f6-t10", "piece").await;
    let harness = Harness::new();
    harness.gate.acquired();
    let io = Io::over(&harness.port);
    let work = piece("f6-t10", "piece");
    let cases = [(pool.clone(), Step::Done), (unreachable(), Step::LeaseBusy)];
    for (pool, step) in cases {
        let hook = turn(&harness, pool, &io);
        let mut writer = harness.writer().with_repost(Some(Box::new(hook)));
        assert_eq!(writer.deliver(&work).await, step);
    }
    assert!(
        harness.port.posts().is_empty(),
        "recovery invents no original"
    );
    assert!(
        harness
            .channel()
            .ledger()
            .latest_piece(key.unit(), 0)
            .is_none()
    );
    pool.close().await;
    db.drop().await;
}

/// Turning on never adopts earlier pieces, open or uncertain, across a restart; an original sent
/// while on has its intent written before its POST, and the next turn admits its uncertain result.
#[tokio::test]
async fn f6_t11_pre_activation_backlog_never_posts_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let harness = Harness::new();
    let epoch = harness.gate.acquired();
    let prepared = |serial, native: &str| LedgerEntry::Prepared {
        local_retry: None,
        serial,
        unit_key: unit(native),
        piece_index: 0,
        payload: format!("{native} payload"),
        anchor_id: 100,
        epoch,
    };
    let mut channel = harness.channel();
    channel.append_ledger(prepared(0, "before")).unwrap();
    channel
        .append_ledger(LedgerEntry::NotFound { serial: 0 })
        .unwrap();
    channel.append_ledger(prepared(1, "open-before")).unwrap();
    drop(channel);
    let io = Io::over(&harness.port);
    let admitted = |native: &str| {
        let (pool, key) = (pool.clone(), key(native));
        async move { load(&pool, &key).await.unwrap().is_some() }
    };
    turn(&harness, pool.clone(), &io)
        .turn(&mut harness.writer())
        .await;
    assert!(!admitted("open-before").await);
    let settled = LedgerEntry::NotFound { serial: 1 };
    harness.channel().append_ledger(settled).unwrap();
    // A restarted actor records a new activation; what came before stays out.
    let mut hook = turn(&harness, pool.clone(), &io);
    let mut writer = harness.writer();
    hook.turn(&mut writer).await;
    let mut writer = writer.with_repost(Some(Box::new(hook)));
    let path = sidecar(&harness);
    let intent_first = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&intent_first);
    let on_post = move || {
        let log = ProvenanceLog::open(&path).unwrap();
        *seen.lock().unwrap() = Some(log.state().intent(2).is_some());
    };
    *harness.port.on_post.lock().unwrap() = Some(Box::new(on_post));
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Unsent);
    // An unreadable history settles the lost reply as unresolved.
    harness.port.unreadable.store(true, Ordering::SeqCst);
    assert_eq!(writer.deliver(&piece("on", "on payload")).await, Step::Done);
    assert_eq!(*intent_first.lock().unwrap(), Some(true));
    writer.repost_turn().await;
    for native in ["before", "open-before"] {
        assert!(!admitted(native).await, "{native}");
    }
    assert!(admitted("on").await);
    assert_eq!(io.wire(), 0, "no absence evidence yet");
    pool.close().await;
    db.drop().await;
}

/// A writer dropped while its re-post waits out a 429 never lets that request send again, and
/// the lease goes back only once the request has ended.
#[tokio::test]
async fn f6_a_dropped_writer_never_lets_its_repost_send_again_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = admitted(&pool, "f6-cancel", "piece").await;
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    writer.set_live(true);
    let io = Io::over(&harness.port);
    io.throttle_first.store(true, Ordering::SeqCst);
    let mut runner = Runner::new(pool.clone(), BOT, run_scope());
    let intent = evidence(&pool, &key).await;
    let admitted = runner.try_next_dispatch(&mut writer, intent);
    let Admission::Granted { permit, lease } = admitted.await.unwrap() else {
        panic!("granted");
    };
    let started = dispatch::start(&mut writer, permit, lease, &*io).await;
    io.waiting.notified().await;
    let released = harness.lease.released.load(Ordering::SeqCst);
    drop((started, writer));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.lease.released.load(Ordering::SeqCst), released);
    io.release.notify_one();
    for _ in 0..50 {
        if harness.lease.released.load(Ordering::SeqCst) > released {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(harness.lease.released.load(Ordering::SeqCst), released + 1);
    assert_eq!(io.wire(), 1, "the 429 alone left");
    pool.close().await;
    db.drop().await;
}

#[path = "../repost/writer_activation_tests.rs"]
mod activation;

// Delivery and turn selection share the ordinary gateway contract despite stale home state.
#[tokio::test(flavor = "current_thread")]
async fn residual_home_row_does_not_change_writer_or_turn_host_pg() {
    use crate::services::provider::ProviderKind;
    use crate::services::turn_host::{self, TurnHost};

    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[]);
    let _channels = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = CHANNEL.to_string();
    let poison_epoch = i64::MAX - 7;
    sqlx::query(
        "INSERT INTO o_channel_homes
          (channel_id, provider, state, holder, target, epoch, renewed_at, detail)
         VALUES ($1, 'codex', 'worker', 'stale-remote', NULL, $2,
                 NOW() - INTERVAL '1 year', 'poison: ignore this home authority')",
    )
    .bind(&channel)
    .bind(poison_epoch)
    .execute(&pool)
    .await
    .unwrap();
    let read = || async {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT row_to_json(h)::jsonb FROM o_channel_homes h WHERE channel_id=$1",
        )
        .bind(&channel)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let before = read().await;
    assert_eq!(before["epoch"], poison_epoch);
    assert_eq!(before["holder"], "stale-remote");
    assert_eq!(before["provider"], "codex");
    let harness = Harness::new();
    let mut writer = harness.writer();
    harness.gate.acquired();
    let posted = writer
        .deliver(&piece("poison-home", "ignored-home-row"))
        .await;
    let refusal =
        turn_host::refusal_before_turn(Some(&pool), &ProviderKind::Claude, CHANNEL, || async {
            None
        })
        .await;
    let host = turn_host::for_turn(Some(&pool), &ProviderKind::Claude, CHANNEL, None).await;
    assert_eq!(
        (
            posted == Step::Done,
            refusal.is_none(),
            matches!(host, TurnHost::Tmux)
        ),
        (true, true, true),
        "writer and TurnHost ignore residual home authority",
    );
    assert_eq!(harness.port.posts(), ["ignored-home-row"]);
    assert_eq!(read().await, before, "residual home row is untouched");
    pool.close().await;
    db.drop().await;
}

#[path = "always_on_repost_tests.rs"]
mod always_on;
