use poise::serenity_prelude as serenity;
use serenity::{
    ChannelId, CreateActionRow, CreateAllowedMentions, CreateMessage, EditMessage, Message,
    MessageId,
};

const DISCORD_EMPTY_MESSAGE_SENTINEL: &str = "\u{200b}";

#[derive(Debug, thiserror::Error)]
pub(in crate::services::discord) enum RequiredReferenceSendError {
    #[error("Discord rejected the required message reference: {0}")]
    UnknownReference(#[source] serenity::Error),
    #[error(transparent)]
    Other(#[from] serenity::Error),
}

fn is_unknown_required_reference_response(
    status: u16,
    discord_code: isize,
    error_paths: impl IntoIterator<Item = String>,
) -> bool {
    if status == 404 && discord_code == 10008 {
        return true;
    }
    status == 400
        && discord_code == 50035
        && error_paths
            .into_iter()
            .any(|path| path.to_ascii_lowercase().contains("message_reference"))
}

fn classify_required_reference_error(error: serenity::Error) -> RequiredReferenceSendError {
    let unknown = match &error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => {
            is_unknown_required_reference_response(
                response.status_code.as_u16(),
                response.error.code,
                response.error.errors.iter().map(|error| error.path.clone()),
            )
        }
        _ => false,
    };
    if unknown {
        RequiredReferenceSendError::UnknownReference(error)
    } else {
        RequiredReferenceSendError::Other(error)
    }
}

fn discord_content_or_zwsp(content: &str) -> &str {
    if content.is_empty() {
        DISCORD_EMPTY_MESSAGE_SENTINEL
    } else {
        content
    }
}

fn channel_message_builder(
    content: &str,
    reference: Option<(ChannelId, MessageId)>,
    nonce: Option<&str>,
) -> CreateMessage {
    let mut message = CreateMessage::new()
        .content(discord_content_or_zwsp(content))
        .allowed_mentions(relay_allowed_mentions());
    if let Some((reference_channel_id, reference_message_id)) = reference {
        message = message.reference_message((reference_channel_id, reference_message_id));
    }
    if let Some(nonce) = nonce {
        message = message
            .nonce(serenity::model::channel::Nonce::String(nonce.to_string()))
            .enforce_nonce(true);
    }
    message
}

/// #2839 (relay-stability): mention policy applied to EVERY relay send/edit.
///
/// Relayed agent output regularly contains `@everyone`/`@here` or role mentions
/// (an agent literally echoing "@everyone" in its answer) that must NEVER ping
/// — a single such relay would alert the entire server. With no
/// `allowed_mentions` set, Discord parses and fires ALL mentions in the content
/// by default, so this was a live mass-ping hole on every relay message.
///
/// Suppress @everyone/@here and ALL role mentions unconditionally, while still
/// allowing user mentions so the bot's own intentional requester pings
/// (`<@requester>` — prompt-too-long, escalation) keep working. Tightening the
/// residual agent-echoed user-ping case is deferred to the relay-content path
/// in the delivery-lease consolidation (it must not break directed pings).
pub(in crate::services::discord) fn relay_allowed_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(true)
        .everyone(false)
        .all_roles(false)
}

pub(in crate::services::discord) async fn send_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
) -> serenity::Result<Message> {
    channel_id
        .send_message(http, channel_message_builder(content, None, None))
        .await
}

/// Send an idempotent create. With `enforce_nonce`, Discord returns the
/// existing message for a recent duplicate nonce and the caller reconciles on
/// that returned message id instead of creating a second physical reply.
pub(in crate::services::discord) async fn send_channel_message_with_nonce(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    nonce: &str,
) -> serenity::Result<Message> {
    channel_id
        .send_message(http, channel_message_builder(content, None, Some(nonce)))
        .await
}

pub(in crate::services::discord) async fn send_channel_message_with_reference(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference_channel_id: ChannelId,
    reference_message_id: MessageId,
) -> serenity::Result<Message> {
    channel_id
        .send_message(
            http,
            channel_message_builder(
                content,
                Some((reference_channel_id, reference_message_id)),
                None,
            ),
        )
        .await
}

pub(in crate::services::discord) async fn send_channel_message_with_reference_and_nonce(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference_channel_id: ChannelId,
    reference_message_id: MessageId,
    nonce: &str,
) -> serenity::Result<Message> {
    channel_id
        .send_message(
            http,
            channel_message_builder(
                content,
                Some((reference_channel_id, reference_message_id)),
                Some(nonce),
            ),
        )
        .await
}

pub(in crate::services::discord) async fn send_channel_message_with_required_reference(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference_channel_id: ChannelId,
    reference_message_id: MessageId,
) -> Result<Message, RequiredReferenceSendError> {
    send_channel_message_with_reference(
        http,
        channel_id,
        content,
        reference_channel_id,
        reference_message_id,
    )
    .await
    .map_err(classify_required_reference_error)
}

pub(in crate::services::discord) async fn send_channel_message_with_required_reference_and_nonce(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference_channel_id: ChannelId,
    reference_message_id: MessageId,
    nonce: &str,
) -> Result<Message, RequiredReferenceSendError> {
    send_channel_message_with_reference_and_nonce(
        http,
        channel_id,
        content,
        reference_channel_id,
        reference_message_id,
        nonce,
    )
    .await
    .map_err(classify_required_reference_error)
}

/// Send a channel message with attached interactive components (buttons,
/// select menus). Lives here so the maintainability audit's
/// `direct_discord_sends` allowlist (which covers `discord/http.rs`)
/// continues to apply to interactive idle-recap cards.
pub(in crate::services::discord) async fn send_channel_message_with_components(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    components: Vec<CreateActionRow>,
) -> serenity::Result<Message> {
    channel_id
        .send_message(
            http,
            CreateMessage::new()
                .content(discord_content_or_zwsp(content))
                .components(components)
                .allowed_mentions(relay_allowed_mentions()),
        )
        .await
}

/// Edit a channel message while replacing its interactive components.
pub(in crate::services::discord) async fn edit_channel_message_with_components(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
    components: Vec<CreateActionRow>,
) -> serenity::Result<Message> {
    channel_id
        .edit_message(
            http,
            message_id,
            EditMessage::new()
                .content(discord_content_or_zwsp(content))
                .components(components)
                .allowed_mentions(relay_allowed_mentions()),
        )
        .await
}

pub(in crate::services::discord) async fn edit_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
) -> serenity::Result<Message> {
    channel_id
        .edit_message(
            http,
            message_id,
            EditMessage::new()
                .content(discord_content_or_zwsp(content))
                .allowed_mentions(relay_allowed_mentions()),
        )
        .await
}

/// Delete a single channel message by id. Errors are propagated; callers
/// that don't care about Discord-side 404s (already deleted) should wrap
/// the call in `let _ =`.
pub(in crate::services::discord) async fn delete_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
) -> serenity::Result<()> {
    channel_id.delete_message(http, message_id).await
}

#[cfg(test)]
mod tests {
    use super::{
        DISCORD_EMPTY_MESSAGE_SENTINEL, channel_message_builder, discord_content_or_zwsp,
        is_unknown_required_reference_response, relay_allowed_mentions,
    };
    use poise::serenity_prelude::{ChannelId, MessageId};

    #[test]
    fn discord_content_or_zwsp_replaces_empty_content() {
        assert_eq!(discord_content_or_zwsp(""), DISCORD_EMPTY_MESSAGE_SENTINEL);
        assert_eq!(discord_content_or_zwsp("hello"), "hello");
    }

    #[test]
    fn relay_allowed_mentions_suppresses_everyone_and_roles_allows_users() {
        // #2839: agent output echoing "@everyone"/"@here"/role mentions must
        // never ping; intentional bot user pings (<@requester>) must still fire.
        // Discord encodes the "allow all of a kind" toggles in the `parse` array.
        let value = serde_json::to_value(relay_allowed_mentions()).expect("serialize");
        let parse: Vec<String> = value
            .get("parse")
            .and_then(|p| p.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            parse.iter().any(|p| p == "users"),
            "users allowed: {parse:?}"
        );
        assert!(
            !parse.iter().any(|p| p == "everyone"),
            "everyone suppressed: {parse:?}"
        );
        assert!(
            !parse.iter().any(|p| p == "roles"),
            "roles suppressed: {parse:?}"
        );
    }

    #[test]
    fn required_reference_classifier_is_structured_and_narrow() {
        assert!(is_unknown_required_reference_response(
            400,
            50035,
            ["message_reference.message_id".to_string()],
        ));
        assert!(is_unknown_required_reference_response(
            404,
            10008,
            std::iter::empty(),
        ));
        assert!(!is_unknown_required_reference_response(
            400,
            50035,
            ["content".to_string()],
        ));
        assert!(!is_unknown_required_reference_response(
            429,
            0,
            ["message_reference.message_id".to_string()],
        ));
    }

    #[test]
    fn required_reference_nonce_builder_enforces_discord_reconciliation() {
        let channel = ChannelId::new(4_055);
        let reference = MessageId::new(90_062);
        let value = serde_json::to_value(channel_message_builder(
            "reply",
            Some((channel, reference)),
            Some("adktr01234567890123456789"),
        ))
        .expect("serialize create-message payload");
        assert_eq!(
            value.get("nonce").and_then(serde_json::Value::as_str),
            Some("adktr01234567890123456789")
        );
        assert_eq!(
            value
                .get("enforce_nonce")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert!(
            value.get("message_reference").is_some(),
            "idempotent reply must retain its required card reference"
        );
    }
}
