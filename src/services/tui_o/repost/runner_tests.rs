use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::admission::{ApprovedRejected, OriginalGate, PriorBudget};
use super::admission_tests::{BOT, CHANNEL, admit_on, next_grant, send_uncertain, sidecar, switch};
use super::dispatch::{Admission, DispatchEnd, DispatchPermit, GuardReason, RepostWriter, start};
use super::identity::{PieceKey, marker};
use super::io::probe::evidence::{EvidenceScope, NotFoundEvidence, RunScope};
use super::io::probe::matcher::ObservedMessage;
use super::io::probe::tests::{Fake, absent_evidence};
use super::o_piece_attempts::{AttemptResult, GrantOutcome, Intent, attempts};
use super::o_piece_delivery::{Failure, load};
use super::runner::{ApprovalCheck, DispatchIntent, Runner, Tick, TurnStep, turn_step};
use super::send::{
    AttemptGuard, BoundedTransport, CreatedMessage, DispatchReport, RepostEnvelope, RepostIds,
    WireOutcome, hand_over,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};

/// Holds the lease while alive; the writer's `held` counts live holds.
#[derive(Debug)]
struct TestLease(Arc<AtomicUsize>);

impl Drop for TestLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A writer whose ledger is a list: Prepared entries and the reports that ended them.
struct TestWriter {
    gate: Arc<OwnershipGate>,
    guard: Result<(), String>,
    lease_busy: bool,
    held: Arc<AtomicUsize>,
    live: Arc<AtomicBool>,
    /// `(epoch, slot)` of every Prepared appended.
    prepared: Arc<StdMutex<Vec<(u64, u8)>>>,
    records: Vec<DispatchReport>,
}

impl TestWriter {
    fn new() -> Self {
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        Self {
            gate,
            guard: Ok(()),
            lease_busy: false,
            held: Arc::default(),
            live: Arc::new(AtomicBool::new(true)),
            prepared: Arc::default(),
            records: Vec::new(),
        }
    }

    fn prepared(&self) -> Vec<(u64, u8)> {
        self.prepared.lock().unwrap().clone()
    }

    fn open(&self) -> usize {
        self.prepared().len() - self.records.len()
    }
}

impl RepostWriter for TestWriter {
    type Lease = TestLease;

    fn settle_open(&mut self) -> impl Future<Output = bool> + Send {
        let settled = self.open() == 0;
        async move { settled }
    }

    fn guard(&self) -> Result<(), String> {
        self.guard.clone()
    }

    fn try_lease(&self) -> Option<TestLease> {
        if self.lease_busy {
            return None;
        }
        self.held.fetch_add(1, Ordering::SeqCst);
        Some(TestLease(Arc::clone(&self.held)))
    }

    fn gate(&self) -> Arc<OwnershipGate> {
        Arc::clone(&self.gate)
    }

    fn live(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let live = Arc::clone(&self.live);
        Arc::new(move || live.load(Ordering::SeqCst))
    }

    fn prepare(&mut self, epoch: u64, permit: &DispatchPermit) -> Result<(), String> {
        self.prepared.lock().unwrap().push((epoch, permit.slot()));
        Ok(())
    }

    fn record(&mut self, report: &DispatchReport) -> impl Future<Output = ()> + Send {
        self.records.push(report.clone());
        async {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// Created, keeping the footer, so the response is the receipt.
    Created,
    Uncertain,
    /// One 429, then waits for `release` before asking again.
    ThrottledThenWait,
    /// Waits for `release`, then created.
    Hang,
}

/// `(epoch, slot)` of the Prepared entries a writer holds.
type Prepared = Vec<(u64, u8)>;

/// A transport that counts POSTs through the real guard and notes what its first poll saw.
struct Wire {
    answer: Answer,
    posts: Arc<AtomicUsize>,
    release: Arc<Notify>,
    prepared: Arc<StdMutex<Vec<(u64, u8)>>>,
    /// The writer's Prepared entries as each request's first poll found them.
    first_polls: Arc<StdMutex<Vec<Prepared>>>,
    /// Requests whose future was dropped, finished or not.
    dropped: Arc<AtomicUsize>,
}

/// Counts its request as gone when the request's future is dropped.
struct Gone(Arc<AtomicUsize>);

impl Drop for Gone {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Wire {
    fn new(answer: Answer, writer: &TestWriter) -> Self {
        Self {
            answer,
            posts: Arc::default(),
            release: Arc::new(Notify::new()),
            prepared: Arc::clone(&writer.prepared),
            first_polls: Arc::default(),
            dropped: Arc::default(),
        }
    }

    fn posts(&self) -> usize {
        self.posts.load(Ordering::SeqCst)
    }
}

impl BoundedTransport for Wire {
    fn create(
        &self,
        envelope: &RepostEnvelope,
        guard: AttemptGuard,
    ) -> impl Future<Output = WireOutcome> + Send + 'static {
        let (answer, posts, release) = (
            self.answer,
            Arc::clone(&self.posts),
            Arc::clone(&self.release),
        );
        let (prepared, first_polls) = (Arc::clone(&self.prepared), Arc::clone(&self.first_polls));
        let gone = Gone(Arc::clone(&self.dropped));
        let body = serde_json::to_value(envelope.message()).unwrap();
        let footer = body["embeds"][0]["footer"]["text"]
            .as_str()
            .map(str::to_owned);
        async move {
            let _gone = gone;
            let seen = prepared.lock().unwrap().clone();
            first_polls.lock().unwrap().push(seen);
            let mut throttled = false;
            loop {
                if let Err(reason) = guard.begin() {
                    return WireOutcome::Unsent(reason);
                }
                posts.fetch_add(1, Ordering::SeqCst);
                match answer {
                    Answer::ThrottledThenWait if !throttled => {
                        throttled = true;
                        guard.throttled();
                        release.notified().await;
                        continue;
                    }
                    Answer::Hang => release.notified().await,
                    Answer::Uncertain => return WireOutcome::Uncertain("reset".into()),
                    _ => {}
                }
                return WireOutcome::Created(CreatedMessage {
                    id: 7_000 + posts.load(Ordering::SeqCst) as u64,
                    author_id: BOT,
                    content: String::new(),
                    footers: footer.clone().into_iter().collect(),
                });
            }
        }
    }
}

fn message(id: u64, author: u64, content: &str) -> ObservedMessage {
    ObservedMessage {
        channel_id: CHANNEL,
        ..super::io::probe::tests::message(id, author, content)
    }
}

fn holder(name: &str) -> RunScope {
    RunScope {
        holder: name.into(),
        run: format!("run-{name}"),
        credentials: "creds".into(),
        evidence_generation: 1,
    }
}

fn runner(pool: &PgPool, on: bool, run: &RunScope) -> Runner {
    Runner::new(switch(on), pool.clone(), BOT, run.clone())
}

/// Fresh absence evidence of `key` as `run` would gather it now.
async fn evidence(pool: &PgPool, key: &PieceKey, run: &RunScope) -> Box<NotFoundEvidence> {
    let row = load(pool, key).await.unwrap().unwrap();
    let spent = attempts(pool, key).await.unwrap();
    let scope = EvidenceScope::of(&row, &spent, run).unwrap();
    Box::new(absent_evidence(scope, run.clone()))
}

async fn slots(pool: &PgPool, key: &PieceKey) -> Vec<(u8, Option<AttemptResult>)> {
    let rows = attempts(pool, key).await.unwrap();
    rows.iter().map(|row| (row.slot, row.result)).collect()
}

/// Waits until `count` sessions of this database block on a lock.
async fn lock_waiters(pool: &PgPool, count: i64) {
    let started = std::time::Instant::now();
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
              WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting == count {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{waiting} waiters"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn block_grants(pool: &PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE o_piece_attempts IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    blocker
}

struct Approves(bool);

impl ApprovalCheck for Approves {
    fn current(&self, _approval_id: &str, _approved: &ApprovedRejected) -> Result<(), String> {
        if self.0 {
            Ok(())
        } else {
            Err("a later disposition".into())
        }
    }
}

#[test]
fn f5_admission_mapping_is_exhaustive() {
    let cases: [(Admission<()>, TurnStep); 11] = [
        (Admission::LegacyOff, TurnStep::Legacy),
        (Admission::WaitForOpen, TurnStep::Wait),
        (Admission::ObserveOnly, TurnStep::Wait),
        (Admission::AlreadyResolved, TurnStep::Done),
        (Admission::CapReached, TurnStep::Done),
        (Admission::CapUnknown, TurnStep::Done),
        (Admission::Parked, TurnStep::Stopped),
        (
            Admission::GuardRejected(GuardReason::LeaseBusy),
            TurnStep::Wait,
        ),
        (
            Admission::GuardRejected(GuardReason::NoGateway),
            TurnStep::Wait,
        ),
        (
            Admission::GuardRejected(GuardReason::Writer("stopped".into())),
            TurnStep::Stopped,
        ),
        (
            Admission::GuardRejected(GuardReason::Approval("used".into())),
            TurnStep::Stopped,
        ),
    ];
    for (admission, step) in cases {
        assert_eq!(turn_step(&admission), step, "{admission:?}");
    }
}

/// Off answers from the recovered index alone: a pool that cannot connect is never reached.
#[tokio::test]
async fn f5_default_off_tick_executes_zero_pg_statements() {
    let unreachable = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/none")
        .unwrap();
    let reader = Fake::with([message(900, BOT, "earlier")]);
    let run = holder("a");
    let mut off = Runner::new(switch(false), unreachable.clone(), BOT, run.clone());
    let ticked = off.tick(CHANNEL, &reader, &[900], Instant::now()).await;
    assert!(matches!(ticked, Ok(Tick::Off)), "{ticked:?}");
    let mut writer = TestWriter::new();
    let expected = [
        (OriginalGate::Admitted, TurnStep::Stopped),
        (OriginalGate::Send, TurnStep::Legacy),
        (OriginalGate::Unknown, TurnStep::Wait),
    ];
    for (membership, step) in expected {
        let intent = DispatchIntent::Auto(Box::new(absent_evidence(
            super::io::probe::tests::scope(&[0]),
            super::io::probe::tests::run("run-1"),
        )));
        let admission = off.try_next_dispatch(&mut writer, membership, intent).await;
        assert_eq!(turn_step(&admission.unwrap()), step);
    }
    assert_eq!((reader.reads(), writer.prepared().len()), (0, 0));

    // The on control does reach PostgreSQL.
    let mut on = Runner::new(switch(true), unreachable, BOT, run);
    assert!(
        on.tick(CHANNEL, &reader, &[900], Instant::now())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn f5_t01_two_holders_get_one_dispatch_permit_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let key = admit_on(&pool, &mut sidecar(&dir), &[(3, "t01")], "piece").await[0].clone();
    let blocker = block_grants(&pool).await;
    let racers = ["a", "b"].map(|name| {
        let (pool, key) = (pool.clone(), key.clone());
        tokio::spawn(async move {
            let run = holder(name);
            let evidence = evidence(&pool, &key, &run).await;
            let mut writer = TestWriter::new();
            let mut runner = runner(&pool, true, &run);
            let intent = DispatchIntent::Auto(evidence);
            let admission = runner
                .try_next_dispatch(&mut writer, OriginalGate::Admitted, intent)
                .await
                .unwrap();
            (admission, writer)
        })
    });
    lock_waiters(&pool, 2).await;
    blocker.rollback().await.unwrap();
    let mut granted = Vec::new();
    for racer in racers {
        let (admission, writer) = racer.await.unwrap();
        match admission {
            Admission::Granted { permit, lease } => granted.push((permit, lease, writer)),
            other => assert_eq!(turn_step(&other), TurnStep::Wait, "{other:?}"),
        }
    }
    assert_eq!(granted.len(), 1);
    let (permit, lease, mut writer) = granted.pop().unwrap();
    let wire = Wire::new(Answer::Created, &writer);
    let started = start(&mut writer, permit, lease, &wire).await;
    let DispatchEnd::Sent(report) = started.finish(&mut writer, &pool).await.unwrap() else {
        panic!("sent");
    };
    assert_eq!((report.counted, wire.posts()), (1, 1));
    assert_eq!(
        slots(&pool, &key).await,
        [
            (0, Some(AttemptResult::Uncertain)),
            (1, Some(AttemptResult::Created))
        ]
    );
    // The response kept the marker, so it is the receipt and the budget closes.
    assert_eq!(next_grant(&pool, &key).await, GrantOutcome::Resolved);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f5_t01_lost_commit_reply_never_recreates_a_permit_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let key = admit_on(&pool, &mut sidecar(&dir), &[(3, "t01-lost")], "piece").await[0].clone();
    let run = holder("a");
    let mut runner = runner(&pool, true, &run);
    let mut writer = TestWriter::new();
    let kept = evidence(&pool, &key, &run).await;
    let evidence = evidence(&pool, &key, &run).await;
    let intent = DispatchIntent::Auto(evidence);
    let first = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    // The committed grant's answer is lost with its permit.
    assert!(matches!(first.await.unwrap(), Admission::Granted { .. }));
    let again = DispatchIntent::Auto(kept);
    let second = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, again);
    // The evidence is refused as stale, before any grant could find the slot open.
    let second = second.await.unwrap();
    assert!(matches!(second, Admission::ObserveOnly), "{second:?}");
    assert_eq!(slots(&pool, &key).await.len(), 2);
    assert_eq!(writer.held.load(Ordering::SeqCst), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f5_t02_t25_gate_lost_after_grant_keeps_the_spent_slot_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let key = admit_on(&pool, &mut sidecar(&dir), &[(3, "t25")], "piece").await[0].clone();
    let run = holder("a");
    let mut runner = runner(&pool, true, &run);
    let mut writer = TestWriter::new();
    let intent = DispatchIntent::Auto(evidence(&pool, &key, &run).await);
    let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("granted");
    };
    writer.gate.lost();
    let wire = Wire::new(Answer::Created, &writer);
    let started = start(&mut writer, permit, lease, &wire).await;
    let end = started.finish(&mut writer, &pool).await.unwrap();
    assert_eq!(end, DispatchEnd::NotAdmitted);
    assert_eq!((writer.prepared().len(), wire.posts()), (0, 0));
    assert_eq!(writer.held.load(Ordering::SeqCst), 0);
    assert_eq!(
        slots(&pool, &key).await,
        [
            (0, Some(AttemptResult::Uncertain)),
            (1, Some(AttemptResult::NotSent))
        ]
    );

    // Only new evidence opens slot 2, and that is the last.
    writer.gate.acquired();
    let intent = DispatchIntent::Auto(evidence(&pool, &key, &run).await);
    let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("slot 2");
    };
    assert_eq!(permit.slot(), 2);
    let wire = Wire::new(Answer::Uncertain, &writer);
    let started = start(&mut writer, permit, lease, &wire).await;
    started.finish(&mut writer, &pool).await.unwrap();
    assert_eq!(wire.posts(), 1);
    let intent = DispatchIntent::Auto(evidence(&pool, &key, &run).await);
    let last = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    assert_eq!(turn_step(&last.await.unwrap()), TurnStep::Done);
    assert_eq!(slots(&pool, &key).await.len(), 3);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f5_t06_gate_closes_during_pg_wait_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let key = admit_on(&pool, &mut sidecar(&dir), &[(3, "t06")], "piece").await[0].clone();
    let run = holder("a");
    let evidence = evidence(&pool, &key, &run).await;
    let mut writer = TestWriter::new();
    let gate = Arc::clone(&writer.gate);
    let blocker = block_grants(&pool).await;
    let waiting = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut runner = runner(&pool, true, &run);
            let intent = DispatchIntent::Auto(evidence);
            let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
            let admission = admission.await.unwrap();
            (admission, writer)
        })
    };
    lock_waiters(&pool, 1).await;
    // The gate is not held across the PostgreSQL wait: losing it completes now.
    let closed = tokio::task::spawn_blocking(move || {
        gate.lost();
        gate
    });
    let gate = tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("the gate closes while the grant waits")
        .unwrap();
    assert!(gate.admit(|_| ()).is_none());
    blocker.rollback().await.unwrap();
    let (admission, mut writer) = waiting.await.unwrap();
    let Admission::Granted { permit, lease } = admission else {
        panic!("granted");
    };
    let wire = Wire::new(Answer::Created, &writer);
    let started = start(&mut writer, permit, lease, &wire).await;
    assert_eq!(
        started.finish(&mut writer, &pool).await.unwrap(),
        DispatchEnd::NotAdmitted
    );
    assert_eq!(wire.posts(), 0);
    assert_eq!(slots(&pool, &key).await.len(), 2);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f5_t06_first_transport_poll_and_wire_guard_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let keys = admit_on(
        &pool,
        &mut sidecar(&dir),
        &[(3, "t06-a"), (4, "t06-b")],
        "p",
    )
    .await;
    let run = holder("a");
    let mut runner = runner(&pool, true, &run);
    let mut writer = TestWriter::new();
    let epoch = writer.gate.acquired();

    // The first poll already sees this send's Prepared at the admitted epoch.
    let intent = DispatchIntent::Auto(evidence(&pool, &keys[0], &run).await);
    let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("granted");
    };
    let wire = Wire::new(Answer::ThrottledThenWait, &writer);
    let started = start(&mut writer, permit, lease, &wire).await;
    // Nothing has yielded since `start`, whose only poll is inside `admit`: the request ran
    // there, after this send's Prepared.
    assert_eq!(*wire.first_polls.lock().unwrap(), [vec![(epoch, 1)]]);
    assert_eq!(wire.posts(), 1);
    // The run changes during the 429 wait: the request asks nothing more.
    writer.live.store(false, Ordering::SeqCst);
    wire.release.notify_one();
    let DispatchEnd::Sent(report) = started.finish(&mut writer, &pool).await.unwrap() else {
        panic!("sent");
    };
    assert!(
        matches!(report.outcome, WireOutcome::Unsent(_)),
        "{report:?}"
    );
    assert_eq!((wire.posts(), report.counted), (1, 0));
    assert_eq!(
        slots(&pool, &keys[0]).await[1],
        (1, Some(AttemptResult::NotSent))
    );

    // A changed run before the gate: no Prepared and no request.
    let intent = DispatchIntent::Auto(evidence(&pool, &keys[1], &run).await);
    writer.live.store(true, Ordering::SeqCst);
    let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("granted");
    };
    writer.live.store(false, Ordering::SeqCst);
    let wire = Wire::new(Answer::Created, &writer);
    let started = start(&mut writer, permit, lease, &wire).await;
    let end = started.finish(&mut writer, &pool).await.unwrap();
    assert!(matches!(end, DispatchEnd::Withdrawn(_)), "{end:?}");
    assert_eq!((wire.posts(), writer.prepared().len()), (0, 1));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f5_cancelled_waiter_cannot_release_a_live_request_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let key = admit_on(&pool, &mut sidecar(&dir), &[(3, "cancel")], "piece").await[0].clone();
    let run = holder("a");
    let mut runner = runner(&pool, true, &run);
    let mut writer = TestWriter::new();
    let intent = DispatchIntent::Auto(evidence(&pool, &key, &run).await);
    let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("granted");
    };
    let wire = Wire::new(Answer::Hang, &writer);
    let started = start(&mut writer, permit, lease, &wire).await;
    drop(started);
    tokio::task::yield_now().await;
    // The caller is gone; the request still holds the lease until it ends.
    assert_eq!((writer.held.load(Ordering::SeqCst), wire.posts()), (1, 1));
    wire.release.notify_one();
    let started = std::time::Instant::now();
    while writer.held.load(Ordering::SeqCst) != 0 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the lease outlived its request"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(wire.posts(), 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f5_t09_t23_t24_a_turn_waits_for_the_channel_and_allocates_nothing_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let key = admit_on(&pool, &mut sidecar(&dir), &[(3, "t09")], "piece").await[0].clone();
    let run = holder("a");
    let mut runner = runner(&pool, true, &run);
    let busy = |writer: &mut TestWriter, case: usize| match case {
        // T09: another send's Prepared is open.
        0 => writer.prepared.lock().unwrap().push((1, 0)),
        // T23: the lease is held elsewhere.
        1 => writer.lease_busy = true,
        // T24: the gateway is not owned, either way.
        2 => writer.gate.lost(),
        3 => writer.gate = Arc::new(OwnershipGate::default()),
        _ => writer.guard = Err("stopped".into()),
    };
    for case in 0..5 {
        let mut writer = TestWriter::new();
        busy(&mut writer, case);
        let intent = DispatchIntent::Auto(evidence(&pool, &key, &run).await);
        let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
        let admission = admission.await.unwrap();
        assert!(!matches!(admission, Admission::Granted { .. }), "{case}");
        assert_eq!(writer.held.load(Ordering::SeqCst), 0, "{case}");
    }
    assert_eq!(slots(&pool, &key).await.len(), 1);

    // The positive control: a free channel grants.
    let mut writer = TestWriter::new();
    let intent = DispatchIntent::Auto(evidence(&pool, &key, &run).await);
    let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    assert!(matches!(
        admission.await.unwrap(),
        Admission::Granted { .. }
    ));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f5_t18_t19_operator_and_auto_share_the_three_post_budget_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let unit = UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: "t18".into(),
        kind: UnitKind::Body,
    };
    let approved = ApprovedRejected {
        rejected_serial: 2,
        key: PieceKey::new(unit, 0).unwrap(),
        payload: "piece".into(),
        anchor: 40,
    };
    let resume = |check| DispatchIntent::OperatorResume {
        approval_id: "approval-1".into(),
        approved: approved.clone(),
        prior: PriorBudget::Known { counted_posts: 1 },
        check,
    };
    let run = holder("a");
    let mut writer = TestWriter::new();

    // T19: an approval no longer current, or the switch off, allocates nothing.
    let mut on = runner(&pool, true, &run);
    let stale = on.try_next_dispatch(&mut writer, OriginalGate::Send, resume(&Approves(false)));
    assert_eq!(turn_step(&stale.await.unwrap()), TurnStep::Stopped);
    let mut off = runner(&pool, false, &run);
    let parked =
        off.try_next_dispatch(&mut writer, OriginalGate::Admitted, resume(&Approves(true)));
    assert!(matches!(parked.await.unwrap(), Admission::Parked));
    assert!(load(&pool, &approved.key).await.unwrap().is_none());

    // T18: the 403 original, the approved retry goes uncertain, Auto takes the last slot.
    let admission = on.try_next_dispatch(&mut writer, OriginalGate::Send, resume(&Approves(true)));
    let Admission::Granted { permit, lease } = admission.await.unwrap() else {
        panic!("the approved retry");
    };
    assert_eq!((permit.slot(), permit.payload()), (1, "piece"));
    let wire = Wire::new(Answer::Uncertain, &writer);
    start(&mut writer, permit, lease, &wire)
        .await
        .finish(&mut writer, &pool)
        .await
        .unwrap();
    // A duplicate of the same approval finds the budget, not a new one.
    let again = on.try_next_dispatch(&mut writer, OriginalGate::Send, resume(&Approves(true)));
    let Admission::Granted { permit, lease } = again.await.unwrap() else {
        panic!("slot 2");
    };
    assert_eq!(permit.slot(), 2);
    start(&mut writer, permit, lease, &wire)
        .await
        .finish(&mut writer, &pool)
        .await
        .unwrap();
    let intent = DispatchIntent::Auto(evidence(&pool, &approved.key, &run).await);
    let last = on.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    assert_eq!(turn_step(&last.await.unwrap()), TurnStep::Done);
    let spent = slots(&pool, &approved.key).await;
    assert_eq!(spent.len(), 3);
    assert_eq!(spent[0], (0, Some(AttemptResult::Rejected)));
    assert_eq!(wire.posts(), 2);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f5_present_is_not_automatically_a_receipt_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let keys = admit_on(
        &pool,
        &mut sidecar(&dir),
        &[(3, "found"), (4, "damaged")],
        "p",
    )
    .await;
    let run = holder("a");
    let mut runner = runner(&pool, true, &run);
    let footer = |key: &PieceKey| {
        let ids = RepostIds::for_piece(&marker(key)).unwrap();
        let envelope = RepostEnvelope::additional(CHANNEL, "p".into(), ids);
        let body = serde_json::to_value(envelope.message()).unwrap();
        body["embeds"][0]["footer"]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let found = ObservedMessage {
        rich_embeds: 1,
        footers: vec![footer(&keys[0])],
        ..message(450, BOT, "p")
    };
    // The second piece's copy lost its marker: damaged, so present but nobody's receipt.
    let damaged = ObservedMessage {
        rich_embeds: 1,
        footers: vec![footer(&keys[1])[..10].to_owned()],
        ..message(460, BOT, "바뀐 본문")
    };
    let reader = Fake::with([message(900, BOT, "earlier"), found, damaged]);
    let settled = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
    let Tick::Ran(report) = runner
        .tick(CHANNEL, &reader, &[900], settled)
        .await
        .unwrap()
    else {
        panic!("on");
    };
    assert_eq!(
        report
            .recorded
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [450]
    );
    assert!(report.ready.is_empty());
    assert_eq!(next_grant(&pool, &keys[0]).await, GrantOutcome::Resolved);
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM o_piece_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(receipts, 1);
    assert_eq!(slots(&pool, &keys[1]).await.len(), 1);
    let _ = Failure::Cap;
    let _ = Intent::AutoReconfirm;
    let _ = send_uncertain;
    pool.close().await;
    db.drop().await;
}

fn live() -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(|| true)
}

fn envelope() -> RepostEnvelope {
    let key = super::io::probe::tests::key("handed");
    RepostEnvelope::additional(
        CHANNEL,
        "p".into(),
        RepostIds::for_piece(&marker(&key)).unwrap(),
    )
}

/// F1r handover: polled once and dropped, the request is gone and its one send stays counted.
#[tokio::test]
async fn f5_a_request_dropped_after_its_first_pending_is_gone_and_stays_counted() {
    let writer = TestWriter::new();
    let wire = Wire::new(Answer::Hang, &writer);
    let (request, counts) = hand_over(&wire, &envelope(), live());
    let mut request = Box::pin(request);
    let first = std::future::poll_fn(|cx| std::task::Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(first.is_pending());
    drop(request);
    assert_eq!(wire.dropped.load(Ordering::SeqCst), 1);
    let report = counts.report(WireOutcome::TimedOut);
    assert_eq!((report.wire, report.counted, report.throttled), (1, 1, 0));
    wire.release.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(wire.posts(), 1);
}

/// F1r handover: the task that owns the request, aborted and joined, takes the request with it.
#[tokio::test]
async fn f5_aborting_the_owning_task_ends_the_request_without_a_retry() {
    for (answer, counted) in [
        (Answer::Hang, (1, 1, 0)),
        (Answer::ThrottledThenWait, (1, 0, 1)),
    ] {
        let writer = TestWriter::new();
        let wire = Wire::new(answer, &writer);
        let (request, counts) = hand_over(&wire, &envelope(), live());
        let owner = tokio::spawn(request);
        while wire.posts() == 0 {
            tokio::task::yield_now().await;
        }
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        assert_eq!(wire.dropped.load(Ordering::SeqCst), 1, "{answer:?}");
        let report = counts.report(WireOutcome::TimedOut);
        assert_eq!((report.wire, report.counted, report.throttled), counted);
        wire.release.notify_one();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(wire.posts(), 1, "{answer:?}");
    }
}

/// Evidence for a piece whose row is gone: the turn observes, it never falls back to the legacy
/// send, and nothing is granted or posted.
#[tokio::test]
async fn auto_missing_row_is_observe_only_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let run = super::io::probe::tests::run("run-1");
    let mut runner = runner(&pool, true, &run);
    let mut writer = TestWriter::new();
    let gone = absent_evidence(super::io::probe::tests::scope(&[0]), run.clone());
    let key = gone.scope().key.clone();
    assert!(load(&pool, &key).await.unwrap().is_none());
    let intent = DispatchIntent::Auto(Box::new(gone));
    let admission = runner.try_next_dispatch(&mut writer, OriginalGate::Admitted, intent);
    let admission = admission.await.unwrap();
    assert!(matches!(admission, Admission::ObserveOnly), "{admission:?}");
    assert_eq!(turn_step(&admission), TurnStep::Wait);
    assert_eq!(writer.prepared().len(), 0);
    assert!(attempts(&pool, &key).await.unwrap().is_empty());
    pool.close().await;
    db.drop().await;
}
