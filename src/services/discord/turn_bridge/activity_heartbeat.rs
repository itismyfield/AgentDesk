//! Active-turn tmux activity-heartbeat refresh helpers.

use super::*;

fn active_turn_thread_channel_id(
    adk_session_name: Option<&str>,
    inflight_state: &InflightTurnState,
) -> Option<u64> {
    adk_session_name
        .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name)
        .or_else(|| {
            inflight_state
                .channel_name
                .as_deref()
                .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name)
        })
        .or(inflight_state.thread_id)
}

pub(in crate::services::discord) fn maybe_refresh_active_turn_activity_heartbeat(
    shared: &SharedData,
    provider: &ProviderKind,
    inflight_state: &InflightTurnState,
    adk_session_name: Option<&str>,
    last_heartbeat_at: &mut Option<std::time::Instant>,
) {
    maybe_refresh_active_turn_activity_heartbeat_at(
        shared,
        provider,
        inflight_state,
        adk_session_name,
        last_heartbeat_at,
        std::time::Instant::now(),
    );
}

#[cfg(unix)]
fn maybe_refresh_active_turn_activity_heartbeat_at(
    shared: &SharedData,
    provider: &ProviderKind,
    inflight_state: &InflightTurnState,
    adk_session_name: Option<&str>,
    last_heartbeat_at: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) {
    if last_heartbeat_at.is_some_and(|last| {
        now.duration_since(last) < super::super::tmux::WATCHER_ACTIVITY_HEARTBEAT_INTERVAL
    }) {
        return;
    }

    let Some(tmux_session_name) = inflight_state.tmux_session_name.as_deref() else {
        return;
    };
    let thread_channel_id = active_turn_thread_channel_id(adk_session_name, inflight_state);

    if super::super::tmux::refresh_session_heartbeat_from_tmux_output(
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        provider,
        tmux_session_name,
        thread_channel_id,
    ) {
        *last_heartbeat_at = Some(now);
    }
}

#[cfg(not(unix))]
fn maybe_refresh_active_turn_activity_heartbeat_at(
    _shared: &SharedData,
    _provider: &ProviderKind,
    _inflight_state: &InflightTurnState,
    _adk_session_name: Option<&str>,
    _last_heartbeat_at: &mut Option<std::time::Instant>,
    _now: std::time::Instant,
) {
}
