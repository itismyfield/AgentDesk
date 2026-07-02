use poise::serenity_prelude::ChannelId;

use super::SharedData;
use crate::services::provider::ProviderKind;

#[cfg(unix)]
fn hosted_tui_ready_state_blocks_idle_queue(
    ready_state: Option<crate::services::tui_turn_state::TuiReadyState>,
    stale_user_submitted: bool,
) -> bool {
    matches!(
        ready_state,
        Some(crate::services::tui_turn_state::TuiReadyState::Busy)
    ) && !stale_user_submitted
}

#[cfg(unix)]
pub(super) async fn idle_queue_blocked_by_hosted_tui_busy_pane(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> bool {
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
        || !provider.uses_managed_tmux_backend()
        || !crate::services::claude::is_tmux_available()
    {
        return false;
    }

    let selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            provider,
            true,
            Some(channel_id.get()),
        );
    if selection.driver != crate::services::provider_hosting::ProviderSessionDriver::TuiHosting {
        return false;
    }

    if matches!(provider, ProviderKind::Claude)
        && crate::services::claude_tui::hook_server::current_hook_endpoint().is_none()
    {
        return false;
    }

    let tmux_session_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
            .map(|name| provider.build_tmux_session_name(&name))
    };
    let Some(tmux_session_name) = tmux_session_name else {
        return false;
    };
    if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name) {
        return false;
    }

    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux_session_name);
    let ready_state = binding.as_ref().and_then(|binding| {
        crate::services::tui_turn_state::runtime_binding_ready_for_input(provider, binding, true)
    });
    let stale_user_submitted = matches!(provider, ProviderKind::Claude)
        && ready_state == Some(crate::services::tui_turn_state::TuiReadyState::Busy)
        && binding
            .as_ref()
            .and_then(|binding| {
                crate::services::tui_turn_state::runtime_binding_turn_state(provider, binding)
            })
            .is_some_and(|turn_state| {
                let activity_age_secs =
                    crate::services::tui_turn_state::runtime_activity_age_secs(&tmux_session_name);
                let prompt_marker_detected =
                    crate::services::claude_tui::input::prompt_readiness_snapshot(
                        &tmux_session_name,
                    )
                    .prompt_marker_detected;
                let stale = crate::services::tui_turn_state::user_submitted_is_stale_stranded(
                    turn_state,
                    activity_age_secs,
                    prompt_marker_detected,
                );
                if stale {
                    tracing::warn!(
                        channel_id = channel_id.get(),
                        provider = provider.as_str(),
                        tmux_session_name = %tmux_session_name,
                        turn_state = turn_state.as_str(),
                        activity_age_secs,
                        "idle queue kickoff proceeding through stale hosted TUI Busy state"
                    );
                }
                stale
            });

    let blocked = hosted_tui_ready_state_blocks_idle_queue(ready_state, stale_user_submitted);
    if blocked {
        tracing::info!(
            channel_id = channel_id.get(),
            provider = provider.as_str(),
            tmux_session_name = %tmux_session_name,
            ready_state = ready_state
                .map(crate::services::tui_turn_state::TuiReadyState::as_str)
                .unwrap_or("unavailable"),
            "idle queue kickoff deferred while hosted TUI structured turn state is busy"
        );
    }
    blocked
}

#[cfg(not(unix))]
pub(super) async fn idle_queue_blocked_by_hosted_tui_busy_pane(
    _shared: &SharedData,
    _provider: &ProviderKind,
    _channel_id: ChannelId,
) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::hosted_tui_ready_state_blocks_idle_queue;
    use crate::services::tui_turn_state::TuiReadyState;

    #[test]
    fn fresh_busy_blocks_idle_queue() {
        assert!(hosted_tui_ready_state_blocks_idle_queue(
            Some(TuiReadyState::Busy),
            false,
        ));
    }

    #[test]
    fn stale_busy_does_not_block_idle_queue() {
        assert!(!hosted_tui_ready_state_blocks_idle_queue(
            Some(TuiReadyState::Busy),
            true,
        ));
    }

    #[test]
    fn ready_unknown_and_unavailable_do_not_block_idle_queue() {
        for ready_state in [
            Some(TuiReadyState::Ready),
            Some(TuiReadyState::Unknown),
            None,
        ] {
            assert!(!hosted_tui_ready_state_blocks_idle_queue(
                ready_state,
                false,
            ));
            assert!(!hosted_tui_ready_state_blocks_idle_queue(ready_state, true,));
        }
    }
}
