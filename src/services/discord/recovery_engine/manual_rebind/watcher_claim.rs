use super::*;

/// Claim under the single-watcher policy. Normal recovery reuses a live
/// same-session watcher; a proven crossed Codex turn forces a fresh generation.
/// A withheld Herdr pane spawns and replaces nothing.
pub(super) fn claim_rebind_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    crossed_codex_turn: bool,
    thread_parent: Option<super::tmux::ThreadFollowUpParent>,
    host: super::tmux::WatchHost,
) -> (bool, bool) {
    let claim = if crossed_codex_turn {
        super::tmux::claim_or_replace_watcher_for_host(
            watchers,
            channel_id,
            handle,
            provider,
            "recovery_restore_inflight_crossed_codex_turn",
            thread_parent,
            host,
        )
    } else {
        super::tmux::claim_or_reuse_watcher_for_host(
            watchers,
            channel_id,
            handle,
            provider,
            "recovery_restore_inflight",
            thread_parent,
            host,
        )
    };
    claim.map_or((false, false), |claim| {
        (claim.should_spawn(), claim.replaced_existing())
    })
}
