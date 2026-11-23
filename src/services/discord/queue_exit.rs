//! Queue-exit feedback: reactions, queued placeholder cards and internal follow-ups.

use super::{
    Arc, ChannelId, HashMap, Instant, Intervention, InterventionMode, MessageId, ProviderKind,
    QueueExitEvent, QueueExitKind, QueueExitPendingPlaceholderDeleter, QueuedCardDisposition,
    QueuedCardTeardown, SharedData, UserId, mailbox_enqueue_intervention, queue_marker,
    queue_overflow_dlq, queued_card_gate, queued_placeholders_store,
    schedule_deferred_idle_queue_kickoff,
};

pub(in crate::services::discord) fn queue_exit_feedback_emoji(kind: QueueExitKind) -> char {
    match kind {
        QueueExitKind::Cancelled => '🚫',
        QueueExitKind::Expired => '⌛',
        // #4260 dual r1: `Overflow` inherits the pre-split ⏏ feedback.
        QueueExitKind::Superseded | QueueExitKind::Overflow => '⏏',
    }
}

/// codex review P2 (#1332 follow-up): replacement card body for a queued
/// placeholder when its intervention exits the queue without ever being
/// dispatched. Replaces the `📬 메시지 대기 중` promise with a concise
/// terminal notice, so the user is not left wondering when the turn will
/// run.
pub(super) fn queue_exit_card_body(kind: QueueExitKind) -> &'static str {
    match kind {
        QueueExitKind::Cancelled => {
            "🚫 **큐에서 제거됨** — 사용자 요청으로 대기 목록에서 제외했습니다."
        }
        QueueExitKind::Expired => "⌛ **큐에서 제거됨** — 대기 시간 초과로 처리되지 않습니다.",
        // #4260 dual r1: the pre-split ⏏ text carries over to `Overflow`.
        QueueExitKind::Superseded | QueueExitKind::Overflow => {
            "⏏ **큐에서 제거됨** — 후속 메시지로 대체되어 처리되지 않습니다."
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QueueExitVisibleCard {
    pub(super) user_msg_id: MessageId,
    pub(super) placeholder_msg_id: MessageId,
    pub(super) kind: QueueExitKind,
}

/// codex review P2 (#1332 follow-up): drain the in-memory `queued_placeholders`
/// + `placeholder_controller` rows for every queue-exit event and return the
/// visible Discord card ids the caller should edit/delete. Split out from
/// `apply_queue_exit_feedback` so the bookkeeping is testable without a
/// serenity HTTP client.
///
/// #5035 (A1/A2): removing the mapping only proves *this* id stopped owning the
/// card, so every drained card goes through `queued_card_gate` and only
/// `Released` ones are returned — which is what keeps the exit-body EDIT /
/// DELETE off a card another queue entry still needs.
pub(super) async fn queue_exit_drain_queued_placeholders(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_exit_events: &[&QueueExitEvent],
) -> Vec<(QueueExitVisibleCard, QueuedCardTeardown)> {
    // codex review round-4 P2 + round-5 P2: hold the channel's persistence
    // mutex (async since round-5 so `.await`-spanning callers serialize too)
    // across the whole batch drain + snapshot write, or a concurrent
    // `insert_queued_placeholder` could win the disk write with a pre-drain
    // snapshot that resurrects already-exited entries on restart.
    let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
    let _persist_guard = persist_lock.lock().await;
    let mut released_cards: Vec<(QueueExitVisibleCard, QueuedCardTeardown)> = Vec::new();
    let mut mutated = false;
    // #5035: complete departing hint (every exiting intervention is in hand).
    // Ordering preference for re-keying only; the verdict ignores it.
    let departing: Vec<MessageId> = queue_exit_events
        .iter()
        .flat_map(|event| {
            std::iter::once(event.intervention.message_id)
                .chain(event.intervention.source_message_ids.iter().copied())
        })
        .collect();
    for event in queue_exit_events {
        for message_id in &event.intervention.source_message_ids {
            if let Some((_, placeholder_msg_id)) = shared
                .queued
                .queued_placeholders
                .remove(&(channel_id, *message_id))
            {
                mutated = true;
                if let QueuedCardDisposition::Released(teardown) =
                    queued_card_gate::release_or_rekey_locked(
                        shared,
                        channel_id,
                        placeholder_msg_id,
                        &departing,
                        &_persist_guard,
                    )
                    .await
                {
                    released_cards.push((
                        QueueExitVisibleCard {
                            user_msg_id: *message_id,
                            placeholder_msg_id,
                            kind: event.kind,
                        },
                        teardown,
                    ));
                }
            }
        }
    }
    // codex review round-3 P2: persist the write-through after the batch
    // drain so a restart sees the same state as memory (queue-exit cleanup
    // must clear the on-disk snapshot, otherwise restart would resurrect
    // mappings for cancelled/expired/superseded interventions).
    if mutated {
        queued_placeholders_store::persist_channel_from_map(
            &shared.queued.queued_placeholders,
            &shared.provider,
            &shared.token_hash,
            channel_id,
        );
    }
    released_cards
}

pub(super) async fn apply_queue_exit_feedback(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_exit_events: &[QueueExitEvent],
) {
    let queue_exit_events: Vec<&QueueExitEvent> = queue_exit_events
        .iter()
        .filter(|event| event.intervention.author_id.get() > 1)
        .collect();
    if queue_exit_events.is_empty() {
        return;
    }

    // #1332: drop stale `📬 메시지 대기 중` placeholder mappings + controller
    // entries up front (Queued rows are exempt from the standard eviction
    // sweep) so a later dispatch never wires a new turn to a cancelled/expired
    // intervention's placeholder; the bookkeeping runs even without a cached
    // serenity ctx so a missing ctx never misroutes the next turn. codex
    // review P2 (#1332 follow-up): also collect the visible card ids to
    // rewrite/delete once a ctx exists (best-effort; drain rationale on the
    // `queue_exit_drain_queued_placeholders` doc).
    let released_cards =
        queue_exit_drain_queued_placeholders(shared, channel_id, &queue_exit_events).await;
    let visible_cards_to_clear: Vec<QueueExitVisibleCard> =
        released_cards.iter().map(|(card, _)| *card).collect();

    // #4260 dual r1: dead-letter + notice for capacity-`Overflow` evicts only
    // (benign Superseded producers pass through untouched). Fire-and-forget —
    // detached spawns inside — and sited before the Http guard below because
    // neither call needs an Http source nor may stall feedback on a pool
    // acquire.
    queue_overflow_dlq::record_queue_overflow_dead_letters(shared, channel_id, &queue_exit_events);
    queue_overflow_dlq::maybe_notify_orphan_queue_overflow(
        shared,
        channel_id,
        &queue_exit_events,
        &visible_cards_to_clear,
    );

    // Phase 5.2 of intake-node-routing (issue #2009): use gateway-or-token
    // fallback so cluster-standby workers can still rewrite queue-exit
    // placeholder cards via REST. Falling back to the deferred-cleanup
    // path is still correct for genuinely-no-token startup races.
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        // #5035: consume the tokens on the deferred path; the parked card is
        // re-gated by the ready-time drain because this verdict goes stale.
        for (_, teardown) in released_cards {
            queued_card_gate::teardown_defer(shared, teardown);
        }
        shared
            .add_pending_queue_exit_placeholder_clears(channel_id, &visible_cards_to_clear)
            .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(target: "agentdesk::services::discord",
            "  [{ts}] ⚠ QUEUE-FEEDBACK: skipped {} queue exit reaction(s) in channel {} (no Http source); queued {} visible card(s) for ready-time cleanup",
            queue_exit_events.len(),
            channel_id,
            visible_cards_to_clear.len(),
        );
        return;
    };

    // codex review P2: rewrite each leftover queued card to a brief
    // exit-state notice so the user is not left looking at a `📬` promise
    // for a turn that will never run. Edit-on-failure falls back to delete
    // — either way the stale `📬 메시지 대기 중` text is removed. We use
    // the shared Discord HTTP boundary instead of the placeholder controller
    // because the controller entry was just detached (and the public
    // `transition` API only renders terminal monitor-handoff cards).
    // #5035: the edit-or-delete pair is now `teardown_exit_body`, reachable
    // only with a gate-issued token.
    for (card, teardown) in released_cards {
        queued_card_gate::teardown_exit_body(&http, shared, teardown, card).await;
    }

    queue_marker::drain_queue_exit_markers(shared, &http, channel_id, &queue_exit_events).await;
    for event in queue_exit_events {
        let message_id = event.intervention.message_id;
        let emoji = queue_exit_feedback_emoji(event.kind);
        queue_marker::note_exit_feedback_added(shared, &http, channel_id, message_id, emoji).await;
    }
}

pub(in crate::services::discord) async fn drain_pending_queue_exit_placeholder_clears(
    shared: &SharedData,
) {
    // Phase 5.2 of intake-node-routing (issue #2009): use gateway-or-token
    // fallback so the deferred drain that fires on `bot_connected` /
    // `runtime_bootstrap` can still run on standby workers.
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    let deleter = QueueExitPendingPlaceholderDeleter { http };
    drain_pending_queue_exit_placeholder_clears_with(shared, &deleter).await;
}

pub(in crate::services::discord) async fn drain_pending_queue_exit_placeholder_clears_with(
    shared: &SharedData,
    deleter: &dyn queued_card_gate::StalePlaceholderDeleter,
) -> (usize, usize) {
    let pending = shared.pending_queue_exit_placeholder_clears();
    if pending.is_empty() {
        return (0, 0);
    }

    let mut cleared_by_channel: HashMap<ChannelId, Vec<(MessageId, MessageId)>> = HashMap::new();
    let mut deleted = 0usize;
    let mut failed = 0usize;
    let mut preserved = 0usize;
    for (channel_id, user_msg_id, placeholder_msg_id) in pending {
        // Re-gate: the verdict taken at parking is stale by now, and the parked owner
        // alone is a partial hint, which cannot flip a verdict. Unmeasured stays parked.
        let teardown = match queued_card_gate::release_or_rekey(
            shared,
            channel_id,
            placeholder_msg_id,
            &[user_msg_id],
        )
        .await
        {
            QueuedCardDisposition::Preserved { owner } => {
                preserved += 1;
                tracing::debug!(target: "agentdesk::services::discord",
                    channel_id = channel_id.get(),
                    user_msg_id = user_msg_id.get(),
                    placeholder_msg_id = placeholder_msg_id.get(),
                    owner = owner.get(),
                    "queue_exit_pending_clear: card belongs to a live queue entry; dropping the pending row without deleting",
                );
                cleared_by_channel
                    .entry(channel_id)
                    .or_default()
                    .push((user_msg_id, placeholder_msg_id));
                continue;
            }
            QueuedCardDisposition::Released(teardown) => teardown,
            QueuedCardDisposition::Unmeasured => continue,
        };
        match queued_card_gate::teardown_via_deleter(shared, deleter, teardown).await {
            Ok(_) => {
                deleted += 1;
                cleared_by_channel
                    .entry(channel_id)
                    .or_default()
                    .push((user_msg_id, placeholder_msg_id));
                tracing::debug!(target: "agentdesk::services::discord",
                    channel_id = channel_id.get(),
                    user_msg_id = user_msg_id.get(),
                    placeholder_msg_id = placeholder_msg_id.get(),
                    "queue_exit_pending_clear: deleted queued placeholder card",
                );
            }
            Err(error) => {
                failed += 1;
                tracing::warn!(target: "agentdesk::services::discord",
                    channel_id = channel_id.get(),
                    user_msg_id = user_msg_id.get(),
                    placeholder_msg_id = placeholder_msg_id.get(),
                    "queue_exit_pending_clear: failed to delete queued placeholder card ({error}); keeping pending",
                );
            }
        }
    }

    for (channel_id, cards) in cleared_by_channel {
        shared
            .remove_pending_queue_exit_placeholder_clears(channel_id, &cards)
            .await;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(target: "agentdesk::services::discord",
        "  [{ts}] 🧹 QUEUE-EXIT: deleted {deleted} pending queued placeholder card(s) after ctx ready (failed {failed}, preserved {preserved})",
    );
    (deleted, failed)
}

pub(in crate::services::discord) async fn enqueue_internal_followup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reply_message_id: MessageId,
    text: impl Into<String>,
    reason: &'static str,
) -> bool {
    let outcome = mailbox_enqueue_intervention(
        shared,
        provider,
        channel_id,
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: reply_message_id,
            queued_generation: shared.restart.current_generation,
            source_message_ids: vec![reply_message_id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.into(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
        },
    )
    .await;

    if let Some(error) = outcome.persistence_error.as_ref() {
        tracing::error!(target: "agentdesk::services::discord",
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            reason,
            error = %error,
            "internal followup enqueue failed durable pending-queue persistence"
        );
        return false;
    }

    if outcome.enqueued {
        schedule_deferred_idle_queue_kickoff(shared.clone(), provider.clone(), channel_id, reason);
    }

    outcome.enqueued
}
