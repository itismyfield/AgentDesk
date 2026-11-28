//! Only refusal-notice retry metadata enters the existing persistent outbox.
use super::send_referenced_lifecycle_notice;
use crate::services::discord::shared_state::health_ports::ChannelRuntimeLookup;
#[cfg(unix)]
use crate::services::message_outbox::{
    OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe,
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{ChannelId, MessageId};

const SOURCE: &str = "lifecycle_notifier";
const REASON: &str = "hosted.recovery_refusal";
const CORRELATION: &str = "hosted-recovery-refusal";

#[cfg(unix)]
pub(crate) async fn enqueue(
    pool: &sqlx::PgPool,
    provider: &str,
    channel: ChannelId,
    anchor: MessageId,
    claim: u64,
    message: &str,
) -> Result<i64, String> {
    enqueue_outbox_pg_returning_id_with_persistent_dedupe(
        pool,
        OutboxMessage {
            target: &format!("channel:{}", channel.get()),
            content: message,
            bot: provider,
            source: SOURCE,
            reason_code: Some(REASON),
            session_key: Some(&format!("{CORRELATION}:{}:claim-{claim}", anchor.get())),
        },
    )
    .await
    .map_err(|error| error.to_string())
}

pub(crate) async fn deliver(
    registry: &impl ChannelRuntimeLookup,
    (source, reason): (&str, Option<&str>),
    (target, bot): (&str, &str),
    (content, key): (&str, Option<&str>),
) -> Option<(String, String)> {
    if source != SOURCE || reason != Some(REASON) {
        return None;
    }
    let result = async {
        let channel = target
            .strip_prefix("channel:")
            .and_then(|id| id.parse::<u64>().ok())
            .filter(|id| *id > 0)
            .ok_or("refusal notice needs an exact channel")?;
        let (anchor, claim) = key
            .and_then(|key| key.strip_prefix(&format!("{CORRELATION}:")))
            .and_then(|key| key.split_once(":claim-"))
            .ok_or("refusal notice identity missing")?;
        let anchor = anchor
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or("refusal notice anchor invalid")?;
        let claim = claim.parse::<u64>().map_err(|error| error.to_string())?;
        let provider = ProviderKind::from_str(bot).ok_or("refusal notice provider invalid")?;
        let channel = ChannelId::new(channel);
        let shared = registry
            .shared_for_provider_on_channel(&provider, channel)
            .await
            .ok_or("refusal notice runtime unavailable or ambiguous")?;
        let http = shared
            .serenity_http_or_token_fallback()
            .ok_or("refusal notice transport unavailable")?;
        send_referenced_lifecycle_notice(
            http,
            shared,
            channel,
            MessageId::new(anchor),
            content,
            CORRELATION.into(),
            format!("claim-{claim}"),
        )
        .await?
        .ok_or_else(|| "refusal notice has no delivery proof".to_string())
    }
    .await;
    Some(match result {
        Ok(id) => (
            "200 OK".into(),
            serde_json::json!({"ok":true,"message_id":id.get().to_string()}).to_string(),
        ),
        Err(error) => ("503 Service Unavailable".into(), error),
    })
}
