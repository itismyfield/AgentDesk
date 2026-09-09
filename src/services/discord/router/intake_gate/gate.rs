//! Pure admission predicates for the Discord live-intake gate.
//!
//! ## What the #5034 gate does not guarantee
//!
//! The dual review of #5034 (PR #5136, merge commit
//! `5c4a6cd04c25cc8c6524c54d7e929d6aebf07367`) declared four areas that the
//! operational-alert / attachment-provenance work deliberately left out of
//! scope. Issue #5137 moved that declaration out of the review report and into
//! the repository; this comment is the canonical copy. Line numbers are as of
//! this commit.
//!
//! 1. The announce HTTP fallback is a different producer. When notify
//!    resolution fails, `monitoring_status::resolve_status_http`
//!    (`src/services/discord/monitoring_status.rs:241`, announce fallback at
//!    `:249`) hands the announce bot's HTTP client to
//!    `MonitoringOutboundClient::post_message` (`:36`), which posts the panel
//!    with no provenance marker. Marker-based rejection is inert for that path.
//! 2. There is no retroactive cleanup. Announce alerts posted or queued before
//!    this gate shipped are not revisited; only text presented at intake is
//!    inspected.
//! 3. The direct watchdog pre-alert length path is uninstrumented. A long
//!    persisted field plus the marker can exceed Discord's limit: the literal
//!    `[origin=operational_alert]` is 26 UTF-8 bytes encoded as eight
//!    zero-width scalars per byte, i.e. 208 characters
//!    (`src/services/discord/dispatch_policy.rs:10` and `:30`). The attachment
//!    branch of `formatting::delivery::build_attachment_inline`
//!    (`src/services/discord/formatting/delivery.rs:491`, note at `:528`) has
//!    no second inline fallback, so a length rejection propagates unchanged.
//!    No bounded fixture measures that path.
//! 4. The marker is not an authentication boundary. It is plain message text
//!    prepended by `dispatch_policy::prepend_operational_alert_origin`
//!    (`src/services/discord/dispatch_policy.rs:59`), not signed metadata, so
//!    any sender who can author in the channel can forge or omit it.

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

/// Admission decision at the live-intake queue boundary, evaluated before any
/// queue branch.
///
/// Scope, not a broader guarantee: this is `is_allowed_turn_sender` combined
/// with `has_non_turn_provenance`. `is_allowed_turn_sender` already rejects
/// operational-alert provenance on its announce and allowed-bot branches, so
/// on those branches the only admission additionally suppressed here is
/// monitor-origin marked text; the alert term is repeated at this outer
/// boundary to keep that rejection explicit rather than to originate it. Its
/// human branch admits either literal, which the combined check then
/// suppresses.
///
/// The decision reads message text only: it neither authenticates the marker
/// nor reaches producers that never attach one. See the module-level
/// non-guarantee list.
pub(super) fn should_admit_turn_message(
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    author_id: u64,
    author_is_bot: bool,
    text: &str,
) -> bool {
    crate::services::discord::dispatch_policy::is_allowed_turn_sender(
        allowed_bot_ids,
        announce_bot_id,
        author_id,
        author_is_bot,
        text,
    ) && !crate::services::discord::dispatch_policy::has_non_turn_provenance(text)
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
    fn boundary_keeps_alert_rejection_and_admits_unmarked_announce_handoff() {
        let marked = crate::services::discord::dispatch_policy::prepend_operational_alert_origin(
            "DISPATCH:watchdog alert",
        );
        assert!(!should_admit_turn_message(
            &[1001],
            Some(1001),
            1001,
            true,
            &marked,
        ));
        assert!(should_admit_turn_message(
            &[1001],
            Some(1001),
            1001,
            true,
            "DISPATCH:real handoff",
        ));
    }
}
