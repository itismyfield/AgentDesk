//! One user clear survives restart until its fresh provider start verifies and releases the token.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[cfg(unix)]
use super::HookSessionUpsert;
#[cfg(unix)]
use super::hosted_execution::{HostedExecution, HostedOwner};
#[cfg(unix)]
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
    upsert_hook_session_with_identity_pg,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeFence {
    pub session_row_id: i64,
    pub token: Uuid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    token: Uuid,
    set_at: chrono::DateTime<chrono::Utc>,
    cause: String,
}

#[cfg(unix)]
pub fn new_payload() -> Value {
    serde_json::json!({
        "token": Uuid::new_v4(),
        "set_at": chrono::Utc::now(),
        "cause": "user_clear",
    })
}

fn decode(session_row_id: i64, raw: Option<Value>) -> Result<Option<ResumeFence>, String> {
    raw.map(|raw| {
        let payload: Payload = serde_json::from_value(raw)
            .map_err(|error| format!("decode provider resume fence: {error}"))?;
        if payload.cause != "user_clear" {
            return Err("provider resume fence has an unknown cause".into());
        }
        Ok(ResumeFence {
            session_row_id,
            token: payload.token,
        })
    })
    .transpose()
}

#[cfg(unix)]
fn identity(owner: &HostedOwner) -> CanonicalSessionIdentity<'_> {
    CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &owner.discord_token_hash,
        channel_id: &owner.channel_id,
    }
}

#[cfg(unix)]
async fn owner_row(pool: &PgPool, owner: &HostedOwner) -> Result<Option<i64>, String> {
    resolve_session_row_pg(pool, None, Some(&owner.provider), Some(identity(owner)))
        .await
        .map(|row| row.map(|(id, _)| id))
        .map_err(|error| format!("resolve provider resume fence owner: {error:?}"))
}

/// The common clear commit also fences legacy or unreadable records without changing that record.
#[cfg(unix)]
pub async fn set_for_channel_pg(
    pool: &PgPool,
    provider: &str,
    identity: CanonicalSessionIdentity<'_>,
    session_key: &str,
) -> Result<ResumeFence, String> {
    if provider != "claude" {
        return Err("provider resume fence requires Claude".into());
    }
    if identity.kind != SessionIdentityKind::DiscordChannel {
        return Err("provider resume fence requires a Discord channel identity".into());
    }
    let resolve = || async move {
        resolve_session_row_pg(pool, Some(session_key), Some(provider), Some(identity))
            .await
            .map(|row| row.map(|(id, _)| id))
            .map_err(|error| format!("resolve provider resume fence channel: {error:?}"))
    };
    let session_row_id = match resolve().await? {
        Some(id) => id,
        None => {
            let params = HookSessionUpsert {
                session_key,
                instance_id: None,
                agent_id: None,
                provider,
                status: "idle",
                session_info: None,
                model: None,
                tokens: None,
                cwd: None,
                active_dispatch_id: None,
                thread_channel_id: None,
                channel_id: Some(identity.channel_id),
                claude_session_id: None,
                raw_provider_session_id: None,
                turn_start_nonce: None,
                dispatched_origin: false,
            };
            upsert_hook_session_with_identity_pg(pool, params, Some(identity))
                .await
                .map_err(|error| format!("create provider resume fence owner: {error:?}"))?;
            resolve()
                .await?
                .ok_or("provider resume fence owner missing after create")?
        }
    };
    let raw: Option<Value> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE id = $1")
            .bind(session_row_id)
            .fetch_one(pool)
            .await
            .map_err(|error| format!("observe provider resume fence execution: {error}"))?;
    let payload = new_payload();
    let updated = sqlx::query(
        "UPDATE sessions SET provider_resume_fence = $5
         WHERE id = $1 AND provider = $2
           AND ((identity_kind = 'discord_channel'
                 AND discord_token_hash = $3 AND channel_id = $4)
             OR (identity_kind IS NULL AND discord_token_hash IS NULL
                 AND (channel_id IS NULL OR channel_id = $4)
                 AND starts_with(session_key, $2 || '/' || $3 || '/')
                 AND starts_with($7, $2 || '/' || $3 || '/')
                 AND (session_key = $7 OR EXISTS (
                     SELECT 1 FROM session_key_aliases
                     WHERE session_key = $7 AND session_id = sessions.id))))
           AND hosted_execution IS NOT DISTINCT FROM $6",
    )
    .bind(session_row_id)
    .bind(provider)
    .bind(identity.discord_token_hash)
    .bind(identity.channel_id)
    .bind(&payload)
    .bind(raw)
    .bind(session_key)
    .execute(pool)
    .await
    .map_err(|error| format!("set provider resume fence: {error}"))?;
    if updated.rows_affected() != 1 {
        return Err("provider resume fence execution changed".into());
    }
    decode(session_row_id, Some(payload))?.ok_or("provider resume fence was absent".into())
}

/// A Held clear forgets only the exact execution observed, leaving any successor untouched.
#[cfg(unix)]
pub async fn set_for_execution_pg(
    pool: &PgPool,
    session_row_id: i64,
    record: &HostedExecution,
) -> Result<bool, String> {
    let raw = serde_json::to_value(record).map_err(|error| error.to_string())?;
    let owner = &record.owner;
    let updated = sqlx::query(
        "UPDATE sessions
         SET provider_resume_fence = CASE WHEN $2 = 'claude' THEN $6 ELSE provider_resume_fence END,
             claude_session_id = NULL,
             raw_provider_session_id = NULL, claude_session_id_recorded_at = NULL,
             raw_provider_transcript_len_watermark = 0,
             raw_provider_transcript_watermark_session_id = NULL,
             raw_provider_transcript_growth_proven = FALSE
         WHERE id = $1 AND provider = $2 AND identity_kind = 'discord_channel'
           AND discord_token_hash = $3 AND channel_id = $4 AND hosted_execution = $5",
    )
    .bind(session_row_id)
    .bind(&owner.provider)
    .bind(&owner.discord_token_hash)
    .bind(&owner.channel_id)
    .bind(raw)
    .bind(new_payload())
    .execute(pool)
    .await
    .map_err(|error| format!("set exact execution provider resume fence: {error}"))?;
    Ok(updated.rows_affected() == 1)
}

/// A launch reads once before selecting either a cached selector or the native binding journal.
#[cfg(unix)]
pub async fn load_for_owner_pg(
    pool: &PgPool,
    owner: &HostedOwner,
) -> Result<Option<ResumeFence>, String> {
    let session_row_id = owner_row(pool, owner)
        .await?
        .ok_or("provider resume fence owner missing")?;
    let raw = sqlx::query_scalar(
        "SELECT provider_resume_fence FROM sessions
         WHERE id = $1 AND provider = $2 AND identity_kind = 'discord_channel'
           AND discord_token_hash = $3 AND channel_id = $4",
    )
    .bind(session_row_id)
    .bind(&owner.provider)
    .bind(&owner.discord_token_hash)
    .bind(&owner.channel_id)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("load provider resume fence: {error}"))?;
    decode(session_row_id, raw)
}

/// The API already resolved aliases before loading its provider selectors.
pub async fn load_by_resolved_key_pg(
    pool: &PgPool,
    resolved_key: &str,
) -> Result<Option<ResumeFence>, String> {
    let row = sqlx::query("SELECT id, provider_resume_fence FROM sessions WHERE session_key = $1")
        .bind(resolved_key)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("load selector provider resume fence: {error}"))?;
    let id = row.try_get("id").map_err(|error| error.to_string())?;
    let raw = row
        .try_get("provider_resume_fence")
        .map_err(|error| error.to_string())?;
    decode(id, raw)
}

/// Only the fresh start that captured this token may release it; a later clear remains fenced.
#[cfg(unix)]
pub async fn release_pg(pool: &PgPool, captured: &ResumeFence) -> Result<bool, String> {
    let updated = sqlx::query(
        "UPDATE sessions SET provider_resume_fence = NULL
         WHERE id = $1 AND provider_resume_fence->>'token' = $2",
    )
    .bind(captured.session_row_id)
    .bind(captured.token.to_string())
    .execute(pool)
    .await
    .map_err(|error| format!("release provider resume fence: {error}"))?;
    Ok(updated.rows_affected() == 1)
}

#[cfg(all(test, unix))]
#[path = "provider_resume_fence/tests.rs"]
mod tests;
