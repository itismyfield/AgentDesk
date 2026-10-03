use super::*;

/// Claim under the single-watcher policy. Normal recovery reuses a live
/// same-session watcher; a proven crossed Codex turn forces a fresh generation.
/// `Err` is a withheld Herdr pane: nothing spawned, replaced or reused.
pub(super) fn claim_rebind_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    crossed_codex_turn: bool,
    thread_parent: Option<super::tmux::ThreadFollowUpParent>,
    host: super::tmux::WatchHost,
) -> Result<(bool, bool), super::tmux::WatchWithheld> {
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
    claim.map(|claim| (claim.should_spawn(), claim.replaced_existing()))
}

/// `WatcherWithheld` when the Herdr admission would withhold the pane's claim now, read before the
/// rebind's first write so a repeat changes nothing; any other or still unnamed pane passes.
#[cfg(unix)]
pub(super) async fn admitted(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_override: &Option<String>,
) -> Result<(), RebindError> {
    let name = match tmux_session_override {
        Some(name) => name.clone(),
        None => {
            let data = shared.core.lock().await;
            let session = data.sessions.get(&channel_id);
            let Some(channel_name) = session.and_then(|s| s.channel_name.clone()) else {
                return Ok(());
            };
            provider.build_tmux_session_name(&channel_name)
        }
    };
    let host = super::tmux::watch_host_of(shared, provider, channel_id.get(), &name).await;
    let herdr_host = host == super::tmux::WatchHost::Herdr;
    match crate::services::tui_prompt_dedupe::herdr_claim_admission(&name, herdr_host) {
        Ok(_) => Ok(()),
        Err(()) => Err(RebindError::WatcherWithheld { tmux_session: name }),
    }
}
