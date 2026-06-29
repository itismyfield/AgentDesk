use super::*;
use crate::services::discord::outbound::reaction_control::{
    ReactionControlReplyReason, send_reaction_control_reply,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemovedControlReaction {
    CancelQueuedTurn,
    StopActiveTurn,
}

pub(super) fn classify_removed_control_reaction(
    emoji: &serenity::ReactionType,
) -> Option<RemovedControlReaction> {
    match emoji {
        serenity::ReactionType::Unicode(value) if value == "📬" => {
            Some(RemovedControlReaction::CancelQueuedTurn)
        }
        serenity::ReactionType::Unicode(value) if value == "⏳" => {
            Some(RemovedControlReaction::StopActiveTurn)
        }
        _ => None,
    }
}

pub(super) async fn handle_reaction_remove(
    ctx: &serenity::Context,
    removed_reaction: &serenity::Reaction,
    data: &Data,
) -> Result<(), Error> {
    let Some(action) = classify_removed_control_reaction(&removed_reaction.emoji) else {
        return Ok(());
    };
    let Some(user_id) = removed_reaction.user_id else {
        return Ok(());
    };
    if user_id == ctx.cache.current_user().id {
        return Ok(());
    }

    // Ignore reactions removed by ANY bot — only human users should be able
    // to cancel turns via reaction removal. Bots (announce/notify) remove
    // reactions during dispatch status sync, which races with active turns
    // in the same thread (#670).
    // If user is not in cache, fetch from API before deciding.
    let cache_result = ctx.cache.user(user_id).map(|u| u.bot);
    let is_bot = match cache_result {
        Some(bot) => bot,
        None => {
            // Cache miss — fetch from Discord API to determine bot status.
            //
            // #2044 F4: on API failure (transient 5xx, timeout, or
            // restart-warmup with empty cache) the previous fallback
            // treated the user as a bot, which silently dropped
            // legitimate user stop/cancel reactions. The reaction event
            // already carried the user_id and we've passed the bot
            // self-check above, so fail-open + warn is the safer
            // default: a missed bot-self event will be re-filtered on
            // the next cache fill, but a missed human stop is
            // user-visible.
            match ctx.http.get_user(user_id).await {
                Ok(user) => user.bot,
                Err(err) => {
                    tracing::warn!(
                        "  [reaction-remove] failed to fetch user {} from API: {err}; defaulting to non-bot (#2044 F4 fail-open)",
                        user_id
                    );
                    false
                }
            }
        }
    };
    if is_bot {
        return Ok(());
    }

    let channel_id = removed_reaction.channel_id;
    let settings_snapshot = { data.shared.settings.read().await.clone() };
    if validate_live_channel_routing_with_dm_hint(
        ctx,
        &data.provider,
        &settings_snapshot,
        channel_id,
        Some(removed_reaction.guild_id.is_none()),
    )
    .await
    .is_err()
    {
        return Ok(());
    }

    // Reaction-removal controls must never imprint owner state.
    // Only already-authorized users may trigger queue cancel / turn stop.
    if !crate::services::discord::discord_io::user_is_authorized(&settings_snapshot, user_id.get())
    {
        return Ok(());
    }

    match action {
        RemovedControlReaction::CancelQueuedTurn => {
            // The 🚫 reaction added by `apply_queue_exit_feedback`
            // (see `mod.rs:queue_exit_feedback_emoji`) is the only feedback
            // we surface here — no extra reply, per operator preference.
            let removed = mailbox_cancel_soft_intervention(
                &data.shared,
                &data.provider,
                channel_id,
                removed_reaction.message_id,
            )
            .await;
            if removed.is_some() {
                crate::services::discord::advance_last_message_checkpoint(
                    &data.shared,
                    &data.provider,
                    channel_id,
                    removed_reaction.message_id,
                );
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📭 QUEUE-CANCEL: removed queued message {} in channel {} via reaction removal",
                    removed_reaction.message_id,
                    channel_id
                );
            }
        }
        RemovedControlReaction::StopActiveTurn => {
            // #441: flows through cancel_text_stop_token_mailbox (mailbox_cancel_active_turn)
            // → cancel_active_token → token.cancelled triggers turn_bridge loop exit
            // → mailbox_finish_turn canonical cleanup
            //
            // #2044 F1 (TOCTOU): snapshot the cancel-token together with
            // the active user_message_id. Between this snapshot and the
            // cancel await, the mailbox actor may finish the current turn
            // and start a new one for a queued message — using the
            // snapshotted token identity via
            // `cancel_text_stop_token_mailbox_if_current` ensures we only
            // cancel if the mailbox is still on the same turn we just
            // observed. The inflight-file fallback intentionally does NOT
            // carry a token (it's only consulted when the mailbox snapshot
            // lacks an active turn), so in that branch we fall back to the
            // legacy unchecked cancel as before.
            let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
            let (active_message_id, expected_token) = match snapshot.active_user_message_id {
                Some(active_id) => (Some(active_id), snapshot.cancel_token.clone()),
                None => {
                    // user_msg_id == 0 (e.g. a TUI-direct turn) anchors no
                    // Discord message that could carry a reaction, so it yields
                    // None (never matches `removed_reaction.message_id`);
                    // `MessageId::new(0)` would panic.
                    let inflight_id = crate::services::discord::inflight::load_inflight_state(
                        &data.provider,
                        channel_id.get(),
                    )
                    .and_then(|state| {
                        crate::services::discord::inflight::optional_message_id(state.user_msg_id)
                    });
                    (inflight_id, None)
                }
            };
            if active_message_id != Some(removed_reaction.message_id) {
                return Ok(());
            }

            let stop_lookup = if let Some(expected) = expected_token {
                super::super::message_handler::cancel_text_stop_token_mailbox_if_current(
                    &data.shared,
                    &data.provider,
                    channel_id,
                    expected,
                    "reaction remove ⏳ (if_current)",
                )
                .await
            } else {
                super::super::message_handler::cancel_text_stop_token_mailbox(
                    &data.shared,
                    &data.provider,
                    channel_id,
                )
                .await
            };
            match stop_lookup {
                super::super::message_handler::TextStopLookup::Stop(token) => {
                    // #1218: stop_active_turn sends the provider abort key
                    // (C-c) FIRST so the CLI sees the interrupt while its
                    // tmux pane is still alive, then flips the cooperative
                    // flag and SIGKILLs the wrapper. The previous order
                    // killed the tmux-wrapper first — tearing down the
                    // tmux session — which made the follow-up send-keys
                    // fail with "can't find pane". For Codex/Qwen TUIs and
                    // resumed runs (`child_pid = None`) the C-c is the
                    // only mechanism that actually stops the provider.
                    crate::services::discord::turn_bridge::stop_active_turn(
                        &data.provider,
                        &token,
                        crate::services::discord::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                        "reaction remove ⏳",
                    )
                    .await;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🛑 TURN-STOP: cancelled active turn for message {} in channel {} via reaction removal",
                        removed_reaction.message_id,
                        channel_id
                    );
                    // #3650: no separate notify-bot stop message — the in-place
                    // `[Stopped]` edit on the assistant message and the 🛑
                    // reaction already cover the stop signal.
                }
                super::super::message_handler::TextStopLookup::AlreadyStopping => {
                    send_reaction_control_reply(
                        ctx,
                        &data.shared,
                        channel_id,
                        removed_reaction.message_id,
                        ReactionControlReplyReason::AlreadyStopping,
                        "Already stopping...",
                    )
                    .await;
                }
                super::super::message_handler::TextStopLookup::NoActiveTurn => {}
            }
        }
    }

    Ok(())
}
