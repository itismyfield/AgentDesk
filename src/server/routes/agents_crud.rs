//! Agent CRUD handlers + system listing endpoints.
//! Extracted from mod.rs for #102.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

// ── Query / Body structs ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct ListAgentsQuery {
    #[serde(rename = "officeId")]
    office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateAgentBody {
    id: String,
    name: String,
    name_ko: Option<String>,
    provider: Option<String>,
    department: Option<String>,
    avatar_emoji: Option<String>,
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
    office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UpdateAgentBody {
    name: Option<String>,
    name_ko: Option<String>,
    provider: Option<String>,
    department: Option<String>,
    department_id: Option<String>,
    avatar_emoji: Option<String>,
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
    alias: Option<String>,
    cli_provider: Option<String>,
    sprite_number: Option<i64>,
    pipeline_config: Option<serde_json::Value>,
}

fn normalize_channel_field(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn merged_channel_values(
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    // New columns (_cc, _cdx) are authoritative; legacy (_id, _alt) are mirrors.
    // Resolve new columns first (fallback from legacy if absent), then mirror back.
    let discord_channel_cc = normalize_channel_field(discord_channel_cc)
        .or_else(|| normalize_channel_field(discord_channel_id));
    let discord_channel_cdx = normalize_channel_field(discord_channel_cdx)
        .or_else(|| normalize_channel_field(discord_channel_alt));
    let discord_channel_id = discord_channel_cc.clone();
    let discord_channel_alt = discord_channel_cdx.clone();
    (
        discord_channel_id,
        discord_channel_alt,
        discord_channel_cc,
        discord_channel_cdx,
    )
}

// ── Handlers ─────────────────────────────────────────────────────

pub(super) async fn list_agents(
    State(state): State<AppState>,
    Query(params): Query<ListAgentsQuery>,
) -> Json<serde_json::Value> {
    let agents = match state.db.lock() {
        Ok(conn) => {
            let (sql, bind_values): (String, Vec<String>) = if let Some(ref oid) = params.office_id
            {
                (
                    "SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
                            a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
                            a.status, a.xp, a.sprite_number, d.name, d.name, NULL, a.created_at,
                            (SELECT COUNT(DISTINCT kc.id) FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
                            (SELECT COALESCE(SUM(s.tokens), 0) FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
                            (SELECT td2.id FROM task_dispatches td2 JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id WHERE td2.to_agent_id = a.id AND kc.status = 'in_progress' LIMIT 1) AS current_task,
                            (SELECT s.thread_channel_id FROM sessions s WHERE s.agent_id = a.id AND s.status = 'working' ORDER BY s.last_heartbeat DESC, s.id DESC LIMIT 1) AS current_thread_channel_id,
                            a.pipeline_config
                     FROM agents a
                     INNER JOIN office_agents oa ON oa.agent_id = a.id
                     LEFT JOIN departments d ON d.id = a.department
                     WHERE oa.office_id = ?1
                     ORDER BY a.id".to_string(),
                    vec![oid.clone()],
                )
            } else {
                (
                    "SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
                            a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
                            a.status, a.xp, a.sprite_number, d.name, d.name, NULL, a.created_at,
                            (SELECT COUNT(DISTINCT kc.id) FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
                            (SELECT COALESCE(SUM(s.tokens), 0) FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
                            (SELECT td2.id FROM task_dispatches td2 JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id WHERE td2.to_agent_id = a.id AND kc.status = 'in_progress' LIMIT 1) AS current_task,
                            (SELECT s.thread_channel_id FROM sessions s WHERE s.agent_id = a.id AND s.status = 'working' ORDER BY s.last_heartbeat DESC, s.id DESC LIMIT 1) AS current_thread_channel_id,
                            a.pipeline_config
                     FROM agents a
                     LEFT JOIN departments d ON d.id = a.department
                     ORDER BY a.id".to_string(),
                    vec![],
                )
            };

            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) => {
                    return Json(json!({ "error": format!("query prepare failed: {e}") }));
                }
            };

            let params_ref: Vec<&dyn rusqlite::types::ToSql> = bind_values
                .iter()
                .map(|v| v as &dyn rusqlite::types::ToSql)
                .collect();

            let rows = stmt
                .query_map(params_ref.as_slice(), |row| {
                    let provider = row.get::<_, Option<String>>(3)?;
                    let discord_channel_alt = row.get::<_, Option<String>>(7)?;
                    let discord_channel_cdx = row.get::<_, Option<String>>(9)?;
                    let xp_val = row.get::<_, f64>(11).unwrap_or(0.0) as i64;
                    Ok(json!({
                        "id": row.get::<_, String>(0)?,
                        "name": row.get::<_, String>(1)?,
                        "name_ko": row.get::<_, Option<String>>(2)?,
                        "provider": provider,
                        "cli_provider": provider,
                        "department": row.get::<_, Option<String>>(4)?,
                        "department_id": row.get::<_, Option<String>>(4)?,
                        "avatar_emoji": row.get::<_, Option<String>>(5)?,
                        "discord_channel_id": row.get::<_, Option<String>>(6)?,
                        "discord_channel_alt": discord_channel_alt,
                        "discord_channel_cc": row.get::<_, Option<String>>(8)?,
                        "discord_channel_cdx": discord_channel_cdx,
                        "discord_channel_id_codex": discord_channel_cdx,
                        "status": row.get::<_, Option<String>>(10)?,
                        "xp": xp_val,
                        "stats_xp": xp_val,
                        "stats_tasks_done": row.get::<_, i64>(17).unwrap_or(0),
                        "stats_tokens": row.get::<_, i64>(18).unwrap_or(0),
                        "sprite_number": row.get::<_, Option<i64>>(12)?,
                        "department_name": row.get::<_, Option<String>>(13)?,
                        "department_name_ko": row.get::<_, Option<String>>(14)?,
                        "department_color": row.get::<_, Option<String>>(15)?,
                        "created_at": row.get::<_, Option<String>>(16)?,
                        "alias": serde_json::Value::Null,
                        "role_id": row.get::<_, Option<String>>(0)?,
                        "personality": serde_json::Value::Null,
                        "current_task_id": row.get::<_, Option<String>>(19)?,
                        "current_thread_channel_id": row.get::<_, Option<String>>(20)?,
                        "pipeline_config": row.get::<_, Option<String>>(21)?
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                    }))
                })
                .ok();

            match rows {
                Some(iter) => iter.filter_map(|r| r.ok()).collect::<Vec<_>>(),
                None => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    };

    Json(json!({ "agents": agents }))
}

pub(super) async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    match state.db.lock() {
        Ok(conn) => {
            let result = conn.query_row(
                "SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
                        a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
                        a.status, a.xp, a.sprite_number, d.name, d.name, NULL, a.created_at,
                        (SELECT COUNT(DISTINCT kc.id) FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
                        (SELECT COALESCE(SUM(s.tokens), 0) FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
                        (SELECT td2.id FROM task_dispatches td2 JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id WHERE td2.to_agent_id = a.id AND kc.status = 'in_progress' LIMIT 1) AS current_task,
                        (SELECT s.thread_channel_id FROM sessions s WHERE s.agent_id = a.id AND s.status = 'working' ORDER BY s.last_heartbeat DESC, s.id DESC LIMIT 1) AS current_thread_channel_id,
                        a.pipeline_config
                 FROM agents a
                 LEFT JOIN departments d ON d.id = a.department
                 WHERE a.id = ?1",
                [&id],
                |row| {
                    let provider = row.get::<_, Option<String>>(3)?;
                    let discord_channel_alt = row.get::<_, Option<String>>(7)?;
                    let discord_channel_cdx = row.get::<_, Option<String>>(9)?;
                    let xp_val = row.get::<_, f64>(11).unwrap_or(0.0) as i64;
                    Ok(json!({
                        "id": row.get::<_, String>(0)?,
                        "name": row.get::<_, String>(1)?,
                        "name_ko": row.get::<_, Option<String>>(2)?,
                        "provider": provider,
                        "cli_provider": provider,
                        "department": row.get::<_, Option<String>>(4)?,
                        "department_id": row.get::<_, Option<String>>(4)?,
                        "avatar_emoji": row.get::<_, Option<String>>(5)?,
                        "discord_channel_id": row.get::<_, Option<String>>(6)?,
                        "discord_channel_alt": discord_channel_alt,
                        "discord_channel_cc": row.get::<_, Option<String>>(8)?,
                        "discord_channel_cdx": discord_channel_cdx,
                        "discord_channel_id_codex": discord_channel_cdx,
                        "status": row.get::<_, Option<String>>(10)?,
                        "xp": xp_val,
                        "stats_xp": xp_val,
                        "stats_tasks_done": row.get::<_, i64>(17).unwrap_or(0),
                        "stats_tokens": row.get::<_, i64>(18).unwrap_or(0),
                        "sprite_number": row.get::<_, Option<i64>>(12)?,
                        "department_name": row.get::<_, Option<String>>(13)?,
                        "department_name_ko": row.get::<_, Option<String>>(14)?,
                        "department_color": row.get::<_, Option<String>>(15)?,
                        "created_at": row.get::<_, Option<String>>(16)?,
                        "alias": serde_json::Value::Null,
                        "role_id": row.get::<_, Option<String>>(0)?,
                        "personality": serde_json::Value::Null,
                        "current_task_id": row.get::<_, Option<String>>(19)?,
                        "current_thread_channel_id": row.get::<_, Option<String>>(20)?,
                        "pipeline_config": row.get::<_, Option<String>>(21)?
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                    }))
                },
            );

            match result {
                Ok(agent) => Json(json!({ "agent": agent })),
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    Json(json!({ "error": "agent not found" }))
                }
                Err(e) => Json(json!({ "error": format!("query failed: {e}") })),
            }
        }
        Err(_) => Json(json!({ "error": "db lock failed" })),
    }
}

pub(super) async fn create_agent(
    State(state): State<AppState>,
    Json(body): Json<CreateAgentBody>,
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

    let (discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx) =
        merged_channel_values(
            body.discord_channel_id.clone(),
            body.discord_channel_alt.clone(),
            body.discord_channel_cc.clone(),
            body.discord_channel_cdx.clone(),
        );

    if let Err(e) = conn.execute(
        "INSERT INTO agents (
            id, name, name_ko, provider, department, avatar_emoji,
            discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            body.id,
            body.name,
            body.name_ko,
            body.provider,
            body.department,
            body.avatar_emoji,
            discord_channel_id,
            discord_channel_alt,
            discord_channel_cc,
            discord_channel_cdx,
        ],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    if let Some(ref office_id) = body.office_id {
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO office_agents (office_id, agent_id) VALUES (?1, ?2)",
            rusqlite::params![office_id, body.id],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    match conn.query_row(
        "SELECT id, name, name_ko, provider, department, avatar_emoji,
                discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx, status, xp
         FROM agents WHERE id = ?1",
        [&body.id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "name_ko": row.get::<_, Option<String>>(2)?,
                "provider": row.get::<_, Option<String>>(3)?,
                "department": row.get::<_, Option<String>>(4)?,
                "avatar_emoji": row.get::<_, Option<String>>(5)?,
                "discord_channel_id": row.get::<_, Option<String>>(6)?,
                "discord_channel_alt": row.get::<_, Option<String>>(7)?,
                "discord_channel_cc": row.get::<_, Option<String>>(8)?,
                "discord_channel_cdx": row.get::<_, Option<String>>(9)?,
                "discord_channel_id_codex": row.get::<_, Option<String>>(9)?,
                "status": row.get::<_, Option<String>>(10)?,
                "xp": row.get::<_, f64>(11).unwrap_or(0.0) as i64,
            }))
        },
    ) {
        Ok(agent) => (StatusCode::CREATED, Json(json!({"agent": agent}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

pub(super) async fn update_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAgentBody>,
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
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;
    let channel_patch_requested = body.discord_channel_id.is_some()
        || body.discord_channel_alt.is_some()
        || body.discord_channel_cc.is_some()
        || body.discord_channel_cdx.is_some();

    if let Some(ref name) = body.name {
        sets.push(format!("name = ?{}", idx));
        values.push(Box::new(name.clone()));
        idx += 1;
    }
    if let Some(ref name_ko) = body.name_ko {
        sets.push(format!("name_ko = ?{}", idx));
        values.push(Box::new(name_ko.clone()));
        idx += 1;
    }
    if let Some(ref provider) = body.provider {
        sets.push(format!("provider = ?{}", idx));
        values.push(Box::new(provider.clone()));
        idx += 1;
    }
    let dept_value = body.department_id.as_ref().or(body.department.as_ref());
    if let Some(department) = dept_value {
        sets.push(format!("department = ?{}", idx));
        values.push(Box::new(department.clone()));
        idx += 1;
    }
    if let Some(ref avatar_emoji) = body.avatar_emoji {
        sets.push(format!("avatar_emoji = ?{}", idx));
        values.push(Box::new(avatar_emoji.clone()));
        idx += 1;
    }
    if channel_patch_requested {
        let existing_channels: (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = match conn.query_row(
            "SELECT discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             FROM agents WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ) {
            Ok(channels) => channels,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                );
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        let (discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx) =
            merged_channel_values(
                body.discord_channel_id.clone().or(existing_channels.0),
                body.discord_channel_alt.clone().or(existing_channels.1),
                body.discord_channel_cc.clone().or(existing_channels.2),
                body.discord_channel_cdx.clone().or(existing_channels.3),
            );
        for (column, value) in [
            ("discord_channel_id", discord_channel_id),
            ("discord_channel_alt", discord_channel_alt),
            ("discord_channel_cc", discord_channel_cc),
            ("discord_channel_cdx", discord_channel_cdx),
        ] {
            sets.push(format!("{column} = ?{idx}"));
            values.push(Box::new(value));
            idx += 1;
        }
    }
    if let Some(ref pipeline_config) = body.pipeline_config {
        if pipeline_config.is_null() {
            sets.push(format!("pipeline_config = NULL"));
        } else {
            let s = pipeline_config.to_string();
            if let Err(e) = crate::pipeline::parse_override(&s) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid pipeline_config: {e}")})),
                );
            }
            sets.push(format!("pipeline_config = ?{}", idx));
            values.push(Box::new(s));
            idx += 1;
        }
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    sets.push(format!("updated_at = datetime('now')"));

    let sql = format!("UPDATE agents SET {} WHERE id = ?{}", sets.join(", "), idx);
    values.push(Box::new(id.clone()));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "agent not found"})),
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

    match conn.query_row(
        "SELECT id, name, name_ko, provider, department, avatar_emoji,
                discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx, status, xp, pipeline_config
         FROM agents WHERE id = ?1",
        [&id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "name_ko": row.get::<_, Option<String>>(2)?,
                "provider": row.get::<_, Option<String>>(3)?,
                "department": row.get::<_, Option<String>>(4)?,
                "avatar_emoji": row.get::<_, Option<String>>(5)?,
                "discord_channel_id": row.get::<_, Option<String>>(6)?,
                "discord_channel_alt": row.get::<_, Option<String>>(7)?,
                "discord_channel_cc": row.get::<_, Option<String>>(8)?,
                "discord_channel_cdx": row.get::<_, Option<String>>(9)?,
                "discord_channel_id_codex": row.get::<_, Option<String>>(9)?,
                "status": row.get::<_, Option<String>>(10)?,
                "xp": row.get::<_, f64>(11).unwrap_or(0.0) as i64,
                "pipeline_config": row.get::<_, Option<String>>(12)?
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
            }))
        },
    ) {
        Ok(agent) => (StatusCode::OK, Json(json!({"agent": agent}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

pub(super) async fn delete_agent(
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

    match conn.execute("DELETE FROM agents WHERE id = ?1", [&id]) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        ),
        Ok(_) => {
            let _ = conn.execute("DELETE FROM office_agents WHERE agent_id = ?1", [&id]);
            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

pub(super) async fn list_sessions(State(state): State<AppState>) -> Json<serde_json::Value> {
    let sessions = match state.db.lock() {
        Ok(conn) => {
            let mut stmt = match conn.prepare(
                "SELECT id, session_key, agent_id, provider, status, active_dispatch_id,
                        model, tokens, cwd, last_heartbeat
                 FROM sessions
                 WHERE status IN ('connected', 'working', 'idle')
                 ORDER BY id",
            ) {
                Ok(s) => s,
                Err(e) => {
                    return Json(json!({ "error": format!("query prepare failed: {e}") }));
                }
            };

            let rows = stmt
                .query_map([], |row| {
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?,
                        "session_key": row.get::<_, Option<String>>(1)?,
                        "agent_id": row.get::<_, Option<String>>(2)?,
                        "provider": row.get::<_, Option<String>>(3)?,
                        "status": row.get::<_, Option<String>>(4)?,
                        "active_dispatch_id": row.get::<_, Option<String>>(5)?,
                        "model": row.get::<_, Option<String>>(6)?,
                        "tokens": row.get::<_, i64>(7)?,
                        "cwd": row.get::<_, Option<String>>(8)?,
                        "last_heartbeat": row.get::<_, Option<String>>(9)?,
                    }))
                })
                .ok();

            match rows {
                Some(iter) => iter.filter_map(|r| r.ok()).collect::<Vec<_>>(),
                None => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    };

    Json(json!({ "sessions": sessions }))
}

pub(super) async fn list_policies(State(state): State<AppState>) -> Json<serde_json::Value> {
    let policies = state.engine.list_policies();
    let items: Vec<serde_json::Value> = policies
        .into_iter()
        .map(|p| {
            json!({
                "name": p.name,
                "file": p.file,
                "priority": p.priority,
                "hooks": p.hooks,
            })
        })
        .collect();
    Json(json!({ "policies": items }))
}
