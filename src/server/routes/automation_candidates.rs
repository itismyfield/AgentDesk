use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::json;

use super::AppState;
use crate::services::automation_candidate_materializer::{
    AutomationCandidateMaterializer, IterationResultInput, MaterializeCandidateInput,
    MaterializerError, PrepareWorktreeOutput,
};

fn pg_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres pool not configured"})),
    )
}

fn materializer_error_response(error: MaterializerError) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        MaterializerError::CardNotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "automation candidate card not found"})),
        ),
        MaterializerError::MissingProgram(msg) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": msg, "code": "MISSING_PROGRAM_CONTRACT"})),
        ),
        MaterializerError::AllowedPathsViolation { path } => (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": format!("path '{}' is not in allowed_write_paths", path),
                "code": "ALLOWED_PATHS_VIOLATION",
                "path": path,
            })),
        ),
        MaterializerError::DuplicateIteration => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "iteration result already recorded for this card/iteration",
                "code": "DUPLICATE_ITERATION",
            })),
        ),
        MaterializerError::WorktreeError(msg) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": msg, "code": "WORKTREE_ERROR"})),
        ),
        MaterializerError::Database(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": msg})),
        ),
    }
}

/// POST /api/automation-candidates
///
/// Materialize a loop-enabled automation candidate Kanban card.
/// A card enters the executor loop only when:
/// `pipeline_stage_id='automation-candidate'`,
/// `metadata.automation_candidate.enabled=true`,
/// and `metadata.program` contains the required contract fields.
pub async fn materialize_candidate(
    State(state): State<AppState>,
    Json(body): Json<MaterializeCandidateInput>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let materializer = AutomationCandidateMaterializer::new(pool.clone());
    match materializer.materialize_candidate(body).await {
        Ok(output) => {
            crate::server::ws::emit_event(
                &state.broadcast_tx,
                "automation_candidate_materialized",
                json!({
                    "card_id": output.card_id,
                    "created": output.created,
                    "status": output.status,
                    "start_ready": output.start_ready,
                }),
            );
            (
                if output.created {
                    StatusCode::CREATED
                } else {
                    StatusCode::OK
                },
                Json(json!({
                    "card_id": output.card_id,
                    "created": output.created,
                    "status": output.status,
                    "pipeline_stage_id": output.pipeline_stage_id,
                    "start_ready": output.start_ready,
                    "discriminator": output.discriminator,
                })),
            )
        }
        Err(error) => materializer_error_response(error),
    }
}

/// POST /api/automation-candidates/{card_id}/iteration-result
///
/// LLM submits iteration result after running against the card's program contract.
/// Rust computes the keep/discard verdict deterministically; LLM cannot override it.
pub async fn submit_iteration_result(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    Json(body): Json<IterationResultInput>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    if body.iteration < 1 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "iteration must be >= 1"})),
        );
    }

    if body.branch.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "branch is required"})),
        );
    }

    let materializer = AutomationCandidateMaterializer::new(pool.clone());
    match materializer.submit_iteration_result(&card_id, body).await {
        Ok(output) => {
            crate::server::ws::emit_event(
                &state.broadcast_tx,
                "automation_candidate_iteration",
                json!({
                    "card_id": card_id,
                    "verdict": output.verdict,
                    "action": output.action,
                    "child_card_id": output.child_card_id,
                }),
            );
            (
                StatusCode::CREATED,
                Json(json!({
                    "record": output.record,
                    "verdict": output.verdict,
                    "action": output.action,
                    "child_card_id": output.child_card_id,
                })),
            )
        }
        Err(error) => materializer_error_response(error),
    }
}

/// GET /api/automation-candidates/{card_id}/iterations
///
/// List all iteration records for a card in chronological order.
pub async fn list_iterations(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let materializer = AutomationCandidateMaterializer::new(pool.clone());
    match materializer.list_iterations(&card_id).await {
        Ok(records) => (StatusCode::OK, Json(json!({"iterations": records}))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/automation-candidates/{card_id}/approve
///
/// Human (or auto_apply_after_green) approves the final iteration.
/// Sets review_status = 'approved'. Returns `final_gate` so callers know
/// whether to wait for CI (`auto_apply_after_green`) or just notify.
pub async fn approve_candidate(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let materializer = AutomationCandidateMaterializer::new(pool.clone());
    match materializer.approve_candidate(&card_id).await {
        Ok(output) => {
            crate::server::ws::emit_event(
                &state.broadcast_tx,
                "automation_candidate_approved",
                json!({
                    "card_id": card_id,
                    "final_gate": output.final_gate,
                    "effective_final_gate": output.effective_final_gate,
                    "safe_for_auto_apply": output.side_effect_simulation.safe_for_auto_apply,
                }),
            );
            (
                StatusCode::OK,
                Json(json!({
                    "status": "approved",
                    "card_id": card_id,
                    "final_gate": output.final_gate,
                    "effective_final_gate": output.effective_final_gate,
                    "next_action": output.next_action,
                    "side_effect_simulation": output.side_effect_simulation,
                })),
            )
        }
        Err(error) => materializer_error_response(error),
    }
}

#[derive(serde::Deserialize)]
pub struct PrepareWorktreeRequest {
    pub iteration: i32,
}

/// GET /api/automation-candidates/{card_id}/automation-inventory
///
/// Returns the per-card iteration history in the shape expected by
/// `ctx.automationInventory[cardId]` in the automation executor routine.
/// Each element contains `iteration`, `status`, `metric_before`, `metric_after`,
/// and `description` fields that the executor prompt uses to summarise past work.
pub async fn get_automation_inventory(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let materializer = AutomationCandidateMaterializer::new(pool.clone());
    match materializer.list_iterations(&card_id).await {
        Ok(records) => (
            StatusCode::OK,
            Json(json!({
                "card_id": card_id,
                "iterations": records,
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/automation-candidates/{card_id}/prepare-worktree
///
/// Creates (or returns existing) git worktree for the given iteration.
/// The executor calls this before dispatching an LLM agent so the agent
/// has an isolated branch to commit into.
///
/// Requires `metadata.program.repo_dir` on the card.
pub async fn prepare_worktree(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    Json(body): Json<PrepareWorktreeRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let materializer = AutomationCandidateMaterializer::new(pool.clone());
    match materializer
        .prepare_worktree(&card_id, body.iteration)
        .await
    {
        Ok(PrepareWorktreeOutput {
            path,
            branch,
            commit,
            created,
        }) => (
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(json!({
                "path": path,
                "branch": branch,
                "commit": commit,
                "created": created,
            })),
        ),
        Err(error) => materializer_error_response(error),
    }
}
