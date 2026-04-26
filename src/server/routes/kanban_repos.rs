use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use super::AppState;

// ── Body types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateRepoBody {
    pub repo: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct UpdateRepoBody {
    pub default_agent_id: Option<String>,
    pub pipeline_config: Option<serde_json::Value>,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/kanban-repos
pub async fn list_repos(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    let rows = match sqlx::query(
        "SELECT id, display_name, sync_enabled, last_synced_at::text AS last_synced_at,
                default_agent_id, pipeline_config::text AS pipeline_config
         FROM github_repos
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    let repos: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            let pipeline_config = row
                .try_get::<Option<String>, _>("pipeline_config")
                .ok()
                .flatten()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .unwrap_or(serde_json::Value::Null);
            let id = row.try_get::<String, _>("id").unwrap_or_default();
            json!({
                "id": id.clone(),
                "repo": id,
                "display_name": row.try_get::<Option<String>, _>("display_name").ok().flatten(),
                "sync_enabled": row.try_get::<Option<bool>, _>("sync_enabled").ok().flatten().unwrap_or(true),
                "last_synced_at": row.try_get::<Option<String>, _>("last_synced_at").ok().flatten(),
                "default_agent_id": row.try_get::<Option<String>, _>("default_agent_id").ok().flatten(),
                "pipeline_config": pipeline_config,
                "created_at": 0,
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({"repos": repos})))
}

/// POST /api/kanban-repos
pub async fn create_repo(
    State(state): State<AppState>,
    Json(body): Json<CreateRepoBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.repo.is_empty() || !body.repo.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "repo must be in 'owner/name' format"})),
        );
    }

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let display_name = body
        .repo
        .split('/')
        .last()
        .unwrap_or(&body.repo)
        .to_string();

    if let Err(error) = crate::db::postgres::register_repo(pool, &body.repo).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        );
    }

    if let Err(error) = sqlx::query(
        "UPDATE github_repos
         SET display_name = CASE
             WHEN display_name IS NULL OR display_name = id THEN $2
             ELSE display_name
         END
         WHERE id = $1",
    )
    .bind(&body.repo)
    .bind(&display_name)
    .execute(pool)
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        );
    }

    match sqlx::query(
        "SELECT id, display_name, sync_enabled, last_synced_at::text AS last_synced_at, default_agent_id
         FROM github_repos
         WHERE id = $1",
    )
    .bind(&body.repo)
    .fetch_one(pool)
    .await
    {
        Ok(row) => (
            StatusCode::CREATED,
            Json(json!({"repo": {
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "display_name": row.try_get::<Option<String>, _>("display_name").ok().flatten(),
                "sync_enabled": row.try_get::<Option<bool>, _>("sync_enabled").ok().flatten().unwrap_or(true),
                "last_synced_at": row.try_get::<Option<String>, _>("last_synced_at").ok().flatten(),
                "default_agent_id": row.try_get::<Option<String>, _>("default_agent_id").ok().flatten(),
            }})),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        ),
    }
}

/// PATCH /api/kanban-repos/:owner/:repo
pub async fn update_repo(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Json(body): Json<UpdateRepoBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = format!("{owner}/{repo}");

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };
    if let Some(ref agent_id) = body.default_agent_id {
        match sqlx::query("UPDATE github_repos SET default_agent_id = $1 WHERE id = $2")
            .bind(agent_id)
            .bind(&id)
            .execute(pool)
            .await
        {
            Ok(result) if result.rows_affected() == 0 => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "repo not found"})),
                );
            }
            Ok(_) => {}
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
    } else {
        let exists =
            match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM github_repos WHERE id = $1")
                .bind(&id)
                .fetch_one(pool)
                .await
            {
                Ok(count) => count > 0,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            };
        if !exists {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "repo not found"})),
            );
        }
    }

    match sqlx::query(
        "SELECT id, display_name, sync_enabled, last_synced_at::text AS last_synced_at, default_agent_id
         FROM github_repos
         WHERE id = $1",
    )
    .bind(&id)
    .fetch_one(pool)
    .await
    {
        Ok(row) => (
            StatusCode::OK,
            Json(json!({"repo": {
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "display_name": row.try_get::<Option<String>, _>("display_name").ok().flatten(),
                "sync_enabled": row.try_get::<Option<bool>, _>("sync_enabled").ok().flatten().unwrap_or(true),
                "last_synced_at": row.try_get::<Option<String>, _>("last_synced_at").ok().flatten(),
                "default_agent_id": row.try_get::<Option<String>, _>("default_agent_id").ok().flatten(),
            }})),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        ),
    }
}

/// DELETE /api/kanban-repos/:owner/:repo
pub async fn delete_repo(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = format!("{owner}/{repo}");

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    match sqlx::query("DELETE FROM github_repos WHERE id = $1")
        .bind(&id)
        .execute(pool)
        .await
    {
        Ok(result) if result.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "repo not found"})),
        ),
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error}")})),
        ),
    }
}

fn pg_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}
