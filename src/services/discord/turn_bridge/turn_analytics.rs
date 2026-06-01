use super::*;

pub(super) fn emit_turn_quality_event(
    provider: &ProviderKind,
    channel_id: ChannelId,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: &str,
    role_binding: Option<&RoleBinding>,
    event_type: &str,
    payload: serde_json::Value,
) {
    crate::services::observability::emit_agent_quality_event(
        crate::services::observability::AgentQualityEvent {
            source_event_id: Some(turn_id.to_string()),
            correlation_id: dispatch_id
                .map(str::to_string)
                .or_else(|| Some(turn_id.to_string())),
            agent_id: role_binding.map(|binding| binding.role_id.clone()),
            provider: Some(provider.as_str().to_string()),
            channel_id: Some(channel_id.get().to_string()),
            card_id: None,
            dispatch_id: dispatch_id.map(str::to_string),
            event_type: event_type.to_string(),
            payload: serde_json::json!({
                "turn_id": turn_id,
                "session_key": session_key,
                "details": payload,
            }),
        },
    );
}

pub(super) fn turn_duration_ms(started_at: std::time::Instant) -> i64 {
    i64::try_from(started_at.elapsed().as_millis()).unwrap_or(i64::MAX)
}

pub(super) fn record_turn_bridge_invariant(
    condition: bool,
    provider: &ProviderKind,
    channel_id: ChannelId,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    invariant: &'static str,
    code_location: &'static str,
    message: &'static str,
    details: serde_json::Value,
) -> bool {
    crate::services::observability::record_invariant_check(
        condition,
        crate::services::observability::InvariantViolation {
            provider: Some(provider.as_str()),
            channel_id: Some(channel_id.get()),
            dispatch_id,
            session_key,
            turn_id,
            invariant,
            code_location,
            message,
            details,
        },
    )
}

pub(super) fn discord_turn_id(
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: Option<MessageId>,
    session_key: Option<&str>,
    // PER-TURN discriminator for the message-less fallback (user_msg_id == 0):
    // the turn's JSONL start offset, distinct per turn within a session. Keying
    // only on `session_key` (stable across the whole tmux/Discord session) would
    // make repeated message-less turns upsert-overwrite each other's transcript
    // rows (Codex P2). Ignored when a real `user_msg_id` is present.
    turn_start_offset: Option<u64>,
) -> String {
    // A recovery turn with no anchored Discord user message (user_msg_id == 0,
    // e.g. a TUI-direct turn) cannot key its turn id on a message id —
    // `discord:<channel>:0` is the bogus form the invariant guards against.
    // Fall back to the session key plus a per-turn discriminator, and skip the
    // non-zero-message-id invariant, which does not apply here.
    let Some(user_msg_id) = user_msg_id else {
        let turn_discriminator = match turn_start_offset {
            Some(offset) => format!("off{offset}"),
            None => "recovery".to_string(),
        };
        return match session_key {
            Some(key) => format!(
                "discord:{}:session:{key}:{turn_discriminator}",
                channel_id.get()
            ),
            None => format!("discord:{}:recovery:{turn_discriminator}", channel_id.get()),
        };
    };
    let turn_id = format!("discord:{}:{}", channel_id.get(), user_msg_id.get());
    let nonzero_components = channel_id.get() != 0 && user_msg_id.get() != 0;
    record_turn_bridge_invariant(
        nonzero_components,
        provider,
        channel_id,
        None,
        session_key,
        Some(turn_id.as_str()),
        "turn_id_unique_within_session",
        "src/services/discord/turn_bridge/turn_analytics.rs:discord_turn_id",
        "turn_id must be built from non-zero Discord channel/message ids",
        serde_json::json!({
            "channel_id": channel_id.get(),
            "user_msg_id": user_msg_id.get(),
            "turn_id": turn_id.as_str(),
        }),
    );
    debug_assert!(
        nonzero_components,
        "turn_id requires non-zero Discord channel/message ids"
    );
    turn_id
}

pub(super) fn assert_response_sent_offset_progress(
    provider: &ProviderKind,
    channel_id: ChannelId,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: &str,
    previous: usize,
    next: usize,
    full_response: &str,
    code_location: &'static str,
) {
    let monotonic = next >= previous;
    record_turn_bridge_invariant(
        monotonic,
        provider,
        channel_id,
        dispatch_id,
        session_key,
        Some(turn_id),
        "response_sent_offset_monotonic",
        code_location,
        "turn_bridge response_sent_offset must not move backwards",
        serde_json::json!({
            "previous": previous,
            "next": next,
            "full_response_len": full_response.len(),
        }),
    );
    debug_assert!(
        monotonic,
        "turn_bridge response_sent_offset must not move backwards"
    );

    let in_bounds = next <= full_response.len() && full_response.is_char_boundary(next);
    record_turn_bridge_invariant(
        in_bounds,
        provider,
        channel_id,
        dispatch_id,
        session_key,
        Some(turn_id),
        "response_sent_offset_in_bounds",
        code_location,
        "turn_bridge response_sent_offset must stay on a full_response boundary",
        serde_json::json!({
            "next": next,
            "full_response_len": full_response.len(),
        }),
    );
    debug_assert!(
        in_bounds,
        "turn_bridge response_sent_offset must stay on a full_response boundary"
    );
}

#[cfg(test)]
mod discord_turn_id_no_user_message_tests {
    //! Regression coverage for the msgid-0 panic class. A recovery turn with no
    //! anchored Discord user message (user_msg_id == 0, e.g. a TUI-direct turn)
    //! must key its turn id on the session instead of `discord:<channel>:0`,
    //! and must not panic via `MessageId::new(0)`.
    use super::discord_turn_id;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};

    #[test]
    fn none_user_msg_id_falls_back_to_session_plus_per_turn_offset() {
        let turn_id = discord_turn_id(
            &ProviderKind::Claude,
            ChannelId::new(4243),
            None,
            Some("session-key-abc"),
            Some(4096),
        );
        assert_eq!(turn_id, "discord:4243:session:session-key-abc:off4096");
        assert!(
            !turn_id.ends_with(":0"),
            "must not produce the bogus discord:<channel>:0 form"
        );
    }

    #[test]
    fn message_less_turns_in_same_session_get_distinct_ids() {
        // Two message-less turns sharing a session must NOT collide (Codex P2):
        // the per-turn JSONL start offset keeps their transcript turn ids apart.
        let first = discord_turn_id(
            &ProviderKind::Claude,
            ChannelId::new(4243),
            None,
            Some("session-key-abc"),
            Some(1000),
        );
        let second = discord_turn_id(
            &ProviderKind::Claude,
            ChannelId::new(4243),
            None,
            Some("session-key-abc"),
            Some(2000),
        );
        assert_ne!(first, second);
    }

    #[test]
    fn none_user_msg_id_and_no_session_uses_recovery_marker() {
        let turn_id = discord_turn_id(
            &ProviderKind::Claude,
            ChannelId::new(4243),
            None,
            None,
            Some(512),
        );
        assert_eq!(turn_id, "discord:4243:recovery:off512");
    }

    #[test]
    fn some_user_msg_id_keeps_legacy_message_keyed_form() {
        let turn_id = discord_turn_id(
            &ProviderKind::Claude,
            ChannelId::new(4243),
            Some(MessageId::new(99)),
            Some("session-key-abc"),
            None,
        );
        assert_eq!(turn_id, "discord:4243:99");
    }
}
