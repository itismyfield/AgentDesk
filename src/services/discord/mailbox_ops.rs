//! Mailbox and idle-queue operations the Discord runtime runs against a channel's actor.

#[cfg(unix)]
use super::tmux;
use super::{
    ActiveTurnKind, Arc, AutomaticQueueProgression, CancelToken, ChannelId, ChannelMailboxSnapshot,
    HasPendingSoftQueueResult, HashMap, HydratePendingQueueResult, Intervention,
    MailboxEnqueueOutcome, MailboxTakeNextSoftOutcome, MessageId,
    PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER, ProviderKind, SharedData, UserId,
    VALVE_CLEARED_DISPATCH_MARKER_GRACE, apply_queue_exit_feedback, automatic_queue_progression,
    catch_up_missed_messages_for_retry, inflight, load_channel_pending_dispatch_marker,
    mailbox_finish, mailbox_has_blocking_active_turn, mailbox_take_next_automatic_intervention,
    queue_io, queue_persistence_context, schedule_deferred_idle_queue_kickoff,
    schedule_deferred_idle_queue_kickoff_immediate, should_trigger_catch_up_retry,
    take_catch_up_retry_checkpoint_after_queue_drain, task_supervisor, tui_direct_pending_start,
    turn_bridge,
};

pub(super) async fn mailbox_snapshot(
    shared: &SharedData,
    channel_id: ChannelId,
) -> ChannelMailboxSnapshot {
    match shared.mailbox_peek(channel_id) {
        Some(handle) => handle.snapshot().await,
        None => ChannelMailboxSnapshot::no_actor(channel_id),
    }
}

pub(super) fn infer_inflight_tmux_session_for_channel(channel_id: ChannelId) -> Option<String> {
    [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Qwen,
    ]
    .into_iter()
    .find_map(|provider| {
        inflight::load_inflight_state(&provider, channel_id.get())
            .and_then(|state| state.tmux_session_name)
    })
}

#[cfg(unix)]
pub(crate) async fn record_turn_stop_tombstone(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    reason: &str,
) {
    tmux::record_recent_turn_stop(channel_id, tmux_session_name, reason).await;
}

#[cfg(not(unix))]
pub(crate) async fn record_turn_stop_tombstone(
    _channel_id: ChannelId,
    _tmux_session_name: Option<&str>,
    _reason: &str,
) {
}

pub(super) fn cleanup_retry_inflight_blocks_idle_kickoff(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> bool {
    let Some(state) = inflight::load_inflight_state(provider, channel_id.get()) else {
        return false;
    };
    let Some(current_msg_id) = inflight::opt_message_id(state.current_msg_id) else {
        return false;
    };

    shared
        .ui
        .placeholder_cleanup
        .terminal_cleanup_retry_pending(provider, channel_id, current_msg_id)
}

pub(super) fn idle_queue_snapshot_has_pending_or_marker_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    if !snapshot.intervention_queue.is_empty() {
        return true;
    }
    let Some((marker, _)) =
        load_channel_pending_dispatch_marker(provider, &shared.token_hash, channel_id)
    else {
        return false;
    };
    if snapshot
        .recently_valve_cleared_dispatch
        .is_some_and(|(cleared_id, cleared_at)| {
            cleared_id == marker.message_id
                && cleared_at.elapsed() < VALVE_CLEARED_DISPATCH_MARKER_GRACE
        })
    {
        return false;
    }
    match (
        snapshot.pending_user_dispatch,
        snapshot.pending_user_dispatch_since,
    ) {
        (Some(reserved_id), Some(reserved_at)) => {
            reserved_id == marker.message_id
                && !snapshot.pending_user_dispatch_lease_held_by_caller
                && reserved_at.elapsed() >= PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER
        }
        (Some(_), None) => false,
        (None, _) => true,
    }
}

pub(super) fn idle_queue_snapshot_has_kickable_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    // #3167 — a background turn (monitor relay / self-paced TUI loop) holds the
    // slot but must NOT block a queued user intervention. Only a real
    // (non-background) active turn blocks the kickoff. The previous
    // `cancel_token.is_none()` / `active_request_owner.is_none()` /
    // `active_user_message_id.is_none()` checks all proxied "no active turn at
    // all" and so starved a queued user message behind a continuously-cycling
    // background turn.
    let blocked_by_real_turn =
        snapshot.cancel_token.is_some() && !snapshot.active_turn_kind.is_background();
    !blocked_by_real_turn
        && snapshot.recovery_started_at.is_none()
        && idle_queue_snapshot_has_pending_or_marker_backlog(shared, provider, channel_id, snapshot)
        && !cleanup_retry_inflight_blocks_idle_kickoff(shared, provider, channel_id)
        // #3154: while a deferred synthetic turn-start is pending for this
        // channel, the per-channel worker is waiting for the prior turn to
        // finalize before claiming. Do NOT kick normal queued work in the
        // meantime — that would re-introduce the very turn-interleave this fix
        // serializes away.
        && !tui_direct_pending_start::pending_synthetic_start_blocks_idle_kickoff(
            provider.as_str(),
            channel_id.get(),
        )
}

pub(super) async fn idle_queue_channel_has_kickable_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    idle_queue_snapshot_has_kickable_backlog(shared, provider, channel_id, snapshot)
        && !matches!(
            automatic_queue_progression(shared, provider, channel_id, snapshot),
            AutomaticQueueProgression::BlockedByCappedRetries
        )
}

pub(super) async fn mailbox_try_start_turn(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
) -> bool {
    mailbox_try_start_turn_kinded(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        ActiveTurnKind::UserOrAgent,
    )
    .await
}

/// #3167 — kinded variant of [`mailbox_try_start_turn`]. The monitor auto-turn
/// and the self-paced TUI loop claim the slot as `ActiveTurnKind::Background`
/// so a queued external USER intervention is not perpetually deferred behind
/// the continuously-cycling background turn.
pub(super) async fn mailbox_try_start_turn_kinded(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
    turn_kind: ActiveTurnKind,
) -> bool {
    queue_io::mailbox_try_start_turn_kinded_with_feedback(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_message_id,
        turn_kind,
    )
    .await
}

pub(super) fn ensure_cancel_token_bound_from_inflight_state(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    cancel_token: &Arc<CancelToken>,
    reason: &str,
) -> bool {
    let Some(tmux_session_name) = state.tmux_session_name.as_deref() else {
        tracing::error!(target: "agentdesk::services::discord",
            "cancel token rebind failed: provider={} channel_id={} reason={} error=inflight_missing_tmux_session",
            provider.as_str(),
            state.channel_id,
            reason
        );
        return false;
    };

    turn_bridge::bind_cancel_token_tmux_runtime(provider, cancel_token, tmux_session_name, reason);
    true
}

pub(super) async fn mailbox_enqueue_intervention(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    intervention: Intervention,
) -> MailboxEnqueueOutcome {
    queue_io::mailbox_enqueue_observed_intervention(
        shared,
        provider,
        channel_id,
        intervention,
        None,
    )
    .await
}

pub(super) async fn mailbox_has_pending_soft_queue(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> HasPendingSoftQueueResult {
    let result = shared
        .mailbox(channel_id)
        .has_pending_soft_queue(queue_persistence_context(shared, provider, channel_id))
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    result
}

pub(super) fn maybe_schedule_catch_up_retry_after_queue_drain(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    queue_len_after: usize,
) -> bool {
    if !should_trigger_catch_up_retry(queue_len_after) {
        return false;
    }

    // Phase 5.2 of intake-node-routing (issue #2009): catch-up retry runs
    // on whatever node hosts the channel; on standby workers it falls back
    // to a token-built REST `Arc<Http>` so retries still fire even
    // without a gateway runtime.
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return false;
    };

    let Some(retry_state) =
        take_catch_up_retry_checkpoint_after_queue_drain(shared, channel_id, queue_len_after)
    else {
        return false;
    };

    let shared = Arc::clone(shared);
    let provider = provider.clone();
    task_supervisor::spawn_observed("catch_up_retry_after_queue_drain", async move {
        let retry_checkpoints = HashMap::from([(channel_id, retry_state)]);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(target: "agentdesk::services::discord",
            "  [{ts}] 🔁 catch-up: retrying channel {} after queue drained to {} item(s)",
            channel_id,
            queue_len_after
        );
        catch_up_missed_messages_for_retry(&http, &shared, &provider, &retry_checkpoints).await;
        schedule_deferred_idle_queue_kickoff(
            shared,
            provider,
            channel_id,
            "catch-up retry after queue drain",
        );
    });
    true
}

pub(super) async fn idle_queue_take_next_soft_if_ready(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> MailboxTakeNextSoftOutcome {
    let _transition_guard = match shared
        .session_transition_locks
        .lock(channel_id)
        .try_lock_owned()
    {
        Ok(guard) => guard,
        Err(_) => {
            return MailboxTakeNextSoftOutcome::default();
        }
    };

    // #3167 — only a real (non-background) active turn blocks the dequeue. The
    // cleanup-retry guard remains a correctness guard; the hosted-TUI busy-pane
    // re-scrape gate was removed in #4048 S3 because finalize completion is now
    // the drain authority.
    if mailbox_has_blocking_active_turn(shared, channel_id).await
        || cleanup_retry_inflight_blocks_idle_kickoff(shared, provider, channel_id)
    {
        return MailboxTakeNextSoftOutcome::default();
    }

    // #3167 — the blocking gate above passed, but a *background* turn (monitor
    // relay / self-paced TUI loop) may still hold the slot. Dequeuing now would
    // race the background turn for the single active-turn slot. Instead, cancel
    // the background turn's token and re-kick: the background turn's own
    // identity-guarded finalizer releases the slot, and the immediate re-kick
    // retries against a now-idle mailbox and dequeues the user intervention.
    //
    // RACE-SAFETY: the user turn only ever claims the slot through the normal
    // actor-serialized `mailbox_try_start_turn` AFTER the background turn fully
    // releases — never two concurrent real turns. Cancelling the monitor/loop
    // turn loses no terminal output: the watcher relays output independently of
    // the mailbox slot.
    //
    // #3167 BLOCKER-1 — the kind check and the cancel are performed as a SINGLE
    // atomic, kind-guarded actor step. The previous code read
    // `active_turn_kind()` and THEN sent a separate unguarded
    // `cancel_active_turn_with_reason()`; between the two the background turn
    // could finalize and a real user turn start, and the unguarded cancel would
    // abort the freshly-started real turn. `cancel_active_background_turn_if_current`
    // returns `true` ONLY when it performs a NEW cancel. We re-kick exactly once
    // on that NEW cancel to drain the superseded slot once the background
    // finalizer releases it.
    //
    // CRITICAL (no hot-loop): when the background token is ALREADY cancelling,
    // the call returns `false` (no-op) — NOT `true`. If it returned `true` here,
    // every re-kick would re-observe the same already-cancelled slot (finalizer
    // not done yet), reply `true`, and spawn yet another immediate re-kick: a
    // livelock. On `false` we spawn NO new re-kick and fall through to the
    // normal dequeue/await path below; the deferred-retry cadence (queue_io.rs,
    // ~2s) waits for the finalizer to release the slot. `false` also covers an
    // idle slot (fall through to dequeue) or a real turn holding it (the
    // blocking gate above would already have returned).
    if shared
        .mailbox(channel_id)
        .cancel_active_background_turn_if_current()
        .await
    {
        schedule_deferred_idle_queue_kickoff_immediate(
            shared.clone(),
            provider.clone(),
            channel_id,
            "background_supersede_drain",
        );
        return MailboxTakeNextSoftOutcome::default();
    }

    mailbox_take_next_automatic_intervention(shared, provider, channel_id).await
}

pub(in crate::services::discord) async fn mailbox_abandon_pending_dispatch(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
) -> bool {
    shared
        .mailbox(channel_id)
        .abandon_pending_dispatch(
            user_message_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await
}

pub(super) async fn mailbox_clear_pending_dispatch_reservation(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
) -> bool {
    shared
        .mailbox(channel_id)
        .clear_pending_dispatch_reservation(
            user_message_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await
}

/// #3864: in-actor merge of SIGTERM-restored disk queue items into the live
/// mailbox queue. Replaces the out-of-actor snapshot→build→`replace_queue`
/// read-modify-write the startup restore path used, which silently lost any
/// live reconcile-window `Enqueue` landing between its snapshot and its
/// replace. The actor reads, dedups, front-inserts and persists in one
/// serialized step (cf. `mailbox_hydrate_pending_queue_from_disk`, #1683).
pub(super) async fn mailbox_merge_restored_queue_items(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    items: Vec<Intervention>,
) -> HydratePendingQueueResult {
    let persistence = queue_persistence_context(shared, provider, channel_id);
    mailbox_finish::restitution(shared, channel_id, |h| {
        let (items, p) = (items.clone(), persistence.clone());
        async move { h.merge_restored_queue_items_or_refused(items, p).await }
    })
    .await
}

pub(super) async fn mailbox_merge_restored_dispatch_marker(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    marker: Intervention,
    restored_override: Option<ChannelId>,
) -> HydratePendingQueueResult {
    let persistence = queue_persistence_context(shared, provider, channel_id);
    mailbox_finish::restitution(shared, channel_id, |h| {
        let (marker, p) = (marker.clone(), persistence.clone());
        async move {
            h.merge_restored_dispatch_marker_or_refused(marker, restored_override, p)
                .await
        }
    })
    .await
}

/// Read and merge disk items inside the actor so stale snapshots cannot reintroduce dequeued work.
pub(super) async fn mailbox_hydrate_pending_queue_from_disk(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Result<
    HydratePendingQueueResult,
    crate::services::turn_orchestrator::registry_purge::MailboxRefusal,
> {
    let persistence = queue_persistence_context(shared, provider, channel_id);
    mailbox_finish::try_restitution(shared, channel_id, |h| {
        let p = persistence.clone();
        async move { h.hydrate_pending_queue_from_disk_or_refused(p).await }
    })
    .await
}

pub(super) async fn mailbox_restart_drain_all(
    shared: &SharedData,
    provider: &ProviderKind,
) -> crate::services::turn_orchestrator::RestartDrainAllResult {
    let result = shared
        .mailboxes
        .restart_drain_all(
            provider,
            &shared.token_hash,
            &shared.dispatch.role_overrides,
        )
        .await;
    for failure in &result.persistence_errors {
        tracing::error!(target: "agentdesk::services::discord",
            provider = provider.as_str(),
            channel_id = failure.channel_id.get(),
            error = %failure.error,
            "restart drain failed durable pending-queue persistence for mailbox"
        );
    }
    result
}

pub(super) async fn mailbox_queue_snapshots(
    shared: &SharedData,
) -> HashMap<ChannelId, Vec<Intervention>> {
    shared
        .mailboxes
        .snapshot_all()
        .await
        .into_iter()
        .filter_map(|(channel_id, snapshot)| {
            if snapshot.intervention_queue.is_empty() {
                None
            } else {
                Some((channel_id, snapshot.intervention_queue))
            }
        })
        .collect()
}
