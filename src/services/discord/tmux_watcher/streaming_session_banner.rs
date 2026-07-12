//! #4147 watcher streaming composition for the first-answer session prefix.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord::tmux::tmux_watcher) fn plan_watcher_streaming_rollover_with_session_banner(
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
    user_msg_id: u64,
    started_at: Option<&str>,
    turn_start_offset: Option<u64>,
    response_sent_offset: usize,
    raw_current_portion: &str,
    status_block: &str,
) -> Option<(
    crate::services::discord::formatting::StreamingRolloverPlan,
    usize,
)> {
    let rendered =
        crate::services::discord::session_banner::with_discord_turn_session_banner_identity_prefix(
            shared,
            channel_id,
            provider,
            user_msg_id,
            started_at,
            turn_start_offset,
            response_sent_offset == 0,
            raw_current_portion.to_string(),
        );
    let plan = plan_streaming_rollover(&rendered, status_block)?;
    let raw_split = crate::services::discord::session_banner::raw_split_after_session_banner_prefix(
        plan.split_at,
        rendered.len(),
        raw_current_portion.len(),
    );
    (raw_split > 0).then_some((plan, raw_split))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord::tmux::tmux_watcher) fn build_watcher_streaming_edit_text_with_session_banner(
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
    user_msg_id: u64,
    started_at: Option<&str>,
    turn_start_offset: Option<u64>,
    response_sent_offset: usize,
    raw_current_portion: &str,
    status_block: &str,
) -> String {
    let rendered =
        crate::services::discord::session_banner::with_discord_turn_session_banner_identity_prefix(
            shared,
            channel_id,
            provider,
            user_msg_id,
            started_at,
            turn_start_offset,
            response_sent_offset == 0,
            raw_current_portion.to_string(),
        );
    build_watcher_streaming_edit_text(
        shared.ui.status_panel_v2_enabled,
        &rendered,
        status_block,
        provider,
    )
}

pub(in crate::services::discord::tmux::tmux_watcher) fn prefix_watcher_terminal_session_banner(
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
    current_inflight: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    pinned_turn: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    first_turn_chunk: bool,
    body: String,
) -> String {
    let Some(identity) = pinned_turn.or(current_inflight) else {
        return body;
    };
    crate::services::discord::session_banner::with_discord_turn_session_banner_identity_prefix(
        shared,
        channel_id,
        provider,
        identity.user_msg_id,
        Some(&identity.started_at),
        identity.turn_start_offset,
        first_turn_chunk,
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn watcher_rollover_advances_only_raw_response_bytes() {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        std::sync::Arc::get_mut(&mut shared)
            .expect("fresh test shared data is uniquely owned")
            .ui
            .status_panel_v2_enabled = true;
        let channel_id = ChannelId::new(4153);
        assert!(
            shared
                .ui
                .placeholder_live_events
                .set_session_panel_lifecycle_event(
                    channel_id,
                    Some("AgentDesk-codex#nonce-a"),
                    "session_fresh",
                    &json!({"provider_session_id": "session-a", "tmux_reused": false}),
                )
        );
        let raw = "가나다 응답 본문 ".repeat(400);

        let (plan, raw_split) = plan_watcher_streaming_rollover_with_session_banner(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            1,
            Some("2026-07-12T00:00:00Z"),
            Some(0),
            0,
            &raw,
            "⠸ 계속 처리 중",
        )
        .expect("prefixed production-sized body should roll over");

        assert!(
            raw_split < plan.split_at,
            "wire prefix bytes must not advance the raw cursor"
        );
        let banner_prefix = plan
            .frozen_chunk
            .strip_suffix(&raw[..raw_split])
            .expect("frozen first chunk ends with the consumed raw bytes");
        assert!(banner_prefix.starts_with("🆕 새 세션 시작"));
        assert!(raw_split > 0 && raw_split < raw.len());
    }

    #[test]
    fn terminal_uses_pinned_fallback_identity_after_inflight_cleanup() {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        std::sync::Arc::get_mut(&mut shared)
            .expect("fresh test shared data is uniquely owned")
            .ui
            .status_panel_v2_enabled = true;
        let channel_id = ChannelId::new(4155);
        assert!(
            shared
                .ui
                .placeholder_live_events
                .set_session_panel_lifecycle_event(
                    channel_id,
                    Some("AgentDesk-codex#nonce-b"),
                    "session_fresh",
                    &json!({"provider_session_id": "session-b", "tmux_reused": false}),
                )
        );
        let identity = crate::services::discord::inflight::InflightTurnIdentity {
            user_msg_id: 0,
            started_at: "2026-07-12T00:00:01Z".to_string(),
            tmux_session_name: Some("AgentDesk-codex".to_string()),
            turn_start_offset: Some(40),
        };
        let newer_inflight = crate::services::discord::inflight::InflightTurnIdentity {
            turn_start_offset: Some(80),
            ..identity.clone()
        };

        let streaming = prefix_watcher_terminal_session_banner(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            Some(&identity),
            None,
            true,
            "streaming".to_string(),
        );
        let terminal = prefix_watcher_terminal_session_banner(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            Some(&newer_inflight),
            Some(&identity),
            true,
            "terminal".to_string(),
        );

        assert!(streaming.starts_with("🆕 새 세션 시작"));
        assert!(terminal.starts_with("🆕 새 세션 시작"));
    }
}
