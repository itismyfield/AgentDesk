//! Exact existing-card edit authority for retirement; POST receipts stay unchanged.

use super::super::{TaskCardPayload, content_hash};
use super::*;

#[derive(Clone)]
struct Snapshot {
    scope: TaskCardScope,
    message_id: u64,
    bot_key: String,
    revision: i32,
    hash: String,
    content: String,
    visible: String,
}

pub(in crate::services::discord::task_notification_delivery) async fn claim_retired_metadata(
    pool: Option<&PgPool>,
    scope: &TaskCardScope,
    payload: &TaskCardPayload,
    bot_key: &str,
    message_id: u64,
) -> Result<Option<(ClaimedCard, String)>, String> {
    match pool {
        Some(pool) => claim_pg(pool, scope, payload, bot_key, message_id).await,
        None if cfg!(any(test, debug_assertions)) => {
            claim_memory(scope, payload, bot_key, message_id)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

async fn claim_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    payload: &TaskCardPayload,
    bot_key: &str,
    message_id: u64,
) -> Result<Option<(ClaimedCard, String)>, String> {
    let row = sqlx::query(
        "SELECT session_key, event_key, terminal_delivery_fingerprint,
                revision, update_count, rendered_content, content_hash
         FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND delivery_state = 'card_posted'
           AND discord_message_id = $6 AND bot_key = $7
           AND ((session_key = $3 AND event_key = $4)
                OR ($5::VARCHAR IS NOT NULL AND terminal_delivery_fingerprint = $5))",
    )
    .bind(db_id(scope.channel_id, "channel_id")?)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(&scope.terminal_delivery_fingerprint)
    .bind(db_id(message_id, "message_id")?)
    .bind(bot_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load task-card metadata projection: {error}"))?;
    let Some(row) = row else { return Ok(None) };
    let content: String = row.get("rendered_content");
    let count =
        u64::try_from(row.get::<i64, _>("update_count")).map_err(|error| error.to_string())?;
    let Some(visible) = super::super::retired_metadata::without_metadata(&content, payload, count)
    else {
        return Ok(None);
    };
    let actual_scope = TaskCardScope {
        channel_id: scope.channel_id,
        provider: scope.provider.clone(),
        session_key: row.get("session_key"),
        event_key: row.get("event_key"),
        terminal_delivery_fingerprint: row.get("terminal_delivery_fingerprint"),
    };
    let hash: String = row.get("content_hash");
    if hash != content_hash(&content) {
        return Ok(None);
    }
    let revision: i32 = row.get("revision");
    let owner = uuid::Uuid::new_v4().to_string();
    let row = sqlx::query(
        "UPDATE task_notification_card_state
         SET lease_owner = $9, lease_expires_at = NOW() + make_interval(secs => $10),
             updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND delivery_state = 'card_posted' AND discord_message_id = $5
           AND bot_key = $6 AND revision = $7 AND content_hash = $8
           AND (lease_owner IS NULL OR lease_expires_at <= NOW())
         RETURNING bot_key, discord_nonce, revision, update_count, rendered_content, post_started_at",
    )
    .bind(db_id(actual_scope.channel_id, "channel_id")?)
    .bind(&actual_scope.provider)
    .bind(&actual_scope.session_key)
    .bind(&actual_scope.event_key)
    .bind(db_id(message_id, "message_id")?)
    .bind(bot_key)
    .bind(revision)
    .bind(&hash)
    .bind(&owner)
    .bind(LEASE_SECONDS)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("claim task-card metadata edit: {error}"))?;
    row.map(|row| {
        claimed_from_row(&actual_scope, owner, &row, ClaimAction::Edit { message_id })
            .map(|claim| (claim, visible))
    })
    .transpose()
}

fn claim_memory(
    scope: &TaskCardScope,
    payload: &TaskCardPayload,
    bot_key: &str,
    message_id: u64,
) -> Result<Option<(ClaimedCard, String)>, String> {
    let snapshot = {
        let rows = MEMORY_STORE
            .lock()
            .map_err(|_| "task card memory store poisoned".to_string())?;
        let Some((key, row)) = rows.iter().find(|(key, row)| {
            key.channel_id == scope.channel_id
                && key.provider == scope.provider
                && (**key == *scope
                    || (scope.terminal_delivery_fingerprint.is_some()
                        && row.terminal_delivery_fingerprint
                            == scope.terminal_delivery_fingerprint))
                && row.message_id == Some(message_id)
                && row.bot_key == bot_key
        }) else {
            return Ok(None);
        };
        let Some(visible) = super::super::retired_metadata::without_metadata(
            &row.rendered_content,
            payload,
            row.update_count,
        ) else {
            return Ok(None);
        };
        Snapshot {
            scope: key.clone(),
            message_id,
            bot_key: bot_key.to_string(),
            revision: row.revision,
            hash: row.content_hash.clone(),
            content: row.rendered_content.clone(),
            visible,
        }
    };
    claim_memory_snapshot(snapshot)
}

fn claim_memory_snapshot(snapshot: Snapshot) -> Result<Option<(ClaimedCard, String)>, String> {
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let Some(row) = rows.get_mut(&snapshot.scope) else {
        return Ok(None);
    };
    let active = row.lease_owner.is_some()
        && row
            .lease_expires_at
            .is_some_and(|until| until > Instant::now());
    if row.state != MemoryState::CardPosted
        || active
        || row.message_id != Some(snapshot.message_id)
        || row.bot_key != snapshot.bot_key
        || row.revision != snapshot.revision
        || row.content_hash != snapshot.hash
        || row.rendered_content != snapshot.content
        || snapshot.hash != content_hash(&snapshot.content)
    {
        return Ok(None);
    }
    let owner = uuid::Uuid::new_v4().to_string();
    row.lease_owner = Some(owner.clone());
    row.lease_expires_at = Some(Instant::now() + Duration::from_secs(LEASE_SECONDS as u64));
    row.last_error = None;
    Ok(Some((
        memory_claim(
            &snapshot.scope,
            &owner,
            row,
            ClaimAction::Edit {
                message_id: snapshot.message_id,
            },
        ),
        snapshot.visible,
    )))
}

#[cfg(test)]
#[path = "metadata_retirement_authority_tests.rs"]
mod tests;
