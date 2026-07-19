use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, QueryBuilder, Row};

use super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};

// ── Body types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateOfficeBody {
    pub name: String,
    pub layout: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateOfficeBody {
    pub name: Option<String>,
    pub layout: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddAgentBody {
    pub agent_id: String,
    pub department_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateOfficeAgentBody {
    pub department_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchAddAgentsBody {
    pub agent_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderOfficeItem {
    pub id: String,
    pub sort_order: i32,
}

fn ensure_pg(state: &AppState) -> AppResult<&PgPool> {
    state.pg_pool_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "postgres pool not configured",
        )
    })
}

fn db_error(error: sqlx::Error) -> AppError {
    AppError::internal(format!("{error}")).with_code(ErrorCode::Database)
}

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/offices
pub async fn list_offices(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let pool = ensure_pg(&state)?;
    let offices = list_offices_pg(pool).await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(json!({"offices": offices}))))
}

/// PATCH /api/offices/reorder
pub async fn reorder_offices(
    State(state): State<AppState>,
    Json(body): Json<Vec<ReorderOfficeItem>>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let pool = ensure_pg(&state)?;
    let mut tx = pool.begin().await.map_err(|error| {
        AppError::internal(format!("begin tx: {error}")).with_code(ErrorCode::Database)
    })?;

    let mut updated = 0usize;
    for item in &body {
        match sqlx::query("UPDATE offices SET sort_order = $1 WHERE id = $2")
            .bind(item.sort_order)
            .bind(&item.id)
            .execute(&mut *tx)
            .await
        {
            Ok(result) => updated += result.rows_affected() as usize,
            Err(error) => {
                let _ = tx.rollback().await;
                return Err(
                    AppError::internal(format!("update id={}: {error}", item.id))
                        .with_code(ErrorCode::Database),
                );
            }
        }
    }

    tx.commit().await.map_err(|error| {
        AppError::internal(format!("commit: {error}")).with_code(ErrorCode::Database)
    })?;

    Ok((
        StatusCode::OK,
        Json(json!({"ok": true, "updated": updated})),
    ))
}

/// POST /api/offices
pub async fn create_office(
    State(state): State<AppState>,
    Json(body): Json<CreateOfficeBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let id = uuid::Uuid::new_v4().to_string();
    let pool = ensure_pg(&state)?;

    sqlx::query(
        "INSERT INTO offices (id, name, layout, sort_order, created_at)
         VALUES ($1, $2, $3, 0, NOW())",
    )
    .bind(&id)
    .bind(body.name.as_str())
    .bind(body.layout.as_deref())
    .execute(pool)
    .await
    .map_err(db_error)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "office": {
                "id": id,
                "name": body.name,
                "layout": body.layout,
            }
        })),
    ))
}

/// PATCH /api/offices/:id
pub async fn update_office(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateOfficeBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let pool = ensure_pg(&state)?;
    let mut updated_any = false;
    let mut builder = QueryBuilder::<sqlx::Postgres>::new("UPDATE offices SET ");
    let mut separated = builder.separated(", ");

    if let Some(ref name) = body.name {
        updated_any = true;
        separated.push("name = ").push_bind_unseparated(name);
    }
    if let Some(ref layout) = body.layout {
        updated_any = true;
        separated.push("layout = ").push_bind_unseparated(layout);
    }

    if !updated_any {
        return Err(AppError::bad_request("no fields to update"));
    }

    builder.push(" WHERE id = ");
    builder.push_bind(&id);

    let result = builder.build().execute(pool).await.map_err(db_error)?;
    if result.rows_affected() == 0 {
        return Err(AppError::not_found("office not found"));
    }

    let row = sqlx::query("SELECT id, name, layout FROM offices WHERE id = $1")
        .bind(&id)
        .fetch_one(pool)
        .await
        .map_err(db_error)?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "office": {
                "id": row.get::<String, _>("id"),
                "name": row.get::<Option<String>, _>("name"),
                "layout": row.get::<Option<String>, _>("layout"),
            }
        })),
    ))
}

/// DELETE /api/offices/:id
pub async fn delete_office(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let pool = ensure_pg(&state)?;
    let result = sqlx::query("DELETE FROM offices WHERE id = $1")
        .bind(&id)
        .execute(pool)
        .await
        .map_err(db_error)?;

    if result.rows_affected() == 0 {
        return Err(AppError::not_found("office not found"));
    }

    let _ = sqlx::query("DELETE FROM office_agents WHERE office_id = $1")
        .bind(&id)
        .execute(pool)
        .await;
    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

/// POST /api/offices/:id/agents
pub async fn add_agent(
    State(state): State<AppState>,
    Path(office_id): Path<String>,
    Json(body): Json<AddAgentBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let pool = ensure_pg(&state)?;
    if !office_exists_or_error(
        office_exists_pg(pool, &office_id).await,
        &office_id,
        "add office agent",
    )? {
        return Err(AppError::not_found("office not found"));
    }

    sqlx::query(
        "INSERT INTO office_agents (office_id, agent_id, department_id)
         VALUES ($1, $2, $3)
         ON CONFLICT (office_id, agent_id)
         DO UPDATE SET department_id = EXCLUDED.department_id",
    )
    .bind(&office_id)
    .bind(body.agent_id.as_str())
    .bind(body.department_id.as_deref())
    .execute(pool)
    .await
    .map_err(db_error)?;

    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

/// DELETE /api/offices/:office_id/agents/:agent_id
pub async fn remove_agent(
    State(state): State<AppState>,
    Path((office_id, agent_id)): Path<(String, String)>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let pool = ensure_pg(&state)?;
    let result = sqlx::query("DELETE FROM office_agents WHERE office_id = $1 AND agent_id = $2")
        .bind(&office_id)
        .bind(&agent_id)
        .execute(pool)
        .await
        .map_err(db_error)?;

    if result.rows_affected() == 0 {
        return Err(AppError::not_found("office-agent link not found"));
    }

    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

/// PATCH /api/offices/:office_id/agents/:agent_id
pub async fn update_office_agent(
    State(state): State<AppState>,
    Path((office_id, agent_id)): Path<(String, String)>,
    Json(body): Json<UpdateOfficeAgentBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let pool = ensure_pg(&state)?;
    let result = sqlx::query(
        "UPDATE office_agents SET department_id = $1 WHERE office_id = $2 AND agent_id = $3",
    )
    .bind(body.department_id.as_deref())
    .bind(&office_id)
    .bind(&agent_id)
    .execute(pool)
    .await
    .map_err(db_error)?;

    if result.rows_affected() == 0 {
        return Err(AppError::not_found("office-agent link not found"));
    }

    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

/// POST /api/offices/:id/agents/batch
pub async fn batch_add_agents(
    State(state): State<AppState>,
    Path(office_id): Path<String>,
    Json(body): Json<BatchAddAgentsBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let pool = ensure_pg(&state)?;
    if !office_exists_or_error(
        office_exists_pg(pool, &office_id).await,
        &office_id,
        "batch add office agents",
    )? {
        return Err(AppError::not_found("office not found"));
    }

    let mut tx = pool.begin().await.map_err(db_error)?;
    for agent_id in &body.agent_ids {
        if let Err(error) = sqlx::query(
            "INSERT INTO office_agents (office_id, agent_id, department_id)
             VALUES ($1, $2, NULL)
             ON CONFLICT (office_id, agent_id)
             DO UPDATE SET department_id = NULL",
        )
        .bind(&office_id)
        .bind(agent_id)
        .execute(&mut *tx)
        .await
        {
            let _ = tx.rollback().await;
            return Err(db_error(error));
        }
    }

    tx.commit().await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

async fn list_offices_pg(pool: &PgPool) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT o.id, o.name, o.name_ko, o.icon, o.color, o.description,
                o.sort_order::BIGINT AS sort_order,
                o.created_at::TEXT AS created_at,
                (SELECT COUNT(*)::BIGINT FROM office_agents oa WHERE oa.office_id = o.id) AS agent_count,
                (SELECT COUNT(*)::BIGINT FROM departments d WHERE d.office_id = o.id) AS dept_count
         FROM offices o
         ORDER BY o.sort_order, o.id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(office_row_to_json_pg).collect())
}

fn office_row_to_json_pg(row: &sqlx::postgres::PgRow) -> serde_json::Value {
    json!({
        "id": row.get::<String, _>("id"),
        "name": row.get::<Option<String>, _>("name"),
        "name_ko": row.get::<Option<String>, _>("name_ko"),
        "icon": row.get::<Option<String>, _>("icon"),
        "color": row.get::<Option<String>, _>("color"),
        "description": row.get::<Option<String>, _>("description"),
        "sort_order": row.get::<i64, _>("sort_order"),
        "created_at": row.get::<Option<String>, _>("created_at"),
        "agent_count": row.get::<i64, _>("agent_count"),
        "department_count": row.get::<i64, _>("dept_count"),
    })
}

async fn office_exists_pg(pool: &PgPool, office_id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM offices WHERE id = $1")
        .bind(office_id)
        .fetch_one(pool)
        .await
        .map(|count| count > 0)
}

fn office_exists_or_error(
    result: Result<bool, sqlx::Error>,
    office_id: &str,
    action: &'static str,
) -> AppResult<bool> {
    result.map_err(|error| {
        tracing::warn!(
            office_id = %office_id,
            action,
            "failed to look up office via postgres: {error}"
        );
        db_error(error)
    })
}
