use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::pipeline_override::{PipelineOverrideError, PipelineOverrideService};
pub use crate::services::pipeline_routes::PipelineStageInput as PutStageItem;
use crate::services::pipeline_routes::{PipelineRouteError, PipelineRouteService};

fn pg_unavailable() -> AppError {
    AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Database,
        "postgres pool not configured",
    )
}

fn pipeline_override_error(error: PipelineOverrideError) -> AppError {
    match error {
        PipelineOverrideError::BadRequest(error) => AppError::bad_request(error),
        PipelineOverrideError::NotFound(error) => AppError::not_found(error),
        PipelineOverrideError::Database(error) => {
            AppError::internal(error).with_code(ErrorCode::Database)
        }
    }
}

fn pipeline_route_error_response(
    error: PipelineRouteError,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    match error {
        PipelineRouteError::BadRequest { stage, error } => Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("stage '{stage}': {error}"),
                "stage": stage,
            })),
        )),
        PipelineRouteError::NotFound(error) => Err(AppError::not_found(error)),
        PipelineRouteError::Readonly { table, source } => Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({
                "error": format!(
                    "table '{}' is file-canonical; edit policies/default-pipeline.yaml \
                     and restart the server to apply changes",
                    table
                ),
                "table": table,
                "source_of_truth": source,
            })),
        )),
        PipelineRouteError::Database(error) => {
            Err(AppError::internal(error).with_code(ErrorCode::Database))
        }
    }
}

// ── Query / Body types ─────────────────────────────────────────

// ── Dashboard v2 types (/pipeline/...) ────────────────────────

#[derive(Debug, Deserialize)]
pub struct GetStagesQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteStagesQuery {
    pub repo: String,
}

#[derive(Debug, Deserialize)]
pub struct PutStagesBody {
    pub repo: String,
    pub stages: Vec<PutStageItem>,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptQuery {
    pub limit: Option<usize>,
}

// ── Dashboard v2 handlers ─────────────────────────────────────

/// GET /api/pipeline/stages?repo=...&agent_id=...
pub async fn get_stages(
    State(state): State<AppState>,
    Query(params): Query<GetStagesQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineRouteService::new(pool);
    let stages = match service
        .list_stages(params.repo.as_deref(), params.agent_id.as_deref())
        .await
    {
        Ok(stages) => stages,
        Err(error) => return pipeline_route_error_response(error),
    };

    Ok((StatusCode::OK, Json(json!({ "stages": stages }))))
}

/// PUT /api/pipeline/stages — bulk replace stages for a repo
pub async fn put_stages(
    State(state): State<AppState>,
    Json(body): Json<PutStagesBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineRouteService::new(pool);
    let stages = match service.replace_stages(&body.repo, &body.stages).await {
        Ok(stages) => stages,
        Err(error) => return pipeline_route_error_response(error),
    };

    Ok((StatusCode::OK, Json(json!({ "stages": stages }))))
}

/// DELETE /api/pipeline/stages?repo=...
pub async fn delete_stages(
    State(state): State<AppState>,
    Query(params): Query<DeleteStagesQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineRouteService::new(pool);
    match service.delete_stages(&params.repo).await {
        Ok(count) => Ok((
            StatusCode::OK,
            Json(json!({"deleted": true, "count": count})),
        )),
        Err(error) => pipeline_route_error_response(error),
    }
}

/// GET /api/pipeline/cards/{card_id} — card pipeline state with history
pub async fn get_card_pipeline(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };

    let service = PipelineRouteService::new(pool);
    let card_pipeline = match service.card_pipeline(&card_id).await {
        Ok(card_pipeline) => card_pipeline,
        Err(error) => return pipeline_route_error_response(error),
    };

    Ok((
        StatusCode::OK,
        Json(json!({
            "repo_id": card_pipeline.repo_id,
            "stages": card_pipeline.stages,
            "history": card_pipeline.history,
            "current_stage": card_pipeline.current_stage,
        })),
    ))
}

/// GET /api/pipeline/cards/{card_id}/history
pub async fn get_card_history(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineRouteService::new(pool);
    let history = match service.card_history(&card_id).await {
        Ok(history) => history,
        Err(error) => return pipeline_route_error_response(error),
    };

    Ok((StatusCode::OK, Json(json!({"history": history}))))
}

/// GET /api/pipeline/cards/{card_id}/transcripts?limit=10
pub async fn get_card_transcripts(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    Query(params): Query<TranscriptQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };

    let service = PipelineRouteService::new(pool);
    match service
        .card_transcripts(&card_id, params.limit.unwrap_or(10))
        .await
    {
        Ok(transcripts) => Ok((
            StatusCode::OK,
            Json(json!({
                "card_id": card_id,
                "transcripts": transcripts,
            })),
        )),
        Err(error) => pipeline_route_error_response(error),
    }
}

// ── Pipeline Config Hierarchy (#135) ─────────────────────────

/// Query params for effective pipeline resolution.
#[derive(Debug, Deserialize)]
pub struct EffectivePipelineQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

/// GET /api/pipeline/config/default — the base pipeline YAML as JSON
pub async fn get_default_pipeline() -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    match crate::pipeline::try_get() {
        Some(p) => Ok((StatusCode::OK, Json(p.to_json()))),
        None => Err(AppError::not_found("default pipeline not loaded")),
    }
}

/// GET /api/pipeline/config/effective?repo=...&agent_id=...
/// Returns the merged effective pipeline for a repo/agent combination.
pub async fn get_effective_pipeline(
    State(state): State<AppState>,
    Query(params): Query<EffectivePipelineQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineRouteService::new(pool);
    match service
        .effective_pipeline(params.repo.as_deref(), params.agent_id.as_deref())
        .await
    {
        Ok(effective) => Ok((StatusCode::OK, Json(effective))),
        Err(error) => pipeline_route_error_response(error),
    }
}

/// Body for setting pipeline override
#[derive(Debug, Deserialize)]
pub struct SetPipelineOverrideBody {
    pub config: Option<serde_json::Value>,
}

/// GET /api/pipeline/config/repo/:owner/:repo
pub async fn get_repo_pipeline(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let id = format!("{owner}/{repo}");
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineOverrideService::new(pool);
    let parsed = service
        .get_repo_pipeline(&id)
        .await
        .map_err(pipeline_override_error)?;

    Ok((
        StatusCode::OK,
        Json(json!({"repo": id, "pipeline_config": parsed})),
    ))
}

/// PUT /api/pipeline/config/repo/:owner/:repo
pub async fn set_repo_pipeline(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Json(body): Json<SetPipelineOverrideBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let id = format!("{owner}/{repo}");
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineOverrideService::new(pool);
    service
        .set_repo_pipeline(&id, body.config.as_ref())
        .await
        .map_err(pipeline_override_error)?;
    Ok((StatusCode::OK, Json(json!({"ok": true, "repo": id}))))
}

/// GET /api/pipeline/config/agent/:agent_id
pub async fn get_agent_pipeline(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineOverrideService::new(pool);
    let parsed = service
        .get_agent_pipeline(&agent_id)
        .await
        .map_err(pipeline_override_error)?;

    Ok((
        StatusCode::OK,
        Json(json!({"agent_id": agent_id, "pipeline_config": parsed})),
    ))
}

/// PUT /api/pipeline/config/agent/:agent_id
pub async fn set_agent_pipeline(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetPipelineOverrideBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineOverrideService::new(pool);
    service
        .set_agent_pipeline(&agent_id, body.config.as_ref())
        .await
        .map_err(pipeline_override_error)?;
    Ok((
        StatusCode::OK,
        Json(json!({"ok": true, "agent_id": agent_id})),
    ))
}

/// GET /api/pipeline/config/graph?repo=...&agent_id=...
/// Returns the effective pipeline as a visual graph (nodes + edges).
pub async fn get_pipeline_graph(
    State(state): State<AppState>,
    Query(params): Query<EffectivePipelineQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable());
    };
    let service = PipelineRouteService::new(pool);
    match service
        .pipeline_graph(params.repo.as_deref(), params.agent_id.as_deref())
        .await
    {
        Ok(graph) => Ok((StatusCode::OK, Json(graph))),
        Err(error) => pipeline_route_error_response(error),
    }
}
