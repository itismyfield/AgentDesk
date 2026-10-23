use super::*;

/// Record the claimed request anchor and clear its previously stored idle card.
pub(super) async fn bootstrap_claimed_turn(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    started: bool,
    channel_id: ChannelId,
    user_msg_id: MessageId,
) {
    // #3811: record the original-request anchor (the real Discord user_msg_id)
    // ONLY once THIS message won the mailbox claim (`started == true`) and the
    // caller's stale-dispatch guard passed. A message that merely QUEUES behind
    // an active turn (`started == false`) must NOT touch that active turn's
    // anchor — it records its own anchor when later dequeued/promoted and
    // re-enters intake with `started == true`. Synthetic voice ids back no real
    // Discord message → `None` (no fake link); headless turns never reach this
    // interactive intake path.
    if started {
        shared
            .ui
            .placeholder_live_events
            .resolve_turn_request_anchor(
                http,
                channel_id,
                (!crate::services::discord::voice_barge_in::is_synthetic_voice_message_id(
                    user_msg_id,
                ))
                .then(|| user_msg_id.get()),
            )
            .await;
    }

    if started && let Some(pool) = shared.pg_pool.as_ref().cloned() {
        crate::services::discord::session_idle_cleanup::spawn_clear_captured_idle_recap_for_channel(
            http.clone(), pool, channel_id.get(),
        ).await;
    }
}
