//! In-memory session and pending-reset state cleared when a user ends a session.

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::super::super::SharedData;

/// The cleared channel's in-memory provider session, history, uploads and role override.
pub(super) async fn clear_session_memory(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) {
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            super::super::super::settings::cleanup_channel_uploads(channel_id);
            session.clear_provider_session();
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }
    shared.dispatch.role_overrides.remove(&channel_id);
}

/// A process reset applies the pending fast-mode, goals and model resets, so their markers go.
pub(super) async fn clear_process_reset_pending(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) {
    super::super::config::clear_fast_mode_reset_pending_for_channel(shared, channel_id);
    super::super::config::clear_codex_goals_reset_pending_for_channel(shared, channel_id);
    let overrides = &shared.overrides;
    overrides.model_session_reset_pending.remove(&channel_id);
    overrides.session_reset_pending.remove(&channel_id);
    super::clear_all_fast_mode_reset_markers(shared, channel_id).await;
    super::persist_codex_goals_reset_marker(shared, channel_id, false).await;
    super::relaunch::forget(channel_id);
}
