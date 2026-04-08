use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use super::AppState;

fn skill_description_from_markdown(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with('#')
                && !line.starts_with("name:")
                && !line.starts_with("description:")
                && !line.starts_with("---")
        })
        .map(ToString::to_string)
        .unwrap_or_else(|| "Skill".to_string())
}

fn codex_skill_file(path: &Path) -> Option<PathBuf> {
    if path.is_file() && path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
        return Some(path.to_path_buf());
    }
    let candidate = path.join("SKILL.md");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn sync_skills_from_disk(conn: &rusqlite::Connection) {
    let mut roots = Vec::new();
    if let Some(runtime_root) = crate::config::runtime_root() {
        let _ = crate::runtime_layout::sync_managed_skills(&runtime_root);
        roots.push(crate::runtime_layout::managed_skills_root(&runtime_root));
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".codex").join("skills"));
        roots.push(home.join(".claude").join("commands"));
    }

    let mut seen = HashSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let skill_path = if root.ends_with("commands") {
                if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                    Some(path.clone())
                } else {
                    None
                }
            } else {
                codex_skill_file(&path)
            };
            let Some(skill_path) = skill_path else {
                continue;
            };

            let name_opt = if root.ends_with("commands") {
                skill_path.file_stem().and_then(|stem| stem.to_str())
            } else {
                skill_path
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|stem| stem.to_str())
            };
            let Some(name) = name_opt else {
                continue;
            };

            let name = name.to_string();
            if !seen.insert(name.clone()) {
                continue;
            }

            let description = fs::read_to_string(&skill_path)
                .ok()
                .map(|content| skill_description_from_markdown(&content))
                .unwrap_or_else(|| name.clone());
            let source_path = skill_path.to_string_lossy().to_string();
            let updated_at = fs::metadata(&skill_path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .map(|modified| DateTime::<Utc>::from(modified).to_rfc3339());

            let _ = conn.execute(
                "INSERT INTO skills (id, name, description, source_path, trigger_patterns, updated_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name,
                   description = excluded.description,
                   source_path = excluded.source_path,
                   updated_at = excluded.updated_at",
                rusqlite::params![name, name, description, source_path, updated_at],
            );
        }
    }
}

/// GET /api/skills/catalog
pub async fn catalog(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    sync_skills_from_disk(&conn);

    let mut stmt = match conn.prepare(
        "SELECT s.name,
                COALESCE(s.description, s.name, '') AS description,
                COALESCE(s.description, s.name, '') AS description_ko,
                COALESCE(u.total_calls, 0) AS total_calls,
                CASE
                    WHEN u.last_used_at IS NULL THEN NULL
                    ELSE CAST(strftime('%s', u.last_used_at) AS INTEGER) * 1000
                END AS last_used_at
         FROM skills s
         LEFT JOIN (
            SELECT skill_id, COUNT(*) AS total_calls, MAX(used_at) AS last_used_at
            FROM skill_usage
            GROUP BY skill_id
         ) u ON u.skill_id = s.id
         ORDER BY total_calls DESC, s.name ASC",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query prepare failed: {e}")})),
            );
        }
    };

    let rows = stmt
        .query_map([], |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "description": row.get::<_, String>(1)?,
                "description_ko": row.get::<_, String>(2)?,
                "total_calls": row.get::<_, i64>(3)?,
                "last_used_at": row.get::<_, Option<i64>>(4)?,
            }))
        })
        .ok();

    let catalog = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect::<Vec<_>>(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({ "catalog": catalog })))
}

#[derive(Debug, Deserialize)]
pub struct RankingQuery {
    window: Option<String>,
    limit: Option<i64>,
}

/// GET /api/skills/ranking?window=7d&limit=20
pub async fn ranking(
    State(state): State<AppState>,
    Query(params): Query<RankingQuery>,
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
    sync_skills_from_disk(&conn);

    let window = params.window.as_deref().unwrap_or("7d");
    let limit = params.limit.unwrap_or(20);

    let date_filter = match window {
        "30d" => "AND su.used_at > datetime('now', '-30 days')",
        "90d" => "AND su.used_at > datetime('now', '-90 days')",
        "all" => "",
        _ => "AND su.used_at > datetime('now', '-7 days')", // default 7d
    };

    let overall_sql = format!(
        "SELECT COALESCE(s.name, su.skill_id) AS skill_name,
                COALESCE(s.description, s.name, su.skill_id) AS skill_desc_ko,
                COUNT(su.id) AS calls,
                CASE
                    WHEN MAX(su.used_at) IS NULL THEN NULL
                    ELSE CAST(strftime('%s', MAX(su.used_at)) AS INTEGER) * 1000
                END AS last_used_at
         FROM skill_usage su
         LEFT JOIN skills s ON s.id = su.skill_id
         WHERE 1=1 {date_filter}
         GROUP BY COALESCE(s.name, su.skill_id), COALESCE(s.description, s.name, su.skill_id)
         ORDER BY calls DESC, last_used_at DESC
         LIMIT ?1"
    );

    let overall = {
        let mut stmt = match conn.prepare(&overall_sql) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query prepare failed: {e}")})),
                );
            }
        };

        let rows = stmt
            .query_map([limit], |row| {
                Ok(json!({
                    "skill_name": row.get::<_, String>(0)?,
                    "skill_desc_ko": row.get::<_, String>(1)?,
                    "calls": row.get::<_, i64>(2)?,
                    "last_used_at": row.get::<_, Option<i64>>(3)?,
                }))
            })
            .ok();

        match rows {
            Some(iter) => iter.filter_map(|r| r.ok()).collect::<Vec<_>>(),
            None => Vec::new(),
        }
    };

    // By-agent ranking
    let by_agent_sql = format!(
        "SELECT su.agent_id AS agent_role_id,
                COALESCE(a.name_ko, a.name, su.agent_id) AS agent_name,
                COALESCE(s.name, su.skill_id) AS skill_name,
                COALESCE(s.description, s.name, su.skill_id) AS skill_desc_ko,
                COUNT(su.id) AS calls,
                CASE
                    WHEN MAX(su.used_at) IS NULL THEN NULL
                    ELSE CAST(strftime('%s', MAX(su.used_at)) AS INTEGER) * 1000
                END AS last_used_at
         FROM skill_usage su
         LEFT JOIN skills s ON s.id = su.skill_id
         LEFT JOIN agents a ON a.id = su.agent_id
         WHERE su.agent_id IS NOT NULL {date_filter}
         GROUP BY su.agent_id, agent_name, skill_name, skill_desc_ko
         ORDER BY calls DESC, last_used_at DESC
         LIMIT 100"
    );

    let by_agent = {
        let mut stmt = match conn.prepare(&by_agent_sql) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query prepare failed: {e}")})),
                );
            }
        };

        let rows = stmt
            .query_map([], |row| {
                Ok(json!({
                    "agent_role_id": row.get::<_, String>(0)?,
                    "agent_name": row.get::<_, String>(1)?,
                    "skill_name": row.get::<_, String>(2)?,
                    "skill_desc_ko": row.get::<_, String>(3)?,
                    "calls": row.get::<_, i64>(4)?,
                    "last_used_at": row.get::<_, Option<i64>>(5)?,
                }))
            })
            .ok();

        match rows {
            Some(iter) => iter.filter_map(|r| r.ok()).collect::<Vec<_>>(),
            None => Vec::new(),
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "window": window,
            "overall": overall,
            "byAgent": by_agent,
        })),
    )
}
