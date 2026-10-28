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
use crate::services::discord::http::dispatch_channel_message;
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

#[cfg(test)]
mod discord_units_tests;
mod headless_nonce;
mod resolved;
#[cfg(test)]
use resolved::send_resolved_manual_message;
pub(super) use resolved::{
    ManualNotification, ResolvedManualMessage, send_authorized_manual_message,
};
mod image_delivery;
#[cfg(test)]
pub(super) mod image_delivery_tests;
#[cfg(test)]
mod production_nonce_tests;

fn manual_delivery_log_emoji(bot: &str) -> &'static str {
    match UtilityBotRole::from_alias(bot) {
        Some(UtilityBotRole::Notify) => "🔔",
        Some(UtilityBotRole::Announce) | None => "📨",
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
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
    headless_nonce_authorized: bool,
) -> (&'static str, String) {
    send_resolved_manual_message(
        client,
        dedup,
        ResolvedManualMessage {
            channel_id_raw,
            target,
            pg_pool,
            record_transcript,
            transcript_source_label,
            notification: ManualNotification {
                channel_id: &channel_id_raw.to_string(),
                content,
                bot,
                source,
                summary,
                delivery_id,
                headless_nonce_enabled: headless_nonce::headless_discord_nonce_enabled(
                    headless_nonce_authorized,
                ),
                attachment: None,
            },
        },
    )
    .await
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
        turn_started_at_millis: None,
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

fn map_serenity_post_error(error: serenity::Error) -> DispatchMessagePostError {
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
        dispatch_channel_message(
            &self.http,
            channel_id,
            CreateMessage::new().content(content),
        )
        .await
        .map(|message| message.id.get().to_string())
        .map_err(map_serenity_post_error)
    }

    async fn post_message_with_nonce(
        &self,
        target_channel: &str,
        content: &str,
        nonce: &str,
        enforce_nonce: bool,
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
        dispatch_channel_message(
            &self.http,
            channel_id,
            CreateMessage::new()
                .content(content)
                .nonce(serenity::model::channel::Nonce::String(nonce.to_string()))
                .enforce_nonce(enforce_nonce),
        )
        .await
        .map(|message| message.id.get().to_string())
        .map_err(map_serenity_post_error)
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
    async fn post_binary_attachment(
        &self,
        target_channel: &str,
        content: &str,
        attachment: crate::services::message_outbox::OutboxAttachment<'_>,
        nonce: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let _ = (target_channel, content, attachment, nonce);
        Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            "binary attachment transport is unavailable".to_string(),
        ))
    }

    async fn post_text_attachment(
        &self,
        target_channel: &str,
        content: &str,
        summary: Option<&str>,
    ) -> Result<String, DispatchMessagePostError>;
}

impl ManualOutboundClient for SerenityManualOutboundClient {
    async fn post_binary_attachment(
        &self,
        target_channel: &str,
        content: &str,
        attachment: crate::services::message_outbox::OutboxAttachment<'_>,
        nonce: &str,
    ) -> Result<String, DispatchMessagePostError> {
        self.post_image(target_channel, content, attachment, nonce)
            .await
    }

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
        self.post_attachment(
            channel_id,
            CreateMessage::new().content(inline).add_file(attachment),
        )
        .await
    }
}

async fn deliver_manual_notification<C: ManualOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    notification: ManualNotification<'_>,
) -> ManualDeliveryOutcome {
    let ManualNotification {
        channel_id,
        content,
        bot,
        source,
        summary,
        delivery_id,
        headless_nonce_enabled,
        attachment,
    } = notification;
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

    if let Some(attachment) = attachment {
        let result = image_delivery::deliver(client, notification, attachment).await;
        record_manual_delivery_success(dedup, reservation, dedup_key.as_deref(), &result);
        return result;
    }

    let content_len = crate::services::discord::formatting::discord_message_units(content);
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
        source,
        summary,
        delivery_id,
        headless_nonce_enabled,
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

    let content_len = crate::services::discord::formatting::discord_message_units(content);
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
        "",
        summary,
        delivery_id,
        false,
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

#[allow(clippy::too_many_arguments)]
async fn deliver_manual_v3_text<C: DiscordOutboundClient>(
    client: &C,
    dedup: &OutboundDeduper,
    target: OutboundTarget,
    target_label: &str,
    bot: &str,
    content: &str,
    source: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    headless_nonce_enabled: bool,
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
    let create_nonce = delivery_id.and_then(|delivery_id| {
        headless_nonce::headless_delivery_nonce(headless_nonce_enabled, source, delivery_id)
    });
    let mut outbound_msg =
        DiscordOutboundMessage::new(correlation_id, semantic_event_id, content, target, policy);
    if let Some(nonce) = create_nonce {
        outbound_msg = outbound_msg.with_create_nonce(nonce, true);
    }
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
#[path = "manual_delivery/compatibility_tests.rs"]
mod manual_v3_delivery_tests;

impl SerenityManualOutboundClient {
    async fn post_attachment(
        &self,
        channel_id: ChannelId,
        message: CreateMessage,
    ) -> Result<String, DispatchMessagePostError> {
        dispatch_channel_message(&self.http, channel_id, message)
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
