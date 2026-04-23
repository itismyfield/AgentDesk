use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};

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

pub(super) async fn sync_skills_from_disk_pg(
    pool: &sqlx::PgPool,
) -> Result<HashSet<String>, String> {
    sync_skills_from_disk_with_prune_pg(pool, true).await
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

async fn sync_skills_from_disk_with_prune_pg(
    pool: &sqlx::PgPool,
    prune_missing: bool,
) -> Result<HashSet<String>, String> {
    let discovery = discover_skills_from_disk();
    let mut disk_skill_ids = HashSet::new();

    for skill in discovery.skills {
        disk_skill_ids.insert(skill.id.clone());
        sqlx::query(
            "INSERT INTO skills (id, name, description, source_path, trigger_patterns, updated_at, deleted_at)
             VALUES ($1, $2, $3, $4, NULL, $5::TIMESTAMPTZ, NULL)
             ON CONFLICT(id) DO UPDATE SET
               name = EXCLUDED.name,
               description = EXCLUDED.description,
               source_path = EXCLUDED.source_path,
               updated_at = EXCLUDED.updated_at,
               deleted_at = NULL",
        )
        .bind(&skill.id)
        .bind(&skill.id)
        .bind(&skill.description)
        .bind(&skill.source_path)
        .bind(&skill.updated_at)
        .execute(pool)
        .await
        .map_err(|error| format!("upsert postgres skill {}: {error}", skill.id))?;
    }

    if prune_missing {
        if discovery.any_root_errored {
            tracing::warn!(
                "sync_skills_from_disk: pruning skipped due to partial disk failure \
                 (at least one skill root failed to enumerate)"
            );
        } else {
            prune_missing_skills_pg(pool, &disk_skill_ids).await?;
        }
    }

    Ok(disk_skill_ids)
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

async fn prune_missing_skills_pg(
    pool: &sqlx::PgPool,
    seen: &HashSet<String>,
) -> Result<(), String> {
    let existing_rows = sqlx::query("SELECT id FROM skills WHERE deleted_at IS NULL")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres skills for prune: {error}"))?;
    let now_secs = Utc::now().timestamp();

    for row in existing_rows {
        let id = row
            .try_get::<String, _>("id")
            .map_err(|error| format!("decode postgres skill id for prune: {error}"))?;
        if seen.contains(&id) {
            continue;
        }
        sqlx::query("UPDATE skills SET deleted_at = $2 WHERE id = $1 AND deleted_at IS NULL")
            .bind(&id)
            .bind(now_secs)
            .execute(pool)
            .await
            .map_err(|error| format!("soft-delete postgres skill {id}: {error}"))?;
    }

    Ok(())
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

async fn load_skill_metadata_pg(
    pool: &sqlx::PgPool,
) -> Result<HashMap<String, SkillMetadata>, String> {
    let rows = sqlx::query(
        "SELECT id,
                COALESCE(name, id) AS skill_name,
                COALESCE(description, name, id) AS skill_desc
         FROM skills
         WHERE deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres skill metadata: {error}"))?;

    let mut metadata = HashMap::new();
    for row in rows {
        let skill_id = row
            .try_get::<String, _>("id")
            .map_err(|error| format!("decode postgres skill metadata id: {error}"))?;
        let skill_name = row
            .try_get::<String, _>("skill_name")
            .map_err(|error| format!("decode postgres skill metadata name: {error}"))?;
        let skill_desc = row
            .try_get::<String, _>("skill_desc")
            .map_err(|error| format!("decode postgres skill metadata description: {error}"))?;
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

async fn load_stale_skill_ids_pg(
    pool: &sqlx::PgPool,
    disk_skill_ids: &HashSet<String>,
) -> Result<Vec<String>, String> {
    let rows = sqlx::query("SELECT id FROM skills ORDER BY id ASC")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres stale skill ids: {error}"))?;

    let mut stale_skill_ids = Vec::new();
    for row in rows {
        let skill_id = row
            .try_get::<String, _>("id")
            .map_err(|error| format!("decode postgres stale skill id: {error}"))?;
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

const DIRECT_TRANSCRIPT_DEDUPE_WINDOW_MS: i64 = 30 * 60 * 1000;

fn skill_markdown_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([A-Za-z0-9][A-Za-z0-9._-]*)/SKILL\.md").expect("valid skill markdown regex")
    })
}

fn normalize_skill_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('/');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn extract_skill_id_from_payload(content: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
    ["skill", "name", "command"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(|field| field.as_str()))
        .and_then(normalize_skill_id)
}

fn infer_skills_from_transcript_pg(
    assistant_message: &str,
    events: &[SessionTranscriptEvent],
    known_skills: &HashSet<String>,
) -> HashSet<String> {
    let mut used = HashSet::new();
    let mut searchable = String::from(assistant_message);

    for event in events {
        searchable.push('\n');
        searchable.push_str(event.summary.as_deref().unwrap_or(""));
        searchable.push('\n');
        searchable.push_str(&event.content);

        if event.kind == SessionTranscriptEventKind::ToolUse
            && event
                .tool_name
                .as_deref()
                .is_some_and(|tool_name| tool_name.eq_ignore_ascii_case("Skill"))
            && let Some(skill_id) = extract_skill_id_from_payload(&event.content)
        {
            used.insert(skill_id);
        }
    }

    for captures in skill_markdown_re().captures_iter(&searchable) {
        let Some(skill_id) = captures.get(1).map(|m| m.as_str()) else {
            continue;
        };
        if known_skills.contains(skill_id) {
            used.insert(skill_id.to_string());
        }
    }

    used
}

async fn collect_known_skills_pg(pool: &sqlx::PgPool) -> Result<HashSet<String>, String> {
    let rows = sqlx::query("SELECT id FROM skills")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres known skills: {error}"))?;
    let mut skills = HashSet::new();
    for row in rows {
        let skill_id = row
            .try_get::<String, _>("id")
            .map_err(|error| format!("decode postgres known skill id: {error}"))?;
        if let Some(normalized) = normalize_skill_id(&skill_id) {
            skills.insert(normalized);
        }
    }
    Ok(skills)
}

async fn load_transcript_skill_usage_pg(
    pool: &sqlx::PgPool,
    days: Option<i64>,
    known_skills: &HashSet<String>,
) -> Result<Vec<SkillUsageRecord>, String> {
    let rows = if let Some(days) = days {
        sqlx::query(
            "SELECT st.session_key,
                    st.agent_id,
                    COALESCE(a.name_ko, a.name, st.agent_id) AS agent_name,
                    (EXTRACT(EPOCH FROM st.created_at) * 1000)::BIGINT AS used_at_ms,
                    TO_CHAR(st.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS stat_day,
                    st.assistant_message,
                    st.events_json::TEXT AS events_json
             FROM session_transcripts st
             LEFT JOIN agents a ON a.id = st.agent_id
             WHERE st.created_at >= NOW() - ($1::BIGINT * INTERVAL '1 day')
               AND (
                    st.assistant_message LIKE '%SKILL.md%'
                    OR st.events_json::TEXT LIKE '%SKILL.md%'
                    OR st.events_json::TEXT LIKE '%\"tool_name\":\"Skill\"%'
                    OR st.events_json::TEXT LIKE '%\"tool_name\": \"Skill\"%'
               )",
        )
        .bind(days)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(
            "SELECT st.session_key,
                    st.agent_id,
                    COALESCE(a.name_ko, a.name, st.agent_id) AS agent_name,
                    (EXTRACT(EPOCH FROM st.created_at) * 1000)::BIGINT AS used_at_ms,
                    TO_CHAR(st.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS stat_day,
                    st.assistant_message,
                    st.events_json::TEXT AS events_json
             FROM session_transcripts st
             LEFT JOIN agents a ON a.id = st.agent_id
             WHERE st.assistant_message LIKE '%SKILL.md%'
                OR st.events_json::TEXT LIKE '%SKILL.md%'
                OR st.events_json::TEXT LIKE '%\"tool_name\":\"Skill\"%'
                OR st.events_json::TEXT LIKE '%\"tool_name\": \"Skill\"%'",
        )
        .fetch_all(pool)
        .await
    }
    .map_err(|error| format!("load postgres transcript skill usage: {error}"))?;

    let mut records = Vec::new();
    for row in rows {
        let session_key = row
            .try_get::<Option<String>, _>("session_key")
            .map_err(|error| format!("decode postgres transcript session_key: {error}"))?;
        let agent_id = row
            .try_get::<Option<String>, _>("agent_id")
            .map_err(|error| format!("decode postgres transcript agent_id: {error}"))?;
        let agent_name = row
            .try_get::<Option<String>, _>("agent_name")
            .map_err(|error| format!("decode postgres transcript agent_name: {error}"))?;
        let Some(used_at_ms) = row
            .try_get::<Option<i64>, _>("used_at_ms")
            .map_err(|error| format!("decode postgres transcript used_at_ms: {error}"))?
        else {
            continue;
        };
        let Some(day) = row
            .try_get::<Option<String>, _>("stat_day")
            .map_err(|error| format!("decode postgres transcript stat_day: {error}"))?
        else {
            continue;
        };
        let assistant_message = row
            .try_get::<Option<String>, _>("assistant_message")
            .map_err(|error| format!("decode postgres transcript assistant_message: {error}"))?
            .unwrap_or_default();
        let events_json = row
            .try_get::<String, _>("events_json")
            .map_err(|error| format!("decode postgres transcript events_json: {error}"))?;
        let events =
            serde_json::from_str::<Vec<SessionTranscriptEvent>>(&events_json).unwrap_or_default();
        for skill_id in infer_skills_from_transcript_pg(&assistant_message, &events, known_skills) {
            records.push(SkillUsageRecord {
                skill_id,
                agent_id: agent_id.clone(),
                agent_name: agent_name.clone(),
                session_key: session_key.clone(),
                used_at_ms,
                day: day.clone(),
            });
        }
    }

    Ok(records)
}

async fn load_direct_skill_usage_pg(
    pool: &sqlx::PgPool,
    days: Option<i64>,
) -> Result<Vec<SkillUsageRecord>, String> {
    let rows = if let Some(days) = days {
        sqlx::query(
            "SELECT su.skill_id,
                    su.agent_id,
                    COALESCE(a.name_ko, a.name, su.agent_id) AS agent_name,
                    su.session_key,
                    (EXTRACT(EPOCH FROM su.used_at) * 1000)::BIGINT AS used_at_ms,
                    TO_CHAR(su.used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS stat_day
             FROM skill_usage su
             LEFT JOIN agents a ON a.id = su.agent_id
             WHERE su.used_at >= NOW() - ($1::BIGINT * INTERVAL '1 day')",
        )
        .bind(days)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(
            "SELECT su.skill_id,
                    su.agent_id,
                    COALESCE(a.name_ko, a.name, su.agent_id) AS agent_name,
                    su.session_key,
                    (EXTRACT(EPOCH FROM su.used_at) * 1000)::BIGINT AS used_at_ms,
                    TO_CHAR(su.used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS stat_day
             FROM skill_usage su
             LEFT JOIN agents a ON a.id = su.agent_id",
        )
        .fetch_all(pool)
        .await
    }
    .map_err(|error| format!("load postgres direct skill usage: {error}"))?;

    let mut records = Vec::new();
    for row in rows {
        let raw_skill_id = row
            .try_get::<Option<String>, _>("skill_id")
            .map_err(|error| format!("decode postgres direct skill_id: {error}"))?;
        let Some(skill_id) = raw_skill_id.as_deref().and_then(normalize_skill_id) else {
            continue;
        };
        let Some(used_at_ms) = row
            .try_get::<Option<i64>, _>("used_at_ms")
            .map_err(|error| format!("decode postgres direct used_at_ms: {error}"))?
        else {
            continue;
        };
        let Some(day) = row
            .try_get::<Option<String>, _>("stat_day")
            .map_err(|error| format!("decode postgres direct stat_day: {error}"))?
        else {
            continue;
        };
        records.push(SkillUsageRecord {
            skill_id,
            agent_id: row
                .try_get::<Option<String>, _>("agent_id")
                .map_err(|error| format!("decode postgres direct agent_id: {error}"))?,
            agent_name: row
                .try_get::<Option<String>, _>("agent_name")
                .map_err(|error| format!("decode postgres direct agent_name: {error}"))?,
            session_key: row
                .try_get::<Option<String>, _>("session_key")
                .map_err(|error| format!("decode postgres direct session_key: {error}"))?,
            used_at_ms,
            day,
        });
    }

    Ok(records)
}

struct UsageMatcher {
    by_session: HashMap<(String, String), Vec<i64>>,
    by_agent: HashMap<(String, String), Vec<i64>>,
}

impl UsageMatcher {
    fn new(records: &[SkillUsageRecord]) -> Self {
        let mut by_session = HashMap::new();
        let mut by_agent = HashMap::new();

        for record in records {
            if let Some(session_key) = record.session_key.as_ref() {
                by_session
                    .entry((record.skill_id.clone(), session_key.clone()))
                    .or_insert_with(Vec::new)
                    .push(record.used_at_ms);
            }
            if let Some(agent_id) = record.agent_id.as_ref() {
                by_agent
                    .entry((record.skill_id.clone(), agent_id.clone()))
                    .or_insert_with(Vec::new)
                    .push(record.used_at_ms);
            }
        }

        Self {
            by_session,
            by_agent,
        }
    }

    fn matches_transcript(&mut self, record: &SkillUsageRecord) -> bool {
        if let Some(session_key) = record.session_key.as_ref()
            && Self::consume_matching_timestamp(
                self.by_session
                    .get_mut(&(record.skill_id.clone(), session_key.clone())),
                record.used_at_ms,
            )
        {
            return true;
        }

        if let Some(agent_id) = record.agent_id.as_ref()
            && Self::consume_matching_timestamp(
                self.by_agent
                    .get_mut(&(record.skill_id.clone(), agent_id.clone())),
                record.used_at_ms,
            )
        {
            return true;
        }

        false
    }

    fn consume_matching_timestamp(timestamps: Option<&mut Vec<i64>>, used_at_ms: i64) -> bool {
        let Some(timestamps) = timestamps else {
            return false;
        };
        let Some((index, _)) = timestamps
            .iter()
            .enumerate()
            .filter(|(_, ts)| (*ts - used_at_ms).abs() <= DIRECT_TRANSCRIPT_DEDUPE_WINDOW_MS)
            .min_by_key(|(_, ts)| (*ts - used_at_ms).abs())
        else {
            return false;
        };
        timestamps.swap_remove(index);
        true
    }
}

async fn collect_skill_usage_pg(
    pool: &sqlx::PgPool,
    days: Option<i64>,
) -> Result<Vec<SkillUsageRecord>, String> {
    let known_skills = collect_known_skills_pg(pool).await?;
    let mut transcript_records = load_transcript_skill_usage_pg(pool, days, &known_skills).await?;
    let direct_records = load_direct_skill_usage_pg(pool, days).await?;
    let mut matcher = UsageMatcher::new(&transcript_records);

    transcript_records.extend(
        direct_records
            .into_iter()
            .filter(|record| !matcher.matches_transcript(record)),
    );
    transcript_records.sort_by_key(|record| record.used_at_ms);
    Ok(transcript_records)
}

fn build_catalog_payload(
    metadata: HashMap<String, SkillMetadata>,
    usage: Vec<SkillUsageRecord>,
    disk_skill_ids: &HashSet<String>,
    include_stale: bool,
) -> serde_json::Value {
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

    json!({
        "catalog": catalog,
        "include_stale": include_stale,
    })
}

fn build_ranking_payload(
    metadata: HashMap<String, SkillMetadata>,
    usage: Vec<SkillUsageRecord>,
    disk_skill_ids: &HashSet<String>,
    window: &str,
    limit: i64,
    include_stale: bool,
) -> serde_json::Value {
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

    json!({
        "window": window,
        "include_stale": include_stale,
        "overall": overall,
        "byAgent": by_agent,
    })
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
    let include_stale = params.include_stale.unwrap_or(false);
    if let Some(pool) = state.pg_pool_ref() {
        let disk_skill_ids = match sync_skills_from_disk_pg(pool).await {
            Ok(ids) => ids,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("skill sync failed: {error}")})),
                );
            }
        };
        let metadata = match load_skill_metadata_pg(pool).await {
            Ok(data) => data,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("metadata query failed: {error}")})),
                );
            }
        };
        let usage = match collect_skill_usage_pg(pool, None).await {
            Ok(data) => data,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("usage query failed: {error}")})),
                );
            }
        };
        return (
            StatusCode::OK,
            Json(build_catalog_payload(
                metadata,
                usage,
                &disk_skill_ids,
                include_stale,
            )),
        );
    }

    let conn = match state.sqlite_db().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let disk_skill_ids = sync_skills_from_disk(&conn);
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
    (
        StatusCode::OK,
        Json(build_catalog_payload(
            metadata,
            usage,
            &disk_skill_ids,
            include_stale,
        )),
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
    let window = params.window.as_deref().unwrap_or("7d");
    let limit = params.limit.unwrap_or(20);
    let include_stale = params.include_stale.unwrap_or(false);
    if let Some(pool) = state.pg_pool_ref() {
        let disk_skill_ids = match sync_skills_from_disk_pg(pool).await {
            Ok(ids) => ids,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("skill sync failed: {error}")})),
                );
            }
        };
        let metadata = match load_skill_metadata_pg(pool).await {
            Ok(data) => data,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("metadata query failed: {error}")})),
                );
            }
        };
        let usage = match collect_skill_usage_pg(pool, ranking_days(window)).await {
            Ok(data) => data,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("usage query failed: {error}")})),
                );
            }
        };
        return (
            StatusCode::OK,
            Json(build_ranking_payload(
                metadata,
                usage,
                &disk_skill_ids,
                window,
                limit,
                include_stale,
            )),
        );
    }

    let conn = match state.sqlite_db().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let disk_skill_ids = sync_skills_from_disk(&conn);
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
    (
        StatusCode::OK,
        Json(build_ranking_payload(
            metadata,
            usage,
            &disk_skill_ids,
            window,
            limit,
            include_stale,
        )),
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
    let dry_run = params.dry_run.unwrap_or(false);
    if let Some(pool) = state.pg_pool_ref() {
        let disk_skill_ids = match sync_skills_from_disk_with_prune_pg(pool, !dry_run).await {
            Ok(ids) => ids,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("skill sync failed: {error}")})),
                );
            }
        };
        let stale_skill_ids = match load_stale_skill_ids_pg(pool, &disk_skill_ids).await {
            Ok(ids) => ids,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("stale skill query failed: {error}")})),
                );
            }
        };

        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "dry_run": dry_run,
                "stale_skill_ids": stale_skill_ids,
                "stale_count": stale_skill_ids.len(),
                "deleted_from_skills": if dry_run { 0 } else { stale_skill_ids.len() },
                "soft_deleted_from_skills": if dry_run { 0 } else { stale_skill_ids.len() },
                "skill_usage_policy": "preserved",
            })),
        );
    }

    let conn = match state.sqlite_db().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

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
            "deleted_from_skills": if dry_run { 0 } else { stale_skill_ids.len() },
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
