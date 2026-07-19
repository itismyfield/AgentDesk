use super::*;

/// #4552 decomposition: the turn-start DISPATCH-GUARD lifted verbatim from
/// `handle_text_message`. It runs immediately after the mailbox claim
/// (`try_start_turn_with_stale_busy_heal`) and before the claim-success
/// bootstrap: when THIS message won the claim (`started == true`) and the
/// caller did not mark it preserve-on-cancel, a text still carrying a stale
/// terminal `DISPATCH:<id>` prefix is aborted here so a re-dispatched terminal
/// turn cannot re-run.
///
/// Returns `true` when the turn was aborted and the caller must `return
/// Ok(())`; `false` when intake should proceed. All side effects (mailbox
/// finish, checkpoint advance, exit-feedback reaction, deferred idle kickoff)
/// are preserved bit-for-bit from the pre-extraction inline block.
///
/// #4247 FIX 1: the `!preserve_on_cancel` gate mirrors the dequeue guard's
/// `filter_queued_dispatch_exit(preserve, stale)` — a preserved (marked)
/// genuine human instruction that survives the dequeue guard must NOT be
/// dropped on re-entry merely because its text carries a stale `DISPATCH:`
/// prefix, which would silently defeat the fail-safe queue-preservation
/// feature end-to-end.
pub(super) async fn abort_terminal_dispatch_at_turn_start(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    user_text: &str,
    started: bool,
    preserve_on_cancel: bool,
) -> bool {
    if started
        && !preserve_on_cancel
        && let Some(stale) = crate::services::discord::stale_dispatch_turn_for_text(
            shared.pg_pool.as_ref(),
            user_text,
        )
        .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⏭ DISPATCH-GUARD: aborted terminal dispatch at turn start in channel {} (dispatch={}, status={})",
            channel_id,
            stale.dispatch_id,
            stale.status
        );
        let finish =
            crate::services::discord::mailbox_finish_turn(shared.as_ref(), provider, channel_id)
                .await;
        crate::services::discord::advance_last_message_checkpoint(
            shared,
            provider,
            channel_id,
            user_msg_id,
        );
        let emoji = crate::services::discord::queue_exit_feedback_emoji(stale.queue_exit_kind);
        crate::services::discord::queue_marker::note_exit_feedback_added(
            shared,
            http,
            channel_id,
            user_msg_id,
            emoji,
        )
        .await;
        if finish.has_pending {
            crate::services::discord::schedule_deferred_idle_queue_kickoff(
                shared.clone(),
                provider.clone(),
                channel_id,
                "terminal dispatch skipped at turn start",
            );
        }
        return true;
    }
    false
}
