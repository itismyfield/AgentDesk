use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox_delivery_proof::settle_unknown_by_operator;
use crate::db::intake_outbox_status::IntakeOutboxStatus;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration as TokioDuration, timeout};

const READY: SettlementCapabilities = SettlementCapabilities {
    stamp_dispatched: false,
    settle_and_sweep: true,
};
const LOWERED: SettlementCapabilities = SettlementCapabilities {
    stamp_dispatched: false,
    settle_and_sweep: false,
};
const D: IntakeOutboxStatus = IntakeOutboxStatus::Dispatched;
const S: IntakeOutboxStatus = IntakeOutboxStatus::Spawned;
const U: IntakeOutboxStatus = IntakeOutboxStatus::Unknown;

async fn setup() -> (TestPostgresDb, PgPool) {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    (database, pool)
}

async fn finish(database: TestPostgresDb, pool: PgPool) {
    pool.close().await;
    database.drop().await;
}

fn cutoffs(now: DateTime<Utc>) -> SweepCutoffs {
    SweepCutoffs {
        dispatched: now - Duration::minutes(30),
        spawned: now - Duration::minutes(30),
        heartbeat_fresh: now - Duration::seconds(30),
    }
}

async fn seed(
    pool: &PgPool,
    key: &str,
    status: IntakeOutboxStatus,
    at: Option<DateTime<Utc>>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO public.intake_outbox(
           target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,
           request_owner_id,user_text,turn_kind,agent_id,status,claim_owner,
           spawned_at,dispatched_at)
         VALUES('worker','leader',$1,$1,'user','hello','standard','agent',$2,
                'dispatch-worker',$3,CASE WHEN $2='dispatched' THEN $3 ELSE NULL END)
         RETURNING id",
    )
    .bind(key)
    .bind(status)
    .bind(at)
    .fetch_one(pool)
    .await
    .expect("seed intake row")
}

async fn status(pool: &PgPool, id: i64) -> IntakeOutboxStatus {
    sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read intake status")
}

async fn seed_clocked(
    pool: &PgPool,
    key: &str,
    status: IntakeOutboxStatus,
    at: DateTime<Utc>,
) -> i64 {
    seed(pool, key, status, Some(at)).await
}

async fn seed_inflight(
    pool: &PgPool,
    channel: &str,
    heartbeat: Option<DateTime<Utc>>,
    active_dispatch_id: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO public.sessions(session_key,status,active_dispatch_id,last_heartbeat,channel_id)
         VALUES($1,'turn_active',$3,$2,$1)",
    )
    .bind(channel)
    .bind(heartbeat)
    .bind(active_dispatch_id)
    .execute(pool)
    .await
    .expect("seed durable inflight signal");
}

async fn lock_intake_row(pool: &PgPool, id: i64) -> sqlx::Transaction<'_, sqlx::Postgres> {
    let mut transaction = pool.begin().await.expect("begin intake row lock");
    sqlx::query("SELECT id FROM public.intake_outbox WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *transaction)
        .await
        .expect("lock intake row");
    transaction
}

async fn wait_for_blocked_on(pool: &PgPool, holder_pid: i32, expected: i64) {
    timeout(TokioDuration::from_secs(5), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*)::bigint FROM pg_stat_activity a
                  WHERE $1 = ANY(pg_blocking_pids(a.pid))",
            )
            .bind(holder_pid)
            .fetch_one(pool)
            .await
            .expect("observe blocked sweep settlement");
            if blocked >= expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("sweep reaches locked terminal CAS");
}

async fn wait_for_spawned_settlement_waiters(pool: &PgPool, expected: i64) {
    timeout(TokioDuration::from_secs(5), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*)::bigint FROM pg_stat_activity
                  WHERE wait_event_type = 'Lock'
                    AND query LIKE 'SELECT id FROM public.intake_outbox%'
                    AND query LIKE '%status = ''spawned'' FOR UPDATE%'",
            )
            .fetch_one(pool)
            .await
            .expect("observe concurrent sweep settlement waiters");
            if blocked >= expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both sweeps reach the locked spawned settlement");
}

#[derive(Clone)]
struct TestLogWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for TestLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn settlement_case(row_status: IntakeOutboxStatus, key: &str, caps: SettlementCapabilities) {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let id = seed_clocked(&pool, key, row_status, now - Duration::hours(1)).await;
    let stats = sweep_once(&pool, caps, cutoffs(now), 200).await.unwrap();
    assert_eq!(stats.settled, 1);
    assert_eq!(status(&pool, id).await, U);
    finish(db, pool).await;
}

#[tokio::test]
async fn sweep_settles_stale_dispatched_as_unknown_pg() {
    settlement_case(D, "stale-dispatched", READY).await;
}

#[tokio::test]
async fn sweep_settles_stale_spawned_as_unknown_pg() {
    settlement_case(S, "stale-spawned", READY).await;
}

#[tokio::test]
async fn sweep_runs_when_stage_lowered_but_open_dispatched_exists_pg() {
    settlement_case(D, "lowered-d", LOWERED).await;
}

#[tokio::test]
async fn sweep_runs_after_restart_with_only_spawned_stamp_debt_pg() {
    settlement_case(S, "lowered-s", LOWERED).await;
}

#[tokio::test]
async fn sweep_skips_rows_not_strictly_stale_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let equal_d = seed_clocked(&pool, "equal-d", D, cutoffs(now).dispatched).await;
    let equal_s = seed_clocked(&pool, "equal-s", S, cutoffs(now).spawned).await;
    assert_eq!(
        sweep_once(&pool, READY, cutoffs(now), 200)
            .await
            .unwrap()
            .settled,
        0
    );
    assert_eq!(status(&pool, equal_d).await, D);
    assert_eq!(status(&pool, equal_s).await, S);
    finish(db, pool).await;
}

#[tokio::test(flavor = "current_thread")]
async fn sweep_is_bounded_and_ordered_and_logs_truncation_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let oldest = seed_clocked(&pool, "oldest", S, now - Duration::hours(2)).await;
    let newer = seed_clocked(&pool, "newer", S, now - Duration::hours(1)).await;
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer_buffer = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || TestLogWriter {
            buffer: Arc::clone(&writer_buffer),
        })
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _guard = tracing::subscriber::set_default(subscriber);
    let stats = sweep_once(&pool, READY, cutoffs(now), 1).await.unwrap();
    drop(_guard);
    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert_eq!((stats.settled, stats.truncated_spawned), (1, 1));
    assert!(
        logs.contains("intake delivery sweep batch limit left stale rows for a later tick")
            && logs.contains("remaining_spawned=1"),
        "truncation warn was not emitted; logs={logs}"
    );
    assert_eq!(status(&pool, oldest).await, U);
    assert_eq!(status(&pool, newer).await, S);
    finish(db, pool).await;
}

#[test]
fn sweep_spawns_exactly_once_per_process() {
    let latch = AtomicBool::new(false);
    let active = claim_active(&latch).expect("first bot starts sweep");
    assert!(claim_active(&latch).is_none());
    assert!(claim_active(&latch).is_none());
    drop(active);
}

#[test]
fn spawn_wiring_claims_process_latch_before_observed_task() {
    // This is intentionally only a lexical adjacency guard. A matching token
    // in a comment or string could satisfy `.find()`; runtime task ownership is
    // covered separately by the latch tests.
    let sweep_source = include_str!("../intake_delivery_sweep.rs");
    let guard = sweep_source
        .find("claim_active(&SWEEP_ACTIVE)")
        .expect("spawn function claims the process latch");
    let task = sweep_source
        .find("task_supervisor::spawn_observed")
        .expect("spawn function registers the observed task");
    assert!(
        guard < task,
        "the process latch is claimed before task spawn"
    );
    assert_eq!(
        include_str!("../framework_setup.rs")
            .matches("spawn_intake_delivery_sweep(shared_clone.clone())")
            .count(),
        1,
        "framework setup wires exactly one sweep spawn attempt per bot"
    );
}

#[test]
fn sweep_cutoffs_do_not_panic_for_extreme_values() {
    let _ = SweepCutoffs::from_now(u64::MAX, u64::MAX);
}

#[tokio::test]
async fn sweep_task_can_restart_after_task_death() {
    let latch = AtomicBool::new(false);
    assert!(contain_tick(async { panic!("tick") }).await.is_err());
    drop(claim_active(&latch).expect("initial task"));
    assert!(
        claim_active(&latch).is_some(),
        "task exit resets the active-task latch"
    );
}

#[tokio::test]
async fn null_clock_rows_are_invisible_to_sweep_but_visible_to_operator_pg() {
    let (db, pool) = setup().await;
    sqlx::query(
        "ALTER TABLE public.intake_outbox DROP CONSTRAINT intake_outbox_dispatched_requires_clock",
    )
    .execute(&pool)
    .await
    .unwrap();
    let dispatched = seed(&pool, "null-d", D, None).await;
    let spawned = seed(&pool, "null-s", S, None).await;
    assert_eq!(
        sweep_once(&pool, READY, cutoffs(Utc::now()), 200)
            .await
            .unwrap()
            .settled,
        0
    );
    let mut tx = pool.begin().await.unwrap();
    assert!(
        settle_unknown_by_operator(&mut tx, dispatched, D, "operator")
            .await
            .unwrap()
    );
    assert!(
        settle_unknown_by_operator(&mut tx, spawned, S, "operator")
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(status(&pool, dispatched).await, U);
    assert_eq!(status(&pool, spawned).await, U);
    finish(db, pool).await;
}

async fn operator_settle_has_no_child(status_value: IntakeOutboxStatus, key: &str) {
    let (db, pool) = setup().await;
    let id = seed_clocked(&pool, key, status_value, Utc::now()).await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM public.intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        settle_unknown_by_operator(&mut tx, id, status_value, "operator")
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM public.intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((before, after, status(&pool, id).await), (1, 1, U));
    finish(db, pool).await;
}

#[tokio::test]
async fn dispatched_settle_cli_does_not_insert_a_child_row_pg() {
    operator_settle_has_no_child(D, "cli-d").await;
}

#[tokio::test]
async fn spawned_settle_cli_does_not_insert_a_child_row_pg() {
    operator_settle_has_no_child(S, "cli-s").await;
}

#[tokio::test]
async fn sweep_skips_live_interactive_turn_without_dispatch_binding_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let id = seed_clocked(&pool, "live", S, now - Duration::hours(1)).await;
    seed_inflight(&pool, "live", Some(now), None).await;
    let stats = sweep_once(&pool, READY, cutoffs(now), 200).await.unwrap();
    assert_eq!((stats.settled, stats.skipped_live), (0, 1));
    assert_eq!(status(&pool, id).await, S);
    finish(db, pool).await;
}

#[tokio::test]
async fn sweep_defers_ambiguous_live_signal_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let stale = seed_clocked(&pool, "ambiguous-stale", D, now - Duration::hours(1)).await;
    let missing = seed_clocked(&pool, "ambiguous-null", S, now - Duration::hours(1)).await;
    seed_inflight(
        &pool,
        "ambiguous-stale",
        Some(now - Duration::minutes(5)),
        Some("dispatch-live"),
    )
    .await;
    seed_inflight(&pool, "ambiguous-null", None, None).await;
    let stats = sweep_once(&pool, READY, cutoffs(now), 200).await.unwrap();
    assert_eq!((stats.settled, stats.skipped_ambiguous), (0, 2));
    assert_eq!(status(&pool, stale).await, D);
    assert_eq!(status(&pool, missing).await, S);
    finish(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_writer_serializes_before_unknown_settlement_pg() {
    for (row_status, key) in [(S, "heartbeat-race-s"), (D, "heartbeat-race-d")] {
        let (db, pool) = setup().await;
        let now = Utc::now();
        let id = seed_clocked(&pool, key, row_status, now - Duration::hours(2)).await;
        seed_inflight(&pool, key, Some(now - Duration::hours(2)), None).await;

        let mut heartbeat = pool.begin().await.unwrap();
        let heartbeat_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *heartbeat)
            .await
            .unwrap();
        sqlx::query("UPDATE public.sessions SET last_heartbeat=$2 WHERE channel_id=$1")
            .bind(key)
            .bind(Utc::now())
            .execute(&mut *heartbeat)
            .await
            .unwrap();

        let sweep_pool = pool.clone();
        let sweeping = tokio::spawn(async move {
            sweep_once(&sweep_pool, READY, cutoffs(now), 200)
                .await
                .unwrap()
        });
        wait_for_blocked_on(&pool, heartbeat_pid, 1).await;
        heartbeat.commit().await.unwrap();

        let stats = sweeping.await.unwrap();
        assert_eq!(stats.settled, 0);
        assert_eq!(status(&pool, id).await, row_status);
        finish(db, pool).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_writer_lock_timeout_skips_row_and_continues_tick_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let blocked = seed_clocked(&pool, "heartbeat-timeout", S, now - Duration::hours(2)).await;
    let next = seed_clocked(
        &pool,
        "after-heartbeat-timeout",
        S,
        now - Duration::hours(1),
    )
    .await;
    seed_inflight(
        &pool,
        "heartbeat-timeout",
        Some(now - Duration::hours(2)),
        None,
    )
    .await;

    let mut heartbeat = pool.begin().await.unwrap();
    sqlx::query("UPDATE public.sessions SET last_heartbeat=$2 WHERE channel_id=$1")
        .bind("heartbeat-timeout")
        .bind(Utc::now())
        .execute(&mut *heartbeat)
        .await
        .unwrap();

    let stats = timeout(
        TokioDuration::from_secs(crate::config::INTAKE_DELIVERY_SWEEP_LOCK_TIMEOUT_SECS + 3),
        sweep_once(&pool, READY, cutoffs(now), 200),
    )
    .await
    .expect("one writer-held session row cannot stall the sweep tick")
    .unwrap();

    assert_eq!((stats.settled, stats.skipped_ambiguous), (1, 1));
    assert_eq!(status(&pool, blocked).await, S);
    assert_eq!(status(&pool, next).await, U);
    heartbeat.rollback().await.unwrap();
    finish(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sweep_once_calls_have_one_terminal_winner_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let id = seed_clocked(&pool, "concurrent-sweeps", S, now - Duration::hours(1)).await;
    let holder = lock_intake_row(&pool, id).await;
    let first_pool = pool.clone();
    let second_pool = pool.clone();
    let first = tokio::spawn(async move {
        sweep_once(&first_pool, READY, cutoffs(now), 200)
            .await
            .unwrap()
    });
    let second = tokio::spawn(async move {
        sweep_once(&second_pool, READY, cutoffs(now), 200)
            .await
            .unwrap()
    });
    wait_for_spawned_settlement_waiters(&pool, 2).await;
    holder.rollback().await.unwrap();
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.unwrap().settled + second.unwrap().settled, 1);
    assert_eq!(status(&pool, id).await, U);
    finish(db, pool).await;
}

#[tokio::test]
async fn sweep_records_absent_basis_and_drains_fence_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let mut ids = Vec::new();
    for (label, row_status) in [("spawned", S), ("dispatched", D)] {
        for basis in ["no_session_row", "no_turn_active_row", "stale_heartbeat"] {
            let key = format!("basis-{label}-{basis}");
            ids.push((
                seed_clocked(&pool, &key, row_status, now - Duration::hours(2)).await,
                basis,
            ));
            if basis != "no_session_row" {
                // A fresh idle row must be excluded; a stale active row must be measured.
                let heartbeat = if basis == "no_turn_active_row" {
                    now
                } else {
                    now - Duration::hours(2)
                };
                seed_inflight(&pool, &key, Some(heartbeat), None).await;
                if basis == "no_turn_active_row" {
                    sqlx::query("UPDATE public.sessions SET status='idle' WHERE channel_id=$1")
                        .bind(&key)
                        .execute(&pool)
                        .await
                        .unwrap();
                }
            }
        }
    }
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = TestLogWriter {
        buffer: buffer.clone(),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::callsite::rebuild_interest_cache();
    let guard = tracing::subscriber::set_default(subscriber);
    let stats = sweep_once(&pool, READY, cutoffs(now), 200).await.unwrap();
    drop(guard);
    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert_eq!(
        (stats.settled, stats.skipped_live, stats.skipped_ambiguous),
        (6, 0, 0)
    );
    for (id, basis) in ids {
        assert_eq!(status(&pool, id).await, U);
        for phase in ["preflight", "cas"] {
            let observations = logs
                .lines()
                .filter(|line| {
                    line.split_whitespace()
                        .any(|field| field == format!("outbox_id={id}"))
                        && line.contains(&format!("absent_basis=\"{basis}\""))
                        && line.contains(&format!("phase=\"{phase}\""))
                })
                .count();
            assert_eq!(
                observations, 1,
                "both paths must record the row's absent basis; id={id}, basis={basis}, phase={phase}, logs={logs}"
            );
        }
    }
    finish(db, pool).await;
}

#[tokio::test]
async fn sweep_liveness_scope_and_boundaries_match_cas_pg() {
    let (db, pool) = setup().await;
    let now = DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap();
    let cutoff = cutoffs(now);
    let cases = [
        (
            "fresh",
            Some(cutoff.heartbeat_fresh),
            None,
            LiveSignal::Live,
        ),
        ("null", None, None, LiveSignal::Ambiguous),
        (
            "absence-equal",
            Some(cutoff.spawned),
            None,
            LiveSignal::Ambiguous,
        ),
        (
            "stale",
            Some(cutoff.spawned - Duration::seconds(1)),
            None,
            LiveSignal::Absent,
        ),
        (
            "idle-fresh",
            Some(cutoff.spawned - Duration::seconds(1)),
            Some(false),
            LiveSignal::Absent,
        ),
        (
            "active-fresh",
            Some(cutoff.spawned - Duration::seconds(1)),
            Some(true),
            LiveSignal::Live,
        ),
    ];
    for row_status in [S, D] {
        for (label, heartbeat, extra_active, expected) in cases {
            let key = format!("scope-{}-{label}", row_status.as_str());
            let id = seed_clocked(&pool, &key, row_status, now - Duration::hours(2)).await;
            seed_inflight(&pool, &key, heartbeat, None).await;
            if let Some(active) = extra_active {
                sqlx::query(
                    "INSERT INTO public.sessions(session_key,channel_id,status,last_heartbeat)
                     VALUES($1,$2,$3,$4)",
                )
                .bind(format!("extra-{key}"))
                .bind(&key)
                .bind(if active { "turn_active" } else { "idle" })
                .bind(now)
                .execute(&pool)
                .await
                .unwrap();
            }
            assert_eq!(
                live_signal(&pool, id, cutoff.heartbeat_fresh, cutoff.spawned)
                    .await
                    .unwrap(),
                expected
            );
            let mut tx = pool.begin().await.unwrap();
            let settled = if row_status == S {
                settle_spawned_unknown(&mut tx, id, cutoff.spawned, cutoff.heartbeat_fresh)
                    .await
                    .unwrap()
            } else {
                settle_dispatched_unknown(&mut tx, id, cutoff.dispatched, cutoff.heartbeat_fresh)
                    .await
                    .unwrap()
            };
            tx.commit().await.unwrap();
            assert_eq!(settled, expected == LiveSignal::Absent, "{key}");
            assert_eq!(
                status(&pool, id).await,
                if settled { U } else { row_status }
            );
        }
    }
    finish(db, pool).await;
}
