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

pub(super) fn live_sender_excluded_from_human_preservation(
    allowed_bot_ids: &[u64],
    author_id: u64,
    announce_resolution: crate::services::discord::health::UtilityBotUserIdResolution,
    notify_resolution: crate::services::discord::health::UtilityBotUserIdResolution,
) -> bool {
    use crate::services::discord::health::UtilityBotUserIdResolution;

    let utility_identity_excludes_human = |resolution| match resolution {
        UtilityBotUserIdResolution::Resolved(utility_bot_id) => utility_bot_id == author_id,
        UtilityBotUserIdResolution::Unconfigured => false,
        // A transient lookup failure is not proof that this sender is human.
        // Fail safe by leaving the source unmarked until utility identity is
        // determinate, matching catch-up's preservation tri-state.
        UtilityBotUserIdResolution::Unavailable => true,
    };

    allowed_bot_ids.contains(&author_id)
        || utility_identity_excludes_human(announce_resolution)
        || utility_identity_excludes_human(notify_resolution)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::health::UtilityBotUserIdResolution;

    #[test]
    fn live_human_preservation_requires_determinate_non_utility_identity() {
        let human_id = 4_247_301;

        assert!(!live_sender_excluded_from_human_preservation(
            &[],
            human_id,
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Unconfigured,
        ));
        assert!(live_sender_excluded_from_human_preservation(
            &[],
            human_id,
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        ));
        assert!(live_sender_excluded_from_human_preservation(
            &[],
            human_id,
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Unavailable,
        ));
    }

    #[test]
    fn live_known_automation_is_excluded_even_with_false_bot_flag() {
        let automation_id = 4_247_302;

        assert!(live_sender_excluded_from_human_preservation(
            &[],
            automation_id,
            UtilityBotUserIdResolution::Resolved(automation_id),
            UtilityBotUserIdResolution::Unconfigured,
        ));
        assert!(live_sender_excluded_from_human_preservation(
            &[],
            automation_id,
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Resolved(automation_id),
        ));
        assert!(live_sender_excluded_from_human_preservation(
            &[automation_id],
            automation_id,
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Unconfigured,
        ));
    }
}
