use super::super::intake_queue_transaction::{IntakeQueueCommitEffects, SoftInterventionSpec};
use super::*;
use crate::services::discord::outbound::reaction_control::{
    ReactionControlReplyReason, send_reaction_control_reply,
};

/// Pick the queue-pending reaction emoji based on the enqueue outcome.
/// Standalone queue head entries get `📬`; merged-into-previous entries get
/// `➕` so users can tell merged from standalone at a glance (#1190 follow-up).
#[cfg(test)]
pub(in crate::services::discord::router) fn queue_pending_reaction_for(
    outcome: crate::services::discord::MailboxEnqueueOutcome,
) -> char {
    super::super::intake_queue_transaction::queue_pending_reaction_for(&outcome)
}

/// #3182: add the queue-pending reaction (📬 standalone / ➕ merged) and
/// self-heal the late-add race.
///
/// Every `intake_gate` enqueue path adds this reaction AFTER one or more
/// Discord `await`s (placeholder POST, queued-card render, dispatch-guard
/// bookkeeping). During that await the active turn can finish and the
/// queued-dispatch entrypoints (`DiscordGateway::dispatch_queued_turn` /
/// `kickoff_idle_queues`) can dequeue THIS message and run their 📬/➕ reaction
/// drain BEFORE the add lands — stranding the reaction on an
/// already-promoted/processed message (the gate-path analog of the
/// `intake_turn.rs` Surface-3 self-heal, #2036). So after the add await
/// resolves, re-snapshot the mailbox and strip the reaction if the message is
/// no longer queued.
///
/// Validity uses SOURCE MEMBERSHIP — head id OR any `source_message_ids`
/// entry — for BOTH standalone and merged. The head-only rule is correct only
/// for placeholder-card OWNERSHIP (one card per active turn); a merged source
/// `B` that is still queued under a newer head `C` keeps a valid `➕`, so a
/// head-only check would wrongly remove it. Reconciled queue-marker removal is
/// best-effort (no-op when already cleared), and only the calling provider bot's
/// own @me reaction is removed.
async fn add_queue_pending_reaction_self_healing(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    emoji: char,
) {
    crate::services::discord::queue_marker::note_added_current(
        &data.shared,
        &ctx.http,
        channel_id,
        user_msg_id,
        emoji,
        "intake_gate_queue_pending",
    )
    .await;
    let still_queued = {
        let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
        snapshot.intervention_queue.iter().any(|intervention| {
            intervention.message_id == user_msg_id
                || intervention.source_message_ids.contains(&user_msg_id)
        })
    };
    if !still_queued {
        crate::services::discord::queue_marker::note_removed_current(
            &data.shared,
            &ctx.http,
            channel_id,
            user_msg_id,
            emoji,
            "intake_gate_queue_pending_self_heal",
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

pub(super) struct IntakeGateQueueEffects<'a> {
    pub(super) ctx: &'a serenity::Context,
    pub(super) data: &'a Data,
}

/// #3903/#4024 — pure verdict for whether a post-enqueue deferred idle-queue
/// drain must run after an intervention actually landed in the mailbox queue.
///
/// Schedule when the enqueue was accepted and no REAL (blocking) turn currently
/// owns the slot. A background/system-injection turn is intentionally treated as
/// non-blocking so queued user work does not strand behind it.
pub(in crate::services::discord::router) fn should_schedule_post_enqueue_idle_drain(
    enqueued: bool,
    has_blocking_active_turn: bool,
) -> bool {
    enqueued && !has_blocking_active_turn
}

#[async_trait::async_trait]
impl IntakeQueueCommitEffects for IntakeGateQueueEffects<'_> {
    async fn enqueue_soft_intervention(
        &mut self,
        intervention: SoftInterventionSpec,
    ) -> crate::services::discord::MailboxEnqueueOutcome {
        let channel_id = intervention.channel_id;
        mailbox_enqueue_intervention(
            &self.data.shared,
            &self.data.provider,
            channel_id,
            intervention.into_intervention(),
        )
        .await
    }

    async fn apply_pending_reaction(
        &mut self,
        channel_id: serenity::ChannelId,
        message_id: serenity::MessageId,
        emoji: char,
    ) {
        add_queue_pending_reaction_self_healing(self.ctx, self.data, channel_id, message_id, emoji)
            .await;
    }

    fn advance_checkpoint(
        &mut self,
        channel_id: serenity::ChannelId,
        message_id: serenity::MessageId,
    ) -> u64 {
        crate::services::discord::advance_last_message_checkpoint(
            &self.data.shared,
            &self.data.provider,
            channel_id,
            message_id,
        )
    }

    async fn schedule_idle_kickoff(&mut self) -> usize {
        crate::services::discord::kickoff_idle_queues(
            self.ctx,
            &self.data.shared,
            &self.data.token,
            &self.data.provider,
        )
        .await
    }
}

#[cfg(test)]
mod schedule_post_enqueue_idle_drain_tests {
    use super::should_schedule_post_enqueue_idle_drain;

    #[test]
    fn schedules_when_enqueued_and_slot_idle() {
        assert!(should_schedule_post_enqueue_idle_drain(true, false));
    }

    #[test]
    fn skips_when_real_turn_holds_slot() {
        assert!(!should_schedule_post_enqueue_idle_drain(true, true));
    }

    #[test]
    fn skips_when_enqueue_was_refused() {
        assert!(!should_schedule_post_enqueue_idle_drain(false, false));
        assert!(!should_schedule_post_enqueue_idle_drain(false, true));
    }
}

/// #3009: when a follow-up message is *merged* into the previous queue head
/// (`MailboxEnqueueOutcome::merged == true`), the head intervention's
/// `message_id` is rewritten to this follow-up's id while the older ids are
/// retained as `source_message_ids` (see `turn_orchestrator::enqueue_intervention`).
/// The previous queue head already owns a `📬 메시지 대기 중` placeholder card
/// keyed under one of those older `source_message_ids`. Re-render and re-key
/// that single card under the new head id instead of POSTing a brand-new card,
/// so a burst of follow-ups collapses to ONE visible waiting placeholder.
///
/// Returns `true` when an existing merged-group placeholder was successfully
/// reused (no new card needed). Returns `false` when there was no existing
/// placeholder to reuse (e.g. the head's earlier POST failed), in which case
/// the caller falls back to creating a fresh card.
/// Pure decision helper for #3009 merged-placeholder reuse. Given the merged
/// head's `source_message_ids` and a lookup into the live `queued_placeholders`
/// map, return the `(prior_source_id, placeholder_msg_id)` of the existing
/// waiting card that should be re-keyed under the new head id. Returns `None`
/// when no earlier source id of the group owns a card yet (caller POSTs a fresh
/// one). The follow-up's own id is excluded because it has no card yet.
fn pick_reusable_merged_placeholder<F>(
    source_message_ids: &[serenity::MessageId],
    user_msg_id: serenity::MessageId,
    mut lookup: F,
) -> Option<(serenity::MessageId, serenity::MessageId)>
where
    F: FnMut(serenity::MessageId) -> Option<serenity::MessageId>,
{
    source_message_ids
        .iter()
        .copied()
        .filter(|id| *id != user_msg_id)
        .find_map(|id| lookup(id).map(|placeholder| (id, placeholder)))
}

/// Outcome of the #3009 merged-placeholder reuse attempt. Distinguishes the
/// "reuse handled it" cases (caller must NOT post a fresh card) from the
/// genuine "no reusable card, post a fresh one" case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergedPlaceholderReuse {
    /// Reused / no-op success: a single waiting card already represents this
    /// merged backlog (either re-keyed here, or this follow-up is a stale
    /// non-head source whose content is already folded into the head's card).
    /// The caller must skip the fresh-card POST.
    Handled,
    /// No reusable card exists for the head and the head's own card render is
    /// still required — the caller should POST a fresh placeholder.
    PostFresh,
}

async fn reuse_merged_queued_placeholder(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
) -> MergedPlaceholderReuse {
    // Serialize the whole reuse (snapshot read → re-key → re-render) against
    // every other `queued_placeholders` mutation / queue-exit drain on this
    // channel. Without the lock a concurrent dispatch/queue-exit drain could
    // remove the source mapping between our snapshot read and our re-key,
    // leaking the card or resurrecting a stale mapping (#3009 TOCTOU guard).
    let persist_lock = data.shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock_owned().await;

    let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
    // Locate the merged head whose source ids include this follow-up. The
    // enqueue path rewrote its `message_id` to `user_msg_id`, so match on the
    // source-id membership to stay robust even if the head id moved again.
    let Some(merged_head) = snapshot
        .intervention_queue
        .iter()
        .find(|intervention| intervention.source_message_ids.contains(&user_msg_id))
    else {
        // No longer queued (already dispatched / exited). The merge outcome
        // told us this id folded into a prior head, so its content is not
        // standalone — do NOT post a fresh card for it.
        return MergedPlaceholderReuse::Handled;
    };
    let already_started = snapshot.active_user_message_id == Some(user_msg_id);
    if already_started {
        // The merged head is being dispatched; its card transition is owned by
        // the dispatch hand-off. No fresh card needed.
        return MergedPlaceholderReuse::Handled;
    }

    // codex round-1 P2 (identity race) + round-2 P2 (stale-source duplicate):
    // only re-key the card when `user_msg_id` is STILL the merged head. Under
    // bursty concurrent merges a newer follow-up can have already advanced the
    // head past this id (rewriting `message_id`), leaving `user_msg_id` as a
    // non-head source. Re-keying to a non-head id would let the dispatch drain
    // delete the only queued card. AND posting a fresh card for this stale
    // non-head source would recreate the very duplicate this path prevents —
    // its content is already folded into the head's card. So treat a stale
    // non-head merged source as Handled (no new card); the head's own reuse
    // pass owns the single visible card.
    if merged_head.message_id != user_msg_id {
        return MergedPlaceholderReuse::Handled;
    }

    // codex round-3 P3: `enqueue_intervention` already appended this follow-up's
    // text to the head's accumulated `text` (e.g. "A\nB"). Render the FULL
    // merged request on the single reused card so the placeholder represents the
    // whole pending backlog, not just the latest line (`text` is only this
    // Discord message). Capture it before any further borrows of `snapshot`.
    let merged_request_text = merged_head.text.clone();

    // Find the existing placeholder card owned by any earlier source id of the
    // merged group (excluding this follow-up, which has no card yet).
    let existing =
        pick_reusable_merged_placeholder(&merged_head.source_message_ids, user_msg_id, |id| {
            data.shared
                .queued
                .queued_placeholders
                .get(&(channel_id, id))
                .map(|entry| *entry.value())
        });
    let Some((prior_source_id, placeholder_msg_id)) = existing else {
        // We are the current head but no earlier source owns a card yet (e.g.
        // the prior head's POST failed). A fresh card IS warranted here.
        return MergedPlaceholderReuse::PostFresh;
    };

    // Re-key the mapping so the head id (== `user_msg_id`) owns the card the
    // dispatch hand-off consumes, and the old source-id mapping is dropped so
    // a later merged-drain cannot delete this still-live card. The
    // placeholder_controller entry is keyed by the *placeholder* Discord
    // message id, which is unchanged, so no controller re-keying is needed.
    data.shared
        .remove_queued_placeholder_locked(channel_id, prior_source_id);
    data.shared
        .insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);

    // Refresh the card body with merged request text; unchanged renders
    // coalesce, changed ones edit.
    let gateway = crate::services::discord::gateway::DiscordGateway::new(
        ctx.http.clone(),
        data.shared.clone(),
        data.provider.clone(),
        None,
    );
    let key = crate::services::discord::placeholder_controller::PlaceholderKey {
        provider: data.provider.clone(),
        channel_id,
        message_id: placeholder_msg_id,
    };
    let queued_input = crate::services::discord::placeholder_controller::PlaceholderActiveInput {
        reason: crate::services::discord::formatting::MonitorHandoffReason::Queued,
        started_at_unix: chrono::Utc::now().timestamp(),
        tool_summary: None,
        command_summary: None,
        reason_detail: None,
        context_line: None,
        request_line: Some(merged_request_text),
        progress_line: None,
    };
    let outcome = data
        .shared
        .ui
        .placeholder_controller
        .ensure_queued(&gateway, key, queued_input)
        .await;

    // codex round-1 P2 (edit-failure rollback): only commit the re-key after
    // `Edited`/`Coalesced`. On `EditFailed`/`Rejected`, restore the prior source
    // id and let the caller fall back to fresh POST, with the persistence lock
    // held so no drain observes the half-applied re-key.
    if !matches!(
        outcome,
        crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::Edited
            | crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::Coalesced
    ) {
        data.shared
            .remove_queued_placeholder_locked(channel_id, user_msg_id);
        data.shared.insert_queued_placeholder_locked(
            channel_id,
            prior_source_id,
            placeholder_msg_id,
        );
        drop(persist_guard);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ QUEUE-ACK: merged reuse of placeholder {} for message {} in channel {} failed to render ({:?}); kept prior mapping, falling back to fresh card",
            placeholder_msg_id,
            user_msg_id,
            channel_id,
            outcome,
        );
        return MergedPlaceholderReuse::PostFresh;
    }
    drop(persist_guard);

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ➕ QUEUE-ACK: merged queued message {} into existing placeholder {} in channel {} (no new card)",
        user_msg_id,
        placeholder_msg_id,
        channel_id
    );
    MergedPlaceholderReuse::Handled
}

/// #3082 part A — pure decision helper: of the existing queued cards on a
/// channel (`(owner_msg_id, placeholder_msg_id)` pairs), pick the single card to
/// re-key onto `new_head` so the channel keeps AT MOST ONE visible queued card.
///
/// * Never reuse a card already owned by `new_head` (it has no card yet / would
///   be a self-reuse).
/// * Prefer a card whose owner is still present in the live queue (`queued_ids`)
///   — that is the canonical waiting card; fall back to any other channel card
///   (e.g. a stale owner whose intervention already drained but whose card has
///   not been cleaned up yet) so we coalesce rather than stack.
/// * `None` when there is no other channel card to reuse → caller posts fresh.
fn pick_channel_queued_placeholder(
    channel_cards: &[(serenity::MessageId, serenity::MessageId)],
    new_head: serenity::MessageId,
    queued_ids: &std::collections::HashSet<serenity::MessageId>,
) -> Option<(serenity::MessageId, serenity::MessageId)> {
    let mut fallback: Option<(serenity::MessageId, serenity::MessageId)> = None;
    for &(owner, placeholder) in channel_cards {
        if owner == new_head {
            continue;
        }
        if queued_ids.contains(&owner) {
            return Some((owner, placeholder));
        }
        fallback.get_or_insert((owner, placeholder));
    }
    fallback
}

/// #3082 part A — generalize "AT MOST ONE queued card per channel/active turn".
///
/// `reuse_merged_queued_placeholder` only collapses *merge-eligible*
/// follow-ups (same author, no reply boundary, within the merge rules). Messages
/// that skip merge — a different author, a reply-boundary message, or an
/// identical re-send outside the 10s dedup window — each used to POST their own
/// fresh `📬` card, leaving multiple waiting cards in the timeline. This helper
/// closes that gap: before POSTing a fresh card, reuse ANY existing queued card
/// already owned by this channel's active turn, re-keying it onto the new head
/// id and re-rendering it with the latest request text.
///
/// Returns:
/// * `Some(true)`  — an existing card was successfully re-keyed/re-rendered onto
///   `user_msg_id`. Caller must NOT post a fresh card.
/// * `Some(false)` — we found a card and tried to reuse it but the re-render
///   failed; the mapping was rolled back. Caller should fall through to the
///   fresh POST path (which deletes/replaces as needed).
/// * `None`        — there is no existing queued card to reuse on this channel.
///   Caller posts a fresh card as before.
async fn reuse_any_queued_placeholder_for_channel(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    text: &str,
) -> Option<bool> {
    // Serialize against every other `queued_placeholders` mutation on this
    // channel (dispatch hand-off, queue-exit drain, merged reuse) — same TOCTOU
    // guard as `reuse_merged_queued_placeholder`.
    let persist_lock = data.shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock_owned().await;

    let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
    // Only reuse while this message is genuinely still queued behind an active
    // turn. If it already started, the dispatch hand-off owns its card; if it is
    // no longer queued, there is nothing to represent.
    let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
        intervention.message_id == user_msg_id
            || intervention.source_message_ids.contains(&user_msg_id)
    });
    if snapshot.active_user_message_id == Some(user_msg_id) || !still_queued {
        return None;
    }

    // Find ANY existing queued card on this channel that does not already belong
    // to `user_msg_id`. Prefer the card owned by a message still present in the
    // queue (the live waiting head) so we re-key the canonical card; fall back to
    // any other channel-scoped card otherwise. Either way the invariant is "one
    // visible card", so reusing whichever exists is correct.
    let queued_ids: std::collections::HashSet<serenity::MessageId> = snapshot
        .intervention_queue
        .iter()
        .flat_map(|intervention| {
            std::iter::once(intervention.message_id)
                .chain(intervention.source_message_ids.iter().copied())
        })
        .collect();

    let channel_cards: Vec<(serenity::MessageId, serenity::MessageId)> = data
        .shared
        .queued
        .queued_placeholders
        .iter()
        .filter_map(|entry| {
            let (ch, owner) = *entry.key();
            (ch == channel_id).then_some((owner, *entry.value()))
        })
        .collect();

    let Some((prior_owner, placeholder_msg_id)) =
        pick_channel_queued_placeholder(&channel_cards, user_msg_id, &queued_ids)
    else {
        return None;
    };

    // Re-key the single card onto this message id so the dispatch hand-off /
    // queue-exit drain track exactly one card per active turn.
    data.shared
        .remove_queued_placeholder_locked(channel_id, prior_owner);
    data.shared
        .insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);

    let gateway = crate::services::discord::gateway::DiscordGateway::new(
        ctx.http.clone(),
        data.shared.clone(),
        data.provider.clone(),
        None,
    );
    let key = crate::services::discord::placeholder_controller::PlaceholderKey {
        provider: data.provider.clone(),
        channel_id,
        message_id: placeholder_msg_id,
    };
    let queued_input = crate::services::discord::placeholder_controller::PlaceholderActiveInput {
        reason: crate::services::discord::formatting::MonitorHandoffReason::Queued,
        started_at_unix: chrono::Utc::now().timestamp(),
        tool_summary: None,
        command_summary: None,
        reason_detail: None,
        context_line: None,
        request_line: Some(text.to_string()),
        progress_line: None,
    };
    let outcome = data
        .shared
        .ui
        .placeholder_controller
        .ensure_queued(&gateway, key, queued_input)
        .await;

    if !matches!(
        outcome,
        crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::Edited
            | crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::Coalesced
    ) {
        // Render failed — roll back the re-key so the prior owner keeps the card,
        // and let the caller post a fresh one (which handles deletion/cleanup).
        data.shared
            .remove_queued_placeholder_locked(channel_id, user_msg_id);
        data.shared
            .insert_queued_placeholder_locked(channel_id, prior_owner, placeholder_msg_id);
        drop(persist_guard);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ QUEUE-ACK: channel-wide reuse of placeholder {} for message {} in channel {} failed to render ({:?}); kept prior mapping, falling back to fresh card",
            placeholder_msg_id,
            user_msg_id,
            channel_id,
            outcome,
        );
        return Some(false);
    }
    drop(persist_guard);

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ➕ QUEUE-ACK: coalesced queued message {} into existing channel placeholder {} in channel {} (one card per active turn)",
        user_msg_id,
        placeholder_msg_id,
        channel_id
    );
    Some(true)
}

pub(super) async fn render_visible_queued_ack(
    ctx: &serenity::Context,
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    text: &str,
    merged: bool,
) -> bool {
    // #3009: a merged follow-up reuses the existing waiting placeholder instead
    // of stacking a duplicate card. Only fall through to a fresh POST when this
    // message is the current merged head AND no prior card exists to reuse
    // (e.g. the head's earlier POST failed). A stale non-head merged source is
    // already represented by the head's single card, so it is `Handled` (no new
    // card) — posting one would recreate the duplicate this path prevents.
    if merged {
        match reuse_merged_queued_placeholder(ctx, data, channel_id, user_msg_id).await {
            MergedPlaceholderReuse::Handled => return true,
            MergedPlaceholderReuse::PostFresh => {}
        }
    }
    // #3082 part A: even when this message did NOT merge (different author, a
    // reply-boundary message, or an identical re-send outside the dedup window),
    // enforce a single visible queued card per active turn by reusing any card
    // already owned by this channel's active turn instead of POSTing a second
    // one. `Some(true)` => handled (no fresh card); `Some(false)`/`None` => fall
    // through to the fresh POST below.
    if let Some(true) =
        reuse_any_queued_placeholder_for_channel(ctx, data, channel_id, user_msg_id, text).await
    {
        return true;
    }
    let post_result = crate::services::discord::gateway::send_intake_placeholder(
        ctx.http.clone(),
        data.shared.clone(),
        channel_id,
        None,
        // #3082 P2-3: this IS the queued-turn "📬" notice — wait behind any
        // in-flight multi-chunk answer flush so the card lands as a trailing
        // notice, never interleaved between answer chunks.
        true,
    )
    .await;
    let placeholder_msg_id = match post_result {
        Ok(msg_id) => msg_id,
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ QUEUE-ACK: placeholder POST failed for queued message {} in channel {}: {}",
                user_msg_id,
                channel_id,
                error
            );
            // #1984 (codex C — observation): record the failure without
            // changing user-visible behaviour. The user message is already
            // enqueued in the mailbox at this point — only the visible
            // "queued" card rendering is missing — so the recovery label
            // reflects that the message itself is not lost.
            crate::services::observability::emit_intake_placeholder_post_failed(
                data.provider.as_str(),
                channel_id.get(),
                Some(user_msg_id.get()),
                "queue_ack_visible",
                "queued_card_skipped",
                &error.to_string(),
            );
            // #2044 F13: best-effort user-visible fallback. Without
            // this, a Discord 5xx on the queued-card POST left users
            // with "no card / no reaction yet / message silently
            // queued" until the per-message 📬 reaction landed later
            // — surface a short text reply so the user knows their
            // message is in the queue even when the card path
            // failed. Reply is rate-limited and best-effort.
            send_reaction_control_reply(
                ctx,
                &data.shared,
                channel_id,
                user_msg_id,
                ReactionControlReplyReason::QueuedCardPostFailed,
                "📬 큐에 추가됨 — 카드 표시는 실패했지만 메시지는 큐잉되었습니다.",
            )
            .await;
            return false;
        }
    };

    let persist_lock = data.shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock_owned().await;
    let snapshot = mailbox_snapshot(&data.shared, channel_id).await;
    // codex round-3 P2: for a merged fallback (reuse returned `PostFresh`),
    // require this id to STILL be the queued head before committing a fresh
    // card. Re-snapshotting under the persist lock closes the burst race where
    // a newer follow-up merged (advancing the head past `user_msg_id`) while
    // our `send_intake_placeholder` POST was in flight — without this, a stale
    // non-head source could keep its own card and recreate the #3009 duplicate.
    // Non-merged (standalone) messages keep the original source-id membership
    // check: they are their own head, so the two conditions coincide.
    let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
        if merged {
            intervention.message_id == user_msg_id
        } else {
            intervention.message_id == user_msg_id
                || intervention.source_message_ids.contains(&user_msg_id)
        }
    });
    let already_started = snapshot.active_user_message_id == Some(user_msg_id);
    if already_started || !still_queued {
        drop(persist_guard);
        // #2044 F13: route Discord 5xx/404 on delete_message into the
        // deferred clear queue instead of silently swallowing them.
        // Without this, a stale "📬 메시지 대기 중" card could remain
        // visible after the turn already started or the queue moved on.
        if let Err(error) = channel_id
            .delete_message(&ctx.http, placeholder_msg_id)
            .await
        {
            tracing::warn!(
                "  [{ts}] ⚠ QUEUE-ACK: delete placeholder {} in channel {} failed ({error}); deferring cleanup",
                placeholder_msg_id,
                channel_id,
                ts = chrono::Local::now().format("%H:%M:%S"),
            );
            data.shared
                .add_pending_queue_exit_placeholder_clear_one(
                    channel_id,
                    user_msg_id,
                    placeholder_msg_id,
                )
                .await;
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔁 QUEUE-ACK: queued message {} in channel {} no longer needs a queued card",
            user_msg_id,
            channel_id
        );
        return false;
    }

    data.shared
        .insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);
    let gateway = crate::services::discord::gateway::DiscordGateway::new(
        ctx.http.clone(),
        data.shared.clone(),
        data.provider.clone(),
        None,
    );
    let key = crate::services::discord::placeholder_controller::PlaceholderKey {
        provider: data.provider.clone(),
        channel_id,
        message_id: placeholder_msg_id,
    };
    let queued_input = crate::services::discord::placeholder_controller::PlaceholderActiveInput {
        reason: crate::services::discord::formatting::MonitorHandoffReason::Queued,
        started_at_unix: chrono::Utc::now().timestamp(),
        tool_summary: None,
        command_summary: None,
        reason_detail: None,
        context_line: None,
        request_line: Some(text.to_string()),
        progress_line: None,
    };
    let outcome = data
        .shared
        .ui
        .placeholder_controller
        .ensure_queued(&gateway, key, queued_input)
        .await;
    if matches!(
        outcome,
        crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::Edited
            | crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::Coalesced
    ) {
        drop(persist_guard);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📬 QUEUE-ACK: rendered queued placeholder for message {} in channel {}",
            user_msg_id,
            channel_id
        );
        return true;
    }

    data.shared
        .remove_queued_placeholder_locked(channel_id, user_msg_id);
    drop(persist_guard);
    // #2044 F13: same deferred-cleanup fallback as the early-return
    // branch above — a delete_message error here is logged + enqueued
    // for the next `drain_pending_queue_exit_placeholder_clears` pass
    // instead of being silently discarded.
    if let Err(error) = channel_id
        .delete_message(&ctx.http, placeholder_msg_id)
        .await
    {
        tracing::warn!(
            "  [{ts}] ⚠ QUEUE-ACK: delete placeholder {} in channel {} failed after render miss ({error}); deferring cleanup",
            placeholder_msg_id,
            channel_id,
            ts = chrono::Local::now().format("%H:%M:%S"),
        );
        data.shared
            .add_pending_queue_exit_placeholder_clear_one(
                channel_id,
                user_msg_id,
                placeholder_msg_id,
            )
            .await;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ QUEUE-ACK: queued placeholder render failed for message {} in channel {}; deleted placeholder",
        user_msg_id,
        channel_id
    );
    false
}

/// #3009 — pure-logic tests for merged-placeholder reuse. These exercise the
/// `pick_reusable_merged_placeholder` decision (which card a merged follow-up
/// re-keys) without needing a live serenity ctx.
#[cfg(test)]
mod merged_placeholder_reuse_pure_tests {
    use super::*;
    use poise::serenity_prelude::MessageId;
    use std::collections::HashMap;

    #[test]
    fn merged_followup_reuses_prior_head_card() {
        // Queue head started as msg A (owns card CA); follow-up B merged in,
        // so the head's source ids are [A, B] and the head message_id == B.
        let a = MessageId::new(800_000_000_000_001);
        let b = MessageId::new(800_000_000_000_002);
        let card_a = MessageId::new(700_000_000_000_001);
        let map: HashMap<MessageId, MessageId> = [(a, card_a)].into_iter().collect();

        let picked = pick_reusable_merged_placeholder(&[a, b], b, |id| map.get(&id).copied());

        // The follow-up B must reuse A's existing card (no new card).
        assert_eq!(picked, Some((a, card_a)));
    }

    #[test]
    fn second_merged_followup_reuses_rekeyed_card() {
        // After B merged and re-keyed CA under B, a third follow-up C merges:
        // source ids [A, B, C], only B currently owns the card.
        let a = MessageId::new(800_000_000_000_001);
        let b = MessageId::new(800_000_000_000_002);
        let c = MessageId::new(800_000_000_000_003);
        let card = MessageId::new(700_000_000_000_001);
        let map: HashMap<MessageId, MessageId> = [(b, card)].into_iter().collect();

        let picked = pick_reusable_merged_placeholder(&[a, b, c], c, |id| map.get(&id).copied());

        assert_eq!(picked, Some((b, card)));
    }

    #[test]
    fn no_prior_card_falls_back_to_fresh_post() {
        // The head's earlier POST failed, so no source id owns a card yet.
        let a = MessageId::new(800_000_000_000_001);
        let b = MessageId::new(800_000_000_000_002);
        let map: HashMap<MessageId, MessageId> = HashMap::new();

        let picked = pick_reusable_merged_placeholder(&[a, b], b, |id| map.get(&id).copied());

        assert_eq!(picked, None);
    }

    #[test]
    fn own_id_card_is_never_reused() {
        // Defensive: even if the follow-up id somehow already mapped to a card,
        // it must be excluded so we never "reuse" the card we are about to
        // create / treat the new head as its own prior.
        let a = MessageId::new(800_000_000_000_001);
        let b = MessageId::new(800_000_000_000_002);
        let card_b = MessageId::new(700_000_000_000_002);
        let map: HashMap<MessageId, MessageId> = [(b, card_b)].into_iter().collect();

        let picked = pick_reusable_merged_placeholder(&[a, b], b, |id| map.get(&id).copied());

        assert_eq!(picked, None);
    }
}

/// #3082 part A pure decision tests for `pick_channel_queued_placeholder` — the
/// "one card per active turn" coalescing that closes the gaps merge cannot
/// (different author, reply-boundary, identical re-send outside the dedup
/// window). These don't need a live serenity ctx.
#[cfg(test)]
mod channel_queued_placeholder_pure_tests {
    use super::*;
    use poise::serenity_prelude::MessageId;
    use std::collections::HashSet;

    fn ids(items: &[MessageId]) -> HashSet<MessageId> {
        items.iter().copied().collect()
    }

    #[test]
    fn different_author_reuses_existing_channel_card() {
        // Author A queued first (card CA, still in queue). Author B's message —
        // which does NOT merge with A — must reuse CA, not POST a second card.
        let a = MessageId::new(900_000_000_000_001);
        let b = MessageId::new(900_000_000_000_002);
        let card_a = MessageId::new(710_000_000_000_001);
        let cards = vec![(a, card_a)];
        let queued = ids(&[a, b]);

        let picked = pick_channel_queued_placeholder(&cards, b, &queued);
        assert_eq!(picked, Some((a, card_a)), "B must coalesce onto A's card");
    }

    #[test]
    fn reply_boundary_message_reuses_existing_card() {
        // Same author, but the new message has a reply boundary so merge was
        // skipped. Still must coalesce onto the single channel card.
        let a = MessageId::new(900_000_000_000_010);
        let b = MessageId::new(900_000_000_000_011);
        let card_a = MessageId::new(710_000_000_000_010);
        let cards = vec![(a, card_a)];
        let queued = ids(&[a, b]);

        let picked = pick_channel_queued_placeholder(&cards, b, &queued);
        assert_eq!(picked, Some((a, card_a)));
    }

    #[test]
    fn identical_resend_outside_dedup_window_reuses_card() {
        // The identical-resend dedup only fires within 10s; a later identical
        // re-send is enqueued as a new head but must still coalesce.
        let a = MessageId::new(900_000_000_000_020);
        let b = MessageId::new(900_000_000_000_021);
        let card_a = MessageId::new(710_000_000_000_020);
        let cards = vec![(a, card_a)];
        let queued = ids(&[a, b]);

        let picked = pick_channel_queued_placeholder(&cards, b, &queued);
        assert_eq!(picked, Some((a, card_a)));
    }

    #[test]
    fn prefers_live_queue_owner_over_stale_card() {
        // Two cards exist on the channel: a stale one (owner drained) and the
        // live waiting head. The live head's card must win so we re-key the
        // canonical card; the stale one is left for normal cleanup.
        let stale = MessageId::new(900_000_000_000_030);
        let live = MessageId::new(900_000_000_000_031);
        let new = MessageId::new(900_000_000_000_032);
        let card_stale = MessageId::new(710_000_000_000_030);
        let card_live = MessageId::new(710_000_000_000_031);
        // Stale first so the loop would hit it before the live one — preference
        // logic must still return the live card.
        let cards = vec![(stale, card_stale), (live, card_live)];
        let queued = ids(&[live, new]);

        let picked = pick_channel_queued_placeholder(&cards, new, &queued);
        assert_eq!(picked, Some((live, card_live)));
    }

    #[test]
    fn falls_back_to_any_card_when_no_live_owner() {
        // No card owner is still in the queue (all drained) but a visible card
        // lingers — coalesce onto it rather than stacking a new one.
        let stale = MessageId::new(900_000_000_000_040);
        let new = MessageId::new(900_000_000_000_041);
        let card_stale = MessageId::new(710_000_000_000_040);
        let cards = vec![(stale, card_stale)];
        let queued = ids(&[new]);

        let picked = pick_channel_queued_placeholder(&cards, new, &queued);
        assert_eq!(picked, Some((stale, card_stale)));
    }

    #[test]
    fn no_existing_card_posts_fresh() {
        // Nothing on the channel yet → None → caller posts the first card.
        let new = MessageId::new(900_000_000_000_050);
        let cards: Vec<(MessageId, MessageId)> = Vec::new();
        let queued = ids(&[new]);

        assert_eq!(pick_channel_queued_placeholder(&cards, new, &queued), None);
    }

    #[test]
    fn own_card_is_never_self_reused() {
        // Defensive: a card already keyed to the new head must be ignored so we
        // never "reuse" the card we are about to create for it.
        let new = MessageId::new(900_000_000_000_060);
        let card_new = MessageId::new(710_000_000_000_060);
        let cards = vec![(new, card_new)];
        let queued = ids(&[new]);

        assert_eq!(pick_channel_queued_placeholder(&cards, new, &queued), None);
    }

    #[test]
    fn three_queued_messages_collapse_to_one_card() {
        // Acceptance: queueing 3 messages leaves exactly one visible card.
        // m1 posts CA. m2 reuses CA (re-keyed to m2). m3 reuses it again.
        let m1 = MessageId::new(900_000_000_000_070);
        let m2 = MessageId::new(900_000_000_000_071);
        let m3 = MessageId::new(900_000_000_000_072);
        let card = MessageId::new(710_000_000_000_070);

        // m2: only m1 owns the card.
        let picked_2 = pick_channel_queued_placeholder(&[(m1, card)], m2, &ids(&[m1, m2]));
        assert_eq!(picked_2, Some((m1, card)), "m2 coalesces onto m1's card");

        // After re-key, the card is owned by m2; m3 coalesces onto it.
        let picked_3 = pick_channel_queued_placeholder(&[(m2, card)], m3, &ids(&[m1, m2, m3]));
        assert_eq!(picked_3, Some((m2, card)), "m3 coalesces onto the one card");
    }
}
