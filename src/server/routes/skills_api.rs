use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use super::{
    AppState,
    skill_usage_analytics::{SkillUsageRecord, collect_skill_usage},
};

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

pub(super) fn sync_skills_from_disk(conn: &libsql_rusqlite::Connection) {
    sync_skills_from_disk_with_prune(conn, true);
}

/// Syncs the `skills` catalog rows with what is currently on disk.
///
/// When `prune_missing` is `true` (the default behavior), any row whose `id`
/// is not present on disk after the upsert pass is hard-deleted so the
/// catalog/ranking endpoints stop surfacing skills that have been removed
/// from disk (#816).
///
/// Pruning is skipped when **none** of the configured skill roots could be
/// enumerated, to avoid wiping the entire table just because the home
/// directory or runtime root is temporarily unavailable.
///
/// Hard delete (rather than soft delete) is used because the `skills` table
/// has no `deleted_at` / `is_active` column today, and `skill_usage.skill_id`
/// has no `FOREIGN KEY ... ON DELETE CASCADE`, so historical usage rows
/// remain intact and the catalog endpoint already surfaces usage-only ids
/// via its existing fallback path.
pub(super) fn sync_skills_from_disk_with_prune(
    conn: &libsql_rusqlite::Connection,
    prune_missing: bool,
) {
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
    let mut any_root_enumerated = false;
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        any_root_enumerated = true;

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
                libsql_rusqlite::params![name, name, description, source_path, updated_at],
            );
        }
    }

    if !prune_missing || !any_root_enumerated {
        return;
    }

    prune_missing_skills(conn, &seen);
}

/// Hard-delete `skills` rows whose `id` is not in `seen`.
///
/// `skill_usage` rows are left untouched because there is no FK CASCADE; the
/// catalog endpoint will continue to expose those entries via its
/// usage-only fallback so analytics aren't lost.
fn prune_missing_skills(conn: &libsql_rusqlite::Connection, seen: &HashSet<String>) {
    let existing_ids: Vec<String> = match conn.prepare("SELECT id FROM skills") {
        Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows.filter_map(|row| row.ok()).collect(),
            Err(_) => return,
        },
        Err(_) => return,
    };

    for id in existing_ids {
        if seen.contains(&id) {
            continue;
        }
        let _ = conn.execute(
            "DELETE FROM skills WHERE id = ?1",
            libsql_rusqlite::params![id],
        );
    }
}

#[derive(Default)]
struct UsageAggregate {
    calls: i64,
    last_used_at: Option<i64>,
}

#[derive(Default)]
struct ByAgentAggregate {
    agent_name: String,
    calls: i64,
    last_used_at: Option<i64>,
}

fn ranking_days(window: &str) -> Option<i64> {
    match window {
        "30d" => Some(30),
        "90d" => Some(90),
        "all" => None,
        _ => Some(7),
    }
}

fn load_skill_metadata(
    conn: &libsql_rusqlite::Connection,
) -> libsql_rusqlite::Result<HashMap<String, (String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id,
                COALESCE(name, id) AS skill_name,
                COALESCE(description, name, id) AS skill_desc
         FROM skills",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut metadata = HashMap::new();
    for row in rows {
        let (skill_id, skill_name, skill_desc) = row?;
        metadata.insert(skill_id, (skill_name, skill_desc));
    }

    Ok(metadata)
}

fn apply_usage(aggregate: &mut UsageAggregate, used_at_ms: i64) {
    aggregate.calls += 1;
    aggregate.last_used_at = Some(
        aggregate
            .last_used_at
            .map_or(used_at_ms, |last_used_at| last_used_at.max(used_at_ms)),
    );
}

fn aggregate_overall_usage(records: &[SkillUsageRecord]) -> HashMap<String, UsageAggregate> {
    let mut totals = HashMap::new();
    for record in records {
        apply_usage(
            totals.entry(record.skill_id.clone()).or_default(),
            record.used_at_ms,
        );
    }
    totals
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
    let metadata = match load_skill_metadata(&conn) {
        Ok(data) => data,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("metadata query failed: {e}")})),
            );
        }
    };
    let usage = match collect_skill_usage(&conn, None) {
        Ok(data) => data,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("usage query failed: {e}")})),
            );
        }
    };
    let totals = aggregate_overall_usage(&usage);
    let known_ids: HashSet<String> = metadata.keys().cloned().collect();

    let mut catalog = metadata
        .into_iter()
        .map(|(skill_id, (name, description))| {
            let aggregate = totals.get(&skill_id);
            json!({
                "name": name,
                "description": description,
                "description_ko": description,
                "total_calls": aggregate.map_or(0, |item| item.calls),
                "last_used_at": aggregate.and_then(|item| item.last_used_at),
            })
        })
        .collect::<Vec<_>>();

    for (skill_id, aggregate) in totals {
        if known_ids.contains(&skill_id) {
            continue;
        }
        catalog.push(json!({
            "name": skill_id,
            "description": skill_id,
            "description_ko": skill_id,
            "total_calls": aggregate.calls,
            "last_used_at": aggregate.last_used_at,
        }));
    }

    catalog.sort_by(|left, right| {
        let left_calls = left["total_calls"].as_i64().unwrap_or(0);
        let right_calls = right["total_calls"].as_i64().unwrap_or(0);
        right_calls
            .cmp(&left_calls)
            .then_with(|| {
                right["last_used_at"]
                    .as_i64()
                    .unwrap_or_default()
                    .cmp(&left["last_used_at"].as_i64().unwrap_or_default())
            })
            .then_with(|| {
                left["name"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(right["name"].as_str().unwrap_or_default())
            })
    });

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
    let metadata = match load_skill_metadata(&conn) {
        Ok(data) => data,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("metadata query failed: {e}")})),
            );
        }
    };
    let usage = match collect_skill_usage(&conn, ranking_days(window)) {
        Ok(data) => data,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("usage query failed: {e}")})),
            );
        }
    };

    let mut overall = aggregate_overall_usage(&usage)
        .into_iter()
        .map(|(skill_id, aggregate)| {
            let (skill_name, skill_desc_ko) = metadata
                .get(&skill_id)
                .cloned()
                .unwrap_or_else(|| (skill_id.clone(), skill_id.clone()));
            json!({
                "skill_name": skill_name,
                "skill_desc_ko": skill_desc_ko,
                "calls": aggregate.calls,
                "last_used_at": aggregate.last_used_at,
            })
        })
        .collect::<Vec<_>>();
    overall.sort_by(|left, right| {
        let left_calls = left["calls"].as_i64().unwrap_or(0);
        let right_calls = right["calls"].as_i64().unwrap_or(0);
        right_calls
            .cmp(&left_calls)
            .then_with(|| {
                right["last_used_at"]
                    .as_i64()
                    .unwrap_or_default()
                    .cmp(&left["last_used_at"].as_i64().unwrap_or_default())
            })
            .then_with(|| {
                left["skill_name"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(right["skill_name"].as_str().unwrap_or_default())
            })
    });
    overall.truncate(limit.max(0) as usize);

    let mut by_agent_totals = HashMap::<(String, String), ByAgentAggregate>::new();
    for record in &usage {
        let Some(agent_role_id) = record.agent_id.clone() else {
            continue;
        };
        let agent_name = record
            .agent_name
            .clone()
            .unwrap_or_else(|| agent_role_id.clone());
        let aggregate = by_agent_totals
            .entry((agent_role_id, record.skill_id.clone()))
            .or_insert_with(|| ByAgentAggregate {
                agent_name: agent_name.clone(),
                ..ByAgentAggregate::default()
            });
        if aggregate.agent_name.is_empty() {
            aggregate.agent_name = agent_name;
        }
        aggregate.calls += 1;
        aggregate.last_used_at = Some(
            aggregate
                .last_used_at
                .map_or(record.used_at_ms, |last_used_at| {
                    last_used_at.max(record.used_at_ms)
                }),
        );
    }

    let mut by_agent = by_agent_totals
        .into_iter()
        .map(|((agent_role_id, skill_id), aggregate)| {
            let (skill_name, skill_desc_ko) = metadata
                .get(&skill_id)
                .cloned()
                .unwrap_or_else(|| (skill_id.clone(), skill_id.clone()));
            json!({
                "agent_role_id": agent_role_id,
                "agent_name": aggregate.agent_name,
                "skill_name": skill_name,
                "skill_desc_ko": skill_desc_ko,
                "calls": aggregate.calls,
                "last_used_at": aggregate.last_used_at,
            })
        })
        .collect::<Vec<_>>();
    by_agent.sort_by(|left, right| {
        let left_calls = left["calls"].as_i64().unwrap_or(0);
        let right_calls = right["calls"].as_i64().unwrap_or(0);
        right_calls
            .cmp(&left_calls)
            .then_with(|| {
                right["last_used_at"]
                    .as_i64()
                    .unwrap_or_default()
                    .cmp(&left["last_used_at"].as_i64().unwrap_or_default())
            })
            .then_with(|| {
                left["agent_name"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(right["agent_name"].as_str().unwrap_or_default())
            })
            .then_with(|| {
                left["skill_name"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(right["skill_name"].as_str().unwrap_or_default())
            })
    });
    by_agent.truncate(100);

    (
        StatusCode::OK,
        Json(json!({
            "window": window,
            "overall": overall,
            "byAgent": by_agent,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_skills_conn() -> libsql_rusqlite::Connection {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY,
                name TEXT,
                description TEXT,
                source_path TEXT,
                trigger_patterns TEXT,
                updated_at TEXT
            );
            CREATE TABLE skill_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                skill_id TEXT NOT NULL,
                agent_id TEXT,
                session_key TEXT,
                used_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .unwrap();
        conn
    }

    fn insert_skill(conn: &libsql_rusqlite::Connection, id: &str, description: &str) {
        conn.execute(
            "INSERT INTO skills (id, name, description, source_path) VALUES (?1, ?1, ?2, ?3)",
            libsql_rusqlite::params![id, description, format!("/tmp/skills/{id}/SKILL.md")],
        )
        .unwrap();
    }

    fn skill_ids(conn: &libsql_rusqlite::Connection) -> Vec<String> {
        let mut stmt = conn.prepare("SELECT id FROM skills ORDER BY id").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    fn prune_removes_rows_not_present_on_disk() {
        let conn = setup_skills_conn();
        insert_skill(&conn, "alive-skill", "still on disk");
        insert_skill(&conn, "deleted-skill", "removed from disk");

        let mut seen = HashSet::new();
        seen.insert("alive-skill".to_string());

        prune_missing_skills(&conn, &seen);

        assert_eq!(skill_ids(&conn), vec!["alive-skill".to_string()]);
    }

    #[test]
    fn prune_keeps_skills_still_on_disk_unchanged() {
        let conn = setup_skills_conn();
        insert_skill(&conn, "skill-a", "a");
        insert_skill(&conn, "skill-b", "b");
        insert_skill(&conn, "skill-c", "c");

        let mut seen = HashSet::new();
        seen.insert("skill-a".to_string());
        seen.insert("skill-b".to_string());
        seen.insert("skill-c".to_string());

        prune_missing_skills(&conn, &seen);

        assert_eq!(
            skill_ids(&conn),
            vec![
                "skill-a".to_string(),
                "skill-b".to_string(),
                "skill-c".to_string()
            ]
        );

        let description: String = conn
            .query_row(
                "SELECT description FROM skills WHERE id = 'skill-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(description, "b");
    }

    #[test]
    fn prune_preserves_skill_usage_history_for_deleted_skills() {
        // Hard-delete should NOT touch skill_usage (no FK CASCADE), so historical
        // analytics aren't broken when a skill is removed from disk.
        let conn = setup_skills_conn();
        insert_skill(&conn, "deleted-skill", "removed from disk");
        conn.execute(
            "INSERT INTO skill_usage (skill_id, agent_id, session_key) VALUES (?1, ?2, ?3)",
            libsql_rusqlite::params!["deleted-skill", "agent-x", "sess-1"],
        )
        .unwrap();

        prune_missing_skills(&conn, &HashSet::new());

        assert!(skill_ids(&conn).is_empty());
        let usage_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM skill_usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(usage_count, 1);
    }

    #[test]
    fn load_skill_metadata_excludes_pruned_rows() {
        // Regression for #816: after sync, /api/skills/catalog and
        // /api/skills/ranking pull names from `skills` via load_skill_metadata,
        // so a pruned row must not appear in the metadata map.
        let conn = setup_skills_conn();
        insert_skill(&conn, "alive", "alive desc");
        insert_skill(&conn, "stale", "stale desc");

        let mut seen = HashSet::new();
        seen.insert("alive".to_string());
        prune_missing_skills(&conn, &seen);

        let metadata = load_skill_metadata(&conn).unwrap();
        assert!(metadata.contains_key("alive"));
        assert!(
            !metadata.contains_key("stale"),
            "pruned skill should not surface in catalog/ranking metadata"
        );
    }

    #[test]
    fn prune_with_empty_seen_when_disk_unavailable_is_handled_by_caller() {
        // Sanity check: prune_missing_skills itself trusts the caller; the
        // safety check (skip pruning if no root could be enumerated) lives in
        // sync_skills_from_disk_with_prune. Verify the helper behaves
        // predictably when given an empty set.
        let conn = setup_skills_conn();
        insert_skill(&conn, "only-skill", "only");

        prune_missing_skills(&conn, &HashSet::new());

        assert!(skill_ids(&conn).is_empty());
    }
}
