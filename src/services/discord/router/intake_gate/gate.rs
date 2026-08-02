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

pub(super) fn bot_turn_message_admitted_for_live_intake(
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    author_id: u64,
    text: &str,
) -> bool {
    bot_author_allowed_for_live_intake(allowed_bot_ids, announce_bot_id, author_id)
        && crate::services::discord::is_allowed_turn_sender(
            allowed_bot_ids,
            announce_bot_id,
            author_id,
            true,
            text,
        )
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

    #[test]
    fn non_turn_allowed_bot_cannot_reach_any_intervention_queue_path() {
        const ANNOUNCE_ID: u64 = 1001;
        const NOTIFY_ID: u64 = 2002;
        let notice = "informational automation notice";

        let mut intervention_queue = Vec::new();
        if bot_turn_message_admitted_for_live_intake(
            &[ANNOUNCE_ID, NOTIFY_ID],
            Some(ANNOUNCE_ID),
            NOTIFY_ID,
            notice,
        ) {
            intervention_queue.push(notice);
        }
        assert!(
            intervention_queue.is_empty(),
            "allowed non-announce bots still require a dispatch or monitor turn marker"
        );

        let intake_source = include_str!("../intake_gate.rs");
        let admission = "bot_turn_message_admitted_for_live_intake(";
        let admission_pos = intake_source
            .find(admission)
            .expect("live bot admission guard is wired into intake");
        let queue_tail = &intake_source[admission_pos + admission.len()..];
        assert_eq!(
            queue_tail
                .matches("commit_soft_intervention_transaction(")
                .count(),
            6,
            "all six intervention queue paths must remain behind bot admission"
        );
    }

    #[test]
    fn watchdog_announce_turn_remains_queue_eligible_when_capacity_is_available() {
        const ANNOUNCE_ID: u64 = 1001;
        let alarm = "[system → project-agentdesk 핸드오프] 🚨 릴레이 갭 감지 (out-of-band 워치독)";

        assert!(bot_turn_message_admitted_for_live_intake(
            &[ANNOUNCE_ID],
            Some(ANNOUNCE_ID),
            ANNOUNCE_ID,
            alarm,
        ));

        let mut queue = Vec::new();
        let outcome = crate::services::turn_orchestrator::enqueue_intervention(
            &mut queue,
            crate::services::turn_orchestrator::Intervention {
                author_id: serenity::UserId::new(ANNOUNCE_ID),
                author_is_bot: true,
                message_id: serenity::MessageId::new(3001),
                queued_generation: 1,
                source_message_ids: vec![serenity::MessageId::new(3001)],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: alarm.to_string(),
                mode: crate::services::turn_orchestrator::InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            },
            None,
        );

        assert!(outcome.enqueued);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].text, alarm);
    }

    #[test]
    fn dispatch_and_monitor_bot_turns_remain_queue_eligible() {
        const ANNOUNCE_ID: u64 = 1001;
        const AUTOMATION_ID: u64 = 2002;
        let dispatch = "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000";
        let monitor =
            crate::services::discord::prepend_monitor_auto_turn_origin("check the active turn");
        let mut intervention_queue = Vec::new();

        for text in [dispatch, monitor.as_str()] {
            if bot_turn_message_admitted_for_live_intake(
                &[ANNOUNCE_ID, AUTOMATION_ID],
                Some(ANNOUNCE_ID),
                AUTOMATION_ID,
                text,
            ) {
                intervention_queue.push(text);
            }
        }

        assert_eq!(
            intervention_queue,
            vec![dispatch, monitor.as_str()],
            "legitimate DISPATCH and monitor-origin bot turns must remain queue eligible"
        );
    }
}
