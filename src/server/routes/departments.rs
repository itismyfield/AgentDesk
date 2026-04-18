use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use libsql_rusqlite::params;
use serde::Deserialize;
use serde_json::json;

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
