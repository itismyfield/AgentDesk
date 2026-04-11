use super::*;
use poise::serenity_prelude::{CreateAttachment, CreateMessage};

/// Check if a user is authorized (owner or allowed user)
/// Returns true if authorized, false if rejected.
/// On first use, registers the user as owner.
pub(super) async fn check_auth(
    user_id: UserId,
    user_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
) -> bool {
    let mut settings = shared.settings.write().await;
    match settings.owner_user_id {
        None => {
            // Imprint: register first user as owner
            settings.owner_user_id = Some(user_id.get());
            save_bot_settings(token, &settings);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ★ Owner registered: {user_name} (id:{})",
                user_id.get()
            );
            true
        }
        Some(_) => {
            let uid = user_id.get();
            if user_is_authorized(&settings, uid) {
                true
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ✗ Rejected: {user_name} (id:{})", uid);
                false
            }
        }
    }
}

pub(super) fn user_is_authorized(settings: &DiscordBotSettings, user_id: u64) -> bool {
    settings.allow_all_users
        || settings.owner_user_id == Some(user_id)
        || settings.allowed_user_ids.contains(&user_id)
}

/// Check if a user is the owner (not just allowed)
pub(super) async fn check_owner(user_id: UserId, shared: &Arc<SharedData>) -> bool {
    let settings = shared.settings.read().await;
    settings.owner_user_id == Some(user_id.get())
}

/// Check for pending DM replies and consume them. The answer text is stored
/// in the consumed row's context (as `_answer`), and a notification is sent
/// to the source agent's Discord channel so its session can process the reply.
pub(super) async fn try_handle_pending_dm_reply(
    db: &crate::db::Db,
    msg: &serenity::Message,
) -> bool {
    if msg.author.bot || msg.guild_id.is_some() {
        return false;
    }
    let answer = msg.content.trim();
    if answer.is_empty() {
        return false;
    }
    let user_id_str = msg.author.id.get().to_string();
    let username = msg.author.name.clone();
    let db = db.clone();
    let answer_owned = answer.to_string();
    let result = tokio::task::spawn_blocking(move || {
        consume_pending_dm_reply(&db, &user_id_str, &answer_owned)
    })
    .await;
    match result {
        Ok(Some(info)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ✉️ DM reply consumed: user={} agent={} id={}",
                msg.author.id.get(),
                info.source_agent,
                info.id
            );

            // Notify the source agent's Discord channel (inline, not fire-and-forget)
            if let Err(e) = notify_source_agent(
                &info.db,
                &info.source_agent,
                info.id,
                info.channel_id.as_deref(),
                &username,
                &info.answer,
            )
            .await
            {
                tracing::warn!("  [dm-reply] notify source agent failed: {e}");
                // Record failure in context so readConsumed can detect it
                let db3 = info.db.clone();
                let reply_id = info.id;
                let err_msg = format!("{e}");
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(conn) = db3.separate_conn() {
                        let _ = conn.execute(
                            "UPDATE pending_dm_replies SET context = \
                             json_set(context, '$._notify_failed', json('true'), '$._notify_error', ?1) \
                             WHERE id = ?2",
                            rusqlite::params![err_msg, reply_id],
                        );
                    }
                })
                .await;
            }

            true
        }
        Ok(None) => false,
        Err(e) => {
            tracing::warn!("  [dm-reply] consume task error: {e}");
            false
        }
    }
}

/// Send a notification to the source agent's Discord channel about the DM reply.
/// Prefers the stored `channel_id` from the pending row (alt/thread channels);
/// falls back to `agents.discord_channel_id` only if none was stored.
async fn notify_source_agent(
    db: &crate::db::Db,
    source_agent: &str,
    reply_id: i64,
    stored_channel_id: Option<&str>,
    username: &str,
    answer: &str,
) -> Result<(), String> {
    let token =
        crate::credential::read_bot_token("announce").ok_or("no announce bot token configured")?;

    // Prefer the stored channel_id from the pending row (supports alt/thread channels)
    let channel_id: u64 = if let Some(ch) = stored_channel_id {
        resolve_channel_to_u64(ch)?
    } else {
        // Fall back to the agent's primary discord_channel_id
        let db = db.clone();
        let agent_name = source_agent.to_string();
        let ch_opt: Option<String> = tokio::task::spawn_blocking(move || {
            let conn = db.separate_conn().map_err(|e| format!("{e}"))?;
            crate::db::agents::resolve_agent_primary_channel_on_conn(&conn, &agent_name)
                .map_err(|e| format!("{e}"))
        })
        .await
        .map_err(|e| format!("join: {e}"))??;
        let raw = ch_opt.ok_or("agent has no discord_channel_id")?;
        resolve_channel_to_u64(&raw)?
    };

    let message = format!("DM_REPLY:{reply_id} from {username}: {answer}");
    send_message_to_channel(&token, channel_id, &message)
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(())
}

/// Parse a channel identifier — numeric ID or name alias (e.g. "윤호네비서") → u64.
fn resolve_channel_to_u64(raw: &str) -> Result<u64, String> {
    raw.parse::<u64>().or_else(|_| {
        crate::server::routes::dispatches::resolve_channel_alias_pub(raw)
            .ok_or_else(|| format!("cannot resolve channel '{raw}'"))
    })
}

/// Retry DM reply notifications that previously failed (`_notify_failed` in context).
/// Called from the 5-min tick loop.
pub async fn retry_failed_dm_notifications(db: &crate::db::Db) {
    let db2 = db.clone();
    let entries: Vec<(i64, String, String, Option<String>)> =
        match tokio::task::spawn_blocking(move || {
            let conn = db2.separate_conn().map_err(|e| format!("{e}"))?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
                     WHERE status = 'consumed' AND json_extract(context, '$._notify_failed') IS NOT NULL \
                     LIMIT 10",
                )
                .map_err(|e| format!("{e}"))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .map_err(|e| format!("{e}"))?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            Ok::<_, String>(rows)
        })
        .await
        {
            Ok(Ok(v)) => v,
            _ => return,
        };

    if entries.is_empty() {
        return;
    }

    for (id, source_agent, context_str, channel_id) in entries {
        let ctx: serde_json::Value =
            serde_json::from_str(&context_str).unwrap_or(serde_json::json!({}));
        let answer = ctx
            .get("_answer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if answer.is_empty() {
            continue;
        }

        match notify_source_agent(
            db,
            &source_agent,
            id,
            channel_id.as_deref(),
            "(retry)",
            &answer,
        )
        .await
        {
            Ok(()) => {
                // Clear _notify_failed on success
                let db3 = db.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(conn) = db3.separate_conn() {
                        let _ = conn.execute(
                            "UPDATE pending_dm_replies SET context = \
                             json_remove(context, '$._notify_failed', '$._notify_error') \
                             WHERE id = ?1",
                            rusqlite::params![id],
                        );
                    }
                })
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ✉️ DM reply retry OK: id={id} agent={source_agent}");
            }
            Err(e) => {
                tracing::warn!("  [dm-reply] retry still failing id={id}: {e}");
            }
        }
    }
}

struct ConsumedDmReply {
    id: i64,
    source_agent: String,
    answer: String,
    channel_id: Option<String>,
    db: crate::db::Db,
}

fn consume_pending_dm_reply(
    db: &crate::db::Db,
    user_id: &str,
    answer: &str,
) -> Option<ConsumedDmReply> {
    let conn = db.separate_conn().ok()?;
    // FIFO: consume oldest non-expired pending entry
    let row: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'pending' \
         AND (expires_at IS NULL OR expires_at > datetime('now')) \
         ORDER BY created_at ASC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    let (id, source_agent, context_str, channel_id) = row.ok()?;

    // Merge the answer into the context JSON
    let mut context: serde_json::Value =
        serde_json::from_str(&context_str).unwrap_or(serde_json::json!({}));
    context["_answer"] = serde_json::Value::String(answer.to_string());
    let updated_context = serde_json::to_string(&context).unwrap_or_default();

    // CAS: only mark consumed if still pending (guards against race)
    let updated = conn.execute(
        "UPDATE pending_dm_replies SET status = 'consumed', consumed_at = datetime('now'), \
         context = ?1 WHERE id = ?2 AND status = 'pending'",
        rusqlite::params![updated_context, id],
    );
    match updated {
        Ok(0) => return None,
        Err(_) => return None,
        _ => {}
    }

    Some(ConsumedDmReply {
        id,
        source_agent,
        answer: answer.to_string(),
        channel_id,
        db: db.clone(),
    })
}

/// Rate limit helper — ensures minimum 1s gap between API calls per channel
pub(super) async fn rate_limit_wait(shared: &Arc<SharedData>, channel_id: ChannelId) {
    let min_gap = tokio::time::Duration::from_millis(1000);
    let sleep_until = {
        let now = tokio::time::Instant::now();
        let default_ts = now - tokio::time::Duration::from_secs(10);
        let last_ts = shared
            .api_timestamps
            .get(&channel_id)
            .map(|r| *r.value())
            .unwrap_or(default_ts);
        let earliest_next = last_ts + min_gap;
        let target = if earliest_next > now {
            earliest_next
        } else {
            now
        };
        shared.api_timestamps.insert(channel_id, target);
        target
    };
    tokio::time::sleep_until(sleep_until).await;
}

/// Add a reaction to a message
pub(super) async fn add_reaction(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    if let Err(e) = channel_id
        .create_reaction(&ctx.http, message_id, reaction)
        .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ Failed to add reaction '{emoji}' to msg {message_id} in channel {channel_id}: {e}"
        );
    }
}

/// Send a file to a Discord channel (called from CLI --discord-sendfile)
pub async fn send_file_to_channel(
    token: &str,
    channel_id: u64,
    file_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(file_path);
    if !path.exists() {
        return Err(format!("File not found: {}", file_path).into());
    }

    let http = serenity::Http::new(token);

    let channel = ChannelId::new(channel_id);
    let attachment = CreateAttachment::path(path).await?;

    channel
        .send_message(
            &http,
            CreateMessage::new()
                .content(format!(
                    "📎 {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ))
                .add_file(attachment),
        )
        .await?;

    Ok(())
}

/// Send a text message to a Discord channel (called from CLI --discord-sendmessage)
pub async fn send_message_to_channel(
    token: &str,
    channel_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let channel = ChannelId::new(channel_id);

    channel
        .send_message(&http, CreateMessage::new().content(message))
        .await?;

    Ok(())
}

/// Send a text message to a Discord user DM (called from CLI --discord-senddm)
pub async fn send_message_to_user(
    token: &str,
    user_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let dm_channel = UserId::new(user_id).create_dm_channel(&http).await?;

    dm_channel
        .id
        .send_message(&http, CreateMessage::new().content(message))
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_is_authorized_allows_owner_and_explicit_users() {
        let settings = DiscordBotSettings {
            owner_user_id: Some(42),
            allowed_user_ids: vec![7],
            ..Default::default()
        };

        assert!(user_is_authorized(&settings, 42));
        assert!(user_is_authorized(&settings, 7));
        assert!(!user_is_authorized(&settings, 99));
    }

    #[test]
    fn user_is_authorized_allows_everyone_when_flag_enabled() {
        let settings = DiscordBotSettings {
            owner_user_id: Some(42),
            allow_all_users: true,
            ..Default::default()
        };

        assert!(user_is_authorized(&settings, 42));
        assert!(user_is_authorized(&settings, 99));
    }
}
