use sqlx::{PgPool, Row as SqlxRow};

pub async fn current_batch_phase_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(COALESCE(batch_phase, 0))::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
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
