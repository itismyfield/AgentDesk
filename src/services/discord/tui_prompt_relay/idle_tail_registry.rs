//! Registry of live Claude idle tails, which may own a channel's mailbox token.

use std::sync::{LazyLock, Mutex};

use poise::serenity_prelude::ChannelId;

use super::super::SharedData;
use super::super::outbound::delivery_record as dr;
use crate::services::provider::ProviderKind;

/// Live Claude idle tails keyed by tmux session, valued by the channel they relay into.
pub(super) static CLAUDE_IDLE_RESPONSE_TAILS: LazyLock<
    Mutex<std::collections::HashMap<String, ChannelId>>,
> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// A registered tail means an idle tail or the bridge it spawned may still own this
/// channel's mailbox token; the entry outlives the tail task until that bridge exits.
pub(in crate::services::discord) fn claude_idle_response_tail_active_for_channel(
    channel_id: ChannelId,
) -> bool {
    CLAUDE_IDLE_RESPONSE_TAILS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .any(|registered| *registered == channel_id)
}

/// Committed delivery offset for a Claude transcript, after the same generation/regression
/// watermark resets the watcher runs, so a stale prior-wrapper watermark never clamps forward.
pub(in crate::services::discord) fn claude_transcript_committed_offset(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_session_name: &str,
    transcript_eof: Option<u64>,
    source: &'static str,
) -> u64 {
    #[cfg(unix)]
    super::super::tmux::reset_stale_relay_watermark_if_output_regressed(
        shared,
        channel_id,
        tmux_session_name,
        transcript_eof.unwrap_or(0),
        source,
    );
    #[cfg(unix)]
    super::super::tmux::reset_relay_watermark_on_generation_change(
        shared,
        channel_id,
        tmux_session_name,
        source,
    );
    dr::effective_committed_offset(
        shared,
        &ProviderKind::Claude,
        channel_id,
        tmux_session_name,
        transcript_eof,
    )
}

#[cfg(test)]
pub(in crate::services::discord) fn register_claude_idle_tail_for_tests(
    tmux_session_name: &str,
    channel_id: ChannelId,
) -> std::sync::Arc<dyn std::any::Any + Send + Sync> {
    CLAUDE_IDLE_RESPONSE_TAILS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(tmux_session_name.to_string(), channel_id);
    std::sync::Arc::new(ClaudeIdleTailGuard {
        tmux_session_name: tmux_session_name.to_string(),
    })
}

pub(super) struct ClaudeIdleTailGuard {
    pub(super) tmux_session_name: String,
}

impl Drop for ClaudeIdleTailGuard {
    fn drop(&mut self) {
        CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.tmux_session_name);
    }
}
