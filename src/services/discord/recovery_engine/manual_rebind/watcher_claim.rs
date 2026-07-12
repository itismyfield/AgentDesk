use super::*;

/// Claim under the single-watcher policy. Normal recovery reuses a live
/// same-session watcher; a proven crossed Codex turn forces a fresh generation.
pub(super) fn claim_rebind_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    crossed_codex_turn: bool,
) -> (bool, bool) {
    let claim = if crossed_codex_turn {
        super::tmux::claim_or_replace_watcher(
            watchers,
            channel_id,
            handle,
            provider,
            "recovery_restore_inflight_crossed_codex_turn",
        )
    } else {
        super::tmux::claim_or_reuse_watcher(
            watchers,
            channel_id,
            handle,
            provider,
            "recovery_restore_inflight",
        )
    };
    (claim.should_spawn(), claim.replaced_existing())
}
