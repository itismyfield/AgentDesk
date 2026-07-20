//! Extracted from `services::discord::health` (#3038 Phase A) — verbatim
//! move; behavior unchanged. Manual outbound delivery (channel + DM) with
//! the #2363 bot+target-scoped dedupe reservation protocol and the v3
//! outbound delegation shims.

use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateMessage};
use sqlx::PgPool;

use crate::db::session_transcripts::{PersistSessionTranscript, persist_turn_db};
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::formatting::{build_long_message_attachment, split_message};
use crate::services::discord::outbound::delivery::{
    deliver_outbound as deliver_v3_outbound, first_raw_message_id,
};
use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
use crate::services::discord::outbound::policy::DiscordOutboundPolicy;
use crate::services::discord::outbound::result::{DeliveryResult, FallbackUsed};
use crate::services::discord::outbound::{
    DISCORD_HARD_LIMIT_CHARS, DISCORD_SAFE_LIMIT_CHARS, DiscordOutboundClient, OutboundDedupClaim,
    OutboundDedupReservation, OutboundDedupWait, OutboundDeduper,
};
use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};

fn manual_delivery_log_emoji(bot: &str) -> &'static str {
    match UtilityBotRole::from_alias(bot) {
        Some(UtilityBotRole::Notify) => "🔔",
        Some(UtilityBotRole::Announce) | None => "📨",
    }
}

pub(super) async fn send_resolved_manual_message_with_client<C: ManualOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    channel_id_raw: u64,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    pg_pool: Option<&PgPool>,
    record_transcript: bool,
    transcript_source_label: Option<&str>,
) -> (&'static str, String) {
    let channel_id = ChannelId::new(channel_id_raw);
    let send_result = deliver_manual_notification(
        client,
        dedup,
        &channel_id_raw.to_string(),
        content,
        bot,
        summary,
        delivery_id,
    )
    .await;
    match send_result {
        ManualDeliveryOutcome::Sent {
            message_id,
            delivery,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let emoji = manual_delivery_log_emoji(bot);
            let delivery_tag = delivery
                .map(|value| format!(" +{value}"))
                .unwrap_or_default();
            tracing::info!(
                "  [{ts}] {emoji} ROUTE: [{source}] → channel {channel_id} (bot={bot}{delivery_tag})"
            );
            if record_transcript && !message_id.is_empty() && !content.trim().is_empty() {
                record_manual_message_transcript(
                    pg_pool,
                    channel_id_raw,
                    &message_id,
                    content,
                    transcript_source_label,
                )
                .await;
            }
            let mut response = serde_json::json!({
                "ok": true,
                "target": format!("channel:{channel_id}"),
                "channel_id": channel_id.get().to_string(),
                "message_id": message_id,
                "source": source,
                "bot": bot,
                "sent_at": chrono::Utc::now().to_rfc3339(),
            });
            if let Some(delivery) = delivery {
                response["delivery"] = serde_json::Value::String(delivery.to_string());
            }
            if target != format!("channel:{channel_id}") {
                response["requested_target"] = serde_json::Value::String(target.to_string());
            }
            ("200 OK", response.to_string())
        }
        ManualDeliveryOutcome::Failed { detail } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ ROUTE: failed to send to channel {channel_id}: {detail}");
            (
                "500 Internal Server Error",
                format!(
                    r#"{{"ok":false,"error":"Discord send failed: {}"}}"#,
                    detail
                ),
            )
        }
    }
}

fn synthetic_routine_pair<'a>(
    channel_id_raw: u64,
    message_id: &'a str,
    content: &'a str,
    source_label: Option<&str>,
) -> (String, String, &'a str) {
    let label = source_label
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("routine");
    (
        format!("manual-discord:{channel_id_raw}:{message_id}"),
        format!("(routine {label} posted)"),
        content,
    )
}

async fn record_manual_message_transcript(
    pg_pool: Option<&PgPool>,
    channel_id_raw: u64,
    message_id: &str,
    content: &str,
    source_label: Option<&str>,
) {
    let (turn_id, user_message, assistant_message) =
        synthetic_routine_pair(channel_id_raw, message_id, content, source_label);
    let channel_id = channel_id_raw.to_string();
    let source_label = source_label
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let entry = PersistSessionTranscript {
        turn_id: &turn_id,
        session_key: None,
        channel_id: Some(&channel_id),
        agent_id: source_label,
        provider: Some("routine"),
        dispatch_id: None,
        user_message: &user_message,
        assistant_message,
        events: &[],
        duration_ms: None,
    };
    if let Err(error) = persist_turn_db(pg_pool, entry).await {
        tracing::warn!(
            channel_id = channel_id_raw,
            message_id,
            error = %error,
            "manual Discord message delivered but transcript persistence failed"
        );
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ManualOutboundDeliveryId<'a> {
    pub(crate) correlation_id: &'a str,
    pub(crate) semantic_event_id: &'a str,
}

pub(super) fn is_reserved_voice_correlation_namespace(
    delivery_id: ManualOutboundDeliveryId<'_>,
) -> bool {
    delivery_id
        .correlation_id
        .trim_start()
        .starts_with("voice:")
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ManualDeliveryOutcome {
    Sent {
        message_id: String,
        delivery: Option<&'static str>,
    },
    Failed {
        detail: String,
    },
}

#[derive(Clone)]
pub(super) struct SerenityManualOutboundClient {
    pub(super) http: Arc<serenity::Http>,
}

impl DiscordOutboundClient for SerenityManualOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = target_channel
            .parse::<u64>()
            .map(ChannelId::new)
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("invalid discord channel id {target_channel}: {error}"),
                )
            })?;
        channel_id
            .send_message(&*self.http, CreateMessage::new().content(content))
            .await
            .map(|message| message.id.get().to_string())
            .map_err(|error| {
                let detail = error.to_string();
                let lowered = detail.to_ascii_lowercase();
                let kind = if detail.contains("BASE_TYPE_MAX_LENGTH")
                    || lowered.contains("2000 or fewer in length")
                    || lowered.contains("length")
                {
                    DispatchMessagePostErrorKind::MessageTooLong
                } else {
                    DispatchMessagePostErrorKind::Other
                };
                DispatchMessagePostError::new(kind, detail)
            })
    }

    async fn resolve_dm_channel(&self, user_id: &str) -> Result<String, DispatchMessagePostError> {
        let user_id = user_id
            .parse::<u64>()
            .map(serenity::UserId::new)
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("invalid Discord user id {user_id}: {error}"),
                )
            })?;
        user_id
            .create_dm_channel(&*self.http)
            .await
            .map(|channel| channel.id.get().to_string())
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("DM channel creation failed: {error}"),
                )
            })
    }
}

pub(super) trait ManualOutboundClient: DiscordOutboundClient {
    async fn post_text_attachment(
        &self,
        target_channel: &str,
        content: &str,
        summary: Option<&str>,
    ) -> Result<String, DispatchMessagePostError>;
}

impl ManualOutboundClient for SerenityManualOutboundClient {
    async fn post_text_attachment(
        &self,
        target_channel: &str,
        content: &str,
        summary: Option<&str>,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = target_channel
            .parse::<u64>()
            .map(ChannelId::new)
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("invalid discord channel id {target_channel}: {error}"),
                )
            })?;
        let (inline, attachment) = build_long_message_attachment(content, summary);
        channel_id
            .send_message(
                &*self.http,
                CreateMessage::new().content(inline).add_file(attachment),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    error.to_string(),
                )
            })
    }
}

async fn deliver_manual_notification<C: ManualOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    channel_id: &str,
    content: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
) -> ManualDeliveryOutcome {
    // Issue #2363: the manual dedupe key must include the resolved target
    // channel AND the sending `bot` identity. Voice announce delivery ids
    // encode (guild, voice_channel, utterance, generation) in
    // correlation+semantic, but the routed **target** channel and the
    // producer bot can still differ (announce vs notify), and external
    // `/api/discord/send` callers can set delivery ids freely — so without
    // bot+target scoping a notify send could poison a later announce
    // send and report "duplicate" while the announce bot never actually
    // delivered the voice transcript trigger.
    let dedup_key = delivery_id.map(|delivery_id| manual_dedup_key(bot, channel_id, delivery_id));
    let reservation = if let Some(key) = dedup_key.as_deref() {
        match reserve_manual_delivery(dedup, key).await {
            ManualDedupReservation::Duplicate(existing_message_id) => {
                return ManualDeliveryOutcome::Sent {
                    message_id: existing_message_id,
                    delivery: Some("duplicate"),
                };
            }
            ManualDedupReservation::InFlight => {
                return ManualDeliveryOutcome::Sent {
                    message_id: String::new(),
                    delivery: Some("in_flight"),
                };
            }
            ManualDedupReservation::Reserved(reservation) => Some(reservation),
        }
    } else {
        None
    };

    let content_len = content.chars().count();
    if content_len > DISCORD_HARD_LIMIT_CHARS {
        // Compatibility shim: v3 text delivery does not yet own attachment
        // upload or manual chunk-posting for over-2k `/api/discord/send` payloads.
        let result = match if UtilityBotRole::from_alias(bot)
            .is_some_and(UtilityBotRole::uses_attachment_for_oversize)
        {
            client
                .post_text_attachment(channel_id, content, summary)
                .await
                .map(|message_id| ManualDeliveryOutcome::Sent {
                    message_id,
                    delivery: Some("summary+txt"),
                })
        } else {
            deliver_chunked_manual_notification(client, channel_id, content).await
        } {
            Ok(outcome) => outcome,
            Err(error) => ManualDeliveryOutcome::Failed {
                detail: error.to_string(),
            },
        };
        if let ManualDeliveryOutcome::Sent { message_id, .. } = &result {
            if let Some(mut reservation) = reservation {
                reservation.record(message_id);
            }
        }
        return result;
    }

    let target_channel = match parse_channel_id_for_manual(channel_id) {
        Ok(channel_id) => channel_id,
        Err(outcome) => return outcome,
    };
    let result = deliver_manual_v3_text(
        client,
        dedup,
        OutboundTarget::Channel(target_channel),
        channel_id,
        bot,
        content,
        summary,
        delivery_id,
        content_len > DISCORD_SAFE_LIMIT_CHARS,
    )
    .await;
    record_manual_delivery_success(dedup, reservation, dedup_key.as_deref(), &result);
    result
}

pub(super) async fn deliver_manual_dm_notification<C: ManualOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    user_id: u64,
    content: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
) -> ManualDeliveryOutcome {
    // Issue #2363: scope the dedupe key by sending bot + DM target so the
    // same delivery id can't be silently suppressed across different
    // recipients or across producer bots.
    let dm_target_label = format!("dm:{user_id}");
    let dedup_key =
        delivery_id.map(|delivery_id| manual_dedup_key(bot, &dm_target_label, delivery_id));
    let reservation = if let Some(key) = dedup_key.as_deref() {
        match reserve_manual_delivery(dedup, key).await {
            ManualDedupReservation::Duplicate(existing_message_id) => {
                return ManualDeliveryOutcome::Sent {
                    message_id: existing_message_id,
                    delivery: Some("duplicate"),
                };
            }
            ManualDedupReservation::InFlight => {
                return ManualDeliveryOutcome::Sent {
                    message_id: String::new(),
                    delivery: Some("in_flight"),
                };
            }
            ManualDedupReservation::Reserved(reservation) => Some(reservation),
        }
    } else {
        None
    };

    let content_len = content.chars().count();
    if content_len > DISCORD_HARD_LIMIT_CHARS {
        // Compatibility shim: keep the existing attachment/chunk behavior for
        // oversize DM payloads while v3 owns the DM channel resolution.
        let dm_channel = match client.resolve_dm_channel(&user_id.to_string()).await {
            Ok(channel_id) => channel_id,
            Err(error) => {
                return ManualDeliveryOutcome::Failed {
                    detail: error.to_string(),
                };
            }
        };
        let result = match if UtilityBotRole::from_alias(bot)
            .is_some_and(UtilityBotRole::uses_attachment_for_oversize)
        {
            client
                .post_text_attachment(&dm_channel, content, summary)
                .await
                .map(|message_id| ManualDeliveryOutcome::Sent {
                    message_id,
                    delivery: Some("summary+txt"),
                })
        } else {
            deliver_chunked_manual_notification(client, &dm_channel, content).await
        } {
            Ok(outcome) => outcome,
            Err(error) => ManualDeliveryOutcome::Failed {
                detail: error.to_string(),
            },
        };
        record_manual_delivery_success(dedup, reservation, dedup_key.as_deref(), &result);
        return result;
    }

    let result = deliver_manual_v3_text(
        client,
        dedup,
        OutboundTarget::DmUser(serenity::UserId::new(user_id)),
        &format!("dm:{user_id}"),
        bot,
        content,
        summary,
        delivery_id,
        content_len > DISCORD_SAFE_LIMIT_CHARS,
    )
    .await;
    record_manual_delivery_success(dedup, reservation, dedup_key.as_deref(), &result);
    result
}

enum ManualDedupReservation {
    Reserved(OutboundDedupReservation),
    Duplicate(String),
    InFlight,
}

async fn reserve_manual_delivery(dedup: &OutboundDeduper, key: &str) -> ManualDedupReservation {
    loop {
        match dedup.reserve(key) {
            OutboundDedupClaim::Reserved(reservation) => {
                return ManualDedupReservation::Reserved(reservation);
            }
            OutboundDedupClaim::Duplicate(message_id) => {
                return ManualDedupReservation::Duplicate(message_id);
            }
            OutboundDedupClaim::InFlight(in_flight) => {
                match in_flight.wait_for_delivery(Duration::from_secs(5)).await {
                    OutboundDedupWait::Delivered(message_id) => {
                        return ManualDedupReservation::Duplicate(message_id);
                    }
                    OutboundDedupWait::Released => continue,
                    OutboundDedupWait::TimedOut => return ManualDedupReservation::InFlight,
                }
            }
        }
    }
}

async fn deliver_manual_v3_text<C: DiscordOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    target: OutboundTarget,
    target_label: &str,
    bot: &str,
    content: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    preserve_inline_content: bool,
) -> ManualDeliveryOutcome {
    let mut policy = if preserve_inline_content {
        DiscordOutboundPolicy::preserve_inline_content()
    } else {
        DiscordOutboundPolicy::review_notification()
    };
    if delivery_id.is_none() {
        policy = policy.without_idempotency();
    }
    let (correlation_id, semantic_event_id) = delivery_id
        .map(|delivery_id| {
            // Issue #2363: prefix the v3 correlation_id with the bot
            // identity so structurally-equal external delivery ids cannot
            // poison sends across different producer bots. The v3 dedup
            // key already includes target, but not bot, and external
            // `/api/discord/send` callers can supply arbitrary
            // (correlation_id, semantic_event_id) pairs.
            (
                format!("bot:{bot}::{}", delivery_id.correlation_id),
                delivery_id.semantic_event_id.to_string(),
            )
        })
        .unwrap_or_else(|| {
            (
                format!("manual:no-idempotency:bot:{bot}:{target_label}"),
                "manual:no-idempotency".to_string(),
            )
        });
    let mut outbound_msg =
        DiscordOutboundMessage::new(correlation_id, semantic_event_id, content, target, policy);
    if let Some(summary) = summary.map(str::trim).filter(|value| !value.is_empty()) {
        outbound_msg = outbound_msg.with_summary(summary.to_string());
    }

    match deliver_v3_outbound(client, dedup, outbound_msg, None).await {
        DeliveryResult::Sent { messages, .. } => ManualDeliveryOutcome::Sent {
            message_id: first_raw_message_id(&messages).unwrap_or_default(),
            delivery: None,
        },
        DeliveryResult::Fallback {
            messages,
            fallback_used,
            ..
        } => ManualDeliveryOutcome::Sent {
            message_id: first_raw_message_id(&messages).unwrap_or_default(),
            delivery: Some(match fallback_used {
                FallbackUsed::LengthCompacted => "truncated",
                FallbackUsed::MinimalFallback => "minimal_fallback",
                FallbackUsed::LengthSplit => "chunked",
                FallbackUsed::FileAttachment => "summary+txt",
                FallbackUsed::ParentChannel => "parent_channel",
            }),
        },
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => ManualDeliveryOutcome::Sent {
            // Issue #2363: surface the prior message id so retry callers
            // (e.g. announce-bot transcript driver) don't fail on an empty
            // numeric body when the v3 layer dedupes structurally.
            message_id: first_raw_message_id(&existing_messages).unwrap_or_default(),
            delivery: Some("duplicate"),
        },
        DeliveryResult::Skip { .. } => ManualDeliveryOutcome::Sent {
            message_id: String::new(),
            delivery: Some("skipped"),
        },
        DeliveryResult::TransientFailure { reason }
        | DeliveryResult::PermanentFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason } => {
            ManualDeliveryOutcome::Failed { detail: reason }
        }
    }
}

fn record_manual_delivery_success(
    dedup: &OutboundDeduper,
    reservation: Option<OutboundDedupReservation>,
    dedup_key: Option<&str>,
    result: &ManualDeliveryOutcome,
) {
    let mut reservation = reservation;
    let ManualDeliveryOutcome::Sent {
        message_id,
        delivery,
    } = result
    else {
        return;
    };
    if message_id.is_empty() {
        return;
    }
    // Don't overwrite the stored entry when we're just replaying a known
    // duplicate — `dedup.record` would re-insert the same id but at a
    // refreshed timestamp on backends that gain TTLs later.
    if matches!(delivery, Some("duplicate")) {
        if let Some(reservation) = reservation.as_mut() {
            reservation.record(message_id);
        }
        return;
    }
    if let Some(key) = dedup_key {
        if let Some(reservation) = reservation.as_mut() {
            reservation.record(message_id);
        } else {
            dedup.record(key, message_id);
        }
    }
}

/// Build a manual-delivery dedupe key scoped to the producer bot and the
/// resolved target so that the same (correlation_id, semantic_event_id)
/// cannot collide across different Discord channels, DM recipients, or
/// producer bots. External callers of `/api/discord/send` may supply
/// arbitrary `correlation_id` / `semantic_event_id`; scoping by `bot`
/// blocks a notify-bot send from poisoning a later announce-bot send to
/// the same target.
fn manual_dedup_key(
    bot: &str,
    target_label: &str,
    delivery_id: ManualOutboundDeliveryId<'_>,
) -> String {
    format!(
        "manual::{}::{}::{}::{}",
        bot, target_label, delivery_id.correlation_id, delivery_id.semantic_event_id
    )
}

fn parse_channel_id_for_manual(channel_id: &str) -> Result<ChannelId, ManualDeliveryOutcome> {
    channel_id
        .parse::<u64>()
        .map(ChannelId::new)
        .map_err(|error| ManualDeliveryOutcome::Failed {
            detail: format!("invalid discord channel id {channel_id}: {error}"),
        })
}

async fn deliver_chunked_manual_notification<C: ManualOutboundClient>(
    client: &C,
    channel_id: &str,
    content: &str,
) -> Result<ManualDeliveryOutcome, DispatchMessagePostError> {
    let mut last_message_id = None;
    for chunk in split_message(content) {
        let message_id = client.post_message(channel_id, &chunk).await?;
        last_message_id = Some(message_id);
    }
    Ok(ManualDeliveryOutcome::Sent {
        message_id: last_message_id.unwrap_or_default(),
        delivery: Some("chunked"),
    })
}

#[cfg(test)]
mod manual_v3_delivery_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::services::discord::health::{HealthRegistry, handle_send};

    #[test]
    fn synthetic_routine_pair_is_complete_and_message_id_deterministic() {
        let first =
            synthetic_routine_pair(42, "message-7", "posted briefing", Some("morning-briefing"));
        let retry =
            synthetic_routine_pair(42, "message-7", "posted briefing", Some("morning-briefing"));

        assert_eq!(first.0, "manual-discord:42:message-7");
        assert_eq!(first.0, retry.0);
        assert_eq!(first.1, "(routine morning-briefing posted)");
        assert!(!first.1.trim().is_empty());
        assert_eq!(first.2, "posted briefing");
        assert!(!first.2.trim().is_empty());
    }

    #[test]
    fn synthetic_routine_pair_uses_non_empty_default_label() {
        let pair = synthetic_routine_pair(42, "message-8", "body", Some("  "));
        assert_eq!(pair.1, "(routine routine posted)");
    }

    #[test]
    fn manual_delivery_log_emoji_preserves_legacy_mapping() {
        assert_eq!(
            manual_delivery_log_emoji(UtilityBotRole::Announce.alias()),
            "📨"
        );
        assert_eq!(
            manual_delivery_log_emoji(UtilityBotRole::Notify.alias()),
            "🔔"
        );
        assert_eq!(manual_delivery_log_emoji("provider"), "📨");
    }

    #[derive(Clone, Default)]
    struct MockManualOutboundClient {
        posts: Arc<Mutex<Vec<String>>>,
        post_targets: Arc<Mutex<Vec<String>>>,
        dm_resolutions: Arc<Mutex<Vec<String>>>,
    }

    impl DiscordOutboundClient for MockManualOutboundClient {
        async fn post_message(
            &self,
            target_channel: &str,
            content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            let mut posts = self.posts.lock().unwrap();
            self.post_targets
                .lock()
                .unwrap()
                .push(target_channel.to_string());
            posts.push(content.to_string());
            Ok(format!("message-{}", posts.len()))
        }

        async fn resolve_dm_channel(
            &self,
            user_id: &str,
        ) -> Result<String, DispatchMessagePostError> {
            self.dm_resolutions
                .lock()
                .unwrap()
                .push(user_id.to_string());
            Ok("9876".to_string())
        }
    }

    impl ManualOutboundClient for MockManualOutboundClient {
        async fn post_text_attachment(
            &self,
            _target_channel: &str,
            _content: &str,
            _summary: Option<&str>,
        ) -> Result<String, DispatchMessagePostError> {
            Ok("attachment-message-1".to_string())
        }
    }

    #[tokio::test]
    async fn manual_dm_notification_uses_v3_dm_target_and_dedupes_before_resolve() {
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: "senddm:42",
            semantic_event_id: "senddm:42:hello",
        };

        let first = deliver_manual_dm_notification(
            &client,
            &dedup,
            42,
            "hello",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let second = deliver_manual_dm_notification(
            &client,
            &dedup,
            42,
            "hello",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        assert_eq!(
            first,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None
            }
        );
        assert_eq!(
            second,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: Some("duplicate")
            }
        );
        assert_eq!(
            client.dm_resolutions.lock().unwrap().clone(),
            vec!["42".to_string()]
        );
        assert_eq!(
            client.post_targets.lock().unwrap().clone(),
            vec!["9876".to_string()]
        );
        assert_eq!(
            client.posts.lock().unwrap().clone(),
            vec!["hello".to_string()]
        );
    }

    #[tokio::test]
    async fn voice_announce_same_utterance_and_generation_dedupes_at_health_layer() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let voice_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-2363-a",
            default_voice_announce_generation(),
        );
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: &voice_id.correlation_id,
            semantic_event_id: &voice_id.semantic_event_id,
        };

        // Issue #2363: announce send retried with identical
        // (guild, voice_channel, utterance, generation) must hit the dedupe
        // path and not produce a second Discord call.
        let first = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        assert_eq!(
            first,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None,
            }
        );
        assert_eq!(
            second,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: Some("duplicate"),
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 1);
        assert_eq!(
            voice_id.correlation_id,
            "voice:7001:8002:utt-2363-a".to_string()
        );
        assert_eq!(
            voice_id.semantic_event_id,
            "announce:generation:1".to_string()
        );
    }

    #[tokio::test]
    async fn voice_announce_new_utterance_does_not_dedupe_against_prior_send() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let first_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-A",
            default_voice_announce_generation(),
        );
        let second_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-B",
            default_voice_announce_generation(),
        );

        let first = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "first transcript",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &first_id.correlation_id,
                semantic_event_id: &first_id.semantic_event_id,
            }),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "second transcript",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &second_id.correlation_id,
                semantic_event_id: &second_id.semantic_event_id,
            }),
        )
        .await;

        assert_eq!(
            first,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None,
            }
        );
        assert_eq!(
            second,
            ManualDeliveryOutcome::Sent {
                message_id: "message-2".to_string(),
                delivery: None,
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn voice_announce_generation_bump_breaks_dedupe_for_same_utterance() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Same (guild, voice_channel, utterance) but a higher generation
        // (e.g. a barge-in follow-up) must NOT dedupe against the original
        // announce — different `semantic_event_id = announce:generation:{n}`.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let gen_one = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-shared",
            default_voice_announce_generation(),
        );
        let gen_two = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-shared",
            default_voice_announce_generation() + 1,
        );
        assert_eq!(gen_one.correlation_id, gen_two.correlation_id);
        assert_ne!(gen_one.semantic_event_id, gen_two.semantic_event_id);

        let first = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "transcript",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &gen_one.correlation_id,
                semantic_event_id: &gen_one.semantic_event_id,
            }),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "transcript with barge-in follow-up",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &gen_two.correlation_id,
                semantic_event_id: &gen_two.semantic_event_id,
            }),
        )
        .await;

        assert!(matches!(
            first,
            ManualDeliveryOutcome::Sent { delivery: None, .. }
        ));
        assert!(matches!(
            second,
            ManualDeliveryOutcome::Sent { delivery: None, .. }
        ));
        assert_eq!(client.posts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn voice_announce_cross_guild_isolation_prevents_dedupe_collision() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Two different guilds happen to use the same utterance_id (they're
        // independent generators) — must NOT dedupe against each other.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let guild_a = voice_announce_delivery_id(
            GuildId::new(1),
            ChannelId::new(10),
            "utt-collide",
            default_voice_announce_generation(),
        );
        let guild_b = voice_announce_delivery_id(
            GuildId::new(2),
            ChannelId::new(10),
            "utt-collide",
            default_voice_announce_generation(),
        );
        assert_ne!(guild_a.correlation_id, guild_b.correlation_id);

        let first = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "from guild 1",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &guild_a.correlation_id,
                semantic_event_id: &guild_a.semantic_event_id,
            }),
        )
        .await;
        let second = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "from guild 2",
            "announce",
            None,
            Some(ManualOutboundDeliveryId {
                correlation_id: &guild_b.correlation_id,
                semantic_event_id: &guild_b.semantic_event_id,
            }),
        )
        .await;

        assert!(matches!(
            first,
            ManualDeliveryOutcome::Sent { delivery: None, .. }
        ));
        assert!(matches!(
            second,
            ManualDeliveryOutcome::Sent { delivery: None, .. }
        ));
        assert_eq!(client.posts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn voice_announce_same_delivery_id_different_target_channel_does_not_dedupe() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Issue #2363 (Codex high-severity finding): the dedupe key must
        // include the resolved target channel. Otherwise an announce queued
        // first to a transcript channel and later re-routed to a different
        // target channel for the same (guild, voice_channel, utterance,
        // generation) tuple would be silently suppressed as a duplicate.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let voice_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-cross-target",
            default_voice_announce_generation(),
        );
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: &voice_id.correlation_id,
            semantic_event_id: &voice_id.semantic_event_id,
        };

        let to_channel_a = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let to_channel_b = deliver_manual_notification(
            &client,
            &dedup,
            "9001",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        assert_eq!(
            to_channel_a,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None,
            }
        );
        assert_eq!(
            to_channel_b,
            ManualDeliveryOutcome::Sent {
                message_id: "message-2".to_string(),
                delivery: None,
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 2);
        assert_eq!(
            client.post_targets.lock().unwrap().clone(),
            vec!["9000".to_string(), "9001".to_string()]
        );
    }

    #[tokio::test]
    async fn voice_announce_different_bot_does_not_dedupe_against_announce() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Issue #2363 (Codex high-severity finding round 2): scoping must
        // also include the producer `bot`. Without this an external
        // `/api/discord/send` caller could send through `notify` with a
        // crafted `voice:{guild}:{voice_channel}:{utterance}` delivery id
        // and silently poison the voice announce path.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let voice_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-cross-bot",
            default_voice_announce_generation(),
        );
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: &voice_id.correlation_id,
            semantic_event_id: &voice_id.semantic_event_id,
        };

        let from_notify = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "notify payload",
            "notify",
            None,
            Some(delivery_id),
        )
        .await;
        let from_announce = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        assert_eq!(
            from_notify,
            ManualDeliveryOutcome::Sent {
                message_id: "message-1".to_string(),
                delivery: None,
            }
        );
        assert_eq!(
            from_announce,
            ManualDeliveryOutcome::Sent {
                message_id: "message-2".to_string(),
                delivery: None,
            }
        );
        assert_eq!(client.posts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn api_send_rejects_user_supplied_voice_delivery_id_namespace() {
        let registry = HealthRegistry::new();

        let (status, body) = handle_send(
            &registry,
            None,
            r#"{
                "target": "channel:9000",
                "content": "forged voice transcript",
                "source": "system",
                "bot": "announce",
                "correlation_id": "voice:7001:8002:utt-forged",
                "semantic_event_id": "announce:generation:1"
            }"#,
        )
        .await;
        let response: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "400 Bad Request");
        assert_eq!(response["ok"], false);
        assert_eq!(
            response["error"],
            "delivery_id correlation namespace is reserved"
        );
    }

    #[tokio::test]
    async fn voice_announce_duplicate_surfaces_prior_message_id_not_empty() {
        use crate::services::discord::voice_background_driver::{
            default_voice_announce_generation, voice_announce_delivery_id,
        };
        use poise::serenity_prelude::{ChannelId, GuildId};

        // Issue #2363 (Codex high-severity finding): the duplicate path must
        // return the prior delivered message id. The announce-bot driver
        // (`AnnounceBotTranscriptDriver::start`) parses `message_id` from
        // the response body and errors out on empty / non-numeric values.
        let client = MockManualOutboundClient::default();
        let dedup = OutboundDeduper::new();
        let voice_id = voice_announce_delivery_id(
            GuildId::new(7001),
            ChannelId::new(8002),
            "utt-known-id",
            default_voice_announce_generation(),
        );
        let delivery_id = ManualOutboundDeliveryId {
            correlation_id: &voice_id.correlation_id,
            semantic_event_id: &voice_id.semantic_event_id,
        };

        let _ = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;
        let dup = deliver_manual_notification(
            &client,
            &dedup,
            "9000",
            "voice transcript",
            "announce",
            None,
            Some(delivery_id),
        )
        .await;

        match dup {
            ManualDeliveryOutcome::Sent {
                message_id,
                delivery,
            } => {
                assert!(
                    !message_id.is_empty(),
                    "duplicate must return prior message id, not empty"
                );
                assert!(
                    message_id
                        .chars()
                        .all(|c| c == '-' || c.is_ascii_alphanumeric()),
                    "message_id should be parseable by callers"
                );
                assert_eq!(delivery, Some("duplicate"));
            }
            other => panic!("expected Sent(duplicate), got {other:?}"),
        }
    }
}
