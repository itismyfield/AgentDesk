use super::*;

pub(super) async fn compensate_post_commit_relay_setup_failure(
    reservation: super::restore_watcher_claim::RecoveryWatcherReservation,
    locked_episode: Option<super::inflight::LockedInflightEpisode>,
    inflight_rollback: Option<PendingRebindInflightRollback>,
) -> (bool, String) {
    drop(locked_episode);
    let inflight_rollback = inflight_rollback
        .map(PendingRebindInflightRollback::apply)
        .unwrap_or_else(|| "none".to_string());
    let registry_rolled_back = reservation
        .abandon("post_commit_relay_setup_failure", None)
        .await;
    (registry_rolled_back, inflight_rollback)
}

/// Claim under the single-watcher policy. Normal recovery reuses a live
/// same-session watcher; a proven crossed Codex turn forces a fresh generation.
pub(super) fn claim_rebind_watcher(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    state: &super::inflight::InflightTurnState,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    crossed_codex_turn: bool,
    thread_parent: Option<super::tmux::ThreadFollowUpParent>,
) -> (
    Option<super::restore_watcher_claim::RecoveryWatcherReservation>,
    bool,
) {
    super::restore_watcher_claim::reserve_rebind_watcher(
        shared,
        provider,
        state,
        channel_id,
        handle,
        crossed_codex_turn,
        thread_parent,
    )
}
