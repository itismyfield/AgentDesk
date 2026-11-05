use super::*;

pub(super) fn streaming_source(
    ctx: &StreamingStatusTickContext<'_>,
    pin: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> (bool, BodySource) {
    let state = crate::services::discord::inflight::load_inflight_state(
        ctx.watcher_provider,
        ctx.channel_id.get(),
    );
    let silent = state.as_ref().is_some_and(|state| state.silent_turn);
    let owner = state.as_ref().filter(|state| {
        pin.is_some_and(|pin| pin.matches_state(state))
            && state.tmux_session_name.as_deref() == Some(ctx.tmux_session_name.as_str())
    });
    (silent, watcher_body_source(owner, ctx.source_authority))
}

pub(super) fn reanchor_inflight(
    ctx: &StreamingStatusTickContext<'_>,
    two_message_panel_enabled: bool,
    watcher_did_rollover_this_interval: bool,
) -> Option<crate::services::discord::inflight::InflightTurnState> {
    let watcher_provider = ctx.watcher_provider;
    let channel_id = ctx.channel_id;
    if two_message_panel_enabled && watcher_did_rollover_this_interval {
        crate::services::discord::inflight::load_inflight_state(&watcher_provider, channel_id.get())
    } else {
        None
    }
}

pub(super) fn inflight_missing_for_streaming(ctx: &StreamingStatusTickContext<'_>) -> bool {
    let watcher_provider = ctx.watcher_provider;
    let channel_id = ctx.channel_id;
    crate::services::discord::inflight::load_inflight_state(&watcher_provider, channel_id.get())
        .is_none()
}
