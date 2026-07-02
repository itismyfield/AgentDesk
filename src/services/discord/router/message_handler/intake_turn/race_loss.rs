use super::*;

/// #3837 decomposition: the start-turn race-loss enqueue path lifted verbatim
/// from `handle_text_message`. Behavior-preserving — this is the exact body of
/// the `if !started { ... }` block (mailbox enqueue, queued-placeholder render,
/// and queue-pending reaction lifecycle) that runs when this message lost the
/// mailbox start-turn claim. Its `return Ok(())` / `return Err(..)` statements
/// map 1:1 onto the original inline returns, so the caller does
/// `if !started { return handle_race_loss_enqueue(..).await; }`.
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
) -> Result<(), Error> {
    let bot_owner_provider = crate::services::discord::resolve_discord_bot_provider(token);
    let is_thread_routed = channel_id != original_channel_id;
    let want_queued_card = !turn_kind.is_background_trigger() && !is_thread_routed;

    // codex review round-9 P2 (#1332): enqueue the intervention BEFORE
    // any Discord HTTP await. The previous order (POST placeholder →
    // insert mapping → enqueue) opened a window where the still-running
    // active turn could finalize during the POST/insert awaits. Without
    // an entry in the mailbox queue, `finalize_turn_state` reports
    // `has_pending == false`, and `turn_bridge` clears
    // `dispatch_role_overrides` for this channel. Our late enqueue then
    // lands without the override, so the queued dispatch runs under the
    // default provider/role instead of the dispatch-role routing the
    // request expects (e.g. a Codex-review hand-off would execute under
    // Claude). Enqueueing first keeps the mailbox snapshot consistent
    // with the override lifecycle: as long as our intervention is
    // queued, the override survives.
    //
    // Trade-off: this inverts the round-2 invariant ("queued_placeholders
    // mapping inserted BEFORE enqueue") — a fast dispatch could now
    // observe the queued intervention before our placeholder mapping
    // lands. The existing dispatch fallback (`else` branch ~line 3066 in
    // `handle_text_message`) tolerates that case by POSTing a fresh card
    // via `send_intake_placeholder`, restoring the pre-PR behavior of "a
    // fresh card on dispatch when no queued mapping exists." Round-2's
    // duplicate-card concern is mitigated below by checking
    // `active_user_message_id == user_msg_id` immediately before the
    // mapping insert: if the dispatch path has already promoted our
    // intervention into an active turn (with its own fresh card), we
    // delete our orphan POST and skip the mapping insert.
    let enqueue_outcome = crate::services::discord::mailbox_enqueue_intervention(
        shared,
        &bot_owner_provider,
        channel_id,
        build_race_requeued_intervention(
            // #2266: attribute the queued `Intervention` to the original
            // Discord author (the announce bot for voice transcripts) so
            // the downstream `handle_text_message`
            // `announce_bot_id == Some(request_owner)` check at line
            // ~2274 passes when the dispatch path replays the queued
            // turn. Passing the post-rebind voice-user id here would
            // make the queued turn look like a non-announce author and
            // the embedded voice payload would be discarded as spoofed.
            original_request_owner,
            user_msg_id,
            user_text,
            reply_context.clone(),
            has_reply_boundary,
            merge_consecutive,
            pending_uploads.to_vec(),
            // #2266: keep the voice payload self-contained in the queued
            // `Intervention` so `dispatch_queued_turn` can reinsert it
            // before re-entering `handle_text_message`, which restores
            // the voice-transcript framing instead of degrading the queued
            // reply to plain text.
            voice_announcement.clone(),
        ),
    )
    .await;
    let enqueued = enqueue_outcome.enqueued;
    if enqueue_outcome.persistence_error.is_some() {
        crate::services::discord::mailbox_clear_pending_dispatch_reservation(
            shared,
            &bot_owner_provider,
            channel_id,
            user_msg_id,
        )
        .await;
    } else {
        crate::services::discord::mailbox_abandon_pending_dispatch(
            shared,
            &bot_owner_provider,
            channel_id,
            user_msg_id,
        )
        .await;
    }

    // codex review P1: cover the residual race window where the active
    // turn finished between `mailbox_try_start_turn` and the enqueue
    // above. In that case `mailbox_finish_turn` saw an empty queue and
    // skipped the dequeue chain — schedule a deferred drain so the
    // intervention we just enqueued does not strand. Round-9: this still
    // runs first so the deferred kickoff fires even if the placeholder
    // POST below ends up failing.
    //
    // #3903: gate on the BLOCKING (real) active turn, not on *any* active
    // turn. The previous `mailbox_has_active_turn` skipped this drain
    // whenever a background SYSTEM-INJECTION turn (e.g. a `/loop`
    // auto-check) held the slot — but such a turn's finalize does NOT
    // re-kick the user queue, so a genuine user message re-enqueued here
    // (it lost the start-turn race to the injection) stranded in the queue
    // until an external fetch surfaced it (user-message loss → data
    // integrity). A background turn is non-blocking (#3167), so scheduling
    // the (idempotent, gated, bounded-retry) drain now lets the drain
    // supersede the background injection and dispatch the queued user
    // message as soon as the pane is idle — exactly-once, never doubled
    // (the dequeue is actor-serialized through `take_next_soft`). When a
    // REAL turn holds the slot we still skip the drain: that turn's own
    // finalize owns the dequeue chain (unchanged pre-#3903 behavior).
    let has_blocking_active_turn =
        crate::services::discord::mailbox_has_blocking_active_turn(shared, channel_id).await;
    if super::super::super::intake_gate::should_schedule_post_enqueue_idle_drain(
        enqueued,
        has_blocking_active_turn,
    ) {
        crate::services::discord::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            bot_owner_provider.clone(),
            channel_id,
            "race-loss enqueue idle drain",
        );
    }

    // If the enqueue was rejected (dedup / duplicate) there is nothing
    // for the dispatch path to pick up. Skip the placeholder POST + the
    // mapping insert entirely — POSTing a fresh card here would orphan
    // it. `📬` reaction is also skipped (the prior live enqueue already
    // owns the card and emoji). Just clean up `⏳` and return.
    if !enqueued {
        crate::services::discord::formatting::remove_reaction_raw(
            http,
            channel_id,
            user_msg_id,
            '⏳',
        )
        .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        // #2728: log which refusal branch fired so race-loss dedup
        // incidents can be classified without re-reading code.
        let refusal_str = enqueue_outcome
            .refusal_reason
            .map(|r| r.as_str())
            .unwrap_or("unknown");
        tracing::info!(
            "  [{ts}] 🔁 RACE: race-lost intervention refused by mailbox (channel {}, refusal_reason={}); skipping placeholder POST",
            channel_id,
            refusal_str,
        );
        return Ok(());
    }

    // codex review round-5 P2 (finding 2 — re-queue reuse): if a queued
    // placeholder mapping already exists for `(channel_id, user_msg_id)`
    // — typically because the active turn finished and the queued
    // turn was about to dispatch, but a new turn intercepted and won
    // the mailbox race before that dispatch could run — REUSE the
    // existing `📬` card instead of POSTing a fresh placeholder.
    // Posting a new placeholder would orphan the prior one (its mapping
    // would be overwritten by the new `insert_queued_placeholder`
    // below, and the old card would stay visible with no bookkeeping
    // path to clean it up). Background-trigger turns and thread-routed
    // turns never write to `queued_placeholders`, so they always go
    // through the fresh POST path.
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
                // POST failed AFTER enqueue. Round-9 trade-off: the
                // intervention is already in the mailbox queue, so a
                // later kickoff (or the deferred idle drain scheduled
                // above) will dispatch it — `dispatch_queued_turn` ->
                // `handle_text_message` will POST its own fresh card
                // through the missing-mapping fallback. The user
                // briefly sees `⏳` only and no `📬`, but the message
                // WILL be processed correctly. Roll back the `⏳`
                // sentinel so the user knows we did not silently
                // accept the message.
                crate::services::discord::formatting::remove_reaction_raw(
                    http,
                    channel_id,
                    user_msg_id,
                    '⏳',
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ RACE: placeholder POST failed for race-lost message AFTER enqueue (channel {}, error={}); message remains queued, dispatch will POST fresh card",
                    channel_id,
                    error
                );
                // #1984 (codex C — observation): the user message is
                // already in the mailbox queue; the dispatch path will
                // POST a fresh card via the missing-mapping fallback.
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
    // cannot take our entry between the recheck and the write (round-9
    // reorder supersedes round-2's "mapping before enqueue").
    //
    // Dispatch-state recheck (round-10/11 codex P2): between our enqueue
    // and here the active turn may have finished AND turn_bridge may have
    // dequeued our intervention, started its turn, and POSTed its own fresh
    // card (no mapping → `send_intake_placeholder`). If we then insert, our
    // `placeholder_msg_id` is an orphan and `ensure_queued` would render
    // `📬` on an already-running turn. Other queue-exit timelines (cancel,
    // supersede, merged-drain of a non-head source id) likewise leave
    // `user_msg_id` out of the queue while the active turn != us. Fix: take
    // the persist lock FIRST, snapshot the mailbox under it, then insert —
    // invariant "ownership check + insert + ensure_queued PATCH all run
    // under one held guard." `remove_queued_placeholder` serializes through
    // the same mutex (mod.rs:1151), so dispatch cannot promote us until we
    // release. The recheck below additionally bails unless `user_msg_id` is
    // still queued (head `message_id` or any `source_message_ids` entry).
    // Background-trigger / thread-routed / reused-mapping turns stay out of
    // `queued_placeholders` by design and skip this recheck entirely.
    let persist_guard_for_render = if want_queued_card && !reused_existing_mapping {
        // Use `lock_owned()` so the guard owns the `Arc` and can outlive
        // the local `persist_lock` binding when we hand it off to the
        // queued-card render branch below (round-10: single critical
        // section spanning the dispatch-state recheck, the mapping
        // insert, and the `ensure_queued` PATCH).
        let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
        let persist_guard = persist_lock.lock_owned().await;
        // Snapshot UNDER the lock so a concurrent dispatch path cannot
        // promote our intervention to active between this read and the
        // mapping insert below. `dispatch_queued_turn` removes the
        // queued mapping via `remove_queued_placeholder`, which itself
        // acquires this same per-channel persist mutex; while we hold
        // the guard, no dispatch path can advance from "queued" to
        // "active for our user_msg_id".
        let snapshot = crate::services::discord::mailbox_snapshot(shared, channel_id).await;
        // Round-11 codex review P2: the round-10 recheck only bailed when
        // `active_user_message_id == user_msg_id`, but there are other
        // states where `user_msg_id` is no longer in the queue and a
        // `📬` mapping must NOT be inserted:
        //   1. The intervention was cancelled / superseded between our
        //      enqueue and our lock acquire (queue-exit drain ran).
        //   2. The intervention was the non-head `source_message_id` of a
        //      merged Intervention that has already been dequeued (the
        //      merged-drain ran on dispatch).
        // In either case `active_user_message_id` may be `None` or a
        // different message (e.g. the merge-head), so the round-10
        // `active == user_msg_id` check passes through and we would
        // insert a `📬` mapping for a `user_msg_id` that no future
        // dispatch or queue-exit cleanup will ever reference → stale
        // card forever.
        //
        // Fix: in addition to the round-10 active-equals-us check, also
        // verify `user_msg_id` is still in the queue (head
        // `intervention.message_id` OR any `source_message_ids` entry).
        // If neither holds, treat it as a race-loss and bail.
        let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
            intervention.message_id == user_msg_id
                || intervention.source_message_ids.contains(&user_msg_id)
        });
        let dispatch_already_running_for_our_msg =
            snapshot.active_user_message_id == Some(user_msg_id);
        if dispatch_already_running_for_our_msg || !still_queued {
            // Either dispatch already promoted us into an active turn
            // (round-10 case) OR our entry has left the queue via
            // cancellation / supersede / merged-drain (round-11 case).
            // In all cases our POSTed placeholder is an orphan that no
            // future dispatch or queue-exit cleanup will ever reference
            // — drop the lock before the HTTP DELETE await, delete the
            // orphan, remove the `⏳` reaction, and skip the mapping
            // insert.
            drop(persist_guard);
            let _ = channel_id.delete_message(http, placeholder_msg_id).await;
            crate::services::discord::formatting::remove_reaction_raw(
                http,
                channel_id,
                user_msg_id,
                '⏳',
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            if dispatch_already_running_for_our_msg {
                tracing::info!(
                    "  [{ts}] 🔁 RACE: dispatch already started turn for our message (channel {}, msg {}); deleting orphan placeholder POST",
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
        // Hand the still-held guard to the `ensure_queued` PATCH branch
        // below so the entire ownership check + insert + PATCH critical
        // section runs under one held lock guard (the round-10
        // atomicity invariant).
        Some(persist_guard)
    } else {
        None
    };

    // #1116 Pending-reaction emoji machine: 📬 queued → ⏳ processing →
    // ✅ done. Round-9: enqueue already happened above; the reaction
    // safely reflects the actual queue state.
    //
    // #2036 Surface 3 fix: previously, if the active turn finished
    // between this enqueue and the `add_reaction` await below, the
    // dequeue path's 📬 cleanup could run before our add landed and
    // leave the icon stuck on a turn that had already started. The
    // user-reported case (run 767447c8): dispatch message lands on a
    // channel whose previous turn is wrapping up, so the message gets
    // queued and reacted with 📬; the bridge then promotes it before
    // the add_reaction await resolves, and the leftover 📬 lies about
    // codex still being queue-pending while codex is in fact already
    // responding to the dispatch. Round-12 fix: after the
    // `add_reaction` await resolves, re-check whether our message is
    // still in the queue. If the queued_placeholder mapping has been
    // consumed (i.e. dispatch already promoted us into an active
    // turn), strip the just-added queue-pending emoji so the visual
    // state matches reality.
    if !is_thread_routed && should_add_turn_pending_reaction(dispatch_id_for_thread.as_deref()) {
        // #1190 follow-up: merged messages get ➕ so the user can tell
        // them apart from standalone queue head entries (📬).
        let emoji = if enqueue_outcome.merged {
            '➕'
        } else {
            '📬'
        };
        add_reaction(http, channel_id, user_msg_id, emoji).await;
        // #2036 Surface 3: detect queue→start races where the
        // dispatch path consumed our mapping before this reaction
        // landed and proactively unstick the emoji.
        if !shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id) {
            crate::services::discord::formatting::remove_reaction_raw(
                http,
                channel_id,
                user_msg_id,
                emoji,
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
    // #796: Background-trigger turns (notify-bot driven, info-only) must
    // NOT have their placeholder deleted on race-loss. The placeholder is
    // the user-visible breadcrumb of the background notification (e.g.
    // a `Bash run_in_background` completion message).
    //
    // #1332: Foreground turns EDIT the bare `...` into a `📬 메시지 대기
    // 중` card via the placeholder controller. Mapping was already
    // inserted before enqueue (codex review P2); on edit failure we roll
    // back the mapping AND delete the Discord message so users never see
    // a stale `...` placeholder.
    if turn_kind.is_background_trigger() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔔 RACE: background-trigger placeholder preserved (channel {}, msg {})",
            channel_id,
            placeholder_msg_id
        );
    } else if want_queued_card && !reused_existing_mapping {
        // codex review round-3 P1 + round-5 P2 (finding 1 — atomic
        // ownership coupling) + round-10 P2 (single critical section):
        // between `mailbox_enqueue_intervention` and the `ensure_queued`
        // await below, the active turn can finish and the dispatch
        // path can already have consumed our
        // `(channel_id, user_msg_id)` mapping — at which point the
        // placeholder we POSTed has been promoted to the live response
        // card. Editing it to `📬 메시지 대기 중` (or deleting it on the
        // fallback branch) would corrupt/erase the active card. Round-4
        // checked ownership immediately before the PATCH, but the await
        // window between the check and the PATCH still allowed
        // `dispatch_queued_turn` (or `queue_exit_drain_queued_placeholders`)
        // to consume the mapping concurrently. Round-5 wraps the
        // ownership recheck + `ensure_queued` PATCH + persistence
        // rollback in a single critical section guarded by the
        // per-channel async persistence mutex. Round-10 extends that
        // critical section UPSTREAM through the dispatch-state recheck
        // and the mapping insert: we acquire the persist lock once
        // (above, where `dispatch_already_running_for_our_msg` is
        // computed), and pass the SAME held guard through to this
        // PATCH branch via `persist_guard_for_render`. Every other
        // path that mutates `queued_placeholders` (insert / remove /
        // merged drain / queue-exit drain) takes the same mutex, so
        // the mapping cannot change underneath this PATCH once we
        // hold the lock.
        //
        // Invariant (round-10): the dispatch-state snapshot, mapping insert,
        // ownership recheck, and `ensure_queued` PATCH share ONE held lock
        // guard; any alternate order reopens the round-4 or round-9 hazard.
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
                    // `...` so dispatch never matches a missing Discord
                    // message. The lock guarantees the mapping is unchanged
                    // since the recheck; use `_locked` to avoid reacquiring it.
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
        // codex review round-5 P2 (finding 2): the existing card
        // already shows `📬 메시지 대기 중`. Skip the redundant
        // `ensure_queued` PATCH (the prior race-loss already wrote it,
        // and re-emitting the identical content would hit a
        // `Coalesced` no-op anyway). Leaving the card untouched is
        // correct — the user already sees it.
        //
        // Round-9 note: the round-6 "reused mapping + dedup-rejected
        // enqueue" sub-branch (preserving a card owned by an earlier
        // enqueue) is gone — this code path is only reached when
        // `enqueued == true` because we now return early on dedup
        // rejection (see the `if !enqueued { return Ok(()); }` block
        // above). The earlier owner's lifecycle still owns the card,
        // and our return runs before any placeholder POST/edit.
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ RACE: re-queue reused existing 📬 card without re-render (channel {}, msg {})",
            channel_id,
            placeholder_msg_id
        );
    } else {
        // Background-trigger turns hit the explicit branch above;
        // remaining cases (e.g. is_thread_routed) fall here and have
        // no queued card to render — POSTed placeholder is a bare
        // `...` and would otherwise leak.
        let _ = channel_id.delete_message(http, placeholder_msg_id).await;
    }
    crate::services::discord::formatting::remove_reaction_raw(http, channel_id, user_msg_id, '⏳')
        .await;
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🔀 RACE: message queued (another turn won), channel {}",
        channel_id
    );
    return Ok(());
}

#[cfg(test)]
mod schedule_post_enqueue_idle_drain_tests {
    use super::super::super::super::intake_gate::should_schedule_post_enqueue_idle_drain;

    #[test]
    fn schedules_when_enqueued_and_slot_idle() {
        // Slot idle (no blocking turn) and the message was enqueued — the
        // original race-window strand guard must still schedule the drain.
        assert!(should_schedule_post_enqueue_idle_drain(true, false));
    }

    #[test]
    fn schedules_when_enqueued_behind_background_injection() {
        // #3903 core case: a `/loop`/system-injection turn holds the slot, so
        // it is NOT a blocking turn. The re-enqueued genuine user message must
        // still trigger a drain so it is not stranded behind the injection.
        assert!(should_schedule_post_enqueue_idle_drain(true, false));
    }

    #[test]
    fn skips_when_real_turn_holds_slot() {
        // A real (blocking) turn owns the dequeue chain via its own finalize —
        // scheduling here would only spin redundantly (pre-#3903 behavior).
        assert!(!should_schedule_post_enqueue_idle_drain(true, true));
    }

    #[test]
    fn skips_when_enqueue_was_refused() {
        // Dedup/duplicate refusal: nothing landed in the queue, so there is
        // nothing to drain regardless of the slot state.
        assert!(!should_schedule_post_enqueue_idle_drain(false, false));
        assert!(!should_schedule_post_enqueue_idle_drain(false, true));
    }
}
