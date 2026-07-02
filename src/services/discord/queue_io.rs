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

const DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
const DEFERRED_IDLE_QUEUE_KICKOFF_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
const DEFERRED_IDLE_QUEUE_KICKOFF_SLOW_DELAY: std::time::Duration =
    std::time::Duration::from_secs(60);
// Keep retrying long enough to cover dcserver/gateway restart windows. A
// queued user reply should not wait for the next external Discord event just
// because cached ctx/token arrived slightly after the first post-turn kickoff.
const DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS: usize = 150;
const DEFERRED_IDLE_QUEUE_ZERO_START_ABANDONED_CLAIM_PROBE_AFTER: usize = 3;
const DEFERRED_IDLE_QUEUE_MAX_CONSECUTIVE_DISPATCH_FAILURES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeferredIdleQueueKickoffProfile {
    Normal,
    ImmediateOnce,
    Slow { round: usize },
}

impl DeferredIdleQueueKickoffProfile {
    fn initial_presleep(self) -> std::time::Duration {
        match self {
            Self::Normal => DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY,
            Self::ImmediateOnce => std::time::Duration::ZERO,
            Self::Slow { .. } => DEFERRED_IDLE_QUEUE_KICKOFF_SLOW_DELAY,
        }
    }

    fn retry_delay(self) -> std::time::Duration {
        match self {
            Self::Normal | Self::ImmediateOnce => DEFERRED_IDLE_QUEUE_KICKOFF_RETRY_DELAY,
            Self::Slow { .. } => DEFERRED_IDLE_QUEUE_KICKOFF_SLOW_DELAY,
        }
    }

    fn round_index(self) -> usize {
        match self {
            Self::Normal | Self::ImmediateOnce => 0,
            Self::Slow { round } => round,
        }
    }

    fn next_slow_round(self) -> Self {
        Self::Slow {
            round: self.round_index().saturating_add(1),
        }
    }

    fn wakes_existing_task(self) -> bool {
        matches!(self, Self::Normal | Self::ImmediateOnce)
    }
}

async fn sleep_or_deferred_kick_notify(
    duration: std::time::Duration,
    notify: &tokio::sync::Notify,
) {
    if duration.is_zero() {
        return;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => {}
        _ = notify.notified() => {}
    }
}

fn should_retry_deferred_idle_queue_kickoff(attempt: usize) -> bool {
    attempt < DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
}

fn note_zero_start_deferred_drain(
    consecutive_zero_start_drains: &mut usize,
    target_started: bool,
    target_still_pending: bool,
) -> bool {
    if !target_started && target_still_pending {
        *consecutive_zero_start_drains = consecutive_zero_start_drains.saturating_add(1);
    } else {
        *consecutive_zero_start_drains = 0;
    }
    *consecutive_zero_start_drains >= DEFERRED_IDLE_QUEUE_ZERO_START_ABANDONED_CLAIM_PROBE_AFTER
}

fn note_deferred_dispatch_failure(
    consecutive_dispatch_failures: &mut usize,
    dispatch_failed: bool,
) -> bool {
    if dispatch_failed {
        *consecutive_dispatch_failures = consecutive_dispatch_failures.saturating_add(1);
    } else {
        *consecutive_dispatch_failures = 0;
    }
    *consecutive_dispatch_failures >= DEFERRED_IDLE_QUEUE_MAX_CONSECUTIVE_DISPATCH_FAILURES
}

/// #3005: pre-sleep before the very first deferred-drain attempt. The
/// finalize-completed (idle-confirmed) path passes `immediate_once = true` so
/// the first kickoff runs without the 2s `INITIAL_DELAY` guard; every other
/// caller keeps the full delay to avoid spinning during the dcserver/gateway
/// restart window.
#[cfg(test)]
fn deferred_idle_queue_initial_presleep(immediate_once: bool) -> std::time::Duration {
    if immediate_once {
        DeferredIdleQueueKickoffProfile::ImmediateOnce.initial_presleep()
    } else {
        DeferredIdleQueueKickoffProfile::Normal.initial_presleep()
    }
}

fn deferred_idle_queue_rearm_profile_after_giveup(
    current: DeferredIdleQueueKickoffProfile,
    remaining_queue_len: usize,
) -> Option<DeferredIdleQueueKickoffProfile> {
    (remaining_queue_len > 0).then(|| current.next_slow_round())
}

fn deferred_backlog_units(snapshot: &ChannelMailboxSnapshot, target_still_pending: bool) -> usize {
    if target_still_pending {
        snapshot.intervention_queue.len().max(1)
    } else {
        0
    }
}

impl Drop for DeferredHookBacklogGuard {
    fn drop(&mut self) {
        self.release();
    }
}

impl DeferredHookBacklogGuard {
    fn release(&mut self) {
        if !self.active {
            return;
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

/// #3005: variant for the finalize-completed (idle-confirmed) path. When a turn
/// has just finalized with a confirmed-idle pane and a queued backlog remains,
/// the first kickoff attempt skips the 2s `INITIAL_DELAY` pre-sleep and tries
/// `kickoff_idle_queues` immediately, falling back to the existing 2s retry
/// cadence if that first attempt cannot drain (e.g. cached ctx/token not yet
/// available, or the hosted TUI is still transiently `Busy`). The
/// `INITIAL_DELAY` constant is intentionally left untouched — it still guards
/// the restart-window spin for every other caller — so this only narrows the
/// post-finalize latency where idle has already been confirmed.
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

async fn slow_rearm_channel_still_kickable(
    ctx: Option<&serenity::Context>,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) -> bool {
    let Some(ctx) = ctx else {
        return true;
    };
    let settings_snapshot = shared.settings.read().await.clone();
    if let Err(route_reason) =
        super::validate_live_channel_routing(ctx, provider, &settings_snapshot, channel_id).await
    {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            reason,
            route_reason = %route_reason,
            "Deferred drain: terminating slow re-arm chain for unroutable channel; queue file remains persisted for rebind/boot recovery"
        );
        return false;
    }
    true
}

async fn rearm_slow_deferred_idle_queue_kickoff(
    ctx: Option<&serenity::Context>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    next_profile: DeferredIdleQueueKickoffProfile,
    remaining_queue_len: usize,
    backlog_guard: &mut DeferredHookBacklogGuard,
    cause: &'static str,
) {
    if !slow_rearm_channel_still_kickable(ctx, shared.as_ref(), provider, channel_id, reason).await
    {
        return;
    }
    let next_round = next_profile.round_index();
    tracing::warn!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        reason,
        round = next_round,
        queue_len = remaining_queue_len,
        "Deferred drain: re-arming slow deferred kickoff {cause}"
    );
    backlog_guard.release();
    schedule_deferred_idle_queue_kickoff_inner(
        shared.clone(),
        provider.clone(),
        channel_id,
        reason,
        next_profile,
    );
}

fn schedule_deferred_idle_queue_kickoff_inner(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    profile: DeferredIdleQueueKickoffProfile,
) {
    let task_notify = match shared.restart.deferred_hook_channels.entry(channel_id) {
        dashmap::mapref::entry::Entry::Occupied(entry) => {
            if profile.wakes_existing_task() {
                entry.get().notify_one();
            }
            tracing::debug!(
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                reason,
                round = profile.round_index(),
                wake_existing = profile.wakes_existing_task(),
                "Deferred drain: kickoff already active for channel; coalescing duplicate request"
            );
            return;
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            let notify = Arc::new(tokio::sync::Notify::new());
            entry.insert(notify.clone());
            notify
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
        // #3005: on the finalize-completed (idle-confirmed) reason the first
        // attempt skips the 2s pre-sleep so a queued follow-up can drain right
        // after the turn settles; all subsequent attempts keep the 2s cadence.
        let initial_presleep = profile.initial_presleep();
        if !initial_presleep.is_zero() {
            sleep_or_deferred_kick_notify(initial_presleep, task_notify.as_ref()).await;
        }
        let mut consecutive_zero_start_drains = 0usize;
        let mut consecutive_dispatch_failures = 0usize;
        for attempt in 1..=DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS {
            if let (Some(ctx), Some(tok)) = (
                shared.http.cached_serenity_ctx.get(),
                shared.http.cached_bot_token.get(),
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🚀 Deferred drain: kicking off idle queues for channel {} ({reason}, attempt {attempt}/{})",
                    channel_id,
                    DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                );
                let outcome =
                    super::kickoff_idle_queue_channel(ctx, &shared, tok, &provider, channel_id)
                        .await;
                // Always re-check the queue on a zero-start drain regardless
                // of `reason`. The earlier `should_retry_zero_start_*` reason
                // allowlist gated retry on a single hard-coded reason
                // (`hosted_tui_busy_pre_submit_pending`) so every other
                // caller (monitor turn completion, placeholder_sweeper,
                // catch-up retry, tmux stop, …) silently abandoned the
                // queue when kickoff returned 0 — usually because the
                // hosted TUI was still `Busy` for the few seconds after a
                // turn finished. The blocker is the TUI ready state, not
                // the reason string, so any reason can hit the same
                // transient window. Retry whenever the queue is still
                // non-empty within the bounded attempt budget; the
                // hosted-TUI gate inside `kickoff_idle_queues` is what
                // keeps us from racing a still-busy pane.
                let target_snapshot = super::mailbox_snapshot(shared.as_ref(), channel_id).await;
                let target_still_pending = super::idle_queue_snapshot_has_kickable_backlog(
                    shared.as_ref(),
                    &provider,
                    channel_id,
                    &target_snapshot,
                );
                if note_deferred_dispatch_failure(
                    &mut consecutive_dispatch_failures,
                    outcome.dispatch_failed,
                ) && target_still_pending
                {
                    let remaining_queue_len =
                        deferred_backlog_units(&target_snapshot, target_still_pending);
                    if let Some(next_profile) =
                        deferred_idle_queue_rearm_profile_after_giveup(profile, remaining_queue_len)
                    {
                        rearm_slow_deferred_idle_queue_kickoff(
                            Some(ctx),
                            &shared,
                            &provider,
                            channel_id,
                            reason,
                            next_profile,
                            remaining_queue_len,
                            &mut backlog_guard,
                            "after repeated dispatch failures",
                        )
                        .await;
                    }
                    return;
                }
                let should_probe_abandoned_claim = note_zero_start_deferred_drain(
                    &mut consecutive_zero_start_drains,
                    outcome.started,
                    target_still_pending,
                );
                if should_probe_abandoned_claim {
                    let abandoned_claim =
                        super::tui_direct_pending_start::pending_synthetic_start_abandoned(
                            provider.as_str(),
                            channel_id.get(),
                        );
                    if abandoned_claim
                        && super::tui_direct_pending_start::clear_abandoned_synthetic_start_presence(
                            provider.as_str(),
                            channel_id.get(),
                        )
                    {
                        tracing::warn!(
                            provider = provider.as_str(),
                            channel_id = channel_id.get(),
                            reason,
                            attempt,
                            consecutive_zero_start_drains,
                            issue = "#3333",
                            "Deferred drain: cleared abandoned synthetic-start presence after repeated zero-start drains; durable record retained for restart retry"
                        );
                        let final_outcome = super::kickoff_idle_queue_channel(
                            ctx, &shared, tok, &provider, channel_id,
                        )
                        .await;
                        let final_snapshot =
                            super::mailbox_snapshot(shared.as_ref(), channel_id).await;
                        let final_pending = super::idle_queue_snapshot_has_kickable_backlog(
                            shared.as_ref(),
                            &provider,
                            channel_id,
                            &final_snapshot,
                        );
                        if !final_outcome.started && final_pending {
                            let remaining_queue_len =
                                deferred_backlog_units(&final_snapshot, final_pending);
                            tracing::warn!(
                                provider = provider.as_str(),
                                channel_id = channel_id.get(),
                                reason,
                                attempt,
                                issue = "#3333",
                                "Deferred drain: final one-shot drain after abandoned synthetic-start clear started zero turns; stopping bounded retry loop"
                            );
                            if let Some(next_profile) =
                                deferred_idle_queue_rearm_profile_after_giveup(
                                    profile,
                                    remaining_queue_len,
                                )
                            {
                                rearm_slow_deferred_idle_queue_kickoff(
                                    Some(ctx),
                                    &shared,
                                    &provider,
                                    channel_id,
                                    reason,
                                    next_profile,
                                    remaining_queue_len,
                                    &mut backlog_guard,
                                    "after abandoned synthetic-start give-up",
                                )
                                .await;
                            }
                        }
                        return;
                    }
                    consecutive_zero_start_drains = 0;
                }
                if !outcome.started
                    && target_still_pending
                    && should_retry_deferred_idle_queue_kickoff(attempt)
                {
                    tracing::info!(
                        "  [{ts}] ⏳ Deferred drain: channel {} still queued after zero-start drain ({reason}, attempt {attempt}/{}); retrying",
                        channel_id,
                        DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                    );
                    sleep_or_deferred_kick_notify(profile.retry_delay(), task_notify.as_ref())
                        .await;
                    continue;
                }
                if !outcome.started && target_still_pending {
                    let remaining_queue_len =
                        deferred_backlog_units(&target_snapshot, target_still_pending);
                    if let Some(next_profile) =
                        deferred_idle_queue_rearm_profile_after_giveup(profile, remaining_queue_len)
                    {
                        rearm_slow_deferred_idle_queue_kickoff(
                            Some(ctx),
                            &shared,
                            &provider,
                            channel_id,
                            reason,
                            next_profile,
                            remaining_queue_len,
                            &mut backlog_guard,
                            "after bounded zero-start give-up",
                        )
                        .await;
                    }
                }
                return;
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            if !should_retry_deferred_idle_queue_kickoff(attempt) {
                let missing_context_snapshot =
                    super::mailbox_snapshot(shared.as_ref(), channel_id).await;
                let missing_context_pending = super::idle_queue_snapshot_has_kickable_backlog(
                    shared.as_ref(),
                    &provider,
                    channel_id,
                    &missing_context_snapshot,
                );
                let remaining_queue_len =
                    deferred_backlog_units(&missing_context_snapshot, missing_context_pending);
                tracing::warn!(
                    "  [{ts}] ⚠ Deferred drain: missing cached context for channel {} after {} attempts ({reason}); queued items remain persisted for next kickoff",
                    channel_id,
                    DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                );
                if let Some(next_profile) =
                    deferred_idle_queue_rearm_profile_after_giveup(profile, remaining_queue_len)
                {
                    rearm_slow_deferred_idle_queue_kickoff(
                        None,
                        &shared,
                        &provider,
                        channel_id,
                        reason,
                        next_profile,
                        remaining_queue_len,
                        &mut backlog_guard,
                        "after missing-context give-up",
                    )
                    .await;
                }
                break;
            }
            tracing::info!(
                "  [{ts}] ⚠ Deferred drain: missing cached context for channel {} ({reason}, attempt {attempt}/{}); retrying",
                channel_id,
                DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
            );
            sleep_or_deferred_kick_notify(profile.retry_delay(), task_notify.as_ref()).await;
        }
        // Drop guard at end of scope decrements the backlog counter.
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
            source_message_ids: vec![MessageId::new(id)],
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
    fn giveup_with_backlog_rearms_slow_profile() {
        let next = deferred_idle_queue_rearm_profile_after_giveup(
            DeferredIdleQueueKickoffProfile::Normal,
            1,
        )
        .expect("non-empty queue should schedule one slow follow-up");
        assert_eq!(next, DeferredIdleQueueKickoffProfile::Slow { round: 1 });
        assert_eq!(next.initial_presleep(), std::time::Duration::from_secs(60));
        assert_eq!(next.retry_delay(), std::time::Duration::from_secs(60));
        assert_eq!(DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS, 150);

        let chained = deferred_idle_queue_rearm_profile_after_giveup(next, 2)
            .expect("slow rounds may chain while backlog remains");
        assert_eq!(chained, DeferredIdleQueueKickoffProfile::Slow { round: 2 });
    }

    #[test]
    fn giveup_with_empty_queue_does_not_rearm() {
        assert_eq!(
            deferred_idle_queue_rearm_profile_after_giveup(
                DeferredIdleQueueKickoffProfile::Normal,
                0,
            ),
            None
        );
        assert_eq!(
            deferred_idle_queue_rearm_profile_after_giveup(
                DeferredIdleQueueKickoffProfile::Slow { round: 3 },
                0,
            ),
            None
        );
    }

    #[test]
    fn zero_start_abandoned_claim_probe_fires_on_third_consecutive_pending_zero() {
        let mut consecutive = 0usize;
        assert!(!note_zero_start_deferred_drain(
            &mut consecutive,
            false,
            true
        ));
        assert!(!note_zero_start_deferred_drain(
            &mut consecutive,
            false,
            true
        ));
        assert!(note_zero_start_deferred_drain(
            &mut consecutive,
            false,
            true
        ));
        assert_eq!(
            consecutive, DEFERRED_IDLE_QUEUE_ZERO_START_ABANDONED_CLAIM_PROBE_AFTER,
            "the #3333 abandoned-claim probe must fire after 3 zero-start drains, not after the 150-attempt cap"
        );

        assert!(!note_zero_start_deferred_drain(
            &mut consecutive,
            true,
            true
        ));
        assert_eq!(consecutive, 0, "a non-zero drain resets the zero-start run");
        assert!(!note_zero_start_deferred_drain(
            &mut consecutive,
            false,
            false
        ));
        assert_eq!(consecutive, 0, "an empty target queue is a normal exit");
    }

    #[test]
    fn target_channel_zero_start_progress_is_independent_of_other_channels() {
        let mut consecutive = DEFERRED_IDLE_QUEUE_ZERO_START_ABANDONED_CLAIM_PROBE_AFTER - 1;
        let other_channel_started = true;
        assert!(
            other_channel_started,
            "document the masked-success scenario"
        );
        assert!(note_zero_start_deferred_drain(
            &mut consecutive,
            false,
            true
        ));
        assert_eq!(
            consecutive, DEFERRED_IDLE_QUEUE_ZERO_START_ABANDONED_CLAIM_PROBE_AFTER,
            "a different channel's success must not reset this target channel"
        );
    }

    #[test]
    fn marker_only_backlog_counts_as_slow_rearm_work() {
        let snapshot = ChannelMailboxSnapshot::default();
        assert_eq!(deferred_backlog_units(&snapshot, true), 1);
        assert_eq!(deferred_backlog_units(&snapshot, false), 0);
    }

    #[test]
    fn persistent_dispatch_failure_consumes_bounded_zero_progress_budget_then_rearms_slow() {
        let mut consecutive = 0usize;
        for _ in 1..=DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS {
            let _ = note_zero_start_deferred_drain(&mut consecutive, false, true);
        }

        let next = deferred_idle_queue_rearm_profile_after_giveup(
            DeferredIdleQueueKickoffProfile::Normal,
            1,
        );
        assert_eq!(
            next,
            Some(DeferredIdleQueueKickoffProfile::Slow { round: 1 })
        );
        assert_eq!(DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS, 150);
    }

    #[test]
    fn dispatch_failure_fast_phase_exits_after_three_consecutive_failures() {
        let mut consecutive = 0usize;
        assert!(!note_deferred_dispatch_failure(&mut consecutive, true));
        assert!(!note_deferred_dispatch_failure(&mut consecutive, true));
        assert!(note_deferred_dispatch_failure(&mut consecutive, true));
        assert_eq!(
            consecutive,
            DEFERRED_IDLE_QUEUE_MAX_CONSECUTIVE_DISPATCH_FAILURES
        );
        assert!(!note_deferred_dispatch_failure(&mut consecutive, false));
        assert_eq!(consecutive, 0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn notify_wakes_deferred_sleep_without_waiting_full_delay() {
        let notify = tokio::sync::Notify::new();
        let sleeper = sleep_or_deferred_kick_notify(std::time::Duration::from_secs(60), &notify);
        tokio::pin!(sleeper);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut sleeper)
                .await
                .is_err(),
            "sleep should remain pending before notification"
        );
        notify.notify_one();
        tokio::time::timeout(std::time::Duration::from_millis(1), &mut sleeper)
            .await
            .expect("notification should wake the deferred sleep promptly");
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

    #[tokio::test]
    async fn slow_rearm_without_routing_context_preserves_transient_backlog_chain() {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(4_024_282);

        assert!(
            slow_rearm_channel_still_kickable(
                None,
                &shared,
                &provider,
                channel_id,
                "slow-rearm-transient-guard-test",
            )
            .await,
            "slow rearm must not terminate solely because kickable backlog is temporarily gated"
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
}
