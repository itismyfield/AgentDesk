use super::*;

/// Claim under the single-watcher policy. Normal recovery reuses a live
/// same-session watcher; a proven crossed Codex turn forces a fresh generation.
pub(super) fn claim_rebind_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    crossed_codex_turn: bool,
    thread_parent: Option<super::tmux::ThreadFollowUpParent>,
) -> (bool, bool, bool, ChannelId) {
    let claim = if crossed_codex_turn {
        super::tmux::claim_or_replace_watcher_with_thread_parent(
            watchers,
            channel_id,
            handle,
            provider,
            "recovery_restore_inflight_crossed_codex_turn",
            thread_parent,
        )
    } else {
        super::tmux::claim_or_reuse_watcher_with_thread_parent(
            watchers,
            channel_id,
            handle,
            provider,
            "recovery_restore_inflight",
            thread_parent,
        )
    };
    let should_spawn = claim.should_spawn();
    let owner_channel_id = claim.owner_channel_id();
    (
        should_spawn,
        claim.replaced_existing(),
        should_spawn || owner_channel_id == channel_id,
        owner_channel_id,
    )
}
