use anyhow::{Context as AnyhowContext, Result};
use sqlx::{PgPool, Row};

use super::formatting::non_negative_u64;
use super::model::{LatestTurn, LifecycleEventRow};
use crate::db::prompt_manifests::{PromptManifest, fetch_prompt_manifest};

pub(super) async fn load_latest_turn(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<LatestTurn>> {
    let row = sqlx::query(
        "SELECT turn_id,
                channel_id,
                provider,
                session_key,
                session_id,
                dispatch_id,
                finished_at,
                duration_ms::BIGINT AS duration_ms,
                input_tokens::BIGINT AS input_tokens,
                cache_create_tokens::BIGINT AS cache_create_tokens,
                cache_read_tokens::BIGINT AS cache_read_tokens
         FROM turns
         WHERE channel_id = $1
         ORDER BY finished_at DESC, started_at DESC, created_at DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("load latest turn")?;

    row.map(|row| {
        Ok(LatestTurn {
            turn_id: row.try_get("turn_id")?,
            channel_id: row.try_get("channel_id")?,
            provider: row.try_get("provider")?,
            session_key: row.try_get("session_key")?,
            session_id: row.try_get("session_id")?,
            dispatch_id: row.try_get("dispatch_id")?,
            finished_at: row.try_get("finished_at")?,
            duration_ms: row.try_get("duration_ms")?,
            input_tokens: non_negative_u64(row.try_get::<i64, _>("input_tokens")?),
            cache_create_tokens: non_negative_u64(row.try_get::<i64, _>("cache_create_tokens")?),
            cache_read_tokens: non_negative_u64(row.try_get::<i64, _>("cache_read_tokens")?),
        })
    })
    .transpose()
}

pub(super) async fn load_latest_prompt_manifest(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<PromptManifest>> {
    let turn_id = sqlx::query_scalar::<_, String>(
        "SELECT turn_id
         FROM prompt_manifests
         WHERE channel_id = $1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("load latest prompt manifest turn_id")?;

    let Some(turn_id) = turn_id else {
        return Ok(None);
    };
    fetch_prompt_manifest(Some(pool), &turn_id).await
}

pub(super) async fn load_latest_session_event(
    pool: &PgPool,
    channel_id: &str,
    turn_id: Option<&str>,
) -> Result<Option<LifecycleEventRow>> {
    let row = sqlx::query(
        "SELECT kind, severity, summary, details_json, created_at
         FROM turn_lifecycle_events
         WHERE channel_id = $1
           AND ($2::TEXT IS NULL OR turn_id = $2)
           AND kind IN (
               'session_fresh',
               'session_resumed',
               'session_resume_failed_with_recovery'
           )
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .context("load latest session lifecycle event")?;

    row.map(decode_lifecycle_event).transpose()
}

pub(super) async fn load_latest_compaction_event(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<LifecycleEventRow>> {
    let row = sqlx::query(
        "SELECT kind, severity, summary, details_json, created_at
         FROM turn_lifecycle_events
         WHERE channel_id = $1
           AND kind = 'context_compacted'
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("load latest context compaction event")?;

    row.map(decode_lifecycle_event).transpose()
}

pub(super) async fn load_lifecycle_events(
    pool: &PgPool,
    channel_id: &str,
    turn_id: &str,
    limit: i64,
) -> Result<Vec<LifecycleEventRow>> {
    let rows = sqlx::query(
        "SELECT kind, severity, summary, details_json, created_at
         FROM turn_lifecycle_events
         WHERE channel_id = $1
           AND turn_id = $2
         ORDER BY created_at DESC, id DESC
         LIMIT $3",
    )
    .bind(channel_id)
    .bind(turn_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("load lifecycle events")?;

    rows.into_iter().map(decode_lifecycle_event).collect()
}

fn decode_lifecycle_event(row: sqlx::postgres::PgRow) -> Result<LifecycleEventRow> {
    Ok(LifecycleEventRow {
        kind: row.try_get("kind")?,
        severity: row.try_get("severity")?,
        summary: row.try_get("summary")?,
        details_json: row.try_get("details_json")?,
        created_at: row.try_get("created_at")?,
    })
}
