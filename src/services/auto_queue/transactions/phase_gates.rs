//! Phase-gate reconcile for terminal dispatches (status writer, dispatch route, runtime) and the
//! operator run repair. A cleared gate attempts run completion on the same tx.

use super::completion::maybe_finalize_run_if_ready_pg;
use crate::db::auto_queue::phase_gates::normalize_optional_text;
use crate::db::auto_queue::phase_gates::reconcile_write::{
    load_phase_gate_repair_remainder_on_pg_tx, lock_phase_gate_repair_candidates_on_pg_tx,
};
use crate::db::auto_queue::{
    PhaseGateReconciliation, PhaseGateRepairError, PhaseGateRepairOptions, PhaseGateRepairOutcome,
    PhaseGateRepairSummary, write_phase_gate_reconcile_on_pg_tx,
};
use sqlx::PgPool;

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
pub(crate) async fn reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    dispatch_status: &str,
    dispatch_context_json: Option<&str>,
    dispatch_result_json: Option<&str>,
) -> Result<PhaseGateReconciliation, String> {
    reconcile_terminal_dispatch(
        tx,
        dispatch_id,
        dispatch_status,
        dispatch_context_json,
        dispatch_result_json,
        false,
    )
    .await
}

async fn reconcile_terminal_dispatch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    dispatch_status: &str,
    dispatch_context_json: Option<&str>,
    dispatch_result_json: Option<&str>,
    allow_failed_repair: bool,
) -> Result<PhaseGateReconciliation, String> {
    let mut reconciliation = write_phase_gate_reconcile_on_pg_tx(
        tx,
        dispatch_id,
        dispatch_status,
        dispatch_context_json,
        dispatch_result_json,
        allow_failed_repair,
    )
    .await?;
    if let PhaseGateReconciliation::Cleared {
        run_id,
        run_finalized,
        ..
    } = &mut reconciliation
    {
        *run_finalized = maybe_finalize_run_if_ready_pg(tx, run_id).await?;
    }
    Ok(reconciliation)
}

/// Re-evaluate terminal phase-gate dispatches for one run, including rows
/// already marked `failed`. This is an operator repair path for cases where a
/// dispatch result was patched or persisted after the normal completion hook
/// already marked the gate failed.
pub(crate) async fn repair_phase_gates_for_run_on_pg(
    pool: &PgPool,
    run_id: &str,
    options: PhaseGateRepairOptions,
) -> Result<PhaseGateRepairSummary, PhaseGateRepairError> {
    let run_id = run_id.trim();
    if run_id.is_empty() {
        return Err(PhaseGateRepairError::invalid_request("run_id is required"));
    }
    if let Some(phase) = options.phase
        && phase < 0
    {
        return Err(PhaseGateRepairError::invalid_request("phase must be >= 0"));
    }
    let dispatch_id = normalize_optional_text(options.dispatch_id.as_deref());

    let mut tx = pool.begin().await.map_err(|error| {
        PhaseGateRepairError::database(format!(
            "begin postgres phase-gate repair for run {run_id}: {error}"
        ))
    })?;

    let candidates = lock_phase_gate_repair_candidates_on_pg_tx(
        &mut tx,
        run_id,
        options.phase,
        dispatch_id.as_deref(),
    )
    .await?;

    let mut cleared_gates = 0;
    let mut failed_gates = 0;
    let mut awaiting_siblings = 0;
    let mut stale_dispatches = 0;
    let mut no_context_dispatches = 0;
    let mut outcomes = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        let outcome = reconcile_terminal_dispatch(
            &mut tx,
            &candidate.dispatch_id,
            &candidate.dispatch_status,
            candidate.dispatch_context.as_deref(),
            candidate.dispatch_result.as_deref(),
            true,
        )
        .await
        .map_err(PhaseGateRepairError::reconcile)?;
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
                no_context_dispatches += 1;
                repair_outcome.outcome = "no_context".to_string();
            }
            PhaseGateReconciliation::AlreadyFailed => {
                failed_gates += 1;
                repair_outcome.outcome = "already_failed".to_string();
            }
            PhaseGateReconciliation::StaleRow => {
                stale_dispatches += 1;
                repair_outcome.outcome = "stale".to_string();
            }
            PhaseGateReconciliation::MarkedFailed { failed_reason, .. } => {
                failed_gates += 1;
                repair_outcome.outcome = "failed".to_string();
                repair_outcome.failed_reason = Some(failed_reason);
            }
            PhaseGateReconciliation::AwaitingSiblings { pending_count, .. } => {
                awaiting_siblings += 1;
                repair_outcome.outcome = "awaiting_siblings".to_string();
                repair_outcome.pending_count = Some(pending_count);
            }
            PhaseGateReconciliation::Cleared {
                run_resumed,
                run_finalized,
                ..
            } => {
                cleared_gates += 1;
                repair_outcome.outcome = "cleared".to_string();
                repair_outcome.run_resumed = run_resumed;
                repair_outcome.run_finalized = run_finalized;
            }
        }
        outcomes.push(repair_outcome);
    }

    let remainder = load_phase_gate_repair_remainder_on_pg_tx(
        &mut tx,
        run_id,
        options.phase,
        dispatch_id.as_deref(),
    )
    .await?;

    // #2257: build the summary before commit so we can emit it on
    // commit-failure too. Without this, a commit error swallows all
    // per-candidate context and operators see a generic 500 with no record
    // of which gates were attempted.
    let summary = PhaseGateRepairSummary {
        run_id: run_id.to_string(),
        phase_filter: options.phase,
        dispatch_id_filter: dispatch_id,
        candidate_dispatches: outcomes.len(),
        cleared_gates,
        failed_gates,
        awaiting_siblings,
        stale_dispatches,
        no_context_dispatches,
        orphan_gates_skipped: remainder.orphan_gates_skipped,
        blocking_gates_remaining: remainder.blocking_gates_remaining,
        run_status: remainder.run_status,
        outcomes,
    };

    if let Err(error) = tx.commit().await {
        tracing::warn!(
            run_id = %summary.run_id,
            candidate_dispatches = summary.candidate_dispatches,
            cleared_gates = summary.cleared_gates,
            failed_gates = summary.failed_gates,
            awaiting_siblings = summary.awaiting_siblings,
            stale_dispatches = summary.stale_dispatches,
            no_context_dispatches = summary.no_context_dispatches,
            orphan_gates_skipped = summary.orphan_gates_skipped,
            error = %error,
            "[auto-queue] phase-gate repair commit failed; rolled back — reporting attempted-but-not-applied summary"
        );
        return Err(PhaseGateRepairError::database(format!(
            "commit postgres phase-gate repair for run {}: {error}",
            summary.run_id
        )));
    }

    Ok(summary)
}
