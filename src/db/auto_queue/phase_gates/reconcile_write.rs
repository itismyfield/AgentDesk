//! Operator phase-gate repair SQL: candidate locking and reads before the per-dispatch
//! reconcile loop, and the remaining-gate reads after it, all on the repair tx.

use sqlx::Row as SqlxRow;

use super::{PhaseGateRepairError, lock_phase_gate_state_on_pg_tx};

/// One terminal phase-gate dispatch the repair re-evaluates.
#[derive(Debug, Clone)]
pub(crate) struct PhaseGateRepairCandidate {
    pub(crate) phase: i64,
    pub(crate) dispatch_id: String,
    pub(crate) dispatch_status: String,
    pub(crate) dispatch_context: Option<String>,
    pub(crate) dispatch_result: Option<String>,
}

const PHASE_GATE_REPAIR_CANDIDATE_PHASES_SQL: &str = "\
SELECT DISTINCT pg.phase
FROM auto_queue_phase_gates pg
JOIN task_dispatches td ON td.id = pg.dispatch_id
WHERE pg.run_id = $1
  AND pg.status IN ('pending', 'failed')
  AND pg.dispatch_id IS NOT NULL
  AND td.status NOT IN ('pending', 'dispatched')
  AND ($2::BIGINT IS NULL OR pg.phase = $2)
  AND ($3::TEXT IS NULL OR pg.dispatch_id = $3)
ORDER BY pg.phase";

const PHASE_GATE_REPAIR_CANDIDATES_FOR_UPDATE_SQL: &str = "\
SELECT pg.phase,
       pg.dispatch_id,
       td.status        AS dispatch_status,
       td.context::TEXT AS dispatch_context,
       td.result::TEXT  AS dispatch_result
FROM auto_queue_phase_gates pg
JOIN task_dispatches td ON td.id = pg.dispatch_id
WHERE pg.run_id = $1
  AND pg.status IN ('pending', 'failed')
  AND pg.dispatch_id IS NOT NULL
  AND td.status NOT IN ('pending', 'dispatched')
  AND ($2::BIGINT IS NULL OR pg.phase = $2)
  AND ($3::TEXT IS NULL OR pg.dispatch_id = $3)
ORDER BY pg.phase, pg.dispatch_id
FOR UPDATE OF pg";

/// #2257 concern 4: lock the candidate `task_dispatches` rows themselves
/// BEFORE the per-phase advisory lock or the `phase_gates` row locks. The
/// regular PATCH completion path takes `task_dispatches` first (implicit
/// row lock on its `UPDATE`) and only then takes the same advisory +
/// phase_gates locks the repair path uses. Keeping the same lock order
/// across both paths is what prevents deadlock between a concurrent
/// repair and PATCH on the same dispatch.
///
/// `ORDER BY td.id` makes acquisition deterministic when two concurrent
/// repairs target overlapping dispatch sets — they line up on the same
/// id sequence instead of fighting in arbitrary order.
const PHASE_GATE_REPAIR_LOCK_DISPATCH_ROWS_SQL: &str = "\
SELECT td.id
FROM task_dispatches td
JOIN auto_queue_phase_gates pg ON pg.dispatch_id = td.id
WHERE pg.run_id = $1
  AND pg.status IN ('pending', 'failed')
  AND pg.dispatch_id IS NOT NULL
  AND td.status NOT IN ('pending', 'dispatched')
  AND ($2::BIGINT IS NULL OR pg.phase = $2)
  AND ($3::TEXT IS NULL OR pg.dispatch_id = $3)
ORDER BY td.id
FOR UPDATE OF td";

async fn lock_phase_gate_repair_candidate_dispatch_rows_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase_filter: Option<i64>,
    dispatch_id_filter: Option<&str>,
) -> Result<(), PhaseGateRepairError> {
    sqlx::query_scalar::<_, String>(PHASE_GATE_REPAIR_LOCK_DISPATCH_ROWS_SQL)
        .bind(run_id)
        .bind(phase_filter)
        .bind(dispatch_id_filter)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            PhaseGateRepairError::database(format!(
                "lock task_dispatches rows for phase-gate repair on run {run_id}: {error}"
            ))
        })?;
    Ok(())
}

async fn lock_phase_gate_repair_candidate_phases_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase_filter: Option<i64>,
    dispatch_id_filter: Option<&str>,
) -> Result<(), PhaseGateRepairError> {
    let phases = sqlx::query_scalar::<_, i64>(PHASE_GATE_REPAIR_CANDIDATE_PHASES_SQL)
        .bind(run_id)
        .bind(phase_filter)
        .bind(dispatch_id_filter)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            PhaseGateRepairError::database(format!(
                "load phase-gate repair candidate phases for run {run_id}: {error}"
            ))
        })?;

    for phase in phases {
        lock_phase_gate_state_on_pg_tx(tx, run_id, phase)
            .await
            .map_err(PhaseGateRepairError::database)?;
    }

    Ok(())
}

/// Checks the run exists, takes the dispatch-row, advisory and gate-row locks in the
/// PATCH-compatible order, then reads the candidates.
pub(crate) async fn lock_phase_gate_repair_candidates_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase_filter: Option<i64>,
    dispatch_id_filter: Option<&str>,
) -> Result<Vec<PhaseGateRepairCandidate>, PhaseGateRepairError> {
    let run_exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM auto_queue_runs WHERE id = $1)")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                PhaseGateRepairError::database(format!(
                    "check postgres auto-queue run {run_id}: {error}"
                ))
            })?;
    if !run_exists {
        return Err(PhaseGateRepairError::not_found(format!(
            "auto-queue run not found: {run_id}"
        )));
    }

    // #2257 concern 4: lock task_dispatches FIRST (matching PATCH's order),
    // then advisory locks, then phase_gates FOR UPDATE. Any concurrent
    // dispatch PATCH on the same dispatch waits at step 1; we then see its
    // committed status/result in the candidate read below.
    lock_phase_gate_repair_candidate_dispatch_rows_on_pg_tx(
        tx,
        run_id,
        phase_filter,
        dispatch_id_filter,
    )
    .await?;

    lock_phase_gate_repair_candidate_phases_on_pg_tx(tx, run_id, phase_filter, dispatch_id_filter)
        .await?;

    let candidate_rows = sqlx::query(PHASE_GATE_REPAIR_CANDIDATES_FOR_UPDATE_SQL)
        .bind(run_id)
        .bind(phase_filter)
        .bind(dispatch_id_filter)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            PhaseGateRepairError::database(format!(
                "load phase-gate repair candidates for run {run_id}: {error}"
            ))
        })?;

    candidate_rows
        .into_iter()
        .map(|row| {
            Ok(PhaseGateRepairCandidate {
                phase: row.try_get("phase").map_err(|error| {
                    PhaseGateRepairError::decode(format!("decode repair candidate phase: {error}"))
                })?,
                dispatch_id: row.try_get("dispatch_id").map_err(|error| {
                    PhaseGateRepairError::decode(format!(
                        "decode repair candidate dispatch_id: {error}"
                    ))
                })?,
                dispatch_status: row.try_get("dispatch_status").map_err(|error| {
                    PhaseGateRepairError::decode(format!("decode repair candidate status: {error}"))
                })?,
                dispatch_context: row.try_get("dispatch_context").map_err(|error| {
                    PhaseGateRepairError::decode(format!(
                        "decode repair candidate context: {error}"
                    ))
                })?,
                dispatch_result: row.try_get("dispatch_result").map_err(|error| {
                    PhaseGateRepairError::decode(format!("decode repair candidate result: {error}"))
                })?,
            })
        })
        .collect()
}

/// Gate and run state read after the reconcile loop, on the same tx.
pub(crate) struct PhaseGateRepairRemainder {
    pub(crate) blocking_gates_remaining: i64,
    pub(crate) orphan_gates_skipped: usize,
    pub(crate) run_status: Option<String>,
}

pub(crate) async fn load_phase_gate_repair_remainder_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase_filter: Option<i64>,
    dispatch_id_filter: Option<&str>,
) -> Result<PhaseGateRepairRemainder, PhaseGateRepairError> {
    let blocking_gates_remaining = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed')
           AND ($2::BIGINT IS NULL OR phase = $2)
           AND ($3::TEXT IS NULL OR dispatch_id = $3)",
    )
    .bind(run_id)
    .bind(phase_filter)
    .bind(dispatch_id_filter)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        PhaseGateRepairError::database(format!(
            "count remaining blocking phase gates for run {run_id}: {error}"
        ))
    })?;

    // #2257: surface the count of orphan gates (no dispatch row) so operators
    // know the candidate query intentionally skipped them. Orphans cannot
    // match a concrete dispatch-id filter, so a filtered repair reports 0.
    let orphan_gates_skipped = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed')
           AND dispatch_id IS NULL
           AND ($2::BIGINT IS NULL OR phase = $2)
           AND $3::TEXT IS NULL",
    )
    .bind(run_id)
    .bind(phase_filter)
    .bind(dispatch_id_filter)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        PhaseGateRepairError::database(format!(
            "count orphan phase gates for run {run_id}: {error}"
        ))
    })? as usize;

    let run_status =
        sqlx::query_scalar::<_, Option<String>>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                PhaseGateRepairError::database(format!(
                    "load repaired run status for {run_id}: {error}"
                ))
            })?;

    Ok(PhaseGateRepairRemainder {
        blocking_gates_remaining,
        orphan_gates_skipped,
        run_status,
    })
}
