use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::future::Future;

use crate::db::session_agent_resolution::resolve_agent_id_for_session_pg;

const FETCH_RECENT_CHANNEL_PAIRS_SQL: &str = "SELECT transcript.user_message,
            transcript.assistant_message,
            transcript.created_at,
            clear_boundary.cleared_at
     FROM session_transcripts AS transcript
     LEFT JOIN channel_session_clear_boundaries AS clear_boundary
       ON clear_boundary.channel_id = transcript.channel_id
     WHERE transcript.channel_id = $1
       AND BTRIM(transcript.user_message) <> ''
       AND BTRIM(transcript.assistant_message) <> ''
     ORDER BY transcript.created_at DESC, transcript.id DESC
     LIMIT $2";

type ChannelTranscriptPairRow = (
    String,
    String,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
);

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelTranscriptPair {
    pub(crate) user_message: String,
    pub(crate) assistant_message: String,
}

pub(crate) async fn record_channel_clear_boundary(
    pg_pool: Option<&PgPool>,
    channel_id: &str,
) -> Result<()> {
    let pool = pg_pool
        .ok_or_else(|| anyhow!("postgres pool is required to persist a channel clear boundary"))?;
    let channel_id = channel_id.trim();
    if channel_id.is_empty() {
        return Err(anyhow!(
            "channel clear boundary requires non-empty channel_id"
        ));
    }

    sqlx::query(
        "INSERT INTO channel_session_clear_boundaries (channel_id, cleared_at)
         VALUES ($1, NOW())
         ON CONFLICT (channel_id) DO UPDATE SET
             cleared_at = GREATEST(
                 channel_session_clear_boundaries.cleared_at,
                 EXCLUDED.cleared_at
             )",
    )
    .bind(channel_id)
    .execute(pool)
    .await
    .map_err(|error| anyhow!("record channel clear boundary failed: {error}"))?;

    Ok(())
}

pub(crate) async fn fetch_recent_channel_pairs(
    pool: &PgPool,
    channel_id: &str,
    limit: u64,
) -> Result<Vec<ChannelTranscriptPair>> {
    fetch_recent_channel_pairs_from_rows(async {
        sqlx::query_as::<_, ChannelTranscriptPairRow>(FETCH_RECENT_CHANNEL_PAIRS_SQL)
            .bind(channel_id)
            .bind(limit.min(i64::MAX as u64) as i64)
            .fetch_all(pool)
            .await
            .map_err(|error| anyhow!("recent channel transcript lookup failed: {error}"))
    })
    .await
}

async fn fetch_recent_channel_pairs_from_rows<F>(rows: F) -> Result<Vec<ChannelTranscriptPair>>
where
    F: Future<Output = Result<Vec<ChannelTranscriptPairRow>>>,
{
    let rows = rows.await?;
    Ok(chronological_channel_pairs_from_desc(
        channel_pairs_after_clear_boundary(rows),
    ))
}

// #4658: frontier-bounded transcript reads for immutable context snapshots.
//
// The capture path (scheduled-message context snapshots) freezes a channel's
// conversation at the last-observed `session_transcripts.id`. It reads the
// frontier and the frontier-bounded recent pairs on the SAME transaction that
// inserts the reservation, so the boundary is atomic with respect to concurrent
// transcript inserts. Rendering/digesting happens in
// `services::scheduled_messages::context_snapshot`.

const FETCH_CHANNEL_PAIRS_UP_TO_FRONTIER_SQL: &str = "SELECT transcript.user_message,
            transcript.assistant_message,
            transcript.created_at,
            clear_boundary.cleared_at
     FROM session_transcripts AS transcript
     LEFT JOIN channel_session_clear_boundaries AS clear_boundary
       ON clear_boundary.channel_id = transcript.channel_id
     WHERE transcript.channel_id = $1
       AND transcript.id <= $2
       AND BTRIM(transcript.user_message) <> ''
       AND BTRIM(transcript.assistant_message) <> ''
     ORDER BY transcript.created_at DESC, transcript.id DESC
     LIMIT $3";

/// The last `session_transcripts.id` for a channel, or 0 when it has none.
/// Runs on a caller-owned transaction so the frontier is consistent with a
/// subsequent [`fetch_channel_pairs_up_to_frontier_tx`] read.
pub(crate) async fn fetch_channel_frontier_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    channel_id: &str,
) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(id), 0) FROM session_transcripts WHERE channel_id = $1",
    )
    .bind(channel_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| anyhow!("channel transcript frontier lookup failed: {error}"))
}

/// Recent channel pairs at or before `frontier`, clear-boundary filtered and
/// returned oldest-first (same ordering contract as `fetch_recent_channel_pairs`).
pub(crate) async fn fetch_channel_pairs_up_to_frontier_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    channel_id: &str,
    frontier: i64,
    limit: u64,
) -> Result<Vec<ChannelTranscriptPair>> {
    let rows =
        sqlx::query_as::<_, ChannelTranscriptPairRow>(FETCH_CHANNEL_PAIRS_UP_TO_FRONTIER_SQL)
            .bind(channel_id)
            .bind(frontier)
            .bind(limit.min(i64::MAX as u64) as i64)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                anyhow!("frontier-bounded channel transcript lookup failed: {error}")
            })?;
    Ok(chronological_channel_pairs_from_desc(
        channel_pairs_after_clear_boundary(rows),
    ))
}

fn channel_pairs_after_clear_boundary(
    rows: Vec<ChannelTranscriptPairRow>,
) -> Vec<ChannelTranscriptPair> {
    rows.into_iter()
        .filter(|(_, _, created_at, cleared_at)| match cleared_at {
            None => true,
            Some(cleared_at) => created_at.is_some_and(|created_at| created_at > *cleared_at),
        })
        .map(
            |(user_message, assistant_message, _created_at, _cleared_at)| ChannelTranscriptPair {
                user_message,
                assistant_message,
            },
        )
        .collect()
}

pub(crate) fn chronological_channel_pairs_from_desc(
    mut pairs: Vec<ChannelTranscriptPair>,
) -> Vec<ChannelTranscriptPair> {
    pairs.reverse();
    pairs
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_channel_pairs_query_breaks_created_at_ties_by_desc_id() {
        assert!(
            FETCH_RECENT_CHANNEL_PAIRS_SQL
                .contains("ORDER BY transcript.created_at DESC, transcript.id DESC"),
            "equal created_at values must use the primary key as a deterministic newest-first tie-breaker"
        );
    }

    #[test]
    fn persisted_clear_boundary_filters_preclear_pairs_without_in_memory_flag() {
        let cleared_at = chrono::DateTime::parse_from_rfc3339("2026-07-14T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let before = cleared_at - chrono::Duration::seconds(1);
        let after = cleared_at + chrono::Duration::seconds(1);
        let rows = vec![
            (
                "post-clear".to_string(),
                "allowed".to_string(),
                Some(after),
                Some(cleared_at),
            ),
            (
                "at-boundary".to_string(),
                "blocked".to_string(),
                Some(cleared_at),
                Some(cleared_at),
            ),
            (
                "pre-clear".to_string(),
                "blocked".to_string(),
                Some(before),
                Some(cleared_at),
            ),
        ];

        // No in-memory `session_was_cleared` state is involved here: this
        // models a fresh process loading only the persisted database boundary.
        let pairs = chronological_channel_pairs_from_desc(channel_pairs_after_clear_boundary(rows));

        assert_eq!(
            pairs,
            vec![ChannelTranscriptPair {
                user_message: "post-clear".to_string(),
                assistant_message: "allowed".to_string(),
            }],
            "a later fresh session must not cross the persisted /clear boundary"
        );
        assert!(
            FETCH_RECENT_CHANNEL_PAIRS_SQL
                .contains("LEFT JOIN channel_session_clear_boundaries AS clear_boundary")
        );
    }

    #[tokio::test]
    async fn goal_fresh_boundary_blocks_prior_pairs_after_restart_through_fetch_pipeline() {
        let cleared_at = chrono::DateTime::parse_from_rfc3339("2026-07-14T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let rows = vec![
            (
                "at-goal-fresh".to_string(),
                "blocked".to_string(),
                Some(cleared_at),
                Some(cleared_at),
            ),
            (
                "before-goal-fresh".to_string(),
                "blocked".to_string(),
                Some(cleared_at - chrono::Duration::seconds(1)),
                Some(cleared_at),
            ),
        ];

        // This uses the same post-query pipeline as `fetch_recent_channel_pairs`
        // with no in-memory force-fresh/session-cleared flag, modeling the first
        // plain fresh turn after dcserver restarts during `/goal fresh`.
        let pairs = fetch_recent_channel_pairs_from_rows(async { Ok(rows) })
            .await
            .unwrap();

        assert!(
            pairs.is_empty(),
            "a restarted fresh session must not fetch pairs at or before the durable /goal fresh boundary"
        );
    }

    #[test]
    fn recent_channel_pairs_are_rendered_oldest_first_after_desc_fetch() {
        let pairs = chronological_channel_pairs_from_desc(vec![
            ChannelTranscriptPair {
                user_message: "higher-id-at-tied-time".to_string(),
                assistant_message: "newer".to_string(),
            },
            ChannelTranscriptPair {
                user_message: "lower-id-at-tied-time".to_string(),
                assistant_message: "older".to_string(),
            },
        ]);

        assert_eq!(pairs[0].user_message, "lower-id-at-tied-time");
        assert_eq!(pairs[1].user_message, "higher-id-at-tied-time");
    }
}
