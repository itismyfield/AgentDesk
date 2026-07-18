use super::*;

/// #4552 decomposition: claim-success request-anchor and idle-recap bootstrap
/// lifted from `handle_text_message`. This remains after the stale-dispatch
/// guard and before the placeholder handoff.
pub(super) async fn bootstrap_claimed_turn(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    started: bool,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    provider: &ProviderKind,
    adk_session_key: Option<&str>,
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
        shared.ui.placeholder_live_events.set_turn_request_anchor(
            channel_id,
            (!crate::services::discord::voice_barge_in::is_synthetic_voice_message_id(user_msg_id))
                .then(|| user_msg_id.get()),
        );
    }

    // #3148: this runs right after the claim succeeds and only for the winner,
    // mirroring the TUI claim → bump → clear path. A queued message that lost
    // the claim race must not bump or clear. Bump BEFORE clear ensures an
    // in-flight idle-recap POST whose persist CAS captured the pre-bump
    // generation cannot overwrite this turn's clear; clear then removes a card
    // that already persisted before the claim.
    if started && let Some(pool) = shared.pg_pool.as_ref().cloned() {
        if let Err(e) = crate::services::discord::idle_recap::bump_turn_generation(
            &pool,
            channel_id.get(),
            provider,
            adk_session_key,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                channel_id = channel_id.get(),
                "idle_recap: failed to bump turn generation on Discord-intake claim"
            );
        }
        crate::services::discord::idle_recap::spawn_clear_captured_idle_recap_for_channel(
            http.clone(),
            pool,
            channel_id.get(),
        )
        .await;
    }
}
