//! A channel without a core session has no session name, so its own checks decide promotion.

use super::*;

/// The canary keeps its verified hold here too, so an unretired pin parks the input instead of
/// letting the launch take it and wait; otherwise the nameless host check decides.
pub(super) async fn nameless_promote_blocked(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    let park = &shared.queue_park_ledger;
    #[cfg(unix)]
    {
        use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
        if matches!(provider, ProviderKind::Codex)
            && channel_id.get() == CANARY_CHANNEL
            && crate::services::tui_prompt_dedupe::clear_relaunch::launch_blocked(CANARY_TMUX)
        {
            park.note_codex_verified_hold(channel_id, CANARY_TMUX);
            return true;
        }
    }
    park.clear_promote_hold(channel_id, "codex_verified");
    let nameless = crate::services::discord::host_defer_gate::nameless_channel_deferred;
    nameless(shared, provider, channel_id.get()).await
}
