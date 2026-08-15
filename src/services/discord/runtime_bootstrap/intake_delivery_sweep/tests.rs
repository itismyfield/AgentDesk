use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox_delivery_proof::{
    settle_null_clock_dispatched, settle_null_clock_spawned, settle_unknown_by_operator,
};
use crate::db::intake_outbox_status::IntakeOutboxStatus;

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

async fn seed_inflight(pool: &PgPool, channel: &str, heartbeat: Option<DateTime<Utc>>) {
    sqlx::query(
        "INSERT INTO public.sessions(session_key,status,active_dispatch_id,last_heartbeat,channel_id)
         VALUES($1,'turn_active','dispatch-live',$2,$1)",
    )
    .bind(channel)
    .bind(heartbeat)
    .execute(pool)
    .await
    .expect("seed durable inflight signal");
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

#[tokio::test]
async fn sweep_is_bounded_and_ordered_and_logs_truncation_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let oldest = seed_clocked(&pool, "oldest", S, now - Duration::hours(2)).await;
    let newer = seed_clocked(&pool, "newer", S, now - Duration::hours(1)).await;
    let stats = sweep_once(&pool, READY, cutoffs(now), 1).await.unwrap();
    assert_eq!((stats.settled, stats.truncated_spawned), (1, 1));
    assert!(stats.truncation_logged);
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
    let clocked_d = seed_clocked(&pool, "clock-d", D, Utc::now()).await;
    let clocked_s = seed_clocked(&pool, "clock-s", S, Utc::now()).await;
    assert_eq!(
        sweep_once(&pool, READY, cutoffs(Utc::now()), 200)
            .await
            .unwrap()
            .settled,
        0
    );
    let mut tx = pool.begin().await.unwrap();
    assert!(
        settle_null_clock_dispatched(&mut tx, dispatched, "operator")
            .await
            .unwrap()
    );
    assert!(
        settle_null_clock_spawned(&mut tx, spawned, "operator")
            .await
            .unwrap()
    );
    assert!(
        !settle_null_clock_dispatched(&mut tx, clocked_d, "operator")
            .await
            .unwrap()
    );
    assert!(
        !settle_null_clock_spawned(&mut tx, clocked_s, "operator")
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
async fn sweep_skips_live_signal_pg() {
    let (db, pool) = setup().await;
    let now = Utc::now();
    let id = seed_clocked(&pool, "live", S, now - Duration::hours(1)).await;
    seed_inflight(&pool, "live", Some(now)).await;
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
    seed_inflight(&pool, "ambiguous-stale", Some(now - Duration::minutes(5))).await;
    seed_inflight(&pool, "ambiguous-null", None).await;
    let stats = sweep_once(&pool, READY, cutoffs(now), 200).await.unwrap();
    assert_eq!((stats.settled, stats.skipped_ambiguous), (0, 2));
    assert_eq!(status(&pool, stale).await, D);
    assert_eq!(status(&pool, missing).await, S);
    finish(db, pool).await;
}
