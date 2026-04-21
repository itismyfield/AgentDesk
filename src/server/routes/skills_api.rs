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

#[derive(Debug, Clone)]
struct DiscoveredSkill {
    id: String,
    description: String,
    source_path: String,
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillRootKind {
    Directory,
    MarkdownFile,
}

fn push_skill_root(
    roots: &mut Vec<(PathBuf, SkillRootKind)>,
    seen: &mut HashSet<PathBuf>,
    path: PathBuf,
    kind: SkillRootKind,
) {
    if seen.insert(path.clone()) {
        roots.push((path, kind));
    }
}

#[derive(Debug, Default)]
struct DiscoveryResult {
    skills: Vec<DiscoveredSkill>,
    any_root_errored: bool,
}

fn discover_skills_from_disk() -> DiscoveryResult {
    let mut roots = Vec::new();
    let mut seen_roots = HashSet::new();
    if let Some(runtime_root) = crate::config::runtime_root() {
        let _ = crate::runtime_layout::sync_managed_skills(&runtime_root);
        push_skill_root(
            &mut roots,
            &mut seen_roots,
            crate::runtime_layout::managed_skills_root(&runtime_root),
            SkillRootKind::Directory,
        );
    }
    if let Some(home) = dirs::home_dir() {
        push_skill_root(
            &mut roots,
            &mut seen_roots,
            home.join("ObsidianVault")
                .join("RemoteVault")
                .join("99_Skills"),
            SkillRootKind::Directory,
        );
        push_skill_root(
            &mut roots,
            &mut seen_roots,
            home.join(".adk").join("release").join("skills"),
            SkillRootKind::Directory,
        );
        push_skill_root(
            &mut roots,
            &mut seen_roots,
            home.join(".codex").join("skills"),
            SkillRootKind::Directory,
        );
        push_skill_root(
            &mut roots,
            &mut seen_roots,
            home.join(".claude").join("commands"),
            SkillRootKind::MarkdownFile,
        );
    }

    let mut discovered = Vec::new();
    let mut any_root_errored = false;
    let mut seen_ids = HashSet::new();
    for (root, kind) in roots {
        if !root.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!(
                    root = %root.display(),
                    error = %err,
                    "sync_skills_from_disk: failed to enumerate skill root; skipping prune"
                );
                any_root_errored = true;
                continue;
            }
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let skill_path = match kind {
                SkillRootKind::MarkdownFile => {
                    if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                        Some(path.clone())
                    } else {
                        None
                    }
                }
                SkillRootKind::Directory => codex_skill_file(&path),
            };
            let Some(skill_path) = skill_path else {
                continue;
            };

            let id_opt = match kind {
                SkillRootKind::MarkdownFile => {
                    skill_path.file_stem().and_then(|stem| stem.to_str())
                }
                SkillRootKind::Directory => skill_path
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|stem| stem.to_str()),
            };
            let Some(id) = id_opt else {
                continue;
            };

            let id = id.to_string();
            if !seen_ids.insert(id.clone()) {
                continue;
            }

            let description = fs::read_to_string(&skill_path)
                .ok()
                .map(|content| skill_description_from_markdown(&content))
                .unwrap_or_else(|| id.clone());
            let source_path = skill_path.to_string_lossy().to_string();
            let updated_at = fs::metadata(&skill_path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .map(|modified| DateTime::<Utc>::from(modified).to_rfc3339());

            discovered.push(DiscoveredSkill {
                id,
                description,
                source_path,
                updated_at,
            });
        }
    }

    DiscoveryResult {
        skills: discovered,
        any_root_errored,
    }
}

pub(super) fn sync_skills_from_disk(conn: &libsql_rusqlite::Connection) -> HashSet<String> {
    sync_skills_from_disk_with_prune(conn, true)
}

pub(super) fn sync_skills_from_disk_with_prune(
    conn: &libsql_rusqlite::Connection,
    prune_missing: bool,
) -> HashSet<String> {
    let discovery = discover_skills_from_disk();
    let mut disk_skill_ids = HashSet::new();

    for skill in discovery.skills {
        disk_skill_ids.insert(skill.id.clone());
        let _ = conn.execute(
            "INSERT INTO skills (id, name, description, source_path, trigger_patterns, updated_at, deleted_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, NULL)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name,
                   description = excluded.description,
                   source_path = excluded.source_path,
                   updated_at = excluded.updated_at,
                   deleted_at = NULL",
            libsql_rusqlite::params![
                skill.id,
                skill.id,
                skill.description,
                skill.source_path,
                skill.updated_at
            ],
        );
    }

    if prune_missing {
        if discovery.any_root_errored {
            tracing::warn!(
                "sync_skills_from_disk: pruning skipped due to partial disk failure \
                 (at least one skill root failed to enumerate)"
            );
        } else {
            prune_missing_skills(conn, &disk_skill_ids);
        }
    }

    disk_skill_ids
}

fn prune_missing_skills(conn: &libsql_rusqlite::Connection, seen: &HashSet<String>) {
    let existing_ids: Vec<String> =
        match conn.prepare("SELECT id FROM skills WHERE deleted_at IS NULL") {
            Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
                Ok(rows) => rows.filter_map(|row| row.ok()).collect(),
                Err(_) => return,
            },
            Err(_) => return,
        };

    let now_secs = Utc::now().timestamp();
    for id in existing_ids {
        if seen.contains(&id) {
            continue;
        }
        let _ = conn.execute(
            "UPDATE skills SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            libsql_rusqlite::params![id, now_secs],
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

#[derive(Debug, Clone)]
struct SkillMetadata {
    name: String,
    description: String,
}

fn load_skill_metadata(
    conn: &libsql_rusqlite::Connection,
) -> libsql_rusqlite::Result<HashMap<String, SkillMetadata>> {
    let mut stmt = conn.prepare(
        "SELECT id,
                COALESCE(name, id) AS skill_name,
                COALESCE(description, name, id) AS skill_desc
         FROM skills
         WHERE deleted_at IS NULL",
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
        metadata.insert(
            skill_id,
            SkillMetadata {
                name: skill_name,
                description: skill_desc,
            },
        );
    }

    Ok(metadata)
}

fn load_stale_skill_ids(
    conn: &libsql_rusqlite::Connection,
    disk_skill_ids: &HashSet<String>,
) -> libsql_rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM skills ORDER BY id ASC")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let mut stale_skill_ids = Vec::new();
    for row in rows {
        let skill_id = row?;
        if !disk_skill_ids.contains(&skill_id) {
            stale_skill_ids.push(skill_id);
        }
    }

    Ok(stale_skill_ids)
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
#[derive(Debug, Default, Deserialize)]
pub struct SkillCatalogQuery {
    include_stale: Option<bool>,
}

pub async fn catalog(
    State(state): State<AppState>,
    Query(params): Query<SkillCatalogQuery>,
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
    let disk_skill_ids = sync_skills_from_disk(&conn);
    let include_stale = params.include_stale.unwrap_or(false);
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
        .map(|(skill_id, metadata)| {
            let aggregate = totals.get(&skill_id);
            let disk_present = disk_skill_ids.contains(&skill_id);
            json!({
                "name": metadata.name,
                "description": metadata.description,
                "description_ko": metadata.description,
                "total_calls": aggregate.map_or(0, |item| item.calls),
                "last_used_at": aggregate.and_then(|item| item.last_used_at),
                "disk_present": disk_present,
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
            "disk_present": false,
        }));
    }

    if !include_stale {
        catalog.retain(|entry| entry["disk_present"].as_bool().unwrap_or(false));
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

    (
        StatusCode::OK,
        Json(json!({
            "catalog": catalog,
            "include_stale": include_stale,
        })),
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct RankingQuery {
    window: Option<String>,
    limit: Option<i64>,
    include_stale: Option<bool>,
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
    let disk_skill_ids = sync_skills_from_disk(&conn);

    let window = params.window.as_deref().unwrap_or("7d");
    let limit = params.limit.unwrap_or(20);
    let include_stale = params.include_stale.unwrap_or(false);
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
            let metadata = metadata
                .get(&skill_id)
                .cloned()
                .unwrap_or_else(|| SkillMetadata {
                    name: skill_id.clone(),
                    description: skill_id.clone(),
                });
            json!({
                "skill_name": metadata.name,
                "skill_desc_ko": metadata.description,
                "calls": aggregate.calls,
                "last_used_at": aggregate.last_used_at,
                "disk_present": disk_skill_ids.contains(&skill_id),
            })
        })
        .collect::<Vec<_>>();
    if !include_stale {
        overall.retain(|entry| entry["disk_present"].as_bool().unwrap_or(false));
    }
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
            let metadata = metadata
                .get(&skill_id)
                .cloned()
                .unwrap_or_else(|| SkillMetadata {
                    name: skill_id.clone(),
                    description: skill_id.clone(),
                });
            json!({
                "agent_role_id": agent_role_id,
                "agent_name": aggregate.agent_name,
                "skill_name": metadata.name,
                "skill_desc_ko": metadata.description,
                "calls": aggregate.calls,
                "last_used_at": aggregate.last_used_at,
                "disk_present": disk_skill_ids.contains(&skill_id),
            })
        })
        .collect::<Vec<_>>();
    if !include_stale {
        by_agent.retain(|entry| entry["disk_present"].as_bool().unwrap_or(false));
    }
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
            "include_stale": include_stale,
            "overall": overall,
            "byAgent": by_agent,
        })),
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct PruneSkillsQuery {
    dry_run: Option<bool>,
}

/// POST /api/skills/prune?dry_run=true
pub async fn prune(
    State(state): State<AppState>,
    Query(params): Query<PruneSkillsQuery>,
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

    let dry_run = params.dry_run.unwrap_or(false);
    let disk_skill_ids = sync_skills_from_disk_with_prune(&conn, !dry_run);
    let stale_skill_ids = match load_stale_skill_ids(&conn, &disk_skill_ids) {
        Ok(ids) => ids,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("stale skill query failed: {e}")})),
            );
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "dry_run": dry_run,
            "stale_skill_ids": stale_skill_ids,
            "stale_count": stale_skill_ids.len(),
            "soft_deleted_from_skills": if dry_run { 0 } else { stale_skill_ids.len() },
            "skill_usage_policy": "preserved",
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
                updated_at TEXT,
                deleted_at INTEGER
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

    fn deleted_at(conn: &libsql_rusqlite::Connection, id: &str) -> Option<i64> {
        conn.query_row(
            "SELECT deleted_at FROM skills WHERE id = ?1",
            libsql_rusqlite::params![id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .unwrap()
    }

    #[test]
    fn prune_soft_deletes_rows_not_present_on_disk() {
        let conn = setup_skills_conn();
        insert_skill(&conn, "alive-skill", "still on disk");
        insert_skill(&conn, "deleted-skill", "removed from disk");

        let mut seen = HashSet::new();
        seen.insert("alive-skill".to_string());

        prune_missing_skills(&conn, &seen);

        assert_eq!(deleted_at(&conn, "alive-skill"), None);
        assert!(deleted_at(&conn, "deleted-skill").is_some());
    }

    #[test]
    fn load_skill_metadata_excludes_soft_deleted_rows() {
        let conn = setup_skills_conn();
        insert_skill(&conn, "alive", "alive desc");
        insert_skill(&conn, "stale", "stale desc");

        let mut seen = HashSet::new();
        seen.insert("alive".to_string());
        prune_missing_skills(&conn, &seen);

        let metadata = load_skill_metadata(&conn).unwrap();
        assert!(metadata.contains_key("alive"));
        assert!(!metadata.contains_key("stale"));
    }

    #[test]
    fn sync_upsert_clears_deleted_at_when_skill_returns() {
        let conn = setup_skills_conn();
        insert_skill(&conn, "resurrected", "old desc");
        prune_missing_skills(&conn, &HashSet::new());
        assert!(deleted_at(&conn, "resurrected").is_some());

        conn.execute(
            "INSERT INTO skills (id, name, description, source_path, trigger_patterns, updated_at, deleted_at)
             VALUES (?1, ?1, ?2, ?3, NULL, NULL, NULL)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name,
               description = excluded.description,
               source_path = excluded.source_path,
               updated_at = excluded.updated_at,
               deleted_at = NULL",
            libsql_rusqlite::params![
                "resurrected",
                "new desc",
                "/tmp/skills/resurrected/SKILL.md"
            ],
        )
        .unwrap();

        assert_eq!(deleted_at(&conn, "resurrected"), None);
    }
}
