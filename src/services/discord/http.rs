use poise::serenity_prelude as serenity;
use serenity::{
    ChannelId, CreateActionRow, CreateAllowedMentions, CreateMessage, EditMessage, Message,
    MessageId,
};

const DISCORD_EMPTY_MESSAGE_SENTINEL: &str = "\u{200b}";
const DISCORD_CONTENT_HARD_CAP_UNITS: usize = super::DISCORD_MSG_LIMIT;
const DISCORD_CONTENT_LIMIT_ERROR: &str =
    "Invalid Form Body (content: BASE_TYPE_MAX_LENGTH Must be 2000 or fewer in length)";

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

pub(in crate::services::discord) fn discord_content_or_zwsp(
    content: &str,
) -> Result<&str, &'static str> {
    let content = if content.is_empty() {
        DISCORD_EMPTY_MESSAGE_SENTINEL
    } else {
        content
    };
    let content_units = super::formatting::discord_message_units(content);
    if content_units > DISCORD_CONTENT_HARD_CAP_UNITS {
        tracing::error!(
            content_units,
            hard_cap_units = DISCORD_CONTENT_HARD_CAP_UNITS,
            "Discord outbound content rejected before dispatch: internal length cap exceeded"
        );
        return Err(DISCORD_CONTENT_LIMIT_ERROR);
    }
    Ok(content)
}

// This guard covers only the Serenity send/edit builders in this module.
// The outbound-v3 `DiscordGateway` transport does not pass through it.

fn channel_message_builder(
    content: &str,
    reference: Option<(ChannelId, MessageId)>,
    nonce: Option<&str>,
) -> Result<CreateMessage, &'static str> {
    let mut message = CreateMessage::new()
        .content(discord_content_or_zwsp(content)?)
        .allowed_mentions(relay_allowed_mentions());
    if let Some((reference_channel_id, reference_message_id)) = reference {
        message = message.reference_message((reference_channel_id, reference_message_id));
    }
    if let Some(nonce) = nonce {
        message = message
            .nonce(serenity::model::channel::Nonce::String(nonce.to_string()))
            .enforce_nonce(true);
    }
    Ok(message)
}

/// #2839 (relay-stability): mention policy applied wherever a Serenity request
/// builder calls this helper — this module's builders plus the outbound-v3
/// `gateway`, `outbound::transport`, and `outbound::serenity_reference` paths.
/// Call sites that build a request without it are not covered; grep this
/// function's callers rather than assuming a module boundary.
///
/// Relayed agent output regularly contains `@everyone`/`@here` or role mentions
/// (an agent literally echoing "@everyone" in its answer) that must NEVER ping
/// — a single such relay would alert the entire server. With no
/// `allowed_mentions` set, Discord parses and fires ALL mentions in the content
/// by default, so a builder skipping this helper carries a mass-ping hole when
/// the content contains them.
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
    let message = channel_message_builder(content, None, None).map_err(serenity::Error::Other)?;
    channel_id.send_message(http, message).await
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
    let message =
        channel_message_builder(content, None, Some(nonce)).map_err(serenity::Error::Other)?;
    channel_id.send_message(http, message).await
}

pub(in crate::services::discord) async fn send_channel_message_with_reference(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference_channel_id: ChannelId,
    reference_message_id: MessageId,
) -> serenity::Result<Message> {
    let message = channel_message_builder(
        content,
        Some((reference_channel_id, reference_message_id)),
        None,
    )
    .map_err(serenity::Error::Other)?;
    channel_id.send_message(http, message).await
}

pub(in crate::services::discord) async fn send_channel_message_with_reference_and_nonce(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference_channel_id: ChannelId,
    reference_message_id: MessageId,
    nonce: &str,
) -> serenity::Result<Message> {
    let message = channel_message_builder(
        content,
        Some((reference_channel_id, reference_message_id)),
        Some(nonce),
    )
    .map_err(serenity::Error::Other)?;
    channel_id.send_message(http, message).await
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
                .content(discord_content_or_zwsp(content).map_err(serenity::Error::Other)?)
                .components(components)
                .allowed_mentions(relay_allowed_mentions()),
        )
        .await
}

/// Edit interactive components, preserving the current body when content is absent.
pub(in crate::services::discord) async fn edit_channel_message_with_components<'a>(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    content: impl Into<Option<&'a str>>,
    components: Vec<CreateActionRow>,
) -> serenity::Result<Message> {
    let mut edit = EditMessage::new()
        .components(components)
        .allowed_mentions(relay_allowed_mentions());
    if let Some(content) = content.into() {
        edit = edit.content(discord_content_or_zwsp(content).map_err(serenity::Error::Other)?);
    }
    channel_id.edit_message(http, message_id, edit).await
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
                .content(discord_content_or_zwsp(content).map_err(serenity::Error::Other)?)
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
        DISCORD_CONTENT_HARD_CAP_UNITS, DISCORD_CONTENT_LIMIT_ERROR,
        DISCORD_EMPTY_MESSAGE_SENTINEL, channel_message_builder, discord_content_or_zwsp,
        edit_channel_message, edit_channel_message_with_components,
        is_unknown_required_reference_response, relay_allowed_mentions, send_channel_message,
        send_channel_message_with_components, send_channel_message_with_nonce,
        send_channel_message_with_reference, send_channel_message_with_reference_and_nonce,
    };
    use crate::services::discord::DISCORD_MSG_LIMIT;
    use crate::services::discord::formatting::{discord_message_units, needs_multiple_messages};
    use poise::serenity_prelude::{ChannelId, HttpBuilder, MessageId};

    #[test]
    fn discord_content_or_zwsp_replaces_empty_content() {
        assert_eq!(
            discord_content_or_zwsp("").unwrap(),
            DISCORD_EMPTY_MESSAGE_SENTINEL
        );
        assert_eq!(discord_content_or_zwsp("hello").unwrap(), "hello");
        assert_eq!(DISCORD_CONTENT_HARD_CAP_UNITS, DISCORD_MSG_LIMIT);

        let exact = "x".repeat(DISCORD_MSG_LIMIT);
        let overflow = format!("{exact}x");
        assert_eq!(discord_message_units(&exact), DISCORD_MSG_LIMIT);
        assert_eq!(discord_message_units(&overflow), DISCORD_MSG_LIMIT + 1);
        assert!(channel_message_builder(&exact, None, None).is_ok());
        assert!(channel_message_builder(&overflow, None, None).is_err());
        assert!(!needs_multiple_messages(&exact));
        assert!(needs_multiple_messages(&overflow));
    }

    #[tokio::test]
    async fn discord_send_and_edit_reject_oversized_content_before_network() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let http = HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build();
        let channel = ChannelId::new(4_055);
        let message = MessageId::new(90_062);
        let content = format!("{}x", "😀".repeat(DISCORD_MSG_LIMIT / 2));
        assert_eq!(discord_message_units(&content), DISCORD_MSG_LIMIT + 1);

        let results = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            [
                ("send", send_channel_message(&http, channel, &content).await),
                (
                    "send with nonce",
                    send_channel_message_with_nonce(&http, channel, &content, "nonce").await,
                ),
                (
                    "send with reference",
                    send_channel_message_with_reference(&http, channel, &content, channel, message)
                        .await,
                ),
                (
                    "send with reference and nonce",
                    send_channel_message_with_reference_and_nonce(
                        &http, channel, &content, channel, message, "nonce",
                    )
                    .await,
                ),
                (
                    "send with components",
                    send_channel_message_with_components(&http, channel, &content, Vec::new())
                        .await,
                ),
                (
                    "edit with components",
                    edit_channel_message_with_components(
                        &http,
                        channel,
                        message,
                        Some(content.as_str()),
                        Vec::new(),
                    )
                    .await,
                ),
                (
                    "edit",
                    edit_channel_message(&http, channel, message, &content).await,
                ),
            ]
        })
        .await
        .expect("oversized content must be rejected before waiting for HTTP");
        for (operation, result) in results {
            assert!(
                matches!(result, Err(super::serenity::Error::Other(error)) if error == DISCORD_CONTENT_LIMIT_ERROR),
                "{operation} must preserve the exact content-limit error"
            );
        }
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "oversized send/edit content must create no network connections"
        );
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
        let value = serde_json::to_value(
            channel_message_builder(
                "reply",
                Some((channel, reference)),
                Some("adktr01234567890123456789"),
            )
            .expect("reply builder"),
        )
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
