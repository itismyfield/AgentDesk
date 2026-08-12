use std::collections::BTreeMap;

use axum::{Json, http::StatusCode};
use serde_json::json;

use super::*;

const END_RUN_HINT: &str =
    "this endpoint is read-only; use POST /api/queue/runs/{id}/end to end a run";

#[derive(Debug, PartialEq, Eq)]
struct ResetScopeClaim {
    run_id: String,
    repo: Option<String>,
    agent_id: Option<String>,
}

fn normalized_claim(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn reset_contract_error(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> AppError {
    AppError::new(status, ErrorCode::AutoQueue, message)
        .with_context("reset_code", code)
        .with_context("mutates", false)
        .with_context("hint", END_RUN_HINT)
}

fn normalize_reset_scope(body: ResetBody) -> Result<ResetScopeClaim, AppError> {
    let run_id = normalized_claim(body.run_id).ok_or_else(|| {
        reset_contract_error(
            StatusCode::BAD_REQUEST,
            "reset_run_id_required",
            "run_id is required for reset",
        )
    })?;
    let repo = normalized_claim(body.repo);
    let agent_id = normalized_claim(body.agent_id);
    if repo.is_none() && agent_id.is_none() {
        return Err(reset_contract_error(
            StatusCode::BAD_REQUEST,
            "reset_scope_claim_required",
            "repo or agent_id is required for reset scope ownership",
        ));
    }

    Ok(ResetScopeClaim {
        run_id,
        repo,
        agent_id,
    })
}

fn require_terminal_status(status: &str) -> Result<(), AppError> {
    match status {
        "completed" | "cancelled" => Ok(()),
        "generated" | "pending" | "active" | "paused" | "restoring" => Err(reset_contract_error(
            StatusCode::CONFLICT,
            "reset_nonterminal_run_unsupported",
            format!("auto-queue run is nonterminal (status={status}); reset does not mutate runs"),
        )
        .with_context("status", status)),
        _ => Err(reset_contract_error(
            StatusCode::CONFLICT,
            "reset_unknown_run_status",
            format!("auto-queue run has unsupported status '{status}'; reset fails closed"),
        )
        .with_context("status", status)),
    }
}

async fn load_terminal_residual_pg(
    pool: &sqlx::PgPool,
    run_id: &str,
) -> Result<serde_json::Value, String> {
    let live_dispatches = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT td.id)::BIGINT
         FROM task_dispatches td
         JOIN auto_queue_entries e ON e.dispatch_id = td.id
         WHERE e.run_id = $1
           AND td.status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(pool);
    let entries_by_status = sqlx::query_as::<_, (String, i64)>(
        "SELECT status, COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
         GROUP BY status
         ORDER BY status",
    )
    .bind(run_id)
    .fetch_all(pool);
    let open_cleanup_tasks = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_run_cleanup_tasks
         WHERE $1 = ANY(run_ids)
           AND dead_lettered_at IS NULL",
    )
    .bind(run_id)
    .fetch_one(pool);
    let cards_in_progress = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT kc.id)::BIGINT
         FROM kanban_cards kc
         JOIN auto_queue_entries e ON e.kanban_card_id = kc.id
         WHERE e.run_id = $1
           AND kc.status = 'in_progress'",
    )
    .bind(run_id)
    .fetch_one(pool);

    let (live_dispatches, entries_by_status, open_cleanup_tasks, cards_in_progress) =
        tokio::try_join!(
            live_dispatches,
            entries_by_status,
            open_cleanup_tasks,
            cards_in_progress
        )
        .map_err(|error| format!("inspect terminal auto-queue run '{run_id}': {error}"))?;
    let entries_by_status = entries_by_status.into_iter().collect::<BTreeMap<_, _>>();

    Ok(json!({
        "live_dispatches": live_dispatches,
        "entries_by_status": entries_by_status,
        "open_cleanup_tasks": open_cleanup_tasks,
        "cards_in_progress": cards_in_progress,
    }))
}

fn terminal_reset_response(
    run_id: &str,
    run_status: &str,
    residual: serde_json::Value,
) -> serde_json::Value {
    json!({
        "ok": true,
        "action": "inspected_terminal_run",
        "mutates": false,
        "run_id": run_id,
        "run_status": run_status,
        "residual": residual,
        "hint": END_RUN_HINT,
    })
}

/// `/api/queue/reset` is deliberately an inspection-only compatibility route.
/// It never starts a transaction or calls run cancellation/end machinery.
pub(super) async fn inspect_reset_scope(
    state: &AppState,
    body: ResetBody,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let claim = normalize_reset_scope(body)?;
    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };
    let run = crate::db::auto_queue::get_run_pg(pool, &claim.run_id)
        .await
        .map_err(|error| {
            reset_contract_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reset_run_load_failed",
                format!("load auto-queue run '{}': {error}", claim.run_id),
            )
        })?
        .ok_or_else(|| {
            reset_contract_error(
                StatusCode::NOT_FOUND,
                "reset_run_not_found",
                format!("auto-queue run '{}' not found", claim.run_id),
            )
        })?;
    let filter = crate::db::auto_queue::StatusFilter {
        repo: claim.repo,
        agent_id: claim.agent_id,
    };
    let owns_run = crate::db::auto_queue::run_matches_scope_claim_pg(pool, &claim.run_id, &filter)
        .await
        .map_err(|error| {
            reset_contract_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reset_scope_check_failed",
                format!("check auto-queue reset scope ownership: {error}"),
            )
        })?;
    if !owns_run {
        return Err(reset_contract_error(
            StatusCode::CONFLICT,
            "reset_scope_mismatch",
            format!("reset scope does not own auto-queue run '{}'", claim.run_id),
        ));
    }

    require_terminal_status(&run.status)?;
    let residual = load_terminal_residual_pg(pool, &claim.run_id)
        .await
        .map_err(|error| {
            reset_contract_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reset_residual_inspection_failed",
                error,
            )
        })?;

    Ok((
        StatusCode::OK,
        Json(terminal_reset_response(
            &claim.run_id,
            &run.status,
            residual,
        )),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(run_id: Option<&str>, repo: Option<&str>, agent_id: Option<&str>) -> ResetBody {
        ResetBody {
            run_id: run_id.map(str::to_string),
            repo: repo.map(str::to_string),
            agent_id: agent_id.map(str::to_string),
        }
    }

    #[test]
    fn reset_requires_a_nonblank_run_id_before_database_access() {
        for run_id in [None, Some(""), Some("  ")] {
            let result = normalize_reset_scope(body(run_id, Some("owner/repo"), None));
            assert!(
                result.is_err(),
                "missing run_id must fail closed: {result:?}"
            );
            let Err(error) = result else { return };
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                error.context().get("reset_code"),
                Some(&json!("reset_run_id_required"))
            );
        }
    }

    #[test]
    fn reset_requires_at_least_one_nonblank_scope_claim() {
        let result = normalize_reset_scope(body(Some("run-1"), Some(" "), Some("")));
        assert!(
            result.is_err(),
            "vacuous ownership claims must fail closed: {result:?}"
        );
        let Err(error) = result else { return };
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            error.context().get("reset_code"),
            Some(&json!("reset_scope_claim_required"))
        );
    }

    #[test]
    fn every_canonical_nonterminal_status_is_conflict_and_points_to_end() {
        for status in ["generated", "pending", "active", "paused", "restoring"] {
            let result = require_terminal_status(status);
            assert!(
                result.is_err(),
                "reset must never mutate a nonterminal run: {result:?}"
            );
            let Err(error) = result else { return };
            assert_eq!(error.status(), StatusCode::CONFLICT, "status={status}");
            assert_eq!(error.context().get("mutates"), Some(&json!(false)));
            assert_eq!(error.context().get("hint"), Some(&json!(END_RUN_HINT)));
        }
    }

    #[test]
    fn only_terminal_statuses_are_inspectable() {
        assert!(require_terminal_status("completed").is_ok());
        assert!(require_terminal_status("cancelled").is_ok());
        let future = require_terminal_status("future_status");
        assert!(
            future.is_err(),
            "unknown statuses must fail closed: {future:?}"
        );
        let Err(error) = future else { return };
        assert_eq!(error.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn reset_scope_module_contains_no_mutation_or_lock_path() {
        let source = include_str!("reset_scope.rs");
        let production_source = source.split("#[cfg(test)]").next();
        assert!(
            production_source.is_some(),
            "reset_scope production source missing"
        );
        let Some(production_source) = production_source else {
            return;
        };
        for forbidden in [
            "end_run_with_pg(",
            "cancel_run_with_pg(",
            ".begin()",
            "FOR UPDATE",
            "UPDATE auto_queue_",
            "DELETE FROM auto_queue_",
        ] {
            assert!(
                !production_source.contains(forbidden),
                "reset inspection must not contain mutation path {forbidden:?}"
            );
        }
    }

    #[test]
    fn terminal_response_reports_raw_residuals_without_summary_judgment() {
        let response = terminal_reset_response(
            "run-terminal",
            "completed",
            json!({
                "live_dispatches": 0,
                "entries_by_status": {},
                "open_cleanup_tasks": 0,
                "cards_in_progress": 0,
            }),
        );

        assert_eq!(response["mutates"], false);
        assert_eq!(response["residual"]["live_dispatches"], 0);
        assert_eq!(response["residual"]["entries_by_status"], json!({}));
        assert_eq!(response["residual"]["open_cleanup_tasks"], 0);
        assert_eq!(response["residual"]["cards_in_progress"], 0);
        assert!(response.get("clean").is_none());
        assert!(response.get("restorable").is_none());
        assert_eq!(response["hint"], END_RUN_HINT);
    }
}
