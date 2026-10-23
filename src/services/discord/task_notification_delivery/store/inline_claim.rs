//! Presentation changes re-pin only unused attempts or a newly claimed revision.
use super::super::{TaskCardEvent, content_hash};
use super::*;

pub(in crate::services::discord::task_notification_delivery) async fn claim_event(
    pool: Option<&PgPool>,
    event: &TaskCardEvent,
    bot: &str,
    intent: StoreIntent,
    metadata: Option<
        &crate::services::discord::completion_footer_metadata::CompletionFooterMetadata,
    >,
) -> Result<CardClaim, String> {
    if event.payload.is_monitor() && intent == StoreIntent::Observation {
        return match pool {
            Some(pool) => monitor::claim(pool, event, bot).await,
            None if cfg!(any(test, debug_assertions)) => monitor::claim_memory(event, bot),
            None => Err(memory_fallback_unavailable()),
        };
    }
    let content = metadata.map_or_else(
        || event.payload.render(1),
        |metadata| event.payload.render_with_completion_metadata(1, metadata),
    );
    let hash = content_hash(&content);
    if let Some(pool) = pool {
        if let Some((message_id, bot_key)) =
            find_terminal_delivery_pg(pool, &event.scope, &hash).await?
        {
            return Ok(CardClaim::Existing {
                message_id,
                bot_key,
            });
        }
        if event.scope.terminal_delivery_fingerprint.is_some() {
            let legacy_hash = content_hash(&event.payload.legacy_render(1));
            if let Some((message_id, bot_key)) =
                find_terminal_delivery_pg(pool, &event.scope, &legacy_hash).await?
            {
                return Ok(CardClaim::Existing {
                    message_id,
                    bot_key,
                });
            }
        }
        if let Some(claim) = repin(pool, event, bot, &content, &hash, intent).await? {
            return Ok(claim);
        }
    } else if cfg!(any(test, debug_assertions)) {
        if event.scope.terminal_delivery_fingerprint.is_some() {
            let legacy_hash = content_hash(&event.payload.legacy_render(1));
            if let Some((message_id, bot_key)) =
                find_terminal_delivery_memory(&event.scope, &legacy_hash)?
            {
                return Ok(CardClaim::Existing {
                    message_id,
                    bot_key,
                });
            }
        }
        repin_memory(event, bot, &content, &hash, intent);
    }
    claim_card(pool, &event.scope, bot, &content, &hash, intent).await
}

async fn repin(
    pool: &PgPool,
    event: &TaskCardEvent,
    bot: &str,
    content: &str,
    hash: &str,
    intent: StoreIntent,
) -> Result<Option<CardClaim>, String> {
    let scope = &event.scope;
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let row=sqlx::query("SELECT *, (lease_owner IS NOT NULL AND lease_expires_at>NOW()) AS lease_active FROM task_notification_card_state WHERE channel_id=$1 AND provider=$2 AND session_key=$3 AND event_key=$4 FOR UPDATE")
        .bind(scope.channel_id as i64).bind(&scope.provider).bind(&scope.session_key).bind(&scope.event_key)
        .fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.get::<bool, _>("lease_active") {
        return Ok(Some(CardClaim::Busy {
            bot_key: row.get("bot_key"),
        }));
    }
    let state: String = row.get("delivery_state");
    let old_bot: String = row.get("bot_key");
    let same = scope.terminal_delivery_fingerprint.is_some()
        && scope.terminal_delivery_fingerprint
            == row.get::<Option<String>, _>("terminal_delivery_fingerprint");
    let can_change_owner = !event.payload.is_completion_card()
        || super::super::super::session_notice::is_inline(
            &row.get::<String, _>("rendered_content"),
        );
    let target_bot = if can_change_owner || old_bot.is_empty() {
        bot
    } else {
        &old_bot
    };
    let next = state == "card_posted"
        && old_bot != bot
        && can_change_owner
        && !same
        && intent == StoreIntent::Observation;
    let unused = state != "card_posted"
        && row
            .get::<Option<DateTime<Utc>>, _>("post_started_at")
            .is_none();
    if !next && !unused {
        return Ok(None);
    }
    if next {
        sqlx::query("INSERT INTO task_notification_terminal_delivery (channel_id,provider,session_key,event_key,terminal_delivery_fingerprint,discord_message_id,bot_key,content_hash) SELECT channel_id,provider,session_key,event_key,terminal_delivery_fingerprint,discord_message_id,bot_key,content_hash FROM task_notification_card_state WHERE id=$1 ON CONFLICT DO NOTHING")
            .bind(row.get::<i64,_>("id")).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    }
    let owner = uuid::Uuid::new_v4().to_string();
    let revision = row
        .get::<i32, _>("revision")
        .saturating_add(i32::from(next));
    let fingerprint = if next {
        scope.terminal_delivery_fingerprint.clone()
    } else {
        row.get::<Option<String>, _>("terminal_delivery_fingerprint")
            .or_else(|| scope.terminal_delivery_fingerprint.clone())
    };
    let changed=sqlx::query("UPDATE task_notification_card_state SET surface_owner='card',delivery_state='posting',bot_key=$2,rendered_content=$3,content_hash=$4,discord_message_id=NULL,revision=$5,discord_nonce=$6,update_count=CASE WHEN $7 THEN 1 ELSE update_count END,terminal_delivery_fingerprint=$8,lease_owner=$9,lease_expires_at=NOW()+make_interval(secs=>$10),updated_at=NOW() WHERE id=$1 RETURNING bot_key,discord_nonce,revision,update_count,rendered_content")
        .bind(row.get::<i64,_>("id")).bind(target_bot).bind(content).bind(hash).bind(revision)
        .bind(stable_nonce(scope,revision)).bind(next).bind(&fingerprint).bind(&owner).bind(LEASE_SECONDS)
        .fetch_one(&mut *tx).await.map_err(|e|e.to_string())?;
    let mut actual_scope = scope.clone();
    actual_scope.terminal_delivery_fingerprint = fingerprint;
    let mut claim = claimed_from_row(&actual_scope, owner, &changed, ClaimAction::Post)?;
    claim.new_terminal_completion = next;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(Some(CardClaim::Owned(claim)))
}

fn repin_memory(event: &TaskCardEvent, bot: &str, content: &str, hash: &str, intent: StoreIntent) {
    let Ok(mut rows) = MEMORY_STORE.lock() else {
        return;
    };
    let Some(row) = rows.get_mut(&event.scope) else {
        return;
    };
    if row.lease_owner.is_some() && row.lease_expires_at.is_some_and(|t| t > Instant::now()) {
        return;
    }
    let same = event.scope.terminal_delivery_fingerprint.is_some()
        && event.scope.terminal_delivery_fingerprint == row.terminal_delivery_fingerprint;
    let can_change_owner = !event.payload.is_completion_card()
        || super::super::super::session_notice::is_inline(&row.rendered_content);
    if row.state == MemoryState::CardPosted
        && row.bot_key != bot
        && can_change_owner
        && !same
        && intent == StoreIntent::Observation
    {
        row.revision = row.revision.saturating_add(1);
        row.nonce = stable_nonce(&event.scope, row.revision);
        row.state = MemoryState::Posting;
        row.message_id = None;
        row.post_started_at = None;
        row.terminal_delivery_fingerprint = event.scope.terminal_delivery_fingerprint.clone();
        row.update_count = 1;
    }
    if row.state != MemoryState::CardPosted && row.post_started_at.is_none() {
        row.state = MemoryState::Posting;
        if can_change_owner || row.bot_key.is_empty() {
            row.bot_key = bot.to_string();
        }
        row.rendered_content = content.to_string();
        row.content_hash = hash.to_string();
    }
}

pub(in crate::services::discord::task_notification_delivery) async fn restore_completion_event(
    pool: Option<&PgPool>,
    event: &TaskCardEvent,
) -> Result<TaskCardEvent, String> {
    if event.payload.is_completion_card() || event.payload.is_monitor() {
        return Ok(event.clone());
    }
    let content = if let Some(pool) = pool {
        sqlx::query_scalar::<_,String>("SELECT rendered_content FROM task_notification_card_state WHERE channel_id=$1 AND provider=$2 AND session_key=$3 AND event_key=$4")
            .bind(event.scope.channel_id as i64).bind(&event.scope.provider).bind(&event.scope.session_key)
            .bind(&event.scope.event_key).fetch_optional(pool).await.map_err(|e|e.to_string())?
    } else if cfg!(any(test, debug_assertions)) {
        MEMORY_STORE
            .lock()
            .map_err(|e| e.to_string())?
            .get(&event.scope)
            .map(|row| row.rendered_content.clone())
    } else {
        None
    };
    let mut event = event.clone();
    if let Some(content) = content.filter(|content| {
        !super::super::super::session_notice::is_inline(content)
            && content.split_once("\n\n").is_some_and(|(_, body)| {
                body.lines()
                    .any(|line| !line.trim().is_empty() && !line.starts_with("-# "))
            })
    }) {
        event.payload = super::super::TaskCardPayload::Subagent(normalize_completion(&content));
    }
    Ok(event)
}

pub(super) fn normalize_completion(content: &str) -> String {
    let mut lines = content.lines();
    let first = lines.next().unwrap_or("");
    let legacy = first
        .split_once(' ')
        .is_some_and(|(_, title)| title.starts_with("Task "));
    let mut result = if legacy {
        let icon = first.split_whitespace().next().unwrap_or("📋");
        format!("{icon} {}", lines.next().unwrap_or("Task notification"))
    } else {
        first.to_string()
    };
    for line in lines {
        let line = if let Some(footer) = line.strip_prefix("-# ").filter(|_| legacy) {
            let items = footer
                .split(" · ")
                .filter(|item| !item.trim_start().starts_with("task "))
                .collect::<Vec<_>>();
            if items.is_empty() {
                continue;
            }
            format!("-# {}", items.join(" · "))
        } else {
            line.to_string()
        };
        result.push('\n');
        result.push_str(&line);
    }
    result.trim_end().to_string()
}
