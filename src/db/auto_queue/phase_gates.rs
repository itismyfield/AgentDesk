use serde_json::Value;
use sqlx::{PgPool, Row as SqlxRow};
use thiserror::Error;

// #4884: verdict inference lives in exactly one place. The aliases keep this
// module's long-standing local names while the bodies moved to the canonical
// reducer shared with the dispatch finalize path.
use super::phase_gate_verdict::{
    DEFAULT_PASS_VERDICT, diagnostic_verdict, pass_verdict_of, resolve_verdict,
    verdict_matches as phase_gate_verdict_matches,
};

pub mod reconcile_write;

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

    // Among skipped transitions at one `created_at` the larger id is latest, as retention keeps.
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(COALESCE(e.batch_phase, 0))::BIGINT
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards c ON c.id = e.kanban_card_id
         WHERE e.run_id = $1
           AND (
               e.status IN ('pending', 'dispatched')
               OR (
                   e.status IN ('done', 'skipped')
                   AND e.kanban_card_id IS NOT NULL
                   AND c.id IS NOT NULL
                   AND (
                       e.status = 'done'
                       OR COALESCE((
                           SELECT t.trigger_source
                           FROM auto_queue_entry_transitions t
                           WHERE t.entry_id = e.id
                             AND t.to_status = 'skipped'
                           ORDER BY t.created_at DESC, t.id DESC
                           LIMIT 1
                       ), '') != 'manual_skip'
                   )
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
        "SELECT EXISTS(SELECT 1
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed'))",
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
        "SELECT EXISTS(SELECT 1
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed'))",
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

pub fn normalize_optional_text(value: Option<&str>) -> Option<String> {
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

pub(super) async fn lock_phase_gate_state_on_pg_tx(
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

pub async fn save_phase_gate_state_on_pg(
    pool: &PgPool,
    run_id: &str,
    phase: i64,
    state: &PhaseGateStateWrite,
) -> Result<PhaseGateSaveResult, String> {
    let normalized = super::run_owner_guard::NormalizedPhaseGateState {
        status: normalize_phase_gate_status(&state.status),
        verdict: normalize_optional_text(state.verdict.as_deref()),
        pass_verdict: normalize_phase_gate_pass_verdict(&state.pass_verdict),
        next_phase: state.next_phase,
        final_phase: state.final_phase,
        anchor_card_id: normalize_optional_text(state.anchor_card_id.as_deref()),
        failure_reason: normalize_optional_text(state.failure_reason.as_deref()),
        created_at: normalize_optional_text(state.created_at.as_deref()),
    };
    let deduped_dispatch_ids = dedupe_phase_gate_dispatch_ids(&state.dispatch_ids);

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres phase-gate save for run {run_id}: {error}"))?;
    let result = super::run_owner_guard::publish_phase_gate_state_on_pg_tx(
        &mut tx,
        run_id,
        phase,
        &normalized,
        &deduped_dispatch_ids,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres phase-gate save for run {run_id}: {error}"))?;
    Ok(result)
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
pub enum PhaseGateReconciliation {
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
    ///     gates through the service's in-transaction `maybe_finalize_run_if_ready_pg`
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

/// Phase-gate reconcile writes for one terminal dispatch. A `Cleared` result carries
/// `run_finalized: false`; the service attempts run completion on the same tx and fills it.
pub async fn write_phase_gate_reconcile_on_pg_tx(
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

    // Parse JSON once so we can reuse it for expected-verdict selection,
    // explicit verdict extraction, and checks-only inference.
    let result_value: Option<Value> = dispatch_result_json
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| serde_json::from_str(raw).ok());
    let context_value: Option<Value> = dispatch_context_json
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| serde_json::from_str(raw).ok());
    let authoritative_context = authoritative_phase_gate_context_on_pg_tx(
        tx,
        &gate.run_id,
        gate.phase,
        context_value.as_ref(),
    )
    .await?;
    // Each dispatch context is the authority for selecting a registry snapshot,
    // but never for declaration contents. Legacy contexts are reconstructed only
    // after the persisted entries for this run/phase prove NULL/blank kinds.
    let pass_verdict = authoritative_context
        .as_ref()
        .and_then(|context| context.get("phase_gate"))
        .map(pass_verdict_of)
        .unwrap_or_else(|| DEFAULT_PASS_VERDICT.to_string());
    // #1980 + #699: legacy / checks-only results do not carry an explicit
    // verdict, so `resolve_verdict` falls back to checks inference — the same
    // fallback the dispatch finalize path applies before persisting a result.
    let verdict_resolution = result_value
        .as_ref()
        .map(|result| resolve_verdict(authoritative_context.as_ref(), result));
    let verdict = verdict_resolution
        .as_ref()
        .and_then(|resolution| resolution.verdict());
    let diagnostic_verdict = result_value
        .as_ref()
        .zip(verdict_resolution.as_ref())
        .and_then(|(result, resolution)| diagnostic_verdict(result, resolution));

    // Treat any non-`completed` terminal status (`failed`/`cancelled`) as a
    // gate failure regardless of verdict — the dispatch never produced a
    // verdict so we cannot pretend it passed.
    if dispatch_status != "completed"
        || !phase_gate_verdict_matches(
            verdict,
            &pass_verdict,
            authoritative_context.as_ref(),
            result_value.as_ref(),
        )
    {
        let failed_reason = compose_failed_reason(
            dispatch_status,
            dispatch_result_json,
            diagnostic_verdict.as_deref().or(verdict),
            &pass_verdict,
        );
        mark_phase_gate_row_failed_on_pg_tx(
            tx,
            &gate.run_id,
            gate.phase,
            dispatch_id,
            diagnostic_verdict.as_deref().or(verdict),
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
    //   - For `final_phase` gates the JS hook calls `completeRunAndNotify`
    //     after clearing the gate. This Rust bypass path instead resumes a
    //     paused run and the service then calls `maybe_finalize_run_if_ready_pg` for every
    //     cleared gate. That helper completes and queues the notification only
    //     when no pending/dispatched entries or other blocking gates remain.
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

    Ok(PhaseGateReconciliation::Cleared {
        run_id: gate.run_id,
        phase: gate.phase,
        next_phase: gate.next_phase,
        final_phase: gate.final_phase,
        run_resumed,
        run_finalized: false,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhaseGateRepairOptions {
    pub phase: Option<i64>,
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum PhaseGateRepairError {
    #[error("{message}")]
    InvalidRequest { message: String },
    #[error("{message}")]
    NotFound { message: String },
    #[error("{message}")]
    Database { message: String },
    #[error("{message}")]
    Decode { message: String },
    #[error("{message}")]
    Reconcile { message: String },
}

impl PhaseGateRepairError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidRequest { .. } => "invalid_request",
            Self::NotFound { .. } => "not_found",
            Self::Database { .. } => "database_error",
            Self::Decode { .. } => "decode_error",
            Self::Reconcile { .. } => "reconcile_error",
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound {
            message: message.into(),
        }
    }

    pub fn database(message: impl Into<String>) -> Self {
        Self::Database {
            message: message.into(),
        }
    }

    pub(crate) fn decode(message: impl Into<String>) -> Self {
        Self::Decode {
            message: message.into(),
        }
    }

    pub fn reconcile(message: impl Into<String>) -> Self {
        Self::Reconcile {
            message: message.into(),
        }
    }
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
    /// #2257: gates that match the filter but have `dispatch_id IS NULL`.
    /// The repair candidate query skips them (it requires a JOIN onto
    /// `task_dispatches`), but operators need to see they exist so they
    /// can decide whether to delete or hand-patch them separately.
    pub orphan_gates_skipped: usize,
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
pub struct ParsedPhaseGateContext {
    pub run_id: String,
    pub phase: i64,
    pub next_phase: Option<i64>,
    pub final_phase: bool,
}

pub fn parse_phase_gate_context(context_json: Option<&str>) -> Option<ParsedPhaseGateContext> {
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
    let next_phase = gate.get("next_phase").and_then(numeric_i64);
    let final_phase = gate
        .get("final_phase")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(ParsedPhaseGateContext {
        run_id,
        phase,
        next_phase,
        final_phase,
    })
}

/// Test seam for the phase-gate tests mounted in the services test host.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn parse_phase_gate_context_for_test(
    context_json: Option<&str>,
) -> Option<ParsedPhaseGateContext> {
    parse_phase_gate_context(context_json)
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
         LIMIT 1
         FOR UPDATE",
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

async fn phase_gate_run_phase_uses_legacy_default_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>(
        "SELECT COALESCE(
             BOOL_AND(NULLIF(BTRIM(phase_gate_kind, E' \t\n\r\\f\\x0b'), '') IS NULL),
             FALSE
         )
         FROM auto_queue_entries
         WHERE run_id = $1
           AND COALESCE(batch_phase, 0) = $2",
    )
    .bind(run_id)
    .bind(phase)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        format!("load persisted phase-gate kind provenance for {run_id}/{phase}: {error}")
    })
}

pub async fn authoritative_phase_gate_context_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
    context: Option<&Value>,
) -> Result<Option<Value>, String> {
    let legacy_default =
        phase_gate_run_phase_uses_legacy_default_on_pg_tx(tx, run_id, phase).await?;
    Ok(super::phase_gate_verdict::authoritative_context(
        context,
        legacy_default,
    ))
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
        let authoritative_context =
            authoritative_phase_gate_context_on_pg_tx(tx, run_id, phase, context_value.as_ref())
                .await?;
        match status.as_str() {
            "pending" | "dispatched" => {
                summary.pending += 1;
                continue;
            }
            "completed" => {}
            _ => {
                let resolution = result_value
                    .as_ref()
                    .map(|result| resolve_verdict(authoritative_context.as_ref(), result));
                let verdict = result_value
                    .as_ref()
                    .zip(resolution.as_ref())
                    .and_then(|(result, resolution)| diagnostic_verdict(result, resolution));
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

        let expected_verdict = authoritative_context
            .as_ref()
            .and_then(|value| value.get("phase_gate"))
            .map(pass_verdict_of)
            .unwrap_or_else(|| DEFAULT_PASS_VERDICT.to_string());
        // Sibling rows go through the same explicit-or-inferred resolution as
        // the primary dispatch so an aggregate gate decision cannot disagree
        // with the per-dispatch one (#699 round 2, #4884).
        let actual_resolution = result_value
            .as_ref()
            .map(|result| resolve_verdict(authoritative_context.as_ref(), result));
        let actual_verdict = actual_resolution
            .as_ref()
            .and_then(|resolution| resolution.verdict());
        let diagnostic_verdict = result_value
            .as_ref()
            .zip(actual_resolution.as_ref())
            .and_then(|(result, resolution)| diagnostic_verdict(result, resolution));
        if !phase_gate_verdict_matches(
            actual_verdict,
            &expected_verdict,
            authoritative_context.as_ref(),
            result_value.as_ref(),
        ) {
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
                        diagnostic_verdict
                            .as_deref()
                            .or(actual_verdict)
                            .unwrap_or("none")
                    )
                });
            summary.failed_sibling = Some(FailedSibling {
                dispatch_id,
                verdict: diagnostic_verdict.or_else(|| actual_verdict.map(str::to_string)),
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
mod current_batch_phase_pg_tests;
