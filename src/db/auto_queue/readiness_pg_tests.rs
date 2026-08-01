use super::{
    ForceRunCompletionCommand, RunCompletionBlockedReason as Blocked,
    RunReadinessOutcome as Outcome, finalize_run_if_ready_on_pg, force_complete_run_on_pg,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use sqlx::{PgPool, Row};

async fn setup_pool() -> (TestPostgresDb, PgPool) {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('readiness-agent', 'Readiness Agent', 'claude', '123456789')",
    )
    .execute(&pool)
    .await
    .expect("seed readiness agent");
    (pg_db, pool)
}

async fn seed_run_with_slot(pool: &PgPool, run_id: &str) {
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, 'repo/readiness', 'readiness-agent', 'active')",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .expect("seed readiness run");
    sqlx::query(
        "INSERT INTO auto_queue_slots
            (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
         VALUES ('readiness-agent', 0, $1, 0, '{}'::jsonb)
         ON CONFLICT (agent_id, slot_index) DO UPDATE
         SET assigned_run_id = EXCLUDED.assigned_run_id,
             assigned_thread_group = EXCLUDED.assigned_thread_group",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .expect("seed readiness slot");
}

async fn run_status(pool: &PgPool, run_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("load run status")
}

async fn assigned_run(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar(
        "SELECT assigned_run_id FROM auto_queue_slots
         WHERE agent_id = 'readiness-agent' AND slot_index = 0",
    )
    .fetch_one(pool)
    .await
    .expect("load assigned run")
}

async fn outbox_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM message_outbox")
        .fetch_one(pool)
        .await
        .expect("count completion outbox")
}

#[tokio::test]
async fn user_cancelled_with_other_terminal_entry_blocks_readiness_pg() {
    let (pg_db, pool) = setup_pool().await;
    seed_run_with_slot(&pool, "run-user-cancelled").await;
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, agent_id, status)
         VALUES ('entry-user-cancelled', 'run-user-cancelled', 'readiness-agent', 'user_cancelled'),
                ('entry-terminal', 'run-user-cancelled', 'readiness-agent', 'failed')",
    )
    .execute(&pool)
    .await
    .expect("seed user-cancelled and terminal entries");

    let outcome = finalize_run_if_ready_on_pg(&pool, "run-user-cancelled")
        .await
        .expect("evaluate user-cancelled run");

    assert_eq!(outcome, Outcome::Blocked(Blocked::UserCancelledEntry));
    assert_eq!(run_status(&pool, "run-user-cancelled").await, "active");
    assert_eq!(
        assigned_run(&pool).await.as_deref(),
        Some("run-user-cancelled")
    );
    assert_eq!(outbox_count(&pool).await, 0);
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn phase_gate_grace_blocks_readiness_pg() {
    let (pg_db, pool) = setup_pool().await;
    seed_run_with_slot(&pool, "run-grace").await;
    sqlx::query(
        "UPDATE auto_queue_runs
         SET phase_gate_grace_until = NOW() + INTERVAL '5 minutes'
         WHERE id = 'run-grace'",
    )
    .execute(&pool)
    .await
    .expect("seed phase-gate grace");

    let outcome = finalize_run_if_ready_on_pg(&pool, "run-grace")
        .await
        .expect("evaluate grace-held run");

    assert_eq!(outcome, Outcome::Blocked(Blocked::PhaseGateGraceWindow));
    assert_eq!(run_status(&pool, "run-grace").await, "active");
    assert_eq!(assigned_run(&pool).await.as_deref(), Some("run-grace"));
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pending_and_failed_phase_gates_block_readiness_pg() {
    let (pg_db, pool) = setup_pool().await;
    for (phase, gate_status) in [(1_i32, "pending"), (2_i32, "failed")] {
        let run_id = format!("run-gate-{gate_status}");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, status)
             VALUES ($1, 'active')",
        )
        .bind(&run_id)
        .execute(&pool)
        .await
        .expect("seed gate-held run");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (run_id, phase, status)
             VALUES ($1, $2, $3)",
        )
        .bind(&run_id)
        .bind(phase)
        .bind(gate_status)
        .execute(&pool)
        .await
        .expect("seed blocking phase gate");

        let outcome = finalize_run_if_ready_on_pg(&pool, &run_id)
            .await
            .expect("evaluate gate-held run");
        assert_eq!(
            outcome,
            Outcome::Blocked(Blocked::BlockingPhaseGate),
            "{gate_status} gate must block completion"
        );
        assert_eq!(run_status(&pool, &run_id).await, "active");
    }
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn runnable_entry_blocks_readiness_pg() {
    let (pg_db, pool) = setup_pool().await;
    seed_run_with_slot(&pool, "run-runnable").await;
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, agent_id, status)
         VALUES ('entry-runnable', 'run-runnable', 'readiness-agent', 'pending')",
    )
    .execute(&pool)
    .await
    .expect("seed runnable entry");

    let outcome = finalize_run_if_ready_on_pg(&pool, "run-runnable")
        .await
        .expect("evaluate runnable run");
    assert_eq!(outcome, Outcome::Blocked(Blocked::RunnableEntry));
    assert_eq!(run_status(&pool, "run-runnable").await, "active");
    assert_eq!(assigned_run(&pool).await.as_deref(), Some("run-runnable"));
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn completion_updates_status_slot_and_outbox_once_pg() {
    let (pg_db, pool) = setup_pool().await;
    seed_run_with_slot(&pool, "run-complete").await;
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, agent_id, status)
         VALUES ('entry-complete', 'run-complete', 'readiness-agent', 'skipped')",
    )
    .execute(&pool)
    .await
    .expect("seed terminal entry");

    let first = finalize_run_if_ready_on_pg(&pool, "run-complete")
        .await
        .expect("complete ready run");
    assert_eq!(first, Outcome::Completed);
    assert_eq!(run_status(&pool, "run-complete").await, "completed");
    assert_eq!(assigned_run(&pool).await, None);
    assert_eq!(outbox_count(&pool).await, 1);

    let second = finalize_run_if_ready_on_pg(&pool, "run-complete")
        .await
        .expect("repeat completion");
    assert_eq!(second, Outcome::AlreadyTerminal);
    assert_eq!(outbox_count(&pool).await, 1);
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn completion_notification_failure_rolls_back_status_and_slot_pg() {
    let (pg_db, pool) = setup_pool().await;
    seed_run_with_slot(&pool, "run-rollback").await;
    sqlx::query(
        "CREATE FUNCTION reject_completion_outbox() RETURNS TRIGGER AS $$
         BEGIN RAISE EXCEPTION 'forced completion outbox failure'; END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&pool)
    .await
    .expect("create outbox rejection function");
    sqlx::query(
        "CREATE TRIGGER reject_completion_outbox
         BEFORE INSERT ON message_outbox
         FOR EACH ROW EXECUTE FUNCTION reject_completion_outbox()",
    )
    .execute(&pool)
    .await
    .expect("create outbox rejection trigger");

    let error = finalize_run_if_ready_on_pg(&pool, "run-rollback")
        .await
        .expect_err("outbox failure must abort completion");
    assert!(
        error.contains("forced completion outbox failure"),
        "{error}"
    );
    assert_eq!(run_status(&pool, "run-rollback").await, "active");
    assert_eq!(assigned_run(&pool).await.as_deref(), Some("run-rollback"));
    assert_eq!(outbox_count(&pool).await, 0);
    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn force_completion_deletes_gate_and_audits_operator_source_pg() {
    let (pg_db, pool) = setup_pool().await;
    seed_run_with_slot(&pool, "run-force").await;
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (run_id, phase, status)
         VALUES ('run-force', 1, 'failed')",
    )
    .execute(&pool)
    .await
    .expect("seed force-deleted gate");

    let outcome = force_complete_run_on_pg(
        &pool,
        &ForceRunCompletionCommand {
            run_id: "run-force",
            operator: "operator@example.com",
            source: "queue_admin",
        },
    )
    .await
    .expect("force complete run");
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(run_status(&pool, "run-force").await, "completed");
    assert_eq!(assigned_run(&pool).await, None);
    let gate_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates WHERE run_id = 'run-force'",
    )
    .fetch_one(&pool)
    .await
    .expect("count remaining gates");
    assert_eq!(gate_count, 0);
    let audit = sqlx::query(
        "SELECT action, actor FROM audit_logs
         WHERE entity_type = 'auto_queue_run' AND entity_id = 'run-force'",
    )
    .fetch_one(&pool)
    .await
    .expect("load force audit");
    assert_eq!(
        audit.get::<String, _>("action"),
        "force_complete:queue_admin"
    );
    assert_eq!(audit.get::<String, _>("actor"), "operator@example.com");
    assert_eq!(outbox_count(&pool).await, 1);
    pool.close().await;
    pg_db.drop().await;
}
