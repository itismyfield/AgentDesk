use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::db::session_agent_resolution::resolve_agent_id_for_session_pg;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTranscriptEventKind {
    User,
    Assistant,
    Thinking,
    ToolUse,
    ToolResult,
    Result,
    Error,
    Task,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTranscriptEvent {
    pub kind: SessionTranscriptEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct PersistSessionTranscript<'a> {
    pub turn_id: &'a str,
    pub session_key: Option<&'a str>,
    pub channel_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub dispatch_id: Option<&'a str>,
    pub user_message: &'a str,
    pub assistant_message: &'a str,
    pub events: &'a [SessionTranscriptEvent],
    pub duration_ms: Option<i64>,
}

// reason: public transcript record for the read/fetch route; the pg-side load
// path that builds it is wired only on selected API paths. See #3034.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionTranscriptRecord {
    pub id: i64,
    pub turn_id: String,
    pub session_key: Option<String>,
    pub channel_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub dispatch_id: Option<String>,
    pub kanban_card_id: Option<String>,
    pub dispatch_title: Option<String>,
    pub card_title: Option<String>,
    pub github_issue_number: Option<i64>,
    pub user_message: String,
    pub assistant_message: String,
    pub events: Vec<SessionTranscriptEvent>,
    pub duration_ms: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
struct PreparedSessionTranscript {
    turn_id: String,
    session_key: Option<String>,
    channel_id: Option<String>,
    agent_id: Option<String>,
    provider: Option<String>,
    dispatch_id: Option<String>,
    user_message: String,
    assistant_message: String,
    events_json: String,
    duration_ms: Option<i64>,
}

pub async fn persist_turn_db(
    pg_pool: Option<&PgPool>,
    entry: PersistSessionTranscript<'_>,
) -> Result<bool> {
    let Some(pool) = pg_pool else {
        return Err(anyhow!("postgres pool is required to persist transcript"));
    };

    let prepared = prepare_persist_entry_pg(pool, &entry).await?;
    let Some(prepared) = prepared else {
        return Ok(false);
    };

    persist_turn_pg(pool, &prepared).await?;
    Ok(true)
}

async fn persist_turn_pg(pool: &PgPool, entry: &PreparedSessionTranscript) -> Result<()> {
    sqlx::query(
        "INSERT INTO session_transcripts (
            turn_id,
            session_key,
            channel_id,
            agent_id,
            provider,
            dispatch_id,
            user_message,
            assistant_message,
            events_json,
            duration_ms
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CAST($9 AS jsonb), $10)
         ON CONFLICT (turn_id) DO UPDATE SET
            session_key = EXCLUDED.session_key,
            channel_id = EXCLUDED.channel_id,
            agent_id = COALESCE(EXCLUDED.agent_id, session_transcripts.agent_id),
            provider = EXCLUDED.provider,
            dispatch_id = EXCLUDED.dispatch_id,
            user_message = EXCLUDED.user_message,
            assistant_message = EXCLUDED.assistant_message,
            events_json = EXCLUDED.events_json,
            duration_ms = EXCLUDED.duration_ms",
    )
    .bind(&entry.turn_id)
    .bind(&entry.session_key)
    .bind(&entry.channel_id)
    .bind(&entry.agent_id)
    .bind(&entry.provider)
    .bind(&entry.dispatch_id)
    .bind(&entry.user_message)
    .bind(&entry.assistant_message)
    .bind(&entry.events_json)
    .bind(entry.duration_ms)
    .execute(pool)
    .await
    .map_err(|e| anyhow!("persist postgres transcript failed: {e}"))?;
    Ok(())
}

/// #4307: per-turn memento recall/feedback stats surfaced by the `/api/stats`
/// reader (`load_memento_feedback_counts`). Restores the writer a1492c05 dropped
/// when it removed the SQLite twin without porting the PG path — the
/// `memento_feedback_turn_stats` table has shipped in the PG schema all along
/// (migrations/postgres/0001_initial_schema.sql) but nothing wrote to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MementoFeedbackTurnStat {
    pub turn_id: String,
    pub stat_date: String,
    pub agent_id: String,
    pub provider: String,
    pub recall_count: i64,
    pub manual_tool_feedback_count: i64,
    pub manual_covered_recall_count: i64,
    pub auto_tool_feedback_count: i64,
    pub covered_recall_count: i64,
}

/// Upsert a turn's memento feedback stats keyed by `turn_id`. Returns `Err`
/// when no PG pool is available (the caller gates on `pg_pool.is_some()`).
pub async fn record_memento_feedback_turn_stats(
    pg_pool: Option<&PgPool>,
    stat: &MementoFeedbackTurnStat,
) -> Result<()> {
    let Some(pool) = pg_pool else {
        return Err(anyhow!(
            "postgres pool is required to record memento feedback stats"
        ));
    };
    validate_memento_feedback_turn_stat(stat)?;

    sqlx::query(
        "INSERT INTO memento_feedback_turn_stats (
            turn_id,
            stat_date,
            agent_id,
            provider,
            recall_count,
            manual_tool_feedback_count,
            manual_covered_recall_count,
            auto_tool_feedback_count,
            covered_recall_count
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (turn_id) DO UPDATE SET
            stat_date = EXCLUDED.stat_date,
            agent_id = EXCLUDED.agent_id,
            provider = EXCLUDED.provider,
            recall_count = EXCLUDED.recall_count,
            manual_tool_feedback_count = EXCLUDED.manual_tool_feedback_count,
            manual_covered_recall_count = EXCLUDED.manual_covered_recall_count,
            auto_tool_feedback_count = EXCLUDED.auto_tool_feedback_count,
            covered_recall_count = EXCLUDED.covered_recall_count",
    )
    .bind(&stat.turn_id)
    .bind(&stat.stat_date)
    .bind(&stat.agent_id)
    .bind(&stat.provider)
    .bind(stat.recall_count)
    .bind(stat.manual_tool_feedback_count)
    .bind(stat.manual_covered_recall_count)
    .bind(stat.auto_tool_feedback_count)
    .bind(stat.covered_recall_count)
    .execute(pool)
    .await
    .map_err(|e| anyhow!("record memento feedback turn stats failed: {e}"))?;

    Ok(())
}

fn validate_memento_feedback_turn_stat(stat: &MementoFeedbackTurnStat) -> Result<()> {
    if stat.turn_id.trim().is_empty() {
        return Err(anyhow!("memento feedback stats require non-empty turn_id"));
    }
    if stat.stat_date.trim().is_empty() {
        return Err(anyhow!(
            "memento feedback stats require non-empty stat_date"
        ));
    }
    if stat.agent_id.trim().is_empty() {
        return Err(anyhow!("memento feedback stats require non-empty agent_id"));
    }
    if stat.provider.trim().is_empty() {
        return Err(anyhow!("memento feedback stats require non-empty provider"));
    }

    for (label, value) in [
        ("recall_count", stat.recall_count),
        (
            "manual_tool_feedback_count",
            stat.manual_tool_feedback_count,
        ),
        (
            "manual_covered_recall_count",
            stat.manual_covered_recall_count,
        ),
        ("auto_tool_feedback_count", stat.auto_tool_feedback_count),
        ("covered_recall_count", stat.covered_recall_count),
    ] {
        if value < 0 {
            return Err(anyhow!(
                "memento feedback stats {label} must be non-negative"
            ));
        }
    }
    if stat.manual_covered_recall_count > stat.recall_count {
        return Err(anyhow!(
            "manual_covered_recall_count cannot exceed recall_count"
        ));
    }
    if stat.covered_recall_count > stat.recall_count {
        return Err(anyhow!("covered_recall_count cannot exceed recall_count"));
    }
    Ok(())
}

fn prepare_persist_entry_base(
    entry: &PersistSessionTranscript<'_>,
) -> Result<Option<PreparedSessionTranscript>> {
    let turn_id = entry.turn_id.trim();
    if turn_id.is_empty() {
        return Err(anyhow!("turn_id is required"));
    }

    let user_message = entry.user_message.trim();
    let assistant_message = entry.assistant_message.trim();
    let events = normalize_events(entry.events);
    if user_message.is_empty() && assistant_message.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let events_json = serde_json::to_string(&events)?;

    Ok(Some(PreparedSessionTranscript {
        turn_id: turn_id.to_string(),
        session_key: normalized_opt(entry.session_key),
        channel_id: normalized_opt(entry.channel_id),
        agent_id: None,
        provider: normalized_opt(entry.provider),
        dispatch_id: normalized_opt(entry.dispatch_id),
        user_message: user_message.to_string(),
        assistant_message: assistant_message.to_string(),
        events_json,
        duration_ms: entry.duration_ms,
    }))
}

async fn prepare_persist_entry_pg(
    pool: &PgPool,
    entry: &PersistSessionTranscript<'_>,
) -> Result<Option<PreparedSessionTranscript>> {
    let Some(mut prepared) = prepare_persist_entry_base(entry)? else {
        return Ok(None);
    };

    prepared.agent_id = resolve_agent_id_for_session_pg(
        pool,
        entry.agent_id,
        prepared.session_key.as_deref(),
        None,
        None,
        prepared.dispatch_id.as_deref(),
        None,
    )
    .await;

    Ok(Some(prepared))
}

pub fn dispatch_has_assistant_response_db(
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<bool> {
    let Some(pool) = pg_pool else {
        return Ok(false);
    };

    let dispatch_id = dispatch_id.to_string();
    run_pg_blocking(pool, move |pool| async move {
        dispatch_has_assistant_response_pg(&pool, &dispatch_id).await
    })
}

async fn dispatch_has_assistant_response_pg(pool: &PgPool, dispatch_id: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM session_transcripts
         WHERE dispatch_id = $1
           AND BTRIM(assistant_message) <> ''",
    )
    .bind(dispatch_id)
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow!("session transcript lookup failed: {e}"))
}

fn run_pg_blocking<T, F>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| {
        anyhow!("build runtime for postgres transcript query failed: {error}")
    })
}

// reason: transcript read-side helper that feeds SessionTranscriptRecord; wired
// only on the selected transcript-fetch path. See #3034.
#[allow(dead_code)]
fn parse_events_json(raw: Option<&str>) -> Vec<SessionTranscriptEvent> {
    raw.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            serde_json::from_str::<Vec<SessionTranscriptEvent>>(trimmed).ok()
        }
    })
    .map(|events| normalize_events(&events))
    .unwrap_or_default()
}

fn normalize_events(events: &[SessionTranscriptEvent]) -> Vec<SessionTranscriptEvent> {
    events
        .iter()
        .filter_map(|event| {
            let mut normalized = event.clone();
            normalized.content = normalized.content.trim().to_string();
            normalized.summary = normalized
                .summary
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            normalized.tool_name = normalized
                .tool_name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            normalized.status = normalized
                .status
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);

            if normalized.content.is_empty()
                && normalized.summary.is_none()
                && normalized.tool_name.is_none()
                && !matches!(
                    normalized.kind,
                    SessionTranscriptEventKind::Thinking
                        | SessionTranscriptEventKind::Result
                        | SessionTranscriptEventKind::Error
                        | SessionTranscriptEventKind::System
                )
            {
                return None;
            }

            Some(normalized)
        })
        .collect()
}

fn normalized_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}
