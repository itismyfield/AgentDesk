use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut sql = String::from(
        "SELECT id, repo_id, stage_name, stage_order, trigger_after, entry_skill,
                timeout_minutes, on_failure, skip_condition, provider,
                agent_override_id, on_failure_target, max_retries, parallel_with
         FROM pipeline_stages WHERE 1=1",
    );
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(ref repo) = params.repo {
        bind_values.push(repo.clone());
        sql.push_str(&format!(" AND repo_id = ?{}", bind_values.len()));
    }

    if let Some(ref agent_id) = params.agent_id {
        bind_values.push(agent_id.clone());
        sql.push_str(&format!(" AND agent_override_id = ?{}", bind_values.len()));
    }

    sql.push_str(" ORDER BY stage_order ASC");

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params_ref.as_slice(), |row| extended_stage_row_to_json(row))
        .ok();

    let stages: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"stages": stages})))
}

/// PUT /api/pipeline/stages — bulk replace stages for a repo
pub async fn put_stages(
    State(state): State<AppState>,
    Json(body): Json<PutStagesBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Transaction: delete all existing stages for the repo, then insert new ones
    if let Err(e) = conn.execute_batch("BEGIN TRANSACTION") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("begin tx: {e}")})),
        );
    }

    if let Err(e) = conn.execute(
        "DELETE FROM pipeline_stages WHERE repo_id = ?1",
        [&body.repo],
    ) {
        let _ = conn.execute_batch("ROLLBACK");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("delete: {e}")})),
        );
    }

    for (idx, stage) in body.stages.iter().enumerate() {
        let order = stage.stage_order.unwrap_or(idx as i64 + 1);
        let timeout = stage.timeout_minutes.unwrap_or(60);
        let on_failure = stage.on_failure.as_deref().unwrap_or("fail");
        let max_retries = stage.max_retries.unwrap_or(0);

        if let Err(e) = conn.execute(
            "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, entry_skill,
                timeout_minutes, on_failure, skip_condition, provider, agent_override_id,
                on_failure_target, max_retries, parallel_with)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
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
                Json(json!({"error": format!("insert stage '{}': {e}", stage.stage_name)})),
            );
        }
    }

    if let Err(e) = conn.execute_batch("COMMIT") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("commit: {e}")})),
        );
    }

    // Read back inserted stages
    let mut stmt = match conn.prepare(
        "SELECT id, repo_id, stage_name, stage_order, trigger_after, entry_skill,
                timeout_minutes, on_failure, skip_condition, provider,
                agent_override_id, on_failure_target, max_retries, parallel_with
         FROM pipeline_stages WHERE repo_id = ?1 ORDER BY stage_order ASC",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("readback: {e}")})),
            );
        }
    };

    let rows = stmt
        .query_map([&body.repo], |row| extended_stage_row_to_json(row))
        .ok();

    let stages: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"stages": stages})))
}

/// DELETE /api/pipeline/stages?repo=...
pub async fn delete_stages(
    State(state): State<AppState>,
    Query(params): Query<DeleteStagesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.execute(
        "DELETE FROM pipeline_stages WHERE repo_id = ?1",
        [&params.repo],
    ) {
        Ok(n) => (StatusCode::OK, Json(json!({"deleted": true, "count": n}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// GET /api/pipeline/cards/{cardId} — card pipeline state with history
pub async fn get_card_pipeline(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // 1. Find the card and its repo_id
    let repo_id: Option<String> = match conn.query_row(
        "SELECT repo_id FROM kanban_cards WHERE id = ?1",
        [&card_id],
        |row| row.get(0),
    ) {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "card not found"})),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // 2. Get pipeline stages for the repo
    let stages: Vec<serde_json::Value> = if let Some(ref rid) = repo_id {
        let mut stmt = match conn.prepare(
            "SELECT id, repo_id, stage_name, stage_order, trigger_after, entry_skill,
                    timeout_minutes, on_failure, skip_condition, provider,
                    agent_override_id, on_failure_target, max_retries, parallel_with
             FROM pipeline_stages WHERE repo_id = ?1 ORDER BY stage_order ASC",
        ) {
            Ok(s) => s,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "failed to query stages"})),
                );
            }
        };

        stmt.query_map([rid], |row| extended_stage_row_to_json(row))
            .ok()
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // 3. Get dispatch history for the card
    let mut hist_stmt = match conn.prepare(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type,
                status, title, context, result, created_at, updated_at
         FROM task_dispatches WHERE kanban_card_id = ?1 ORDER BY created_at ASC",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("history query: {e}")})),
            );
        }
    };

    let history: Vec<serde_json::Value> = hist_stmt
        .query_map([&card_id], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kanban_card_id": row.get::<_, Option<String>>(1)?,
                "from_agent_id": row.get::<_, Option<String>>(2)?,
                "to_agent_id": row.get::<_, Option<String>>(3)?,
                "dispatch_type": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, Option<String>>(5)?,
                "title": row.get::<_, Option<String>>(6)?,
                "context": row.get::<_, Option<String>>(7)?,
                "result": row.get::<_, Option<String>>(8)?,
                "created_at": row.get::<_, Option<String>>(9)?,
                "updated_at": row.get::<_, Option<String>>(10)?,
            }))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    // 4. Determine current_stage by matching the most recent non-completed dispatch's
    //    dispatch_type or title against stage entry_skill names
    let current_stage: serde_json::Value = if !history.is_empty() && !stages.is_empty() {
        // Find the most recent active dispatch (pending/running)
        let active_dispatch = history.iter().rev().find(|d| {
            let s = d["status"].as_str().unwrap_or("");
            s == "pending" || s == "running" || s == "in_progress"
        });

        if let Some(dispatch) = active_dispatch {
            let dtype = dispatch["dispatch_type"].as_str().unwrap_or("");
            let title = dispatch["title"].as_str().unwrap_or("");

            // Match against stage entry_skill or stage_name
            stages
                .iter()
                .find(|st| {
                    let skill = st["entry_skill"].as_str().unwrap_or("");
                    let name = st["stage_name"].as_str().unwrap_or("");
                    (!skill.is_empty() && (skill == dtype || skill == title))
                        || (!name.is_empty() && (name == dtype || name == title))
                })
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Null
        }
    } else {
        serde_json::Value::Null
    };

    (
        StatusCode::OK,
        Json(json!({
            "stages": stages,
            "history": history,
            "current_stage": current_stage,
        })),
    )
}

/// GET /api/pipeline/cards/{cardId}/history
pub async fn get_card_history(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT id, dispatch_type, status, from_agent_id, to_agent_id, title, result, created_at, updated_at
         FROM task_dispatches WHERE kanban_card_id = ?1 ORDER BY created_at ASC",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            )
        }
    };

    let history: Vec<serde_json::Value> = stmt
        .query_map([&card_id], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "dispatch_type": row.get::<_, Option<String>>(1)?,
                "status": row.get::<_, Option<String>>(2)?,
                "from_agent_id": row.get::<_, Option<String>>(3)?,
                "to_agent_id": row.get::<_, Option<String>>(4)?,
                "title": row.get::<_, Option<String>>(5)?,
                "result": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, Option<String>>(7)?,
                "updated_at": row.get::<_, Option<String>>(8)?,
            }))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    (StatusCode::OK, Json(json!({"history": history})))
}

/// GET /api/pipeline/cards/{cardId}/transcripts?limit=10
pub async fn get_card_transcripts(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    Query(params): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let card_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .unwrap_or(false);
    if !card_exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    }

    match crate::db::session_transcripts::list_transcripts_for_card(
        &conn,
        &card_id,
        params.limit.unwrap_or(10),
    ) {
        Ok(transcripts) => (
            StatusCode::OK,
            Json(json!({
                "card_id": card_id,
                "transcripts": transcripts,
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query: {e}")})),
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let effective = crate::pipeline::resolve_for_card(
        &conn,
        params.repo.as_deref(),
        params.agent_id.as_deref(),
    );

    // Also return which layers had overrides
    let repo_has_override = params.repo.as_ref().map_or(false, |rid| {
        conn.query_row(
            "SELECT pipeline_config IS NOT NULL AND pipeline_config != '' FROM github_repos WHERE id = ?1",
            [rid],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
    });
    let agent_has_override = params.agent_id.as_ref().map_or(false, |aid| {
        conn.query_row(
            "SELECT pipeline_config IS NOT NULL AND pipeline_config != '' FROM agents WHERE id = ?1",
            [aid],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
    });

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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let config: Option<String> = conn
        .query_row(
            "SELECT pipeline_config FROM github_repos WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap_or(None);

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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Validate the override parses correctly if provided
    let config_str = match &body.config {
        Some(v) if !v.is_null() => {
            let s = v.to_string();
            if let Err(e) = crate::pipeline::parse_override(&s) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid pipeline config: {e}")})),
                );
            }
            Some(s)
        }
        _ => None,
    };

    match conn.execute(
        "UPDATE github_repos SET pipeline_config = ?1 WHERE id = ?2",
        rusqlite::params![config_str, id],
    ) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "repo not found"})),
        ),
        Ok(_) => {
            // Validate the merged effective pipeline
            let effective = crate::pipeline::resolve_for_card(&conn, Some(&id), None);
            if let Err(e) = effective.validate() {
                // Rollback: clear the override since merge is invalid
                let _ = conn.execute(
                    "UPDATE github_repos SET pipeline_config = NULL WHERE id = ?1",
                    rusqlite::params![id],
                );
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("merged pipeline validation failed: {e}")})),
                );
            }
            (StatusCode::OK, Json(json!({"ok": true, "repo": id})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// GET /api/pipeline/config/agent/:agent_id
pub async fn get_agent_pipeline(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let config: Option<String> = conn
        .query_row(
            "SELECT pipeline_config FROM agents WHERE id = ?1",
            [&agent_id],
            |row| row.get(0),
        )
        .unwrap_or(None);

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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let config_str = match &body.config {
        Some(v) if !v.is_null() => {
            let s = v.to_string();
            if let Err(e) = crate::pipeline::parse_override(&s) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid pipeline config: {e}")})),
                );
            }
            Some(s)
        }
        _ => None,
    };

    match conn.execute(
        "UPDATE agents SET pipeline_config = ?1 WHERE id = ?2",
        rusqlite::params![config_str, agent_id],
    ) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        ),
        Ok(_) => {
            // Look up repo_id for this agent to validate merged pipeline
            let repo_id: Option<String> = conn
                .query_row(
                    "SELECT repo_id FROM agents WHERE id = ?1",
                    [&agent_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            let effective =
                crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), Some(&agent_id));
            if let Err(e) = effective.validate() {
                // Rollback: clear the override
                let _ = conn.execute(
                    "UPDATE agents SET pipeline_config = NULL WHERE id = ?1",
                    rusqlite::params![agent_id],
                );
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("merged pipeline validation failed: {e}")})),
                );
            }
            (
                StatusCode::OK,
                Json(json!({"ok": true, "agent_id": agent_id})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let effective = crate::pipeline::resolve_for_card(
        &conn,
        params.repo.as_deref(),
        params.agent_id.as_deref(),
    );

    (StatusCode::OK, Json(effective.to_graph()))
}

/// Extended version that includes dashboard v2 columns
fn extended_stage_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    let repo_id = row.get::<_, Option<String>>(1)?;
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "repo_id": repo_id,
        "repo": repo_id,  // alias for frontend
        "stage_name": row.get::<_, Option<String>>(2)?,
        "stage_order": row.get::<_, i64>(3)?,
        "trigger_after": row.get::<_, Option<String>>(4)?,
        "entry_skill": row.get::<_, Option<String>>(5)?,
        "timeout_minutes": row.get::<_, i64>(6)?,
        "on_failure": row.get::<_, Option<String>>(7)?,
        "skip_condition": row.get::<_, Option<String>>(8)?,
        "provider": row.get::<_, Option<String>>(9)?,
        "agent_override_id": row.get::<_, Option<String>>(10)?,
        "on_failure_target": row.get::<_, Option<String>>(11)?,
        "max_retries": row.get::<_, Option<i64>>(12)?,
        "parallel_with": row.get::<_, Option<String>>(13)?,
    }))
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
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new(&config, db.clone()).unwrap()
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
                "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
                 VALUES ('card-transcript-1', 'Transcript Card', 'in_progress', datetime('now'), datetime('now'))",
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
        assert_eq!(body["transcripts"][0]["events"][0]["kind"], "result");
        assert_eq!(body["transcripts"][0]["duration_ms"], 6100);
    }
}
