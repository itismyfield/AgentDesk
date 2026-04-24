use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, Row};

use super::AppState;
use crate::db::table_metadata;

/// #1097 (910-3) — readonly guard.
///
/// Returns an HTTP 405 response when `db_source_of_truth(table) == 'file'`
/// or `'file-canonical'`.  Call at the top of any mutating route that
/// targets a table whose canonical source lives on disk.
async fn reject_if_readonly(
    state: &AppState,
    table: &str,
) -> Option<(StatusCode, Json<serde_json::Value>)> {
    let source = if let Some(pool) = state.pg_pool_ref() {
        table_metadata::source_of_truth_pg(pool, table).await
    } else {
        match state.sqlite_db().read_conn() {
            Ok(conn) => table_metadata::source_of_truth_sqlite(&conn, table),
            Err(_) => None,
        }
    };

    match source {
        Some(s) if s.is_readonly() => Some((
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({
                "error": format!(
                    "table '{}' is file-canonical; edit policies/default-pipeline.yaml \
                     and restart the server to apply changes",
                    table
                ),
                "table": table,
                "source_of_truth": match s {
                    table_metadata::Source::File => "file",
                    table_metadata::Source::FileCanonical => "file-canonical",
                    table_metadata::Source::Db => "db",
                },
            })),
        )),
        _ => None,
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
pub struct PutStageItem {
    pub stage_name: String,
    pub stage_order: Option<i64>,
    pub trigger_after: Option<String>,
    pub entry_skill: Option<String>,
    pub provider: Option<String>,
    pub agent_override_id: Option<String>,
    pub timeout_minutes: Option<i64>,
    pub on_failure: Option<String>,
    pub on_failure_target: Option<String>,
    pub max_retries: Option<i64>,
    pub skip_condition: Option<String>,
    pub parallel_with: Option<String>,
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
) -> (StatusCode, Json<serde_json::Value>) {
    let stages = if let Some(pool) = state.pg_pool_ref() {
        match list_pipeline_stages_pg(pool, params.repo.as_deref(), params.agent_id.as_deref())
            .await
        {
            Ok(stages) => stages,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };
        match list_pipeline_stages_sqlite(&conn, params.repo.as_deref(), params.agent_id.as_deref())
        {
            Ok(stages) => stages,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    };

    (StatusCode::OK, Json(json!({ "stages": stages })))
}

/// PUT /api/pipeline/stages — bulk replace stages for a repo
pub async fn put_stages(
    State(state): State<AppState>,
    Json(body): Json<PutStagesBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // #1097: reject when pipeline_stages is file-canonical.
    if let Some(resp) = reject_if_readonly(&state, "pipeline_stages").await {
        return resp;
    }

    let stages = if let Some(pool) = state.pg_pool_ref() {
        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("begin tx: {error}")})),
                );
            }
        };

        if let Err(error) = sqlx::query("DELETE FROM pipeline_stages WHERE repo_id = $1")
            .bind(&body.repo)
            .execute(&mut *tx)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("delete: {error}")})),
            );
        }

        for (idx, stage) in body.stages.iter().enumerate() {
            let order = stage.stage_order.unwrap_or(idx as i64 + 1);
            let timeout = stage.timeout_minutes.unwrap_or(60);
            let on_failure = stage.on_failure.as_deref().unwrap_or("fail");
            let max_retries = stage.max_retries.unwrap_or(0);

            if let Err(error) = sqlx::query(
                "INSERT INTO pipeline_stages (
                    repo_id, stage_name, stage_order, trigger_after, entry_skill,
                    timeout_minutes, on_failure, skip_condition, provider, agent_override_id,
                    on_failure_target, max_retries, parallel_with
                 ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13
                 )",
            )
            .bind(&body.repo)
            .bind(&stage.stage_name)
            .bind(order)
            .bind(stage.trigger_after.as_deref())
            .bind(stage.entry_skill.as_deref())
            .bind(timeout)
            .bind(on_failure)
            .bind(stage.skip_condition.as_deref())
            .bind(stage.provider.as_deref())
            .bind(stage.agent_override_id.as_deref())
            .bind(stage.on_failure_target.as_deref())
            .bind(max_retries)
            .bind(stage.parallel_with.as_deref())
            .execute(&mut *tx)
            .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("insert stage '{}': {error}", stage.stage_name)})),
                );
            }
        }

        if let Err(error) = tx.commit().await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("commit: {error}")})),
            );
        }

        match list_pipeline_stages_pg(pool, Some(&body.repo), None).await {
            Ok(stages) => stages,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        if let Err(error) = conn.execute_batch("BEGIN TRANSACTION") {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("begin tx: {error}")})),
            );
        }

        if let Err(error) = conn.execute(
            "DELETE FROM pipeline_stages WHERE repo_id = ?1",
            [&body.repo],
        ) {
            let _ = conn.execute_batch("ROLLBACK");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("delete: {error}")})),
            );
        }

        for (idx, stage) in body.stages.iter().enumerate() {
            let order = stage.stage_order.unwrap_or(idx as i64 + 1);
            let timeout = stage.timeout_minutes.unwrap_or(60);
            let on_failure = stage.on_failure.as_deref().unwrap_or("fail");
            let max_retries = stage.max_retries.unwrap_or(0);

            if let Err(error) = conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, entry_skill,
                    timeout_minutes, on_failure, skip_condition, provider, agent_override_id,
                    on_failure_target, max_retries, parallel_with)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                libsql_rusqlite::params![
                    body.repo,
                    stage.stage_name,
                    order,
                    stage.trigger_after,
                    stage.entry_skill,
                    timeout,
                    on_failure,
                    stage.skip_condition,
                    stage.provider,
                    stage.agent_override_id,
                    stage.on_failure_target,
                    max_retries,
                    stage.parallel_with,
                ],
            ) {
                let _ = conn.execute_batch("ROLLBACK");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("insert stage '{}': {error}", stage.stage_name)})),
                );
            }
        }

        if let Err(error) = conn.execute_batch("COMMIT") {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("commit: {error}")})),
            );
        }

        match list_pipeline_stages_sqlite(&conn, Some(&body.repo), None) {
            Ok(stages) => stages,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    };

    (StatusCode::OK, Json(json!({ "stages": stages })))
}

/// DELETE /api/pipeline/stages?repo=...
pub async fn delete_stages(
    State(state): State<AppState>,
    Query(params): Query<DeleteStagesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    // #1097: reject when pipeline_stages is file-canonical.
    if let Some(resp) = reject_if_readonly(&state, "pipeline_stages").await {
        return resp;
    }

    if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query("DELETE FROM pipeline_stages WHERE repo_id = $1")
            .bind(&params.repo)
            .execute(pool)
            .await
        {
            Ok(result) => (
                StatusCode::OK,
                Json(json!({"deleted": true, "count": result.rows_affected()})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        }
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        match conn.execute(
            "DELETE FROM pipeline_stages WHERE repo_id = ?1",
            [&params.repo],
        ) {
            Ok(count) => (
                StatusCode::OK,
                Json(json!({"deleted": true, "count": count})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        }
    }
}

/// GET /api/pipeline/cards/{card_id} — card pipeline state with history
pub async fn get_card_pipeline(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (repo_id, stages, history) = if let Some(pool) = state.pg_pool_ref() {
        let repo_id = match sqlx::query_scalar::<_, Option<String>>(
            "SELECT repo_id FROM kanban_cards WHERE id = $1",
        )
        .bind(&card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(repo_id)) => repo_id,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        let stages = if let Some(repo_id) = repo_id.as_deref() {
            match list_pipeline_stages_pg(pool, Some(repo_id), None).await {
                Ok(stages) => stages,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": error})),
                    );
                }
            }
        } else {
            Vec::new()
        };

        let history = match list_card_pipeline_history_pg(pool, &card_id).await {
            Ok(history) => history,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

        (repo_id, stages, history)
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        let repo_id: Option<String> = match conn.query_row(
            "SELECT repo_id FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| row.get(0),
        ) {
            Ok(repo_id) => repo_id,
            Err(libsql_rusqlite::Error::QueryReturnedNoRows) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        let stages = if let Some(repo_id) = repo_id.as_deref() {
            match list_pipeline_stages_sqlite(&conn, Some(repo_id), None) {
                Ok(stages) => stages,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": error})),
                    );
                }
            }
        } else {
            Vec::new()
        };

        let history = match list_card_pipeline_history_sqlite(&conn, &card_id) {
            Ok(history) => history,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

        (repo_id, stages, history)
    };

    let current_stage = find_current_stage(&stages, &history);

    (
        StatusCode::OK,
        Json(json!({
            "repo_id": repo_id,
            "stages": stages,
            "history": history,
            "current_stage": current_stage,
        })),
    )
}

/// GET /api/pipeline/cards/{card_id}/history
pub async fn get_card_history(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let history = if let Some(pool) = state.pg_pool_ref() {
        match list_card_history_pg(pool, &card_id).await {
            Ok(history) => history,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };
        match list_card_history_sqlite(&conn, &card_id) {
            Ok(history) => history,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    };

    (StatusCode::OK, Json(json!({"history": history})))
}

/// GET /api/pipeline/cards/{card_id}/transcripts?limit=10
pub async fn get_card_transcripts(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    Query(params): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let card_exists = if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query("SELECT COUNT(*)::BIGINT AS count FROM kanban_cards WHERE id = $1")
            .bind(&card_id)
            .fetch_one(pool)
            .await
        {
            Ok(row) => row.try_get::<i64, _>("count").unwrap_or(0) > 0,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }
    } else {
        let conn = match state.sqlite_db().read_conn() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        conn.query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .unwrap_or(false)
    };
    if !card_exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    }

    match crate::db::session_transcripts::list_transcripts_for_card_db(
        state.sqlite_db(),
        state.pg_pool_ref(),
        &card_id,
        params.limit.unwrap_or(10),
    )
    .await
    {
        Ok(transcripts) => (
            StatusCode::OK,
            Json(json!({
                "card_id": card_id,
                "transcripts": transcripts,
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("transcripts: {e}")})),
        ),
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
pub async fn get_default_pipeline() -> (StatusCode, Json<serde_json::Value>) {
    match crate::pipeline::try_get() {
        Some(p) => (StatusCode::OK, Json(p.to_json())),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "default pipeline not loaded"})),
        ),
    }
}

/// GET /api/pipeline/config/effective?repo=...&agent_id=...
/// Returns the merged effective pipeline for a repo/agent combination.
pub async fn get_effective_pipeline(
    State(state): State<AppState>,
    Query(params): Query<EffectivePipelineQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if crate::pipeline::try_get().is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "default pipeline not loaded"})),
        );
    }

    let (effective, repo_has_override, agent_has_override) = if let Some(pool) = state.pg_pool_ref()
    {
        let effective = crate::pipeline::resolve_for_card_pg(
            pool,
            params.repo.as_deref(),
            params.agent_id.as_deref(),
        )
        .await;

        let repo_has_override = if let Some(repo_id) = params.repo.as_deref() {
            match sqlx::query_scalar::<_, bool>(
                "SELECT pipeline_config IS NOT NULL AND TRIM(pipeline_config::text) != ''
                 FROM github_repos
                 WHERE id = $1",
            )
            .bind(repo_id)
            .fetch_optional(pool)
            .await
            {
                Ok(Some(value)) => value,
                Ok(None) => false,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            }
        } else {
            false
        };

        let agent_has_override = if let Some(agent_id) = params.agent_id.as_deref() {
            match sqlx::query_scalar::<_, bool>(
                "SELECT pipeline_config IS NOT NULL AND TRIM(pipeline_config::text) != ''
                 FROM agents
                 WHERE id = $1",
            )
            .bind(agent_id)
            .fetch_optional(pool)
            .await
            {
                Ok(Some(value)) => value,
                Ok(None) => false,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            }
        } else {
            false
        };

        (effective, repo_has_override, agent_has_override)
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        let effective = crate::pipeline::resolve_for_card(
            &conn,
            params.repo.as_deref(),
            params.agent_id.as_deref(),
        );

        let repo_has_override = params.repo.as_ref().map_or(false, |repo_id| {
            conn.query_row(
                "SELECT pipeline_config IS NOT NULL AND pipeline_config != '' FROM github_repos WHERE id = ?1",
                [repo_id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
        });
        let agent_has_override = params.agent_id.as_ref().map_or(false, |agent_id| {
            conn.query_row(
                "SELECT pipeline_config IS NOT NULL AND pipeline_config != '' FROM agents WHERE id = ?1",
                [agent_id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
        });

        (effective, repo_has_override, agent_has_override)
    };

    (
        StatusCode::OK,
        Json(json!({
            "pipeline": effective.to_json(),
            "layers": {
                "default": true,
                "repo": repo_has_override,
                "agent": agent_has_override,
            },
        })),
    )
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
) -> (StatusCode, Json<serde_json::Value>) {
    let id = format!("{owner}/{repo}");
    let config = if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config FROM github_repos WHERE id = $1",
        )
        .bind(&id)
        .fetch_optional(pool)
        .await
        {
            Ok(config) => config.flatten(),
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        conn.query_row(
            "SELECT pipeline_config FROM github_repos WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap_or(None)
    };

    let parsed: serde_json::Value = config
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);

    (
        StatusCode::OK,
        Json(json!({"repo": id, "pipeline_config": parsed})),
    )
}

/// PUT /api/pipeline/config/repo/:owner/:repo
pub async fn set_repo_pipeline(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Json(body): Json<SetPipelineOverrideBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = format!("{owner}/{repo}");
    let config_str = match &body.config {
        Some(value) if !value.is_null() => {
            let config = value.to_string();
            if let Err(error) = crate::pipeline::parse_override(&config) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid pipeline config: {error}")})),
                );
            }
            Some(config)
        }
        _ => None,
    };

    let response = {
        if let Some(pool) = state.pg_pool_ref() {
            match sqlx::query("UPDATE github_repos SET pipeline_config = $1::jsonb WHERE id = $2")
                .bind(config_str.as_deref())
                .bind(&id)
                .execute(pool)
                .await
            {
                Ok(result) if result.rows_affected() == 0 => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "repo not found"})),
                ),
                Ok(_) => {
                    let effective =
                        crate::pipeline::resolve_for_card_pg(pool, Some(&id), None).await;
                    if let Err(error) = effective.validate() {
                        let _ = sqlx::query(
                            "UPDATE github_repos SET pipeline_config = NULL WHERE id = $1",
                        )
                        .bind(&id)
                        .execute(pool)
                        .await;
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(
                                json!({"error": format!("merged pipeline validation failed: {error}")}),
                            ),
                        );
                    }
                    (StatusCode::OK, Json(json!({"ok": true, "repo": id})))
                }
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                ),
            }
        } else {
            let conn = match state.sqlite_db().lock() {
                Ok(c) => c,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            };

            match conn.execute(
                "UPDATE github_repos SET pipeline_config = ?1 WHERE id = ?2",
                libsql_rusqlite::params![config_str, id],
            ) {
                Ok(0) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "repo not found"})),
                ),
                Ok(_) => {
                    let effective = crate::pipeline::resolve_for_card(&conn, Some(&id), None);
                    if let Err(error) = effective.validate() {
                        let _ = conn.execute(
                            "UPDATE github_repos SET pipeline_config = NULL WHERE id = ?1",
                            libsql_rusqlite::params![id],
                        );
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(
                                json!({"error": format!("merged pipeline validation failed: {error}")}),
                            ),
                        );
                    }
                    (StatusCode::OK, Json(json!({"ok": true, "repo": id})))
                }
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                ),
            }
        }
    };

    if response.0 == StatusCode::OK {
        crate::pipeline::refresh_override_health_report(state.sqlite_db(), state.pg_pool_ref())
            .await;
    }
    response
}

/// GET /api/pipeline/config/agent/:agent_id
pub async fn get_agent_pipeline(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let config = if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config FROM agents WHERE id = $1",
        )
        .bind(&agent_id)
        .fetch_optional(pool)
        .await
        {
            Ok(config) => config.flatten(),
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        conn.query_row(
            "SELECT pipeline_config FROM agents WHERE id = ?1",
            [&agent_id],
            |row| row.get(0),
        )
        .unwrap_or(None)
    };

    let parsed: serde_json::Value = config
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);

    (
        StatusCode::OK,
        Json(json!({"agent_id": agent_id, "pipeline_config": parsed})),
    )
}

/// PUT /api/pipeline/config/agent/:agent_id
pub async fn set_agent_pipeline(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetPipelineOverrideBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let config_str = match &body.config {
        Some(value) if !value.is_null() => {
            let config = value.to_string();
            if let Err(error) = crate::pipeline::parse_override(&config) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid pipeline config: {error}")})),
                );
            }
            Some(config)
        }
        _ => None,
    };

    let response = {
        if let Some(pool) = state.pg_pool_ref() {
            match sqlx::query("UPDATE agents SET pipeline_config = $1::jsonb WHERE id = $2")
                .bind(config_str.as_deref())
                .bind(&agent_id)
                .execute(pool)
                .await
            {
                Ok(result) if result.rows_affected() == 0 => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                ),
                Ok(_) => {
                    let effective =
                        crate::pipeline::resolve_for_card_pg(pool, None, Some(&agent_id)).await;
                    if let Err(error) = effective.validate() {
                        let _ =
                            sqlx::query("UPDATE agents SET pipeline_config = NULL WHERE id = $1")
                                .bind(&agent_id)
                                .execute(pool)
                                .await;
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(
                                json!({"error": format!("merged pipeline validation failed: {error}")}),
                            ),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({"ok": true, "agent_id": agent_id})),
                    )
                }
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                ),
            }
        } else {
            let conn = match state.sqlite_db().lock() {
                Ok(c) => c,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            };

            match conn.execute(
                "UPDATE agents SET pipeline_config = ?1 WHERE id = ?2",
                libsql_rusqlite::params![config_str, agent_id],
            ) {
                Ok(0) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                ),
                Ok(_) => {
                    let effective = crate::pipeline::resolve_for_card(&conn, None, Some(&agent_id));
                    if let Err(error) = effective.validate() {
                        let _ = conn.execute(
                            "UPDATE agents SET pipeline_config = NULL WHERE id = ?1",
                            libsql_rusqlite::params![agent_id],
                        );
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(
                                json!({"error": format!("merged pipeline validation failed: {error}")}),
                            ),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({"ok": true, "agent_id": agent_id})),
                    )
                }
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                ),
            }
        }
    };

    if response.0 == StatusCode::OK {
        crate::pipeline::refresh_override_health_report(state.sqlite_db(), state.pg_pool_ref())
            .await;
    }
    response
}

/// GET /api/pipeline/config/graph?repo=...&agent_id=...
/// Returns the effective pipeline as a visual graph (nodes + edges).
pub async fn get_pipeline_graph(
    State(state): State<AppState>,
    Query(params): Query<EffectivePipelineQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if crate::pipeline::try_get().is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "default pipeline not loaded"})),
        );
    }

    let effective = if let Some(pool) = state.pg_pool_ref() {
        crate::pipeline::resolve_for_card_pg(
            pool,
            params.repo.as_deref(),
            params.agent_id.as_deref(),
        )
        .await
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        };

        crate::pipeline::resolve_for_card(&conn, params.repo.as_deref(), params.agent_id.as_deref())
    };

    (StatusCode::OK, Json(effective.to_graph()))
}

fn stage_json(
    id: i64,
    repo_id: Option<String>,
    stage_name: Option<String>,
    stage_order: i64,
    trigger_after: Option<String>,
    entry_skill: Option<String>,
    timeout_minutes: i64,
    on_failure: Option<String>,
    skip_condition: Option<String>,
    provider: Option<String>,
    agent_override_id: Option<String>,
    on_failure_target: Option<String>,
    max_retries: Option<i64>,
    parallel_with: Option<String>,
) -> serde_json::Value {
    json!({
        "id": id,
        "repo_id": repo_id,
        "repo": repo_id,
        "stage_name": stage_name,
        "stage_order": stage_order,
        "trigger_after": trigger_after,
        "entry_skill": entry_skill,
        "timeout_minutes": timeout_minutes,
        "on_failure": on_failure,
        "skip_condition": skip_condition,
        "provider": provider,
        "agent_override_id": agent_override_id,
        "on_failure_target": on_failure_target,
        "max_retries": max_retries,
        "parallel_with": parallel_with,
    })
}

fn sqlite_stage_row_to_json(
    row: &libsql_rusqlite::Row,
) -> libsql_rusqlite::Result<serde_json::Value> {
    Ok(stage_json(
        row.get::<_, i64>(0)?,
        row.get::<_, Option<String>>(1)?,
        row.get::<_, Option<String>>(2)?,
        row.get::<_, i64>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, Option<String>>(5)?,
        row.get::<_, i64>(6)?,
        row.get::<_, Option<String>>(7)?,
        row.get::<_, Option<String>>(8)?,
        row.get::<_, Option<String>>(9)?,
        row.get::<_, Option<String>>(10)?,
        row.get::<_, Option<String>>(11)?,
        row.get::<_, Option<i64>>(12)?,
        row.get::<_, Option<String>>(13)?,
    ))
}

fn pg_stage_row_to_json(row: &sqlx::postgres::PgRow) -> Result<serde_json::Value, sqlx::Error> {
    let stage_order = row.try_get::<i64, _>("stage_order")?;
    let timeout_minutes = row.try_get::<i64, _>("timeout_minutes")?;
    let max_retries = row.try_get::<Option<i64>, _>("max_retries")?;

    Ok(stage_json(
        row.try_get::<i64, _>("id")?,
        row.try_get::<Option<String>, _>("repo_id")?,
        row.try_get::<Option<String>, _>("stage_name")?,
        stage_order,
        row.try_get::<Option<String>, _>("trigger_after")?,
        row.try_get::<Option<String>, _>("entry_skill")?,
        timeout_minutes,
        row.try_get::<Option<String>, _>("on_failure")?,
        row.try_get::<Option<String>, _>("skip_condition")?,
        row.try_get::<Option<String>, _>("provider")?,
        row.try_get::<Option<String>, _>("agent_override_id")?,
        row.try_get::<Option<String>, _>("on_failure_target")?,
        max_retries,
        row.try_get::<Option<String>, _>("parallel_with")?,
    ))
}

fn list_pipeline_stages_sqlite(
    conn: &libsql_rusqlite::Connection,
    repo: Option<&str>,
    agent_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let mut sql = String::from(
        "SELECT id, repo_id, stage_name, stage_order, trigger_after, entry_skill,
                timeout_minutes, on_failure, skip_condition, provider,
                agent_override_id, on_failure_target, max_retries, parallel_with
         FROM pipeline_stages WHERE 1=1",
    );
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(repo) = repo {
        bind_values.push(repo.to_string());
        sql.push_str(&format!(" AND repo_id = ?{}", bind_values.len()));
    }

    if let Some(agent_id) = agent_id {
        bind_values.push(agent_id.to_string());
        sql.push_str(&format!(" AND agent_override_id = ?{}", bind_values.len()));
    }

    sql.push_str(" ORDER BY stage_order ASC");

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| format!("prepare stages: {error}"))?;
    let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|value| value as &dyn libsql_rusqlite::types::ToSql)
        .collect();
    let rows = stmt
        .query_map(params_ref.as_slice(), sqlite_stage_row_to_json)
        .map_err(|error| format!("query stages: {error}"))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

async fn list_pipeline_stages_pg(
    pool: &PgPool,
    repo: Option<&str>,
    agent_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = sqlx::query(
        "SELECT id, repo_id, stage_name, stage_order, trigger_after, entry_skill,
                timeout_minutes, on_failure, skip_condition, provider,
                agent_override_id, on_failure_target, max_retries, parallel_with
         FROM pipeline_stages
         WHERE ($1::text IS NULL OR repo_id = $1)
           AND ($2::text IS NULL OR agent_override_id = $2)
         ORDER BY stage_order ASC",
    )
    .bind(repo)
    .bind(agent_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("query postgres stages: {error}"))?;

    rows.into_iter()
        .map(|row| {
            pg_stage_row_to_json(&row).map_err(|error| format!("decode postgres stage: {error}"))
        })
        .collect()
}

fn dispatch_pipeline_history_json(
    id: String,
    kanban_card_id: Option<String>,
    from_agent_id: Option<String>,
    to_agent_id: Option<String>,
    dispatch_type: Option<String>,
    status: Option<String>,
    title: Option<String>,
    context: Option<String>,
    result: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
) -> serde_json::Value {
    json!({
        "id": id,
        "kanban_card_id": kanban_card_id,
        "from_agent_id": from_agent_id,
        "to_agent_id": to_agent_id,
        "dispatch_type": dispatch_type,
        "status": status,
        "title": title,
        "context": context,
        "result": result,
        "created_at": created_at,
        "updated_at": updated_at,
    })
}

fn dispatch_history_json(
    id: String,
    dispatch_type: Option<String>,
    status: Option<String>,
    from_agent_id: Option<String>,
    to_agent_id: Option<String>,
    title: Option<String>,
    result: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
) -> serde_json::Value {
    json!({
        "id": id,
        "dispatch_type": dispatch_type,
        "status": status,
        "from_agent_id": from_agent_id,
        "to_agent_id": to_agent_id,
        "title": title,
        "result": result,
        "created_at": created_at,
        "updated_at": updated_at,
    })
}

fn list_card_pipeline_history_sqlite(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type,
                    status, title, context, result, created_at, updated_at
             FROM task_dispatches WHERE kanban_card_id = ?1 ORDER BY created_at ASC",
        )
        .map_err(|error| format!("history query: {error}"))?;
    let rows = stmt
        .query_map([card_id], |row| {
            Ok(dispatch_pipeline_history_json(
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .map_err(|error| format!("history query: {error}"))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

async fn list_card_pipeline_history_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = sqlx::query(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type,
                status, title, context, result, created_at::text AS created_at, updated_at::text AS updated_at
         FROM task_dispatches
         WHERE kanban_card_id = $1
         ORDER BY created_at ASC",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("history query: {error}"))?;
    rows.into_iter()
        .map(|row| {
            Ok(dispatch_pipeline_history_json(
                row.try_get::<String, _>("id")?,
                row.try_get::<Option<String>, _>("kanban_card_id")?,
                row.try_get::<Option<String>, _>("from_agent_id")?,
                row.try_get::<Option<String>, _>("to_agent_id")?,
                row.try_get::<Option<String>, _>("dispatch_type")?,
                row.try_get::<Option<String>, _>("status")?,
                row.try_get::<Option<String>, _>("title")?,
                row.try_get::<Option<String>, _>("context")?,
                row.try_get::<Option<String>, _>("result")?,
                row.try_get::<Option<String>, _>("created_at")?,
                row.try_get::<Option<String>, _>("updated_at")?,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|error| format!("decode history row: {error}"))
}

fn list_card_history_sqlite(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, dispatch_type, status, from_agent_id, to_agent_id, title, result, created_at, updated_at
             FROM task_dispatches WHERE kanban_card_id = ?1 ORDER BY created_at ASC",
        )
        .map_err(|error| format!("prepare: {error}"))?;
    let rows = stmt
        .query_map([card_id], |row| {
            Ok(dispatch_history_json(
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })
        .map_err(|error| format!("prepare: {error}"))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

async fn list_card_history_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = sqlx::query(
        "SELECT id, dispatch_type, status, from_agent_id, to_agent_id, title, result,
                created_at::text AS created_at, updated_at::text AS updated_at
         FROM task_dispatches
         WHERE kanban_card_id = $1
         ORDER BY created_at ASC",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("prepare: {error}"))?;
    rows.into_iter()
        .map(|row| {
            Ok(dispatch_history_json(
                row.try_get::<String, _>("id")?,
                row.try_get::<Option<String>, _>("dispatch_type")?,
                row.try_get::<Option<String>, _>("status")?,
                row.try_get::<Option<String>, _>("from_agent_id")?,
                row.try_get::<Option<String>, _>("to_agent_id")?,
                row.try_get::<Option<String>, _>("title")?,
                row.try_get::<Option<String>, _>("result")?,
                row.try_get::<Option<String>, _>("created_at")?,
                row.try_get::<Option<String>, _>("updated_at")?,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(|error| format!("decode history row: {error}"))
}

fn find_current_stage(
    stages: &[serde_json::Value],
    history: &[serde_json::Value],
) -> serde_json::Value {
    if history.is_empty() || stages.is_empty() {
        return serde_json::Value::Null;
    }

    let active_dispatch = history.iter().rev().find(|dispatch| {
        let status = dispatch["status"].as_str().unwrap_or("");
        status == "pending" || status == "running" || status == "in_progress"
    });

    let Some(dispatch) = active_dispatch else {
        return serde_json::Value::Null;
    };

    let dispatch_type = dispatch["dispatch_type"].as_str().unwrap_or("");
    let title = dispatch["title"].as_str().unwrap_or("");
    stages
        .iter()
        .find(|stage| {
            let skill = stage["entry_skill"].as_str().unwrap_or("");
            let name = stage["stage_name"].as_str().unwrap_or("");
            (!skill.is_empty() && (skill == dispatch_type || skill == title))
                || (!name.is_empty() && (name == dispatch_type || name == title))
        })
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::session_transcripts::{
        PersistSessionTranscript, SessionTranscriptEvent, SessionTranscriptEventKind,
        persist_turn_on_conn,
    };
    use crate::engine::PolicyEngine;

    fn test_db() -> Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    #[tokio::test]
    async fn get_card_transcripts_returns_linked_turns() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        {
            let mut conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('agent-card-transcript', 'Card Transcript Agent', 'codex', 'idle', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, github_issue_number, created_at, updated_at)
                 VALUES ('card-transcript-1', 'Transcript Card', 'in_progress', 525, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                 ) VALUES (
                    'dispatch-card-transcript-1', 'card-transcript-1', 'agent-card-transcript',
                    'implementation', 'completed', 'Transcript Dispatch', datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();

            let events = vec![SessionTranscriptEvent {
                kind: SessionTranscriptEventKind::Result,
                tool_name: None,
                summary: Some("done".to_string()),
                content: "viewer wired".to_string(),
                status: Some("success".to_string()),
                is_error: false,
            }];
            persist_turn_on_conn(
                &mut conn,
                PersistSessionTranscript {
                    turn_id: "discord:card-transcript:1",
                    session_key: Some("host:card-transcript"),
                    channel_id: Some("chan-card"),
                    agent_id: Some("agent-card-transcript"),
                    provider: Some("codex"),
                    dispatch_id: Some("dispatch-card-transcript-1"),
                    user_message: "wire transcript viewer",
                    assistant_message: "wired",
                    events: &events,
                    duration_ms: Some(6100),
                },
            )
            .unwrap();
        }

        let (status, Json(body)) = get_card_transcripts(
            State(state),
            Path("card-transcript-1".to_string()),
            Query(TranscriptQuery { limit: Some(5) }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["card_id"],
            serde_json::Value::String("card-transcript-1".to_string())
        );
        assert_eq!(
            body["transcripts"][0]["turn_id"],
            "discord:card-transcript:1"
        );
        assert_eq!(
            body["transcripts"][0]["dispatch_title"],
            "Transcript Dispatch"
        );
        assert_eq!(body["transcripts"][0]["card_title"], "Transcript Card");
        assert_eq!(body["transcripts"][0]["github_issue_number"], 525);
        assert_eq!(body["transcripts"][0]["events"][0]["kind"], "result");
        assert_eq!(body["transcripts"][0]["duration_ms"], 6100);
    }

    /// #1097 (910-3): pipeline_stages is file-canonical, so PUT/DELETE
    /// must be rejected with HTTP 405.
    #[tokio::test]
    async fn put_stages_rejected_when_db_source_of_truth_is_file() {
        let db = test_db();
        let engine = test_engine(&db);
        let state = AppState::test_state(db.clone(), engine);

        // pipeline_stages is seeded as file-canonical by the schema migrator.
        let body = PutStagesBody {
            repo: "owner/repo".to_string(),
            stages: vec![PutStageItem {
                stage_name: "build".to_string(),
                stage_order: Some(1),
                trigger_after: None,
                entry_skill: None,
                provider: None,
                agent_override_id: None,
                timeout_minutes: None,
                on_failure: None,
                on_failure_target: None,
                max_retries: None,
                skip_condition: None,
                parallel_with: None,
            }],
        };

        let (status, Json(json)) = put_stages(State(state.clone()), Json(body)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(json["table"], "pipeline_stages");
        assert_eq!(json["source_of_truth"], "file-canonical");
        assert!(
            json["error"]
                .as_str()
                .unwrap_or("")
                .contains("file-canonical")
        );

        let (status2, Json(json2)) = delete_stages(
            State(state),
            Query(DeleteStagesQuery {
                repo: "owner/repo".to_string(),
            }),
        )
        .await;
        assert_eq!(status2, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(json2["table"], "pipeline_stages");
    }
}
