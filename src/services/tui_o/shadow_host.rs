//! Read-only target discovery retained for relay-state invariant tests.

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::shadow::binding_reader::ShadowTarget;

/// Allowlisted channels with a live TUI binding, read without purging or waiting on relay state.
pub(crate) fn discover_targets(allowlist: &[u64]) -> Vec<ShadowTarget> {
    if allowlist.is_empty() {
        return Vec::new();
    }
    use crate::services::tui_prompt_dedupe::peek_tui_session_channels;
    let kinds = [RuntimeHandoffKind::ClaudeTui, RuntimeHandoffKind::CodexTui];
    let sessions = peek_tui_session_channels(&kinds).unwrap_or_default();
    let allowed = sessions
        .into_iter()
        .filter(|(_, channel_id)| allowlist.contains(channel_id));
    let target = |(tmux_session, channel_id)| ShadowTarget {
        channel_id,
        tmux_session,
    };
    allowed.map(target).collect()
}
