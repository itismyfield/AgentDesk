use super::current_batch_phase_pg;
use crate::db::auto_queue::test_support::TestPostgresDb;
use sqlx::PgPool;

async fn setup_phase_gate_fixture(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-pg-test', 'Agent', 'claude', '999')",
    )
    .execute(pool)
    .await
    .expect("seed agent");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-pg-test', 'repo', 'agent-pg-test', 'active')",
    )
    .execute(pool)
    .await
    .expect("seed run");
}

async fn insert_card(pool: &PgPool, id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ($1, $2, $3, 'agent-pg-test')",
    )
    .bind(id)
    .bind(format!("card {id}"))
    .bind(status)
    .execute(pool)
    .await
    .expect("seed card");
}

async fn insert_entry(pool: &PgPool, id: &str, card_id: &str, batch_phase: i64, status: &str) {
    sqlx::query(
        "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase)
             VALUES ($1, 'run-pg-test', $2, 'agent-pg-test', $3, 0, 0, $4)",
    )
    .bind(id)
    .bind(card_id)
    .bind(status)
    .bind(batch_phase)
    .execute(pool)
    .await
    .expect("seed entry");
}

async fn insert_skip_transition(pool: &PgPool, entry_id: &str, trigger_source: &str) {
    sqlx::query(
        "INSERT INTO auto_queue_entry_transitions
                (entry_id, from_status, to_status, trigger_source)
             VALUES ($1, 'dispatched', 'skipped', $2)",
    )
    .bind(entry_id)
    .bind(trigger_source)
    .execute(pool)
    .await
    .expect("seed skip transition");
}

async fn insert_review_dispatch(pool: &PgPool, id: &str, card_id: &str, status: &str) {
    insert_typed_dispatch(pool, id, card_id, "review", status).await;
}

async fn insert_typed_dispatch(
    pool: &PgPool,
    id: &str,
    card_id: &str,
    dispatch_type: &str,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, 'agent-pg-test', $3, $4, 'dispatch test')",
    )
    .bind(id)
    .bind(card_id)
    .bind(dispatch_type)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed dispatch");
}

async fn insert_orphan_entry(pool: &PgPool, id: &str, batch_phase: i64, status: &str) {
    sqlx::query(
        "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase)
             VALUES ($1, 'run-pg-test', NULL, 'agent-pg-test', $2, 0, 0, $3)",
    )
    .bind(id)
    .bind(status)
    .bind(batch_phase)
    .execute(pool)
    .await
    .expect("seed orphan entry");
}

/// #1979 baseline: pending/dispatched entries still drive phase MIN as
/// before. Confirms the new SQL is backward-compatible for the trivial
/// case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returns_min_pending_phase_before_card_lookup() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_phase_gate_fixture(&pool).await;
    insert_card(&pool, "c0", "in_progress").await;
    insert_card(&pool, "c1", "in_progress").await;
    insert_entry(&pool, "e0", "c0", 0, "pending").await;
    insert_entry(&pool, "e1", "c1", 1, "pending").await;

    assert_eq!(
        current_batch_phase_pg(&pool, "run-pg-test").await.unwrap(),
        Some(0)
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1979 regression: entry.status='done' with card still in 'review'
/// must continue to hold the phase. Previously this fell out of the
/// MIN(pending|dispatched) filter and let the next phase dispatch
/// before review verdicts were collected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_entry_with_card_in_review_blocks_phase_advance() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_phase_gate_fixture(&pool).await;
    // Phase 0: implementation finished (entry done) but card still
    // sits in `review` while the review verdict is pending.
    insert_card(&pool, "c0", "review").await;
    insert_entry(&pool, "e0", "c0", 0, "done").await;
    // Phase 1: a pending entry waiting for the gate to lift.
    insert_card(&pool, "c1", "in_progress").await;
    insert_entry(&pool, "e1", "c1", 1, "pending").await;

    assert_eq!(
        current_batch_phase_pg(&pool, "run-pg-test").await.unwrap(),
        Some(0),
        "phase 0 must remain current while a card under it is still in review"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #2780 regression: recovery/cancel can mark the queue entry skipped
/// while the card is still active. That must keep the current phase pinned
/// so a later dependent entry is not dispatched early.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_manual_skipped_entry_with_active_card_blocks_phase_advance() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_phase_gate_fixture(&pool).await;
    insert_card(&pool, "c0", "review").await;
    insert_entry(&pool, "e0", "c0", 0, "skipped").await;
    insert_skip_transition(&pool, "e0", "run_cancel").await;
    insert_card(&pool, "c1", "in_progress").await;
    insert_entry(&pool, "e1", "c1", 1, "pending").await;

    assert_eq!(
        current_batch_phase_pg(&pool, "run-pg-test").await.unwrap(),
        Some(0),
        "non-manual skipped entries with active cards must hold the phase"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_skipped_entry_with_active_card_does_not_block_phase_advance() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_phase_gate_fixture(&pool).await;
    insert_card(&pool, "c0", "review").await;
    insert_entry(&pool, "e0", "c0", 0, "skipped").await;
    insert_skip_transition(&pool, "e0", "manual_skip").await;
    insert_card(&pool, "c1", "in_progress").await;
    insert_entry(&pool, "e1", "c1", 1, "pending").await;

    assert_eq!(
        current_batch_phase_pg(&pool, "run-pg-test").await.unwrap(),
        Some(1),
        "explicit manual skip remains an operator override"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1979 regression: even when the card has already been transitioned
/// elsewhere, a still-live `review` or `review-decision` dispatch on
/// the same card holds the phase.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_entry_with_active_review_dispatch_blocks_phase_advance() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_phase_gate_fixture(&pool).await;
    // Card already terminal (`done`) but a review dispatch is still
    // dispatched — verdict not yet recorded.
    insert_card(&pool, "c0", "done").await;
    insert_entry(&pool, "e0", "c0", 0, "done").await;
    insert_review_dispatch(&pool, "d0", "c0", "dispatched").await;
    insert_card(&pool, "c1", "in_progress").await;
    insert_entry(&pool, "e1", "c1", 1, "pending").await;

    assert_eq!(
        current_batch_phase_pg(&pool, "run-pg-test").await.unwrap(),
        Some(0),
        "phase 0 must remain current while an in-flight review dispatch exists"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1979 regression: a live `review-decision` dispatch (suggestion-pending
/// loop) holds the phase the same way `review` does. Codex re-review
/// flagged that the original tests only covered `review`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_entry_with_active_review_decision_dispatch_blocks_phase_advance() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_phase_gate_fixture(&pool).await;
    insert_card(&pool, "c0", "done").await;
    insert_entry(&pool, "e0", "c0", 0, "done").await;
    insert_typed_dispatch(&pool, "d-rd", "c0", "review-decision", "pending").await;
    insert_card(&pool, "c1", "in_progress").await;
    insert_entry(&pool, "e1", "c1", 1, "pending").await;

    assert_eq!(
        current_batch_phase_pg(&pool, "run-pg-test").await.unwrap(),
        Some(0),
        "phase 0 must remain current while a review-decision dispatch is still pending"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1979 regression: an entry with `kanban_card_id = NULL` (recovery edge)
/// must NOT loop forever in the gate. Without the explicit NOT NULL guard
/// the LEFT JOIN miss made `COALESCE(c.status, 'unknown')` register as
/// "non-terminal" and pinned the phase indefinitely. Codex re-review P2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_orphan_entry_without_card_does_not_block_phase_advance() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_phase_gate_fixture(&pool).await;
    insert_orphan_entry(&pool, "e0-orphan", 0, "done").await;
    insert_card(&pool, "c1", "in_progress").await;
    insert_entry(&pool, "e1", "c1", 1, "pending").await;

    assert_eq!(
        current_batch_phase_pg(&pool, "run-pg-test").await.unwrap(),
        Some(1),
        "orphan done entries must not pin the phase forever"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1979 happy path: when phase 0 is fully settled (every card terminal,
/// no live review dispatch) the phase advances normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_advances_once_cards_are_terminal_and_no_review_inflight() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_phase_gate_fixture(&pool).await;
    insert_card(&pool, "c0", "done").await;
    insert_entry(&pool, "e0", "c0", 0, "done").await;
    insert_card(&pool, "c1", "in_progress").await;
    insert_entry(&pool, "e1", "c1", 1, "pending").await;

    assert_eq!(
        current_batch_phase_pg(&pool, "run-pg-test").await.unwrap(),
        Some(1),
        "phase should advance when phase-0 cards reached terminal status"
    );

    pool.close().await;
    pg_db.drop().await;
}
