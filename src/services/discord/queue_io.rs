use super::*;

/// #2044 F3: RAII guard that ensures `deferred_hook_backlog` is
/// decremented even if the spawned future panics inside
/// `kickoff_idle_queues` (which awaits multiple IO calls and may
/// unwind). The previous code used a plain `fetch_sub` at the end of
/// the spawned future, so any panic between the matching `fetch_add`
/// and the trailing decrement permanently leaked the counter — which
/// the shutdown drain loop and operator dashboards both rely on for
/// "is the deferred backlog empty yet?" decisions.
struct DeferredHookBacklogGuard {
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IdleQueueBackstopRearm {
    backlog_units: usize,
}

const DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
const DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
const IDLE_QUEUE_BACKSTOP_WARN_TARGET: &str = "agentdesk::discord::idle_queue_backstop";

#[cfg(test)]
static IDLE_QUEUE_BACKSTOP_FIRES_FOR_TESTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

tokio::task_local! {
    static SUPPRESS_POST_ENQUEUE_IDLE_QUEUE_KICK: bool;
}

#[cfg(test)]
type IdleQueueKickHookForTests = std::sync::Arc<
    dyn Fn(
            Arc<SharedData>,
            ProviderKind,
            ChannelId,
            &'static str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<IdleQueueKickoffChannelOutcome>> + Send>,
        > + Send
        + Sync,
>;

#[cfg(test)]
static IDLE_QUEUE_KICK_HOOK_FOR_TESTS: std::sync::Mutex<Option<IdleQueueKickHookForTests>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
struct IdleQueueKickHookResetForTests;

#[cfg(test)]
impl Drop for IdleQueueKickHookResetForTests {
    fn drop(&mut self) {
        *IDLE_QUEUE_KICK_HOOK_FOR_TESTS
            .lock()
            .expect("idle queue kick hook lock") = None;
    }
}

#[cfg(test)]
fn set_idle_queue_kick_hook_for_tests(
    hook: IdleQueueKickHookForTests,
) -> IdleQueueKickHookResetForTests {
    *IDLE_QUEUE_KICK_HOOK_FOR_TESTS
        .lock()
        .expect("idle queue kick hook lock") = Some(hook);
    IdleQueueKickHookResetForTests
}

#[cfg(test)]
async fn idle_queue_kick_hook_outcome_for_tests(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) -> Option<IdleQueueKickoffChannelOutcome> {
    let hook = IDLE_QUEUE_KICK_HOOK_FOR_TESTS
        .lock()
        .expect("idle queue kick hook lock")
        .clone();
    hook?(shared, provider, channel_id, reason).await
}

pub(in crate::services::discord) async fn mailbox_cancel_queued_primary_message(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Option<Intervention> {
    let result: CancelQueuedMessageResult = shared
        .mailbox(channel_id)
        .cancel_queued_primary_message(
            message_id,
            queue_persistence_context(shared, provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    result.removed
}

pub(super) async fn with_post_enqueue_idle_queue_kick_suppressed<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    SUPPRESS_POST_ENQUEUE_IDLE_QUEUE_KICK
        .scope(true, future)
        .await
}

fn post_enqueue_idle_queue_kick_suppressed() -> bool {
    SUPPRESS_POST_ENQUEUE_IDLE_QUEUE_KICK
        .try_with(|suppressed| *suppressed)
        .unwrap_or(false)
}

fn race_loss_requeue_snapshot_has_active_holder(snapshot: &ChannelMailboxSnapshot) -> bool {
    snapshot.cancel_token.is_some()
        || snapshot.active_request_owner.is_some()
        || snapshot.active_user_message_id.is_some()
}

fn race_loss_requeue_snapshot_has_idle_kickable_backlog(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    !race_loss_requeue_snapshot_has_active_holder(snapshot)
        && idle_queue_snapshot_has_kickable_backlog(shared, provider, channel_id, snapshot)
}

pub(super) fn schedule_race_loss_requeue_post_enqueue_idle_recheck(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
) {
    super::task_supervisor::spawn_observed("race_loss_requeue_idle_recheck", async move {
        let snapshot = super::mailbox_snapshot(&shared, channel_id).await;
        if !race_loss_requeue_snapshot_has_idle_kickable_backlog(
            &shared, &provider, channel_id, &snapshot,
        ) {
            tracing::debug!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                active_holder = race_loss_requeue_snapshot_has_active_holder(&snapshot),
                queue_len = snapshot.intervention_queue.len(),
                "Deferred drain: race-loss requeue post-enqueue recheck found no idle kickable backlog"
            );
            return;
        }

        let outcome = kick_idle_queue_channel_if_context_available(
            &shared,
            &provider,
            channel_id,
            "race_loss_requeue_idle_recheck",
        )
        .await;
        arm_event_backstop_after_no_start_if_queue_nonempty(
            &shared,
            &provider,
            channel_id,
            outcome,
            "race_loss_requeue_idle_recheck",
        )
        .await;
    });
}

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
fn deferred_idle_queue_initial_presleep(immediate_once: bool) -> std::time::Duration {
    if immediate_once {
        DeferredIdleQueueKickoffProfile::ImmediateOnce.initial_presleep()
    } else {
        DeferredIdleQueueKickoffProfile::Normal.initial_presleep()
    }
}

fn idle_queue_backstop_backlog_units(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> usize {
    if idle_queue_snapshot_has_raw_rearm_backlog(shared, provider, channel_id, snapshot) {
        snapshot.intervention_queue.len().max(1)
    } else {
        0
    }
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
        self.release();
    }
}

impl DeferredHookBacklogGuard {
    fn release(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.shared
            .restart
            .deferred_hook_channels
            .remove(&self.channel_id);
        self.shared
            .restart
            .deferred_hook_backlog
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        self.active = false;
        true
    }
}

pub(super) fn schedule_deferred_idle_queue_kickoff(
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
pub(super) fn schedule_deferred_idle_queue_kickoff_immediate(
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

pub(super) fn schedule_post_enqueue_idle_queue_kick(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
) {
    if post_enqueue_idle_queue_kick_suppressed() {
        tracing::debug!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            "Deferred drain: suppressed post-enqueue idle snapshot kick for race-loss requeue"
        );
        return;
    }

    // #4048 S3 enqueue-then-check closes the lost-wakeup window that remains
    // after subscribe-then-snapshot on the completion listener: a turn can
    // publish/release before this enqueue is durable, so the event listener's
    // snapshot legitimately sees an empty queue. Once persistence succeeds, the
    // spawned task mirrors the listener by taking a fresh mailbox snapshot and
    // kicking immediately when no real active turn owns the channel. Spawning
    // keeps the dispatch future acyclic: the kick path can re-enter
    // `handle_text_message`, whose race-loss branch can enqueue again. The drain
    // is idempotent: actor-serialized dequeue plus the foreground guard prevent
    // double-starts when older race-loss compensation also schedules a kick.
    super::task_supervisor::spawn_observed("post_enqueue_idle_queue_kick", async move {
        let snapshot = super::mailbox_snapshot(&shared, channel_id).await;
        if idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot) {
            let outcome = kick_idle_queue_channel_if_context_available(
                &shared,
                &provider,
                channel_id,
                "post_enqueue_idle_snapshot",
            )
            .await;
            arm_event_backstop_after_no_start_if_queue_nonempty(
                &shared,
                &provider,
                channel_id,
                outcome,
                "post_enqueue_idle_snapshot",
            )
            .await;
        }
    });
}

pub(super) async fn mailbox_try_start_turn_kinded_with_feedback(
    shared: &SharedData,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_message_id: MessageId,
    turn_kind: ActiveTurnKind,
) -> bool {
    let result = shared
        .mailbox(channel_id)
        .try_start_turn_kinded_with_persistence(
            cancel_token,
            request_owner,
            user_message_id,
            turn_kind,
            queue_persistence_context(shared, &shared.provider, channel_id),
        )
        .await;
    apply_queue_exit_feedback(shared, channel_id, &result.queue_exit_events).await;
    if let Some(error) = result.persistence_error.as_ref() {
        tracing::error!(
            provider = shared.provider.as_str(),
            channel_id = channel_id.get(),
            user_message_id = user_message_id.get(),
            turn_kind = ?turn_kind,
            error = %error,
            "mailbox try-start failed durable active-source queue purge"
        );
    }
    result.started
}

pub(super) async fn kick_idle_queue_channel_if_context_available(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) -> IdleQueueKickoffChannelOutcome {
    #[cfg(test)]
    if let Some(outcome) =
        idle_queue_kick_hook_outcome_for_tests(shared.clone(), provider.clone(), channel_id, reason)
            .await
    {
        return outcome;
    }

    let (Some(ctx), Some(tok)) = (
        shared.http.cached_serenity_ctx.get(),
        shared.http.cached_bot_token.get(),
    ) else {
        tracing::debug!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            reason,
            "Deferred drain: cached Discord context/token unavailable; preserving queued work for the slow backstop"
        );
        return IdleQueueKickoffChannelOutcome::default();
    };

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🚀 Deferred drain: one-shot kick for channel {} ({reason})",
        channel_id
    );
    super::kickoff_idle_queue_channel(ctx, shared, tok, provider, channel_id).await
}

fn idle_queue_snapshot_blocked_by_real_turn(snapshot: &ChannelMailboxSnapshot) -> bool {
    snapshot.cancel_token.is_some() && !snapshot.active_turn_kind.is_background()
}

pub(super) async fn arm_event_backstop_after_no_start_if_queue_nonempty(
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
pub(super) async fn arm_slow_idle_queue_backstop_if_queue_nonempty(
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

fn emit_idle_queue_backstop_warn(
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
) -> Option<IdleQueueBackstopRearm> {
    tokio::time::sleep(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
    let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    let backlog_units =
        idle_queue_backstop_backlog_units(shared.as_ref(), provider, channel_id, &snapshot);
    if backlog_units == 0 {
        return None;
    }

    emit_idle_queue_backstop_warn(
        provider,
        Some(channel_id),
        reason,
        backlog_units,
        "channel_backstop",
    );
    let _outcome =
        kick_idle_queue_channel_if_context_available(shared, provider, channel_id, reason).await;

    // #4270 — decide the re-arm from the ACTUAL post-kick mailbox state, not
    // from the kick's `started` flag. A kickoff can report `started == true`
    // without a real turn owning the slot: the pre-claim readiness gate (#4270 A)
    // re-preserves a still-busy hosted-TUI follow-up and returns `Ok`, so the
    // kickoff reports `started` while the message is merely back in the queue.
    // The previous `if outcome.started { return None; }` short-circuit then
    // dropped this backstop (and the gate's own re-arm had coalesced onto this
    // very still-registered task), stranding the follow-up with no fail-open net
    // until the watcher-idle edge — a #4247-class lost-wakeup. Re-checking the
    // real state below is strictly more precise: a genuinely started turn now
    // owns the slot (`blocked_by_real_turn`) and still suppresses the successor,
    // while a defer/no-start that leaves un-drained backlog re-arms as intended.
    let snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    let backlog_units =
        idle_queue_backstop_backlog_units(shared.as_ref(), provider, channel_id, &snapshot);
    if backlog_units == 0 || idle_queue_snapshot_blocked_by_real_turn(&snapshot) {
        return None;
    }

    Some(IdleQueueBackstopRearm { backlog_units })
}

fn schedule_single_slow_idle_queue_backstop(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    backlog_units: usize,
) -> bool {
    match shared.restart.deferred_hook_channels.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(_) => {
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
            entry.insert(Arc::new(tokio::sync::Notify::new()));
        }
    };
    shared
        .restart
        .deferred_hook_backlog
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    super::task_supervisor::spawn_observed("event_idle_queue_backstop", async move {
        let mut backlog_guard = DeferredHookBacklogGuard {
            shared: shared.clone(),
            channel_id,
            active: true,
        };
        let rearm =
            run_single_slow_idle_queue_backstop(&shared, &provider, channel_id, reason).await;
        backlog_guard.release();
        if let Some(rearm) = rearm {
            schedule_single_slow_idle_queue_backstop(
                shared,
                provider,
                channel_id,
                reason,
                rearm.backlog_units,
            );
        }
    });
    true
}

async fn reconcile_all_ready_idle_queues(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    reason: &'static str,
) -> usize {
    let (Some(ctx), Some(tok)) = (
        shared.http.cached_serenity_ctx.get(),
        shared.http.cached_bot_token.get(),
    ) else {
        tracing::debug!(
            provider = provider.as_str(),
            reason,
            "Idle queue completion listener: cached Discord context/token unavailable; full reconcile deferred to slow backstop"
        );
        return 0;
    };

    tracing::debug!(
        provider = provider.as_str(),
        reason,
        "Idle queue completion listener: reconciling all queued channels from mailbox snapshots"
    );
    super::kickoff_idle_queues(ctx, shared, tok, provider).await
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
        loop {
            match rx.recv().await {
                Ok(event) => {
                    tracing::debug!(
                        target: "agentdesk::discord::turn_completion_events",
                        provider = provider.as_str(),
                        channel_id = event.channel_id.get(),
                        "turn completion event received; kicking idle queue channel"
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
fn idle_queue_backstop_fires_for_tests() -> usize {
    IDLE_QUEUE_BACKSTOP_FIRES_FOR_TESTS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
fn idle_queue_backstop_delay_for_tests() -> std::time::Duration {
    DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY
}

fn schedule_deferred_idle_queue_kickoff_inner(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    profile: DeferredIdleQueueKickoffProfile,
) {
    match shared.restart.deferred_hook_channels.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(entry) => {
            if profile.wakes_existing_task() {
                entry.get().notify_one();
            }
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
            entry.insert(Arc::new(tokio::sync::Notify::new()));
        }
    };
    shared
        .restart
        .deferred_hook_backlog
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    super::task_supervisor::spawn_observed("deferred_idle_queue_kickoff", async move {
        // #2044 F3: bind the decrement to a Drop guard so it fires on
        // panic-unwind as well as on normal return.
        let mut backlog_guard = DeferredHookBacklogGuard {
            shared: shared.clone(),
            channel_id,
            active: true,
        };

        let initial_presleep = profile.initial_presleep();
        if !initial_presleep.is_zero() {
            tokio::time::sleep(initial_presleep).await;
        }

        let _ =
            kick_idle_queue_channel_if_context_available(&shared, &provider, channel_id, reason)
                .await;

        let rearm =
            run_single_slow_idle_queue_backstop(&shared, &provider, channel_id, reason).await;
        backlog_guard.release();
        if let Some(rearm) = rearm {
            schedule_single_slow_idle_queue_backstop(
                shared,
                provider,
                channel_id,
                reason,
                rearm.backlog_units,
            );
        }
    });
}

#[cfg(test)]
mod presleep_tests {
    use super::*;

    struct EnvReset(Option<std::ffi::OsString>);

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn pending_record(
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> super::super::tui_direct_pending_start::TuiDirectPendingStart {
        super::super::tui_direct_pending_start::TuiDirectPendingStart {
            provider: provider.as_str().to_string(),
            channel_id: channel_id.get(),
            tmux_session_name: format!("tmux-{}", channel_id.get()),
            prompt_text: "/loop tick".to_string(),
            anchor_message_id: 9_333_300,
            lease_relay_owner: "bridge_adapter".to_string(),
            lease_runtime_kind: Some("claude_tui".to_string()),
            lease_turn_id: None,
            lease_session_key: None,
            generation: 0,
            created_at_ms: 0,
            observed_at_ms: 0,
            state: super::super::tui_direct_pending_start::PendingStartState::Waiting,
            attempt_count: super::super::tui_direct_pending_start::PENDING_START_MAX_CLAIM_ATTEMPTS,
        }
    }

    fn user_intervention(id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(id),
            author_is_bot: false,
            message_id: MessageId::new(id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    async fn yield_backstop_tasks() {
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    fn cleanup_retry_inflight_state(
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> super::super::inflight::InflightTurnState {
        super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            None,
            4_048_210,
            4_048_211,
            message_id.get(),
            "cleanup retry gate".to_string(),
            Some("cleanup-retry-session".to_string()),
            Some(format!("tmux-cleanup-retry-{}", channel_id.get())),
            Some(format!("/tmp/cleanup-retry-{}.jsonl", channel_id.get())),
            None,
            0,
        )
    }

    fn record_terminal_cleanup_outcome(
        shared: &SharedData,
        provider: ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
        outcome: super::super::placeholder_cleanup::PlaceholderCleanupOutcome,
    ) {
        shared.ui.placeholder_cleanup.record(
            super::super::placeholder_cleanup::PlaceholderCleanupRecord {
                provider,
                channel_id,
                message_id,
                tmux_session_name: Some(format!("tmux-cleanup-retry-{}", channel_id.get())),
                operation:
                    super::super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteTerminal,
                outcome,
                source: "queue_io_test",
            },
        );
    }

    /// #3005: the finalize-completed immediate path must skip the 2s
    /// INITIAL_DELAY pre-sleep on the first attempt, while every other caller
    /// keeps the full delay (restart-window spin guard). The INITIAL_DELAY
    /// constant itself must remain 2s — only the immediate flag bypasses it.
    #[test]
    fn immediate_once_skips_initial_presleep() {
        assert_eq!(
            deferred_idle_queue_initial_presleep(true),
            std::time::Duration::ZERO,
            "finalize-completed immediate path must not pre-sleep"
        );
        assert_eq!(
            deferred_idle_queue_initial_presleep(false),
            DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY,
            "non-immediate callers keep the full INITIAL_DELAY"
        );
        // Guard against silently lowering the shared constant (issue rule).
        assert_eq!(
            DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY,
            std::time::Duration::from_secs(2)
        );
    }

    #[test]
    fn single_backstop_profile_is_the_only_slow_cycle() {
        assert_eq!(
            idle_queue_backstop_delay_for_tests(),
            std::time::Duration::from_secs(60),
            "the deferred path keeps exactly one slow-cycle backstop"
        );
    }

    #[test]
    fn marker_only_backlog_counts_as_backstop_work() {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_024_283);
        let snapshot = ChannelMailboxSnapshot {
            pending_user_dispatch: Some(MessageId::new(4_024_284)),
            ..ChannelMailboxSnapshot::default()
        };
        assert_eq!(
            idle_queue_backstop_backlog_units(&shared, &provider, channel_id, &snapshot),
            1
        );
        assert_eq!(
            idle_queue_backstop_backlog_units(
                &shared,
                &provider,
                channel_id,
                &ChannelMailboxSnapshot::default(),
            ),
            0
        );
    }

    #[test]
    fn transient_gate_backlog_counts_for_backstop() {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_024_285);
        let snapshot = ChannelMailboxSnapshot {
            intervention_queue: vec![user_intervention(4_024_286, "queued under recovery gate")],
            recovery_started_at: Some(Instant::now()),
            ..ChannelMailboxSnapshot::default()
        };

        assert!(
            !idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot),
            "recovery gate still blocks the actual dequeue attempt"
        );
        assert_eq!(
            idle_queue_backstop_backlog_units(&shared, &provider, channel_id, &snapshot),
            1,
            "recovery gate must not suppress the slow backstop decision"
        );
    }

    #[test]
    fn backstop_warn_metric_counter_exists_for_soak() {
        let before = idle_queue_backstop_fires_for_tests();
        emit_idle_queue_backstop_warn(
            &ProviderKind::Claude,
            Some(ChannelId::new(4_024_289)),
            "metric-test",
            1,
            "unit_test",
        );
        assert!(
            idle_queue_backstop_fires_for_tests() > before,
            "warn target has a test-visible counter for soak assertions"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn deferred_kickoff_coalesces_per_channel_while_task_is_live() {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_024_280);

        schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            "coalesce-test",
        );
        schedule_deferred_idle_queue_kickoff(shared.clone(), provider, channel_id, "coalesce-test");

        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "duplicate deferred kicks for one channel must share one live task"
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "the live per-channel guard must remain registered while the task is sleeping"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn coalesced_immediate_kick_notifies_existing_slow_task() {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_024_281);
        let notify = Arc::new(tokio::sync::Notify::new());
        shared
            .restart
            .deferred_hook_channels
            .insert(channel_id, notify.clone());

        schedule_deferred_idle_queue_kickoff_immediate(
            shared,
            provider,
            channel_id,
            "coalesced-immediate-test",
        );

        tokio::time::timeout(std::time::Duration::from_millis(1), notify.notified())
            .await
            .expect("immediate coalesce should wake the existing task");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completion_event_empty_queue_does_not_arm_backstop() {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_048_200);

        spawn_turn_completion_idle_queue_listener(shared.clone(), provider);
        super::super::turn_completion_events::publish_turn_completion_event(
            shared.as_ref(),
            super::super::turn_completion_events::TurnCompletionEvent::new(channel_id),
        );
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "completion-event drain path arms no backstop when the channel queue is empty"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_kick_no_start_with_nonempty_queue_arms_single_backstop() {
        let _root = scoped_runtime_root();
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_048_201);
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(4_048_202, "queued after no-start event")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        spawn_turn_completion_idle_queue_listener(shared.clone(), provider);
        super::super::turn_completion_events::publish_turn_completion_event(
            shared.as_ref(),
            super::super::turn_completion_events::TurnCompletionEvent::new(channel_id),
        );
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "event kick that cannot start and leaves a non-empty queue must arm the single slow backstop"
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "the armed backstop is channel-scoped and coalescible"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_kick_started_does_not_arm_backstop_even_if_queue_snapshot_remains() {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_048_203);
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(4_048_204, "queued but start won")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        let armed = arm_event_backstop_after_no_start_if_queue_nonempty(
            &shared,
            &provider,
            channel_id,
            IdleQueueKickoffChannelOutcome { started: true },
            "started-event-test",
        )
        .await;

        assert!(
            !armed,
            "a started event kick owns progress and needs no slow backstop"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "started event path must not arm the single slow backstop"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn backstop_rearms_after_cleanup_retry_gate_then_next_fire_starts_queue() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_048_210);
        let cleanup_msg = MessageId::new(4_048_211);
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(4_048_212, "queued behind cleanup retry")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;
        let inflight = cleanup_retry_inflight_state(&provider, channel_id, cleanup_msg);
        super::super::inflight::save_inflight_state(&inflight).expect("save inflight gate");
        record_terminal_cleanup_outcome(
            &shared,
            provider.clone(),
            channel_id,
            cleanup_msg,
            super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed("http 500"),
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(
            !idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot),
            "the failed terminal-cleanup retry record blocks the actual kickoff"
        );

        let kick_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = kick_calls.clone();
        let _hook = set_idle_queue_kick_hook_for_tests(std::sync::Arc::new(
            move |shared, provider, channel, _reason| {
                let hook_calls = hook_calls.clone();
                Box::pin(async move {
                    if channel != channel_id {
                        return None;
                    }
                    let call = hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let snapshot = mailbox_snapshot(&shared, channel).await;
                    if call == 0 {
                        assert!(
                            !idle_queue_snapshot_has_kickable_backlog(
                                &shared, &provider, channel, &snapshot,
                            ),
                            "the first fire sees the cleanup retry gate and cannot start"
                        );
                        return Some(IdleQueueKickoffChannelOutcome { started: false });
                    }

                    assert!(
                        idle_queue_snapshot_has_kickable_backlog(
                            &shared, &provider, channel, &snapshot,
                        ),
                        "the successor fire runs after the cleanup gate clears"
                    );
                    shared
                        .mailbox(channel)
                        .replace_queue(
                            Vec::new(),
                            queue_persistence_context(&shared, &provider, channel),
                        )
                        .await;
                    let started = mailbox_try_start_turn(
                        &shared,
                        channel,
                        std::sync::Arc::new(CancelToken::new()),
                        UserId::new(4_048_213),
                        MessageId::new(4_048_212),
                    )
                    .await;
                    Some(IdleQueueKickoffChannelOutcome { started })
                })
            },
        ));
        let before_warns = idle_queue_backstop_fires_for_tests();

        let armed = arm_event_backstop_after_no_start_if_queue_nonempty(
            &shared,
            &provider,
            channel_id,
            IdleQueueKickoffChannelOutcome { started: false },
            "cleanup-retry-gate-test",
        )
        .await;
        assert!(armed, "the event no-start path arms the first backstop");

        yield_backstop_tasks().await;
        tokio::time::advance(idle_queue_backstop_delay_for_tests()).await;
        yield_backstop_tasks().await;

        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the first backstop fire attempts exactly one kick"
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "release-before-rearm must leave a successor backstop registered"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the successor backstop replaces the fired guard instead of coalescing away"
        );

        record_terminal_cleanup_outcome(
            &shared,
            provider.clone(),
            channel_id,
            cleanup_msg,
            super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(
            idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot),
            "committing the cleanup record clears the transient retry gate without publishing a wake edge"
        );

        tokio::time::advance(idle_queue_backstop_delay_for_tests()).await;
        yield_backstop_tasks().await;

        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the successor backstop fires and kicks the channel"
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(
            snapshot.cancel_token.is_some(),
            "the successor kick starts the queued foreground work"
        );
        assert!(
            snapshot.intervention_queue.is_empty(),
            "the queued work is consumed by the successor kick"
        );
        assert!(
            !shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "a successful successor kick does not arm another backstop"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "no backstop guard remains after the successor starts work"
        );
        assert!(
            idle_queue_backstop_fires_for_tests() >= before_warns + 2,
            "each backstop fire emits the slow-path warn"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn backstop_no_rearm_when_fire_observes_real_foreground_turn() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_048_220);
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(4_048_221, "queued behind real turn")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        let kick_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = kick_calls.clone();
        let _hook = set_idle_queue_kick_hook_for_tests(std::sync::Arc::new(
            move |_shared, _provider, channel, _reason| {
                let hook_calls = hook_calls.clone();
                Box::pin(async move {
                    if channel != channel_id {
                        return None;
                    }
                    hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Some(IdleQueueKickoffChannelOutcome { started: false })
                })
            },
        ));

        assert!(
            arm_event_backstop_after_no_start_if_queue_nonempty(
                &shared,
                &provider,
                channel_id,
                IdleQueueKickoffChannelOutcome { started: false },
                "real-turn-no-rearm-test",
            )
            .await
        );

        yield_backstop_tasks().await;
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                std::sync::Arc::new(CancelToken::new()),
                UserId::new(4_048_222),
                MessageId::new(4_048_222),
            )
            .await,
            "a real foreground turn becomes the future wake edge before the backstop fires"
        );

        tokio::time::advance(idle_queue_backstop_delay_for_tests()).await;
        yield_backstop_tasks().await;

        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the fired backstop still emits and attempts its single kick"
        );
        assert!(
            !shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "a real foreground active turn suppresses the successor backstop"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "no successor guard remains while a real foreground turn owns the wake edge"
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(snapshot.intervention_queue.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_after_finalize_kicks_immediately_from_generic_helper() {
        let _root = scoped_runtime_root();
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_048_205);
        let live_msg = MessageId::new(4_048_206);
        let token = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn(&shared, channel_id, token, UserId::new(1), live_msg).await,
            "steering window starts with a live turn observed before enqueue"
        );
        let _ = mailbox_finish_turn(&shared, &provider, channel_id).await;

        let enqueue = mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            user_intervention(4_048_207, "steer after completion event already fired"),
        )
        .await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(enqueue.enqueued, "the steering intervention is persisted");
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a no-context unit kick still leaves the single slow backstop armed for the queued channel"
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "generic enqueue helper must kick immediately enough to arm the channel backstop without advancing time"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn race_loss_requeue_does_not_rekick_until_holding_turn_completes_4078() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_078_200);
        let holder_msg = MessageId::new(4_078_201);
        let queued_msg = MessageId::new(4_078_202);

        spawn_turn_completion_idle_queue_listener(shared.clone(), provider.clone());
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(
            mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                Arc::new(CancelToken::new()),
                UserId::new(4_078_201),
                holder_msg,
                ActiveTurnKind::Background,
            )
            .await,
            "seed the mailbox-holding background turn"
        );

        let kick_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = kick_calls.clone();
        let _hook = set_idle_queue_kick_hook_for_tests(Arc::new(
            move |shared, provider, channel, reason| {
                let hook_calls = hook_calls.clone();
                Box::pin(async move {
                    if channel != channel_id {
                        return None;
                    }
                    let call = hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let taken = shared
                        .mailbox(channel)
                        .take_next_soft(queue_persistence_context(&shared, &provider, channel))
                        .await;
                    let intervention = taken
                        .intervention
                        .expect("kick should dequeue the queued message");
                    assert_eq!(intervention.message_id, queued_msg);

                    if call == 0 {
                        assert_eq!(reason, "post_enqueue_idle_snapshot");
                        let started = mailbox_try_start_turn(
                            &shared,
                            channel,
                            Arc::new(CancelToken::new()),
                            intervention.author_id,
                            intervention.message_id,
                        )
                        .await;
                        assert!(
                            !started,
                            "the mailbox holder has not completed, so the dequeued turn loses the race"
                        );
                        let requeued_msg = intervention.message_id;
                        let requeue = with_post_enqueue_idle_queue_kick_suppressed(
                            mailbox_enqueue_intervention(&shared, &provider, channel, intervention),
                        )
                        .await;
                        assert!(requeue.enqueued, "race-loss path requeues the message");
                        mailbox_abandon_pending_dispatch(&shared, &provider, channel, requeued_msg)
                            .await;
                        schedule_race_loss_requeue_post_enqueue_idle_recheck(
                            shared.clone(),
                            provider.clone(),
                            channel,
                        );
                        return Some(IdleQueueKickoffChannelOutcome { started });
                    }

                    assert_eq!(reason, "turn_completion_event");
                    let started = mailbox_try_start_turn(
                        &shared,
                        channel,
                        Arc::new(CancelToken::new()),
                        intervention.author_id,
                        intervention.message_id,
                    )
                    .await;
                    assert!(
                        started,
                        "the completion-event kick must start the requeued message after the holder finishes"
                    );
                    Some(IdleQueueKickoffChannelOutcome { started })
                })
            },
        ));

        let enqueue = mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            user_intervention(queued_msg.get(), "queued behind background holder"),
        )
        .await;
        assert!(enqueue.enqueued);
        yield_backstop_tasks().await;
        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fresh enqueue gets the initial post-enqueue kick"
        );

        tokio::time::advance(
            DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY + std::time::Duration::from_secs(1),
        )
        .await;
        yield_backstop_tasks().await;
        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "race-loss requeue must not schedule another immediate/deferred kick while the holder is live"
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(snapshot.active_user_message_id, Some(holder_msg));
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(snapshot.intervention_queue[0].message_id, queued_msg);

        let finish = mailbox_finish_turn(&shared, &provider, channel_id).await;
        assert!(
            finish.has_pending,
            "the holder completion observes the requeued message and publishes the wake edge"
        );
        yield_backstop_tasks().await;
        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "only the holder completion event produces the second kick"
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(snapshot.active_user_message_id, Some(queued_msg));
        assert!(
            snapshot.intervention_queue.is_empty(),
            "completion-event kick consumes the requeued message"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn hosted_tui_followup_busy_retry_release_then_enqueue_redelivers_active_message_4107() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_107_300);
        let user_msg = MessageId::new(4_107_301);
        let owner = UserId::new(4_107_302);

        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                Arc::new(CancelToken::new()),
                owner,
                user_msg,
            )
            .await,
            "hosted-TUI pre-submit path starts as the active message first"
        );

        let finish = mailbox_finish_turn(&shared, &provider, channel_id).await;
        assert!(
            !finish.has_pending,
            "release happens before retry enqueue, so the active guard cannot refuse self-requeue"
        );

        let kick_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = kick_calls.clone();
        let _hook = set_idle_queue_kick_hook_for_tests(Arc::new(
            move |shared, provider, channel, reason| {
                let hook_calls = hook_calls.clone();
                Box::pin(async move {
                    if channel != channel_id {
                        return None;
                    }
                    hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(reason, "post_enqueue_idle_snapshot");
                    let taken = shared
                        .mailbox(channel)
                        .take_next_soft(queue_persistence_context(&shared, &provider, channel))
                        .await;
                    let intervention = taken
                        .intervention
                        .expect("busy retry should remain queued for redispatch");
                    assert_eq!(intervention.message_id, user_msg);
                    let started = mailbox_try_start_turn(
                        &shared,
                        channel,
                        Arc::new(CancelToken::new()),
                        intervention.author_id,
                        intervention.message_id,
                    )
                    .await;
                    assert!(started, "post-enqueue kick must redispatch the retry");
                    Some(IdleQueueKickoffChannelOutcome { started })
                })
            },
        ));

        let enqueue = mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            user_intervention(user_msg.get(), "retry after hosted TUI busy pre-submit"),
        )
        .await;
        assert!(enqueue.enqueued);
        assert_eq!(
            mailbox_snapshot(&shared, channel_id)
                .await
                .intervention_queue
                .len(),
            1,
            "same message id must be durably queued after release"
        );

        yield_backstop_tasks().await;
        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "retry enqueue must schedule one redispatch kick"
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.active_user_message_id, Some(user_msg));
        assert!(
            snapshot.intervention_queue.is_empty(),
            "redispatch consumes the retry queue entry"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn race_loss_requeue_idle_recheck_kicks_after_missed_completion_4078() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_078_300);
        let holder_msg = MessageId::new(4_078_301);
        let queued_msg = MessageId::new(4_078_302);

        spawn_turn_completion_idle_queue_listener(shared.clone(), provider.clone());
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                Arc::new(CancelToken::new()),
                UserId::new(4_078_301),
                holder_msg,
            )
            .await,
            "seed the mailbox holder before the missed-completion window"
        );

        let finish = mailbox_finish_turn(&shared, &provider, channel_id).await;
        assert!(
            !finish.has_pending,
            "holder completion must see the queue empty before the race-loss requeue lands"
        );
        yield_backstop_tasks().await;
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "empty completion-event drain must arm no backstop before the requeue"
        );

        let kick_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = kick_calls.clone();
        let _hook = set_idle_queue_kick_hook_for_tests(Arc::new(
            move |shared, provider, channel, reason| {
                let hook_calls = hook_calls.clone();
                Box::pin(async move {
                    if channel != channel_id {
                        return None;
                    }
                    hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(reason, "race_loss_requeue_idle_recheck");
                    let taken = shared
                        .mailbox(channel)
                        .take_next_soft(queue_persistence_context(&shared, &provider, channel))
                        .await;
                    let intervention = taken
                        .intervention
                        .expect("idle recheck kick should dequeue the missed race-loss requeue");
                    assert_eq!(intervention.message_id, queued_msg);
                    let started = mailbox_try_start_turn(
                        &shared,
                        channel,
                        Arc::new(CancelToken::new()),
                        intervention.author_id,
                        intervention.message_id,
                    )
                    .await;
                    assert!(
                        started,
                        "the channel is already idle, so the recheck kick must start the queued message"
                    );
                    Some(IdleQueueKickoffChannelOutcome { started })
                })
            },
        ));

        let requeue = with_post_enqueue_idle_queue_kick_suppressed(mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            user_intervention(queued_msg.get(), "missed completion race-loss requeue"),
        ))
        .await;
        assert!(requeue.enqueued, "race-loss requeue lands durably");
        mailbox_abandon_pending_dispatch(&shared, &provider, channel_id, queued_msg).await;
        schedule_race_loss_requeue_post_enqueue_idle_recheck(
            shared.clone(),
            provider.clone(),
            channel_id,
        );
        yield_backstop_tasks().await;

        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the post-enqueue idle recheck schedules exactly one missed-completion kick"
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(snapshot.active_user_message_id, Some(queued_msg));
        assert!(
            snapshot.intervention_queue.is_empty(),
            "the recheck kick consumes and starts the requeued message"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a successful recheck kick must not leave a slow backstop armed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lagged_completion_receiver_warns_and_full_reconciles() {
        let before = idle_queue_backstop_fires_for_tests();
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        spawn_turn_completion_idle_queue_listener(shared.clone(), provider);

        for offset in 0..=super::super::turn_completion_events::TURN_COMPLETION_EVENT_BUS_CAPACITY {
            super::super::turn_completion_events::publish_turn_completion_event(
                shared.as_ref(),
                super::super::turn_completion_events::TurnCompletionEvent::new(ChannelId::new(
                    4_048_300 + offset as u64,
                )),
            );
        }
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(
            idle_queue_backstop_fires_for_tests() > before,
            "lagged broadcast receive must emit the warn target before full snapshot reconcile"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dropped_completion_event_slow_backstop_rearms_if_no_start_leaves_queue() {
        let _root = scoped_runtime_root();
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_024_290);
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(4_024_291, "event dropped")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        schedule_deferred_idle_queue_kickoff_immediate(
            shared.clone(),
            provider,
            channel_id,
            "dropped-event-test",
        );

        tokio::task::yield_now().await;
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "the single slow backstop is armed before the 60s delay"
        );
        tokio::time::advance(idle_queue_backstop_delay_for_tests()).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "a fired backstop that still cannot start queued work must arm a successor"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the successor backstop replaces the fired guard after release"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn normal_event_path_drains_before_backstop_records_zero_fires() {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_024_292);
        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(4_024_293, "event drains")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        schedule_deferred_idle_queue_kickoff_immediate(
            shared.clone(),
            provider.clone(),
            channel_id,
            "normal-event-test",
        );
        tokio::task::yield_now().await;
        shared
            .mailbox(channel_id)
            .replace_queue(
                Vec::new(),
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        tokio::time::advance(idle_queue_backstop_delay_for_tests()).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(
            !shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "normal completion-event drain should leave no channel backstop guard after the delay"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "normal completion-event drain should leave no live backstop task"
        );
    }

    // Sync test + explicit block_on: the std-mutex test-env guards live only in
    // this sync scope and never span an await, so no await_holding_lock allow is
    // needed (#3034 ratchet stays frozen at its baseline).
    #[test]
    fn abandoned_presence_auto_reconcile_releases_idle_queue_gate_but_keeps_durable_record() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let tmp = tempfile::tempdir().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        super::super::tui_direct_pending_start::reset_present_for_tests();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        rt.block_on(async {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(3_333_300);
            shared
                .mailbox(channel_id)
                .replace_queue(
                    vec![user_intervention(3_333_301, "queued after abandoned claim")],
                    queue_persistence_context(&shared, &provider, channel_id),
                )
                .await;

            let record = pending_record(&provider, channel_id);
            super::super::tui_direct_pending_start::persist(&record).unwrap();
            assert!(
                super::super::tui_direct_pending_start::pending_synthetic_start_present(
                    provider.as_str(),
                    channel_id.get(),
                ),
                "the retained pending-start presence starts out blocking the idle queue"
            );
            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert!(
                idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot),
                "#3691: abandoned retry-exhausted presence must self-clear so the queued item is kickable"
            );
            assert!(
                !super::super::tui_direct_pending_start::pending_synthetic_start_present(
                    provider.as_str(),
                    channel_id.get(),
                ),
                "#3691: only the in-memory presence is cleared"
            );
            assert!(
                super::super::tui_direct_pending_start::pending_synthetic_start_abandoned(
                    provider.as_str(),
                    channel_id.get(),
                ),
                "the durable record remains retained for restart retry; only the in-memory gate is cleared"
            );
        });

        super::super::tui_direct_pending_start::reset_present_for_tests();
    }

    /// #4270 — env-lock + temp runtime root held inside a struct so the guard
    /// is not a local binding across `.await` (keeps the repo-wide
    /// `await_holding_lock` allow ratchet flat; same pattern as
    /// `queue_marker.rs::ScopedRuntimeRoot`).
    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("temp runtime root");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                temp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        ScopedRuntimeRoot {
            _lock: lock,
            _temp: temp,
            prev,
        }
    }

    /// #4270 pin — sustained-busy convergence over the PRODUCTION defer
    /// sequence (`take_next_soft` promote → `mailbox_requeue_intervention_front`
    /// + slow-backstop arm, exactly what the `dispatch_queued_turn` promote gate
    /// runs): repeated cycles converge to a SINGLE queue entry and a SINGLE
    /// coalesced slow backstop (no accumulation / oscillation), and never fire a
    /// fast kick.
    #[tokio::test(flavor = "current_thread")]
    async fn promote_defer_requeue_converges_no_oscillation_across_cycles_4270() {
        let _root = scoped_runtime_root();

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_270_200);
        let user_msg = MessageId::new(4_270_201);

        let kick_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = kick_calls.clone();
        let _hook = set_idle_queue_kick_hook_for_tests(Arc::new(
            move |_shared, _provider, channel, _reason| {
                let hook_calls = hook_calls.clone();
                Box::pin(async move {
                    if channel == channel_id {
                        hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    None
                })
            },
        ));

        with_post_enqueue_idle_queue_kick_suppressed(mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            user_intervention(user_msg.get(), "sustained busy follow-up"),
        ))
        .await;

        for cycle in 0..3 {
            let taken = shared
                .mailbox(channel_id)
                .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
                .await;
            let intervention = taken
                .intervention
                .unwrap_or_else(|| panic!("cycle {cycle}: the follow-up must still be promotable"));
            mailbox_requeue_intervention_front(&shared, &provider, channel_id, intervention).await;
            arm_slow_idle_queue_backstop_if_queue_nonempty(
                &shared,
                &provider,
                channel_id,
                "hosted_tui_busy_pre_drain_defer",
            )
            .await;
            yield_backstop_tasks().await;

            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(
                snapshot.intervention_queue.len(),
                1,
                "cycle {cycle}: exactly one queue entry (never accumulates duplicates)"
            );
            assert_eq!(
                snapshot.pending_user_dispatch, None,
                "cycle {cycle}: the front-requeue consumes the stale dispatch reservation"
            );
            assert!(
                snapshot.cancel_token.is_none(),
                "cycle {cycle}: the mailbox is never claimed by the still-busy follow-up"
            );
            assert_eq!(
                shared
                    .restart
                    .deferred_hook_backlog
                    .load(std::sync::atomic::Ordering::Relaxed),
                1,
                "cycle {cycle}: the slow backstop coalesces to a single armed net"
            );
        }
        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no fast kick across any busy cycle"
        );
    }

    /// #4270 pin #2 — the busy-defer edge-trigger helper arms ONLY the slow
    /// (60s) fail-open backstop for a non-empty queue and fires NO fast kickoff;
    /// the fast wakeup is delegated to the watcher-idle re-drain. This is the
    /// arm that both `release_mailbox_after_hosted_tui_busy_pre_submit`
    /// (post-claim busy defer, turn_start.rs) and the `dispatch_queued_turn`
    /// promote gate call instead of the fixed-delay kickoff.
    #[tokio::test(flavor = "current_thread")]
    async fn busy_defer_arms_slow_backstop_not_fast_kickoff_4270() {
        let _root = scoped_runtime_root();

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_270_300);
        let user_msg = MessageId::new(4_270_301);

        let kick_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = kick_calls.clone();
        let _hook = set_idle_queue_kick_hook_for_tests(Arc::new(
            move |_shared, _provider, channel, _reason| {
                let hook_calls = hook_calls.clone();
                Box::pin(async move {
                    if channel == channel_id {
                        hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    None
                })
            },
        ));

        // Suppress the setup enqueue's OWN post-enqueue kick so this test
        // exercises `arm_slow_idle_queue_backstop_if_queue_nonempty` in
        // isolation (otherwise that kick would arm the backstop first and this
        // direct call would merely coalesce).
        with_post_enqueue_idle_queue_kick_suppressed(mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            user_intervention(user_msg.get(), "busy-defer backlog"),
        ))
        .await;

        let armed = arm_slow_idle_queue_backstop_if_queue_nonempty(
            &shared,
            &provider,
            channel_id,
            "hosted_tui_busy_pre_submit_pending",
        )
        .await;
        assert!(armed, "a non-empty queue arms the slow backstop");
        yield_backstop_tasks().await;
        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the slow-backstop arm must not fire an immediate fast kickoff"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "exactly one slow (60s) backstop is armed"
        );

        // A second call for the same channel coalesces onto the single armed
        // backstop (no accumulation) rather than arming a duplicate.
        let armed_again = arm_slow_idle_queue_backstop_if_queue_nonempty(
            &shared,
            &provider,
            channel_id,
            "hosted_tui_busy_pre_submit_pending",
        )
        .await;
        assert!(
            !armed_again,
            "a second arm coalesces onto the single channel-scoped backstop"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the slow backstop stays coalesced to a single armed net"
        );
    }

    /// #4270 pin #4 — TUI Idle transition: once the promote gate has re-preserved
    /// the follow-up at the queue front (production defer sequence), the
    /// watcher-idle drain (soft-take → claim) dispatches it EXACTLY once and
    /// empties the queue.
    #[tokio::test(flavor = "current_thread")]
    async fn idle_drain_after_promote_defer_dispatches_exactly_once_4270() {
        let _root = scoped_runtime_root();

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_270_400);
        let user_msg = MessageId::new(4_270_401);
        let owner = UserId::new(4_270_402);

        with_post_enqueue_idle_queue_kick_suppressed(mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            user_intervention(user_msg.get(), "deferred until TUI idle"),
        ))
        .await;
        let taken = shared
            .mailbox(channel_id)
            .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
            .await;
        let intervention = taken.intervention.expect("promote once");
        mailbox_requeue_intervention_front(&shared, &provider, channel_id, intervention).await;

        // Watcher-idle drain: the TUI reached Idle, so the drain soft-takes and
        // claims. It must start exactly once and leave the queue empty.
        let drained = shared
            .mailbox(channel_id)
            .take_next_soft(queue_persistence_context(&shared, &provider, channel_id))
            .await
            .intervention
            .expect("idle drain re-promotes the preserved follow-up");
        let started = mailbox_try_start_turn(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            owner,
            drained.message_id,
        )
        .await;
        assert!(started, "on TUI Idle the follow-up claims exactly once");
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.active_user_message_id, Some(user_msg));
        assert!(
            snapshot.intervention_queue.is_empty(),
            "the idle dispatch consumes the single queued follow-up"
        );
    }

    /// #4270 fail-open — the slow (60s) backstop RE-ARMS itself when its own kick
    /// reports `started` yet leaves un-drained backlog with no real turn owning
    /// the slot. That is exactly the promote gate's steady state (the kickoff
    /// defers pre-dequeue and reports no-start, or the dispatch gate re-preserves
    /// and returns `Ok`), and the gate's own inline re-arm coalesces onto THIS
    /// still-registered task. The net must therefore persist across busy cycles
    /// instead of dying after a single fire — otherwise the follow-up strands
    /// with no fail-open net until the watcher-idle edge (a #4247-class
    /// lost-wakeup).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn slow_backstop_rearms_when_kick_defers_without_claiming_4270() {
        let _root = scoped_runtime_root();

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_270_500);

        shared
            .mailbox(channel_id)
            .replace_queue(
                vec![user_intervention(4_270_501, "still-busy follow-up")],
                queue_persistence_context(&shared, &provider, channel_id),
            )
            .await;

        // Emulate the promote-gate deferral: the kick reports `started` but
        // never claims and leaves the follow-up queued (no active turn).
        let kick_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = kick_calls.clone();
        let _hook = set_idle_queue_kick_hook_for_tests(Arc::new(
            move |_shared, _provider, channel, _reason| {
                let hook_calls = hook_calls.clone();
                Box::pin(async move {
                    if channel != channel_id {
                        return None;
                    }
                    hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Some(IdleQueueKickoffChannelOutcome { started: true })
                })
            },
        ));

        assert!(
            arm_slow_idle_queue_backstop_if_queue_nonempty(
                &shared,
                &provider,
                channel_id,
                "hosted_tui_busy_pre_submit_pending",
            )
            .await
        );

        // Let the spawned backstop task reach its 60s sleep before advancing.
        yield_backstop_tasks().await;

        // First fire (60s): the kick defers again ⇒ the successor net must re-arm.
        tokio::time::advance(idle_queue_backstop_delay_for_tests()).await;
        yield_backstop_tasks().await;
        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the backstop fired once"
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "a started-but-deferred kick that leaves backlog must re-arm the fail-open backstop (no strand)"
        );

        // Second fire: still deferring ⇒ the net persists across successive cycles.
        tokio::time::advance(idle_queue_backstop_delay_for_tests()).await;
        yield_backstop_tasks().await;
        assert_eq!(
            kick_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the re-armed backstop fires again on the next cycle"
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "the fail-open net persists across successive busy cycles"
        );
    }
}
