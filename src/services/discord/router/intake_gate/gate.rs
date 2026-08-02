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

pub(super) fn should_skip_bot_message_for_live_intake(
    author_is_bot: bool,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    author_id: u64,
    text: &str,
) -> bool {
    let is_allowed_bot_sender =
        bot_author_allowed_for_live_intake(allowed_bot_ids, announce_bot_id, author_id);
    (author_is_bot || is_allowed_bot_sender)
        && !bot_turn_message_admitted_for_live_intake(
            allowed_bot_ids,
            announce_bot_id,
            author_id,
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

    fn intervention(
        author_id: u64,
        message_id: u64,
        author_is_bot: bool,
        text: impl Into<String>,
    ) -> crate::services::turn_orchestrator::Intervention {
        crate::services::turn_orchestrator::Intervention {
            author_id: serenity::UserId::new(author_id),
            author_is_bot,
            message_id: serenity::MessageId::new(message_id),
            queued_generation: 1,
            source_message_ids: vec![serenity::MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.into(),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

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
    fn false_flag_known_automation_non_turn_notice_is_skipped() {
        const ANNOUNCE_ID: u64 = 1001;
        const NOTIFY_ID: u64 = 2002;
        let notice = "informational automation notice";

        assert!(should_skip_bot_message_for_live_intake(
            false,
            &[ANNOUNCE_ID, NOTIFY_ID],
            Some(ANNOUNCE_ID),
            NOTIFY_ID,
            notice,
        ));
    }

    #[test]
    fn live_bot_skip_predicate_is_the_effective_outer_intake_guard() {
        let intake_source = include_str!("../intake_gate.rs");
        let compact: String = intake_source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let effective_guard = concat!(
            "ifgate::should_skip_bot_message_for_live_intake(",
            "new_message.author.bot,",
            "&settings_snapshot.allowed_bot_ids,",
            "announce_bot_id,",
            "user_id.get(),",
            "raw_text,",
            "){"
        );

        assert_eq!(
            compact.matches(effective_guard).count(),
            1,
            "the live skip predicate must be the complete outer guard; appended bypasses such as `&& false` are forbidden"
        );

        let guard_pos = compact
            .find(effective_guard)
            .expect("effective live bot skip guard is wired into intake");
        let queue_tail = &compact[guard_pos + effective_guard.len()..];
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
            intervention(ANNOUNCE_ID, 3001, true, alarm),
            None,
        );

        assert!(outcome.enqueued);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].text, alarm);
    }

    #[test]
    fn false_flag_known_automation_turn_markers_remain_admitted() {
        const ANNOUNCE_ID: u64 = 1001;
        const AUTOMATION_ID: u64 = 2002;
        let dispatch = "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000";
        let monitor =
            crate::services::discord::prepend_monitor_auto_turn_origin("check the active turn");

        for text in [dispatch, monitor.as_str()] {
            assert!(!should_skip_bot_message_for_live_intake(
                false,
                &[ANNOUNCE_ID, AUTOMATION_ID],
                Some(ANNOUNCE_ID),
                AUTOMATION_ID,
                text,
            ));
        }
    }

    #[test]
    fn unregistered_true_flag_bot_is_skipped_before_auth() {
        assert!(should_skip_bot_message_for_live_intake(
            true,
            &[],
            None,
            3003,
            "unregistered bot notification",
        ));
    }

    #[test]
    fn false_flag_automation_notice_flood_cannot_evict_queued_user() {
        const USER_ID: u64 = 1;
        const AUTOMATION_ID: u64 = 2002;
        let mut queue = Vec::new();

        let user_outcome = crate::services::turn_orchestrator::enqueue_intervention(
            &mut queue,
            intervention(USER_ID, 1, false, "user input"),
            None,
        );
        assert!(user_outcome.enqueued);

        for offset in 0..30 {
            let text = format!("informational automation notice {offset}");
            if !should_skip_bot_message_for_live_intake(
                false,
                &[AUTOMATION_ID],
                None,
                AUTOMATION_ID,
                &text,
            ) {
                let _ = crate::services::turn_orchestrator::enqueue_intervention(
                    &mut queue,
                    intervention(AUTOMATION_ID, 100 + offset, false, text),
                    None,
                );
            }
        }

        assert_eq!(
            queue.len(),
            1,
            "all 30 distinct false-flag automation notices must be rejected before enqueue"
        );
        assert_eq!(queue[0].author_id, serenity::UserId::new(USER_ID));
        assert_eq!(queue[0].message_id, serenity::MessageId::new(1));
    }
}
