use anyhow::{Result, anyhow};
use poise::serenity_prelude::MessageId;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use std::future::Future;

use crate::db::session_agent_resolution::resolve_agent_id_for_session_pg;

const FETCH_RECENT_CHANNEL_PAIRS_SQL: &str = "SELECT transcript.user_message,
            transcript.assistant_message,
            transcript.created_at,
            clear_boundary.cleared_at,
            transcript.id,
            clear_boundary.cleared_through_id
     FROM session_transcripts AS transcript
     LEFT JOIN channel_session_clear_boundaries AS clear_boundary
       ON clear_boundary.channel_id = transcript.channel_id
     WHERE transcript.channel_id = $1
       AND BTRIM(transcript.user_message) <> ''
       AND BTRIM(transcript.assistant_message) <> ''
       AND (clear_boundary.cleared_through_id IS NULL
            OR transcript.id > clear_boundary.cleared_through_id)
     ORDER BY transcript.created_at DESC, transcript.id DESC
     LIMIT $2";

type ChannelTranscriptPairRow = (
    String,
    String,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
    i64,
    Option<i64>,
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
    /// Unix timestamp (milliseconds) used to fence interactive turns against `/clear`.
    /// `None` preserves persistence for callers that do not represent user turns.
    pub turn_started_at_millis: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelTranscriptPair {
    pub(crate) user_message: String,
    pub(crate) assistant_message: String,
}

pub(crate) fn discord_message_started_at_millis(message_id: Option<MessageId>) -> Option<i64> {
    message_id.map(|message_id| message_id.created_at().timestamp_millis())
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

    let tx = begin_channel_clear_boundary_tx(pool).await?;
    finish_channel_clear_boundary_tx(tx, channel_id).await
}

/// #5707 phase 1 of a boundary write: open the transaction, take no lock yet.
/// The transaction's `NOW()` — the value that becomes `cleared_at` — is already
/// frozen here, which is why it can be earlier than a transcript this clear
/// ends up covering.
pub(crate) async fn begin_channel_clear_boundary_tx(
    pool: &PgPool,
) -> Result<Transaction<'_, Postgres>> {
    pool.begin()
        .await
        .map_err(|error| anyhow!("begin channel clear boundary transaction failed: {error}"))
}

/// #5707 phase 2: take the channel lock, advance both durable markers, commit.
/// `cleared_through_id` is `MAX(session_transcripts.id)` read after the lock, so
/// it covers every transcript row serialized before this clear.
pub(crate) async fn finish_channel_clear_boundary_tx(
    mut tx: Transaction<'_, Postgres>,
    channel_id: &str,
) -> Result<()> {
    let channel_id = channel_id.trim();
    if channel_id.is_empty() {
        return Err(anyhow!(
            "channel clear boundary requires non-empty channel_id"
        ));
    }
    lock_channel_transcript_clear_fence(&mut tx, channel_id)
        .await
        .map_err(|error| anyhow!("lock channel clear boundary failed: {error}"))?;
    sqlx::query(
        "INSERT INTO channel_session_clear_boundaries (
             channel_id, cleared_at, cleared_through_id, clear_generation
         )
         SELECT $1, NOW(), COALESCE(MAX(id), 0), 1
           FROM session_transcripts
          WHERE channel_id = $1
         ON CONFLICT (channel_id) DO UPDATE SET
             cleared_at = GREATEST(
                 channel_session_clear_boundaries.cleared_at,
                 EXCLUDED.cleared_at
             ),
             cleared_through_id = GREATEST(
                 channel_session_clear_boundaries.cleared_through_id,
                 EXCLUDED.cleared_through_id
             ),
             clear_generation = channel_session_clear_boundaries.clear_generation + 1",
    )
    .bind(channel_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| anyhow!("record channel clear boundary failed: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit channel clear boundary failed: {error}"))?;

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
            clear_boundary.cleared_at,
            transcript.id,
            clear_boundary.cleared_through_id
     FROM session_transcripts AS transcript
     LEFT JOIN channel_session_clear_boundaries AS clear_boundary
       ON clear_boundary.channel_id = transcript.channel_id
     WHERE transcript.channel_id = $1
       AND transcript.id <= $2
       AND BTRIM(transcript.user_message) <> ''
       AND BTRIM(transcript.assistant_message) <> ''
       AND (clear_boundary.cleared_through_id IS NULL
            OR transcript.id > clear_boundary.cleared_through_id)
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

// #5707: two independent conditions, both required.
//
// `created_at > cleared_at` is the legacy timestamp axis and stays. It cannot
// see a clear that froze `NOW()` before a transcript committed but only took
// the channel lock afterwards, because both values are transaction-begin times
// and neither follows the lock order.
//
// SQL applies `id > cleared_through_id` before LIMIT: the clear reads
// `MAX(session_transcripts.id)` while holding the lock, so any row it was
// serialized after is at or below the recorded frontier. Rows written before
// migration 0114 compare against the `0` default, which every id exceeds.
fn channel_pairs_after_clear_boundary(
    rows: Vec<ChannelTranscriptPairRow>,
) -> Vec<ChannelTranscriptPair> {
    rows.into_iter()
        .filter(|(_, _, created_at, cleared_at, ..)| match cleared_at {
            None => true,
            Some(cleared_at) => created_at.is_some_and(|created_at| created_at > *cleared_at),
        })
        .map(
            |(user_message, assistant_message, ..)| ChannelTranscriptPair {
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

/// Fixed first observation; -1 permanently represents failed observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChannelClearFence(i64);

async fn channel_clear_fence_tx<'a>(
    pool: &'a PgPool,
    channel_id: &str,
) -> Result<(Transaction<'a, Postgres>, ChannelClearFence)> {
    let mut tx = pool.begin().await?;
    lock_channel_transcript_clear_fence(&mut tx, channel_id).await?;
    let generation = sqlx::query_scalar::<_, i64>(
        "SELECT clear_generation FROM channel_session_clear_boundaries WHERE channel_id = $1",
    )
    .bind(channel_id)
    .fetch_optional(&mut *tx)
    .await?;
    Ok((tx, ChannelClearFence(generation.unwrap_or(0))))
}

pub(crate) async fn capture_channel_clear_fence(
    pool: Option<&PgPool>,
    channel_id: &str,
) -> ChannelClearFence {
    if let Some(pool) = pool {
        if let Ok((tx, fence)) = channel_clear_fence_tx(pool, channel_id).await {
            if tx.commit().await.is_ok() {
                return fence;
            }
        }
    }
    ChannelClearFence(-1)
}

pub(crate) async fn routine_attempt_owns_turn_pg(
    pool: Option<&PgPool>,
    turn_id: &str,
    channel_id: &str,
) -> bool {
    let Some(pool) = pool else { return false };
    sqlx::query("SELECT 1 FROM routine_runs WHERE turn_id = $1 AND result_json->>'channel_id' = $2 AND result_json->>'discord_thread_id' = $2 LIMIT 2")
        .bind(turn_id)
        .bind(channel_id)
        .fetch_all(pool)
        .await
        .is_ok_and(|rows| rows.len() == 1)
}

pub async fn persist_turn_db(
    pg_pool: Option<&PgPool>,
    entry: PersistSessionTranscript<'_>,
) -> Result<bool> {
    persist_turn_db_with_clear_fence(pg_pool, entry, None).await
}

pub(crate) async fn persist_turn_db_with_clear_fence(
    pg_pool: Option<&PgPool>,
    entry: PersistSessionTranscript<'_>,
    fence: Option<ChannelClearFence>,
) -> Result<bool> {
    let Some(pool) = pg_pool else {
        return Err(anyhow!("postgres pool is required to persist transcript"));
    };

    let prepared = prepare_persist_entry_pg(pool, &entry).await?;
    let Some(prepared) = prepared else {
        return Ok(false);
    };

    // Required proof never falls back to the legacy timestamp/fail-open path.
    if let Some(captured) = fence {
        let Some(channel_id) = prepared.channel_id.as_deref() else {
            return Ok(false);
        };
        let Ok((mut tx, observed)) = channel_clear_fence_tx(pool, channel_id).await else {
            return Ok(false);
        };
        if captured != observed {
            return Ok(false);
        }
        persist_turn_pg_on(&mut *tx, &prepared).await?;
        tx.commit().await?;
        return Ok(true);
    }

    let Some(channel_id) = prepared.channel_id.as_deref() else {
        persist_turn_pg(pool, &prepared).await?;
        return Ok(true);
    };
    let Some(turn_started_at) = entry
        .turn_started_at_millis
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
    else {
        persist_turn_pg(pool, &prepared).await?;
        return Ok(true);
    };

    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!(
                channel_id,
                error = %error,
                "transcript clear-fence transaction unavailable; preserving legacy persistence"
            );
            persist_turn_pg(pool, &prepared).await?;
            return Ok(true);
        }
    };
    if let Err(error) = lock_channel_transcript_clear_fence(&mut tx, channel_id).await {
        tracing::warn!(
            channel_id,
            error = %error,
            "transcript clear-fence lock unavailable; preserving legacy persistence"
        );
        drop(tx);
        persist_turn_pg(pool, &prepared).await?;
        return Ok(true);
    }

    let boundary = sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "SELECT cleared_at FROM channel_session_clear_boundaries WHERE channel_id = $1",
    )
    .bind(channel_id)
    .fetch_optional(&mut *tx)
    .await;
    let should_skip = match boundary {
        Ok(Some(Some(cleared_at))) => turn_started_at <= cleared_at,
        Ok(_) => false,
        Err(error) => {
            tracing::warn!(
                channel_id,
                error = %error,
                "transcript clear-boundary lookup unavailable; preserving legacy persistence"
            );
            tx.rollback().await.map_err(|rollback_error| {
                anyhow!(
                    "rollback failed after clear-boundary lookup error ({error}): {rollback_error}"
                )
            })?;
            persist_turn_pg(pool, &prepared).await?;
            return Ok(true);
        }
    };
    if should_skip {
        tx.rollback().await.map_err(|error| {
            anyhow!("rollback pre-clear transcript transaction failed: {error}")
        })?;
        return Ok(false);
    }

    persist_turn_pg_on(&mut *tx, &prepared).await?;
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit postgres transcript failed: {error}"))?;
    Ok(true)
}

async fn lock_channel_transcript_clear_fence(
    tx: &mut Transaction<'_, Postgres>,
    channel_id: &str,
) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(4533, hashtext($1))")
        .bind(channel_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| anyhow!("channel transcript clear fence lock failed: {error}"))?;
    Ok(())
}

async fn persist_turn_pg(pool: &PgPool, entry: &PreparedSessionTranscript) -> Result<()> {
    persist_turn_pg_on(pool, entry).await
}

async fn persist_turn_pg_on<'e, E>(executor: E, entry: &PreparedSessionTranscript) -> Result<()>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
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
    .execute(executor)
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
    fn discord_message_start_millis_uses_serenity_snowflake_timestamp() {
        let message_id = MessageId::new(1);

        assert_eq!(
            discord_message_started_at_millis(Some(message_id)),
            Some(message_id.created_at().timestamp_millis())
        );
        assert_eq!(discord_message_started_at_millis(None), None);
    }

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
                7,
                Some(5),
            ),
            (
                "at-boundary".to_string(),
                "blocked".to_string(),
                Some(cleared_at),
                Some(cleared_at),
                4,
                Some(5),
            ),
            (
                "pre-clear".to_string(),
                "blocked".to_string(),
                Some(before),
                Some(cleared_at),
                3,
                Some(5),
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
            "a later fresh session must not cross the persisted timestamp boundary"
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
                2,
                Some(0),
            ),
            (
                "before-goal-fresh".to_string(),
                "blocked".to_string(),
                Some(cleared_at - chrono::Duration::seconds(1)),
                Some(cleared_at),
                1,
                Some(0),
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

// Test-only PostgreSQL setup and assertions intentionally fail fast.
#[cfg(test)]
#[allow(clippy::expect_used)]
mod clear_fence_pg_tests {
    use super::*;
    use crate::services::routines::{RoutineAgentExecutor, RoutineStore};
    use std::sync::Arc;

    async fn create_pool() -> (
        crate::dispatch::test_support::DispatchPostgresTestDb,
        PgPool,
    ) {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_transcript_clear_fence_4533",
            "session transcript clear fence",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        (db, pool)
    }

    async fn sa2_routine_store(pool: &PgPool) -> RoutineStore {
        sqlx::query(
            "INSERT INTO routines (id, script_ref, name) VALUES ('routine', 'fixture', 'fixture')",
        )
        .execute(pool)
        .await
        .expect("routine");
        sqlx::query("INSERT INTO routine_runs (id, routine_id) VALUES ('run', 'routine'), ('duplicate', 'routine')").execute(pool).await.expect("runs");
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 1024)
    }

    async fn sa2_mark(store: &RoutineStore, run: &str, channel: &str, thread: &str) {
        let proof = serde_json::json!({"channel_id": channel, "discord_thread_id": thread});
        assert!(
            store
                .mark_agent_turn_started(run, "discord:200:9001", Some(proof), "fixture", "fresh")
                .await
                .expect("mark attempt")
        );
    }

    #[tokio::test]
    async fn sa2_own_clear_and_midturn_clear_pg() {
        for response in ["오늘 브리핑입니다", "NO_REPLY"] {
            let (db, pool) = create_pool().await;
            let store = sa2_routine_store(&pool).await;
            let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), None, 1800);
            sa2_mark(&store, "run", "200", "200").await;
            record_channel_clear_boundary(Some(&pool), "200")
                .await
                .expect("own clear");
            let fence = capture_channel_clear_fence(Some(&pool), "200").await;
            let mut item = entry("discord:200:9001", "200", 0); // Synthetic snowflake predates own clear.
            item.assistant_message = response;
            assert!(
                persist_turn_db_with_clear_fence(Some(&pool), item, Some(fence))
                    .await
                    .expect("own insert")
            );
            assert_eq!(channel_pairs(&pool, "200").await.len(), 1);
            let outcomes = executor
                .poll_agent_runs(&store, 10, false)
                .await
                .expect("completion poll");
            assert_eq!(outcomes.len(), 1);
            assert_eq!(outcomes[0].status, "succeeded");
            assert_eq!(outcomes[0].run_id, "run");
            record_channel_clear_boundary(Some(&pool), "200")
                .await
                .expect("midturn clear");
            assert!(
                !persist_turn_db_with_clear_fence(
                    Some(&pool),
                    entry("stale", "200", i64::MAX),
                    Some(fence)
                )
                .await
                .expect("stale insert")
            );
            assert!(channel_pairs(&pool, "200").await.is_empty());
            pool.close().await;
            db.drop().await;
        }
    }

    #[tokio::test]
    async fn sa2_failed_observation_survives_recovery_pg() {
        let (db, pool) = create_pool().await;
        let captured = capture_channel_clear_fence(Some(&pool), "200").await;
        sqlx::query("ALTER TABLE channel_session_clear_boundaries RENAME TO hidden_boundary")
            .execute(&pool)
            .await
            .expect("hide");
        let failed = capture_channel_clear_fence(Some(&pool), "200").await;
        assert!(
            !persist_turn_db_with_clear_fence(
                Some(&pool),
                entry("unobservable", "200", 0),
                Some(captured)
            )
            .await
            .expect("closed")
        );
        assert!(
            persist_turn_db(Some(&pool), entry("legacy", "200", 0))
                .await
                .expect("legacy fail open")
        );
        sqlx::query("ALTER TABLE hidden_boundary RENAME TO channel_session_clear_boundaries")
            .execute(&pool)
            .await
            .expect("restore");
        assert!(
            !persist_turn_db_with_clear_fence(
                Some(&pool),
                entry("recovered", "200", 0),
                Some(failed)
            )
            .await
            .expect("permanent sentinel")
        );
        assert_eq!(channel_pairs(&pool, "200").await.len(), 1);
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn sa2_exact_routine_attempt_proof_pg() {
        let (db, pool) = create_pool().await;
        let store = sa2_routine_store(&pool).await;
        for (channel, thread, expected) in [
            ("200", "200", true),
            ("parent", "200", false),
            ("200", "sibling", false),
        ] {
            sa2_mark(&store, "run", channel, thread).await;
            assert_eq!(
                routine_attempt_owns_turn_pg(Some(&pool), "discord:200:9001", "200").await,
                expected
            );
        }
        assert!(!routine_attempt_owns_turn_pg(Some(&pool), "foreign", "200").await);
        assert!(!routine_attempt_owns_turn_pg(None, "discord:200:9001", "200").await);
        for run in ["run", "duplicate"] {
            sa2_mark(&store, run, "200", "200").await;
        }
        assert!(!routine_attempt_owns_turn_pg(Some(&pool), "discord:200:9001", "200").await);
        sqlx::query("ALTER TABLE routine_runs RENAME TO hidden_runs")
            .execute(&pool)
            .await
            .expect("hide proof");
        assert!(!routine_attempt_owns_turn_pg(Some(&pool), "discord:200:9001", "200").await);
        pool.close().await;
        db.drop().await;
    }

    fn entry<'a>(
        turn_id: &'a str,
        channel_id: &'a str,
        turn_started_at_millis: i64,
    ) -> PersistSessionTranscript<'a> {
        PersistSessionTranscript {
            turn_id,
            session_key: Some("clear-fence-session"),
            channel_id: Some(channel_id),
            agent_id: None,
            provider: Some("claude"),
            dispatch_id: None,
            user_message: "secret before clear",
            assistant_message: "private answer",
            events: &[],
            duration_ms: None,
            turn_started_at_millis: Some(turn_started_at_millis),
        }
    }

    fn bridge_owned_entry<'a>(
        turn_id: &'a str,
        channel_id: &'a str,
        user_msg_id: MessageId,
    ) -> PersistSessionTranscript<'a> {
        let mut entry = entry(turn_id, channel_id, 0);
        entry.turn_started_at_millis = discord_message_started_at_millis(Some(user_msg_id));
        entry
    }

    async fn channel_pairs(pool: &PgPool, channel_id: &str) -> Vec<ChannelTranscriptPair> {
        fetch_recent_channel_pairs(pool, channel_id, 10)
            .await
            .expect("fetch recent pairs") // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    }

    /// `(clear_generation, cleared_through_id)` for a channel's boundary row.
    async fn boundary_markers(pool: &PgPool, channel_id: &str) -> (i64, i64) {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT clear_generation, cleared_through_id
               FROM channel_session_clear_boundaries
              WHERE channel_id = $1",
        )
        .bind(channel_id)
        .fetch_one(pool)
        .await
        .expect("read clear boundary markers") // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_clear_turn_committed_after_boundary_is_not_reinjected_pg() {
        let (db, pool) = create_pool().await;
        let channel_id = "4533001";
        let turn_started_at = chrono::Utc::now() - chrono::Duration::minutes(1);
        record_channel_clear_boundary(Some(&pool), channel_id)
            .await
            .expect("record clear boundary"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        let stored = persist_turn_db(
            Some(&pool),
            entry("pre-clear", channel_id, turn_started_at.timestamp_millis()),
        )
        .await
        .expect("persist pre-clear transcript"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        assert!(
            !stored,
            "pre-clear turn must be rejected after the boundary commits"
        );
        assert!(
            channel_pairs(&pool, channel_id).await.is_empty(),
            "recent-pairs injection must not observe the rejected pre-clear turn"
        );
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_owned_pre_clear_turn_is_blocked_and_post_clear_turn_is_reinjected_pg() {
        let (db, pool) = create_pool().await;
        let channel_id = "4533002";
        let pre_clear_user_msg_id = MessageId::new(1);
        record_channel_clear_boundary(Some(&pool), channel_id)
            .await
            .expect("record clear boundary"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        let post_clear_user_msg_id = MessageId::new(u64::MAX);

        let pre_clear_stored = persist_turn_db(
            Some(&pool),
            bridge_owned_entry("bridge-pre-clear", channel_id, pre_clear_user_msg_id),
        )
        .await
        .expect("persist bridge-owned pre-clear transcript"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        let post_clear_stored = persist_turn_db(
            Some(&pool),
            bridge_owned_entry("bridge-post-clear", channel_id, post_clear_user_msg_id),
        )
        .await
        .expect("persist bridge-owned post-clear transcript"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        assert!(
            !pre_clear_stored,
            "bridge-owned turns anchored before /clear must be rejected"
        );
        assert!(
            post_clear_stored,
            "bridge-owned turns anchored after /clear must persist normally"
        );
        let pairs = channel_pairs(&pool, channel_id).await;
        assert_eq!(
            pairs.len(),
            1,
            "only the post-clear bridge turn is injectable"
        );
        assert_eq!(pairs[0].user_message, "secret before clear");
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boundary_lookup_failure_preserves_legacy_persist_pg() {
        let (db, pool) = create_pool().await;
        let channel_id = "4533003";
        sqlx::query(
            "ALTER TABLE channel_session_clear_boundaries RENAME TO channel_session_clear_boundaries_unavailable",
        )
        .execute(&pool)
        .await
        .expect("hide boundary table"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        let stored = persist_turn_db(
            Some(&pool),
            entry(
                "lookup-failure",
                channel_id,
                chrono::Utc::now().timestamp_millis(),
            ),
        )
        .await
        .expect("fall back to legacy transcript persistence"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        assert!(
            stored,
            "boundary lookup failure must preserve existing behavior"
        );
        let row_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM session_transcripts WHERE channel_id = $1",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .expect("count persisted transcript"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        assert_eq!(row_count, 1);
        pool.close().await;
        db.drop().await;
    }

    /// #5707 (F3): the reversal `created_at > cleared_at` cannot see.
    ///
    /// The clear freezes its `NOW()` first, a transcript commits on another
    /// connection, and only then does the clear take the channel lock. The
    /// first assertion pins that the timestamp axis alone would expose the row;
    /// the second pins that the frontier marker covers it anyway.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_that_locks_after_an_earlier_begin_still_hides_the_committed_pair_pg() {
        let (db, pool) = create_pool().await;
        let channel_id = "4533004";

        // Seed a boundary so the clear under test takes the ON CONFLICT branch.
        record_channel_clear_boundary(Some(&pool), channel_id)
            .await
            .expect("seed clear boundary"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        let mut clear_tx = begin_channel_clear_boundary_tx(&pool)
            .await
            .expect("begin clear boundary transaction"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        let clear_begin_at = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>("SELECT NOW()")
            .fetch_one(&mut *clear_tx)
            .await
            .expect("read the clear transaction timestamp"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        // Barrier: a turn that started after the seeded boundary commits while
        // the clear is parked between its two phases.
        let stored = persist_turn_db(
            Some(&pool),
            entry(
                "reversed-clear",
                channel_id,
                (clear_begin_at + chrono::Duration::seconds(1)).timestamp_millis(),
            ),
        )
        .await
        .expect("persist a transcript between the two clear phases"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        assert!(
            stored,
            "the transcript must commit before the clear takes the channel lock"
        );

        finish_channel_clear_boundary_tx(clear_tx, channel_id)
            .await
            .expect("finish clear boundary transaction"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module

        let created_after_cleared = sqlx::query_scalar::<_, bool>(
            "SELECT transcript.created_at > clear_boundary.cleared_at
               FROM session_transcripts AS transcript
               JOIN channel_session_clear_boundaries AS clear_boundary
                 ON clear_boundary.channel_id = transcript.channel_id
              WHERE transcript.channel_id = $1",
        )
        .bind(channel_id)
        .fetch_one(&pool)
        .await
        .expect("compare the transcript and boundary timestamps"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        assert!(
            created_after_cleared,
            "this schedule is only a regression while created_at > cleared_at"
        );
        assert!(
            channel_pairs(&pool, channel_id).await.is_empty(),
            "a clear that locks after the transcript committed must still cover it"
        );
        pool.close().await;
        db.drop().await;
    }

    /// #5707 (F6): both markers move on every boundary write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boundary_markers_advance_on_every_clear_pg() {
        let (db, pool) = create_pool().await;
        let channel_id = "4533005";
        let mut observed: Vec<(i64, i64)> = Vec::new();

        for round in 0..3 {
            record_channel_clear_boundary(Some(&pool), channel_id)
                .await
                .expect("record clear boundary"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
            observed.push(boundary_markers(&pool, channel_id).await);

            let turn_id = format!("marker-round-{round}");
            let stored = persist_turn_db(
                Some(&pool),
                entry(
                    &turn_id,
                    channel_id,
                    (chrono::Utc::now() + chrono::Duration::minutes(1)).timestamp_millis(),
                ),
            )
            .await
            .expect("persist a transcript between two clears"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
            assert!(
                stored,
                "each round must add the transcript row that the next clear has to cover"
            );
        }

        let generations: Vec<i64> = observed.iter().map(|(generation, _)| *generation).collect();
        assert_eq!(
            generations,
            vec![1, 2, 3],
            "clear_generation must advance exactly once per boundary write"
        );
        assert_eq!(
            observed[0].1, 0,
            "the first clear has no transcript row to cover"
        );
        assert!(
            observed[1].1 > observed[0].1 && observed[2].1 > observed[1].1,
            "cleared_through_id must advance as transcripts commit between clears, got {observed:?}"
        );
        assert_eq!(channel_pairs(&pool, channel_id).await.len(), 1);
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_frontier_filters_before_limit_preserving_later_commit_pg() -> Result<()> {
        let (db, pool) = create_pool().await;
        let channel_id = "4533006";
        let clear_tx = begin_channel_clear_boundary_tx(&pool).await?;
        let mut writer_b = pool.begin().await?;
        let b_started = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>("SELECT NOW()")
            .fetch_one(&mut *writer_b)
            .await?;
        let mut a = entry("limit-A", channel_id, b_started.timestamp_millis());
        a.user_message = "A must be excluded";
        assert!(persist_turn_db(Some(&pool), a).await?);
        finish_channel_clear_boundary_tx(clear_tx, channel_id).await?;
        let (_, covered_id) = boundary_markers(&pool, channel_id).await;
        assert!(covered_id > 0);
        // B resumes the production lock + INSERT path only after C commits.
        lock_channel_transcript_clear_fence(&mut writer_b, channel_id).await?;
        let mut b = entry("limit-B", channel_id, b_started.timestamp_millis());
        b.user_message = "B must survive";
        let prepared = prepare_persist_entry_pg(&pool, &b)
            .await?
            .ok_or_else(|| anyhow!("B must prepare"))?;
        persist_turn_pg_on(&mut *writer_b, &prepared).await?;
        writer_b.commit().await?;

        let rows = sqlx::query_as::<_, ChannelTranscriptPairRow>(
            "SELECT t.user_message, t.assistant_message, t.created_at, b.cleared_at,
                    t.id, b.cleared_through_id FROM session_transcripts t
             JOIN channel_session_clear_boundaries b ON b.channel_id = t.channel_id
             WHERE t.channel_id = $1 ORDER BY t.created_at DESC, t.id DESC",
        )
        .bind(channel_id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].4, covered_id); // A equals the excluded frontier.
        assert!(rows[1].4 > covered_id);
        assert!(rows[0].2 > rows[1].2 && rows[1].2 > rows[1].3);
        let mut tx = pool.begin().await?;
        let recent = fetch_recent_channel_pairs(&pool, channel_id, 1).await?;
        let bounded =
            fetch_channel_pairs_up_to_frontier_tx(&mut tx, channel_id, rows[1].4, 1).await?;
        let expected = vec![ChannelTranscriptPair {
            user_message: "B must survive".to_string(),
            assistant_message: "private answer".to_string(),
        }];
        assert_eq!((recent, bounded), (expected.clone(), expected));
        tx.commit().await?;
        pool.close().await;
        db.drop().await;
        Ok(())
    }
}
