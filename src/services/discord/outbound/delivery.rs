//! v3 Discord outbound delivery implementation.
//!
//! This module consumes the v3 envelope (`message.rs`), policy (`policy.rs`),
//! planner (`decision.rs`), and result (`result.rs`) types. The transport
//! trait and in-process deduper still live in `legacy.rs` during the migration
//! so existing production clients do not need to be rewritten in the same
//! slice.

use std::borrow::Cow;
use std::time::Duration;

use poise::serenity_prelude::ChannelId;

use crate::server::routes::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};
use crate::services::provider::{CancelToken, cancel_requested};

use super::decision::{
    LengthPolicyDecision, OutboundPolicyLimits, PrimaryDeliveryTarget, ThreadFallbackDecision,
    decide_policy_with_limits,
};
use super::legacy::{
    DiscordOutboundClient, OutboundDedupClaim, OutboundDedupReservation, OutboundDedupWait,
    OutboundDeduper,
};
use super::message::{
    DiscordOutboundMessage, OutboundDedupKey, OutboundOperation, OutboundReferenceContext,
};
use super::result::{DeliveredMessage, DeliveryResult, FallbackUsed};

#[derive(Clone, Debug, Default)]
pub(crate) struct DeliveryTransportOverrides {
    pub(crate) target_channel: Option<String>,
    pub(crate) edit_message_id: Option<String>,
    pub(crate) reference: Option<DeliveryReferenceOverride>,
    pub(crate) limits: Option<OutboundPolicyLimits>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeliveryReferenceOverride {
    pub(crate) channel_id: String,
    pub(crate) message_id: String,
}

pub(crate) async fn deliver_outbound<C>(
    client: &C,
    dedup: &OutboundDeduper,
    message: DiscordOutboundMessage,
    cancel_token: Option<&CancelToken>,
) -> DeliveryResult
where
    C: DiscordOutboundClient,
{
    deliver_outbound_with_overrides(
        client,
        dedup,
        message,
        DeliveryTransportOverrides::default(),
        cancel_token,
    )
    .await
}

pub(crate) async fn deliver_outbound_with_overrides<C>(
    client: &C,
    dedup: &OutboundDeduper,
    message: DiscordOutboundMessage,
    overrides: DeliveryTransportOverrides,
    cancel_token: Option<&CancelToken>,
) -> DeliveryResult
where
    C: DiscordOutboundClient,
{
    if let Some(result) = cancelled_delivery_result(cancel_token) {
        return result;
    }

    let limits = overrides.limits.unwrap_or_default();
    let decision = decide_policy_with_limits(&message, limits);
    let dedup_key = decision.dedup_key.clone();
    let store_key = dedup_store_key(&dedup_key);

    let (stored_duplicate, reservation) = if message.policy.idempotency_window > Duration::ZERO {
        loop {
            match dedup.reserve(&store_key) {
                OutboundDedupClaim::Duplicate(stored) => break (Some(stored), None),
                OutboundDedupClaim::Reserved(reservation) => break (None, Some(reservation)),
                OutboundDedupClaim::InFlight(in_flight) => {
                    if let Some(result) = cancelled_delivery_result(cancel_token) {
                        return result;
                    }
                    match in_flight.wait_for_delivery(Duration::from_secs(5)).await {
                        OutboundDedupWait::Delivered(stored) => {
                            if let Some(messages) = decode_stored_delivered_messages(&stored) {
                                return DeliveryResult::Duplicate {
                                    dedup_key,
                                    existing_messages: messages,
                                };
                            }
                            break (Some(stored), None);
                        }
                        OutboundDedupWait::Released => continue,
                        OutboundDedupWait::TimedOut => {
                            if let Some(result) = cancelled_delivery_result(cancel_token) {
                                return result;
                            }
                            return DeliveryResult::Skip {
                                reason: "outbound delivery already in flight".into(),
                            };
                        }
                    }
                }
            }
        }
    } else {
        (None, None)
    };
    if let Some(stored) = stored_duplicate.as_deref() {
        if let Some(messages) = decode_stored_delivered_messages(stored) {
            return DeliveryResult::Duplicate {
                dedup_key,
                existing_messages: messages,
            };
        }
    }

    if let Some(result) = cancelled_delivery_result(cancel_token) {
        return result;
    }

    let (target_channel, delivered_channel_id) =
        match resolve_primary_delivery_target(client, &decision.primary_target, &overrides).await {
            Ok(target) => target,
            Err(reason) => return DeliveryResult::PermanentFailure { reason },
        };

    if let Some(result) = cancelled_delivery_result(cancel_token) {
        return result;
    }

    if let Some(stored) = stored_duplicate {
        return DeliveryResult::Duplicate {
            dedup_key,
            existing_messages: decode_legacy_delivered_messages(&stored, delivered_channel_id),
        };
    }

    match decision.length {
        LengthPolicyDecision::Inline { .. } => {
            deliver_single(
                client,
                dedup,
                &message,
                &overrides,
                &dedup_key,
                &target_channel,
                delivered_channel_id,
                &message.content,
                None,
                decision.thread_fallback,
                limits,
                reservation,
                cancel_token,
            )
            .await
        }
        LengthPolicyDecision::Compact { .. } => {
            let (primary, truncated) =
                truncate_with_marker(&message.content, limits.compact_char_limit);
            deliver_single(
                client,
                dedup,
                &message,
                &overrides,
                &dedup_key,
                &target_channel,
                delivered_channel_id,
                &primary,
                truncated.then_some(FallbackUsed::LengthCompacted),
                decision.thread_fallback,
                limits,
                reservation,
                cancel_token,
            )
            .await
        }
        LengthPolicyDecision::Split {
            chunk_char_limit, ..
        } => {
            deliver_split(
                client,
                dedup,
                &message,
                &overrides,
                &dedup_key,
                &target_channel,
                delivered_channel_id,
                chunk_char_limit,
                reservation,
                cancel_token,
            )
            .await
        }
        LengthPolicyDecision::FileAttachment { .. } => DeliveryResult::PermanentFailure {
            reason: "v3 file-attachment delivery requires an attachment-capable transport".into(),
        },
        LengthPolicyDecision::RejectOverLimit {
            char_count,
            inline_char_limit,
        } => DeliveryResult::PermanentFailure {
            reason: format!(
                "content length {char_count} exceeds inline limit {inline_char_limit} (RejectOverLimit)"
            ),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn deliver_single<C>(
    client: &C,
    dedup: &OutboundDeduper,
    message: &DiscordOutboundMessage,
    overrides: &DeliveryTransportOverrides,
    dedup_key: &OutboundDedupKey,
    target_channel: &str,
    delivered_channel_id: ChannelId,
    content: &str,
    fallback_used: Option<FallbackUsed>,
    thread_fallback: ThreadFallbackDecision,
    limits: OutboundPolicyLimits,
    reservation: Option<OutboundDedupReservation>,
    cancel_token: Option<&CancelToken>,
) -> DeliveryResult
where
    C: DiscordOutboundClient,
{
    let mut reservation = reservation;
    match send_content(
        client,
        message,
        overrides,
        target_channel,
        content,
        cancel_token,
    )
    .await
    {
        Ok(raw_message_id) => {
            let messages = vec![DeliveredMessage::single_raw(
                delivered_channel_id,
                raw_message_id,
            )];
            record_success(dedup, reservation.as_mut(), dedup_key, message, &messages);
            delivery_success(dedup_key.clone(), messages, fallback_used)
        }
        Err(error) => {
            if let Some(result) = cancelled_delivery_result(cancel_token) {
                return result;
            }
            if error.kind() == DispatchMessagePostErrorKind::MessageTooLong {
                if let Some(result) = retry_minimal_fallback(
                    client,
                    dedup,
                    message,
                    overrides,
                    dedup_key,
                    target_channel,
                    delivered_channel_id,
                    limits,
                    reservation.as_mut(),
                    cancel_token,
                )
                .await
                {
                    return result;
                }
            }

            if let ThreadFallbackDecision::RetryParent { parent, .. } = thread_fallback {
                let parent_target = parent.get().to_string();
                match send_content(
                    client,
                    message,
                    overrides,
                    &parent_target,
                    content,
                    cancel_token,
                )
                .await
                {
                    Ok(raw_message_id) => {
                        let messages = vec![DeliveredMessage::single_raw(parent, raw_message_id)];
                        record_success(dedup, reservation.as_mut(), dedup_key, message, &messages);
                        return DeliveryResult::Fallback {
                            dedup_key: dedup_key.clone(),
                            messages,
                            reason: error.to_string(),
                            fallback_used: FallbackUsed::ParentChannel,
                        };
                    }
                    Err(parent_error) => {
                        if let Some(result) = cancelled_delivery_result(cancel_token) {
                            return result;
                        }
                        return DeliveryResult::PermanentFailure {
                            reason: parent_error.to_string(),
                        };
                    }
                }
            }

            DeliveryResult::PermanentFailure {
                reason: error.to_string(),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn retry_minimal_fallback<C>(
    client: &C,
    dedup: &OutboundDeduper,
    message: &DiscordOutboundMessage,
    overrides: &DeliveryTransportOverrides,
    dedup_key: &OutboundDedupKey,
    target_channel: &str,
    delivered_channel_id: ChannelId,
    limits: OutboundPolicyLimits,
    reservation: Option<&mut OutboundDedupReservation>,
    cancel_token: Option<&CancelToken>,
) -> Option<DeliveryResult>
where
    C: DiscordOutboundClient,
{
    let mut reservation = reservation;
    let summary = message.summary.as_ref()?.content.trim();
    if summary.is_empty() {
        return None;
    }
    let (minimal, _) = truncate_with_marker(summary, limits.compact_char_limit);
    Some(
        match send_content(
            client,
            message,
            overrides,
            target_channel,
            &minimal,
            cancel_token,
        )
        .await
        {
            Ok(raw_message_id) => {
                let messages = vec![DeliveredMessage::single_raw(
                    delivered_channel_id,
                    raw_message_id,
                )];
                record_success(
                    dedup,
                    reservation.as_mut().map(|reservation| &mut **reservation),
                    dedup_key,
                    message,
                    &messages,
                );
                DeliveryResult::Fallback {
                    dedup_key: dedup_key.clone(),
                    messages,
                    reason: "primary delivery hit Discord length limit; minimal fallback posted"
                        .into(),
                    fallback_used: FallbackUsed::MinimalFallback,
                }
            }
            Err(error) => {
                if let Some(result) = cancelled_delivery_result(cancel_token) {
                    return Some(result);
                }
                DeliveryResult::PermanentFailure {
                    reason: error.to_string(),
                }
            }
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn deliver_split<C>(
    client: &C,
    dedup: &OutboundDeduper,
    message: &DiscordOutboundMessage,
    overrides: &DeliveryTransportOverrides,
    dedup_key: &OutboundDedupKey,
    target_channel: &str,
    delivered_channel_id: ChannelId,
    chunk_char_limit: usize,
    reservation: Option<OutboundDedupReservation>,
    cancel_token: Option<&CancelToken>,
) -> DeliveryResult
where
    C: DiscordOutboundClient,
{
    let mut reservation = reservation;
    if matches!(message.operation, OutboundOperation::Edit { .. }) {
        return DeliveryResult::PermanentFailure {
            reason: "split length strategy cannot edit one message into multiple chunks".into(),
        };
    }

    let chunks = split_content(&message.content, chunk_char_limit);
    let chunk_count = chunks.len();
    let mut messages = Vec::with_capacity(chunk_count);
    for (index, chunk) in chunks.iter().enumerate() {
        let raw_message_id = match send_content(
            client,
            message,
            overrides,
            target_channel,
            chunk,
            cancel_token,
        )
        .await
        {
            Ok(message_id) => message_id,
            Err(error) => {
                if let Some(result) = cancelled_delivery_result(cancel_token) {
                    return result;
                }
                return DeliveryResult::PermanentFailure {
                    reason: error.to_string(),
                };
            }
        };
        messages.push(DeliveredMessage::chunk_raw(
            delivered_channel_id,
            raw_message_id,
            index,
            chunk_count,
        ));
    }

    record_success(dedup, reservation.as_mut(), dedup_key, message, &messages);
    DeliveryResult::Fallback {
        dedup_key: dedup_key.clone(),
        messages,
        reason: "payload split across multiple Discord messages".into(),
        fallback_used: FallbackUsed::LengthSplit,
    }
}

async fn send_content<C>(
    client: &C,
    message: &DiscordOutboundMessage,
    overrides: &DeliveryTransportOverrides,
    target_channel: &str,
    content: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<String, DispatchMessagePostError>
where
    C: DiscordOutboundClient,
{
    if cancel_requested(cancel_token) {
        return Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            "outbound delivery cancelled".into(),
        ));
    }
    match message.operation {
        OutboundOperation::Edit { message_id } => {
            let edit_message_id = overrides
                .edit_message_id
                .as_deref()
                .map(Cow::Borrowed)
                .unwrap_or_else(|| Cow::Owned(message_id.get().to_string()));
            client
                .edit_message(target_channel, edit_message_id.as_ref(), content)
                .await
        }
        OutboundOperation::Send => {
            if let Some(reference) = resolve_reference(message.reference.as_ref(), overrides) {
                client
                    .post_message_with_reference(
                        target_channel,
                        content,
                        &reference.channel_id,
                        &reference.message_id,
                    )
                    .await
            } else {
                client.post_message(target_channel, content).await
            }
        }
    }
}

fn cancelled_delivery_result(cancel_token: Option<&CancelToken>) -> Option<DeliveryResult> {
    cancel_requested(cancel_token).then(|| DeliveryResult::Skip {
        reason: "cancelled".into(),
    })
}

fn resolve_reference(
    reference: Option<&OutboundReferenceContext>,
    overrides: &DeliveryTransportOverrides,
) -> Option<DeliveryReferenceOverride> {
    if let Some(reference) = overrides.reference.clone() {
        return Some(reference);
    }
    reference.and_then(|reference| {
        reference.message.map(|message| DeliveryReferenceOverride {
            channel_id: message.channel_id.get().to_string(),
            message_id: message.message_id.get().to_string(),
        })
    })
}

async fn resolve_primary_delivery_target<C>(
    client: &C,
    primary_target: &PrimaryDeliveryTarget,
    overrides: &DeliveryTransportOverrides,
) -> Result<(String, ChannelId), String>
where
    C: DiscordOutboundClient,
{
    if let Some(target) = overrides.target_channel.as_ref() {
        return Ok((target.clone(), parse_channel_id_lossy(target)));
    }
    match primary_target {
        PrimaryDeliveryTarget::Channel(channel_id) => {
            Ok((channel_id.get().to_string(), *channel_id))
        }
        PrimaryDeliveryTarget::DmUser(user_id) => {
            let target_channel = client
                .resolve_dm_channel(&user_id.get().to_string())
                .await
                .map_err(|error| error.to_string())?;
            Ok((
                target_channel.clone(),
                parse_channel_id_lossy(&target_channel),
            ))
        }
    }
}

fn delivery_success(
    dedup_key: OutboundDedupKey,
    messages: Vec<DeliveredMessage>,
    fallback_used: Option<FallbackUsed>,
) -> DeliveryResult {
    match fallback_used {
        Some(fallback_used) => DeliveryResult::Fallback {
            dedup_key,
            messages,
            reason: format!("{fallback_used:?}"),
            fallback_used,
        },
        None => DeliveryResult::Sent {
            dedup_key,
            messages,
        },
    }
}

fn record_success(
    dedup: &OutboundDeduper,
    reservation: Option<&mut OutboundDedupReservation>,
    dedup_key: &OutboundDedupKey,
    message: &DiscordOutboundMessage,
    messages: &[DeliveredMessage],
) {
    if message.policy.idempotency_window <= Duration::ZERO {
        return;
    }
    let encoded = encode_delivered_messages(messages);
    if let Some(reservation) = reservation {
        reservation.record(&encoded);
    } else {
        dedup.record(&dedup_store_key(dedup_key), &encoded);
    }
}

fn dedup_store_key(dedup_key: &OutboundDedupKey) -> String {
    serde_json::to_string(dedup_key).unwrap_or_else(|_| format!("{dedup_key:?}"))
}

fn encode_delivered_messages(messages: &[DeliveredMessage]) -> String {
    serde_json::to_string(messages).unwrap_or_default()
}

fn decode_stored_delivered_messages(stored: &str) -> Option<Vec<DeliveredMessage>> {
    let messages = serde_json::from_str::<Vec<DeliveredMessage>>(stored).ok()?;
    (!messages.is_empty()).then_some(messages)
}

fn decode_legacy_delivered_messages(stored: &str, channel_id: ChannelId) -> Vec<DeliveredMessage> {
    match serde_json::from_str::<Vec<String>>(stored) {
        Ok(raw_ids) if !raw_ids.is_empty() => raw_ids
            .into_iter()
            .map(|raw| DeliveredMessage::single_raw(channel_id, raw))
            .collect(),
        Ok(_) => Vec::new(),
        Err(_) => vec![DeliveredMessage::single_raw(channel_id, stored)],
    }
}

fn parse_channel_id_lossy(raw: &str) -> ChannelId {
    raw.parse::<u64>()
        .map(ChannelId::new)
        .unwrap_or_else(|_| ChannelId::new(1))
}

pub(crate) fn first_raw_message_id(messages: &[DeliveredMessage]) -> Option<String> {
    messages
        .first()
        .map(|message| message.raw_message_id.clone())
}

/// Truncate `content` to at most `max_chars` characters, appending a truncation
/// marker on a new paragraph when truncation occurred.
pub(crate) fn truncate_with_marker(content: &str, max_chars: usize) -> (String, bool) {
    if content.chars().count() <= max_chars {
        return (content.to_string(), false);
    }
    let boundary: usize = content
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    let cut = content[..boundary].rfind('\n').unwrap_or(boundary);
    (format!("{}\n\n[… truncated]", &content[..cut]), true)
}

fn split_content(content: &str, chunk_limit: usize) -> Vec<String> {
    let chunk_limit = chunk_limit.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in content.chars() {
        if current.chars().count() >= chunk_limit {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
    use crate::services::discord::outbound::policy::DiscordOutboundPolicy;
    use crate::services::provider::CancelToken;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockClient {
        posts: Arc<Mutex<Vec<(String, String)>>>,
        dm_resolutions: Arc<Mutex<Vec<String>>>,
        length_failures_remaining: Arc<Mutex<usize>>,
        send_failures_remaining: Arc<Mutex<usize>>,
        post_delay_ms: Arc<AtomicU64>,
        cancel_after_post_count: Arc<Mutex<Option<(usize, Arc<CancelToken>)>>>,
    }

    impl MockClient {
        fn set_post_delay_ms(&self, delay_ms: u64) {
            self.post_delay_ms.store(delay_ms, Ordering::SeqCst);
        }

        fn fail_next_with_length(&self) {
            *self.length_failures_remaining.lock().unwrap() += 1;
        }

        fn fail_next_send(&self) {
            *self.send_failures_remaining.lock().unwrap() += 1;
        }

        fn cancel_after_post_count(&self, post_count: usize, token: Arc<CancelToken>) {
            *self.cancel_after_post_count.lock().unwrap() = Some((post_count, token));
        }

        fn posts(&self) -> Vec<(String, String)> {
            self.posts.lock().unwrap().clone()
        }

        fn dm_resolutions(&self) -> Vec<String> {
            self.dm_resolutions.lock().unwrap().clone()
        }
    }

    impl DiscordOutboundClient for MockClient {
        async fn post_message(
            &self,
            target_channel: &str,
            content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            let post_count = {
                let mut posts = self.posts.lock().unwrap();
                posts.push((target_channel.to_string(), content.to_string()));
                posts.len()
            };
            if let Some((threshold, token)) = self.cancel_after_post_count.lock().unwrap().as_ref()
                && post_count >= *threshold
            {
                token.cancelled.store(true, Ordering::Relaxed);
            }
            let post_delay_ms = self.post_delay_ms.load(Ordering::SeqCst);
            if post_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(post_delay_ms)).await;
            }
            let mut failures = self.length_failures_remaining.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::MessageTooLong,
                    "mock length failure".into(),
                ));
            }
            drop(failures);
            let mut send_failures = self.send_failures_remaining.lock().unwrap();
            if *send_failures > 0 {
                *send_failures -= 1;
                return Err(DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    "mock transient send failure".into(),
                ));
            }
            Ok(format!("msg-{target_channel}-{}", content.chars().count()))
        }

        async fn resolve_dm_channel(
            &self,
            user_id: &str,
        ) -> Result<String, DispatchMessagePostError> {
            self.dm_resolutions
                .lock()
                .unwrap()
                .push(user_id.to_string());
            Ok(format!("9{user_id}"))
        }
    }

    #[tokio::test]
    async fn dispatch_v3_envelope_retries_minimal_fallback_after_length_error() {
        let client = MockClient::default();
        client.fail_next_with_length();
        let dedup = OutboundDeduper::new();
        let message = DiscordOutboundMessage::new(
            "dispatch:1436",
            "dispatch:1436:notify",
            "A".repeat(180),
            OutboundTarget::Channel(ChannelId::new(123)),
            DiscordOutboundPolicy::dispatch_outbox(),
        )
        .with_summary("minimal fallback message");

        let result = deliver_outbound(&client, &dedup, message, None).await;

        match result {
            DeliveryResult::Fallback { fallback_used, .. } => {
                assert_eq!(fallback_used, FallbackUsed::MinimalFallback);
            }
            other => panic!("expected minimal fallback, got {other:?}"),
        }
        assert_eq!(
            client.posts(),
            vec![
                ("123".to_string(), "A".repeat(180)),
                ("123".to_string(), "minimal fallback message".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn v3_dedup_replays_existing_delivery_without_second_post() {
        let client = MockClient::default();
        let dedup = OutboundDeduper::new();
        let make = || {
            DiscordOutboundMessage::new(
                "dispatch:1436",
                "dispatch:1436:notify",
                "hello",
                OutboundTarget::Channel(ChannelId::new(123)),
                DiscordOutboundPolicy::dispatch_outbox(),
            )
        };

        let first = deliver_outbound(&client, &dedup, make(), None).await;
        assert!(matches!(first, DeliveryResult::Sent { .. }));
        let second = deliver_outbound(&client, &dedup, make(), None).await;

        match second {
            DeliveryResult::Duplicate {
                existing_messages, ..
            } => {
                assert_eq!(existing_messages[0].raw_message_id, "msg-123-5");
            }
            other => panic!("expected duplicate, got {other:?}"),
        }
        assert_eq!(client.posts().len(), 1);
    }

    #[tokio::test]
    async fn v3_split_duplicate_preserves_ordered_chunk_metadata() {
        let client = MockClient::default();
        let dedup = OutboundDeduper::new();
        let make = || {
            DiscordOutboundMessage::new(
                "dispatch:split",
                "dispatch:split:final",
                "ABCDEFGHIJK",
                OutboundTarget::Channel(ChannelId::new(123)),
                DiscordOutboundPolicy::default(),
            )
        };
        let overrides = DeliveryTransportOverrides {
            limits: Some(OutboundPolicyLimits::for_tests(4)),
            ..DeliveryTransportOverrides::default()
        };

        let first =
            deliver_outbound_with_overrides(&client, &dedup, make(), overrides.clone(), None).await;
        match first {
            DeliveryResult::Fallback {
                messages,
                fallback_used,
                ..
            } => {
                assert_eq!(fallback_used, FallbackUsed::LengthSplit);
                assert_eq!(messages.len(), 3);
                assert_eq!(
                    messages
                        .iter()
                        .map(|message| (message.chunk_index, message.chunk_count))
                        .collect::<Vec<_>>(),
                    vec![(Some(0), Some(3)), (Some(1), Some(3)), (Some(2), Some(3))]
                );
            }
            other => panic!("expected split fallback, got {other:?}"),
        }

        let second =
            deliver_outbound_with_overrides(&client, &dedup, make(), overrides, None).await;
        match second {
            DeliveryResult::Duplicate {
                existing_messages, ..
            } => {
                assert_eq!(
                    existing_messages
                        .iter()
                        .map(|message| (message.chunk_index, message.chunk_count))
                        .collect::<Vec<_>>(),
                    vec![(Some(0), Some(3)), (Some(1), Some(3)), (Some(2), Some(3))]
                );
            }
            other => panic!("expected duplicate, got {other:?}"),
        }
        assert_eq!(
            client.posts(),
            vec![
                ("123".to_string(), "ABCD".to_string()),
                ("123".to_string(), "EFGH".to_string()),
                ("123".to_string(), "IJK".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn v3_cancelled_before_delivery_skips_without_posting() {
        let client = MockClient::default();
        let dedup = OutboundDeduper::new();
        let token = CancelToken::new();
        token.cancelled.store(true, Ordering::Relaxed);
        let message = DiscordOutboundMessage::new(
            "dispatch:cancelled",
            "dispatch:cancelled:notify",
            "hello",
            OutboundTarget::Channel(ChannelId::new(123)),
            DiscordOutboundPolicy::dispatch_outbox(),
        );

        let result = deliver_outbound(&client, &dedup, message, Some(&token)).await;

        assert!(matches!(result, DeliveryResult::Skip { reason } if reason == "cancelled"));
        assert!(client.posts().is_empty());
    }

    #[tokio::test]
    async fn v3_split_delivery_stops_between_chunks_when_cancelled() {
        let client = MockClient::default();
        let dedup = OutboundDeduper::new();
        let token = Arc::new(CancelToken::new());
        client.cancel_after_post_count(1, token.clone());
        let message = DiscordOutboundMessage::new(
            "dispatch:split-cancel",
            "dispatch:split-cancel:notify",
            "ABCDEFGHIJK",
            OutboundTarget::Channel(ChannelId::new(123)),
            DiscordOutboundPolicy::default(),
        );
        let overrides = DeliveryTransportOverrides {
            limits: Some(OutboundPolicyLimits::for_tests(4)),
            ..DeliveryTransportOverrides::default()
        };

        let result =
            deliver_outbound_with_overrides(&client, &dedup, message, overrides, Some(&token))
                .await;

        assert!(matches!(result, DeliveryResult::Skip { reason } if reason == "cancelled"));
        assert_eq!(
            client.posts(),
            vec![("123".to_string(), "ABCD".to_string())]
        );
    }

    #[tokio::test]
    async fn v3_dm_user_target_resolves_before_posting() {
        let client = MockClient::default();
        let dedup = OutboundDeduper::new();
        let make = || {
            DiscordOutboundMessage::new(
                "dm:7",
                "dm:7:hello",
                "hello",
                OutboundTarget::DmUser(poise::serenity_prelude::UserId::new(7)),
                DiscordOutboundPolicy::review_notification(),
            )
        };

        let result = deliver_outbound(&client, &dedup, make(), None).await;

        assert!(matches!(result, DeliveryResult::Sent { .. }));
        assert_eq!(client.dm_resolutions(), vec!["7".to_string()]);
        assert_eq!(
            client.posts(),
            vec![("97".to_string(), "hello".to_string())]
        );

        let duplicate = deliver_outbound(&client, &dedup, make(), None).await;
        assert!(matches!(duplicate, DeliveryResult::Duplicate { .. }));
        assert_eq!(client.dm_resolutions(), vec!["7".to_string()]);
        assert_eq!(
            client.posts(),
            vec![("97".to_string(), "hello".to_string())]
        );
    }

    #[tokio::test]
    async fn v3_dedup_reservation_suppresses_concurrent_retry_send() {
        let client = MockClient::default();
        client.set_post_delay_ms(50);
        let dedup = OutboundDeduper::new();
        let make = || {
            DiscordOutboundMessage::new(
                "dispatch:2368",
                "dispatch:2368:notify",
                "hello",
                OutboundTarget::Channel(ChannelId::new(123)),
                DiscordOutboundPolicy::dispatch_outbox(),
            )
        };

        let (first, second) = tokio::join!(
            deliver_outbound(&client, &dedup, make(), None),
            deliver_outbound(&client, &dedup, make(), None)
        );

        let sent_count = [&first, &second]
            .iter()
            .filter(|result| matches!(result, DeliveryResult::Sent { .. }))
            .count();
        let duplicate_count = [&first, &second]
            .iter()
            .filter(|result| matches!(result, DeliveryResult::Duplicate { .. }))
            .count();
        assert_eq!(sent_count, 1);
        assert_eq!(duplicate_count, 1);
        assert_eq!(client.posts().len(), 1);
    }

    #[tokio::test]
    async fn v3_dedup_reservation_retries_after_inflight_owner_failure() {
        let client = MockClient::default();
        client.set_post_delay_ms(50);
        client.fail_next_send();
        let dedup = OutboundDeduper::new();
        let make = || {
            DiscordOutboundMessage::new(
                "dispatch:2368:owner-fail",
                "dispatch:2368:notify",
                "hello",
                OutboundTarget::Channel(ChannelId::new(123)),
                DiscordOutboundPolicy::dispatch_outbox(),
            )
        };

        let (first, second) = tokio::join!(
            deliver_outbound(&client, &dedup, make(), None),
            deliver_outbound(&client, &dedup, make(), None)
        );

        let sent_count = [&first, &second]
            .iter()
            .filter(|result| matches!(result, DeliveryResult::Sent { .. }))
            .count();
        let failure_count = [&first, &second]
            .iter()
            .filter(|result| matches!(result, DeliveryResult::PermanentFailure { .. }))
            .count();
        assert_eq!(sent_count, 1);
        assert_eq!(failure_count, 1);
        assert_eq!(client.posts().len(), 2);
    }
}
