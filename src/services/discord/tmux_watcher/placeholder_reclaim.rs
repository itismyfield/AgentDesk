//! #3351 relay-placeholder orphan reclaim helpers (sibling of the #3003
//! status-panel orphan arms in the parent watcher loop).

use super::*;

/// #3351 (#3003 r21 mirror): drop a durable orphan record once the placeholder
/// lifecycle finished for the turn (consumed into the final response, deleted,
/// or intentionally preserved with content) so a later drain cannot delete a
/// message that is no longer an orphan spinner.
pub(super) fn drop_placeholder_orphan_record(
    provider: &ProviderKind,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    msg_id: serenity::MessageId,
) {
    crate::services::discord::status_panel_orphan_store::remove(
        provider,
        &shared.token_hash,
        channel_id.get(),
        msg_id.get(),
    );
}

/// #3351: reclaim the same turn's relay placeholder alongside the orphan status
/// panel. Caller has already passed `watcher_should_reclaim_orphan_turn_placeholder`.
/// Outcome handling mirrors the panel arm (#3003 r10/r16): transient failure keeps
/// the local id for an in-turn retry + enqueues a durable record; committed /
/// permanent failure drops the handles and compare-and-clears the persisted
/// `current_msg_id` (#3077 pattern) so a later segment cannot edit the stale id.
/// The return value is NOT wired into finalize decisions (panel defer semantics
/// #3003 r5/r12 unchanged).
#[allow(clippy::too_many_arguments)]
pub(super) async fn reclaim_orphan_external_input_placeholder(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    placeholder_msg_id: &mut Option<serenity::MessageId>,
    placeholder_from_restored_inflight: &mut bool,
    last_edit_text: &mut String,
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> bool {
    let Some(msg_id) = *placeholder_msg_id else {
        return true;
    };
    let outcome = delete_nonterminal_placeholder(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        msg_id,
        "watcher_orphan_external_input_placeholder_cleanup",
    )
    .await;
    if !outcome.is_committed() && !outcome.is_permanent_failure() {
        crate::services::discord::status_panel_orphan_store::enqueue(
            provider,
            &shared.token_hash,
            channel_id.get(),
            msg_id.get(),
        );
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: orphan placeholder delete did not commit for channel {} msg {}; kept local id + enqueued durable retry",
            channel_id.get(),
            msg_id.get()
        );
        return false;
    }
    // #3351 (#3003 r21 mirror): an earlier transient attempt this turn may have
    // enqueued this placeholder in the durable store; the delete has now
    // committed (or permanently failed and is treated as committed), so drop
    // the record before the local handle is cleared.
    crate::services::discord::status_panel_orphan_store::remove(
        provider,
        &shared.token_hash,
        channel_id.get(),
        msg_id.get(),
    );
    if !outcome.is_committed() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: orphan placeholder delete permanently failed for channel {} msg {}; giving up (treated as committed)",
            channel_id.get(),
            msg_id.get()
        );
    }
    *placeholder_msg_id = None;
    *placeholder_from_restored_inflight = false;
    last_edit_text.clear();
    let _ = crate::services::discord::inflight::clear_current_msg_if_matches(
        provider,
        channel_id.get(),
        msg_id.get(),
        Some(tmux_session_name),
    );
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 watcher: cleaned orphan relay placeholder for TUI-direct turn (channel {}, tmux={}, msg={})",
        channel_id.get(),
        tmux_session_name,
        msg_id.get()
    );
    true
}
