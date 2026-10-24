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

const INTAKE_WORKER: &str = include_str!("../intake_worker.rs");
const DONE_WRITE: &str = "let advanced = mark_done(";
const CLASSIFY: &str = "classify_mark_done_miss(";

/// Classification 2: from the Ok-arm done writer to its `Processed` return, the code mask
/// holds one `mark_done(` and one classifier call. A lexical tripwire, not reachability.
fn classifier_branch_gaps(source: &str) -> Vec<String> {
    let mask = crate::test_support::rust_code_mask(source).expect("intake worker lexes");
    let Some(start) = mask.find(DONE_WRITE) else {
        return vec![format!("missing {DONE_WRITE}")];
    };
    let Some(end) = mask[start..].find("Ok(TickOutcome::Processed)") else {
        return vec!["the done-writer branch never returns Processed".to_string()];
    };
    let branch = &mask[start..start + end];
    match (
        branch.matches("mark_done(").count(),
        branch.matches(CLASSIFY).count(),
    ) {
        (1, 1) => Vec::new(),
        counts => vec![format!(
            "mark_done / classifier counts {counts:?}, want (1, 1)"
        )],
    }
}

fn assert_worker_false_branch_calls_classifier() {
    assert_eq!(classifier_branch_gaps(INTAKE_WORKER), Vec::<String>::new());
}

#[test]
fn classifier_branch_guard_rejects_commented_calls() {
    assert_worker_false_branch_calls_classifier();
    for needle in [DONE_WRITE, CLASSIFY] {
        for forgery in crate::test_support::CommentForgery::ALL {
            let forged = crate::test_support::comment_out_code(INTAKE_WORKER, needle, forgery);
            assert!(
                !classifier_branch_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let note = INTAKE_WORKER.replacen(DONE_WRITE, &format!("// {CLASSIFY}\n{DONE_WRITE}"), 1);
    assert_eq!(classifier_branch_gaps(&note), Vec::<String>::new());
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
