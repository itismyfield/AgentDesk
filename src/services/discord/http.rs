use poise::serenity_prelude as serenity;
use serenity::{
    ChannelId, CreateActionRow, CreateAllowedMentions, CreateMessage, EditMessage, Message,
    MessageId,
};

const DISCORD_EMPTY_MESSAGE_SENTINEL: &str = "\u{200b}";

fn discord_content_or_zwsp(content: &str) -> &str {
    if content.is_empty() {
        DISCORD_EMPTY_MESSAGE_SENTINEL
    } else {
        content
    }
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
        .send_message(
            http,
            CreateMessage::new()
                .content(discord_content_or_zwsp(content))
                .allowed_mentions(relay_allowed_mentions()),
        )
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
            CreateMessage::new()
                .reference_message((reference_channel_id, reference_message_id))
                .content(discord_content_or_zwsp(content))
                .allowed_mentions(relay_allowed_mentions()),
        )
        .await
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
    use super::{DISCORD_EMPTY_MESSAGE_SENTINEL, discord_content_or_zwsp, relay_allowed_mentions};

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
}
