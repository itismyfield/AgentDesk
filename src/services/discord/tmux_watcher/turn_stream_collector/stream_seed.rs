use super::*;

// Restore the turn seed before initializing the collector's response and render state.
pub(super) fn restore_stream_seed(
    pending_terminal_rewind_seed: &mut Option<RestoredWatcherTurn>,
    restored_turn: &mut Option<RestoredWatcherTurn>,
    watcher_turn_identity: &Option<crate::services::discord::inflight::InflightTurnIdentity>,
    continuation: &Option<CollectedTurnStream>,
    watcher_provider: &ProviderKind,
    tmux_session_name: &str,
    channel_id: ChannelId,
) -> (WatcherStreamSeed, RestoredResponseSeed, bool) {
    let restored_turn_seed =
        take_pending_or_restored_rewind_seed(pending_terminal_rewind_seed, restored_turn);
    let seed_source = restored_turn_seed
        .as_ref()
        .and_then(|seed| seed.delivery_source.as_ref())
        .map(|source| (source.range, source.generation_mtime_ns));
    let prompt_anchor_for_seed_discard =
        crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
            watcher_provider.as_str(),
            tmux_session_name,
            channel_id.get(),
        );
    let seed_disposition = watcher_stream_seed_after_restored_seed_discard(
        restored_turn_seed,
        watcher_turn_identity.as_ref(),
        prompt_anchor_for_seed_discard.map(|anchor| anchor.message_id),
    );
    if !seed_disposition.discard_restored_seed
        && seed_disposition.prompt_anchor_present
        && seed_disposition.restored_seed_undelivered_body_len > 0
    {
        tracing::info!(
            target: "agentdesk::services::discord::tmux::tmux_watcher::turn_stream_collector",
            channel_id = channel_id.get(),
            body_len = seed_disposition.restored_seed_undelivered_body_len,
            tmux_session = %tmux_session_name,
            "watcher: preserving restored stream seed with undelivered body for idle SSH-direct prompt"
        );
    }
    if seed_disposition.discard_restored_seed {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            target: "agentdesk::services::discord::tmux::tmux_watcher::turn_stream_collector",
            "  [{ts}] 👁 watcher: discarding restored stream seed for idle SSH-direct prompt on channel {} (tmux={}, cross_turn={})",
            channel_id.get(),
            tmux_session_name,
            seed_disposition.seed_reassigned_to_different_turn
        );
    }
    let stream_seed = continuation
        .as_ref()
        .map(|turn| WatcherStreamSeed {
            full_response: turn.full_response.clone(),
            placeholder_msg_id: turn.placeholder_msg_id,
            status_panel_msg_id: turn.status_panel_msg_id,
            last_edit_text: turn.last_edit_text.clone(),
            response_sent_offset: turn.response_sent_offset,
            streaming_rollover_frozen_msg_ids: turn
                .watcher_streaming_rollover_frozen_msg_ids
                .clone(),
            task_notification_kind: turn.task_notification_kind,
            finish_mailbox_on_completion: turn.finish_mailbox_on_completion,
        })
        .unwrap_or(seed_disposition.stream_seed);
    let restored_response_seed = continuation
        .as_ref()
        .map(|turn| turn.restored_response_seed.clone())
        .unwrap_or_else(|| RestoredResponseSeed {
            body: stream_seed.full_response.clone(),
            source: seed_source.filter(|_| !stream_seed.full_response.is_empty()),
        });
    let restored_assistant_text_seen = !stream_seed.full_response.trim().is_empty();
    (
        stream_seed,
        restored_response_seed,
        restored_assistant_text_seen,
    )
}
