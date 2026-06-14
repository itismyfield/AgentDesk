//! EPIC #3464 — scope unauthorized voice-transcript announcements to the owning
//! agent. Extracted from the LoC-frozen `intake_turn.rs` so the decision and the
//! one-shot drop warn are unit-testable in isolation and the frozen file only
//! carries a single call site.

use super::*;

/// Pure decision: should an inbound voice-transcript announcement be DROPPED
/// (not answered) by this agent? Only the owning agent — which resolved the
/// durable metadata (`voice_announcement_resolved`) — replies; everyone else
/// must stop, or the shared voice channel gets a multi-agent reply storm.
pub(super) fn should_drop_unauthorized_voice_announcement(
    has_stored_voice_announcement: bool,
    has_legacy_voice_announcement: bool,
    is_readable_voice_announcement: bool,
    voice_announcement_resolved: bool,
    announce_bot_id: Option<u64>,
    request_owner: UserId,
) -> bool {
    if voice_announcement_resolved {
        return false;
    }

    has_stored_voice_announcement
        || (announce_bot_id == Some(request_owner.get())
            && (has_legacy_voice_announcement || is_readable_voice_announcement))
}

/// Intake scoping guard: returns `true` (the caller must `return Ok(())`) when
/// this is an unauthorized voice announcement a non-owning agent must not answer,
/// emitting the one-shot observability warn. Returns `false` for the owning agent
/// (resolved durable metadata) and for non-announcements, which proceed normally.
#[allow(clippy::too_many_arguments)]
pub(super) fn drop_unauthorized_voice_announcement(
    has_stored_voice_announcement: bool,
    has_legacy_voice_announcement: bool,
    is_readable_voice_announcement: bool,
    voice_announcement_resolved: bool,
    announce_bot_id: Option<u64>,
    request_owner: UserId,
    channel_id: ChannelId,
    user_msg_id: MessageId,
) -> bool {
    if !should_drop_unauthorized_voice_announcement(
        has_stored_voice_announcement,
        has_legacy_voice_announcement,
        is_readable_voice_announcement,
        voice_announcement_resolved,
        announce_bot_id,
        request_owner,
    ) {
        return false;
    }

    if has_stored_voice_announcement && announce_bot_id.is_none() {
        tracing::warn!(
            channel_id = channel_id.get(),
            message_id = user_msg_id.get(),
            author_id = request_owner.get(),
            "dropping stored voice transcript announcement because announce bot user id is unavailable"
        );
    } else {
        tracing::warn!(
            channel_id = channel_id.get(),
            message_id = user_msg_id.get(),
            author_id = request_owner.get(),
            announce_bot_id = ?announce_bot_id,
            "ignoring voice transcript announcement without authorized durable metadata"
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_readable_voice_shape_is_not_dropped() {
        assert!(!should_drop_unauthorized_voice_announcement(
            false,
            false,
            true,
            false,
            Some(100),
            UserId::new(42),
        ));
    }

    #[test]
    fn announce_bot_readable_without_metadata_is_dropped() {
        assert!(should_drop_unauthorized_voice_announcement(
            false,
            false,
            true,
            false,
            Some(100),
            UserId::new(100),
        ));
    }

    #[test]
    fn stored_announcement_without_metadata_is_dropped() {
        assert!(should_drop_unauthorized_voice_announcement(
            true,
            false,
            false,
            false,
            None,
            UserId::new(42),
        ));
    }

    #[test]
    fn resolved_announcement_is_not_dropped() {
        assert!(!should_drop_unauthorized_voice_announcement(
            true,
            true,
            true,
            true,
            Some(100),
            UserId::new(100),
        ));
    }
}
