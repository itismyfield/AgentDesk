use super::*;

pub(in crate::services::discord::router::message_handler) mod mailbox_reaction;
mod queued_intake_cause;

pub(in crate::services::discord::router::message_handler) use queued_intake_cause::QueuedIntakeCause;

fn race_loss_persistence_failure(
    channel_id: ChannelId,
    persistence_error: Option<&str>,
) -> Result<(), Error> {
    let Some(persistence_error) = persistence_error else {
        return Ok(());
    };
    Err(std::io::Error::other(format!(
        "failed to persist queued intake for channel {}: {persistence_error}",
        channel_id.get()
    ))
    .into())
}

async fn enqueue_race_loss_requeued_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    intervention: Intervention,
    cause: QueuedIntakeCause,
) -> crate::services::discord::MailboxEnqueueOutcome {
    let outcome = crate::services::discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
        crate::services::discord::mailbox_enqueue_intervention(
            shared,
            provider,
            channel_id,
            intervention,
        ),
    )
    .await;
    if outcome.persistence_error.is_some() {
        let cleared = crate::services::discord::mailbox_clear_pending_dispatch_reservation(
            shared,
            provider,
            channel_id,
            user_msg_id,
        )
        .await;
        if !cleared {
            tracing::error!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                user_message_id = user_msg_id.get(),
                "race-loss persistence rollback could not clear the pending dispatch reservation"
            );
        }
    } else {
        let abandoned = crate::services::discord::mailbox_abandon_pending_dispatch(
            shared,
            provider,
            channel_id,
            user_msg_id,
        )
        .await;
        if !abandoned {
            tracing::debug!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                user_message_id = user_msg_id.get(),
                "race-loss enqueue had no matching pending dispatch reservation to abandon"
            );
        }
    }
    if outcome.enqueued && outcome.persistence_error.is_none() {
        if cause.wants_immediate_idle_recheck() {
            crate::services::discord::queue_io::schedule_race_loss_requeue_post_enqueue_idle_recheck(
                shared.clone(),
                provider.clone(),
                channel_id,
            );
        } else {
            // #5170 A — the durable enqueue above stays exactly where it is (it
            // is what closes the process-crash loss window). Only the wake
            // policy changes: an immediate re-kick here would re-enter intake
            // while the transition it just failed to take is still held, and
            // every such failure enqueued again. Arm the slow fail-open
            // backstop so the channel still drains if no other edge arrives —
            // the same treatment the finalize epilogue already gives a
            // transition-owned channel.
            tracing::debug!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                user_message_id = user_msg_id.get(),
                "session-transition-busy intake queued durably; deferring the drain to the slow backstop instead of an immediate re-kick"
            );
            crate::services::discord::arm_slow_idle_queue_backstop_if_queue_nonempty(
                shared,
                provider,
                channel_id,
                "intake_session_transition_busy",
            )
            .await;
        }
    }
    outcome
}
/// Runs the start-turn race-loss enqueue path (#3837): mailbox enqueue,
/// queued-placeholder render, and queue-pending reaction lifecycle for a
/// message that lost the mailbox start-turn claim.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_race_loss_enqueue(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
    channel_id: ChannelId,
    original_channel_id: ChannelId,
    turn_kind: TurnKind,
    original_request_owner: UserId,
    user_msg_id: MessageId,
    user_text: &str,
    reply_context: &Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    pending_uploads: &[String],
    voice_announcement: &Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    reply_to_user_message: bool,
    dispatch_id_for_thread: &Option<String>,
    turn_start_attempt: Option<crate::services::discord::turn_view_reconciler::TurnStartAttempt>,
    preserve_on_cancel: bool,
    cause: QueuedIntakeCause,
) -> Result<(), Error> {
    let bot_owner_provider = crate::services::discord::resolve_discord_bot_provider(token);
    let want_queued_card = !turn_kind.is_background_trigger() && channel_id == original_channel_id;

    // Enqueue the intervention BEFORE any Discord HTTP await (#1332): once
    // the mailbox reports no pending entry for this channel, turn_bridge
    // clears the dispatch-role override, so a late enqueue would run the
    // queued turn under the default provider/role instead of the routing
    // the request expects. A fast dispatch may then observe the queued
    // intervention before the placeholder mapping below lands; that's
    // tolerated because the dispatch fallback POSTs a fresh card when no
    // mapping exists, and the `active_user_message_id == user_msg_id` check
    // before the mapping insert drops our orphan POST instead of duplicating.
    let enqueue_outcome = enqueue_race_loss_requeued_intervention(
        shared,
        &bot_owner_provider,
        channel_id,
        user_msg_id,
        build_race_requeued_intervention(
            // #2266: attribute to the original Discord author (the announce
            // bot) so the downstream `announce_bot_id == Some(request_owner)`
            // check passes on replay; the post-rebind voice-user id would
            // look like a non-announce author and discard the voice payload
            // as spoofed.
            original_request_owner,
            user_msg_id,
            user_text,
            preserve_on_cancel,
            reply_context.clone(),
            has_reply_boundary,
            merge_consecutive,
            pending_uploads.to_vec(),
            // #2266: keep the voice payload in the queued `Intervention` so
            // `dispatch_queued_turn` can reinsert it before re-entering
            // `handle_text_message`, restoring voice-transcript framing.
            voice_announcement.clone(),
        ),
        cause,
    )
    .await;

    // #4078: a blind post-enqueue kick here would self-feed a
    // KICKOFF -> race-loss -> requeue loop while the race winner still holds
    // the token; the helper above suppresses it and kicks only if the
    // channel is already idle by the time the enqueue lands.

    if let Some(persistence_error) = enqueue_outcome.persistence_error.as_ref() {
        mailbox_reaction::clear_rejected_attempt_pending(
            shared,
            http,
            channel_id,
            user_msg_id,
            turn_start_attempt,
        )
        .await;
        tracing::error!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            user_message_id = user_msg_id.get(),
            error = %persistence_error,
            "race-lost intake could not be persisted; returning failure to the caller"
        );
        return race_loss_persistence_failure(channel_id, Some(persistence_error));
    }

    // Enqueue rejected (dedup/duplicate): skip the placeholder POST and
    // mapping insert (a fresh card would orphan) and the `📬` reaction (the
    // prior live enqueue owns it). Only the matching start attempt clears
    // its own pending `⏳`, so a delayed attempt can't erase a newer one's.
    if !enqueue_outcome.enqueued {
        mailbox_reaction::clear_rejected_attempt_pending(
            shared,
            http,
            channel_id,
            user_msg_id,
            turn_start_attempt,
        )
        .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        // #2728: log which refusal branch fired so race-loss dedup
        // incidents can be classified without re-reading code.
        tracing::info!(
            "  [{ts}] 🔁 RACE: race-lost intervention refused by mailbox before placeholder POST (channel {}, refusal_reason={}); no duplicate queue entry retained",
            channel_id,
            enqueue_outcome
                .refusal_reason
                .map(|r| r.as_str())
                .unwrap_or("unknown"),
        );
        return Ok(());
    }

    let want_queued_card = want_queued_card
        && super::super::super::queue_status_presentation::queue_status_card_enabled();

    // If a queued placeholder mapping already exists for
    // `(channel_id, user_msg_id)`, reuse the existing `📬` card instead of
    // POSTing a fresh one — a new POST would orphan the prior mapping (it
    // gets overwritten below, leaving the old card with no cleanup path).
    // Background-trigger and thread-routed turns never write to
    // `queued_placeholders`, so they always take the fresh-POST path.
    let existing_queued_card = if want_queued_card {
        shared
            .queued
            .queued_placeholders
            .get(&(channel_id, user_msg_id))
            .map(|entry| *entry.value())
    } else {
        None
    };
    let reused_existing_mapping = existing_queued_card.is_some();

    let placeholder_msg_id = if let Some(existing) = existing_queued_card {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ RACE: reusing existing queued placeholder (channel {}, msg {}) — re-queue without new POST",
            channel_id,
            existing
        );
        existing
    } else {
        let post_result = send_intake_placeholder(
            http.clone(),
            shared.clone(),
            channel_id,
            if reply_to_user_message && dispatch_id_for_thread.is_none() {
                Some((channel_id, user_msg_id))
            } else {
                None
            },
            // #3082 P2-3: this message lost the start-turn race and is now
            // QUEUED — its "📬" card is a trailing notice that must wait
            // behind any in-flight multi-chunk answer flush.
            true,
        )
        .await;

        match post_result {
            Ok(msg_id) => msg_id,
            Err(error) => {
                // POST failed after enqueue: the intervention is already in
                // the mailbox queue, so a later kickoff (or the deferred idle
                // drain above) dispatches it and POSTs a fresh card through
                // the missing-mapping fallback. Roll back the pending `⏳` to
                // the marker-only queued state.
                if let Some(turn_start_attempt) = turn_start_attempt {
                    crate::services::discord::turn_view_reconciler::note_intake_start_rolled_back_to_queued(
                        shared,
                        channel_id,
                        user_msg_id,
                        shared.restart.current_generation,
                        turn_start_attempt,
                        "race_loss_placeholder_post_failed",
                    )
                    .await;
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ RACE: placeholder POST failed for race-lost message AFTER enqueue (channel {}, error={}); message remains queued, dispatch will POST fresh card",
                    channel_id,
                    error
                );
                // #1984: the message is already queued; dispatch POSTs a
                // fresh card via the missing-mapping fallback.
                crate::services::observability::emit_intake_placeholder_post_failed(
                    provider.as_str(),
                    channel_id.get(),
                    Some(user_msg_id.get()),
                    "race_after_enqueue",
                    "fresh_card_via_dispatch",
                    &error.to_string(),
                );
                return Ok(());
            }
        }
    };

    // Insert the mapping AFTER the POST, holding the per-channel persist
    // mutex across recheck+insert so a concurrent `dispatch_queued_turn`
    // cannot take our entry between the recheck and the write. Between our
    // enqueue and here the active turn may have finished and dispatched our
    // intervention with its own fresh card (no mapping); inserting anyway
    // would leave `placeholder_msg_id` orphaned and render `📬` over an
    // already-running turn. Take the persist lock first, snapshot the
    // mailbox under it, then insert — ownership check + insert +
    // `ensure_queued` PATCH all run under one held guard, since
    // `remove_queued_placeholder` (mod.rs:1151) serializes through the same
    // mutex. The recheck bails unless `user_msg_id` is still queued (head
    // `message_id` or any `source_message_ids` entry); background-trigger /
    // thread-routed / reused-mapping turns skip it entirely.
    let persist_guard_for_render = if want_queued_card && !reused_existing_mapping {
        // `lock_owned()` so the guard owns the `Arc` and can outlive the
        // local `persist_lock` binding when handed to the render branch
        // below — one critical section spanning the recheck, the mapping
        // insert, and the `ensure_queued` PATCH.
        let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
        let persist_guard = persist_lock.lock_owned().await;
        // Snapshot UNDER the lock: `dispatch_queued_turn` removes the
        // queued mapping via `remove_queued_placeholder`, which acquires
        // this same mutex, so while we hold the guard no dispatch path can
        // advance us from "queued" to "active".
        let snapshot = crate::services::discord::mailbox_snapshot(shared, channel_id).await;
        // A `📬` mapping must not be inserted when `user_msg_id` is no
        // longer in the queue: it may have been cancelled/superseded since
        // our enqueue, or been the non-head `source_message_id` of an
        // already-dequeued merged Intervention. In both cases
        // `active_user_message_id` may be `None` or a different message, so
        // checking only `active == user_msg_id` would miss it and leave a
        // stale card forever — also verify `user_msg_id` is still queued
        // (head `message_id` or any `source_message_ids` entry); if
        // neither holds, treat it as a race-loss and bail.
        let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
            intervention.message_id == user_msg_id
                || intervention.source_message_ids.contains(&user_msg_id)
        });
        let dispatch_already_running_for_our_msg =
            snapshot.active_user_message_id == Some(user_msg_id);
        if dispatch_already_running_for_our_msg || !still_queued {
            // Either dispatch already promoted us into an active turn, or
            // our entry left the queue via cancellation/supersede/merged-
            // drain. Either way the POSTed placeholder is an orphan no
            // future cleanup will reference — drop the lock before the
            // HTTP DELETE, delete the orphan, clear the matching pending
            // attempt, and skip the mapping insert.
            drop(persist_guard);
            let _ = channel_id.delete_message(http, placeholder_msg_id).await;
            crate::services::discord::turn_view_reconciler::note_intake_turn_cleared_current_if_attempt_matches(
                shared,
                http,
                channel_id,
                user_msg_id,
                turn_start_attempt,
                "race_loss_orphan_placeholder",
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            if dispatch_already_running_for_our_msg {
                tracing::info!(
                    "  [{ts}] 🔁 RACE: dispatch already started turn for our message (channel {}, msg {}); deleting orphan placeholder POST after queue handoff",
                    channel_id,
                    user_msg_id
                );
            } else {
                tracing::info!(
                    "  [{ts}] 🔁 RACE: message no longer queued (cancelled/superseded/merged-drained) (channel {}, msg {}); deleting orphan placeholder POST",
                    channel_id,
                    user_msg_id
                );
            }
            return Ok(());
        }
        shared.insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);
        // Hand the still-held guard to the `ensure_queued` PATCH branch so
        // ownership check + insert + PATCH run under one held lock guard.
        Some(persist_guard)
    } else {
        None
    };

    // #1116/#2036: enqueue already happened above, then the marker path
    // rechecks ownership after the Discord await so a fast dequeue cannot
    // leave a stale 📬 behind.
    let mut queued_marker_notified = false;
    let routed_to_thread = channel_id != original_channel_id;
    if !routed_to_thread && should_add_turn_pending_reaction(dispatch_id_for_thread.as_deref()) {
        // #1190 follow-up: merged messages get ➕ so the user can tell
        // them apart from standalone queue head entries (📬).
        let emoji = if enqueue_outcome.merged {
            '➕'
        } else {
            '📬'
        };
        queued_marker_notified =
            emoji == crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION;
        mailbox_reaction::note_queue_pending(
            shared,
            http,
            channel_id,
            user_msg_id,
            emoji,
            turn_start_attempt,
            "race_loss_message_queued",
        )
        .await;
        // #2036 Surface 3: detect queue→start races where the
        // dispatch path consumed our mapping before this reaction
        // landed and proactively unstick the emoji.
        if !shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id) {
            crate::services::discord::queue_marker::note_removed_current(
                shared,
                http,
                channel_id,
                user_msg_id,
                emoji,
                "race_loss_queue_self_heal",
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔁 RACE: queue-pending {emoji} reacted after dequeue promotion (channel {}, msg {}); removed stale reaction",
                channel_id,
                user_msg_id
            );
        }
    }
    // #796: background-trigger placeholders must not be deleted on
    // race-loss — the placeholder is the user-visible breadcrumb of the
    // background notification. #1332: foreground turns instead EDIT the
    // bare `...` into a `📬 메시지 대기 중` card; on edit failure we roll
    // back the mapping and delete the Discord message so users never see a
    // stale placeholder.
    if turn_kind.is_background_trigger() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔔 RACE: background-trigger placeholder preserved (channel {}, msg {})",
            channel_id,
            placeholder_msg_id
        );
    } else if want_queued_card && !reused_existing_mapping {
        // Between `mailbox_enqueue_intervention` and the `ensure_queued`
        // await below, the active turn can finish and dispatch can consume
        // our `(channel_id, user_msg_id)` mapping — at which point our
        // placeholder has been promoted to the live response card, and
        // editing/deleting it here would corrupt it. The dispatch-state
        // snapshot, mapping insert, ownership recheck, and `ensure_queued`
        // PATCH therefore all share the ONE held `persist_guard_for_render`
        // lock guard acquired above: every other path that mutates
        // `queued_placeholders` takes the same per-channel mutex, so the
        // mapping cannot change underneath this PATCH while we hold it.
        let persist_guard = persist_guard_for_render
            .expect("round-10: persist guard must be held by the matching insert branch");
        if !shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id) {
            drop(persist_guard);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔁 RACE: queued placeholder handoff already consumed by dispatch (channel {}, msg {}); skipping render",
                channel_id,
                placeholder_msg_id
            );
        } else {
            let gateway = DiscordGateway::new(
                http.clone(),
                shared.clone(),
                bot_owner_provider.clone(),
                None,
            );
            let key = crate::services::discord::placeholder_controller::PlaceholderKey {
                provider: bot_owner_provider.clone(),
                channel_id,
                message_id: placeholder_msg_id,
            };
            let queued_input =
                crate::services::discord::placeholder_controller::PlaceholderActiveInput {
                    reason: crate::services::discord::formatting::MonitorHandoffReason::Queued,
                    started_at_unix: chrono::Utc::now().timestamp(),
                    tool_summary: None,
                    command_summary: None,
                    reason_detail: None,
                    context_line: None,
                    request_line: Some(user_text.to_string()),
                    progress_line: None,
                };
            let outcome = shared
                .ui
                .placeholder_controller
                .ensure_queued(&gateway, key, queued_input)
                .await;
            use crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::*;
            match outcome {
                Edited | Coalesced => {
                    drop(persist_guard);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 📬 RACE: queued placeholder rendered (channel {}, msg {})",
                        channel_id,
                        placeholder_msg_id
                    );
                }
                _ => {
                    // Edit failed — roll back the mapping and delete the raw
                    // `...`. The lock guarantees the mapping is unchanged
                    // since the recheck; use `_locked` to avoid reacquiring.
                    let still_owned_under_lock = shared.queued_placeholder_still_owned(
                        channel_id,
                        user_msg_id,
                        placeholder_msg_id,
                    );
                    if still_owned_under_lock {
                        shared.remove_queued_placeholder_locked(channel_id, user_msg_id);
                    }
                    drop(persist_guard);
                    if still_owned_under_lock {
                        let _ = channel_id.delete_message(http, placeholder_msg_id).await;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⚠ RACE: queued placeholder render failed, deleted instead (channel {}, msg {})",
                            channel_id,
                            placeholder_msg_id
                        );
                    } else {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🔁 RACE: queued placeholder render failed AND handoff already consumed (channel {}, msg {}); leaving Discord state intact",
                            channel_id,
                            placeholder_msg_id
                        );
                    }
                }
            }
        }
    } else if want_queued_card && reused_existing_mapping {
        // The existing card already shows `📬 메시지 대기 중`. Skip the
        // redundant `ensure_queued` PATCH — the prior race-loss already
        // wrote it, and re-emitting identical content would just hit a
        // `Coalesced` no-op. This path is only reached when
        // `enqueued == true` (dedup-rejected enqueues return early above),
        // so the earlier owner's lifecycle still owns the card.
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ RACE: re-queue reused existing 📬 card without re-render (channel {}, msg {})",
            channel_id,
            placeholder_msg_id
        );
    } else {
        // Background-trigger turns hit the branch above; remaining cases
        // (e.g. is_thread_routed) have no queued card to render — the bare
        // `...` placeholder would otherwise leak.
        let _ = channel_id.delete_message(http, placeholder_msg_id).await;
    }
    if !queued_marker_notified {
        crate::services::discord::turn_view_reconciler::note_intake_turn_cleared_current_if_attempt_matches(
            shared,
            http,
            channel_id,
            user_msg_id,
            turn_start_attempt,
            "race_loss_message_queued",
        )
        .await;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    // #5170: only the mailbox start-turn claim can be lost to "another turn".
    // A transition-busy requeue has no winning opponent to name, and saying so
    // sent operators looking for a competing turn that was never there.
    match cause {
        QueuedIntakeCause::RaceLoss => tracing::info!(
            "  [{ts}] 🔀 RACE: message queued (another turn won), channel {}",
            channel_id
        ),
        QueuedIntakeCause::SessionTransitionBusy => tracing::info!(
            "  [{ts}] 🔀 QUEUE: message queued (session transition held at intake), channel {}",
            channel_id
        ),
    }
    return Ok(());
}

#[cfg(test)]
#[path = "race_loss/requeue_tests.rs"]
mod race_loss_requeue_tests;

#[cfg(test)]
mod mailbox_reaction_tests;
