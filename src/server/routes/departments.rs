use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use libsql_rusqlite::params;
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

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/departments
pub async fn list_departments(
    State(state): State<AppState>,
    Query(params): Query<ListDepartmentsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
        return match list_departments_pg(pool, params.office_id.as_deref()).await {
            Ok(departments) => (StatusCode::OK, Json(json!({"departments": departments}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
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

    let mut sql = String::from(
        "SELECT d.id, d.name, d.name_ko, d.icon, d.color, d.description, d.office_id, d.sort_order, d.created_at,
                (SELECT COUNT(*) FROM office_agents oa WHERE oa.department_id = d.id) as agent_count
         FROM departments d WHERE 1=1"
    );
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(ref oid) = params.office_id {
        bind_values.push(oid.clone());
        sql.push_str(&format!(" AND d.office_id = ?{}", bind_values.len()));
    }

    sql.push_str(" ORDER BY d.sort_order, d.id");

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn libsql_rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "name_ko": row.get::<_, Option<String>>(2)?,
                "name_ja": serde_json::Value::Null,
                "name_zh": serde_json::Value::Null,
                "icon": row.get::<_, Option<String>>(3)?,
                "color": row.get::<_, Option<String>>(4)?,
                "description": row.get::<_, Option<String>>(5)?,
                "office_id": row.get::<_, Option<String>>(6)?,
                "sort_order": row.get::<_, i64>(7).unwrap_or(0),
                "created_at": row.get::<_, Option<String>>(8)?,
                "agent_count": row.get::<_, i64>(9).unwrap_or(0),
                "prompt": serde_json::Value::Null,
            }))
        })
        .ok();

    let departments: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"departments": departments})))
}

/// POST /api/departments
pub async fn create_department(
    State(state): State<AppState>,
    Json(body): Json<CreateDepartmentBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = uuid::Uuid::new_v4().to_string();

    if let Some(pool) = state.pg_pool.as_ref() {
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    if let Err(e) = conn.execute(
        "INSERT INTO departments (id, name, office_id) VALUES (?1, ?2, ?3)",
        libsql_rusqlite::params![id, body.name, body.office_id],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "department": {
                "id": id,
                "name": body.name,
                "office_id": body.office_id,
            }
        })),
    )
}

/// PATCH /api/departments/:id
pub async fn update_department(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDepartmentBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(ref name) = body.name {
        sets.push(format!("name = ?{}", idx));
        values.push(Box::new(name.clone()));
        idx += 1;
    }
    if let Some(ref office_id) = body.office_id {
        sets.push(format!("office_id = ?{}", idx));
        values.push(Box::new(office_id.clone()));
        idx += 1;
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    let sql = format!(
        "UPDATE departments SET {} WHERE id = ?{}",
        sets.join(", "),
        idx
    );
    values.push(Box::new(id.clone()));

    let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> =
        values.iter().map(|v| v.as_ref()).collect();
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "department not found"})),
            );
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    // Read back
    match conn.query_row(
        "SELECT id, name, office_id FROM departments WHERE id = ?1",
        [&id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "office_id": row.get::<_, Option<String>>(2)?,
            }))
        },
    ) {
        Ok(dept) => (StatusCode::OK, Json(json!({"department": dept}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// DELETE /api/departments/:id
pub async fn delete_department(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.execute("DELETE FROM departments WHERE id = ?1", [&id]) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "department not found"})),
        ),
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// PATCH /api/departments/reorder
pub async fn reorder_departments(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    if let Err(e) = conn.execute_batch("BEGIN TRANSACTION") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("begin tx: {e}")})),
        );
    }

    let mut updated = 0usize;
    for item in &body.order {
        match conn.execute(
            "UPDATE departments SET sort_order = ?1 WHERE id = ?2",
            params![item.sort_order, item.id],
        ) {
            Ok(n) => updated += n,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("update id={}: {e}", item.id)})),
                );
            }
        }
    }

    if let Err(e) = conn.execute_batch("COMMIT") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("commit: {e}")})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({"ok": true, "updated": updated})),
    )
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
