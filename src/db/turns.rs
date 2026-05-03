use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::db::session_agent_resolution::resolve_agent_id_for_session_pg;

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
    let started_at = normalized_opt(entry.started_at.as_deref()).unwrap_or_else(now_string);
    let finished_at = normalized_opt(entry.finished_at.as_deref()).unwrap_or_else(now_string);
    let agent_id = resolve_agent_id_for_session_pg(
        pool,
        entry.agent_id.as_deref(),
        session_key.as_deref(),
        None,
        thread_id.as_deref(),
        dispatch_id.as_deref(),
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
            CAST($10 AS timestamptz),
            CAST($11 AS timestamptz),
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

fn normalized_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
