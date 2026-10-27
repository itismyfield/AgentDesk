//! A non-TUI watcher may retain a request, never override its receipt with a legacy retry.

use super::PreEmitGuardContext;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::{inflight, turn_bridge};

pub(super) async fn blocked(context: &PreEmitGuardContext<'_>) -> bool {
    let loaded = inflight::load_inflight_state(context.watcher_provider, context.channel_id.get());
    let Some(state) = context.captured_turn.or(loaded.as_ref()) else {
        return false;
    };
    if matches!(
        state.runtime_kind,
        Some(RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui)
    ) {
        return false;
    }
    if state.replay_rerun_blocked() {
        return true;
    }
    turn_bridge::recovery_text::replay::lookup(
        context.shared,
        context.watcher_provider,
        state.channel_id,
        state.user_msg_id,
    )
    .await
    .unwrap_or(true)
}
