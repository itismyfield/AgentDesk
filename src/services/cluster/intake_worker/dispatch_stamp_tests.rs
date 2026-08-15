use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use chrono::Utc;

static COUNTER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn seed_dispatched(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO public.intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id,
            user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
            status, claim_owner, spawned_at, dispatched_at
         ) VALUES (
            'worker', 'leader', 'worker-cas', 'worker-cas', 'user', 'hello',
            'standard', 'agent', 'dispatched', 'dispatch-worker', NOW(), $1
         ) RETURNING id",
    )
    .bind(Utc::now())
    .fetch_one(pool)
    .await
    .expect("seed dispatched intake row")
}

fn assert_worker_false_branch_calls_classifier() {
    let source = include_str!("../intake_worker.rs");
    let start = source
        .find("let advanced = mark_done(")
        .expect("worker done writer exists");
    let end = source[start..]
        .find("Ok(TickOutcome::Processed)")
        .map(|offset| start + offset)
        .expect("worker Ok branch terminates");
    let branch = &source[start..end];
    assert_eq!(branch.matches("mark_done(").count(), 1);
    assert_eq!(branch.matches("classify_mark_done_miss(").count(), 1);
}

#[tokio::test]
async fn worker_mark_done_false_on_dispatched_is_not_a_divergence_pg() {
    let _counter_guard = COUNTER_TEST_LOCK.lock().await;
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let id = seed_dispatched(&pool).await;
    let counters = mark_done_miss_counters();
    let handoff_before = counters.stamp_handoff_observed.load(Ordering::Relaxed);
    let divergence_before = counters.divergence.load(Ordering::Relaxed);

    classify_mark_done_miss(&pool, id, "worker-cas", "worker-cas").await;

    assert_eq!(
        counters.stamp_handoff_observed.load(Ordering::Relaxed),
        handoff_before + 1
    );
    assert_eq!(
        counters.divergence.load(Ordering::Relaxed),
        divergence_before
    );
    assert_worker_false_branch_calls_classifier();

    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn worker_mark_done_false_on_missing_row_is_a_divergence_pg() {
    let _counter_guard = COUNTER_TEST_LOCK.lock().await;
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let counters = mark_done_miss_counters();
    let divergence_before = counters.divergence.load(Ordering::Relaxed);

    classify_mark_done_miss(&pool, i64::MAX, "missing", "missing").await;

    assert_eq!(
        counters.divergence.load(Ordering::Relaxed),
        divergence_before + 1
    );

    pool.close().await;
    database.drop().await;
}
