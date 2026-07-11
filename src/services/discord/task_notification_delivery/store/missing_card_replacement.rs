//! Exact-CAS recovery when a task-response reference card disappeared.

use super::*;

#[derive(Clone, Debug)]
pub(in crate::services::discord::task_notification_delivery) enum MissingCardReplacementClaim {
    Existing { message_id: u64, bot_key: String },
    Owned(ClaimedCard),
    Busy { bot_key: String },
}

pub(in crate::services::discord::task_notification_delivery) async fn claim_missing_card_replacement(
    pool: Option<&PgPool>,
    scope: &TaskCardScope,
    missing_message_id: u64,
) -> Result<MissingCardReplacementClaim, String> {
    match pool {
        Some(pool) => claim_missing_card_replacement_pg(pool, scope, missing_message_id).await,
        None if cfg!(any(test, debug_assertions)) => {
            claim_missing_card_replacement_memory(scope, missing_message_id)
        }
        None => Err(memory_fallback_unavailable()),
    }
}

async fn claim_missing_card_replacement_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    missing_message_id: u64,
) -> Result<MissingCardReplacementClaim, String> {
    let channel_id = db_id(scope.channel_id, "channel_id")?;
    let current = sqlx::query(
        "SELECT delivery_state, bot_key, discord_message_id, revision, rendered_content,
                (lease_owner IS NOT NULL AND lease_expires_at > NOW()) AS lease_active
         FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("load missing task card for replacement: {error}"))?;
    let state: String = current.get("delivery_state");
    let bot_key: String = current.get("bot_key");
    let current_message: Option<i64> = current.get("discord_message_id");
    let lease_active: bool = current.get("lease_active");
    if state == "card_posted" {
        let current_message_id = message_id(current_message)?;
        if current_message_id != missing_message_id {
            return Ok(MissingCardReplacementClaim::Existing {
                message_id: current_message_id,
                bot_key,
            });
        }
    }
    if state == "posting" {
        if lease_active {
            return Ok(MissingCardReplacementClaim::Busy { bot_key });
        }
        let lease_owner = uuid::Uuid::new_v4().to_string();
        let row = sqlx::query(
            "UPDATE task_notification_card_state
             SET lease_owner = $5,
                 lease_expires_at = NOW() + make_interval(secs => $6),
                 updated_at = NOW()
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
               AND delivery_state = 'posting'
               AND (lease_owner IS NULL OR lease_expires_at <= NOW())
             RETURNING bot_key, discord_nonce, revision, update_count, rendered_content",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(&scope.session_key)
        .bind(&scope.event_key)
        .bind(&lease_owner)
        .bind(LEASE_SECONDS)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("resume missing task card replacement: {error}"))?;
        let Some(row) = row else {
            return Ok(MissingCardReplacementClaim::Busy { bot_key });
        };
        return Ok(MissingCardReplacementClaim::Owned(claimed_from_row(
            scope,
            lease_owner,
            &row,
            ClaimAction::Post,
        )?));
    }
    if state != "card_posted" || lease_active {
        return Ok(MissingCardReplacementClaim::Busy { bot_key });
    }

    let current_revision: i32 = current.get("revision");
    let next_revision = current_revision.saturating_add(1);
    let next_nonce = stable_nonce(scope, next_revision);
    let rendered_content: String = current.get("rendered_content");
    let lease_owner = uuid::Uuid::new_v4().to_string();
    let row = sqlx::query(
        "UPDATE task_notification_card_state
         SET delivery_state = 'posting', discord_message_id = NULL,
             revision = $7, discord_nonce = $8, lease_owner = $9,
             lease_expires_at = NOW() + make_interval(secs => $10),
             post_started_at = NULL, last_error = NULL, updated_at = NOW()
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND delivery_state = 'card_posted' AND discord_message_id = $5
           AND revision = $6 AND (lease_owner IS NULL OR lease_expires_at <= NOW())
         RETURNING bot_key, discord_nonce, revision, update_count, rendered_content",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(db_id(missing_message_id, "message_id")?)
    .bind(current_revision)
    .bind(next_revision)
    .bind(next_nonce)
    .bind(&lease_owner)
    .bind(LEASE_SECONDS)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("claim missing task card replacement: {error}"))?;
    let Some(row) = row else {
        return Ok(MissingCardReplacementClaim::Busy { bot_key });
    };
    let mut claimed = claimed_from_row(scope, lease_owner, &row, ClaimAction::Post)?;
    if claimed.rendered_content.is_empty() {
        claimed.rendered_content = rendered_content;
    }
    Ok(MissingCardReplacementClaim::Owned(claimed))
}

fn claim_missing_card_replacement_memory(
    scope: &TaskCardScope,
    missing_message_id: u64,
) -> Result<MissingCardReplacementClaim, String> {
    let mut rows = MEMORY_STORE
        .lock()
        .map_err(|_| "task card memory store poisoned".to_string())?;
    let row = rows
        .get_mut(scope)
        .ok_or_else(|| "memory task card row disappeared".to_string())?;
    if row.state == MemoryState::CardPosted && row.message_id != Some(missing_message_id) {
        return Ok(MissingCardReplacementClaim::Existing {
            message_id: row
                .message_id
                .ok_or_else(|| "memory card row omitted message id".to_string())?,
            bot_key: row.bot_key.clone(),
        });
    }
    let now = Instant::now();
    let lease_active = row
        .lease_expires_at
        .is_some_and(|expiry| expiry > now && row.lease_owner.is_some());
    if row.state == MemoryState::Posting && !lease_active {
        let lease_owner = uuid::Uuid::new_v4().to_string();
        row.lease_owner = Some(lease_owner.clone());
        row.lease_expires_at = Some(now + Duration::from_secs(LEASE_SECONDS as u64));
        row.last_error = None;
        return Ok(MissingCardReplacementClaim::Owned(memory_claim(
            scope,
            &lease_owner,
            row,
            ClaimAction::Post,
        )));
    }
    if row.state != MemoryState::CardPosted || lease_active {
        return Ok(MissingCardReplacementClaim::Busy {
            bot_key: row.bot_key.clone(),
        });
    }
    let lease_owner = uuid::Uuid::new_v4().to_string();
    row.revision = row.revision.saturating_add(1);
    row.nonce = stable_nonce(scope, row.revision);
    row.state = MemoryState::Posting;
    row.message_id = None;
    row.post_started_at = None;
    row.lease_owner = Some(lease_owner.clone());
    row.lease_expires_at = Some(now + Duration::from_secs(LEASE_SECONDS as u64));
    row.last_error = None;
    Ok(MissingCardReplacementClaim::Owned(memory_claim(
        scope,
        &lease_owner,
        row,
        ClaimAction::Post,
    )))
}
