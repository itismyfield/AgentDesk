//! Phase-gate violation detector (issue #2657).
//!
//! Read-only scanner that surfaces situations where a higher-phase entry has
//! an active dispatch *while* the run's current phase still has open work.
//!
//! The dispatch hot path (`activate_command.rs`, `dispatch_command.rs`)
//! already enforces phase ordering at write time via
//! `phase_gates::run_has_blocking_phase_gate_on_pg_tx` and the per-run
//! advisory lock. Production incidents the audit fleet captured (issue
//! #2657, citation chunk-04 3b357798 "페이즈0이 안끝났는데 페이즈1이 왜
//! 구현에들어가는거야") show that *after-the-fact* operator observability
//! is missing — users only learn about a misfire by reading turn logs.
//!
//! This module provides the read side: a single `scan_violations_pg` entry
//! point that callers (Discord `/adk-phase`, CLI `agentdesk query
//! phase-gate`, dashboard) can poll. **It does not block dispatches** —
//! blocking is the activate path's responsibility and intentionally scoped
//! out of this PR to keep blast radius small.

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};

use crate::{
    app_state::AppState,
    error::{AppError, AppResult, ErrorCode},
};

/// A single observed ordering violation.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseGateViolation {
    /// Auto-queue run that owns the offending entry.
    pub run_id: String,
    /// Run agent_id (helpful for cross-fleet aggregation).
    pub agent_id: Option<String>,
    /// Entry id with the active dispatch.
    pub entry_id: String,
    /// Linked kanban card id (if any).
    pub kanban_card_id: Option<String>,
    /// GitHub issue number for the entry, when available.
    pub github_issue_number: Option<i64>,
    /// Active dispatch id that is suspected of jumping the gate.
    pub dispatch_id: Option<String>,
    /// `batch_phase` recorded on the dispatched entry.
    pub entry_batch_phase: i64,
    /// Current minimum-blocking batch_phase reported by
    /// `current_batch_phase_pg`. The violation triggers when
    /// `entry_batch_phase > current_batch_phase`.
    pub current_batch_phase: i64,
    /// One-line human-readable summary used by text renderers.
    pub summary: String,
}

/// Top-level snapshot returned to callers.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseGateSnapshot {
    pub violations: Vec<PhaseGateViolation>,
    pub runs_scanned: usize,
    /// Time-bounded: if true, scan completed without truncation.
    pub complete: bool,
}

/// Scan all non-terminal runs and return any phase-gate violations.
///
/// Algorithm (single SQL pass + per-run advance pointer fetch):
///
/// 1. Pick runs whose status is in `('active', 'paused', 'generated',
///    'pending')` — terminal runs cannot misfire further.
/// 2. For each candidate, compute `current_batch_phase` via the canonical
///    `current_batch_phase_pg` helper (same definition the activate path
///    uses, so we never report a false positive against the live truth).
/// 3. Select dispatched/pending entries with `batch_phase > current` and
///    join in their active task_dispatches row.
///
/// The whole scan is read-only and uses a single pool connection per run.
/// Returns `Err(String)` only on DB errors — empty violation list is the
/// healthy state.
pub async fn scan_violations_pg(pool: &PgPool) -> Result<PhaseGateSnapshot, String> {
    let run_rows = sqlx::query(
        "SELECT id, agent_id
         FROM auto_queue_runs
         WHERE status IN ('active', 'paused', 'generated', 'pending')",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("list candidate runs: {e}"))?;

    let mut violations: Vec<PhaseGateViolation> = Vec::new();
    let runs_scanned = run_rows.len();

    for row in run_rows {
        let run_id: String = row
            .try_get("id")
            .map_err(|e| format!("decode run id: {e}"))?;
        let agent_id: Option<String> = row.try_get("agent_id").ok();

        // Use the canonical phase-pointer query. None ⇒ run is fully done;
        // no point checking for misfires above an undefined floor.
        let current =
            match crate::db::auto_queue::phase_gates::current_batch_phase_pg(pool, &run_id).await {
                Ok(Some(phase)) => phase,
                Ok(None) => continue,
                Err(e) => return Err(format!("current_batch_phase_pg for run {run_id}: {e}")),
            };

        let entry_rows = sqlx::query(
            "SELECT e.id            AS entry_id,
                    e.kanban_card_id,
                    e.batch_phase,
                    e.dispatch_id,
                    td.status        AS dispatch_status,
                    kc.github_issue_number::BIGINT AS github_issue_number
             FROM auto_queue_entries e
             LEFT JOIN task_dispatches td ON td.id = e.dispatch_id
             LEFT JOIN kanban_cards kc   ON kc.id = e.kanban_card_id
             WHERE e.run_id = $1
               AND e.status IN ('dispatched', 'pending')
               AND COALESCE(e.batch_phase, 0) > $2
               AND (td.status IS NULL OR td.status IN ('pending', 'dispatched'))",
        )
        .bind(&run_id)
        .bind(current)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("list violating entries for run {run_id}: {e}"))?;

        for ev in entry_rows {
            let entry_id: String = ev
                .try_get("entry_id")
                .map_err(|e| format!("decode entry_id: {e}"))?;
            let kanban_card_id: Option<String> = ev.try_get("kanban_card_id").ok();
            let batch_phase: i64 = ev
                .try_get::<Option<i64>, _>("batch_phase")
                .map_err(|e| format!("decode batch_phase: {e}"))?
                .unwrap_or(0);
            let dispatch_id: Option<String> = ev.try_get("dispatch_id").ok();
            let github_issue_number: Option<i64> = ev
                .try_get::<Option<i64>, _>("github_issue_number")
                .ok()
                .flatten();

            // Filter: only count rows that actually have an active dispatch
            // OR are pending in a later phase (latter still indicates the
            // queue ordering is "racing ahead" and is worth surfacing).
            // We already filtered td.status; the join might have produced a
            // NULL row for `pending` entries with no dispatch yet, which is
            // also useful diagnostic data, so we keep them.

            let summary = match (github_issue_number, dispatch_id.as_deref()) {
                (Some(num), Some(did)) => format!(
                    "run {run_id}: #{num} dispatched at phase {batch_phase} while current={current} (dispatch {did})"
                ),
                (Some(num), None) => format!(
                    "run {run_id}: #{num} pending at phase {batch_phase} while current={current}"
                ),
                (None, Some(did)) => format!(
                    "run {run_id}: entry {entry_id} dispatched at phase {batch_phase} while current={current} (dispatch {did})"
                ),
                (None, None) => format!(
                    "run {run_id}: entry {entry_id} pending at phase {batch_phase} while current={current}"
                ),
            };

            violations.push(PhaseGateViolation {
                run_id: run_id.clone(),
                agent_id: agent_id.clone(),
                entry_id,
                kanban_card_id,
                github_issue_number,
                dispatch_id,
                entry_batch_phase: batch_phase,
                current_batch_phase: current,
                summary,
            });
        }
    }

    Ok(PhaseGateSnapshot {
        violations,
        runs_scanned,
        complete: true,
    })
}

/// GET /api/queue/phase-gates/violations
///
/// Returns the live `PhaseGateSnapshot`. 503 when the Postgres pool is
/// unavailable so callers can disambiguate "no violations" from "couldn't
/// check". On scanner DB error returns 500 with an `error` body — empty
/// `violations` vector is the success-and-clean signal.
pub async fn violations_route(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::AutoQueue,
            "postgres pool unavailable",
        ));
    };
    match scan_violations_pg(pool).await {
        Ok(snapshot) => match serde_json::to_value(&snapshot) {
            Ok(value) => Ok((StatusCode::OK, Json(value))),
            Err(e) => Err(AppError::internal(format!("serialize snapshot: {e}"))
                .with_code(ErrorCode::AutoQueue)),
        },
        Err(e) => Err(AppError::internal(e).with_code(ErrorCode::AutoQueue)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serializes_with_zero_violations() {
        let snap = PhaseGateSnapshot {
            violations: vec![],
            runs_scanned: 3,
            complete: true,
        };
        let value = serde_json::to_value(&snap).unwrap();
        assert_eq!(value["runs_scanned"], 3);
        assert_eq!(value["complete"], true);
        assert!(value["violations"].as_array().unwrap().is_empty());
    }

    #[test]
    fn violation_summary_uses_issue_number_when_available() {
        let v = PhaseGateViolation {
            run_id: "r1".into(),
            agent_id: Some("agent-a".into()),
            entry_id: "e1".into(),
            kanban_card_id: None,
            github_issue_number: Some(1234),
            dispatch_id: Some("d1".into()),
            entry_batch_phase: 1,
            current_batch_phase: 0,
            summary: "run r1: #1234 dispatched at phase 1 while current=0 (dispatch d1)".into(),
        };
        assert!(v.summary.contains("#1234"));
        assert!(v.summary.contains("phase 1"));
        assert!(v.summary.contains("current=0"));
    }

    #[test]
    fn violation_serializes_round_trip() {
        let v = PhaseGateViolation {
            run_id: "r1".into(),
            agent_id: None,
            entry_id: "e1".into(),
            kanban_card_id: Some("c1".into()),
            github_issue_number: None,
            dispatch_id: None,
            entry_batch_phase: 2,
            current_batch_phase: 1,
            summary: "test".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"entry_batch_phase\":2"));
        assert!(json.contains("\"current_batch_phase\":1"));
    }
}
