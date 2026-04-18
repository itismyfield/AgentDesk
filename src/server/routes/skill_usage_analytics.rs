use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use chrono::{DateTime, NaiveDateTime, Utc};
use libsql_rusqlite::Connection;
use regex::Regex;
use serde_json::Value;

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};

const DIRECT_TRANSCRIPT_DEDUPE_WINDOW_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SkillUsageRecord {
    pub skill_id: String,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub session_key: Option<String>,
    pub used_at_ms: i64,
    pub day: String,
}

fn skill_markdown_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([A-Za-z0-9][A-Za-z0-9._-]*)/SKILL\.md").expect("valid skill markdown regex")
    })
}

fn sqlite_datetime_to_millis(value: &str) -> Option<i64> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(value) {
        return Some(ts.timestamp_millis());
    }
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|ts| DateTime::<Utc>::from_naive_utc_and_offset(ts, Utc).timestamp_millis())
}

fn normalize_skill_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('/');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn extract_skill_id_from_payload(content: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(content).ok()?;
    ["skill", "name", "command"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(|field| field.as_str()))
        .and_then(normalize_skill_id)
}

fn infer_skills_from_transcript(
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

fn collect_known_skills(conn: &Connection) -> libsql_rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT id FROM skills")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut skills = HashSet::new();
    for skill_id in rows.flatten() {
        if let Some(normalized) = normalize_skill_id(&skill_id) {
            skills.insert(normalized);
        }
    }
    Ok(skills)
}

fn load_transcript_skill_usage(
    conn: &Connection,
    days: Option<i64>,
    known_skills: &HashSet<String>,
) -> libsql_rusqlite::Result<Vec<SkillUsageRecord>> {
    let sql_all = "SELECT st.session_key,
                          st.agent_id,
                          COALESCE(a.name_ko, a.name, st.agent_id) AS agent_name,
                          st.created_at,
                          DATE(st.created_at) AS stat_day,
                          st.assistant_message,
                          st.events_json
                   FROM session_transcripts st
                   LEFT JOIN agents a ON a.id = st.agent_id
                   WHERE st.assistant_message LIKE '%SKILL.md%'
                      OR st.events_json LIKE '%SKILL.md%'
                      OR st.events_json LIKE '%\"tool_name\":\"Skill\"%'";
    let sql_window = "SELECT st.session_key,
                             st.agent_id,
                             COALESCE(a.name_ko, a.name, st.agent_id) AS agent_name,
                             st.created_at,
                             DATE(st.created_at) AS stat_day,
                             st.assistant_message,
                             st.events_json
                      FROM session_transcripts st
                      LEFT JOIN agents a ON a.id = st.agent_id
                      WHERE st.created_at >= datetime('now', '-' || ?1 || ' days')
                        AND (
                            st.assistant_message LIKE '%SKILL.md%'
                            OR st.events_json LIKE '%SKILL.md%'
                            OR st.events_json LIKE '%\"tool_name\":\"Skill\"%'
                        )";

    let mut records = Vec::new();
    let mut push_rows = |stmt: &mut libsql_rusqlite::Statement<'_>,
                         params: &[&dyn libsql_rusqlite::ToSql]|
     -> libsql_rusqlite::Result<()> {
        let rows = stmt.query_map(params, |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                row.get::<_, String>(6)?,
            ))
        })?;

        for row in rows {
            let (
                session_key,
                agent_id,
                agent_name,
                created_at,
                day,
                assistant_message,
                events_json,
            ) = match row {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(used_at_ms) = sqlite_datetime_to_millis(&created_at) else {
                continue;
            };
            let events = serde_json::from_str::<Vec<SessionTranscriptEvent>>(&events_json)
                .unwrap_or_default();
            for skill_id in infer_skills_from_transcript(&assistant_message, &events, known_skills)
            {
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
        Ok(())
    };

    if let Some(days) = days {
        let mut stmt = conn.prepare(sql_window)?;
        push_rows(&mut stmt, &[&days])?;
    } else {
        let mut stmt = conn.prepare(sql_all)?;
        push_rows(&mut stmt, &[])?;
    }

    Ok(records)
}

fn load_direct_skill_usage(
    conn: &Connection,
    days: Option<i64>,
) -> libsql_rusqlite::Result<Vec<SkillUsageRecord>> {
    let sql_all = "SELECT su.skill_id,
                          su.agent_id,
                          COALESCE(a.name_ko, a.name, su.agent_id) AS agent_name,
                          su.session_key,
                          CAST(strftime('%s', su.used_at) AS INTEGER) * 1000 AS used_at_ms,
                          DATE(su.used_at) AS stat_day
                   FROM skill_usage su
                   LEFT JOIN agents a ON a.id = su.agent_id";
    let sql_window = "SELECT su.skill_id,
                             su.agent_id,
                             COALESCE(a.name_ko, a.name, su.agent_id) AS agent_name,
                             su.session_key,
                             CAST(strftime('%s', su.used_at) AS INTEGER) * 1000 AS used_at_ms,
                             DATE(su.used_at) AS stat_day
                      FROM skill_usage su
                      LEFT JOIN agents a ON a.id = su.agent_id
                      WHERE su.used_at >= datetime('now', '-' || ?1 || ' days')";

    let mut records = Vec::new();
    let mut push_rows = |stmt: &mut libsql_rusqlite::Statement<'_>,
                         params: &[&dyn libsql_rusqlite::ToSql]|
     -> libsql_rusqlite::Result<()> {
        let rows = stmt.query_map(params, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;

        for row in rows {
            let (skill_id, agent_id, agent_name, session_key, used_at_ms, day) = match row {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(skill_id) = normalize_skill_id(&skill_id) else {
                continue;
            };
            let Some(used_at_ms) = used_at_ms else {
                continue;
            };
            let Some(day) = day else {
                continue;
            };
            records.push(SkillUsageRecord {
                skill_id,
                agent_id,
                agent_name,
                session_key,
                used_at_ms,
                day,
            });
        }
        Ok(())
    };

    if let Some(days) = days {
        let mut stmt = conn.prepare(sql_window)?;
        push_rows(&mut stmt, &[&days])?;
    } else {
        let mut stmt = conn.prepare(sql_all)?;
        push_rows(&mut stmt, &[])?;
    }

    Ok(records)
}

struct TranscriptUsageMatcher {
    by_session: HashMap<(String, String), Vec<i64>>,
    by_agent: HashMap<(String, String), Vec<i64>>,
}

impl TranscriptUsageMatcher {
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

pub(super) fn collect_skill_usage(
    conn: &Connection,
    days: Option<i64>,
) -> libsql_rusqlite::Result<Vec<SkillUsageRecord>> {
    let known_skills = collect_known_skills(conn)?;
    let mut transcript_records = load_transcript_skill_usage(conn, days, &known_skills)?;
    let direct_records = load_direct_skill_usage(conn, days)?;
    let mut matcher = TranscriptUsageMatcher::new(&transcript_records);

    transcript_records.extend(
        direct_records
            .into_iter()
            .filter(|record| !matcher.matches_transcript(record)),
    );
    transcript_records.sort_by_key(|record| record.used_at_ms);
    Ok(transcript_records)
}

#[cfg(test)]
mod tests {
    use super::collect_skill_usage;
    use std::collections::HashMap;

    fn setup_conn() -> libsql_rusqlite::Connection {
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
            CREATE TABLE agents (
                id TEXT PRIMARY KEY,
                name TEXT,
                name_ko TEXT
            );
            CREATE TABLE session_transcripts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id TEXT NOT NULL UNIQUE,
                session_key TEXT,
                channel_id TEXT,
                agent_id TEXT,
                provider TEXT,
                dispatch_id TEXT,
                user_message TEXT NOT NULL DEFAULT '',
                assistant_message TEXT NOT NULL DEFAULT '',
                events_json TEXT NOT NULL DEFAULT '[]',
                duration_ms INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
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

    #[test]
    fn hybrid_usage_uses_transcripts_and_keeps_only_unmatched_direct_rows() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO skills (id, name, description) VALUES (?1, ?2, ?3)",
            libsql_rusqlite::params!["create-issue", "create-issue", "issue"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skills (id, name, description) VALUES (?1, ?2, ?3)",
            libsql_rusqlite::params!["memory-write", "memory-write", "memory"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, name_ko) VALUES (?1, ?2, ?3)",
            libsql_rusqlite::params!["project-agentdesk", "AgentDesk", "AgentDesk 에이전트"],
        )
        .unwrap();

        let events = serde_json::json!([
            {
                "kind": "tool_use",
                "tool_name": "Skill",
                "summary": "create-issue",
                "content": "{\"skill\":\"create-issue\"}",
                "status": "running",
                "is_error": false
            },
            {
                "kind": "tool_use",
                "tool_name": "Read",
                "summary": null,
                "content": "{\"file_path\":\"/tmp/skills/create-issue/SKILL.md\"}",
                "status": "running",
                "is_error": false
            }
        ]);
        conn.execute(
            "INSERT INTO session_transcripts (
                turn_id,
                session_key,
                agent_id,
                assistant_message,
                events_json,
                created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            libsql_rusqlite::params![
                "turn-1",
                "sess-1",
                "project-agentdesk",
                "99_Skills/create-issue/SKILL.md 를 읽습니다",
                events.to_string(),
                "2026-04-12 12:00:00"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_transcripts (
                turn_id,
                session_key,
                agent_id,
                assistant_message,
                events_json,
                created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            libsql_rusqlite::params![
                "turn-2",
                "sess-2",
                "project-agentdesk",
                "memory-write/SKILL.md 를 읽고 메모리를 정리합니다",
                "[]",
                "2026-04-12 13:00:00"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
             VALUES (?1, ?2, ?3, ?4)",
            libsql_rusqlite::params![
                "create-issue",
                "project-agentdesk",
                "sess-1",
                "2026-04-12 12:01:00"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
             VALUES (?1, ?2, ?3, ?4)",
            libsql_rusqlite::params![
                "create-issue",
                "project-agentdesk",
                "sess-other",
                "2026-04-12 14:00:00"
            ],
        )
        .unwrap();

        let records = collect_skill_usage(&conn, None).unwrap();
        let mut counts = HashMap::new();
        for record in records {
            *counts.entry(record.skill_id).or_insert(0) += 1;
        }

        assert_eq!(counts.get("create-issue"), Some(&2));
        assert_eq!(counts.get("memory-write"), Some(&1));
    }
}
