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
    user_msg_id: MessageId,
    session_key: Option<&str>,
) -> String {
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
