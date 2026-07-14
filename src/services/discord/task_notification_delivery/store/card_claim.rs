//! Semantic task-card claim and terminal replay lookup authority.

use super::*;

#[cfg(test)]
static NEW_TERMINAL_CLAIM_RACE_HOOK: LazyLock<
    Mutex<Option<(String, std::sync::Arc<tokio::sync::Barrier>)>>,
> = LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
pub(in crate::services::discord::task_notification_delivery) fn install_new_terminal_claim_race_hook(
    event_key: &str,
) {
    let mut installed = NEW_TERMINAL_CLAIM_RACE_HOOK
        .lock()
        .expect("new terminal claim race hook");
    assert!(installed.is_none(), "new terminal claim race hook leaked");
    *installed = Some((
        event_key.to_string(),
        std::sync::Arc::new(tokio::sync::Barrier::new(2)),
    ));
}

#[cfg(test)]
async fn pause_new_terminal_claim_for_race(scope: &TaskCardScope) {
    let barrier = NEW_TERMINAL_CLAIM_RACE_HOOK
        .lock()
        .expect("new terminal claim race hook")
        .as_ref()
        .filter(|(event_key, _)| event_key == &scope.event_key)
        .map(|(_, barrier)| std::sync::Arc::clone(barrier));
    let Some(barrier) = barrier else {
        return;
    };
    if barrier.wait().await.is_leader() {
        NEW_TERMINAL_CLAIM_RACE_HOOK
            .lock()
            .expect("new terminal claim race hook")
            .take();
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    }
}

pub(super) async fn claim_card_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    preferred_bot_key: &str,
    seed_content: &str,
    seed_hash: &str,
    intent: StoreIntent,
) -> Result<CardClaim, String> {
    let channel_id = db_id(scope.channel_id, "channel_id")?;
    if let Some((message_id, bot_key)) = find_terminal_delivery_pg(pool, scope, seed_hash).await? {
        return Ok(CardClaim::Existing {
            message_id,
            bot_key,
        });
    }
    let lease_owner = uuid::Uuid::new_v4().to_string();
    let nonce = stable_nonce(scope, 1);
    if let Some(row) = sqlx::query(
        "INSERT INTO task_notification_card_state
             (channel_id, provider, session_key, event_key, surface_owner,
              delivery_state, bot_key, discord_nonce, revision, update_count,
              rendered_content, content_hash, lease_owner, lease_expires_at,
              terminal_delivery_fingerprint)
         VALUES ($1, $2, $3, $4, 'card', 'posting', $5, $6, 1, 1, $7, $8,
                 $9, NOW() + make_interval(secs => $10), $11)
         ON CONFLICT DO NOTHING
         RETURNING bot_key, discord_nonce, revision, update_count, rendered_content",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(preferred_bot_key)
    .bind(&nonce)
    .bind(seed_content)
    .bind(seed_hash)
    .bind(&lease_owner)
    .bind(LEASE_SECONDS)
    .bind(&scope.terminal_delivery_fingerprint)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("insert task card lease: {error}"))?
    {
        return Ok(CardClaim::Owned(claimed_from_row(
            scope,
            lease_owner,
            &row,
            ClaimAction::Post,
        )?));
    }

    let current = sqlx::query(
        "SELECT session_key, event_key, terminal_delivery_fingerprint,
                delivery_state, bot_key, discord_nonce, discord_message_id,
                revision, update_count, rendered_content, content_hash,
                (lease_owner IS NOT NULL AND lease_expires_at > NOW()) AS lease_active
         FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2
           AND ((session_key = $3 AND event_key = $4)
                OR ($5::VARCHAR IS NOT NULL AND terminal_delivery_fingerprint = $5))
         ORDER BY CASE WHEN session_key = $3 AND event_key = $4 THEN 0 ELSE 1 END
         LIMIT 1",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(&scope.session_key)
    .bind(&scope.event_key)
    .bind(&scope.terminal_delivery_fingerprint)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("load task card state after conflict: {error}"))?;
    let state: String = current.get("delivery_state");
    let current_bot: String = current.get("bot_key");
    let current_message: Option<i64> = current.get("discord_message_id");
    let lease_active: bool = current.get("lease_active");
    let current_fingerprint: Option<String> = current.get("terminal_delivery_fingerprint");
    let exact_terminal_replay = scope.terminal_delivery_fingerprint.is_some()
        && current_fingerprint == scope.terminal_delivery_fingerprint;
    if state == "card_posted" && (intent == StoreIntent::Promotion || exact_terminal_replay) {
        if lease_active {
            return Ok(CardClaim::Busy {
                bot_key: current_bot,
            });
        }
        return Ok(CardClaim::Existing {
            message_id: message_id(current_message)?,
            bot_key: current_bot,
        });
    }

    let new_terminal_completion = state == "card_posted"
        && intent == StoreIntent::Observation
        && scope.terminal_delivery_fingerprint.is_some()
        && !exact_terminal_replay;
    let prior_terminal_fingerprint = current_fingerprint.clone();
    let mut current_scope = TaskCardScope {
        channel_id: scope.channel_id,
        provider: scope.provider.clone(),
        session_key: current.get("session_key"),
        event_key: current.get("event_key"),
        terminal_delivery_fingerprint: current_fingerprint,
    };
    if current_scope.terminal_delivery_fingerprint.is_none() {
        current_scope.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint.clone();
    }

    let row = if new_terminal_completion {
        sqlx::query(
            "INSERT INTO task_notification_terminal_delivery
                 (channel_id, provider, session_key, event_key,
                  terminal_delivery_fingerprint, discord_message_id, bot_key, content_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (channel_id, provider, session_key, event_key, discord_message_id)
             DO UPDATE SET
                 terminal_delivery_fingerprint = COALESCE(
                     task_notification_terminal_delivery.terminal_delivery_fingerprint,
                     EXCLUDED.terminal_delivery_fingerprint
                 ),
                 bot_key = EXCLUDED.bot_key,
                 content_hash = task_notification_terminal_delivery.content_hash",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(&current_scope.session_key)
        .bind(&current_scope.event_key)
        .bind(&prior_terminal_fingerprint)
        .bind(current_message)
        .bind(&current_bot)
        .bind(current.get::<String, _>("content_hash"))
        .execute(pool)
        .await
        .map_err(|error| format!("preserve prior terminal delivery: {error}"))?;

        current_scope.terminal_delivery_fingerprint = scope.terminal_delivery_fingerprint.clone();
        let current_revision = current.get::<i32, _>("revision");
        let next_revision = current_revision.saturating_add(1);
        let next_nonce = stable_nonce(&current_scope, next_revision);
        #[cfg(test)]
        pause_new_terminal_claim_for_race(scope).await;
        sqlx::query(
            "UPDATE task_notification_card_state
             SET surface_owner = 'card',
                 delivery_state = 'posting',
                 discord_message_id = NULL,
                 discord_nonce = $5,
                 revision = $6,
                 update_count = 1,
                 rendered_content = $7,
                 content_hash = $8,
                 terminal_delivery_fingerprint = $9,
                 post_started_at = NULL,
                 lease_owner = $10,
                 lease_expires_at = NOW() + make_interval(secs => $11),
                 last_error = NULL,
                 updated_at = NOW()
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
               AND delivery_state = 'card_posted'
               AND revision = $12
               AND (lease_owner IS NULL OR lease_expires_at <= NOW())
             RETURNING bot_key, discord_nonce, discord_message_id, revision,
                       update_count, rendered_content",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(&current_scope.session_key)
        .bind(&current_scope.event_key)
        .bind(next_nonce)
        .bind(next_revision)
        .bind(seed_content)
        .bind(seed_hash)
        .bind(&current_scope.terminal_delivery_fingerprint)
        .bind(&lease_owner)
        .bind(LEASE_SECONDS)
        .bind(current_revision)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("claim new terminal completion: {error}"))?
    } else if state == "card_posted" {
        sqlx::query(
            "UPDATE task_notification_card_state
             SET lease_owner = $5,
                 lease_expires_at = NOW() + make_interval(secs => $6),
                 update_count = update_count + 1,
                 updated_at = NOW()
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
               AND delivery_state = 'card_posted'
               AND (lease_owner IS NULL OR lease_expires_at <= NOW())
             RETURNING bot_key, discord_nonce, discord_message_id, revision,
                       update_count, rendered_content",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(&current_scope.session_key)
        .bind(&current_scope.event_key)
        .bind(&lease_owner)
        .bind(LEASE_SECONDS)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("claim task card edit lease: {error}"))?
    } else {
        let preserve_footer_content = intent == StoreIntent::Promotion;
        sqlx::query(
            "UPDATE task_notification_card_state
             SET surface_owner = 'card',
                 delivery_state = 'posting',
                 bot_key = CASE WHEN bot_key = '' THEN $5 ELSE bot_key END,
                 discord_nonce = CASE WHEN bot_key = '' THEN $6 ELSE discord_nonce END,
                 rendered_content = CASE
                     WHEN delivery_state = 'posting' AND post_started_at IS NOT NULL
                         THEN rendered_content
                     WHEN $9 THEN rendered_content ELSE $7
                 END,
                 content_hash = CASE
                     WHEN delivery_state = 'posting' AND post_started_at IS NOT NULL
                         THEN content_hash
                     WHEN $9 THEN content_hash ELSE $8
                 END,
                 post_started_at = CASE
                     WHEN delivery_state = 'footer_only' THEN NULL
                     ELSE post_started_at
                 END,
                 lease_owner = $10,
                 lease_expires_at = NOW() + make_interval(secs => $11),
                 terminal_delivery_fingerprint = COALESCE(
                     terminal_delivery_fingerprint, $12
                 ),
                 last_error = NULL,
                 updated_at = NOW()
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
               AND delivery_state IN ('footer_only', 'posting')
               AND (lease_owner IS NULL OR lease_expires_at <= NOW())
             RETURNING bot_key, discord_nonce, discord_message_id, revision,
                       update_count, rendered_content",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(&current_scope.session_key)
        .bind(&current_scope.event_key)
        .bind(preferred_bot_key)
        .bind(&nonce)
        .bind(seed_content)
        .bind(seed_hash)
        .bind(preserve_footer_content)
        .bind(&lease_owner)
        .bind(LEASE_SECONDS)
        .bind(&current_scope.terminal_delivery_fingerprint)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("claim task card post lease: {error}"))?
    };

    let Some(row) = row else {
        return Ok(CardClaim::Busy {
            bot_key: current_bot,
        });
    };
    let action = if state == "card_posted" && !new_terminal_completion {
        ClaimAction::Edit {
            message_id: message_id(row.get("discord_message_id"))?,
        }
    } else {
        ClaimAction::Post
    };
    let mut claimed = claimed_from_row(&current_scope, lease_owner, &row, action)?;
    claimed.new_terminal_completion = new_terminal_completion;
    Ok(CardClaim::Owned(claimed))
}

pub(super) async fn find_terminal_delivery_pg(
    pool: &PgPool,
    scope: &TaskCardScope,
    content_hash: &str,
) -> Result<Option<(u64, String)>, String> {
    let Some(fingerprint) = scope.terminal_delivery_fingerprint.as_deref() else {
        return Ok(None);
    };
    let channel_id = db_id(scope.channel_id, "channel_id")?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| format!("begin terminal delivery lookup: {error}"))?;
    let delivered = sqlx::query(
        "SELECT id, session_key, event_key, terminal_delivery_fingerprint,
                discord_message_id, bot_key
         FROM task_notification_terminal_delivery
         WHERE channel_id = $1 AND provider = $2
           AND (terminal_delivery_fingerprint = $3
                OR (terminal_delivery_fingerprint IS NULL
                    AND event_key = $4 AND content_hash = $5))
         ORDER BY CASE WHEN terminal_delivery_fingerprint = $3 THEN 0 ELSE 1 END,
                  delivered_at DESC
         LIMIT 1
         FOR UPDATE",
    )
    .bind(channel_id)
    .bind(&scope.provider)
    .bind(fingerprint)
    .bind(&scope.event_key)
    .bind(content_hash)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| format!("load terminal delivery identity: {error}"))?;
    let delivered = match delivered {
        Some(delivered) => delivered,
        None => {
            let card = sqlx::query(
                "SELECT session_key, event_key, terminal_delivery_fingerprint,
                        discord_message_id, bot_key, content_hash
                 FROM task_notification_card_state
                 WHERE channel_id = $1 AND provider = $2
                   AND delivery_state = 'card_posted'
                   AND (terminal_delivery_fingerprint = $3
                        OR (terminal_delivery_fingerprint IS NULL
                            AND event_key = $4 AND content_hash = $5))
                 ORDER BY CASE WHEN terminal_delivery_fingerprint = $3 THEN 0 ELSE 1 END,
                          updated_at DESC
                 LIMIT 1
                 FOR UPDATE",
            )
            .bind(channel_id)
            .bind(&scope.provider)
            .bind(fingerprint)
            .bind(&scope.event_key)
            .bind(content_hash)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|error| format!("load semantic terminal card identity: {error}"))?;
            let Some(card) = card else {
                transaction
                    .commit()
                    .await
                    .map_err(|error| format!("commit empty terminal delivery lookup: {error}"))?;
                return Ok(None);
            };
            let delivered_fingerprint: Option<String> = card.get("terminal_delivery_fingerprint");
            if delivered_fingerprint.is_none() {
                sqlx::query(
                    "UPDATE task_notification_card_state
                     SET terminal_delivery_fingerprint = $6, updated_at = NOW()
                     WHERE channel_id = $1 AND provider = $2
                       AND session_key = $3 AND event_key = $4
                       AND discord_message_id = $5
                       AND terminal_delivery_fingerprint IS NULL",
                )
                .bind(channel_id)
                .bind(&scope.provider)
                .bind(card.get::<String, _>("session_key"))
                .bind(card.get::<String, _>("event_key"))
                .bind(card.get::<i64, _>("discord_message_id"))
                .bind(fingerprint)
                .execute(&mut *transaction)
                .await
                .map_err(|error| format!("backfill semantic card fingerprint: {error}"))?;
            }
            sqlx::query(
                "INSERT INTO task_notification_terminal_delivery
                     (channel_id, provider, session_key, event_key,
                      terminal_delivery_fingerprint, discord_message_id, bot_key, content_hash)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (channel_id, provider, session_key, event_key, discord_message_id)
                 DO UPDATE SET
                     terminal_delivery_fingerprint = COALESCE(
                         task_notification_terminal_delivery.terminal_delivery_fingerprint,
                         EXCLUDED.terminal_delivery_fingerprint
                     ),
                     bot_key = EXCLUDED.bot_key,
                     content_hash = EXCLUDED.content_hash",
            )
            .bind(channel_id)
            .bind(&scope.provider)
            .bind(card.get::<String, _>("session_key"))
            .bind(card.get::<String, _>("event_key"))
            .bind(fingerprint)
            .bind(card.get::<i64, _>("discord_message_id"))
            .bind(card.get::<String, _>("bot_key"))
            .bind(card.get::<String, _>("content_hash"))
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("backfill terminal delivery ledger: {error}"))?;
            let message_id = message_id(Some(card.get("discord_message_id")))?;
            let bot_key: String = card.get("bot_key");
            transaction
                .commit()
                .await
                .map_err(|error| format!("commit semantic terminal lookup: {error}"))?;
            return Ok(Some((message_id, bot_key)));
        }
    };

    let delivery_id: i64 = delivered.get("id");
    let delivered_fingerprint: Option<String> = delivered.get("terminal_delivery_fingerprint");
    if delivered_fingerprint.is_none() {
        sqlx::query(
            "UPDATE task_notification_terminal_delivery
             SET terminal_delivery_fingerprint = $2
             WHERE id = $1 AND terminal_delivery_fingerprint IS NULL",
        )
        .bind(delivery_id)
        .bind(fingerprint)
        .execute(&mut *transaction)
        .await
        .map_err(|error| format!("backfill terminal delivery fingerprint: {error}"))?;
        sqlx::query(
            "UPDATE task_notification_card_state
             SET terminal_delivery_fingerprint = $6, updated_at = NOW()
             WHERE channel_id = $1 AND provider = $2
               AND session_key = $3 AND event_key = $4
               AND discord_message_id = $5
               AND terminal_delivery_fingerprint IS NULL",
        )
        .bind(channel_id)
        .bind(&scope.provider)
        .bind(delivered.get::<String, _>("session_key"))
        .bind(delivered.get::<String, _>("event_key"))
        .bind(delivered.get::<i64, _>("discord_message_id"))
        .bind(fingerprint)
        .execute(&mut *transaction)
        .await
        .map_err(|error| format!("backfill semantic card fingerprint: {error}"))?;
    }
    let message_id = message_id(Some(delivered.get("discord_message_id")))?;
    let bot_key: String = delivered.get("bot_key");
    transaction
        .commit()
        .await
        .map_err(|error| format!("commit terminal delivery lookup: {error}"))?;
    Ok(Some((message_id, bot_key)))
}
