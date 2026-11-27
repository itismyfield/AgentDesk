use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use sqlx::{PgPool, Postgres, Transaction};

use super::{
    ChannelTranscriptPair, chronological_channel_pairs_from_desc,
    lock_channel_transcript_clear_fence,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecentContextBoundaryKind {
    Cleared,
    ContextSevered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PreviousProviderSession {
    pub(crate) provider: String,
    pub(crate) session_id: String,
    pub(crate) path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecentContextBoundary {
    pub(crate) kind: RecentContextBoundaryKind,
    pub(crate) previous_session: Option<PreviousProviderSession>,
}

pub(crate) async fn record_recent_context_boundary(
    pool: Option<&PgPool>,
    channel_id: &str,
    context: &RecentContextBoundary,
) -> Result<()> {
    record_boundary(pool, channel_id, Some(context)).await
}

#[cfg(test)]
pub(crate) async fn record_channel_clear_boundary(
    pool: Option<&PgPool>,
    channel_id: &str,
) -> Result<()> {
    record_boundary(pool, channel_id, None).await
}

async fn record_boundary(
    pool: Option<&PgPool>,
    channel_id: &str,
    context: Option<&RecentContextBoundary>,
) -> Result<()> {
    let pool = pool
        .ok_or_else(|| anyhow!("postgres pool is required to persist a channel clear boundary"))?;
    let tx = begin_channel_clear_boundary_tx(pool).await?;
    finish_boundary(tx, channel_id, context).await
}

pub(crate) async fn begin_channel_clear_boundary_tx(
    pool: &PgPool,
) -> Result<Transaction<'_, Postgres>> {
    pool.begin()
        .await
        .map_err(|error| anyhow!("begin channel clear boundary transaction failed: {error}"))
}

#[cfg(test)]
pub(super) async fn finish_channel_clear_boundary_tx(
    tx: Transaction<'_, Postgres>,
    channel_id: &str,
) -> Result<()> {
    finish_boundary(tx, channel_id, None).await
}

async fn finish_boundary(
    mut tx: Transaction<'_, Postgres>,
    channel_id: &str,
    context: Option<&RecentContextBoundary>,
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
             channel_id, cleared_at, cleared_through_id, clear_generation, recent_context
         )
         SELECT $1, NOW(), COALESCE(MAX(id), 0), 1, $2
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
             clear_generation = channel_session_clear_boundaries.clear_generation + 1,
             recent_context = EXCLUDED.recent_context",
    )
    .bind(channel_id)
    .bind(context.map(Json))
    .execute(&mut *tx)
    .await
    .map_err(|error| anyhow!("record channel clear boundary failed: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit channel clear boundary failed: {error}"))?;
    Ok(())
}

// Both halves of the read use the same committed boundary generation and transcript snapshot.
pub(super) const FETCH_RECENT_CHANNEL_PAIRS_SQL: &str = "
    WITH scope AS (
        SELECT clear_boundary.cleared_at, clear_boundary.cleared_through_id,
               CASE WHEN $3 AND clear_boundary.recent_context IS NOT NULL AND NOT EXISTS (
                   SELECT 1 FROM session_transcripts AS later
                   WHERE later.channel_id = $1 AND later.channel_context
                     AND later.id > clear_boundary.cleared_through_id
                     AND later.created_at > clear_boundary.cleared_at
               ) THEN clear_boundary.recent_context END AS recent_context
        FROM (VALUES ($1::text)) AS channel(id)
        LEFT JOIN channel_session_clear_boundaries AS clear_boundary
          ON clear_boundary.channel_id = channel.id
    )
    SELECT scope.recent_context, pair.user_message, pair.assistant_message
    FROM scope LEFT JOIN LATERAL (
        SELECT transcript.user_message, transcript.assistant_message,
               transcript.created_at, transcript.id
        FROM session_transcripts AS transcript
        WHERE transcript.channel_id = $1 AND transcript.channel_context
          AND BTRIM(transcript.user_message) <> ''
          AND BTRIM(transcript.assistant_message) <> ''
          AND CASE WHEN scope.recent_context IS NOT NULL
              THEN transcript.id <= scope.cleared_through_id
              ELSE (scope.cleared_through_id IS NULL OR transcript.id > scope.cleared_through_id)
                AND (scope.cleared_at IS NULL OR transcript.created_at > scope.cleared_at)
              END
        ORDER BY transcript.created_at DESC, transcript.id DESC
        LIMIT $2
    ) AS pair ON TRUE
    ORDER BY pair.created_at DESC, pair.id DESC";

pub(crate) async fn fetch_recent_channel_prompt_context(
    pool: &PgPool,
    channel_id: &str,
    limit: u64,
) -> Result<(Vec<ChannelTranscriptPair>, Option<RecentContextBoundary>)> {
    fetch_recent_channel_context(pool, channel_id, limit, true).await
}

pub(super) async fn fetch_recent_channel_context(
    pool: &PgPool,
    channel_id: &str,
    limit: u64,
    include_boundary: bool,
) -> Result<(Vec<ChannelTranscriptPair>, Option<RecentContextBoundary>)> {
    type Row = (
        Option<Json<RecentContextBoundary>>,
        Option<String>,
        Option<String>,
    );
    let rows = sqlx::query_as::<_, Row>(FETCH_RECENT_CHANNEL_PAIRS_SQL)
        .bind(channel_id)
        .bind(limit.min(i64::MAX as u64) as i64)
        .bind(include_boundary)
        .fetch_all(pool)
        .await
        .map_err(|error| anyhow!("recent channel transcript lookup failed: {error}"))?;
    let boundary = rows
        .first()
        .and_then(|row| row.0.as_ref())
        .map(|value| value.0.clone());
    let pairs = rows
        .into_iter()
        .filter_map(|(_, user_message, assistant_message)| {
            Some(ChannelTranscriptPair {
                user_message: user_message?,
                assistant_message: assistant_message?,
            })
        })
        .collect();
    Ok((chronological_channel_pairs_from_desc(pairs), boundary))
}
