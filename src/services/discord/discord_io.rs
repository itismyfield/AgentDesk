use super::*;
use crate::services::discord::outbound::{
    DeliveryResult, DiscordOutboundMessage, DiscordOutboundPolicy, FallbackKind,
    HttpOutboundClient, OutboundDeduper, deliver_outbound,
};
use poise::serenity_prelude::{CreateAttachment, CreateMessage};
use std::sync::{Arc, OnceLock};

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
    _db: Option<&crate::db::Db>,
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
    match consume_pending_dm_reply(pg_pool, &user_id_str, &answer_owned).await {
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
                pg_pool,
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
                let reply_id = info.id;
                let err_msg = format!("{e}");
                let _ = crate::services::discord_dm_reply_store::mark_pending_dm_reply_notify_failed_db(
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
    pg_pool: Option<&sqlx::PgPool>,
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
        let pg_pool = pg_pool.ok_or("postgres pool unavailable during agent lookup")?;
        let raw = crate::db::agents::resolve_agent_primary_channel_pg(pg_pool, source_agent)
            .await
            .map_err(|e| format!("agent lookup failed for {source_agent}: {e}"))?
            .ok_or("agent has no primary channel")?;
        resolve_channel_to_u64(&raw)?
    };

    let message = format_dm_reply_notification(reply_id, username, answer, context)?;
    let minimal_fallback = format!(
        "DM_REPLY:{reply_id} from {username}: [reply notification omitted because Discord rejected the full payload]"
    );
    let delivery = deliver_channel_message(
        &token,
        channel_id,
        &message,
        Some(DiscordIoDeliveryId {
            correlation_id: format!("dm_reply:{reply_id}"),
            semantic_event_id: format!("dm_reply:{reply_id}:source_notification"),
        }),
        Some(&minimal_fallback),
    )
    .await?;
    tracing::info!(
        delivery_status = delivery.status,
        fallback_kind = ?delivery.fallback_kind,
        message_id = ?delivery.message_id,
        reply_id,
        source_agent,
        "[dm-reply] source notification delivery recorded"
    );
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
pub async fn retry_failed_dm_notifications(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
) {
    let entries =
        match crate::services::discord_dm_reply_store::load_failed_consumed_dm_replies_db(pg_pool)
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
            pg_pool,
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
                let _ = crate::services::discord_dm_reply_store::clear_pending_dm_reply_notify_failure_db(
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
}

async fn consume_pending_dm_reply(
    pg_pool: Option<&sqlx::PgPool>,
    user_id: &str,
    answer: &str,
) -> Option<ConsumedDmReply> {
    let row =
        crate::services::discord_dm_reply_store::load_oldest_pending_dm_reply_db(pg_pool, user_id)
            .await
            .ok()??;

    // Merge the answer into the context JSON
    let mut context: serde_json::Value =
        serde_json::from_str(&row.context_json).unwrap_or(serde_json::json!({}));
    let notification_context = context.clone();
    context["_answer"] = serde_json::Value::String(answer.to_string());
    let updated_context = serde_json::to_string(&context).unwrap_or_default();

    // CAS: only mark consumed if still pending (guards against race)
    let updated = crate::services::discord_dm_reply_store::mark_pending_dm_reply_consumed_db(
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
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    if let Err(e) = channel_id.create_reaction(http, message_id, reaction).await {
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
    deliver_channel_message(token, channel_id, message, None, None)
        .await
        .map(|_| ())
        .map_err(Into::into)
}

/// Send a text message to a Discord user DM (called from CLI --discord-senddm)
pub async fn send_message_to_user(
    token: &str,
    user_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let dm_channel = UserId::new(user_id).create_dm_channel(&http).await?;

    deliver_channel_message(token, dm_channel.id.get(), message, None, None)
        .await
        .map(|_| ())
        .map_err(Into::into)
}

#[derive(Clone, Debug)]
struct DiscordIoDeliveryId {
    correlation_id: String,
    semantic_event_id: String,
}

#[derive(Clone, Debug)]
struct DiscordIoDeliveryReport {
    status: &'static str,
    message_id: Option<String>,
    fallback_kind: Option<&'static str>,
}

fn discord_io_deduper() -> &'static OutboundDeduper {
    static DEDUPER: OnceLock<OutboundDeduper> = OnceLock::new();
    DEDUPER.get_or_init(OutboundDeduper::new)
}

async fn deliver_channel_message(
    token: &str,
    channel_id: u64,
    message: &str,
    delivery_id: Option<DiscordIoDeliveryId>,
    minimal_fallback: Option<&str>,
) -> Result<DiscordIoDeliveryReport, String> {
    let client = HttpOutboundClient::new(
        reqwest::Client::new(),
        token.to_string(),
        crate::server::routes::dispatches::discord_delivery::discord_api_base_url(),
    );
    let mut outbound_msg = DiscordOutboundMessage::new(channel_id.to_string(), message);
    if let Some(delivery_id) = delivery_id.as_ref() {
        outbound_msg = outbound_msg
            .with_correlation(&delivery_id.correlation_id, &delivery_id.semantic_event_id);
    }
    let policy = DiscordOutboundPolicy::review_notification(
        minimal_fallback
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );

    match deliver_outbound(&client, discord_io_deduper(), outbound_msg, policy).await {
        DeliveryResult::Success { message_id } => Ok(DiscordIoDeliveryReport {
            status: "success",
            message_id: Some(message_id),
            fallback_kind: None,
        }),
        DeliveryResult::Fallback { message_id, kind } => Ok(DiscordIoDeliveryReport {
            status: "fallback",
            message_id: Some(message_id),
            fallback_kind: Some(match kind {
                FallbackKind::Truncated => "truncated",
                FallbackKind::MinimalFallback => "minimal_fallback",
            }),
        }),
        DeliveryResult::Duplicate { message_id } => Ok(DiscordIoDeliveryReport {
            status: "duplicate",
            message_id,
            fallback_kind: None,
        }),
        DeliveryResult::Skipped { .. } => Ok(DiscordIoDeliveryReport {
            status: "skip",
            message_id: None,
            fallback_kind: None,
        }),
        DeliveryResult::PermanentFailure { detail } => Err(detail),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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
}
