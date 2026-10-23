//! Monitor observations share a leased card; successful edits atomically retain receipts.

use super::*;
#[path = "monitor_memory.rs"]
mod memory;
use super::super::{TaskCardEvent, content_hash};
pub(super) use memory::{claim as claim_memory, record as record_memory};

#[derive(Clone, Debug)]
pub(in crate::services::discord::task_notification_delivery) struct Observation {
    pub(in crate::services::discord::task_notification_delivery) payload: String,
    pub(in crate::services::discord::task_notification_delivery) source: Option<String>,
}

async fn receipt(
    connection: &mut sqlx::PgConnection,
    event: &TaskCardEvent,
    payload: &str,
) -> Result<Option<CardClaim>, sqlx::Error> {
    let candidates = sqlx::query(
        "SELECT id,source_fingerprint,discord_message_id,bot_key,payload_fingerprint,legacy_render_count
         FROM task_notification_monitor_receipt WHERE channel_id=$1 AND provider=$2
           AND (($3::TEXT IS NOT NULL AND source_fingerprint=$3)
                OR (event_key=$4 AND ($3::TEXT IS NULL OR source_fingerprint IS NULL)
                    AND (payload_fingerprint=$5 OR legacy_render_count IS NOT NULL)))
         ORDER BY CASE WHEN source_fingerprint=$3 THEN 0 ELSE 1 END,id FOR UPDATE",
    ).bind(event.scope.channel_id as i64).bind(&event.scope.provider)
        .bind(&event.scope.terminal_delivery_fingerprint).bind(&event.scope.event_key).bind(payload)
        .fetch_all(&mut *connection).await?;
    let row = candidates.into_iter().find(|row| {
        (event.scope.terminal_delivery_fingerprint.is_some()
            && event.scope.terminal_delivery_fingerprint
                == row.get::<Option<String>, _>("source_fingerprint"))
            || row.get::<String, _>("payload_fingerprint") == payload
            || row
                .get::<Option<i64>, _>("legacy_render_count")
                .is_some_and(|count| {
                    content_hash(&event.payload.legacy_render(count as u64))
                        == row.get::<String, _>("payload_fingerprint")
                })
    });
    let Some(row) = row else {
        return Ok(None);
    };
    if row.get::<Option<String>, _>("source_fingerprint").is_none()
        || row.get::<Option<i64>, _>("legacy_render_count").is_some()
    {
        sqlx::query("UPDATE task_notification_monitor_receipt SET source_fingerprint=COALESCE(source_fingerprint,$2),payload_fingerprint=$3,legacy_render_count=NULL WHERE id=$1")
            .bind(row.get::<i64,_>("id")).bind(&event.scope.terminal_delivery_fingerprint).bind(payload)
            .execute(&mut *connection).await?;
    }
    Ok(Some(CardClaim::Existing {
        message_id: row.get::<i64, _>("discord_message_id") as u64,
        bot_key: row.get("bot_key"),
    }))
}

pub(super) async fn record(
    connection: &mut sqlx::PgConnection,
    claimed: &ClaimedCard,
    message_id: u64,
) -> Result<(), sqlx::Error> {
    let Some(observation) = &claimed.monitor else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO task_notification_monitor_receipt
         (channel_id, provider, session_key, event_key, payload_fingerprint,
          source_fingerprint, discord_message_id, bot_key)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
    )
    .bind(claimed.scope.channel_id as i64)
    .bind(&claimed.scope.provider)
    .bind(&claimed.scope.session_key)
    .bind(&claimed.scope.event_key)
    .bind(&observation.payload)
    .bind(&observation.source)
    .bind(message_id as i64)
    .bind(&claimed.bot_key)
    .execute(&mut *connection)
    .await?;
    sqlx::query("UPDATE task_notification_monitor_receipt SET discord_message_id=$6,bot_key=$7 WHERE channel_id=$1 AND provider=$2 AND event_key=$3 AND (($4::TEXT IS NOT NULL AND source_fingerprint=$4) OR ($4::TEXT IS NULL AND source_fingerprint IS NULL AND payload_fingerprint=$5))")
        .bind(claimed.scope.channel_id as i64).bind(&claimed.scope.provider).bind(&claimed.scope.event_key)
        .bind(&observation.source).bind(&observation.payload).bind(message_id as i64).bind(&claimed.bot_key)
        .execute(connection).await?;
    Ok(())
}

pub(super) async fn claim(
    pool: &PgPool,
    event: &TaskCardEvent,
    bot: &str,
) -> Result<CardClaim, String> {
    let payload = event.payload.monitor_fingerprint();
    let source = event.scope.terminal_delivery_fingerprint.clone();
    let mut scope = event.scope.clone();
    scope.terminal_delivery_fingerprint = Some(source.clone().unwrap_or_else(|| payload.clone()));
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    if let Some(existing) = receipt(&mut tx, event, &payload)
        .await
        .map_err(|e| e.to_string())?
    {
        tx.commit().await.map_err(|e| e.to_string())?;
        return Ok(existing);
    }
    // Terminal receipts predate Monitor merging and remain authoritative across rollout.
    if let Some(source) = &source {
        let old = sqlx::query("SELECT discord_message_id, bot_key FROM task_notification_terminal_delivery WHERE channel_id=$1 AND provider=$2 AND terminal_delivery_fingerprint=$3")
            .bind(scope.channel_id as i64).bind(&scope.provider).bind(source)
            .fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
        if let Some(old) = old {
            let existing = CardClaim::Existing {
                message_id: old.get::<i64, _>("discord_message_id") as u64,
                bot_key: old.get("bot_key"),
            };
            tx.commit().await.map_err(|e| e.to_string())?;
            return Ok(existing);
        }
    }
    let mut current = sqlx::query(
        "SELECT *, (lease_owner IS NOT NULL AND lease_expires_at > NOW()) AS lease_active
         FROM task_notification_card_state WHERE channel_id=$1 AND provider=$2
           AND ((session_key=$3 AND event_key=$4) OR terminal_delivery_fingerprint=$5)
         ORDER BY CASE WHEN session_key=$3 AND event_key=$4 THEN 0 ELSE 1 END LIMIT 1 FOR UPDATE",
    )
    .bind(scope.channel_id as i64)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(&scope.terminal_delivery_fingerprint)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let mut inserted = false;
    if current.is_none() {
        let content = event.payload.render(1);
        current = sqlx::query(
            "INSERT INTO task_notification_card_state
             (channel_id,provider,session_key,event_key,surface_owner,delivery_state,
              bot_key,discord_nonce,rendered_content,content_hash,terminal_delivery_fingerprint)
             VALUES ($1,$2,$3,$4,'card','posting',$5,$6,$7,$8,$9)
             ON CONFLICT DO NOTHING RETURNING *, false AS lease_active",
        )
        .bind(scope.channel_id as i64)
        .bind(&scope.provider)
        .bind(&scope.session_key)
        .bind(&scope.event_key)
        .bind(bot)
        .bind(stable_nonce(&scope, 1))
        .bind(&content)
        .bind(content_hash(&content))
        .bind(&scope.terminal_delivery_fingerprint)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        inserted = current.is_some();
    }
    // A concurrent edit can release the old source unique key while INSERT waits.
    if let Some(existing) = receipt(&mut tx, event, &payload)
        .await
        .map_err(|e| e.to_string())?
    {
        if inserted {
            tx.rollback().await.map_err(|e| e.to_string())?;
        } else {
            tx.commit().await.map_err(|e| e.to_string())?;
        }
        return Ok(existing);
    }
    let Some(row) = current else {
        return Ok(CardClaim::Busy {
            bot_key: bot.to_string(),
        });
    };
    let current_bot: String = row.get("bot_key");
    if row.get::<bool, _>("lease_active") {
        return Ok(CardClaim::Busy {
            bot_key: current_bot,
        });
    }
    scope.session_key = row.get("session_key");
    scope.event_key = row.get("event_key");
    let state: String = row.get("delivery_state");
    let old_fingerprint: Option<String> = row.get("terminal_delivery_fingerprint");
    let count: i64 = row.get("update_count");
    let old_content: String = row.get("rendered_content");
    let revision: i32 = row.get("revision");
    let old_message: Option<i64> = row.get("discord_message_id");
    let same = old_fingerprint == scope.terminal_delivery_fingerprint
        || (old_fingerprint.is_none()
            && source.is_none()
            && old_content == event.payload.legacy_render(count as u64));
    if state == "card_posted" && same {
        let mut claimed = claimed_from_row(&scope, String::new(), &row, ClaimAction::Post)?;
        claimed.monitor = Some(Observation { payload, source });
        record(&mut tx, &claimed, message_id(old_message)?)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        return Ok(CardClaim::Existing {
            message_id: message_id(old_message)?,
            bot_key: current_bot,
        });
    }
    if state == "card_posted" {
        sqlx::query(
            "INSERT INTO task_notification_monitor_receipt
             (channel_id,provider,session_key,event_key,payload_fingerprint,source_fingerprint,discord_message_id,bot_key,legacy_render_count)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT DO NOTHING",
        ).bind(scope.channel_id as i64).bind(&scope.provider).bind(&scope.session_key)
            .bind(&scope.event_key).bind(old_fingerprint.clone().unwrap_or_else(|| row.get::<String,_>("content_hash")))
            .bind(&old_fingerprint).bind(old_message).bind(&current_bot)
            .bind(if old_fingerprint.is_none() {Some(count)} else {None})
            .execute(&mut *tx).await.map_err(|e| e.to_string())?;
    }
    let started = row
        .get::<Option<DateTime<Utc>>, _>("post_started_at")
        .is_some();
    let editing = state == "card_posted" && current_bot == bot;
    let replacing = state == "card_posted" && !editing;
    let next_count = if state == "card_posted" {
        count.saturating_add(1)
    } else {
        count
    };
    let next_revision = if replacing {
        revision.saturating_add(1)
    } else {
        revision
    };
    let next_content = if started {
        old_content
    } else {
        event.payload.render(next_count as u64)
    };
    let owner = uuid::Uuid::new_v4().to_string();
    let actual_bot = if started {
        current_bot.clone()
    } else {
        bot.to_string()
    };
    let actual_fingerprint = if started {
        row.get::<Option<String>, _>("terminal_delivery_fingerprint")
    } else {
        scope.terminal_delivery_fingerprint.clone()
    };
    let nonce = if started || editing {
        row.get::<String, _>("discord_nonce")
    } else {
        stable_nonce(&scope, next_revision)
    };
    let changed = sqlx::query(
        "UPDATE task_notification_card_state SET
         surface_owner='card', delivery_state=$2, bot_key=$3, discord_nonce=$4,
         revision=$5, rendered_content=CASE WHEN $6 THEN rendered_content ELSE $7 END,
         content_hash=CASE WHEN $6 THEN content_hash ELSE $8 END,
         update_count=CASE WHEN $6 THEN update_count ELSE $9 END,
         discord_message_id=CASE WHEN $6 THEN discord_message_id ELSE NULL END,
         terminal_delivery_fingerprint=CASE WHEN $6 THEN terminal_delivery_fingerprint ELSE $10 END,
         lease_owner=$11, lease_expires_at=NOW()+make_interval(secs=>$12), last_error=NULL,
         updated_at=NOW() WHERE id=$1
         RETURNING bot_key,discord_nonce,revision,update_count,rendered_content",
    )
    .bind(row.get::<i64, _>("id"))
    .bind(if editing { "card_posted" } else { "posting" })
    .bind(actual_bot)
    .bind(nonce)
    .bind(next_revision)
    .bind(editing)
    .bind(&next_content)
    .bind(content_hash(&next_content))
    .bind(next_count)
    .bind(&actual_fingerprint)
    .bind(&owner)
    .bind(LEASE_SECONDS)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    scope.terminal_delivery_fingerprint = actual_fingerprint;
    let action = if editing {
        ClaimAction::Edit {
            message_id: message_id(old_message)?,
        }
    } else {
        ClaimAction::Post
    };
    let mut claimed = claimed_from_row(&scope, owner, &changed, action)?;
    claimed.update_count = next_count as u64;
    claimed.new_terminal_completion = replacing;
    if !started || same {
        claimed.monitor = Some(Observation { payload, source });
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(CardClaim::Owned(claimed))
}

pub(super) async fn mark_edited(
    pool: &PgPool,
    claimed: &ClaimedCard,
    message: u64,
    content: &str,
    hash: &str,
) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let changed=sqlx::query(
        "UPDATE task_notification_card_state SET rendered_content=$7,content_hash=$8,
         update_count=$9,terminal_delivery_fingerprint=$10,lease_owner=NULL,lease_expires_at=NULL,
         last_error=NULL,updated_at=NOW() WHERE channel_id=$1 AND provider=$2 AND session_key=$3
         AND event_key=$4 AND lease_owner=$5 AND discord_message_id=$6 AND delivery_state='card_posted'",
    ).bind(claimed.scope.channel_id as i64).bind(&claimed.scope.provider).bind(&claimed.scope.session_key)
        .bind(&claimed.scope.event_key).bind(&claimed.lease_owner).bind(message as i64).bind(content)
        .bind(hash).bind(claimed.update_count as i64).bind(&claimed.scope.terminal_delivery_fingerprint)
        .execute(&mut *tx).await.map_err(|e|e.to_string())?.rows_affected();
    exact_change(changed, "commit Monitor edit")?;
    record(&mut tx, claimed, message)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}
