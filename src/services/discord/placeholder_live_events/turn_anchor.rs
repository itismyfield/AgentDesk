//! #3811: deterministic turn-anchor rendering + store plumbing for the
//! status/result surfaces.
//!
//! The `턴 트리거:` original-request line is built from ADK relay metadata
//! (`guild_id` + `channel_id` + the real Discord `user_msg_id`), never from
//! agent prose. It deliberately renders ONLY for genuine interactive Discord
//! turns: headless / synthetic / voice / id-0 turns carry no real user message,
//! so they get no link (the `대상:` target tags still render from the existing
//! task-panel snapshot — see `task_panel::render_task_panel_line`). The render
//! helpers stay free/pure; the store hooks live here too so the already-full
//! `status_panel.rs` / `mod.rs` need only the minimal field + call sites.

use std::sync::Mutex;

use poise::serenity_prelude::ChannelId;

use super::status_panel::StatusPanelState;
use crate::services::discord::is_synthetic_headless_message_id_raw;

/// Builds the `턴 트리거:` original-request deeplink, or `None` when no real
/// Discord user message backs the turn (headless / synthetic / voice / id-0) or
/// the process has no configured guild id.
///
/// Gating: id `0` is the id-less sentinel; the synthetic-headless floor
/// (`SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR`, 8e18) sits BELOW both the voice
/// (`INTERNAL_VOICE_MESSAGE_ID_START`, 9e18) and headless
/// (`HEADLESS_TURN_MESSAGE_ID_BASE`, 9.1e18) id bases, so the single floor check
/// rejects every synthetic id class with no fake link. Real Discord snowflakes
/// stay well below the floor.
pub(super) fn render_request_anchor_line(
    request_user_msg_id: Option<u64>,
    channel_id: ChannelId,
    guild_id: Option<&str>,
) -> Option<String> {
    let user_msg_id = request_user_msg_id?;
    if user_msg_id == 0 || is_synthetic_headless_message_id_raw(user_msg_id) {
        return None;
    }
    let guild_id = guild_id.map(str::trim).filter(|guild| !guild.is_empty())?;
    // #3983 item 3: the label reads `턴 트리거:` (the turn's triggering request),
    // rendered as the LAST footer line by `render_status_panel` (the completion
    // footer still prepends it via `prepend_request_anchor`).
    Some(format!(
        "턴 트리거: https://discord.com/channels/{guild_id}/{}/{user_msg_id}",
        channel_id.get()
    ))
}

/// Prepends the `요청:` anchor as the FIRST panel/footer section when present, so
/// it leads the surface and is the LAST section dropped by the trailing-section
/// overflow trim — surviving on the first visible chunk when the body splits.
pub(super) fn prepend_request_anchor(
    sections: &mut Vec<String>,
    request_anchor_line: Option<String>,
) {
    if let Some(anchor) = request_anchor_line.filter(|line| !line.trim().is_empty()) {
        sections.insert(0, anchor);
    }
}

impl super::PlaceholderLiveEvents {
    /// #3811: records the real Discord `user_msg_id` of the turn's original
    /// request (or clears it with `None`) so the `요청:` deeplink can render on
    /// the status panel and completion footer. Called at interactive intake with
    /// the real id, and with `None` on the TUI-direct path so a prior interactive
    /// link never leaks onto a later id-0 turn. Idempotent.
    pub(in crate::services::discord) fn set_turn_request_anchor(
        &self,
        channel_id: ChannelId,
        user_msg_id: Option<u64>,
    ) {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.request_user_msg_id = user_msg_id;
    }

    /// #3811: builds the `요청:` line for a channel's snapshot, reading the
    /// process-global guild id at render time — the same `load_graceful()` layer
    /// `render_status_panel` already uses for `cluster`. `None` for headless /
    /// synthetic / voice / id-0 turns or when no guild id is configured.
    pub(super) fn request_anchor_line(
        &self,
        channel_id: ChannelId,
        snapshot: &StatusPanelState,
    ) -> Option<String> {
        let config = crate::config::load_graceful();
        render_request_anchor_line(
            snapshot.request_user_msg_id,
            channel_id,
            config.discord.guild_id.as_deref(),
        )
    }

    /// #3811: test-only accessor for the stored anchor id, so the lifecycle tests
    /// can assert preserve-across-turn-reset / clear-on-reset without depending on
    /// the (config-dependent) rendered link.
    #[cfg(test)]
    pub(in crate::services::discord) fn request_user_msg_id_for_test(
        &self,
        channel_id: ChannelId,
    ) -> Option<u64> {
        self.status_by_channel.get(&channel_id).and_then(|entry| {
            entry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .request_user_msg_id
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real Discord snowflake (well below the 8e18 synthetic floor).
    const REAL_USER_MSG_ID: u64 = 1_520_312_799_245_504_542;
    const GUILD: &str = "1469870512812462284";
    const CHANNEL: u64 = 1475086789696946196;

    #[test]
    fn normal_turn_with_real_id_and_guild_renders_request_link() {
        let line = render_request_anchor_line(
            Some(REAL_USER_MSG_ID),
            ChannelId::new(CHANNEL),
            Some(GUILD),
        );
        assert_eq!(
            line.as_deref(),
            Some(
                "턴 트리거: https://discord.com/channels/1469870512812462284/1475086789696946196/1520312799245504542"
            )
        );
    }

    #[test]
    fn missing_user_msg_id_omits_link() {
        assert_eq!(
            render_request_anchor_line(None, ChannelId::new(CHANNEL), Some(GUILD)),
            None
        );
    }

    #[test]
    fn id_zero_omits_link() {
        assert_eq!(
            render_request_anchor_line(Some(0), ChannelId::new(CHANNEL), Some(GUILD)),
            None
        );
    }

    #[test]
    fn synthetic_headless_id_omits_link() {
        // HEADLESS_TURN_MESSAGE_ID_BASE = 9.1e18, above the 8e18 floor.
        let headless = 9_100_000_000_000_000_001;
        assert_eq!(
            render_request_anchor_line(Some(headless), ChannelId::new(CHANNEL), Some(GUILD)),
            None
        );
    }

    #[test]
    fn synthetic_voice_id_omits_link() {
        // INTERNAL_VOICE_MESSAGE_ID_START = 9e18, above the 8e18 floor.
        let voice = 9_000_000_000_000_000_007;
        assert_eq!(
            render_request_anchor_line(Some(voice), ChannelId::new(CHANNEL), Some(GUILD)),
            None
        );
    }

    #[test]
    fn missing_guild_omits_link() {
        assert_eq!(
            render_request_anchor_line(Some(REAL_USER_MSG_ID), ChannelId::new(CHANNEL), None),
            None
        );
    }

    #[test]
    fn blank_guild_omits_link() {
        assert_eq!(
            render_request_anchor_line(
                Some(REAL_USER_MSG_ID),
                ChannelId::new(CHANNEL),
                Some("   ")
            ),
            None
        );
    }

    #[test]
    fn prepend_inserts_anchor_first_and_skips_blank() {
        let mut sections = vec!["header".to_string()];
        prepend_request_anchor(&mut sections, Some("요청: link".to_string()));
        assert_eq!(
            sections,
            vec!["요청: link".to_string(), "header".to_string()]
        );

        let mut none_sections = vec!["header".to_string()];
        prepend_request_anchor(&mut none_sections, None);
        prepend_request_anchor(&mut none_sections, Some("   ".to_string()));
        assert_eq!(none_sections, vec!["header".to_string()]);
    }
}
