use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use chrono::{DateTime, NaiveDateTime, Utc};
use regex::Regex;
use serde_json::Value;
use sqlx::{PgPool, Row};

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

fn transcript_datetime_to_millis(value: &str) -> Option<i64> {
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

async fn collect_known_skills_pg(pool: &PgPool) -> Result<HashSet<String>, sqlx::Error> {
    let rows = sqlx::query("SELECT id FROM skills").fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.try_get::<String, _>("id").ok())
        .filter_map(|skill_id| normalize_skill_id(&skill_id))
        .collect())
}

async fn load_transcript_skill_usage_pg(
    pool: &PgPool,
    days: Option<i64>,
    known_skills: &HashSet<String>,
) -> Result<Vec<SkillUsageRecord>, sqlx::Error> {
    let sql_all = "SELECT st.session_key,
                          st.agent_id,
                          COALESCE(a.name_ko, a.name, st.agent_id) AS agent_name,
                          TO_CHAR(st.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at,
                          TO_CHAR(st.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS stat_day,
                          st.assistant_message,
                          st.events_json::TEXT AS events_json
                   FROM session_transcripts st
                   LEFT JOIN agents a ON a.id = st.agent_id
                   WHERE st.assistant_message LIKE '%SKILL.md%'
                      OR st.events_json::TEXT LIKE '%SKILL.md%'
                      OR st.events_json::TEXT LIKE '%\"tool_name\":\"Skill\"%'";
    let sql_window = "SELECT st.session_key,
                             st.agent_id,
                             COALESCE(a.name_ko, a.name, st.agent_id) AS agent_name,
                             TO_CHAR(st.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at,
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
                        )";

    let rows = if let Some(days) = days {
        sqlx::query(sql_window).bind(days).fetch_all(pool).await?
    } else {
        sqlx::query(sql_all).fetch_all(pool).await?
    };

    let mut records = Vec::new();
    for row in rows {
        let session_key = row
            .try_get::<Option<String>, _>("session_key")
            .ok()
            .flatten();
        let agent_id = row.try_get::<Option<String>, _>("agent_id").ok().flatten();
        let agent_name = row
            .try_get::<Option<String>, _>("agent_name")
            .ok()
            .flatten();
        let created_at = row.try_get::<String, _>("created_at").unwrap_or_default();
        let day = row.try_get::<String, _>("stat_day").unwrap_or_default();
        let assistant_message = row
            .try_get::<Option<String>, _>("assistant_message")
            .ok()
            .flatten()
            .unwrap_or_default();
        let events_json = row
            .try_get::<String, _>("events_json")
            .unwrap_or_else(|_| "[]".to_string());
        let Some(used_at_ms) = transcript_datetime_to_millis(&created_at) else {
            continue;
        };
        let events =
            serde_json::from_str::<Vec<SessionTranscriptEvent>>(&events_json).unwrap_or_default();
        for skill_id in infer_skills_from_transcript(&assistant_message, &events, known_skills) {
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
    pool: &PgPool,
    days: Option<i64>,
) -> Result<Vec<SkillUsageRecord>, sqlx::Error> {
    let sql_all = "SELECT su.skill_id,
                          su.agent_id,
                          COALESCE(a.name_ko, a.name, su.agent_id) AS agent_name,
                          su.session_key,
                          (EXTRACT(EPOCH FROM su.used_at)::BIGINT * 1000) AS used_at_ms,
                          TO_CHAR(su.used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS stat_day
                   FROM skill_usage su
                   LEFT JOIN agents a ON a.id = su.agent_id";
    let sql_window = "SELECT su.skill_id,
                             su.agent_id,
                             COALESCE(a.name_ko, a.name, su.agent_id) AS agent_name,
                             su.session_key,
                             (EXTRACT(EPOCH FROM su.used_at)::BIGINT * 1000) AS used_at_ms,
                             TO_CHAR(su.used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS stat_day
                      FROM skill_usage su
                      LEFT JOIN agents a ON a.id = su.agent_id
                      WHERE su.used_at >= NOW() - ($1::BIGINT * INTERVAL '1 day')";

    let rows = if let Some(days) = days {
        sqlx::query(sql_window).bind(days).fetch_all(pool).await?
    } else {
        sqlx::query(sql_all).fetch_all(pool).await?
    };

    let mut records = Vec::new();
    for row in rows {
        let Some(skill_id) = row
            .try_get::<String, _>("skill_id")
            .ok()
            .and_then(|skill_id| normalize_skill_id(&skill_id))
        else {
            continue;
        };
        let Some(used_at_ms) = row.try_get::<Option<i64>, _>("used_at_ms").ok().flatten() else {
            continue;
        };
        let Some(day) = row.try_get::<Option<String>, _>("stat_day").ok().flatten() else {
            continue;
        };
        records.push(SkillUsageRecord {
            skill_id,
            agent_id: row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
            agent_name: row
                .try_get::<Option<String>, _>("agent_name")
                .ok()
                .flatten(),
            session_key: row
                .try_get::<Option<String>, _>("session_key")
                .ok()
                .flatten(),
            used_at_ms,
            day,
        });
    }

    Ok(records)
}

pub(super) async fn collect_skill_usage_pg(
    pool: &PgPool,
    days: Option<i64>,
) -> Result<Vec<SkillUsageRecord>, sqlx::Error> {
    let known_skills = collect_known_skills_pg(pool).await?;
    let mut transcript_records = load_transcript_skill_usage_pg(pool, days, &known_skills).await?;
    let direct_records = load_direct_skill_usage_pg(pool, days).await?;
    let mut matcher = TranscriptUsageMatcher::new(&transcript_records);

    transcript_records.extend(
        direct_records
            .into_iter()
            .filter(|record| !matcher.matches_transcript(record)),
    );
    transcript_records.sort_by_key(|record| record.used_at_ms);
    Ok(transcript_records)
}
