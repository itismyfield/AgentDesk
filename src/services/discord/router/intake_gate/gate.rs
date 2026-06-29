use super::*;

pub(in crate::services::discord) fn should_process_turn_message(
    kind: serenity::model::channel::MessageType,
) -> bool {
    matches!(
        kind,
        serenity::model::channel::MessageType::Regular
            | serenity::model::channel::MessageType::InlineReply
    )
}

pub(super) fn content_has_explicit_user_mention(content: &str, user_id: serenity::UserId) -> bool {
    let raw_id = user_id.get();
    content.contains(&format!("<@{raw_id}>")) || content.contains(&format!("<@!{raw_id}>"))
}

pub(super) fn should_skip_self_authored_turn_message(
    author_id: serenity::UserId,
    current_bot_id: serenity::UserId,
) -> bool {
    author_id == current_bot_id
}

pub(super) fn should_skip_for_missing_required_mention(
    settings: &DiscordBotSettings,
    effective_channel_id: serenity::ChannelId,
    is_dm: bool,
    content: &str,
    bot_user_id: serenity::UserId,
) -> bool {
    !is_dm
        && settings
            .require_mention_channel_ids
            .contains(&effective_channel_id.get())
        && !content_has_explicit_user_mention(content, bot_user_id)
}

pub(super) fn strip_leading_bot_mention(text: &str) -> String {
    static BOT_MENTION_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^<@!?\d+>\s*").expect("static bot-mention regex is valid")
    });
    BOT_MENTION_RE.replace(text, "").to_string()
}

pub(super) fn should_start_attachment_only_turn(text: &str, saved_attachment_count: usize) -> bool {
    saved_attachment_count > 0 && strip_leading_bot_mention(text).trim().is_empty()
}

pub(in crate::services::discord) fn bot_author_allowed_for_live_intake(
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    author_id: u64,
) -> bool {
    allowed_bot_ids.contains(&author_id) || announce_bot_id.is_some_and(|id| id == author_id)
}

pub(super) fn should_skip_human_slash_message(
    content: &str,
    known_slash_commands: Option<&std::collections::HashSet<String>>,
) -> bool {
    if !content.starts_with('/') {
        return false;
    }

    let command_name = content[1..].split_whitespace().next().unwrap_or("");
    if command_name.is_empty() {
        return false;
    }

    known_slash_commands.is_some_and(|set| set.contains(command_name))
}

pub(super) fn should_merge_consecutive_messages(text: &str, is_allowed_bot: bool) -> bool {
    !is_allowed_bot
        && !text.starts_with('!')
        && !text.starts_with('/')
        && !text.starts_with("DISPATCH:")
}
