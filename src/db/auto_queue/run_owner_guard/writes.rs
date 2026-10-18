//! Raw owner-write SQL. Each function is reached only through a guarded function in
//! the parent module, which takes the owner run tokens first.

use sqlx::{Postgres, Transaction};

use super::{NewDispatchRow, NewRetryDispatchRow, NormalizedPhaseGateState, StatusRowUpdate};

pub(super) async fn insert_dispatch_row(
    tx: &mut Transaction<'_, Postgres>,
    row: &NewDispatchRow<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
            parent_dispatch_id, chain_depth, required_capabilities, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, 'pending', $5, $6, $7, $8, $9, NOW(), NOW()
        )",
    )
    .bind(row.id)
    .bind(row.kanban_card_id)
    .bind(row.to_agent_id)
    .bind(row.dispatch_type)
    .bind(row.title)
    .bind(row.context)
    .bind(row.parent_dispatch_id)
    .bind(row.chain_depth)
    .bind(row.required_capabilities)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

pub(super) async fn insert_retry_dispatch_row(
    tx: &mut Transaction<'_, Postgres>,
    row: &NewRetryDispatchRow<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            retry_count,
            created_at,
            updated_at
        ) VALUES (
            $1, $2, $3, $4, 'pending', $5, $6, $7, NOW(), NOW()
        )",
    )
    .bind(row.id)
    .bind(row.kanban_card_id)
    .bind(row.to_agent_id)
    .bind(row.dispatch_type)
    .bind(row.title)
    .bind(row.context)
    .bind(row.retry_count)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

pub(super) async fn delete_stale_phase_gate_rows(
    tx: &mut Transaction<'_, Postgres>,
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
        .bind(dispatch_ids)
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

pub(super) async fn upsert_unattached_phase_gate_row(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
    phase: i64,
    state: &NormalizedPhaseGateState,
) -> Result<(), String> {
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
    .bind(&state.status)
    .bind(state.verdict.as_deref())
    .bind(&state.pass_verdict)
    .bind(state.next_phase)
    .bind(state.final_phase)
    .bind(state.anchor_card_id.as_deref())
    .bind(state.failure_reason.as_deref())
    .bind(state.created_at.as_deref())
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!("upsert postgres phase-gate row for run {run_id} phase {phase}: {error}")
    })?;
    Ok(())
}

/// Move `dispatch_id` from any other (run, phase) gate row into this one.
pub(super) async fn move_phase_gate_dispatch_row(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
    phase: i64,
    state: &NormalizedPhaseGateState,
    dispatch_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "DELETE FROM auto_queue_phase_gates
         WHERE dispatch_id = $1
           AND NOT (run_id = $2 AND phase = $3)",
    )
    .bind(dispatch_id)
    .bind(run_id)
    .bind(phase)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!("delete existing postgres phase-gate row for dispatch {dispatch_id}: {error}")
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
    .bind(&state.status)
    .bind(state.verdict.as_deref())
    .bind(dispatch_id)
    .bind(&state.pass_verdict)
    .bind(state.next_phase)
    .bind(state.final_phase)
    .bind(state.anchor_card_id.as_deref())
    .bind(state.failure_reason.as_deref())
    .bind(state.created_at.as_deref())
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!(
            "upsert postgres phase-gate row for run {run_id} phase {phase} dispatch {dispatch_id}: {error}"
        )
    })?;
    Ok(())
}

pub(super) async fn update_dispatch_status_row(
    tx: &mut Transaction<'_, Postgres>,
    update: &StatusRowUpdate<'_>,
) -> Result<u64, sqlx::Error> {
    let query = match (update.result_json, update.touch_completed_at) {
        (Some(result_json), true) => sqlx::query(
            "UPDATE task_dispatches
             SET status = $1,
                 result = CAST($2 AS jsonb),
                 updated_at = NOW(),
                 last_stuck_alert_at = NULL,
                 completed_at = CASE
                     WHEN $1 IN ('completed', 'failed', 'cancelled') THEN COALESCE(completed_at, NOW())
                     ELSE completed_at
                 END
             WHERE id = $3
               AND status = $4",
        )
        .bind(update.to_status)
        .bind(result_json)
        .bind(update.dispatch_id)
        .bind(update.current_status),
        (Some(result_json), false) => sqlx::query(
            "UPDATE task_dispatches
             SET status = $1,
                 result = CAST($2 AS jsonb),
                 updated_at = NOW(),
                 last_stuck_alert_at = NULL
             WHERE id = $3
               AND status = $4",
        )
        .bind(update.to_status)
        .bind(result_json)
        .bind(update.dispatch_id)
        .bind(update.current_status),
        (None, true) => sqlx::query(
            "UPDATE task_dispatches
             SET status = $1,
                 updated_at = NOW(),
                 last_stuck_alert_at = NULL,
                 completed_at = CASE
                     WHEN $1 IN ('completed', 'failed', 'cancelled') THEN COALESCE(completed_at, NOW())
                     ELSE completed_at
                 END
             WHERE id = $2
               AND status = $3",
        )
        .bind(update.to_status)
        .bind(update.dispatch_id)
        .bind(update.current_status),
        (None, false) => sqlx::query(
            "UPDATE task_dispatches
             SET status = $1,
                 updated_at = NOW(),
                 last_stuck_alert_at = NULL
             WHERE id = $2
               AND status = $3",
        )
        .bind(update.to_status)
        .bind(update.dispatch_id)
        .bind(update.current_status),
    };
    query
        .execute(&mut **tx)
        .await
        .map(|done| done.rows_affected())
}

pub(super) async fn update_review_recovery_row(
    tx: &mut Transaction<'_, Postgres>,
    dispatch_id: &str,
    context: &str,
    from_status: &str,
    to_status: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             status = $2,
             result = CASE WHEN status = 'failed' THEN NULL ELSE result END,
             completed_at = CASE WHEN status = 'failed' THEN NULL ELSE completed_at END,
             updated_at = NOW(),
             last_stuck_alert_at = NULL
         WHERE id = $3
           AND status = $4",
    )
    .bind(context)
    .bind(to_status)
    .bind(dispatch_id)
    .bind(from_status)
    .execute(&mut **tx)
    .await
    .map(|done| done.rows_affected())
}
