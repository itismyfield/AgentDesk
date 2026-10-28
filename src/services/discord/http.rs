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
pub(crate) fn relay_allowed_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(true)
        .everyone(false)
        .all_roles(false)
}

pub(crate) fn relay_message_payload(content: &str) -> serde_json::Value {
    serde_json::json!({
        "content": content,
        "allowed_mentions": relay_allowed_mentions(),
    })
}

pub(in crate::services::discord) async fn send_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
) -> serenity::Result<Message> {
    let message = channel_message_builder(content, None, None).map_err(serenity::Error::Other)?;
    dispatch_channel_message(http, channel_id, message).await
}

/// Preserve the SDK reply's author-ping behavior while applying the relay policy.
pub(in crate::services::discord) async fn send_channel_message_reply(
    http: &serenity::Http,
    original: &Message,
    content: impl Into<String>,
) -> serenity::Result<Message> {
    original
        .channel_id
        .send_message(
            http,
            CreateMessage::new()
                .content(content)
                .reference_message(original)
                .allowed_mentions(relay_allowed_mentions().replied_user(false)),
        )
        .await
}

pub(in crate::services::discord) async fn dispatch_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message: CreateMessage,
) -> serenity::Result<Message> {
    super::outbound::raw_output::request(
        channel_id,
        channel_id.send_message(http, message.allowed_mentions(relay_allowed_mentions())),
    )
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
    let message =
        channel_message_builder(content, None, Some(nonce)).map_err(serenity::Error::Other)?;
    super::outbound::raw_output::request(channel_id, channel_id.send_message(http, message)).await
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
    super::outbound::raw_output::request(channel_id, channel_id.send_message(http, message)).await
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
    super::outbound::raw_output::request(channel_id, channel_id.send_message(http, message)).await
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

/// Send control components through the ordinary channel request boundary.
pub(in crate::services::discord) async fn send_channel_message_with_components(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    components: Vec<CreateActionRow>,
) -> serenity::Result<Message> {
    let message = channel_message_builder(content, None, None)
        .map_err(serenity::Error::Other)?
        .components(components);
    dispatch_channel_message(http, channel_id, message).await
}

fn channel_edit_builder(
    content: Option<&str>,
    components: Option<Vec<CreateActionRow>>,
) -> Result<EditMessage, &'static str> {
    let mut edit = EditMessage::new().allowed_mentions(relay_allowed_mentions());
    if let Some(content) = content {
        edit = edit.content(discord_content_or_zwsp(content)?);
    }
    if let Some(components) = components {
        edit = edit.components(components);
    }
    Ok(edit)
}

/// Edit interactive components, preserving the current body when content is absent.
pub(in crate::services::discord) async fn edit_channel_message_with_components<'a>(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    content: impl Into<Option<&'a str>>,
    components: Vec<CreateActionRow>,
) -> serenity::Result<Message> {
    let edit =
        channel_edit_builder(content.into(), Some(components)).map_err(serenity::Error::Other)?;
    dispatch_channel_edit(http, channel_id, message_id, edit).await
}

pub(in crate::services::discord) async fn edit_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
) -> serenity::Result<Message> {
    let edit = channel_edit_builder(Some(content), None).map_err(serenity::Error::Other)?;
    dispatch_channel_edit(http, channel_id, message_id, edit).await
}

async fn dispatch_channel_edit(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    edit: EditMessage,
) -> serenity::Result<Message> {
    super::outbound::raw_output::request(
        channel_id,
        channel_id.edit_message(http, message_id, edit),
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
    super::outbound::raw_output::request(channel_id, channel_id.delete_message(http, message_id))
        .await
}

#[cfg(test)]
mod tests {
    use super::{
        DISCORD_CONTENT_HARD_CAP_UNITS, DISCORD_CONTENT_LIMIT_ERROR,
        DISCORD_EMPTY_MESSAGE_SENTINEL, channel_edit_builder, channel_message_builder,
        discord_content_or_zwsp, edit_channel_message, edit_channel_message_with_components,
        is_unknown_required_reference_response, relay_allowed_mentions, send_channel_message,
        send_channel_message_with_components, send_channel_message_with_nonce,
        send_channel_message_with_reference, send_channel_message_with_reference_and_nonce,
    };
    use crate::services::discord::DISCORD_MSG_LIMIT;
    use crate::services::discord::formatting::{discord_message_units, needs_multiple_messages};
    use poise::serenity_prelude::{
        ChannelId, CreateActionRow, CreateButton, EditMessage, HttpBuilder, MessageId,
    };

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

    #[tokio::test]
    async fn p2_allowed_mentions_sdk_reply_wire_preserves_reference_and_no_author_ping() {
        let (address, request) = recording_error_server();
        let http = HttpBuilder::new("test-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .default_allowed_mentions(relay_allowed_mentions())
            .build();
        let mut original = super::serenity::Message::default();
        original.id = MessageId::new(90_062);
        original.channel_id = ChannelId::new(4_055);
        original.guild_id = Some(super::serenity::GuildId::new(70_001));
        original.author.id = super::serenity::UserId::new(70_002);
        original.content = "!restart original command".to_owned();
        let content = "<@1234> @everyone @here <@&5678> original reply body";
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            super::send_channel_message_reply(&http, &original, content),
        )
        .await
        .expect("reply returns from owned HTTP boundary");
        let (line, body) = request.join().expect("reply request recorded");
        assert!(line.starts_with("POST "));
        assert!(line.contains("/channels/4055/messages"));
        let error = response.expect_err("owned fixture deliberately rejects the reply");
        assert!(matches!(
            error,
            super::serenity::Error::Http(super::serenity::http::HttpError::UnsuccessfulRequest(response))
                if response.status_code.as_u16() == 400 && response.error.code == 50035
        ));
        assert_eq!(body["content"], content);
        assert_eq!(original.content, "!restart original command");
        assert_eq!(body["message_reference"]["message_id"], "90062");
        assert_eq!(body["message_reference"]["channel_id"], "4055");
        assert_eq!(body["message_reference"]["guild_id"], "70001");
        assert!(body["message_reference"]["fail_if_not_exists"].is_null());
        assert_eq!(body["allowed_mentions"]["replied_user"], false);
        assert_eq!(
            body["allowed_mentions"]["parse"],
            serde_json::json!(["users"])
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

    fn control_components() -> Vec<CreateActionRow> {
        vec![CreateActionRow::Buttons(vec![
            CreateButton::new("fixture-consent").label("Allow"),
        ])]
    }

    fn assert_request_mentions_are_safe(value: &serde_json::Value) {
        let parse = value["allowed_mentions"]["parse"]
            .as_array()
            .expect("explicit mention policy");
        assert!(parse.iter().any(|value| value == "users"));
        assert!(
            !parse
                .iter()
                .any(|value| value == "everyone" || value == "roles")
        );
    }

    #[test]
    fn component_edits_preserve_absent_body_and_explicit_component_removal() {
        let preserve = serde_json::to_value(
            channel_edit_builder(None, Some(Vec::new())).expect("disable components"),
        )
        .expect("serialize disable request");
        assert!(
            preserve.get("content").is_none(),
            "disabling controls must omit content rather than replace the current body"
        );
        assert_eq!(preserve["components"], serde_json::json!([]));
        assert_request_mentions_are_safe(&preserve);

        for content in ["replacement body", ""] {
            let replacement = serde_json::to_value(
                channel_edit_builder(Some(content), Some(control_components()))
                    .expect("replace controls and body"),
            )
            .expect("serialize replacement request");
            assert_eq!(
                replacement["content"],
                if content.is_empty() {
                    DISCORD_EMPTY_MESSAGE_SENTINEL
                } else {
                    content
                }
            );
            assert_eq!(
                replacement["components"][0]["components"][0]["custom_id"],
                "fixture-consent"
            );
            assert_request_mentions_are_safe(&replacement);
        }
    }

    #[test]
    fn ordinary_edit_keeps_its_body_and_leaves_components_unspecified() {
        let actual = serde_json::to_value(
            channel_edit_builder(Some("ordinary body"), None).expect("ordinary edit"),
        )
        .expect("serialize ordinary edit");
        let expected = serde_json::to_value(
            EditMessage::new()
                .content("ordinary body")
                .allowed_mentions(relay_allowed_mentions()),
        )
        .expect("serialize existing ordinary edit contract");
        assert_eq!(actual, expected);
        assert!(actual.get("components").is_none());
        assert_request_mentions_are_safe(&actual);
    }

    fn recording_error_server() -> (
        std::net::SocketAddr,
        std::thread::JoinHandle<(String, serde_json::Value)>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("owned loopback listener");
        let address = listener.local_addr().expect("owned address");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let task = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "request reaches owned HTTP boundary"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("owned accept failed: {error}"),
                }
            };
            socket
                .set_nonblocking(false)
                .expect("blocking reads on accepted socket");
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("bounded request read");
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let header_end = loop {
                let count = socket.read(&mut buffer).expect("request bytes");
                assert!(count > 0, "request headers reach the owned boundary");
                request.extend_from_slice(&buffer[..count]);
                if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8(request[..header_end].to_vec()).expect("HTTP headers");
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then_some(value.trim())
                })
                .expect("JSON request has a content length")
                .parse()
                .expect("content length is numeric");
            while request.len() < header_end + length {
                let count = socket.read(&mut buffer).expect("JSON body bytes");
                assert!(count > 0, "request body is complete");
                request.extend_from_slice(&buffer[..count]);
            }
            let body = serde_json::from_slice(&request[header_end..header_end + length])
                .expect("control request JSON");
            let response = r#"{"code":50035,"message":"fixture rejection","errors":{}}"#;
            write!(socket, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response)
                .expect("owned error response");
            (
                headers.lines().next().expect("request line").to_owned(),
                body,
            )
        });
        (address, task)
    }

    #[tokio::test]
    async fn component_requests_share_dispatch_and_preserve_http_error_classification() {
        let channel = ChannelId::new(4_055);
        let message = MessageId::new(90_062);
        for operation in ["send", "replace", "disable"] {
            let (address, request) = recording_error_server();
            let http = HttpBuilder::new("test-token")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build();
            let response = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                match operation {
                    "send" => {
                        send_channel_message_with_components(
                            &http,
                            channel,
                            "control body",
                            control_components(),
                        )
                        .await
                    }
                    "replace" => {
                        edit_channel_message_with_components(
                            &http,
                            channel,
                            message,
                            Some("replacement body"),
                            control_components(),
                        )
                        .await
                    }
                    _ => {
                        edit_channel_message_with_components(
                            &http,
                            channel,
                            message,
                            None::<&str>,
                            Vec::new(),
                        )
                        .await
                    }
                }
            })
            .await
            .expect("owned HTTP error returns promptly");
            let (line, body) = request.join().expect("owned request recorded");
            assert!(line.starts_with(if operation == "send" {
                "POST "
            } else {
                "PATCH "
            }));
            assert!(line.contains("/channels/4055/messages"));
            if operation != "send" {
                assert!(line.contains("/channels/4055/messages/90062"));
            }
            if operation == "disable" {
                assert!(
                    body.get("content").is_none(),
                    "body-preserving PATCH omits content"
                );
                assert_eq!(body["components"], serde_json::json!([]));
            } else {
                assert_eq!(
                    body["content"],
                    if operation == "send" {
                        "control body"
                    } else {
                        "replacement body"
                    }
                );
                assert_eq!(
                    body["components"][0]["components"][0]["custom_id"],
                    "fixture-consent"
                );
            }
            assert_request_mentions_are_safe(&body);
            let error = response.expect_err("owned fixture deliberately rejects the request");
            assert!(
                matches!(
                    &error,
                    super::serenity::Error::Http(super::serenity::http::HttpError::UnsuccessfulRequest(response))
                        if response.status_code.as_u16() == 400 && response.error.code == 50035
                ),
                "shared dispatch must preserve the exact HTTP status and Discord error code"
            );
            assert!(
                matches!(
                    super::classify_required_reference_error(error),
                    super::RequiredReferenceSendError::Other(super::serenity::Error::Http(_))
                ),
                "an ordinary HTTP rejection must not become an unknown-reference error"
            );
        }
    }
}
