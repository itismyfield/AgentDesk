use sqlx::{PgPool, Row as SqlxRow};

pub async fn current_batch_phase_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    // #1979: phase advance must consider card lifecycle, not just entry status.
    // Implementation completion sets entry.status='done' while the linked card
    // can still be in 'review'/'in_progress'. The previous "MIN(pending|
    // dispatched)" formulation advanced phases prematurely whenever every
    // implementation entry of a phase finished, even though reviews/decisions
    // for that phase's cards were still mid-flight.
    //
    // An entry continues to hold its phase open if:
    //   1. entry.status in ('pending','dispatched'), or
    //   2. entry.status='done' AND the linked card exists, has not reached a
    //      kanban terminal status (still active in review/in_progress/etc.),
    //      or has a live review/review-decision dispatch.
    //
    // Notes:
    // - The card-side check is gated on `e.kanban_card_id IS NOT NULL` so an
    //   entry with no linked card (rare/recovery edge) falls through to the
    //   pure entry-status path instead of looping forever.
    // - "Terminal" here is the conservative `('done','cancelled','failed')`
    //   set: liberal enough to cover repo-/agent-specific pipeline overrides
    //   that mark non-`done` terminals (`is_terminal()` in pipeline.rs is
    //   dynamic per-pipeline, but holding the phase open longer is the safe
    //   direction since the only cost is one more dispatch wait cycle).
    // - The `task_dispatches` lookup leans on the partial unique indexes for
    //   active `review` / `review-decision` rows added in
    //   migrations/postgres/0001_initial_schema.sql; if production-scale runs
    //   show planner regressions, add a dedicated covering index.
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(COALESCE(e.batch_phase, 0))::BIGINT
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards c ON c.id = e.kanban_card_id
         WHERE e.run_id = $1
           AND (
               e.status IN ('pending', 'dispatched')
               OR (
                   e.status = 'done'
                   AND e.kanban_card_id IS NOT NULL
                   AND (
                       COALESCE(c.status, 'unknown') NOT IN ('done', 'cancelled', 'failed')
                       OR EXISTS (
                           SELECT 1 FROM task_dispatches td
                           WHERE td.kanban_card_id = e.kanban_card_id
                             AND td.dispatch_type IN ('review', 'review-decision')
                             AND td.status IN ('pending', 'dispatched')
                       )
                   )
               )
           )",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
}

pub fn batch_phase_is_eligible(batch_phase: i64, current_phase: Option<i64>) -> bool {
    match current_phase {
        Some(phase) => batch_phase == phase,
        None => true,
    }
}

#[allow(dead_code)]
pub async fn run_has_blocking_phase_gate_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed')",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
}

pub(super) async fn run_has_blocking_phase_gate_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed')",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count blocking phase gates for run {run_id}: {error}"))
}

#[derive(Debug, Clone, Default)]
pub struct PhaseGateStateWrite {
    pub status: String,
    pub verdict: Option<String>,
    pub dispatch_ids: Vec<String>,
    pub pass_verdict: String,
    pub next_phase: Option<i64>,
    pub final_phase: bool,
    pub anchor_card_id: Option<String>,
    pub failure_reason: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhaseGateSaveResult {
    pub persisted_dispatch_ids: Vec<String>,
    pub removed_stale_rows: usize,
}

fn normalize_phase_gate_status(status: &str) -> String {
    let trimmed = status.trim();
    if trimmed.is_empty() {
        "pending".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_phase_gate_pass_verdict(pass_verdict: &str) -> String {
    let trimmed = pass_verdict.trim();
    if trimmed.is_empty() {
        "phase_gate_passed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_optional_text(value: Option<&str>) -> Option<String> {
    value.and_then(|item| {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn dedupe_phase_gate_dispatch_ids(dispatch_ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for dispatch_id in dispatch_ids {
        let normalized = dispatch_id.trim();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.to_string()) {
            deduped.push(normalized.to_string());
        }
    }
    deduped
}

async fn lock_phase_gate_state_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
) -> Result<(), String> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2::TEXT))")
        .bind(run_id)
        .bind(phase)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!("lock postgres phase-gate rows for run {run_id} phase {phase}: {error}")
        })?;
    Ok(())
}

async fn valid_phase_gate_dispatch_ids_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_ids: &[String],
) -> Result<Vec<String>, String> {
    if dispatch_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query("SELECT id FROM task_dispatches WHERE id = ANY($1)")
        .bind(dispatch_ids.to_vec())
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| format!("load postgres phase-gate dispatch ids: {error}"))?;

    let valid: std::collections::HashSet<String> = rows
        .into_iter()
        .filter_map(|row| row.try_get::<String, _>("id").ok())
        .collect();

    Ok(dispatch_ids
        .iter()
        .filter(|dispatch_id| valid.contains(dispatch_id.as_str()))
        .cloned()
        .collect())
}

async fn delete_stale_phase_gate_rows_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
    dispatch_ids: &[String],
) -> Result<usize, String> {
    let rows_affected = if dispatch_ids.is_empty() {
        sqlx::query(
            "DELETE FROM auto_queue_phase_gates
             WHERE run_id = $1
               AND phase = $2
               AND dispatch_id IS NOT NULL",
        )
        .bind(run_id)
        .bind(phase)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!("delete postgres stale phase-gate rows for run {run_id} phase {phase}: {error}")
        })?
        .rows_affected()
    } else {
        sqlx::query(
            "DELETE FROM auto_queue_phase_gates
             WHERE run_id = $1
               AND phase = $2
               AND (dispatch_id IS NULL OR NOT (dispatch_id = ANY($3)))",
        )
        .bind(run_id)
        .bind(phase)
        .bind(dispatch_ids.to_vec())
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!("delete postgres stale phase-gate rows for run {run_id} phase {phase}: {error}")
        })?
        .rows_affected()
    };

    usize::try_from(rows_affected)
        .map_err(|error| format!("convert postgres phase-gate delete count for {run_id}: {error}"))
}

pub async fn save_phase_gate_state_on_pg(
    pool: &PgPool,
    run_id: &str,
    phase: i64,
    state: &PhaseGateStateWrite,
) -> Result<PhaseGateSaveResult, String> {
    let status = normalize_phase_gate_status(&state.status);
    let verdict = normalize_optional_text(state.verdict.as_deref());
    let pass_verdict = normalize_phase_gate_pass_verdict(&state.pass_verdict);
    let anchor_card_id = normalize_optional_text(state.anchor_card_id.as_deref());
    let failure_reason = normalize_optional_text(state.failure_reason.as_deref());
    let created_at = normalize_optional_text(state.created_at.as_deref());
    let deduped_dispatch_ids = dedupe_phase_gate_dispatch_ids(&state.dispatch_ids);

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres phase-gate save for run {run_id}: {error}"))?;
    lock_phase_gate_state_on_pg_tx(&mut tx, run_id, phase).await?;
    let dispatch_ids =
        valid_phase_gate_dispatch_ids_on_pg_tx(&mut tx, &deduped_dispatch_ids).await?;
    let removed_stale_rows =
        delete_stale_phase_gate_rows_on_pg_tx(&mut tx, run_id, phase, &dispatch_ids).await?;

    if dispatch_ids.is_empty() {
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase,
                final_phase, anchor_card_id, failure_reason, created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, NULL, $5, $6, $7, $8, $9,
                COALESCE($10::timestamptz, NOW()), NOW()
             )
             ON CONFLICT (run_id, phase, COALESCE(dispatch_id, ''))
             DO UPDATE SET
                status = EXCLUDED.status,
                verdict = EXCLUDED.verdict,
                pass_verdict = EXCLUDED.pass_verdict,
                next_phase = EXCLUDED.next_phase,
                final_phase = EXCLUDED.final_phase,
                anchor_card_id = EXCLUDED.anchor_card_id,
                failure_reason = EXCLUDED.failure_reason,
                created_at = COALESCE($10::timestamptz, auto_queue_phase_gates.created_at, NOW()),
                updated_at = NOW()",
        )
        .bind(run_id)
        .bind(phase)
        .bind(&status)
        .bind(verdict.as_deref())
        .bind(&pass_verdict)
        .bind(state.next_phase)
        .bind(state.final_phase)
        .bind(anchor_card_id.as_deref())
        .bind(failure_reason.as_deref())
        .bind(created_at.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!("upsert postgres phase-gate row for run {run_id} phase {phase}: {error}")
        })?;
    } else {
        for dispatch_id in &dispatch_ids {
            sqlx::query(
                "DELETE FROM auto_queue_phase_gates
                 WHERE dispatch_id = $1
                   AND NOT (run_id = $2 AND phase = $3)",
            )
            .bind(dispatch_id)
            .bind(run_id)
            .bind(phase)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!(
                    "delete existing postgres phase-gate row for dispatch {dispatch_id}: {error}"
                )
            })?;
            sqlx::query(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase,
                    final_phase, anchor_card_id, failure_reason, created_at, updated_at
                 ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    COALESCE($11::timestamptz, NOW()), NOW()
                 )
                 ON CONFLICT (run_id, phase, COALESCE(dispatch_id, ''))
                 DO UPDATE SET
                    status = EXCLUDED.status,
                    verdict = EXCLUDED.verdict,
                    dispatch_id = EXCLUDED.dispatch_id,
                    pass_verdict = EXCLUDED.pass_verdict,
                    next_phase = EXCLUDED.next_phase,
                    final_phase = EXCLUDED.final_phase,
                    anchor_card_id = EXCLUDED.anchor_card_id,
                    failure_reason = EXCLUDED.failure_reason,
                    created_at = COALESCE($11::timestamptz, auto_queue_phase_gates.created_at, NOW()),
                    updated_at = NOW()",
            )
            .bind(run_id)
            .bind(phase)
            .bind(&status)
            .bind(verdict.as_deref())
            .bind(dispatch_id)
            .bind(&pass_verdict)
            .bind(state.next_phase)
            .bind(state.final_phase)
            .bind(anchor_card_id.as_deref())
            .bind(failure_reason.as_deref())
            .bind(created_at.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!(
                    "upsert postgres phase-gate row for run {run_id} phase {phase} dispatch {dispatch_id}: {error}"
                )
            })?;
        }
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres phase-gate save for run {run_id}: {error}"))?;

    Ok(PhaseGateSaveResult {
        persisted_dispatch_ids: dispatch_ids,
        removed_stale_rows,
    })
}

pub async fn clear_phase_gate_state_on_pg(
    pool: &PgPool,
    run_id: &str,
    phase: i64,
) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres phase-gate clear for run {run_id}: {error}"))?;
    lock_phase_gate_state_on_pg_tx(&mut tx, run_id, phase).await?;
    let deleted =
        sqlx::query("DELETE FROM auto_queue_phase_gates WHERE run_id = $1 AND phase = $2")
            .bind(run_id)
            .bind(phase)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("clear postgres phase-gate rows for run {run_id} phase {phase}: {error}")
            })?
            .rows_affected();
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres phase-gate clear for run {run_id}: {error}"))?;
    Ok(deleted > 0)
}

#[cfg(test)]
mod current_batch_phase_pg_tests {
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
}
