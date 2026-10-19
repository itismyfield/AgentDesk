//! Completion against writers that add a new blocker to the same run: whichever takes
//! `aq_run:<run_id>` first wins, and a writer that loses to a completion refuses.

use std::time::{Duration, Instant};

use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};

use super::phase_gates::{PhaseGateStateWrite, save_phase_gate_state_on_pg};
use super::runs::{
    RunCompletionBlockedReason, RunCompletionOutcome, finalize_run_if_ready_on_pg_tx,
    finalize_run_if_ready_pg,
};
use super::test_support::TestPostgresDb;
use super::{
    ENTRY_STATUS_PENDING, EntryStatusUpdateOptions, update_entry_status_on_pg,
    update_entry_status_on_pg_tx,
};

const DEADLINE: Duration = Duration::from_secs(5);

async fn setup_pool(pg_db: &TestPostgresDb, run_status: &str) -> PgPool {
    let pool = pg_db.connect_and_migrate_with_max_connections(6).await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-1', 'Agent 1', 'claude', '123')",
    )
    .execute(&pool)
    .await
    .expect("seed agent");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-1', 'repo-1', 'agent-1', $1)",
    )
    .bind(run_status)
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

async fn seed_card(pool: &PgPool, card_id: &str) {
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id)
         VALUES ($1, $1, 'ready', 'agent-1', 'repo-1')",
    )
    .bind(card_id)
    .execute(pool)
    .await
    .expect("seed card");
}

async fn seed_entry(pool: &PgPool, entry_id: &str, status: &str) {
    let card_id = format!("card-{entry_id}");
    seed_card(pool, &card_id).await;
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

async fn scalar_i64(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn run_status(pool: &PgPool, run_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("run status")
}

async fn entry_status(pool: &PgPool, entry_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = $1")
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .expect("entry status")
}

async fn slot_run(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar(
        "SELECT assigned_run_id FROM auto_queue_slots WHERE agent_id = 'agent-1' AND slot_index = 0",
    )
    .fetch_one(pool)
    .await
    .expect("slot assignment")
}

/// The canonical finalizer has completed run-1 in a transaction that is still open.
async fn uncommitted_completion(pool: &PgPool) -> Transaction<'static, Postgres> {
    let mut completion = pool.begin().await.expect("begin completion");
    assert_eq!(
        finalize_run_if_ready_on_pg_tx(&mut completion, "run-1")
            .await
            .expect("complete run-1"),
        RunCompletionOutcome::Completed
    );
    completion
}

/// Opportunistic completion in its own transaction, committed whatever the outcome.
async fn try_completion(pool: &PgPool) -> RunCompletionOutcome {
    let mut completion = pool.begin().await.expect("begin completion");
    let outcome = finalize_run_if_ready_on_pg_tx(&mut completion, "run-1")
        .await
        .expect("completion attempt");
    completion
        .commit()
        .await
        .expect("commit completion attempt");
    outcome
}

/// True once a backend of this database waits on a lock; false if `task` finished first.
pub(super) async fn wait_for_lock_waiter<T>(
    pool: &PgPool,
    task: &tokio::task::JoinHandle<T>,
) -> bool {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let waiting = scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .await;
        if waiting > 0 {
            return true;
        }
        if task.is_finished() {
            return false;
        }
        assert!(
            Instant::now() < deadline,
            "writer neither waited nor finished"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn join<T>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(DEADLINE, task)
        .await
        .expect("writer finishes before the deadline")
        .expect("join writer")
}

fn reopen(
    pool: &PgPool,
) -> tokio::task::JoinHandle<Result<super::EntryStatusUpdateResult, String>> {
    let pool = pool.clone();
    tokio::spawn(async move {
        update_entry_status_on_pg(
            &pool,
            "entry-1",
            ENTRY_STATUS_PENDING,
            "manual_update",
            &EntryStatusUpdateOptions::default(),
        )
        .await
    })
}

fn save_gate(
    pool: &PgPool,
    status: &str,
    dispatch_ids: Vec<String>,
) -> tokio::task::JoinHandle<Result<super::phase_gates::PhaseGateSaveResult, String>> {
    let pool = pool.clone();
    let state = PhaseGateStateWrite {
        status: status.to_string(),
        dispatch_ids,
        ..Default::default()
    };
    tokio::spawn(async move { save_phase_gate_state_on_pg(&pool, "run-1", 0, &state).await })
}

/// The completion held: run completed, its slot released and announced once, no blocker added.
async fn assert_completed_without_new_blocker(pool: &PgPool) {
    assert_eq!(run_status(pool, "run-1").await, "completed");
    assert_eq!(slot_run(pool).await, None);
    assert_eq!(
        scalar_i64(pool, "SELECT COUNT(*)::BIGINT FROM message_outbox").await,
        1
    );
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_entries
             WHERE run_id = 'run-1' AND status IN ('pending', 'dispatched', 'user_cancelled')",
        )
        .await,
        0
    );
    assert_eq!(
        scalar_i64(
            pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates WHERE run_id = 'run-1'",
        )
        .await,
        0
    );
}

/// Contract: reopening a failed/skipped entry while a completion of its run is still
/// uncommitted waits for that completion, then refuses instead of reviving the run's work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_reopen_waits_for_uncommitted_completion_and_refuses_pg() {
    for from_status in ["failed", "skipped"] {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db, "active").await;
        seed_entry(&pool, "entry-1", from_status).await;
        let completion = uncommitted_completion(&pool).await;
        let writer = reopen(&pool);
        let waited = wait_for_lock_waiter(&pool, &writer).await;
        completion.commit().await.expect("commit completion");

        let result = join(writer).await;
        assert_completed_without_new_blocker(&pool).await;
        let error = result.expect_err("reopen after completion is refused");
        assert!(
            error.contains("run is completed: refusing to reopen"),
            "{error}"
        );
        assert_eq!(entry_status(&pool, "entry-1").await, from_status);
        assert!(
            waited,
            "{from_status} reopen did not wait for the completion"
        );
        pool.close().await;
        pg_db.drop().await;
    }
}

/// Contract: a completion arriving while a reopen holds the run token defers instead of
/// completing past it, and the next attempt sees the reopened entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_defers_behind_entry_reopen_holding_the_token_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    seed_entry(&pool, "entry-1", "failed").await;
    let mut row_holder = pool.begin().await.expect("begin row holder");
    sqlx::query("SELECT id FROM auto_queue_entries WHERE id = 'entry-1' FOR UPDATE")
        .execute(&mut *row_holder)
        .await
        .expect("hold entry row");
    let writer = reopen(&pool);
    assert!(wait_for_lock_waiter(&pool, &writer).await);

    let first = try_completion(&pool).await;
    row_holder.rollback().await.expect("release entry row");
    let reopened = join(writer).await.expect("reopen commits");
    assert_eq!(
        first,
        RunCompletionOutcome::Blocked(RunCompletionBlockedReason::TokenContended)
    );
    assert!(reopened.changed);
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("retry completion"),
        RunCompletionOutcome::Blocked(RunCompletionBlockedReason::RunnableEntry)
    );
    assert_eq!(run_status(&pool, "run-1").await, "active");
    assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
    assert_eq!(entry_status(&pool, "entry-1").await, "pending");
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: the transaction-scoped reopen used by restore refuses a completed run too.
#[tokio::test]
async fn tx_entry_reopen_refuses_completed_run_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    seed_entry(&pool, "entry-1", "skipped").await;
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("complete run-1"),
        RunCompletionOutcome::Completed
    );
    let mut tx = pool.begin().await.expect("begin reopen");
    let result = update_entry_status_on_pg_tx(
        &mut tx,
        "entry-1",
        ENTRY_STATUS_PENDING,
        "restore_run_pending",
        &EntryStatusUpdateOptions::default(),
    )
    .await;
    tx.commit().await.expect("commit reopen transaction");
    assert_completed_without_new_blocker(&pool).await;
    let error = result.expect_err("tx reopen of a completed run is refused");
    assert!(
        error.contains("run is completed: refusing to reopen"),
        "{error}"
    );
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: a dispatch-free pending/failed gate save waits for an uncommitted completion
/// of its run and then refuses to leave a gate on the completed run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entryless_gate_save_waits_for_uncommitted_completion_and_refuses_pg() {
    for gate_status in ["pending", "failed"] {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db, "active").await;
        seed_entry(&pool, "entry-done", "failed").await;
        let completion = uncommitted_completion(&pool).await;
        let writer = save_gate(&pool, gate_status, Vec::new());
        let waited = wait_for_lock_waiter(&pool, &writer).await;
        completion.commit().await.expect("commit completion");

        let result = join(writer).await;
        assert_completed_without_new_blocker(&pool).await;
        let error = result.expect_err("gate save after completion is refused");
        assert!(
            error.contains("run is completed: refusing to save"),
            "{error}"
        );
        assert!(
            waited,
            "{gate_status} gate save did not wait for the completion"
        );
        pool.close().await;
        pg_db.drop().await;
    }
}

/// Contract: a completion arriving while a gate save holds the run token defers, and the
/// next attempt sees the saved gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_defers_behind_gate_save_holding_the_token_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    seed_entry(&pool, "entry-done", "failed").await;
    let mut phase_holder = pool.begin().await.expect("begin phase-key holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('run-1'), hashtext('0'))")
        .execute(&mut *phase_holder)
        .await
        .expect("hold phase key");
    let writer = save_gate(&pool, "pending", Vec::new());
    assert!(wait_for_lock_waiter(&pool, &writer).await);

    let first = try_completion(&pool).await;
    phase_holder.rollback().await.expect("release phase key");
    join(writer).await.expect("gate save commits");
    assert_eq!(
        first,
        RunCompletionOutcome::Blocked(RunCompletionBlockedReason::TokenContended)
    );
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("retry completion"),
        RunCompletionOutcome::Blocked(RunCompletionBlockedReason::BlockingPhaseGate)
    );
    assert_eq!(run_status(&pool, "run-1").await, "active");
    assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: a gate save carrying only a terminal dispatch is a new blocker too; it may not
/// attach a failed gate to a run that has already completed.
#[tokio::test]
async fn terminal_dispatch_gate_save_refuses_completed_run_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    seed_entry(&pool, "entry-done", "failed").await;
    sqlx::query(
        "INSERT INTO task_dispatches
            (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context)
         VALUES ('dispatch-done', 'card-entry-done', 'agent-1', 'phase-gate', 'completed',
                 'Gate Dispatch', '{}')",
    )
    .execute(&pool)
    .await
    .expect("seed terminal dispatch");
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-1")
            .await
            .expect("complete run-1"),
        RunCompletionOutcome::Completed
    );
    let result = join(save_gate(
        &pool,
        "failed",
        vec!["dispatch-done".to_string()],
    ))
    .await;
    assert_completed_without_new_blocker(&pool).await;
    let error = result.expect_err("terminal-dispatch gate save is refused");
    assert!(
        error.contains("run is completed: refusing to save"),
        "{error}"
    );
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: a cancelled run keeps admitting operator reopen and a failed gate, so the
/// cancel → restore flow is unchanged; only `completed` refuses.
#[tokio::test]
async fn cancelled_run_still_admits_reopen_and_failed_gate_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "cancelled").await;
    seed_entry(&pool, "entry-1", "failed").await;
    let reopened = join(reopen(&pool))
        .await
        .expect("reopen on a cancelled run");
    assert!(reopened.changed);
    join(save_gate(&pool, "failed", Vec::new()))
        .await
        .expect("failed gate on a cancelled run");
    assert_eq!(run_status(&pool, "run-1").await, "cancelled");
    assert_eq!(entry_status(&pool, "entry-1").await, "pending");
    pool.close().await;
    pg_db.drop().await;
}

/// Contract: a gate save waiting on the phase key holds no run row lock, so a holder of the
/// phase key that then updates the run row (gate reconciliation's order) never deadlocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_save_waiting_on_phase_key_holds_no_run_row_lock_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    let mut phase_holder = pool.begin().await.expect("begin phase-key holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('run-1'), hashtext('0'))")
        .execute(&mut *phase_holder)
        .await
        .expect("hold phase key");
    let writer = save_gate(&pool, "pending", Vec::new());
    assert!(wait_for_lock_waiter(&pool, &writer).await);
    tokio::time::timeout(
        DEADLINE,
        sqlx::query(
            "UPDATE auto_queue_runs SET status = 'paused' WHERE id = 'run-1' AND status = 'active'",
        )
        .execute(&mut *phase_holder),
    )
    .await
    .expect("run row is not held by the waiting save")
    .expect("pause run under the phase key");
    phase_holder.commit().await.expect("commit pause");
    join(writer)
        .await
        .expect("gate save commits after the phase key");
    assert_eq!(run_status(&pool, "run-1").await, "paused");
    pool.close().await;
    pg_db.drop().await;
}

fn policy_engine(pool: &PgPool) -> crate::engine::PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
        .expect("create policy engine")
}

fn begin_grace(
    engine: &crate::engine::PolicyEngine,
    run_id: &'static str,
) -> tokio::task::JoinHandle<String> {
    let engine = engine.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .eval_js::<String>(&format!(
                r#"String(require("./lib/auto-queue-phase-gate").beginPhaseGateGraceWindow("{run_id}"))"#
            ))
            .expect("run beginPhaseGateGraceWindow")
    })
}

/// Contract: the policy grace writer, racing an uncommitted completion of the same run row,
/// matches no row once the completion commits and reports it; before completion it blocks it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grace_writer_skips_run_completed_meanwhile_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    seed_entry(&pool, "entry-done", "failed").await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-2', 'repo-1', 'agent-1', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed live run");
    let engine = policy_engine(&pool);
    let completion = uncommitted_completion(&pool).await;
    let writer = begin_grace(&engine, "run-1");
    let waited = wait_for_lock_waiter(&pool, &writer).await;
    completion.commit().await.expect("commit completion");
    let opened = join(writer).await;
    assert_completed_without_new_blocker(&pool).await;
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_runs
             WHERE id = 'run-1' AND phase_gate_grace_until IS NOT NULL",
        )
        .await,
        0
    );
    assert_eq!(opened, "false");
    assert!(waited, "grace writer did not wait for the run row");

    assert_eq!(join(begin_grace(&engine, "run-2")).await, "true");
    assert_eq!(
        finalize_run_if_ready_pg(&pool, "run-2")
            .await
            .expect("completion inside grace"),
        RunCompletionOutcome::Blocked(RunCompletionBlockedReason::PhaseGateGraceWindow)
    );
    // Like the other policy-engine PG tests, drop the database without closing the pool.
    drop(engine);
    pg_db.drop().await;
}

/// Contract: a CLI-created run becomes visible only together with its first entry, so an
/// empty-run completion can never finish it before the entry lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_cli_run_is_published_with_its_first_entry_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db, "active").await;
    seed_card(&pool, "card-cli").await;
    let mut card_holder = pool.begin().await.expect("begin card holder");
    sqlx::query("SELECT id FROM kanban_cards WHERE id = 'card-cli' FOR UPDATE")
        .execute(&mut *card_holder)
        .await
        .expect("hold card row");
    let writer_pool = pool.clone();
    let writer = tokio::spawn(async move {
        crate::cli::direct::auto_queue_add::add_card_to_new_run_pg(
            &writer_pool,
            &crate::cli::direct::auto_queue_add::NewRunEntry {
                repo_id: Some("repo-1"),
                agent_id: "agent-1",
                card_id: "card-cli",
                card_status: "ready",
                priority: None,
                thread_group: None,
                phase: None,
            },
        )
        .await
    });
    assert!(wait_for_lock_waiter(&pool, &writer).await);

    let visible = sqlx::query_scalar::<_, String>(
        "SELECT id FROM auto_queue_runs WHERE ai_model = 'manual-cli'",
    )
    .fetch_all(&pool)
    .await
    .expect("list visible CLI runs");
    for run_id in &visible {
        finalize_run_if_ready_pg(&pool, run_id)
            .await
            .expect("empty-run completion attempt");
    }
    card_holder.rollback().await.expect("release card row");
    let added: Value = join(writer).await.expect("CLI add commits");
    let run_id = added["run_id"].as_str().expect("new run id").to_string();
    assert_eq!(run_status(&pool, &run_id).await, "pending");
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*)::BIGINT FROM auto_queue_entries
             WHERE kanban_card_id = 'card-cli' AND status = 'pending'",
        )
        .await,
        1
    );
    assert!(
        visible.is_empty(),
        "run {visible:?} was visible before its entry"
    );
    pool.close().await;
    pg_db.drop().await;
}
