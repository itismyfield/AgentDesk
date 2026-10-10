use super::*;

/// One task owns this slot until all arm requests have been evaluated.
#[derive(Default)]
pub(in crate::services) struct BackstopSlot {
    pub(in crate::services) wake: tokio::sync::Notify,
    pub(super) pending_request: std::sync::atomic::AtomicBool,
}

impl BackstopSlot {
    pub(in crate::services) fn new() -> Self {
        Self {
            wake: tokio::sync::Notify::new(),
            pending_request: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn request(&self, wake_existing: bool) {
        self.pending_request
            .store(true, std::sync::atomic::Ordering::Release);
        if wake_existing {
            self.wake.notify_one();
        }
    }

    fn consume_requests(&self) {
        self.pending_request
            .swap(false, std::sync::atomic::Ordering::AcqRel);
    }
}
/// Count each live task once, including panic cleanup, while preserving arm requests.
struct DeferredHookBacklogGuard {
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    slot: Arc<BackstopSlot>,
    active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IdleQueueBackstopRearm {
    backlog_units: usize,
}

pub(super) const DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
pub(super) const DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY: std::time::Duration =
    std::time::Duration::from_secs(60);
const IDLE_QUEUE_BACKSTOP_WARN_TARGET: &str = "agentdesk::discord::idle_queue_backstop";

#[cfg(test)]
static IDLE_QUEUE_BACKSTOP_FIRES_FOR_TESTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeferredIdleQueueKickoffProfile {
    Normal,
    ImmediateOnce,
}

impl DeferredIdleQueueKickoffProfile {
    fn initial_presleep(self) -> std::time::Duration {
        match self {
            Self::Normal => DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY,
            Self::ImmediateOnce => std::time::Duration::ZERO,
        }
    }

    fn wakes_existing_task(self) -> bool {
        matches!(self, Self::ImmediateOnce)
    }
}

/// #3005/#4048: pre-sleep before the one non-event deferred-drain attempt.
/// Completion events bypass this helper and kick their channel immediately.
/// Every other caller keeps the 2s delay to avoid restart-window spin.
#[cfg(test)]
pub(super) fn deferred_idle_queue_initial_presleep(immediate_once: bool) -> std::time::Duration {
    if immediate_once {
        DeferredIdleQueueKickoffProfile::ImmediateOnce.initial_presleep()
    } else {
        DeferredIdleQueueKickoffProfile::Normal.initial_presleep()
    }
}

pub(super) fn idle_queue_backstop_backlog_units(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> usize {
    if !idle_queue_snapshot_has_raw_rearm_backlog(shared, provider, channel_id, snapshot) {
        return 0;
    }
    if matches!(
        super::automatic_queue_progression(shared, provider, channel_id, snapshot),
        AutomaticQueueProgression::BlockedByCappedRetries
    ) {
        return 0;
    }
    snapshot.intervention_queue.len().max(1)
}

fn idle_queue_snapshot_has_raw_rearm_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    !snapshot.intervention_queue.is_empty()
        || snapshot.pending_user_dispatch.is_some()
        || load_channel_pending_dispatch_marker(provider, &shared.token_hash, channel_id).is_some()
}

impl Drop for DeferredHookBacklogGuard {
    fn drop(&mut self) {
        if self.active {
            #[cfg(test)]
            cancel_backstop_test_support::set_backstop_waiting(self.channel_id, false);
            self.shared
                .restart
                .deferred_hook_channels
                .remove_if(&self.channel_id, |_, slot| Arc::ptr_eq(slot, &self.slot));
            tracing::warn!(
                channel_id = self.channel_id.get(),
                "Idle queue backstop task dropped before normal slot release"
            );
            self.finish_task();
        }
    }
}

impl DeferredHookBacklogGuard {
    fn finish_task(&mut self) {
        if self.active {
            self.shared
                .restart
                .deferred_hook_backlog
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            self.active = false;
        }
    }

    fn release(&mut self) -> bool {
        if !self.active {
            return true;
        }
        let removed =
            self.shared
                .restart
                .deferred_hook_channels
                .remove_if(&self.channel_id, |_, slot| {
                    Arc::ptr_eq(slot, &self.slot)
                        && !slot
                            .pending_request
                            .load(std::sync::atomic::Ordering::Acquire)
                });
        let own_slot_remains = removed.is_none()
            && self
                .shared
                .restart
                .deferred_hook_channels
                .get(&self.channel_id)
                .is_some_and(|slot| Arc::ptr_eq(slot.value(), &self.slot));
        if own_slot_remains {
            return false;
        }
        self.finish_task();
        true
    }
}

pub(in crate::services::discord) fn schedule_deferred_idle_queue_kickoff(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) {
    schedule_deferred_idle_queue_kickoff_inner(
        shared,
        provider,
        channel_id,
        reason,
        DeferredIdleQueueKickoffProfile::Normal,
    );
}

/// #3005/#4048: variant for already-confirmed non-finalizer paths. Turn
/// completion now bypasses this helper through the completion-event listener;
/// this remains for internal paths that have an independent idle signal.
pub(in crate::services::discord) fn schedule_deferred_idle_queue_kickoff_immediate(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) {
    schedule_deferred_idle_queue_kickoff_inner(
        shared,
        provider,
        channel_id,
        reason,
        DeferredIdleQueueKickoffProfile::ImmediateOnce,
    );
}

fn idle_queue_snapshot_blocked_by_live_turn(snapshot: &ChannelMailboxSnapshot) -> bool {
    !snapshot.active_turn_kind.is_background()
        && snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| !token.cancelled.load(std::sync::atomic::Ordering::Relaxed))
}

fn idle_queue_snapshot_blocked_by_cancelled_anchor(snapshot: &ChannelMailboxSnapshot) -> bool {
    !snapshot.active_turn_kind.is_background()
        && snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| token.cancelled.load(std::sync::atomic::Ordering::Relaxed))
}

async fn release_cancelled_backstop_anchor(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    if matching_residue_owns_anchor(shared, channel_id, snapshot) {
        return false;
    }
    #[cfg(test)]
    cancel_backstop_test_support::pause_before_admit(channel_id).await;
    let Ok(permit) = input_runtime::fence::effect::admit(provider, channel_id.get()) else {
        return false;
    };
    input_runtime::fence::effect::scope(permit.clone(), async {
        let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        if !idle_queue_snapshot_blocked_by_cancelled_anchor(&snapshot)
            || matching_residue_owns_anchor(shared, channel_id, &snapshot)
            || health::legacy_supervision::legacy_retired(
                provider.as_str(),
                channel_id.get(),
                "idle_queue_backstop",
            )
        {
            return false;
        }
        super::zombie_foreground_release::release_zombie_foreground_turn_guarded(
            shared,
            provider,
            channel_id,
            "idle_queue_backstop",
            |observed| {
                !matching_residue_owns_anchor(shared, channel_id, observed)
                    && permit
                        .as_ref()
                        .is_none_or(|permit| permit.validate(provider, channel_id.get()).is_ok())
                    && !health::legacy_supervision::legacy_retired(
                        provider.as_str(),
                        channel_id.get(),
                        "idle_queue_backstop_before_finish",
                    )
            },
        )
        .await
        .released
    })
    .await
}

fn matching_residue_owns_anchor(
    shared: &SharedData,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    shared
        .turn_finalizer
        .guarded_finish_residues()
        .get(&channel_id)
        .is_some_and(|residue| residue.matches_observed_owner(snapshot))
}

pub(in crate::services::discord) async fn arm_event_backstop_after_no_start_if_queue_nonempty(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    outcome: IdleQueueKickoffChannelOutcome,
    reason: &'static str,
) -> bool {
    if outcome.started {
        return false;
    }
    let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    let backlog_units =
        idle_queue_backstop_backlog_units(shared.as_ref(), provider, channel_id, &snapshot);
    if backlog_units == 0 {
        return false;
    }
    schedule_single_slow_idle_queue_backstop(
        shared.clone(),
        provider.clone(),
        channel_id,
        reason,
        backlog_units,
    )
}

/// #4270 — busy-defer edge-trigger net: arm ONLY the slow (60s) fail-open
/// backstop for a channel, WITHOUT the fast 2s deferred kick. Used by (1) the
/// hosted-TUI busy-defer release path
/// (`release_mailbox_after_hosted_tui_busy_pre_submit`) and (2) the live
/// dispatch promote gate (`DiscordGateway::dispatch_queued_turn`), so a
/// still-busy follow-up does not fast-spin the kickoff: the watcher-idle
/// re-drain delivers the fast edge when the TUI reaches Idle, and this backstop
/// is the lost-wakeup net. Thin wrapper over
/// [`arm_event_backstop_after_no_start_if_queue_nonempty`] with a synthetic
/// no-start outcome so the same "arm only when queue is non-empty" guard and
/// single-backstop coalescing apply.
pub(in crate::services::discord) async fn arm_slow_idle_queue_backstop_if_queue_nonempty(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) -> bool {
    arm_event_backstop_after_no_start_if_queue_nonempty(
        shared,
        provider,
        channel_id,
        IdleQueueKickoffChannelOutcome { started: false },
        reason,
    )
    .await
}

pub(super) fn emit_idle_queue_backstop_warn(
    provider: &ProviderKind,
    channel_id: Option<ChannelId>,
    reason: &'static str,
    backlog_units: usize,
    cause: &'static str,
) {
    #[cfg(test)]
    IDLE_QUEUE_BACKSTOP_FIRES_FOR_TESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    tracing::warn!(
        target: IDLE_QUEUE_BACKSTOP_WARN_TARGET,
        provider = provider.as_str(),
        channel_id = channel_id.map(|id| id.get()).unwrap_or(0),
        all_channels = channel_id.is_none(),
        reason,
        backlog_units,
        cause,
        "Idle queue slow backstop fired; the turn-completion event path should normally drain before this"
    );
}

async fn idle_queue_backstop_backlog_units_all(
    shared: &SharedData,
    provider: &ProviderKind,
) -> usize {
    shared
        .mailboxes
        .snapshot_all()
        .await
        .into_iter()
        .map(|(channel_id, snapshot)| {
            idle_queue_backstop_backlog_units(shared, provider, channel_id, &snapshot)
        })
        .sum()
}

async fn run_single_slow_idle_queue_backstop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    slot: &BackstopSlot,
    wait_for_trigger: bool,
) -> Option<IdleQueueBackstopRearm> {
    let timed_out = if wait_for_trigger {
        #[cfg(test)]
        cancel_backstop_test_support::set_backstop_waiting(channel_id, true);
        let timed_out = tokio::select! {
            _ = tokio::time::sleep(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY) => true,
            _ = slot.wake.notified() => false,
        };
        #[cfg(test)]
        cancel_backstop_test_support::set_backstop_waiting(channel_id, false);
        timed_out
    } else {
        false
    };
    // This snapshot evaluates all requests received before it; newer requests remain pending.
    slot.consume_requests();
    let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    let backlog_units =
        idle_queue_backstop_backlog_units(shared.as_ref(), provider, channel_id, &snapshot);
    if backlog_units == 0 || idle_queue_snapshot_blocked_by_live_turn(&snapshot) {
        return None;
    }

    if timed_out {
        emit_idle_queue_backstop_warn(
            provider,
            Some(channel_id),
            reason,
            backlog_units,
            "channel_backstop",
        );
    }
    let may_kick = !idle_queue_snapshot_blocked_by_cancelled_anchor(&snapshot)
        || release_cancelled_backstop_anchor(shared, provider, channel_id, &snapshot).await;
    if may_kick {
        let kickoff =
            kick_idle_queue_channel_if_context_available(shared, provider, channel_id, reason);
        #[cfg(test)]
        let _ = queue_dispatch::kickoff::cancel_backstop_test_support::with_origin(
            "idle_queue_backstop",
            kickoff,
        )
        .await;
        #[cfg(not(test))]
        let _ = kickoff.await;
    }

    // A reported start can still have re-preserved the message at the hosted-TUI gate.
    slot.consume_requests();
    let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    let backlog_units =
        idle_queue_backstop_backlog_units(shared.as_ref(), provider, channel_id, &snapshot);
    if backlog_units == 0 || idle_queue_snapshot_blocked_by_live_turn(&snapshot) {
        return None;
    }
    Some(IdleQueueBackstopRearm { backlog_units })
}

async fn run_slow_idle_queue_backstop_loop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    slot: &Arc<BackstopSlot>,
    backlog_guard: &mut DeferredHookBacklogGuard,
) {
    let mut wait_for_trigger = true;
    loop {
        let rearm = run_single_slow_idle_queue_backstop(
            shared,
            provider,
            channel_id,
            reason,
            slot,
            wait_for_trigger,
        )
        .await;
        #[cfg(test)]
        cancel_backstop_test_support::completed_cycle(channel_id);
        if let Some(rearm) = rearm {
            tracing::debug!(
                channel_id = channel_id.get(),
                backlog_units = rearm.backlog_units,
                "Idle queue slow backstop remains armed"
            );
            wait_for_trigger = true;
            continue;
        }
        #[cfg(test)]
        cancel_backstop_test_support::pause_before_shutdown(channel_id).await;
        if backlog_guard.release() {
            break;
        }
        // A request racing shutdown belongs to this task and needs a fresh evaluation now.
        wait_for_trigger = false;
    }
}

fn schedule_single_slow_idle_queue_backstop(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    backlog_units: usize,
) -> bool {
    let slot = match shared.restart.deferred_hook_channels.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(entry) => {
            entry.get().request(false);
            tracing::debug!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                reason,
                backlog_units,
                "Idle queue slow backstop already active for channel; coalescing event-path no-start"
            );
            return false;
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            let slot = Arc::new(BackstopSlot::new());
            entry.insert(slot.clone());
            slot
        }
    };
    shared
        .restart
        .deferred_hook_backlog
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut backlog_guard = DeferredHookBacklogGuard {
        shared: shared.clone(),
        channel_id,
        slot: slot.clone(),
        active: true,
    };
    super::task_supervisor::spawn_observed("event_idle_queue_backstop", async move {
        run_slow_idle_queue_backstop_loop(
            &shared,
            &provider,
            channel_id,
            reason,
            &slot,
            &mut backlog_guard,
        )
        .await;
    });
    true
}

pub(in crate::services::discord) async fn reconcile_all_ready_idle_queues(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    reason: &'static str,
) -> usize {
    let Some(transport) = QueueTransport::from_runtime(shared) else {
        tracing::debug!(
            provider = provider.as_str(),
            reason,
            "Idle queue completion listener: Discord REST credentials unavailable; full reconcile deferred to slow backstop"
        );
        return 0;
    };

    tracing::debug!(
        provider = provider.as_str(),
        reason,
        "Idle queue completion listener: reconciling all queued channels from mailbox snapshots"
    );
    super::kickoff_idle_queues_with_deps(&transport.intake_deps(shared), provider).await
}

pub(in crate::services::discord) fn spawn_turn_completion_idle_queue_listener(
    shared: Arc<SharedData>,
    provider: ProviderKind,
) {
    // #4048 S3 lost-wakeup ordering: subscribe/register the broadcast receiver
    // synchronously first, then the task's first action is a mailbox snapshot
    // reconcile through `kickoff_idle_queues`. A completion racing this
    // registration is either delivered to `rx` or observed by that snapshot.
    let mut rx = super::turn_completion_events::subscribe_turn_completion_events(shared.as_ref());
    super::task_supervisor::spawn_observed("turn_completion_idle_queue_listener", async move {
        let _ =
            reconcile_all_ready_idle_queues(&shared, &provider, "turn_completion_listener_start")
                .await;
        #[cfg(test)]
        for (channel_id, _) in shared.mailboxes.snapshot_all().await {
            cancel_backstop_test_support::set_listener_ready(channel_id);
        }
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !event.queue_is_eligible() {
                        tracing::debug!(
                            target: "agentdesk::discord::turn_completion_events",
                            provider = provider.as_str(),
                            channel_id = event.channel_id.get(),
                            "mailbox release observed before terminal projection settled; queue kick withheld"
                        );
                        continue;
                    }
                    tracing::debug!(
                        target: "agentdesk::discord::turn_completion_events",
                        provider = provider.as_str(),
                        channel_id = event.channel_id.get(),
                        "queue-eligible completion event received; kicking idle queue channel"
                    );
                    let outcome = kick_idle_queue_channel_if_context_available(
                        &shared,
                        &provider,
                        event.channel_id,
                        "turn_completion_event",
                    )
                    .await;
                    arm_event_backstop_after_no_start_if_queue_nonempty(
                        &shared,
                        &provider,
                        event.channel_id,
                        outcome,
                        "turn_completion_event",
                    )
                    .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    emit_idle_queue_backstop_warn(
                        &provider,
                        None,
                        "turn_completion_event_lagged",
                        skipped as usize,
                        "broadcast_lagged_full_reconcile",
                    );
                    let _ = reconcile_all_ready_idle_queues(
                        &shared,
                        &provider,
                        "turn_completion_event_lagged",
                    )
                    .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::error!(
                        target: IDLE_QUEUE_BACKSTOP_WARN_TARGET,
                        provider = provider.as_str(),
                        "Turn-completion event bus closed; idle queue listener is falling back to the slow reconcile backstop"
                    );
                    loop {
                        tokio::time::sleep(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
                        let backlog_units =
                            idle_queue_backstop_backlog_units_all(shared.as_ref(), &provider).await;
                        if backlog_units > 0 {
                            emit_idle_queue_backstop_warn(
                                &provider,
                                None,
                                "turn_completion_event_bus_closed",
                                backlog_units,
                                "broadcast_closed_full_reconcile",
                            );
                        }
                        let _ = reconcile_all_ready_idle_queues(
                            &shared,
                            &provider,
                            "turn_completion_event_bus_closed",
                        )
                        .await;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
pub(in crate::services::discord) fn idle_queue_backstop_fires_for_tests() -> usize {
    IDLE_QUEUE_BACKSTOP_FIRES_FOR_TESTS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(in crate::services::discord) fn idle_queue_backstop_delay_for_tests() -> std::time::Duration {
    DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY
}

fn schedule_deferred_idle_queue_kickoff_inner(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    profile: DeferredIdleQueueKickoffProfile,
) {
    let slot = match shared.restart.deferred_hook_channels.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(entry) => {
            entry.get().request(profile.wakes_existing_task());
            tracing::debug!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                reason,
                immediate = matches!(profile, DeferredIdleQueueKickoffProfile::ImmediateOnce),
                wake_existing = profile.wakes_existing_task(),
                "Deferred drain: kickoff already active for channel; coalescing duplicate request"
            );
            return;
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            let slot = Arc::new(BackstopSlot::new());
            entry.insert(slot.clone());
            slot
        }
    };
    shared
        .restart
        .deferred_hook_backlog
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut backlog_guard = DeferredHookBacklogGuard {
        shared: shared.clone(),
        channel_id,
        slot: slot.clone(),
        active: true,
    };
    super::task_supervisor::spawn_observed("deferred_idle_queue_kickoff", async move {
        let initial_presleep = profile.initial_presleep();
        if !initial_presleep.is_zero() {
            tokio::select! {
                _ = tokio::time::sleep(initial_presleep) => {}
                _ = slot.wake.notified() => {}
            }
        }

        let _ =
            kick_idle_queue_channel_if_context_available(&shared, &provider, channel_id, reason)
                .await;
        run_slow_idle_queue_backstop_loop(
            &shared,
            &provider,
            channel_id,
            reason,
            &slot,
            &mut backlog_guard,
        )
        .await;
    });
}
