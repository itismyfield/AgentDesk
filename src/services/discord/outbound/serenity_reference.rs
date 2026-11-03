use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};

use super::delivery::{deliver_outbound, first_raw_message_id};
use super::message::{DiscordOutboundMessage, OutboundReferenceContext, OutboundTarget};
use super::policy::DiscordOutboundPolicy;
use super::result::DeliveryResult;
use super::transport::{outbound_fingerprint, post_serenity_message_with_nonce};
use super::{DiscordOutboundClient, shared_outbound_deduper};
use crate::services::discord::{SharedData, rate_limit_wait};

struct SerenityReferenceOutboundClient {
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
}

pub(in crate::services::discord) async fn send_referenced_lifecycle_notice(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    reference_message_id: MessageId,
    content: &str,
    correlation_id: String,
    semantic_event_id: String,
) -> Result<Option<MessageId>, String> {
    let client = SerenityReferenceOutboundClient { http, shared };
    let nonce = referenced_lifecycle_notice_nonce(
        channel_id,
        reference_message_id,
        &correlation_id,
        &semantic_event_id,
    );
    let msg = DiscordOutboundMessage::new(
        correlation_id,
        semantic_event_id,
        content,
        OutboundTarget::Channel(channel_id),
        DiscordOutboundPolicy::preserve_inline_content(),
    )
    .with_reference(OutboundReferenceContext::reply_to(
        channel_id,
        reference_message_id,
    ))
    .with_create_nonce(nonce, true);
    delivery_message_id(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)
}

fn referenced_lifecycle_notice_nonce(
    channel_id: ChannelId,
    reference_message_id: MessageId,
    correlation_id: &str,
    semantic_event_id: &str,
) -> String {
    outbound_fingerprint(&[
        "referenced-lifecycle-notice",
        &channel_id.get().to_string(),
        &reference_message_id.get().to_string(),
        correlation_id,
        semantic_event_id,
    ])
}

fn delivery_message_id(result: DeliveryResult) -> Result<Option<MessageId>, String> {
    match result {
        DeliveryResult::Sent { messages, .. } => first_raw_message_id(&messages)
            .as_deref()
            .map(parse_message_id)
            .transpose(),
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => first_raw_message_id(&existing_messages)
            .as_deref()
            .map(parse_message_id)
            .transpose(),
        DeliveryResult::Fallback { messages, .. } => first_raw_message_id(&messages)
            .as_deref()
            .map(parse_message_id)
            .transpose(),
        DeliveryResult::Skip { .. } => Ok(None),
        DeliveryResult::TransientFailure { reason }
        | DeliveryResult::PermanentFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason } => Err(reason),
    }
}

impl DiscordOutboundClient for SerenityReferenceOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        rate_limit_wait(&self.shared, channel_id).await;
        super::raw_output::request(
            channel_id,
            super::raw_output::Operation::Post,
            channel_id.send_message(
                &self.http,
                serenity::CreateMessage::new()
                    .content(content)
                    .allowed_mentions(super::super::http::relay_allowed_mentions()),
            ),
        )
        .await
        .inspect(|message| {
            super::body_census::note_legacy_created(channel_id.get(), message.id.get())
        })
        .map(|message| message.id.get().to_string())
        .map_err(dispatch_post_error)
    }

    async fn post_message_with_reference(
        &self,
        target_channel: &str,
        content: &str,
        reference_channel: &str,
        reference_message: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        let reference_channel_id = parse_channel_id(reference_channel)?;
        let reference_message_id = parse_message_id(reference_message).map_err(|error| {
            DispatchMessagePostError::new(DispatchMessagePostErrorKind::Other, error)
        })?;
        rate_limit_wait(&self.shared, channel_id).await;
        super::raw_output::request(
            channel_id,
            super::raw_output::Operation::Post,
            channel_id.send_message(
                &self.http,
                serenity::CreateMessage::new()
                    .reference_message(
                        serenity::MessageReference::from((
                            reference_channel_id,
                            reference_message_id,
                        ))
                        .fail_if_not_exists(false),
                    )
                    .content(content)
                    .allowed_mentions(super::super::http::relay_allowed_mentions()),
            ),
        )
        .await
        .inspect(|message| {
            super::body_census::note_legacy_created(channel_id.get(), message.id.get())
        })
        .map(|message| message.id.get().to_string())
        .map_err(dispatch_post_error)
    }

    async fn post_message_with_reference_and_nonce(
        &self,
        target_channel: &str,
        content: &str,
        reference_channel: &str,
        reference_message: &str,
        nonce: &str,
        enforce_nonce: bool,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        let reference_channel_id = parse_channel_id(reference_channel)?;
        let reference_message_id = parse_message_id(reference_message).map_err(|error| {
            DispatchMessagePostError::new(DispatchMessagePostErrorKind::Other, error)
        })?;
        post_serenity_message_with_nonce(
            &self.http,
            &self.shared,
            channel_id,
            content,
            Some((reference_channel_id, reference_message_id)),
            nonce,
            enforce_nonce,
        )
        .await
        .map_err(dispatch_post_error)
    }
}

fn parse_channel_id(raw: &str) -> Result<ChannelId, DispatchMessagePostError> {
    raw.parse::<u64>().map(ChannelId::new).map_err(|error| {
        DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("invalid Discord channel id {raw}: {error}"),
        )
    })
}

fn parse_message_id(raw: &str) -> Result<MessageId, String> {
    raw.parse::<u64>()
        .map(MessageId::new)
        .map_err(|error| format!("invalid Discord message id {raw}: {error}"))
}

fn dispatch_post_error(error: serenity::Error) -> DispatchMessagePostError {
    let detail = crate::utils::redact::redact_known_secrets(&error.to_string());
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

#[cfg(test)]
mod tests {
    use super::referenced_lifecycle_notice_nonce;
    use poise::serenity_prelude::{ChannelId, MessageId};

    #[tokio::test]
    async fn common_reference_posts_are_observed_once() {
        use super::*;
        use crate::services::discord::outbound::raw_output::tests as fixture;
        if fixture::isolated(
            concat!(module_path!(), "::common_reference_posts_are_observed_once")
                .trim_start_matches("agentdesk::"),
        ) {
            return;
        }
        let log = fixture::journal().await;
        let mock = fixture::Mock::start(vec![
            fixture::message(9001),
            fixture::message(9002),
            fixture::message(9003),
        ])
        .await;
        let client = SerenityReferenceOutboundClient {
            http: Arc::new(fixture::client(&mock)),
            shared: crate::services::discord::make_shared_data_for_tests_with_storage(None),
        };
        crate::services::discord::outbound::census_scope(Some(serde_json::json!({"case":"reference","output_class":"LegacyBody","binding_mode":"TUI"})), async {
            client.post_message(&fixture::CHANNEL.to_string(), "body").await.unwrap();
            client.post_message_with_reference(&fixture::CHANNEL.to_string(), "body", &fixture::CHANNEL.to_string(), "8001").await.unwrap();
            client.post_message_with_reference_and_nonce(&fixture::CHANNEL.to_string(), "body", &fixture::CHANNEL.to_string(), "8001", "nonce", true).await.unwrap();
        }).await;
        fixture::flushed().await;
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .filter(|v: &serde_json::Value| v["kind"] == "target")
            .collect();
        assert_eq!(mock.requests().len(), 3);
        assert_eq!(rows.len(), 6);
        assert!(
            rows.iter()
                .filter(|r| r["phase"] == "result")
                .all(|r| r["outcome"] == "confirmed")
        );
    }

    #[test]
    fn lifecycle_notice_nonce_is_stable_and_semantic_event_scoped() {
        let channel_id = ChannelId::new(123);
        let message_id = MessageId::new(456);
        let first = referenced_lifecycle_notice_nonce(
            channel_id,
            message_id,
            "intake-reaction-control:123:456",
            "intake-reaction-control:123:456:queue_reaction_failed",
        );
        let retry = referenced_lifecycle_notice_nonce(
            channel_id,
            message_id,
            "intake-reaction-control:123:456",
            "intake-reaction-control:123:456:queue_reaction_failed",
        );
        let distinct_reason = referenced_lifecycle_notice_nonce(
            channel_id,
            message_id,
            "intake-reaction-control:123:456",
            "intake-reaction-control:123:456:queued_card_post_failed",
        );

        assert_eq!(
            first, retry,
            "retries across process-local dedupers need one nonce"
        );
        assert_ne!(
            first, distinct_reason,
            "different lifecycle reasons must not coalesce"
        );
        assert_eq!(first.len(), 16);
    }
}
