use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, QueryBuilder, Row};

use super::AppState;

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

fn pg_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres pool not configured"})),
    )
}

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/departments
pub async fn list_departments(
    State(state): State<AppState>,
    Query(params): Query<ListDepartmentsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match list_departments_pg(pool, params.office_id.as_deref()).await {
            Ok(departments) => (StatusCode::OK, Json(json!({"departments": departments}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_unavailable()
}

/// POST /api/departments
pub async fn create_department(
    State(state): State<AppState>,
    Json(body): Json<CreateDepartmentBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = uuid::Uuid::new_v4().to_string();

    if let Some(pool) = state.pg_pool_ref() {
        if let Err(error) = sqlx::query(
            "INSERT INTO departments (id, name, office_id, sort_order, created_at)
             VALUES ($1, $2, $3, 0, NOW())",
        )
        .bind(&id)
        .bind(body.name.as_str())
        .bind(body.office_id.as_deref())
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }

        return (
            StatusCode::CREATED,
            Json(json!({
                "department": {
                    "id": id,
                    "name": body.name,
                    "office_id": body.office_id,
                }
            })),
        );
    }

    pg_unavailable()
}

/// PATCH /api/departments/:id
pub async fn update_department(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDepartmentBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
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
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "no fields to update"})),
            );
        }

        query.push(" WHERE id = ");
        query.push_bind(&id);

        match query.build().execute(pool).await {
            Ok(result) if result.rows_affected() == 0 => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "department not found"})),
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

        return match sqlx::query("SELECT id, name, office_id FROM departments WHERE id = $1")
            .bind(&id)
            .fetch_one(pool)
            .await
        {
            Ok(row) => (
                StatusCode::OK,
                Json(json!({
                    "department": {
                        "id": row.get::<String, _>("id"),
                        "name": row.get::<Option<String>, _>("name"),
                        "office_id": row.get::<Option<String>, _>("office_id"),
                    }
                })),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_unavailable()
}

/// DELETE /api/departments/:id
pub async fn delete_department(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query("DELETE FROM departments WHERE id = $1")
            .bind(&id)
            .execute(pool)
            .await
        {
            Ok(result) if result.rows_affected() == 0 => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "department not found"})),
            ),
            Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_unavailable()
}

/// PATCH /api/departments/reorder
pub async fn reorder_departments(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("begin tx: {error}")})),
                );
            }
        };

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
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("update id={}: {error}", item.id)})),
                    );
                }
            }
        }

        if let Err(error) = tx.commit().await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("commit: {error}")})),
            );
        }

        return (
            StatusCode::OK,
            Json(json!({"ok": true, "updated": updated})),
        );
    }

    pg_unavailable()
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
