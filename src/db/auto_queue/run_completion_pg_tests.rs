//! Acceptance tests for the single derived run-completion writer: readiness
//! blockers, transactional slot release and notification, idempotency.

use super::runs::{
    RunCompletionBlockedReason, RunCompletionOutcome, acquire_run_advisory_xact_lock_on_pg_tx,
    finalize_run_if_ready_on_pg_tx, finalize_run_if_ready_pg,
};
use super::test_support::TestPostgresDb;
use super::{ENTRY_STATUS_SKIPPED, EntryStatusUpdateOptions, update_entry_status_on_pg};
use sqlx::PgPool;

async fn setup_pool(pg_db: &TestPostgresDb) -> PgPool {
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-1', 'Agent 1', 'claude', '123')",
    )
    .execute(&pool)
    .await
    .expect("seed agent");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-1', 'repo-1', 'agent-1', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed run");
    sqlx::query(
        "INSERT INTO auto_queue_slots
            (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
         VALUES ('agent-1', 0, 'run-1', 0, CAST('{}' AS jsonb))",
    )
    .execute(&pool)
    .await
    .expect("seed slot");
    pool
}

async fn seed_entry(pool: &PgPool, entry_id: &str, status: &str) {
    let card_id = format!("card-{entry_id}");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ($1, $1, 'in_progress', 'agent-1')",
    )
    .bind(&card_id)
    .execute(pool)
    .await
    .expect("seed card");
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status)
         VALUES ($1, 'run-1', $2, 'agent-1', $3)",
    )
    .bind(entry_id)
    .bind(&card_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed entry");
}

async fn run_status(pool: &PgPool, run_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("run status")
}

async fn slot_run(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar(
        "SELECT assigned_run_id FROM auto_queue_slots WHERE agent_id = 'agent-1' AND slot_index = 0",
    )
    .fetch_one(pool)
    .await
    .expect("slot assignment")
}

async fn outbox_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM message_outbox")
        .fetch_one(pool)
        .await
        .expect("outbox count")
}

async fn gate_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates WHERE run_id = 'run-1'",
    )
    .fetch_one(pool)
    .await
    .expect("gate count")
}

async fn set_grace(pool: &PgPool, offset_seconds: f64) {
    sqlx::query(
        "UPDATE auto_queue_runs
         SET phase_gate_grace_until = NOW() + make_interval(secs => $1)
         WHERE id = 'run-1'",
    )
    .bind(offset_seconds)
    .execute(pool)
    .await
    .expect("set grace window");
}

fn blocked(reason: RunCompletionBlockedReason) -> RunCompletionOutcome {
    RunCompletionOutcome::Blocked(reason)
}

/// Contract: a run holding a `user_cancelled` entry never completes, even when a
/// sibling entry later reaches a terminal state through the entry writer.
#[tokio::test]
async fn user_cancelled_entry_blocks_completion_after_sibling_terminal_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-held", "user_cancelled").await;
    seed_entry(&pool, "entry-sibling", "pending").await;

    let transition = update_entry_status_on_pg(
        &pool,
        "entry-sibling",
        ENTRY_STATUS_SKIPPED,
        "test_sibling_terminal",
        &EntryStatusUpdateOptions::default(),
    )
    .await
    .expect("skip sibling entry");
    assert!(transition.changed);
    assert_eq!(run_status(&pool, "run-1").await, "active");
    assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
    assert_eq!(outbox_count(&pool).await, 0);

    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("explicit completion attempt"),
        blocked(RunCompletionBlockedReason::UserCancelledEntry)
    );
    assert_eq!(run_status(&pool, "run-1").await, "active");

    let released = update_entry_status_on_pg(
        &pool,
        "entry-held",
        ENTRY_STATUS_SKIPPED,
        "test_operator_skip",
        &EntryStatusUpdateOptions::default(),
    )
    .await
    .expect("operator skips held entry");
    assert!(released.changed);
    assert_eq!(run_status(&pool, "run-1").await, "completed");
    assert_eq!(slot_run(&pool).await, None);
    assert_eq!(outbox_count(&pool).await, 1);

    pool.close().await;
    pg_db.drop().await;
}

/// Contract: an open phase-gate grace window defers both the explicit and the
/// terminal-entry completion paths; completion follows once it expires.
#[tokio::test]
async fn grace_window_blocks_every_derived_entry_point_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-done", "failed").await;
    seed_entry(&pool, "entry-last", "pending").await;
    set_grace(&pool, 30.0).await;

    let transition = update_entry_status_on_pg(
        &pool,
        "entry-last",
        ENTRY_STATUS_SKIPPED,
        "test_terminal_in_grace",
        &EntryStatusUpdateOptions::default(),
    )
    .await
    .expect("skip last entry");
    assert!(transition.changed);
    assert_eq!(run_status(&pool, "run-1").await, "active");
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("completion in grace"),
        blocked(RunCompletionBlockedReason::PhaseGateGraceWindow)
    );
    assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
    assert_eq!(outbox_count(&pool).await, 0);

    set_grace(&pool, -1.0).await;
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("completion after grace"),
        RunCompletionOutcome::Completed
    );
    assert_eq!(run_status(&pool, "run-1").await, "completed");
    assert_eq!(slot_run(&pool).await, None);
    assert_eq!(outbox_count(&pool).await, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[derive(Clone)]
struct CompletionBarrier {
    point: &'static str,
    reached: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
}

tokio::task_local! {
    static COMPLETION_BARRIER: CompletionBarrier;
}

pub(super) async fn pause_completion_at(point: &str) {
    if let Ok(barrier) = COMPLETION_BARRIER.try_with(Clone::clone)
        && barrier.point == point
    {
        barrier.reached.notify_one();
        barrier.resume.notified().await;
    }
}

async fn paused_completion(
    pool: &PgPool,
    point: &'static str,
) -> (
    tokio::task::JoinHandle<Result<RunCompletionOutcome, String>>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let barrier = CompletionBarrier {
        point,
        reached: Default::default(),
        resume: Default::default(),
    };
    let writer_pool = pool.clone();
    let writer_barrier = barrier.clone();
    let writer = tokio::spawn(COMPLETION_BARRIER.scope(writer_barrier, async move {
        finalize_run_if_ready_pg(&writer_pool, "run-1").await
    }));
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        barrier.reached.notified(),
    )
    .await
    .expect("writer reaches readiness barrier");
    (writer, barrier.resume)
}

async fn assert_completion_blocked(
    pool: &PgPool,
    writer: tokio::task::JoinHandle<Result<RunCompletionOutcome, String>>,
    resume: std::sync::Arc<tokio::sync::Notify>,
    reason: RunCompletionBlockedReason,
) {
    resume.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), writer)
        .await
        .expect("writer finishes after the barrier opens")
        .expect("join writer")
        .expect("writer result");
    assert_eq!(outcome, blocked(reason));
    assert_eq!(run_status(pool, "run-1").await, "active");
    assert_eq!(slot_run(pool).await.as_deref(), Some("run-1"));
    assert_eq!(outbox_count(pool).await, 0);
}

// A separate connection commits grace after SELECT while the finalizer holds the run token.
#[tokio::test]
async fn grace_opened_after_readiness_read_blocks_completion_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-done", "failed").await;
    let (writer, resume) = paused_completion(&pool, "grace_read").await;
    set_grace(&pool, 30.0).await;
    assert_completion_blocked(
        &pool,
        writer,
        resume,
        RunCompletionBlockedReason::PhaseGateGraceWindow,
    )
    .await;
    pool.close().await;
    pg_db.drop().await;
}

// The policy grace writer commits on the run row after the finalizer read it; the final
// UPDATE re-reads grace and refuses, and completion follows once the window lapses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grace_writer_committed_after_grace_read_blocks_completion_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-done", "failed").await;
    let engine = super::run_admission_pg_tests::policy_engine(&pool);
    let (completion, resume) = paused_completion(&pool, "grace_read").await;
    let opened = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        super::run_admission_pg_tests::begin_grace(&engine, "run-1"),
    )
    .await
    .expect("grace writer commits while the finalizer is paused")
    .expect("join grace writer");
    assert_eq!(opened, "true");
    let graced: bool = sqlx::query_scalar(
        "SELECT phase_gate_grace_until > NOW() FROM auto_queue_runs WHERE id = 'run-1'",
    )
    .fetch_one(&pool)
    .await
    .expect("read grace window");
    assert!(graced, "grace writer left no open window");
    assert_completion_blocked(
        &pool,
        completion,
        resume,
        RunCompletionBlockedReason::PhaseGateGraceWindow,
    )
    .await;

    set_grace(&pool, -1.0).await;
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("completion after grace lapses"),
        RunCompletionOutcome::Completed
    );
    assert_eq!(run_status(&pool, "run-1").await, "completed");
    assert_eq!(slot_run(&pool).await, None);
    assert_eq!(outbox_count(&pool).await, 1);
    // Like the other policy-engine PG tests, drop the database without closing the pool.
    drop(engine);
    pg_db.drop().await;
}

/// Let a completion paused at `point` finish once `writer` waits on the run token.
async fn completion_after_writer_waits<T>(
    pool: &PgPool,
    point: &'static str,
    start_writer: impl FnOnce(PgPool) -> tokio::task::JoinHandle<T>,
) -> (RunCompletionOutcome, T, bool) {
    let (completion, resume) = paused_completion(pool, point).await;
    let writer = start_writer(pool.clone());
    let waited = super::run_admission_pg_tests::wait_for_lock_waiter(pool, &writer).await;
    resume.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), completion)
        .await
        .expect("completion finishes after the barrier opens")
        .expect("join completion")
        .expect("completion result");
    let written = tokio::time::timeout(std::time::Duration::from_secs(5), writer)
        .await
        .expect("writer finishes after the completion commits")
        .expect("join writer");
    (outcome, written, waited)
}

// A gate save started while the finalizer holds the run token waits for the completion and
// then refuses the completed run, so no gate commits between readiness read and UPDATE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_saved_after_readiness_read_blocks_completion_pg() {
    for status in ["pending", "failed"] {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        seed_entry(&pool, "entry-done", "failed").await;
        let (outcome, saved, waited) =
            completion_after_writer_waits(&pool, "readiness_read", |pool| {
                tokio::spawn(async move {
                    super::phase_gates::save_phase_gate_state_on_pg(
                        &pool,
                        "run-1",
                        0,
                        &super::phase_gates::PhaseGateStateWrite {
                            status: status.to_string(),
                            ..Default::default()
                        },
                    )
                    .await
                })
            })
            .await;
        assert_eq!(outcome, RunCompletionOutcome::Completed);
        let error = saved.expect_err("gate save after the completion is refused");
        assert!(
            error.contains("run is completed: refusing to save"),
            "{error}"
        );
        assert_eq!(gate_count(&pool).await, 0);
        assert_eq!(run_status(&pool, "run-1").await, "completed");
        assert!(waited, "{status} gate save did not wait for the run token");
        pool.close().await;
        pg_db.drop().await;
    }
}

// With grace and a pending gate both opened mid-completion, the refusal reports
// the gate first, matching the readiness check order.
#[tokio::test]
async fn gate_and_grace_opened_after_readiness_read_report_gate_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-done", "failed").await;
    let (writer, resume) = paused_completion(&pool, "readiness_read").await;
    set_grace(&pool, 30.0).await;
    // Gate saves now wait for the run token; a raw insert stands in for a token-free writer.
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (run_id, phase, status) VALUES ('run-1', 0, 'pending')",
    )
    .execute(&pool)
    .await
    .expect("commit phase gate outside the token");
    assert_completion_blocked(
        &pool,
        writer,
        resume,
        RunCompletionBlockedReason::BlockingPhaseGate,
    )
    .await;
    // Grace is still open behind the gate: clearing the gate exposes it.
    assert!(
        super::phase_gates::clear_phase_gate_state_on_pg(&pool, "run-1", 0)
            .await
            .expect("clear phase gate through its writer")
    );
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("completion after gate clears"),
        blocked(RunCompletionBlockedReason::PhaseGateGraceWindow)
    );
    pool.close().await;
    pg_db.drop().await;
}

// Reopening a terminal entry waits for the run token held by the finalizer, then refuses
// the completed run instead of leaving a pending entry behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_reopened_after_readiness_read_blocks_completion_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-done", "failed").await;
    let (outcome, reopened, waited) =
        completion_after_writer_waits(&pool, "readiness_read", |pool| {
            tokio::spawn(async move {
                update_entry_status_on_pg(
                    &pool,
                    "entry-done",
                    super::ENTRY_STATUS_PENDING,
                    "test_reopen",
                    &EntryStatusUpdateOptions::default(),
                )
                .await
            })
        })
        .await;
    assert_eq!(outcome, RunCompletionOutcome::Completed);
    let error = reopened.expect_err("reopen after the completion is refused");
    assert!(
        error.contains("run is completed: refusing to reopen"),
        "{error}"
    );
    assert_eq!(run_status(&pool, "run-1").await, "completed");
    assert_eq!(slot_run(&pool).await, None);
    assert!(waited, "reopen did not wait for the run token");
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: pending and failed gates block derived completion and are never
/// deleted by it; only the operator end command may remove them.
#[tokio::test]
async fn blocking_phase_gates_refuse_derived_completion_and_keep_rows_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-done", "failed").await;
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (run_id, phase, status)
         VALUES ('run-1', 0, 'pending')",
    )
    .execute(&pool)
    .await
    .expect("seed pending gate");

    for gate_status in ["pending", "failed"] {
        sqlx::query("UPDATE auto_queue_phase_gates SET status = $1 WHERE run_id = 'run-1'")
            .bind(gate_status)
            .execute(&pool)
            .await
            .expect("set gate status");
        assert_eq!(
            finalize_run_if_ready_pg(&pool, "run-1")
                .await
                .expect("gated completion"),
            blocked(RunCompletionBlockedReason::BlockingPhaseGate),
            "{gate_status} gate must block"
        );
        assert_eq!(
            gate_count(&pool).await,
            1,
            "{gate_status} gate must survive"
        );
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
    }
    assert_eq!(outbox_count(&pool).await, 0);

    pool.close().await;
    pg_db.drop().await;
}

/// Contract: run status, slot release and the completion notification commit
/// together; a failed notification leaves the slot owned and the run active.
#[tokio::test]
async fn completion_rolls_back_slot_release_when_notification_fails_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-done", "failed").await;
    sqlx::query(
        "CREATE FUNCTION reject_outbox_insert() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'outbox rejected by test'; END $$ LANGUAGE plpgsql",
    )
    .execute(&pool)
    .await
    .expect("create reject function");
    sqlx::query(
        "CREATE TRIGGER reject_outbox BEFORE INSERT ON message_outbox
         FOR EACH ROW EXECUTE FUNCTION reject_outbox_insert()",
    )
    .execute(&pool)
    .await
    .expect("install reject trigger");

    let error = finalize_run_if_ready_pg(&pool, "run-1")
        .await
        .expect_err("notification failure must fail completion");
    assert!(error.contains("outbox rejected by test"), "{error}");
    assert_eq!(run_status(&pool, "run-1").await, "active");
    assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
    assert_eq!(outbox_count(&pool).await, 0);

    sqlx::query("DROP TRIGGER reject_outbox ON message_outbox")
        .execute(&pool)
        .await
        .expect("drop reject trigger");
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("completion after trigger removal"),
        RunCompletionOutcome::Completed
    );
    assert_eq!(run_status(&pool, "run-1").await, "completed");
    assert_eq!(slot_run(&pool).await, None);
    assert_eq!(outbox_count(&pool).await, 1);

    pool.close().await;
    pg_db.drop().await;
}

/// Contract: a completion waiting behind the run token observes the committed
/// terminal state, and repeated calls queue the notification exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_completion_waits_for_token_and_sees_terminal_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    seed_entry(&pool, "entry-done", "failed").await;

    let mut holder = pool.begin().await.expect("begin token holder");
    acquire_run_advisory_xact_lock_on_pg_tx(&mut holder, "run-1")
        .await
        .expect("hold run token");

    let waiter_pool = pool.clone();
    let waiter = tokio::spawn(async move { finalize_run_if_ready_pg(&waiter_pool, "run-1").await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let waiting = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM pg_locks WHERE locktype = 'advisory' AND NOT granted",
        )
        .fetch_one(&pool)
        .await
        .expect("inspect advisory waiters");
        if waiting > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "second completion never waited for the run token"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(
        finalize_run_if_ready_on_pg_tx(&mut holder, "run-1")
            .await
            .expect("token holder completes"),
        RunCompletionOutcome::Completed
    );
    holder.commit().await.expect("commit holder completion");

    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("waiter finishes after commit")
        .expect("join waiter")
        .expect("waiter result");
    assert_eq!(waited, RunCompletionOutcome::AlreadyTerminal);
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("third completion"),
        RunCompletionOutcome::AlreadyTerminal
    );
    assert_eq!(run_status(&pool, "run-1").await, "completed");
    assert_eq!(slot_run(&pool).await, None);
    assert_eq!(outbox_count(&pool).await, 1);

    pool.close().await;
    pg_db.drop().await;
}

/// Contract: a never-populated run completes silently, and a run outside the
/// completable statuses is reported as blocked rather than completed.
#[tokio::test]
async fn empty_run_completes_silently_and_restoring_run_is_blocked_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-empty', 'repo-1', 'agent-1', 'pending')",
    )
    .execute(&pool)
    .await
    .expect("seed empty run");
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-empty")
            .await
            .expect("complete empty run"),
        RunCompletionOutcome::Completed
    );
    assert_eq!(run_status(&pool, "run-empty").await, "completed");
    assert_eq!(outbox_count(&pool).await, 0);

    seed_entry(&pool, "entry-done", "failed").await;
    sqlx::query("UPDATE auto_queue_runs SET status = 'restoring' WHERE id = 'run-1'")
        .execute(&pool)
        .await
        .expect("enter restore window");
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("completion during restore"),
        blocked(RunCompletionBlockedReason::RunStatusNotCompletable)
    );
    assert_eq!(run_status(&pool, "run-1").await, "restoring");
    assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-missing")
            .await
            .expect("completion of unknown run"),
        blocked(RunCompletionBlockedReason::RunNotFound)
    );

    pool.close().await;
    pg_db.drop().await;
}
