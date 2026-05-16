use serde_json::Value;
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
    // #2048 F21: skip done-entry rows whose linked card has been deleted
    // (orphan reference). Without `c.id IS NOT NULL` the LEFT JOIN yields
    // a NULL row that COALESCE turns into the 'unknown' bucket — which the
    // NOT IN check treats as still-active, locking the phase forever.
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
                   AND c.id IS NOT NULL
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

/// Outcome of `reconcile_phase_gate_for_terminal_dispatch_on_pg_tx`. Used by
/// callers (and tests) to decide whether a follow-up resume/complete is owed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PhaseGateReconciliation {
    /// Dispatch carries no `phase_gate` context, so nothing to do.
    NoContext,
    /// Phase-gate state row was already in `failed` — leaving it alone.
    AlreadyFailed,
    /// Phase-gate row was missing for this dispatch (stale or pre-policy
    /// dispatch) — nothing to reconcile.
    StaleRow,
    /// Verdict mismatch or required sibling failure detected. The phase-gate
    /// row was flipped to `failed` and the run was paused (when active).
    /// Caller may emit observability/alerting on top.
    MarkedFailed {
        run_id: String,
        phase: i64,
        failed_dispatch_id: String,
        failed_reason: String,
    },
    /// Pass verdict received but at least one sibling dispatch is still
    /// pending/dispatched. Phase-gate stays open.
    AwaitingSiblings {
        run_id: String,
        phase: i64,
        pending_count: i64,
    },
    /// All sibling phase-gate dispatches passed. The phase-gate rows for
    /// this `(run_id, phase)` were cleared, and the helper also:
    ///   - resumed the run if it was paused on this gate (`run_resumed`);
    ///   - mirrored the JS hook's `completeRunAndNotify` for final-phase
    ///     gates by calling `maybe_finalize_run_if_ready_pg` in-transaction
    ///     (`run_finalized` reflects whether that flipped the run to
    ///     `completed` and queued the completion notification).
    ///
    /// Non-final activation of the next phase's first dispatch is intentionally
    /// not performed here — that is owned by the JS hook's `activateRun` host
    /// call and by the existing `onTick1min` recovery path; the resumed run
    /// status is enough for the next tick to pick it up.
    Cleared {
        run_id: String,
        phase: i64,
        next_phase: Option<i64>,
        final_phase: bool,
        run_resumed: bool,
        run_finalized: bool,
    },
}

/// #1980: durable reconciliation for phase-gate sidecar dispatches.
///
/// `policies/auto-queue.js::onDispatchCompleted` is the canonical path that
/// reads the phase-gate row, compares verdicts, and either clears the row
/// (resuming the run) or marks it `failed` (pausing the run). That hook only
/// fires from `complete_dispatch_pg_inner`; direct status transitions through
/// `set_dispatch_status_with_backends` (used by the dispatch CRUD route and
/// some recovery paths) bypass it, leaving phase-gate rows stuck in
/// `pending`/`failed` forever and blocking every subsequent phase via
/// `activate_preflight`.
///
/// This helper applies the same rules in the durable Postgres path so the
/// reconciliation happens whether the JS hook fires or not. It is idempotent:
/// running it twice for the same dispatch is a no-op (rows already in their
/// final state are detected and left alone).
///
/// `dispatch_context_json` and `dispatch_result_json` are the raw JSONB text
/// columns — the function tolerates `None`/empty/malformed inputs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    dispatch_status: &str,
    dispatch_context_json: Option<&str>,
    dispatch_result_json: Option<&str>,
) -> Result<PhaseGateReconciliation, String> {
    reconcile_phase_gate_for_terminal_dispatch_on_pg_tx_inner(
        tx,
        dispatch_id,
        dispatch_status,
        dispatch_context_json,
        dispatch_result_json,
        false,
    )
    .await
}

async fn repair_phase_gate_for_terminal_dispatch_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    dispatch_status: &str,
    dispatch_context_json: Option<&str>,
    dispatch_result_json: Option<&str>,
) -> Result<PhaseGateReconciliation, String> {
    reconcile_phase_gate_for_terminal_dispatch_on_pg_tx_inner(
        tx,
        dispatch_id,
        dispatch_status,
        dispatch_context_json,
        dispatch_result_json,
        true,
    )
    .await
}

async fn reconcile_phase_gate_for_terminal_dispatch_on_pg_tx_inner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    dispatch_status: &str,
    dispatch_context_json: Option<&str>,
    dispatch_result_json: Option<&str>,
    allow_failed_repair: bool,
) -> Result<PhaseGateReconciliation, String> {
    let Some(gate) = parse_phase_gate_context(dispatch_context_json) else {
        return Ok(PhaseGateReconciliation::NoContext);
    };

    lock_phase_gate_state_on_pg_tx(tx, &gate.run_id, gate.phase).await?;

    let Some(gate_row) =
        load_phase_gate_row_for_dispatch_on_pg_tx(tx, &gate.run_id, gate.phase, dispatch_id)
            .await?
    else {
        return Ok(PhaseGateReconciliation::StaleRow);
    };

    if gate_row.status == "failed" && !allow_failed_repair {
        return Ok(PhaseGateReconciliation::AlreadyFailed);
    }

    let pass_verdict = if gate.pass_verdict.is_empty() {
        "phase_gate_passed".to_string()
    } else {
        gate.pass_verdict.clone()
    };

    // Parse JSON once so we can reuse it for both explicit verdict extraction
    // and checks-only inference.
    let result_value: Option<Value> = dispatch_result_json
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| serde_json::from_str(raw).ok());
    let context_value: Option<Value> = dispatch_context_json
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| serde_json::from_str(raw).ok());
    let mut verdict = result_value.as_ref().and_then(extract_explicit_verdict);
    if verdict.is_none()
        && let Some(result) = result_value.as_ref()
    {
        // #1980 + #699: legacy / checks-only results do not include an
        // explicit verdict. Mirror the JS hook's inference so the durable
        // Rust path does not spuriously fail those gates.
        verdict = infer_phase_gate_pass_verdict(context_value.as_ref(), result);
    }

    // Treat any non-`completed` terminal status (`failed`/`cancelled`) as a
    // gate failure regardless of verdict — the dispatch never produced a
    // verdict so we cannot pretend it passed.
    if dispatch_status != "completed" || verdict.as_deref() != Some(pass_verdict.as_str()) {
        let failed_reason = compose_failed_reason(
            dispatch_status,
            dispatch_result_json,
            verdict.as_deref(),
            &pass_verdict,
        );
        mark_phase_gate_row_failed_on_pg_tx(
            tx,
            &gate.run_id,
            gate.phase,
            dispatch_id,
            verdict.as_deref(),
            &failed_reason,
        )
        .await?;
        // Don't preempt the JS hook's pause/notify for `pending`-row cases
        // where the policy may want richer side effects; instead flip the
        // run to paused only when it is currently active so we don't lose
        // the gate-blocked invariant during the gap.
        pause_run_if_active_on_pg_tx(tx, &gate.run_id).await?;
        return Ok(PhaseGateReconciliation::MarkedFailed {
            run_id: gate.run_id,
            phase: gate.phase,
            failed_dispatch_id: dispatch_id.to_string(),
            failed_reason,
        });
    }

    let sibling_summary =
        load_sibling_phase_gate_dispatches_on_pg_tx(tx, &gate.run_id, gate.phase).await?;

    if sibling_summary.pending > 0 {
        return Ok(PhaseGateReconciliation::AwaitingSiblings {
            run_id: gate.run_id,
            phase: gate.phase,
            pending_count: sibling_summary.pending,
        });
    }

    if let Some(failed_sibling) = sibling_summary.failed_sibling {
        // Aggregate verdict failure: mark this gate failed referencing the
        // first failing sibling so the policy/operator sees the same
        // diagnostic the JS path would produce.
        mark_phase_gate_row_failed_on_pg_tx(
            tx,
            &gate.run_id,
            gate.phase,
            &failed_sibling.dispatch_id,
            failed_sibling.verdict.as_deref(),
            &failed_sibling.reason,
        )
        .await?;
        pause_run_if_active_on_pg_tx(tx, &gate.run_id).await?;
        return Ok(PhaseGateReconciliation::MarkedFailed {
            run_id: gate.run_id,
            phase: gate.phase,
            failed_dispatch_id: failed_sibling.dispatch_id,
            failed_reason: failed_sibling.reason,
        });
    }

    // All siblings passed (or none). Clear the gate row, resume the run if
    // it was paused on this gate, and mirror the JS hook's pass side effects
    // for bypass callers:
    //
    //   - For `final_phase` gates the JS hook calls `completeRunAndNotify`,
    //     which marks the run completed and queues the Discord completion
    //     ping. The Rust equivalent is `maybe_finalize_run_if_ready_pg` —
    //     it is idempotent, in-transaction, and a no-op when the run still
    //     has pending entries or another blocking gate. We always call it
    //     here so a CRUD/recovery completion of a final-phase gate cannot
    //     leave the run sitting in `active` with no pending work.
    //
    //   - Non-final gate activation (kicking off the next phase's first
    //     dispatch) is owned by the JS hook's `activateRun` host call and
    //     by the existing `onTick1min` recovery path. We do not duplicate
    //     that here; resuming the run's status to `active` is enough for
    //     the next tick (or any operator-driven activate) to pick it up.
    sqlx::query("DELETE FROM auto_queue_phase_gates WHERE run_id = $1 AND phase = $2")
        .bind(&gate.run_id)
        .bind(gate.phase)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!(
                "clear phase-gate rows for run {} phase {} after pass: {}",
                gate.run_id, gate.phase, error
            )
        })?;

    let run_resumed = resume_run_if_paused_on_pg_tx(tx, &gate.run_id).await?;
    let run_finalized = super::runs::maybe_finalize_run_if_ready_pg(tx, &gate.run_id).await?;

    Ok(PhaseGateReconciliation::Cleared {
        run_id: gate.run_id,
        phase: gate.phase,
        next_phase: gate.next_phase,
        final_phase: gate.final_phase,
        run_resumed,
        run_finalized,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhaseGateRepairOptions {
    pub phase: Option<i64>,
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhaseGateRepairSummary {
    pub run_id: String,
    pub phase_filter: Option<i64>,
    pub dispatch_id_filter: Option<String>,
    pub candidate_dispatches: usize,
    pub cleared_gates: usize,
    pub failed_gates: usize,
    pub awaiting_siblings: usize,
    pub stale_dispatches: usize,
    pub no_context_dispatches: usize,
    pub blocking_gates_remaining: i64,
    pub run_status: Option<String>,
    pub outcomes: Vec<PhaseGateRepairOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseGateRepairOutcome {
    pub dispatch_id: String,
    pub phase: i64,
    pub outcome: String,
    pub run_resumed: bool,
    pub run_finalized: bool,
    pub pending_count: Option<i64>,
    pub failed_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct PhaseGateRepairCandidate {
    phase: i64,
    dispatch_id: String,
    dispatch_status: String,
    dispatch_context: Option<String>,
    dispatch_result: Option<String>,
}

/// Re-evaluate terminal phase-gate dispatches for one run, including rows
/// already marked `failed`. This is an operator repair path for cases where a
/// dispatch result was patched or persisted after the normal completion hook
/// already marked the gate failed.
pub async fn repair_phase_gates_for_run_on_pg(
    pool: &PgPool,
    run_id: &str,
    options: PhaseGateRepairOptions,
) -> Result<PhaseGateRepairSummary, String> {
    let run_id = run_id.trim();
    if run_id.is_empty() {
        return Err("run_id is required".to_string());
    }
    if let Some(phase) = options.phase
        && phase < 0
    {
        return Err("phase must be >= 0".to_string());
    }
    let dispatch_id = normalize_optional_text(options.dispatch_id.as_deref());

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres phase-gate repair for run {run_id}: {error}"))?;

    let run_exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM auto_queue_runs WHERE id = $1)")
            .bind(run_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| format!("check postgres auto-queue run {run_id}: {error}"))?;
    if !run_exists {
        return Err(format!("not_found:auto-queue run not found: {run_id}"));
    }

    let candidate_rows = sqlx::query(
        "SELECT pg.phase,
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
         ORDER BY pg.phase, pg.dispatch_id",
    )
    .bind(run_id)
    .bind(options.phase)
    .bind(dispatch_id.as_deref())
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load phase-gate repair candidates for run {run_id}: {error}"))?;

    let candidates: Vec<PhaseGateRepairCandidate> = candidate_rows
        .into_iter()
        .map(|row| {
            Ok(PhaseGateRepairCandidate {
                phase: row
                    .try_get("phase")
                    .map_err(|error| format!("decode repair candidate phase: {error}"))?,
                dispatch_id: row
                    .try_get("dispatch_id")
                    .map_err(|error| format!("decode repair candidate dispatch_id: {error}"))?,
                dispatch_status: row
                    .try_get("dispatch_status")
                    .map_err(|error| format!("decode repair candidate status: {error}"))?,
                dispatch_context: row
                    .try_get("dispatch_context")
                    .map_err(|error| format!("decode repair candidate context: {error}"))?,
                dispatch_result: row
                    .try_get("dispatch_result")
                    .map_err(|error| format!("decode repair candidate result: {error}"))?,
            })
        })
        .collect::<Result<_, String>>()?;

    let mut summary = PhaseGateRepairSummary {
        run_id: run_id.to_string(),
        phase_filter: options.phase,
        dispatch_id_filter: dispatch_id.clone(),
        candidate_dispatches: candidates.len(),
        ..PhaseGateRepairSummary::default()
    };

    for candidate in candidates {
        let outcome = repair_phase_gate_for_terminal_dispatch_on_pg_tx(
            &mut tx,
            &candidate.dispatch_id,
            &candidate.dispatch_status,
            candidate.dispatch_context.as_deref(),
            candidate.dispatch_result.as_deref(),
        )
        .await?;
        let mut repair_outcome = PhaseGateRepairOutcome {
            dispatch_id: candidate.dispatch_id,
            phase: candidate.phase,
            outcome: "unknown".to_string(),
            run_resumed: false,
            run_finalized: false,
            pending_count: None,
            failed_reason: None,
        };
        match outcome {
            PhaseGateReconciliation::NoContext => {
                summary.no_context_dispatches += 1;
                repair_outcome.outcome = "no_context".to_string();
            }
            PhaseGateReconciliation::AlreadyFailed => {
                summary.failed_gates += 1;
                repair_outcome.outcome = "already_failed".to_string();
            }
            PhaseGateReconciliation::StaleRow => {
                summary.stale_dispatches += 1;
                repair_outcome.outcome = "stale".to_string();
            }
            PhaseGateReconciliation::MarkedFailed { failed_reason, .. } => {
                summary.failed_gates += 1;
                repair_outcome.outcome = "failed".to_string();
                repair_outcome.failed_reason = Some(failed_reason);
            }
            PhaseGateReconciliation::AwaitingSiblings { pending_count, .. } => {
                summary.awaiting_siblings += 1;
                repair_outcome.outcome = "awaiting_siblings".to_string();
                repair_outcome.pending_count = Some(pending_count);
            }
            PhaseGateReconciliation::Cleared {
                run_resumed,
                run_finalized,
                ..
            } => {
                summary.cleared_gates += 1;
                repair_outcome.outcome = "cleared".to_string();
                repair_outcome.run_resumed = run_resumed;
                repair_outcome.run_finalized = run_finalized;
            }
        }
        summary.outcomes.push(repair_outcome);
    }

    summary.blocking_gates_remaining = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed')
           AND ($2::BIGINT IS NULL OR phase = $2)
           AND ($3::TEXT IS NULL OR dispatch_id = $3)",
    )
    .bind(run_id)
    .bind(options.phase)
    .bind(dispatch_id.as_deref())
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| format!("count remaining blocking phase gates for run {run_id}: {error}"))?;

    summary.run_status =
        sqlx::query_scalar::<_, Option<String>>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| format!("load repaired run status for {run_id}: {error}"))?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres phase-gate repair for run {run_id}: {error}"))?;

    Ok(summary)
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedPhaseGateContext {
    pub run_id: String,
    pub phase: i64,
    pub pass_verdict: String,
    pub next_phase: Option<i64>,
    pub final_phase: bool,
}

fn parse_phase_gate_context(context_json: Option<&str>) -> Option<ParsedPhaseGateContext> {
    let raw = context_json?.trim();
    if raw.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(raw).ok()?;
    let gate = value.get("phase_gate")?;
    let run_id = gate.get("run_id")?.as_str()?.trim().to_string();
    if run_id.is_empty() {
        return None;
    }
    let phase = gate
        .get("batch_phase")
        .and_then(numeric_i64)
        .or_else(|| gate.get("phase").and_then(numeric_i64))?;
    if phase < 0 {
        return None;
    }
    let pass_verdict = gate
        .get("pass_verdict")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let next_phase = gate.get("next_phase").and_then(numeric_i64);
    let final_phase = gate
        .get("final_phase")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(ParsedPhaseGateContext {
        run_id,
        phase,
        pass_verdict,
        next_phase,
        final_phase,
    })
}

fn numeric_i64(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(f) = value.as_f64()
        && f.is_finite()
        && f.fract() == 0.0
    {
        return Some(f as i64);
    }
    if let Some(text) = value.as_str() {
        return text.trim().parse::<i64>().ok();
    }
    None
}

fn extract_explicit_verdict(result: &Value) -> Option<String> {
    for key in ["verdict", "decision"] {
        if let Some(text) = result.get(key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Mirror of the JS `_inferPhaseGatePassVerdict` helper in
/// `policies/auto-queue.js`. When a phase-gate dispatch result is missing an
/// explicit `verdict` / `decision`, but reports check entries that all pass,
/// fall back to the gate's `pass_verdict` (default `phase_gate_passed`). This
/// keeps legacy / checks-only results from being reconciled as failures by
/// the durable Rust path.
///
/// Refuses to infer if any check fails, if any declared check is missing, if
/// no checks are reported, or if `result.verdict` / `result.decision` is
/// already set to anything truthy (mirroring JS's `||` operator semantics —
/// numbers, booleans, objects all count as explicit just like in JS).
fn infer_phase_gate_pass_verdict(context: Option<&Value>, result: &Value) -> Option<String> {
    if has_js_truthy_explicit_verdict(result) {
        return None;
    }
    let phase_gate = context?.get("phase_gate")?;
    if !phase_gate.is_object() {
        return None;
    }
    let checks = result.get("checks")?.as_object()?;
    if checks.is_empty() {
        return None;
    }

    if let Some(declared) = phase_gate.get("checks").and_then(Value::as_array) {
        for required in declared {
            let Some(name) = required.as_str() else {
                continue;
            };
            let Some(entry) = checks.get(name) else {
                return None;
            };
            if !js_check_entry_is_pass(entry) {
                return None;
            }
        }
    }

    for entry in checks.values() {
        if !js_check_entry_is_pass(entry) {
            return None;
        }
    }

    let pass_verdict = phase_gate
        .get("pass_verdict")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "phase_gate_passed".to_string());
    Some(pass_verdict)
}

/// JS-style truthiness check for `result.verdict || result.decision`.
/// Mirrors the `||` short-circuit in `policies/auto-queue.js:47` so any
/// non-falsy value (boolean true, non-zero number, non-empty string, any
/// object/array) blocks inference.
fn has_js_truthy_explicit_verdict(result: &Value) -> bool {
    for key in ["verdict", "decision"] {
        if let Some(value) = result.get(key)
            && is_js_truthy(value)
        {
            return true;
        }
    }
    false
}

fn is_js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Mirrors `entryIsPass` from `policies/auto-queue.js`. Notably, JS does NOT
/// trim whitespace before comparing — only lowercases via `String(...)`. We
/// match that exactly so a status like `" pass "` is rejected here and in JS.
fn js_check_entry_is_pass(entry: &Value) -> bool {
    // #2048 F12: mirror JS `entry.status || entry.result` truthiness. JS
    // treats an empty string as falsy and falls through to `entry.result`;
    // the previous Rust port short-circuited on `status` key presence even
    // when its value was `""`, causing a Rust→JS verdict divergence. Now
    // we ignore empty strings on the `status` side and fall back to
    // `result` just like JS.
    let raw = match entry {
        Value::String(text) => Some(text.as_str()),
        Value::Object(map) => map
            .get("status")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| map.get("result").and_then(Value::as_str)),
        _ => None,
    };
    raw.map(|status| {
        let lower = status.to_ascii_lowercase();
        lower == "pass" || lower == "passed"
    })
    .unwrap_or(false)
}

fn compose_failed_reason(
    dispatch_status: &str,
    dispatch_result_json: Option<&str>,
    verdict: Option<&str>,
    pass_verdict: &str,
) -> String {
    if dispatch_status != "completed" {
        return format!("dispatch reached terminal status {dispatch_status} without verdict");
    }
    if let Some(raw) = dispatch_result_json
        && let Ok(value) = serde_json::from_str::<Value>(raw)
    {
        for key in ["summary", "reason"] {
            if let Some(text) = value.get(key).and_then(Value::as_str) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    format!(
        "expected verdict {pass_verdict}, got {}",
        verdict.unwrap_or("none")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhaseGateRow {
    status: String,
}

async fn load_phase_gate_row_for_dispatch_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
    dispatch_id: &str,
) -> Result<Option<PhaseGateRow>, String> {
    let row = sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM auto_queue_phase_gates
         WHERE run_id = $1 AND phase = $2 AND dispatch_id = $3
         LIMIT 1",
    )
    .bind(run_id)
    .bind(phase)
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        format!(
            "load phase-gate row for run {run_id} phase {phase} dispatch {dispatch_id}: {error}"
        )
    })?;
    Ok(row.map(|status| PhaseGateRow { status }))
}

#[derive(Debug, Default)]
struct SiblingSummary {
    pending: i64,
    failed_sibling: Option<FailedSibling>,
}

#[derive(Debug)]
struct FailedSibling {
    dispatch_id: String,
    verdict: Option<String>,
    reason: String,
}

async fn load_sibling_phase_gate_dispatches_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
) -> Result<SiblingSummary, String> {
    // `task_dispatches.context` and `.result` are TEXT today, but cast to TEXT
    // explicitly so the JSON parsing path stays correct if the schema is ever
    // promoted to JSONB. We also decode them as `Option<String>` and surface
    // decode errors instead of `.ok()`-swallowing them.
    let rows = sqlx::query(
        "SELECT pg.dispatch_id,
                td.status            AS dispatch_status,
                td.context::TEXT     AS dispatch_context,
                td.result::TEXT      AS dispatch_result
         FROM auto_queue_phase_gates pg
         JOIN task_dispatches td ON td.id = pg.dispatch_id
         WHERE pg.run_id = $1
           AND pg.phase = $2
           AND pg.dispatch_id IS NOT NULL",
    )
    .bind(run_id)
    .bind(phase)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load sibling phase-gate dispatches for {run_id}/{phase}: {error}"))?;

    let mut summary = SiblingSummary::default();
    for row in rows {
        let dispatch_id: String = row
            .try_get("dispatch_id")
            .map_err(|error| format!("decode sibling dispatch_id: {error}"))?;
        let status: String = row
            .try_get("dispatch_status")
            .map_err(|error| format!("decode sibling status: {error}"))?;
        let context_text: Option<String> = row.try_get("dispatch_context").map_err(|error| {
            format!("decode sibling dispatch_context for {dispatch_id}: {error}")
        })?;
        let result_text: Option<String> = row.try_get("dispatch_result").map_err(|error| {
            format!("decode sibling dispatch_result for {dispatch_id}: {error}")
        })?;
        let context_value: Option<Value> = context_text
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(|raw| {
                serde_json::from_str(raw).map_err(|error| {
                    format!("parse sibling dispatch_context JSON for {dispatch_id}: {error}")
                })
            })
            .transpose()?;
        let result_value: Option<Value> = result_text
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(|raw| {
                serde_json::from_str(raw).map_err(|error| {
                    format!("parse sibling dispatch_result JSON for {dispatch_id}: {error}")
                })
            })
            .transpose()?;
        match status.as_str() {
            "pending" | "dispatched" => {
                summary.pending += 1;
                continue;
            }
            "completed" => {}
            _ => {
                let verdict = result_value
                    .as_ref()
                    .and_then(|v| {
                        v.get("verdict")
                            .or_else(|| v.get("decision"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_string);
                let reason = result_value
                    .as_ref()
                    .and_then(|v| {
                        v.get("summary")
                            .or_else(|| v.get("reason"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!("sibling dispatch {dispatch_id} reached terminal status {status}")
                    });
                summary.failed_sibling = Some(FailedSibling {
                    dispatch_id,
                    verdict,
                    reason,
                });
                return Ok(summary);
            }
        }

        let expected_verdict = context_value
            .as_ref()
            .and_then(|v| v.get("phase_gate"))
            .and_then(|gate| gate.get("pass_verdict"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "phase_gate_passed".to_string());
        let mut actual_verdict = result_value.as_ref().and_then(extract_explicit_verdict);
        if actual_verdict.is_none()
            && let Some(result) = result_value.as_ref()
        {
            // Mirror the JS hook's #699-round-2 inference for legacy sibling
            // rows that completed before the server fix shipped.
            actual_verdict = infer_phase_gate_pass_verdict(context_value.as_ref(), result);
        }
        if actual_verdict.as_deref() != Some(expected_verdict.as_str()) {
            let reason = result_value
                .as_ref()
                .and_then(|v| {
                    v.get("summary")
                        .or_else(|| v.get("reason"))
                        .and_then(Value::as_str)
                })
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "expected verdict {expected_verdict}, got {}",
                        actual_verdict.as_deref().unwrap_or("none")
                    )
                });
            summary.failed_sibling = Some(FailedSibling {
                dispatch_id,
                verdict: actual_verdict,
                reason,
            });
            return Ok(summary);
        }
    }
    Ok(summary)
}

async fn mark_phase_gate_row_failed_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
    failed_dispatch_id: &str,
    failed_verdict: Option<&str>,
    failed_reason: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE auto_queue_phase_gates
         SET status = 'failed',
             verdict = COALESCE($3, verdict),
             failure_reason = $4,
             updated_at = NOW()
         WHERE run_id = $1 AND phase = $2",
    )
    .bind(run_id)
    .bind(phase)
    .bind(failed_verdict)
    .bind(failed_reason)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!(
            "mark phase-gate {run_id}/{phase} failed via dispatch {failed_dispatch_id}: {error}"
        )
    })?;
    Ok(())
}

async fn pause_run_if_active_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    let rows = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE id = $1
           AND status = 'active'",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("pause auto-queue run {run_id} during phase-gate failure: {error}"))?
    .rows_affected();
    Ok(rows > 0)
}

async fn resume_run_if_paused_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    let rows = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE id = $1
           AND status = 'paused'",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("resume auto-queue run {run_id} after phase-gate clear: {error}"))?
    .rows_affected();
    Ok(rows > 0)
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

#[cfg(test)]
mod reconcile_phase_gate_pg_tests {
    use super::{
        PhaseGateReconciliation, PhaseGateStateWrite, infer_phase_gate_pass_verdict,
        parse_phase_gate_context, reconcile_phase_gate_for_terminal_dispatch_on_pg_tx,
        save_phase_gate_state_on_pg,
    };
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use serde_json::json;
    use sqlx::PgPool;

    async fn fixture(pool: &PgPool) {
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

    async fn insert_dispatch(
        pool: &PgPool,
        id: &str,
        status: &str,
        context: serde_json::Value,
        result: Option<serde_json::Value>,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, to_agent_id, dispatch_type, status, title, context, result)
             VALUES ($1, 'agent-pg-test', 'phase-gate', $2, 'gate dispatch',
                     CAST($3 AS jsonb), CAST($4 AS jsonb))",
        )
        .bind(id)
        .bind(status)
        .bind(context.to_string())
        .bind(result.map(|v| v.to_string()))
        .execute(pool)
        .await
        .expect("seed gate dispatch");
    }

    async fn run_reconcile(
        pool: &PgPool,
        dispatch_id: &str,
        status: &str,
        context: serde_json::Value,
        result: Option<serde_json::Value>,
    ) -> PhaseGateReconciliation {
        let context_text = context.to_string();
        let result_text = result.as_ref().map(|v| v.to_string());
        let mut tx = pool.begin().await.expect("begin tx");
        let outcome = reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
            &mut tx,
            dispatch_id,
            status,
            Some(context_text.as_str()),
            result_text.as_deref(),
        )
        .await
        .expect("reconcile");
        tx.commit().await.expect("commit");
        outcome
    }

    async fn run_status(pool: &PgPool, run_id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("run row")
    }

    async fn gate_count(pool: &PgPool, run_id: &str, phase: i64) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2",
        )
        .bind(run_id)
        .bind(phase)
        .fetch_one(pool)
        .await
        .expect("gate count")
    }

    async fn gate_status(pool: &PgPool, run_id: &str, phase: i64) -> String {
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2 LIMIT 1",
        )
        .bind(run_id)
        .bind(phase)
        .fetch_one(pool)
        .await
        .expect("gate status")
    }

    fn gate_context() -> serde_json::Value {
        json!({
            "phase_gate": {
                "run_id": "run-pg-test",
                "batch_phase": 0,
                "pass_verdict": "phase_gate_passed",
                "next_phase": 1,
                "final_phase": false,
            }
        })
    }

    #[test]
    fn parse_phase_gate_context_handles_string_batch_phase() {
        let ctx = parse_phase_gate_context(Some(
            r#"{"phase_gate":{"run_id":"r","batch_phase":"0","pass_verdict":"p"}}"#,
        ))
        .expect("ctx");
        assert_eq!(ctx.run_id, "r");
        assert_eq!(ctx.phase, 0);
        assert_eq!(ctx.pass_verdict, "p");
    }

    #[test]
    fn parse_phase_gate_context_returns_none_without_run_id() {
        assert!(
            parse_phase_gate_context(Some(
                r#"{"phase_gate":{"batch_phase":0,"pass_verdict":"p"}}"#
            ))
            .is_none()
        );
        assert!(parse_phase_gate_context(Some("{}")).is_none());
        assert!(parse_phase_gate_context(None).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_phase_gate_context_is_noop() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        let outcome =
            run_reconcile(&pool, "dsp-noop", "completed", json!({}), Some(json!({}))).await;
        assert!(matches!(outcome, PhaseGateReconciliation::NoContext));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_phase_gate_row_returns_stale_row() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        let outcome = run_reconcile(
            &pool,
            "dsp-stale",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        assert!(matches!(outcome, PhaseGateReconciliation::StaleRow));

        pool.close().await;
        pg_db.drop().await;
    }

    /// Helper: seed a `pending` auto-queue entry so the run still has work
    /// remaining after a gate clear. This prevents `maybe_finalize_run_if_ready_pg`
    /// from automatically finalizing the run, exposing the resume-only
    /// behavior we want to assert.
    async fn seed_pending_entry(pool: &PgPool, run_id: &str, entry_id: &str, batch_phase: i64) {
        sqlx::query(
            "INSERT INTO kanban_cards (id, repo_id, title, status)
             VALUES ($1, NULL, 'card', 'pending')",
        )
        .bind(format!("{entry_id}-card"))
        .execute(pool)
        .await
        .expect("seed kanban card");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, batch_phase, priority_rank)
             VALUES ($1, $2, $3, 'agent-pg-test', 'pending', $4, 0)",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(format!("{entry_id}-card"))
        .bind(batch_phase)
        .execute(pool)
        .await
        .expect("seed pending entry");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pass_verdict_with_no_siblings_clears_gate_and_resumes_paused_run() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;
        // Seed a pending entry on the next phase so finalization does not
        // run the moment we clear the gate — we want to assert resume here.
        seed_pending_entry(&pool, "run-pg-test", "entry-next-phase", 1).await;

        insert_dispatch(
            &pool,
            "dsp-pass",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-pass".into()],
                next_phase: Some(1),
                final_phase: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");
        sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
            .execute(&pool)
            .await
            .unwrap();

        let outcome = run_reconcile(
            &pool,
            "dsp-pass",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        match outcome {
            PhaseGateReconciliation::Cleared {
                run_resumed,
                next_phase,
                run_finalized,
                ..
            } => {
                assert!(run_resumed, "paused run should resume after pass");
                assert_eq!(next_phase, Some(1));
                assert!(
                    !run_finalized,
                    "run with pending entries must not finalize on gate clear"
                );
            }
            other => panic!("expected Cleared, got {other:?}"),
        }
        assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);
        assert_eq!(run_status(&pool, "run-pg-test").await, "active");

        pool.close().await;
        pg_db.drop().await;
    }

    /// #1980 (codex v2 HIGH): a final-phase gate clear must finalize the run
    /// in-transaction so CRUD/recovery completion does not leave the run
    /// active without a completion notification. Mirrors the JS hook's
    /// `completeRunAndNotify` for the bypass path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn final_phase_pass_finalizes_run_and_queues_completion_notify() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        // No pending entries — drained run, ready to finalize.
        let final_context = json!({
            "phase_gate": {
                "run_id": "run-pg-test",
                "batch_phase": 0,
                "pass_verdict": "phase_gate_passed",
                "final_phase": true,
            }
        });
        insert_dispatch(
            &pool,
            "dsp-final",
            "completed",
            final_context.clone(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-final".into()],
                next_phase: None,
                final_phase: true,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");

        let outcome = run_reconcile(
            &pool,
            "dsp-final",
            "completed",
            final_context,
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        match outcome {
            PhaseGateReconciliation::Cleared {
                run_finalized,
                final_phase,
                ..
            } => {
                assert!(final_phase, "context flagged final_phase");
                assert!(
                    run_finalized,
                    "final-phase pass with drained run must finalize"
                );
            }
            other => panic!("expected Cleared, got {other:?}"),
        }
        assert_eq!(run_status(&pool, "run-pg-test").await, "completed");
        // Completion notification should be queued in the outbox.
        let notify_count: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM message_outbox WHERE bot = 'notify'",
        )
        .fetch_one(&pool)
        .await
        .expect("count notify rows");
        assert!(
            notify_count >= 1,
            "expected completion notification queued, got {notify_count}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verdict_mismatch_marks_gate_failed_and_pauses_run() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        insert_dispatch(
            &pool,
            "dsp-bad",
            "completed",
            gate_context(),
            Some(json!({"verdict":"please_revise","summary":"reviewer wants edits"})),
        )
        .await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-bad".into()],
                next_phase: Some(1),
                final_phase: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");

        let outcome = run_reconcile(
            &pool,
            "dsp-bad",
            "completed",
            gate_context(),
            Some(json!({"verdict":"please_revise","summary":"reviewer wants edits"})),
        )
        .await;
        match outcome {
            PhaseGateReconciliation::MarkedFailed { failed_reason, .. } => {
                assert_eq!(failed_reason, "reviewer wants edits");
            }
            other => panic!("expected MarkedFailed, got {other:?}"),
        }
        assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");
        assert_eq!(run_status(&pool, "run-pg-test").await, "paused");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn already_failed_gate_is_left_alone() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        insert_dispatch(
            &pool,
            "dsp-already",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "failed".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-already".into()],
                next_phase: Some(1),
                final_phase: false,
                failure_reason: Some("earlier failure".into()),
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");
        sqlx::query("UPDATE auto_queue_runs SET status='paused' WHERE id='run-pg-test'")
            .execute(&pool)
            .await
            .unwrap();

        let outcome = run_reconcile(
            &pool,
            "dsp-already",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        assert!(matches!(outcome, PhaseGateReconciliation::AlreadyFailed));
        assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");
        assert_eq!(run_status(&pool, "run-pg-test").await, "paused");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn awaiting_siblings_keeps_gate_open() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        insert_dispatch(
            &pool,
            "dsp-pass-1",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        insert_dispatch(&pool, "dsp-pending-2", "dispatched", gate_context(), None).await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-pass-1".into(), "dsp-pending-2".into()],
                next_phase: Some(1),
                final_phase: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");

        let outcome = run_reconcile(
            &pool,
            "dsp-pass-1",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        match outcome {
            PhaseGateReconciliation::AwaitingSiblings { pending_count, .. } => {
                assert_eq!(pending_count, 1);
            }
            other => panic!("expected AwaitingSiblings, got {other:?}"),
        }
        assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 2);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_status_marks_gate_failed_even_when_verdict_absent() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        insert_dispatch(&pool, "dsp-cancelled", "cancelled", gate_context(), None).await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-cancelled".into()],
                next_phase: Some(1),
                final_phase: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");

        let outcome =
            run_reconcile(&pool, "dsp-cancelled", "cancelled", gate_context(), None).await;
        match outcome {
            PhaseGateReconciliation::MarkedFailed { failed_reason, .. } => {
                assert!(failed_reason.contains("cancelled"));
            }
            other => panic!("expected MarkedFailed, got {other:?}"),
        }
        assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");

        pool.close().await;
        pg_db.drop().await;
    }

    /// #1980 + #699: legacy / checks-only results have all required checks
    /// passing but no explicit `verdict` / `decision`. The Rust reconciler
    /// must mirror the JS hook's inference and treat these as a pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checks_only_pass_without_explicit_verdict_is_inferred() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        let context = json!({
            "phase_gate": {
                "run_id": "run-pg-test",
                "batch_phase": 0,
                "pass_verdict": "phase_gate_passed",
                "next_phase": 1,
                "final_phase": false,
                "checks": ["lint", "tests"],
            }
        });
        let result = json!({
            "checks": {
                "lint": { "status": "pass" },
                "tests": "passed",
            }
        });

        insert_dispatch(
            &pool,
            "dsp-checks-only",
            "completed",
            context.clone(),
            Some(result.clone()),
        )
        .await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-checks-only".into()],
                next_phase: Some(1),
                final_phase: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");

        let outcome =
            run_reconcile(&pool, "dsp-checks-only", "completed", context, Some(result)).await;
        assert!(
            matches!(outcome, PhaseGateReconciliation::Cleared { .. }),
            "checks-only pass should infer phase_gate_passed and clear: got {outcome:?}"
        );
        assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #1980 + codex v2 MED #5 parity: any truthy explicit `verdict`/`decision`
    /// (including non-string values like numbers or booleans) must block
    /// inference, matching JS's `||` operator semantics. The result here has
    /// `verdict: 1` plus all-passing checks — JS would refuse to infer, so
    /// Rust must too.
    #[test]
    fn truthy_non_string_explicit_verdict_blocks_inference() {
        let context = json!({"phase_gate": {"pass_verdict": "phase_gate_passed"}});
        let result = json!({
            "verdict": 1,
            "checks": { "tests": "pass" },
        });
        assert!(
            infer_phase_gate_pass_verdict(Some(&context), &result).is_none(),
            "truthy non-string verdict must block inference"
        );
    }

    /// JS's `entryIsPass` does NOT trim whitespace before comparing. Mirror
    /// that so a status of `" pass "` is rejected (it would also be rejected
    /// in JS).
    #[test]
    fn whitespace_padded_check_status_is_not_inferred_as_pass() {
        let context =
            json!({"phase_gate": {"pass_verdict": "phase_gate_passed", "checks": ["tests"]}});
        let result = json!({"checks": {"tests": " pass "}});
        assert!(
            infer_phase_gate_pass_verdict(Some(&context), &result).is_none(),
            "whitespace-padded status must not be inferred (JS parity)"
        );
    }

    /// #1980: failing check entry must NOT be inferred as pass; the gate
    /// stays open as a fail (mirroring the JS guard).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checks_with_any_fail_is_not_inferred_as_pass() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        let context = json!({
            "phase_gate": {
                "run_id": "run-pg-test",
                "batch_phase": 0,
                "pass_verdict": "phase_gate_passed",
                "next_phase": 1,
                "final_phase": false,
                "checks": ["lint", "tests"],
            }
        });
        let result = json!({
            "checks": {
                "lint": { "status": "pass" },
                "tests": { "status": "fail" },
            }
        });

        insert_dispatch(
            &pool,
            "dsp-mixed-checks",
            "completed",
            context.clone(),
            Some(result.clone()),
        )
        .await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-mixed-checks".into()],
                next_phase: Some(1),
                final_phase: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");

        let outcome = run_reconcile(
            &pool,
            "dsp-mixed-checks",
            "completed",
            context,
            Some(result),
        )
        .await;
        assert!(
            matches!(outcome, PhaseGateReconciliation::MarkedFailed { .. }),
            "mixed-pass/fail checks must not be inferred as pass: got {outcome:?}"
        );
        assert_eq!(gate_status(&pool, "run-pg-test", 0).await, "failed");

        pool.close().await;
        pg_db.drop().await;
    }

    /// #1980: when this dispatch passes and ALL its sibling phase-gate
    /// dispatches have already completed with a passing verdict, the gate
    /// should be cleared. This exercises the multi-sibling all-pass branch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_siblings_completed_and_passing_clears_gate() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        for id in ["dsp-pass-a", "dsp-pass-b"] {
            insert_dispatch(
                &pool,
                id,
                "completed",
                gate_context(),
                Some(json!({"verdict":"phase_gate_passed"})),
            )
            .await;
        }
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-pass-a".into(), "dsp-pass-b".into()],
                next_phase: Some(1),
                final_phase: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");

        let outcome = run_reconcile(
            &pool,
            "dsp-pass-b",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        assert!(
            matches!(outcome, PhaseGateReconciliation::Cleared { .. }),
            "all siblings passing should clear gate: got {outcome:?}"
        );
        assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #1980 fix #2: callers may write `status='completed'` without a `result`
    /// (CRUD route). Reconciliation must fall back to the persisted dispatch
    /// row's result so a previously-recorded passing verdict is honored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_only_completion_uses_persisted_result_for_pass() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        fixture(&pool).await;

        // Seed dispatch row with persisted passing verdict already stored.
        insert_dispatch(
            &pool,
            "dsp-status-only",
            "completed",
            gate_context(),
            Some(json!({"verdict":"phase_gate_passed"})),
        )
        .await;
        save_phase_gate_state_on_pg(
            &pool,
            "run-pg-test",
            0,
            &PhaseGateStateWrite {
                status: "pending".into(),
                pass_verdict: "phase_gate_passed".into(),
                dispatch_ids: vec!["dsp-status-only".into()],
                next_phase: Some(1),
                final_phase: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed gate state");

        // Caller passes None for the result_json — simulating a status-only
        // CRUD update. Reconciliation should still see the persisted result.
        let context_text = gate_context().to_string();
        let mut tx = pool.begin().await.expect("begin tx");
        // Mirror what `set_dispatch_status_on_pg_with_sync` does: read the
        // persisted result text from the row and pass it as the fallback.
        let persisted: Option<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT result::TEXT FROM task_dispatches WHERE id = $1",
        )
        .bind("dsp-status-only")
        .fetch_one(&mut *tx)
        .await
        .expect("load persisted result");
        let outcome = reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
            &mut tx,
            "dsp-status-only",
            "completed",
            Some(context_text.as_str()),
            persisted.as_deref(),
        )
        .await
        .expect("reconcile");
        tx.commit().await.expect("commit");
        assert!(
            matches!(outcome, PhaseGateReconciliation::Cleared { .. }),
            "status-only completion with persisted pass result should clear: got {outcome:?}"
        );
        assert_eq!(gate_count(&pool, "run-pg-test", 0).await, 0);

        pool.close().await;
        pg_db.drop().await;
    }
}
