use super::*;
use poise::serenity_prelude::{CreateAttachment, CreateMessage};

/// Check if a user is authorized (owner or allowed user)
/// Returns true if authorized, false if rejected.
/// Requires an explicitly configured owner unless allow-all mode is enabled.
pub(super) async fn check_auth(
    user_id: UserId,
    user_name: &str,
    shared: &Arc<SharedData>,
    _token: &str,
) -> bool {
    let settings = shared.settings.write().await;
    match settings.owner_user_id {
        None => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ✗ Rejected: {user_name} (id:{}) — owner_user_id is not configured",
                user_id.get()
            );
            false
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
    pg_pool: Option<&sqlx::PgPool>,
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
    let answer_owned = answer.to_string();
    match consume_pending_dm_reply(db, pg_pool, &user_id_str, &answer_owned).await {
        Some(info) => {
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
                &info.context,
            )
            .await
            {
                tracing::warn!("  [dm-reply] notify source agent failed: {e}");
                // Record failure in context so readConsumed can detect it
                let db3 = info.db.clone();
                let reply_id = info.id;
                let err_msg = format!("{e}");
                let _ = crate::services::discord::dm_reply_store::mark_pending_dm_reply_notify_failed_db(
                    &db3,
                    pg_pool,
                    reply_id,
                    &err_msg,
                )
                .await;
            }

            true
        }
        None => false,
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
    context: &serde_json::Value,
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

    let message = format_dm_reply_notification(reply_id, username, answer, context)?;
    send_message_to_channel(&token, channel_id, &message)
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(())
}

fn format_dm_reply_notification(
    reply_id: i64,
    username: &str,
    answer: &str,
    context: &serde_json::Value,
) -> Result<String, String> {
    let context_json = serde_json::to_string(&notification_context(context))
        .map_err(|e| format!("serialize dm reply context: {e}"))?;
    Ok(format!(
        "DM_REPLY:{reply_id} from {username}: {answer}\ncontext={context_json}"
    ))
}

fn notification_context(context: &serde_json::Value) -> serde_json::Value {
    match context {
        serde_json::Value::Object(map) => {
            let mut cleaned = map.clone();
            cleaned.remove("_answer");
            cleaned.remove("_notify_failed");
            cleaned.remove("_notify_error");
            serde_json::Value::Object(cleaned)
        }
        _ => context.clone(),
    }
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
pub async fn retry_failed_dm_notifications(db: &crate::db::Db, pg_pool: Option<&sqlx::PgPool>) {
    let entries =
        match crate::services::discord::dm_reply_store::load_failed_consumed_dm_replies_db(
            db, pg_pool,
        )
        .await
        {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!("  [dm-reply] list failed notifications: {error}");
                return;
            }
        };

    if entries.is_empty() {
        return;
    }

    for entry in entries {
        let ctx: serde_json::Value =
            serde_json::from_str(&entry.context_json).unwrap_or(serde_json::json!({}));
        let answer = ctx.get("_answer").and_then(|v| v.as_str()).unwrap_or("");
        if answer.is_empty() {
            continue;
        }

        match notify_source_agent(
            db,
            &entry.source_agent,
            entry.id,
            entry.channel_id.as_deref(),
            "(retry)",
            answer,
            &ctx,
        )
        .await
        {
            Ok(()) => {
                // Clear _notify_failed on success
                let _ = crate::services::discord::dm_reply_store::clear_pending_dm_reply_notify_failure_db(
                    db,
                    pg_pool,
                    entry.id,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ✉️ DM reply retry OK: id={} agent={}",
                    entry.id,
                    entry.source_agent
                );
            }
            Err(e) => {
                tracing::warn!("  [dm-reply] retry still failing id={}: {e}", entry.id);
            }
        }
    }
}

struct ConsumedDmReply {
    id: i64,
    source_agent: String,
    answer: String,
    context: serde_json::Value,
    channel_id: Option<String>,
    db: crate::db::Db,
}

async fn consume_pending_dm_reply(
    db: &crate::db::Db,
    pg_pool: Option<&sqlx::PgPool>,
    user_id: &str,
    answer: &str,
) -> Option<ConsumedDmReply> {
    let row = crate::services::discord::dm_reply_store::load_oldest_pending_dm_reply_db(
        db, pg_pool, user_id,
    )
    .await
    .ok()??;

    // Merge the answer into the context JSON
    let mut context: serde_json::Value =
        serde_json::from_str(&row.context_json).unwrap_or(serde_json::json!({}));
    let notification_context = context.clone();
    context["_answer"] = serde_json::Value::String(answer.to_string());
    let updated_context = serde_json::to_string(&context).unwrap_or_default();

    // CAS: only mark consumed if still pending (guards against race)
    let updated = crate::services::discord::dm_reply_store::mark_pending_dm_reply_consumed_db(
        db,
        pg_pool,
        row.id,
        &updated_context,
    )
    .await
    .ok()?;
    if !updated {
        return None;
    }

    Some(ConsumedDmReply {
        id: row.id,
        source_agent: row.source_agent,
        answer: answer.to_string(),
        context: notification_context,
        channel_id: row.channel_id,
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
    use serde_json::json;

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

    #[test]
    fn format_dm_reply_notification_inlines_saved_context() {
        let message = format_dm_reply_notification(
            42,
            "family-counsel",
            "지난주에 했어",
            &json!({
                "topicKey": "obujang.health_checkup",
                "targetKey": "obujang",
            }),
        )
        .expect("notification should serialize");

        let mut lines = message.lines();
        assert_eq!(
            lines.next(),
            Some("DM_REPLY:42 from family-counsel: 지난주에 했어")
        );
        let context_line = lines.next().expect("context line should exist");
        assert!(lines.next().is_none());
        let context_json = context_line
            .strip_prefix("context=")
            .expect("context line should have prefix");
        let context: serde_json::Value =
            serde_json::from_str(context_json).expect("context should be valid json");
        assert_eq!(
            context,
            json!({
                "topicKey": "obujang.health_checkup",
                "targetKey": "obujang",
            })
        );
    }

    #[test]
    fn format_dm_reply_notification_keeps_empty_context_explicit() {
        let message = format_dm_reply_notification(
            7,
            "(retry)",
            "네",
            &json!({
                "_answer": "네",
                "_notify_failed": true,
                "_notify_error": "timeout",
            }),
        )
        .expect("notification should serialize");

        let mut lines = message.lines();
        assert_eq!(lines.next(), Some("DM_REPLY:7 from (retry): 네"));
        assert_eq!(lines.next(), Some("context={}"));
        assert!(lines.next().is_none());
    }

    #[tokio::test]
    async fn consume_pending_dm_reply_stores_answer_but_returns_original_context() {
        let db = crate::db::test_db();
        let reply_id = crate::services::discord::dm_reply_store::register_pending_dm_reply(
            &db,
            "family-counsel",
            "12345",
            Some("1473922824350601297"),
            r#"{"topicKey":"obujang.health_checkup","question":"건강검진 했어?"}"#,
            3_600,
        )
        .expect("insert should succeed");

        let consumed = consume_pending_dm_reply(&db, None, "12345", "지난주에 했어")
            .await
            .expect("reply should consume");

        assert_eq!(consumed.id, reply_id);
        assert_eq!(consumed.source_agent, "family-counsel");
        assert_eq!(consumed.answer, "지난주에 했어");
        assert_eq!(consumed.channel_id.as_deref(), Some("1473922824350601297"));
        assert_eq!(
            consumed.context,
            json!({
                "topicKey": "obujang.health_checkup",
                "question": "건강검진 했어?",
            })
        );

        let stored_context: String = db
            .separate_conn()
            .expect("db connection")
            .query_row(
                "SELECT context FROM pending_dm_replies WHERE id = ?1",
                libsql_rusqlite::params![reply_id],
                |row| row.get(0),
            )
            .expect("stored context should exist");
        let stored_context: serde_json::Value =
            serde_json::from_str(&stored_context).expect("stored context should be json");
        assert_eq!(stored_context["topicKey"], "obujang.health_checkup");
        assert_eq!(stored_context["question"], "건강검진 했어?");
        assert_eq!(stored_context["_answer"], "지난주에 했어");
    }
}
