use super::*;

pub(super) async fn refresh_existing_status_panel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    watcher_provider: &ProviderKind,
    tmux_session_name: &str,
    turn_identity_for_panel: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    status_panel_started_at: i64,
    status_panel_msg_id: Option<serenity::MessageId>,
    last_status_panel_text: &mut String,
) {
    if shared.ui.status_panel_v2_enabled {
        refresh_watcher_session_panel_from_lifecycle(
            shared,
            channel_id,
            turn_identity_for_panel
                .map(|identity| identity.user_msg_id)
                .unwrap_or(0),
            tmux_session_name,
        )
        .await;
    }
    if !watcher_separate_status_panel_enabled(shared.ui.status_panel_v2_enabled) {
        return;
    }
    let Some(status_msg_id) = status_panel_msg_id else {
        return;
    };
    let panel_text = shared.ui.placeholder_live_events.render_status_panel(
        channel_id,
        watcher_provider,
        status_panel_started_at,
    );
    let panel_cache_invalidation_epoch = shared
        .ui
        .placeholder_live_events
        .panel_cache_invalidation_epoch(channel_id, status_msg_id.get());
    if panel_cache_invalidation_epoch.is_none() && panel_text == *last_status_panel_text {
        return;
    }
    rate_limit_wait(shared, channel_id).await;
    match crate::services::discord::http::edit_channel_message(
        http,
        channel_id,
        status_msg_id,
        &panel_text,
    )
    .await
    {
        Ok(_) => {
            *last_status_panel_text = panel_text;
            if let Some(epoch) = panel_cache_invalidation_epoch {
                shared
                    .ui
                    .placeholder_live_events
                    .clear_panel_cache_invalidation_if_epoch(
                        channel_id,
                        status_msg_id.get(),
                        epoch,
                    );
            }
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ tmux status-panel-v2 edit failed for msg {} in channel {}: {}",
                status_msg_id.get(),
                channel_id.get(),
                error
            );
        }
    }
}
