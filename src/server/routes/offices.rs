use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use libsql_rusqlite::params;
use serde::Deserialize;
use serde_json::json;

use super::AppState;

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

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/offices
pub async fn list_offices(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
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
        "SELECT o.id, o.name, o.name_ko, o.icon, o.color, o.description, o.sort_order, o.created_at,
                (SELECT COUNT(*) FROM office_agents oa WHERE oa.office_id = o.id) as agent_count,
                (SELECT COUNT(*) FROM departments d WHERE d.office_id = o.id) as dept_count
         FROM offices o ORDER BY o.sort_order, o.id"
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            )
        }
    };

    let rows = stmt
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "name_ko": row.get::<_, Option<String>>(2)?,
                "icon": row.get::<_, Option<String>>(3)?,
                "color": row.get::<_, Option<String>>(4)?,
                "description": row.get::<_, Option<String>>(5)?,
                "sort_order": row.get::<_, i64>(6).unwrap_or(0),
                "created_at": row.get::<_, Option<String>>(7)?,
                "agent_count": row.get::<_, i64>(8).unwrap_or(0),
                "department_count": row.get::<_, i64>(9).unwrap_or(0),
            }))
        })
        .ok();

    let offices: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"offices": offices})))
}

/// PATCH /api/offices/reorder
pub async fn reorder_offices(
    State(state): State<AppState>,
    Json(body): Json<Vec<ReorderOfficeItem>>,
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
    for item in &body {
        match conn.execute(
            "UPDATE offices SET sort_order = ?1 WHERE id = ?2",
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

/// POST /api/offices
pub async fn create_office(
    State(state): State<AppState>,
    Json(body): Json<CreateOfficeBody>,
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
        "INSERT INTO offices (id, name, layout) VALUES (?1, ?2, ?3)",
        libsql_rusqlite::params![id, body.name, body.layout],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "office": {
                "id": id,
                "name": body.name,
                "layout": body.layout,
            }
        })),
    )
}

/// PATCH /api/offices/:id
pub async fn update_office(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateOfficeBody>,
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
    if let Some(ref layout) = body.layout {
        sets.push(format!("layout = ?{}", idx));
        values.push(Box::new(layout.clone()));
        idx += 1;
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    let sql = format!("UPDATE offices SET {} WHERE id = ?{}", sets.join(", "), idx);
    values.push(Box::new(id.clone()));

    let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> =
        values.iter().map(|v| v.as_ref()).collect();
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "office not found"})),
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
        "SELECT id, name, layout FROM offices WHERE id = ?1",
        [&id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "layout": row.get::<_, Option<String>>(2)?,
            }))
        },
    ) {
        Ok(office) => (StatusCode::OK, Json(json!({"office": office}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// DELETE /api/offices/:id
pub async fn delete_office(
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

    match conn.execute("DELETE FROM offices WHERE id = ?1", [&id]) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "office not found"})),
        ),
        Ok(_) => {
            // Clean up related office_agents rows
            let _ = conn.execute("DELETE FROM office_agents WHERE office_id = ?1", [&id]);
            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/offices/:id/agents
pub async fn add_agent(
    State(state): State<AppState>,
    Path(office_id): Path<String>,
    Json(body): Json<AddAgentBody>,
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

    // Check office exists
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM offices WHERE id = ?1",
            [&office_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "office not found"})),
        );
    }

    if let Err(e) = conn.execute(
        "INSERT OR REPLACE INTO office_agents (office_id, agent_id, department_id) VALUES (?1, ?2, ?3)",
        libsql_rusqlite::params![office_id, body.agent_id, body.department_id],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    (StatusCode::OK, Json(json!({"ok": true})))
}

/// DELETE /api/offices/:office_id/agents/:agent_id
pub async fn remove_agent(
    State(state): State<AppState>,
    Path((office_id, agent_id)): Path<(String, String)>,
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
        "DELETE FROM office_agents WHERE office_id = ?1 AND agent_id = ?2",
        libsql_rusqlite::params![office_id, agent_id],
    ) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "office-agent link not found"})),
        ),
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// PATCH /api/offices/:office_id/agents/:agent_id
pub async fn update_office_agent(
    State(state): State<AppState>,
    Path((office_id, agent_id)): Path<(String, String)>,
    Json(body): Json<UpdateOfficeAgentBody>,
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
        "UPDATE office_agents SET department_id = ?1 WHERE office_id = ?2 AND agent_id = ?3",
        libsql_rusqlite::params![body.department_id, office_id, agent_id],
    ) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "office-agent link not found"})),
        ),
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/offices/:id/agents/batch
pub async fn batch_add_agents(
    State(state): State<AppState>,
    Path(office_id): Path<String>,
    Json(body): Json<BatchAddAgentsBody>,
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

    // Check office exists
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM offices WHERE id = ?1",
            [&office_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "office not found"})),
        );
    }

    for agent_id in &body.agent_ids {
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO office_agents (office_id, agent_id) VALUES (?1, ?2)",
            libsql_rusqlite::params![office_id, agent_id],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    (StatusCode::OK, Json(json!({"ok": true})))
}
