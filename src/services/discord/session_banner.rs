//! #4147: one-shot session banner composed into the first answer message.
//!
//! #3983 item4 moved the residual SESSION line out of the every-tick footer and
//! POSTed it as a separate Discord message. The answer placeholder is created
//! first and later edited in place, so that later POST could only appear below
//! the first answer chunk (or between chunks). #4147 restores the #3653 contract:
//! the line is a wire-only prefix on the first answer message itself.
//!
//! The prefix is never persisted in `full_response`, inflight state, transcript,
//! or delivery fingerprints. The per-channel status state owns an atomic
//! session claim plus a sticky winning turn: the same turn may re-compose the
//! prefix during streaming and terminal replacement, while later turns in that
//! session cannot claim it again.

use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

use super::SharedData;

pub(in crate::services::discord) struct DiscordTurnSessionBanner<'a> {
    shared: &'a SharedData,
    channel_id: ChannelId,
    provider: &'a ProviderKind,
    turn_id: Option<String>,
}

impl<'a> DiscordTurnSessionBanner<'a> {
    /// Build the canonical Discord turn key used by the delivery fence. A real
    /// Discord message id is already unique. Synthetic/TUI-direct id-0 turns
    /// require both the start timestamp and monotonic JSONL start offset; if
    /// either is unavailable, the banner stays unclaimed rather than aliasing
    /// multiple turns onto `discord:<channel>:0`.
    pub(in crate::services::discord) fn new_with_turn_key(
        shared: &'a SharedData,
        channel_id: ChannelId,
        provider: &'a ProviderKind,
        user_msg_id: u64,
        started_at: Option<&str>,
        turn_start_offset: Option<u64>,
    ) -> Self {
        let turn_id = if user_msg_id != 0 {
            Some(format!("discord:{}:{user_msg_id}", channel_id.get()))
        } else {
            started_at
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .zip(turn_start_offset)
                .map(|(started_at, offset)| {
                    format!("discord:{}:0:{started_at}:{offset}", channel_id.get())
                })
        };
        Self {
            shared,
            channel_id,
            provider,
            turn_id,
        }
    }

    pub(in crate::services::discord) fn prefix(
        &self,
        first_turn_chunk: bool,
        body: String,
    ) -> String {
        let Some(turn_id) = self.turn_id.as_deref() else {
            return body;
        };
        with_session_banner_prefix(
            self.shared,
            self.channel_id,
            self.provider,
            turn_id,
            first_turn_chunk,
            body,
        )
    }

    pub(in crate::services::discord) fn format_discord_body(&self, body: &str) -> String {
        if self.shared.ui.status_panel_v2_enabled {
            super::formatting::format_for_discord_with_status_panel(body, self.provider)
        } else {
            super::formatting::format_for_discord_with_provider(body, self.provider)
        }
    }

    pub(in crate::services::discord) fn format_and_prefix(
        &self,
        first_turn_chunk: bool,
        body: &str,
    ) -> String {
        self.prefix(first_turn_chunk, self.format_discord_body(body))
    }
}

/// Decorate the first logical answer chunk for `turn_id` with the current
/// session line. `first_turn_chunk=false`, an empty body, disabled status-panel
/// v2, or a non-winning turn returns `body` unchanged.
pub(in crate::services::discord) fn with_session_banner_prefix(
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
    turn_id: &str,
    first_turn_chunk: bool,
    body: String,
) -> String {
    if !shared.ui.status_panel_v2_enabled || !first_turn_chunk || body.trim().is_empty() {
        return body;
    }
    let Some(line) = shared
        .ui
        .placeholder_live_events
        .claim_session_banner_prefix_line(channel_id, provider, turn_id)
    else {
        return body;
    };
    format!("{line}\n\n{body}")
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn with_discord_turn_session_banner_identity_prefix(
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
    user_msg_id: u64,
    started_at: Option<&str>,
    turn_start_offset: Option<u64>,
    first_turn_chunk: bool,
    body: String,
) -> String {
    DiscordTurnSessionBanner::new_with_turn_key(
        shared,
        channel_id,
        provider,
        user_msg_id,
        started_at,
        turn_start_offset,
    )
    .prefix(first_turn_chunk, body)
}

/// Convert a split byte offset computed on a prefixed wire body back to the
/// matching byte offset in the raw response body. The banner is a strict prefix,
/// so the delta between rendered/raw lengths is exactly the prefix byte count.
pub(in crate::services::discord) fn raw_split_after_session_banner_prefix(
    rendered_split_at: usize,
    rendered_len: usize,
    raw_len: usize,
) -> usize {
    rendered_split_at.saturating_sub(rendered_len.saturating_sub(raw_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BANNER: &str = "🆕 새 세션 시작";

    fn shared_with_v2(enabled: bool) -> std::sync::Arc<SharedData> {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        std::sync::Arc::get_mut(&mut shared)
            .expect("fresh test shared data is uniquely owned")
            .ui
            .status_panel_v2_enabled = enabled;
        shared
    }

    fn arm_session(shared: &SharedData, channel_id: ChannelId, nonce: &str, provider_id: &str) {
        assert!(
            shared
                .ui
                .placeholder_live_events
                .set_session_panel_lifecycle_event(
                    channel_id,
                    Some(nonce),
                    "session_fresh",
                    &json!({
                        "provider_session_id": provider_id,
                        "tmux_reused": false
                    }),
                )
        );
    }

    #[test]
    fn banner_is_top_and_sticky_across_streaming_then_terminal() {
        let shared = shared_with_v2(true);
        let channel_id = ChannelId::new(4147);
        arm_session(&shared, channel_id, "AgentDesk-claude#nonce-a", "session-a");

        let streaming = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Claude,
            "discord:4147:1",
            true,
            "부분 답변".to_string(),
        );
        let terminal = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Claude,
            "discord:4147:1",
            true,
            "완성된 답변".to_string(),
        );

        assert!(streaming.starts_with(BANNER));
        assert!(terminal.starts_with(BANNER));
        assert_eq!(streaming.matches(BANNER).count(), 1);
        assert_eq!(terminal.matches(BANNER).count(), 1);
    }

    #[test]
    fn later_turn_same_session_is_not_bannered_but_new_session_rearms() {
        let shared = shared_with_v2(true);
        let channel_id = ChannelId::new(4148);
        arm_session(&shared, channel_id, "AgentDesk-claude#nonce-a", "session-a");
        let _ = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Claude,
            "discord:4148:1",
            true,
            "첫 턴".to_string(),
        );

        let warm = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Claude,
            "discord:4148:2",
            true,
            "다음 턴".to_string(),
        );
        assert_eq!(warm, "다음 턴");

        arm_session(&shared, channel_id, "AgentDesk-claude#nonce-b", "session-b");
        let next_session = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Claude,
            "discord:4148:2",
            true,
            "새 세션 첫 답변".to_string(),
        );
        assert!(next_session.starts_with(BANNER));
    }

    #[test]
    fn zero_id_turns_use_offset_identity_and_missing_identity_does_not_claim() {
        let shared = shared_with_v2(true);
        let channel_id = ChannelId::new(4154);
        arm_session(&shared, channel_id, "AgentDesk-codex#nonce-a", "session-a");

        let missing_identity = DiscordTurnSessionBanner::new_with_turn_key(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            0,
            None,
            None,
        )
        .prefix(true, "unkeyed".to_string());
        assert_eq!(missing_identity, "unkeyed");

        let first = with_discord_turn_session_banner_identity_prefix(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            0,
            Some("2026-07-12T00:00:00Z"),
            Some(10),
            true,
            "first".to_string(),
        );
        let same_turn_terminal = with_discord_turn_session_banner_identity_prefix(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            0,
            Some("2026-07-12T00:00:00Z"),
            Some(10),
            true,
            "terminal".to_string(),
        );
        let next_turn_same_second = with_discord_turn_session_banner_identity_prefix(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            0,
            Some("2026-07-12T00:00:00Z"),
            Some(20),
            true,
            "next".to_string(),
        );

        assert!(first.starts_with(BANNER));
        assert!(same_turn_terminal.starts_with(BANNER));
        assert_eq!(next_turn_same_second, "next");
    }

    #[test]
    fn winning_turn_keeps_prefix_while_snapshot_is_temporarily_cleared() {
        let shared = shared_with_v2(true);
        let channel_id = ChannelId::new(4149);
        arm_session(&shared, channel_id, "AgentDesk-codex#nonce-a", "session-a");
        let _ = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            "discord:4149:1",
            true,
            "stream".to_string(),
        );
        assert!(
            shared
                .ui
                .placeholder_live_events
                .clear_session_panel(channel_id)
        );

        let terminal = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            "discord:4149:1",
            true,
            "terminal".to_string(),
        );
        let other_turn = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            "discord:4149:2",
            true,
            "other".to_string(),
        );
        assert!(terminal.starts_with(BANNER));
        assert_eq!(other_turn, "other");
    }

    #[test]
    fn non_first_empty_disabled_and_missing_snapshot_paths_do_not_claim() {
        let shared = shared_with_v2(true);
        let channel_id = ChannelId::new(4150);
        arm_session(&shared, channel_id, "AgentDesk-claude#nonce-a", "session-a");
        assert_eq!(
            with_session_banner_prefix(
                &shared,
                channel_id,
                &ProviderKind::Claude,
                "discord:4150:1",
                false,
                "tail".to_string(),
            ),
            "tail"
        );
        assert_eq!(
            with_session_banner_prefix(
                &shared,
                channel_id,
                &ProviderKind::Claude,
                "discord:4150:1",
                true,
                String::new(),
            ),
            ""
        );
        assert!(
            with_session_banner_prefix(
                &shared,
                channel_id,
                &ProviderKind::Claude,
                "discord:4150:1",
                true,
                "first real body".to_string(),
            )
            .starts_with(BANNER)
        );

        let disabled = shared_with_v2(false);
        arm_session(
            &disabled,
            ChannelId::new(4151),
            "AgentDesk-claude#nonce-a",
            "session-a",
        );
        assert_eq!(
            with_session_banner_prefix(
                &disabled,
                ChannelId::new(4151),
                &ProviderKind::Claude,
                "discord:4151:1",
                true,
                "body".to_string(),
            ),
            "body"
        );
        assert_eq!(
            with_session_banner_prefix(
                &shared,
                ChannelId::new(9999),
                &ProviderKind::Claude,
                "discord:9999:1",
                true,
                "body".to_string(),
            ),
            "body"
        );
    }

    #[test]
    fn prefixed_rollover_split_maps_back_to_raw_offset() {
        let raw = "응답".repeat(900);
        let prefix = "🆕 새 세션 시작\n\n";
        let rendered = format!("{prefix}{raw}");
        let rendered_split = prefix.len() + 777;
        assert_eq!(
            raw_split_after_session_banner_prefix(rendered_split, rendered.len(), raw.len(),),
            777
        );
    }

    #[test]
    fn real_rollover_plan_freezes_banner_but_advances_only_raw_bytes() {
        let shared = shared_with_v2(true);
        let channel_id = ChannelId::new(4152);
        arm_session(&shared, channel_id, "AgentDesk-codex#nonce-a", "session-a");
        let raw = "가나다 응답 본문 ".repeat(400);
        let rendered = with_session_banner_prefix(
            &shared,
            channel_id,
            &ProviderKind::Codex,
            "discord:4152:1",
            true,
            raw.clone(),
        );
        let plan = crate::services::discord::formatting::plan_streaming_rollover(
            &rendered,
            "⠸ 계속 처리 중",
        )
        .expect("prefixed production-sized body should roll over");
        let raw_split =
            raw_split_after_session_banner_prefix(plan.split_at, rendered.len(), raw.len());

        assert!(plan.frozen_chunk.starts_with(BANNER));
        assert!(raw_split > 0 && raw_split < raw.len());
        assert_eq!(
            plan.frozen_chunk
                .strip_prefix(&rendered[..rendered.len() - raw.len()])
                .expect("frozen first chunk keeps the exact wire prefix"),
            &raw[..raw_split]
        );
        assert_eq!(format!("{}{}", &raw[..raw_split], &raw[raw_split..]), raw);
    }

    #[test]
    fn production_paths_compose_prefix_and_never_emit_separate_banner_post() {
        let bridge_tick = include_str!("turn_bridge/stream_tick.rs");
        let watcher_tick = include_str!("tmux_watcher/streaming_status_tick.rs");
        let watcher_banner = include_str!("tmux_watcher/streaming_session_banner.rs");
        let bridge_terminal = include_str!("turn_bridge/terminal_outcome_delivery.rs");
        let bridge_cancel =
            include_str!("turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs");
        let watcher_terminal = include_str!("tmux_watcher/terminal_direct_fallback.rs");
        let session_sink = include_str!("session_relay_sink/relay_format.rs");
        let standby = include_str!("standby_relay.rs");
        let bridge_lifecycle = include_str!("turn_bridge/panel_lifecycle.rs");
        let watcher_lifecycle = include_str!("tmux_watcher/orphan_status_panel_cleanup.rs");

        assert!(
            bridge_tick
                .matches("turn_session_banner_identity_prefix")
                .count()
                >= 2
        );
        assert!(watcher_tick.contains("with_session_banner"));
        assert!(
            watcher_banner
                .matches("turn_session_banner_identity_prefix")
                .count()
                >= 2
        );
        assert!(bridge_terminal.contains("DiscordTurnSessionBanner"));
        assert!(bridge_cancel.contains("DiscordTurnSessionBanner"));
        assert!(bridge_cancel.matches("banner.prefix").count() >= 2);
        assert!(watcher_terminal.contains("prefix_watcher_terminal_session_banner"));
        assert!(session_sink.contains("with_discord_turn_session_banner_identity_prefix"));
        assert!(standby.contains("turn_session_banner_identity_prefix"));
        assert!(!bridge_lifecycle.contains("emit_session_banner_if_new"));
        assert!(!watcher_lifecycle.contains("emit_session_banner_if_new"));
    }
}
