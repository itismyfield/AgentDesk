use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::db::session_agent_resolution::resolve_agent_id_for_session_pg;
use crate::db::timestamps::parse_timestamp;

#[cfg(all(test, unix))]
#[path = "turns/timestamp_tests.rs"]
mod timestamp_tests;

#[cfg(all(test, unix))]
#[path = "turns/receipt_tests.rs"]
mod receipt_tests;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTokenUsage {
    pub input_tokens: u64,
    pub cache_create_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
}

impl TurnTokenUsage {
    /// Tokens occupying the model context for the final request in the turn.
    ///
    /// Anthropic reports cache reads, cache writes, and uncached input as
    /// separate fields for pricing/rate-limit purposes. For context-window
    /// occupancy, cached prefixes still count as prompt tokens.
    pub fn context_occupancy_input_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_create_tokens)
            .saturating_add(self.cache_read_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::TurnTokenUsage;

    #[test]
    fn context_occupancy_counts_cached_and_uncached_input_tokens() {
        let no_cache = TurnTokenUsage {
            input_tokens: 850,
            cache_create_tokens: 0,
            cache_read_tokens: 0,
            output_tokens: 42,
        };
        assert_eq!(no_cache.context_occupancy_input_tokens(), 850);

        let cache_read = TurnTokenUsage {
            input_tokens: 50,
            cache_create_tokens: 0,
            cache_read_tokens: 200_000,
            output_tokens: 42,
        };
        assert_eq!(cache_read.context_occupancy_input_tokens(), 200_050);

        let cache_create = TurnTokenUsage {
            input_tokens: 21,
            cache_create_tokens: 188_086,
            cache_read_tokens: 0,
            output_tokens: 393,
        };
        assert_eq!(cache_create.context_occupancy_input_tokens(), 188_107);
    }

    #[test]
    fn context_occupancy_saturates_on_overflow() {
        let usage = TurnTokenUsage {
            input_tokens: u64::MAX,
            cache_create_tokens: 1,
            cache_read_tokens: 1,
            output_tokens: 0,
        };

        assert_eq!(usage.context_occupancy_input_tokens(), u64::MAX);
    }
}

#[derive(Debug, Clone)]
pub struct PersistTurnOwned {
    pub turn_id: String,
    pub session_key: Option<String>,
    pub thread_id: Option<String>,
    pub thread_title: Option<String>,
    pub channel_id: String,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub session_id: Option<String>,
    pub dispatch_id: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub token_usage: TurnTokenUsage,
}

pub async fn upsert_turn_owned_db(
    pg_pool: Option<&PgPool>,
    entry: &PersistTurnOwned,
) -> Result<()> {
    let pool = pg_pool.ok_or_else(|| anyhow!("postgres pool unavailable for turns upsert"))?;
    upsert_turn_owned_pg(pool, entry).await
}

async fn upsert_turn_owned_pg(pool: &PgPool, entry: &PersistTurnOwned) -> Result<()> {
    let turn_id = entry.turn_id.trim();
    if turn_id.is_empty() {
        return Err(anyhow!("turn_id is required"));
    }

    let channel_id = entry.channel_id.trim();
    if channel_id.is_empty() {
        return Err(anyhow!("channel_id is required"));
    }

    let session_key = normalized_opt(entry.session_key.as_deref());
    let thread_id = normalized_opt(entry.thread_id.as_deref());
    let thread_title = normalized_opt(entry.thread_title.as_deref());
    let provider = normalized_opt(entry.provider.as_deref());
    let session_id = normalized_opt(entry.session_id.as_deref());
    let dispatch_id = normalized_opt(entry.dispatch_id.as_deref());
    let started_at = normalized_opt(entry.started_at.as_deref())
        .map(|value| parse_timestamp(&value))
        .transpose()?
        .unwrap_or_else(chrono::Utc::now);
    let finished_at = normalized_opt(entry.finished_at.as_deref())
        .map(|value| parse_timestamp(&value))
        .transpose()?
        .unwrap_or_else(chrono::Utc::now);
    let agent_id = resolve_agent_id_for_session_pg(
        pool,
        entry.agent_id.as_deref(),
        session_key.as_deref(),
        None,
        thread_id.as_deref(),
        dispatch_id.as_deref(),
        Some(channel_id),
    )
    .await;

    sqlx::query(
        "INSERT INTO turns (
            turn_id,
            session_key,
            thread_id,
            thread_title,
            channel_id,
            agent_id,
            provider,
            session_id,
            dispatch_id,
            started_at,
            finished_at,
            duration_ms,
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
            output_tokens
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9,
            $10, $11,
            $12, $13, $14, $15, $16
         )
         ON CONFLICT(turn_id) DO UPDATE SET
            session_key = COALESCE(EXCLUDED.session_key, turns.session_key),
            thread_id = COALESCE(EXCLUDED.thread_id, turns.thread_id),
            thread_title = COALESCE(EXCLUDED.thread_title, turns.thread_title),
            channel_id = EXCLUDED.channel_id,
            agent_id = COALESCE(EXCLUDED.agent_id, turns.agent_id),
            provider = COALESCE(EXCLUDED.provider, turns.provider),
            session_id = COALESCE(EXCLUDED.session_id, turns.session_id),
            dispatch_id = COALESCE(EXCLUDED.dispatch_id, turns.dispatch_id),
            started_at = COALESCE(EXCLUDED.started_at, turns.started_at),
            finished_at = COALESCE(EXCLUDED.finished_at, turns.finished_at),
            duration_ms = COALESCE(EXCLUDED.duration_ms, turns.duration_ms),
            input_tokens = CASE
                WHEN EXCLUDED.input_tokens > 0 THEN EXCLUDED.input_tokens
                ELSE turns.input_tokens
            END,
            cache_create_tokens = CASE
                WHEN EXCLUDED.cache_create_tokens > 0 THEN EXCLUDED.cache_create_tokens
                ELSE turns.cache_create_tokens
            END,
            cache_read_tokens = CASE
                WHEN EXCLUDED.cache_read_tokens > 0 THEN EXCLUDED.cache_read_tokens
                ELSE turns.cache_read_tokens
            END,
            output_tokens = CASE
                WHEN EXCLUDED.output_tokens > 0 THEN EXCLUDED.output_tokens
                ELSE turns.output_tokens
            END",
    )
    .bind(turn_id)
    .bind(session_key)
    .bind(thread_id)
    .bind(thread_title)
    .bind(channel_id)
    .bind(agent_id)
    .bind(provider)
    .bind(session_id)
    .bind(dispatch_id)
    .bind(started_at)
    .bind(finished_at)
    .bind(entry.duration_ms)
    .bind(u64_to_i64(entry.token_usage.input_tokens))
    .bind(u64_to_i64(entry.token_usage.cache_create_tokens))
    .bind(u64_to_i64(entry.token_usage.cache_read_tokens))
    .bind(u64_to_i64(entry.token_usage.output_tokens))
    .execute(pool)
    .await
    .map_err(|e| anyhow!("persist postgres turn failed: {e}"))?;

    Ok(())
}

/// How an execution actually ended; a hand-off or retry that keeps the execution is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    Completed,
    Cancelled,
    Failed,
    Empty,
}

impl TerminalOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Empty => "empty",
        }
    }
}

/// The receipt an execution leaves on its `turns` row, independent of any answer text.
pub struct TurnReceipt<'a> {
    pub turn_id: &'a str,
    pub channel_id: &'a str,
    pub agent_id: Option<&'a str>,
    pub provider: &'a str,
    pub session_key: Option<&'a str>,
    pub started_at: Option<&'a str>,
    pub consumed_origin_ids: &'a [String],
    pub outcome: TerminalOutcome,
    pub confirmed_at: chrono::DateTime<chrono::Utc>,
}

/// Commits the receipt before returning. The first terminal wins and origins accumulate, so a
/// repeated or concurrent write for the same execution changes nothing it already recorded.
pub async fn record_turn_receipt(pool: &PgPool, receipt: &TurnReceipt<'_>) -> Result<()> {
    let turn_id = receipt.turn_id.trim();
    let channel_id = receipt.channel_id.trim();
    if turn_id.is_empty() || channel_id.is_empty() {
        return Err(anyhow!("turn receipt needs a turn_id and a channel_id"));
    }
    // A row needs a start instant; an unreadable one falls back to the terminal instant.
    let started_at = normalized_opt(receipt.started_at)
        .and_then(|value| parse_timestamp(&value).ok())
        .unwrap_or(receipt.confirmed_at);
    sqlx::query(
        "INSERT INTO turns (
            turn_id, session_key, channel_id, agent_id, provider, started_at, finished_at,
            consumed_origin_ids, terminal_outcome, terminal_confirmed_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $7)
         ON CONFLICT(turn_id) DO UPDATE SET
            consumed_origin_ids = ARRAY(
                SELECT DISTINCT origin
                FROM unnest(turns.consumed_origin_ids || EXCLUDED.consumed_origin_ids) AS origin
                ORDER BY origin
            ),
            terminal_outcome = COALESCE(turns.terminal_outcome, EXCLUDED.terminal_outcome),
            terminal_confirmed_at =
                COALESCE(turns.terminal_confirmed_at, EXCLUDED.terminal_confirmed_at)",
    )
    .bind(turn_id)
    .bind(normalized_opt(receipt.session_key))
    .bind(channel_id)
    .bind(normalized_opt(receipt.agent_id))
    .bind(receipt.provider)
    .bind(started_at)
    .bind(receipt.confirmed_at)
    .bind(receipt.consumed_origin_ids)
    .bind(receipt.outcome.as_str())
    .execute(pool)
    .await
    .map_err(|e| anyhow!("persist turn receipt failed: {e}"))?;
    Ok(())
}

/// A committed terminal receipt naming one origin.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OriginReceipt {
    pub turn_id: String,
    pub channel_id: String,
    pub terminal_outcome: String,
    pub terminal_confirmed_at: chrono::DateTime<chrono::Utc>,
}

/// The terminal receipt of the execution that consumed `origin_id` for this agent, whether the
/// row is still in `turns` or retention already moved it to `turns_archive`.
pub async fn find_origin_receipt(
    pool: &PgPool,
    origin_id: &str,
    agent_id: &str,
    channel_ids: &[String],
) -> Result<Option<OriginReceipt>> {
    sqlx::query_as(
        "SELECT turn_id, channel_id, terminal_outcome, terminal_confirmed_at FROM (
            SELECT turn_id, channel_id, agent_id, terminal_outcome, terminal_confirmed_at, 0 AS tier
            FROM turns WHERE consumed_origin_ids @> ARRAY[$1]::TEXT[]
            UNION ALL
            SELECT turn_id, channel_id, agent_id, terminal_outcome, terminal_confirmed_at, 1 AS tier
            FROM turns_archive WHERE consumed_origin_ids @> ARRAY[$1]::TEXT[]
         ) found
         WHERE terminal_outcome IS NOT NULL AND (agent_id = $2 OR channel_id = ANY($3))
         ORDER BY terminal_confirmed_at, tier
         LIMIT 1",
    )
    .bind(origin_id)
    .bind(agent_id)
    .bind(channel_ids)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow!("read turn receipt failed: {e}"))
}

fn normalized_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
