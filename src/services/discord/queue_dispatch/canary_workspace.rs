//! The canary keeps its queued input while nothing could run it in the configured workspace.

use super::super::*;
use crate::services::codex_tui::canary::CANARY_CHANNEL;

const OWNER: &str = "canary_workspace";

/// Without a core session intake can only auto-start, and an unusable workspace would consume the
/// input with an error reply. Not cached, so a config reload is picked up on the next kick.
pub(super) async fn parked(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> bool {
    if !matches!(provider, ProviderKind::Codex) || channel.get() != CANARY_CHANNEL {
        return false;
    }
    let no_core_session = !shared.core.lock().await.sessions.contains_key(&channel);
    if no_core_session
        && crate::services::discord::settings::resolve_workspace(channel, None)
            .is_none_or(|workspace| !std::path::Path::new(&workspace).is_dir())
    {
        let reason = "workspace_unresolved";
        shared
            .queue_park_ledger
            .note_promote_hold(provider, channel, OWNER, reason);
        return true;
    }
    shared.queue_park_ledger.clear_promote_hold(channel, OWNER);
    false
}

#[cfg(test)]
#[path = "canary_workspace_tests.rs"]
mod tests;
