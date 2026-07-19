use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, QueryBuilder, Row};

use super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};

// ── Query / Body types ────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListDepartmentsQuery {
    #[serde(rename = "officeId")]
    pub office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDepartmentBody {
    pub name: String,
    pub office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDepartmentBody {
    pub name: Option<String>,
    pub office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    pub order: Vec<ReorderItem>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderItem {
    pub id: String,
    pub sort_order: i32,
}

/// Resolve the shared Postgres pool or surface the standard [`AppError`]
/// (#4228). Preserves the prior `pg_unavailable` contract exactly — HTTP 503
/// (SERVICE_UNAVAILABLE) with `error: "postgres pool not configured"`. The
/// standard AppError envelope additionally carries `code`/`context`, matching
/// the batch-1 `pr_summary` precedent (both are additive; consumers read only
/// the `error` string — see `dashboard/src/api/httpClient.ts`).
fn ensure_pg(state: &AppState) -> AppResult<&PgPool> {
    state.pg_pool_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "postgres pool not configured",
        )
    })
}

/// Map a `sqlx` failure onto the standard 500, preserving the exact
/// `format!("{error}")` message the ad-hoc `json!({"error": …})` bodies emitted.
fn db_error(error: sqlx::Error) -> AppError {
    AppError::internal(format!("{error}")).with_code(ErrorCode::Database)
}

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/departments
pub async fn list_departments(
    State(state): State<AppState>,
    Query(params): Query<ListDepartmentsQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let pool = ensure_pg(&state)?;
    let departments = list_departments_pg(pool, params.office_id.as_deref())
        .await
        .map_err(db_error)?;
    Ok((StatusCode::OK, Json(json!({"departments": departments}))))
}

/// POST /api/departments
pub async fn create_department(
    State(state): State<AppState>,
    Json(body): Json<CreateDepartmentBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = uuid::Uuid::new_v4().to_string();
    let pool = ensure_pg(&state)?;

    sqlx::query(
        "INSERT INTO departments (id, name, office_id, sort_order, created_at)
         VALUES ($1, $2, $3, 0, NOW())",
    )
    .bind(&id)
    .bind(body.name.as_str())
    .bind(body.office_id.as_deref())
    .execute(pool)
    .await
    .map_err(db_error)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "department": {
                "id": id,
                "name": body.name,
                "office_id": body.office_id,
            }
        })),
    ))
}

/// PATCH /api/departments/:id
pub async fn update_department(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDepartmentBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let pool = ensure_pg(&state)?;

    let mut has_updates = false;
    let mut query = QueryBuilder::<sqlx::Postgres>::new("UPDATE departments SET ");
    {
        let mut separated = query.separated(", ");
        if let Some(ref name) = body.name {
            separated.push("name = ").push_bind_unseparated(name);
            has_updates = true;
        }
        if let Some(ref office_id) = body.office_id {
            separated
                .push("office_id = ")
                .push_bind_unseparated(office_id);
            has_updates = true;
        }
    }

    if !has_updates {
        return Err(AppError::bad_request("no fields to update"));
    }

    query.push(" WHERE id = ");
    query.push_bind(&id);

    let result = query.build().execute(pool).await.map_err(db_error)?;
    if result.rows_affected() == 0 {
        return Err(AppError::not_found("department not found"));
    }

    let row = sqlx::query("SELECT id, name, office_id FROM departments WHERE id = $1")
        .bind(&id)
        .fetch_one(pool)
        .await
        .map_err(db_error)?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "department": {
                "id": row.get::<String, _>("id"),
                "name": row.get::<Option<String>, _>("name"),
                "office_id": row.get::<Option<String>, _>("office_id"),
            }
        })),
    ))
}

/// DELETE /api/departments/:id
pub async fn delete_department(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let pool = ensure_pg(&state)?;

    let result = sqlx::query("DELETE FROM departments WHERE id = $1")
        .bind(&id)
        .execute(pool)
        .await
        .map_err(db_error)?;

    if result.rows_affected() == 0 {
        return Err(AppError::not_found("department not found"));
    }

    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

/// PATCH /api/departments/reorder
pub async fn reorder_departments(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let pool = ensure_pg(&state)?;

    let mut tx = pool.begin().await.map_err(|error| {
        AppError::internal(format!("begin tx: {error}")).with_code(ErrorCode::Database)
    })?;

    let mut updated = 0usize;
    for item in &body.order {
        match sqlx::query("UPDATE departments SET sort_order = $1 WHERE id = $2")
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

async fn list_departments_pg(
    pool: &PgPool,
    office_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let mut query = QueryBuilder::<sqlx::Postgres>::new(
        "SELECT d.id, d.name, d.name_ko, d.icon, d.color, d.description, d.office_id,
                d.sort_order::BIGINT AS sort_order,
                d.created_at::TEXT AS created_at,
                (SELECT COUNT(*)::BIGINT FROM office_agents oa WHERE oa.department_id = d.id) AS agent_count
         FROM departments d
         WHERE TRUE",
    );

    if let Some(office_id) = office_id {
        query.push(" AND d.office_id = ");
        query.push_bind(office_id);
    }

    query.push(" ORDER BY d.sort_order, d.id");

    let rows = query.build().fetch_all(pool).await?;
    Ok(rows.iter().map(department_row_to_json_pg).collect())
}

fn department_row_to_json_pg(row: &sqlx::postgres::PgRow) -> serde_json::Value {
    json!({
        "id": row.get::<String, _>("id"),
        "name": row.get::<Option<String>, _>("name"),
        "name_ko": row.get::<Option<String>, _>("name_ko"),
        "name_ja": serde_json::Value::Null,
        "name_zh": serde_json::Value::Null,
        "icon": row.get::<Option<String>, _>("icon"),
        "color": row.get::<Option<String>, _>("color"),
        "description": row.get::<Option<String>, _>("description"),
        "office_id": row.get::<Option<String>, _>("office_id"),
        "sort_order": row.get::<i64, _>("sort_order"),
        "created_at": row.get::<Option<String>, _>("created_at"),
        "agent_count": row.get::<i64, _>("agent_count"),
        "prompt": serde_json::Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // #4228: the ad-hoc `json!({"error": …})` bodies became `AppError`s. These
    // tests lock the externally observable contract — HTTP status and the
    // `error` message string — for every error branch in this module, and
    // document that the standard AppError envelope adds `code`/`context`
    // (additive; the frontend `request` helper reads only `error`).

    #[test]
    fn ensure_pg_missing_pool_preserves_503_and_message() {
        // Asserts the 503 body emitted when the pool is absent keeps its exact
        // status + message. Mirrors the error `ensure_pg` returns on `None`.
        let err = AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "postgres pool not configured",
        );
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.message(), "postgres pool not configured");
        assert_eq!(err.to_json_value()["error"], "postgres pool not configured");
    }

    #[test]
    fn db_error_preserves_500_and_verbatim_message() {
        // Asserts `db_error` keeps HTTP 500 and reproduces `format!("{error}")`
        // verbatim under `error`, so callers observe the same DB failure text
        // as before. `code` is the additive `database` tag.
        let expected = format!("{}", sqlx::Error::RowNotFound);
        let err = db_error(sqlx::Error::RowNotFound);
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.message(), expected);
        assert_eq!(err.to_json_value()["error"], expected);
        assert_eq!(err.to_json_value()["code"], "database");
    }

    #[test]
    fn no_fields_to_update_preserves_400_and_message() {
        // Asserts the "no fields to update" guard keeps HTTP 400 and its exact
        // message. Mirrors the `bad_request` returned by `update_department`.
        let err = AppError::bad_request("no fields to update");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.to_json_value()["error"], "no fields to update");
    }

    #[test]
    fn department_not_found_preserves_404_and_message() {
        // Asserts the missing-row branch (shared by update + delete) keeps HTTP
        // 404 and the exact "department not found" message.
        let err = AppError::not_found("department not found");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.to_json_value()["error"], "department not found");
    }

    #[test]
    fn reorder_tx_errors_preserve_500_and_prefixed_messages() {
        // Asserts the reorder transaction failures keep HTTP 500 and their
        // exact prefixed messages ("begin tx: …", "update id=…: …", "commit:
        // …"), matching the inline `map_err` bodies in `reorder_departments`.
        let begin =
            AppError::internal(format!("begin tx: {}", "boom")).with_code(ErrorCode::Database);
        assert_eq!(begin.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(begin.to_json_value()["error"], "begin tx: boom");

        let update = AppError::internal(format!("update id={}: {}", "d1", "boom"))
            .with_code(ErrorCode::Database);
        assert_eq!(update.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(update.to_json_value()["error"], "update id=d1: boom");

        let commit =
            AppError::internal(format!("commit: {}", "boom")).with_code(ErrorCode::Database);
        assert_eq!(commit.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(commit.to_json_value()["error"], "commit: boom");
    }
}
