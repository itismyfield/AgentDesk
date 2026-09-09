//! #5191/#4229: the throttled tick's EXISTING-status-panel refresh/edit block,
//! moved verbatim out of `streaming_status_tick.rs` so the parent stays under its
//! namespace cap. The two original `if` blocks, every predicate, awaited call,
//! log and panel-cache epoch update are unchanged; the #5191 terminal gate lives
//! in the CALLER, never here.

use super::*;

pub(super) async fn update_existing_panel(
    ctx: &StreamingStatusTickContext<'_>,
    turn: &StreamingStatusTickTurn<'_>,
    turn_identity_for_panel: &Option<crate::services::discord::inflight::InflightTurnIdentity>,
    status_panel_msg_id: Option<serenity::MessageId>,
    mut last_status_panel_text: String,
) -> String {
    let http = ctx.http;
    let shared = ctx.shared;
    let channel_id = ctx.channel_id;
    let watcher_provider = ctx.watcher_provider;
    let tmux_session_name = ctx.tmux_session_name;
    let single_message_panel_footer_mode = turn.single_message_panel_footer_mode;
    let status_panel_started_at = turn.status_panel_started_at;
    if shared.ui.status_panel_v2_enabled
        && (single_message_panel_footer_mode || status_panel_msg_id.is_some())
    {
        // #3055: re-derive this turn's session lifecycle panel
        // line on the throttled status tick, matching bridge
        // behavior and avoiding stale per-channel snapshots.
        refresh_watcher_session_panel_from_lifecycle(
            &shared,
            channel_id,
            turn_identity_for_panel
                .as_ref()
                .map(|identity| identity.user_msg_id)
                .unwrap_or(0),
            &tmux_session_name,
        )
        .await;
    }
    if watcher_separate_status_panel_enabled(shared.ui.status_panel_v2_enabled)
        && let Some(status_msg_id) = status_panel_msg_id
    {
        let panel_text = shared.ui.placeholder_live_events.render_status_panel(
            channel_id,
            &watcher_provider,
            status_panel_started_at,
        );
        let panel_cache_invalidation_epoch = shared
            .ui
            .placeholder_live_events
            .panel_cache_invalidation_epoch(channel_id, status_msg_id.get());
        if panel_cache_invalidation_epoch.is_some() || panel_text != last_status_panel_text {
            rate_limit_wait(&shared, channel_id).await;
            match crate::services::discord::http::edit_channel_message(
                &http,
                channel_id,
                status_msg_id,
                &panel_text,
            )
            .await
            {
                Ok(_) => {
                    last_status_panel_text = panel_text;
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
    }
    last_status_panel_text
}
