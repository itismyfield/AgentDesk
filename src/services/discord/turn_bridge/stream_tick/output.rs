use super::*;
use std::ops::ControlFlow;

// Edit the authorized status panel before the footer and response phases run.
pub(super) async fn refresh_status_panel(
    ctx: &runtime::TickContext<'_>,
    tick: &mut runtime::TickRuntime<'_>,
    state_dirty: &mut bool,
    status_msg_id: MessageId,
) {
    let runtime::TickContext {
        channel_id,
        gateway,
        provider,
        shared_owned,
        status_panel_started_at,
        ..
    } = *ctx;
    let provider = ProviderRef(provider);
    let panel_text = shared_owned.ui.placeholder_live_events.render_status_panel(
        channel_id,
        &provider,
        status_panel_started_at,
    );
    let panel_cache_invalidation_epoch = shared_owned
        .ui
        .placeholder_live_events
        .panel_cache_invalidation_epoch(channel_id, status_msg_id.get());
    if panel_cache_invalidation_epoch.is_some() || panel_text != *tick.last_status_panel_text {
        match TurnGateway::edit_message(gateway.as_ref(), channel_id, status_msg_id, &panel_text)
            .await
        {
            Ok(()) => {
                *tick.last_status_panel_text = panel_text;
                *tick.last_status_panel_edit = tokio::time::Instant::now();
                if let Some(epoch) = panel_cache_invalidation_epoch {
                    shared_owned
                        .ui
                        .placeholder_live_events
                        .clear_panel_cache_invalidation_if_epoch(
                            channel_id,
                            status_msg_id.get(),
                            epoch,
                        );
                }
                tick.inflight_state.status_message_id = Some(status_msg_id.get());
                *state_dirty = true;
            }
            Err(error) => {
                tracing::warn!(
                    target: "agentdesk::services::discord::turn_bridge::stream_tick",
                    "[turn_bridge] failed to edit status-panel-v2 message {} in channel {}: {}",
                    status_msg_id,
                    channel_id,
                    error
                );
            }
        }
    }
    *tick.status_panel_dirty = false;
}
