use super::*;

pub(super) fn streaming_silent_turn(ctx: &StreamingStatusTickContext<'_>) -> bool {
    let watcher_provider = ctx.watcher_provider;
    let channel_id = ctx.channel_id;
    crate::services::discord::inflight::load_inflight_state(&watcher_provider, channel_id.get())
        .map(|state| state.silent_turn)
        .unwrap_or(false)
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
