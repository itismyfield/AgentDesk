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
}

const DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
const DEFERRED_IDLE_QUEUE_KICKOFF_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
// Keep retrying long enough to cover dcserver/gateway restart windows. A
// queued user reply should not wait for the next external Discord event just
// because cached ctx/token arrived slightly after the first post-turn kickoff.
const DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS: usize = 150;
const DEFERRED_IDLE_QUEUE_ZERO_START_ABANDONED_CLAIM_PROBE_AFTER: usize = 3;

fn should_retry_deferred_idle_queue_kickoff(attempt: usize) -> bool {
    attempt < DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
}

fn note_zero_start_deferred_drain(
    consecutive_zero_start_drains: &mut usize,
    started: usize,
    target_still_pending: bool,
) -> bool {
    if started == 0 && target_still_pending {
        *consecutive_zero_start_drains = consecutive_zero_start_drains.saturating_add(1);
    } else {
        *consecutive_zero_start_drains = 0;
    }
    *consecutive_zero_start_drains >= DEFERRED_IDLE_QUEUE_ZERO_START_ABANDONED_CLAIM_PROBE_AFTER
}

/// #3005: pre-sleep before the very first deferred-drain attempt. The
/// finalize-completed (idle-confirmed) path passes `immediate_once = true` so
/// the first kickoff runs without the 2s `INITIAL_DELAY` guard; every other
/// caller keeps the full delay to avoid spinning during the dcserver/gateway
/// restart window.
fn deferred_idle_queue_initial_presleep(immediate_once: bool) -> std::time::Duration {
    if immediate_once {
        std::time::Duration::ZERO
    } else {
        DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY
    }
}

impl Drop for DeferredHookBacklogGuard {
    fn drop(&mut self) {
        self.shared
            .deferred_hook_backlog
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(super) fn schedule_deferred_idle_queue_kickoff(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) {
    schedule_deferred_idle_queue_kickoff_inner(shared, provider, channel_id, reason, false);
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
    schedule_deferred_idle_queue_kickoff_inner(shared, provider, channel_id, reason, true);
}

fn schedule_deferred_idle_queue_kickoff_inner(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    immediate_once: bool,
) {
    shared
        .deferred_hook_backlog
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    super::task_supervisor::spawn_observed("deferred_idle_queue_kickoff", async move {
        // #2044 F3: bind the decrement to a Drop guard so it fires on
        // panic-unwind as well as on normal return.
        let _backlog_guard = DeferredHookBacklogGuard {
            shared: shared.clone(),
        };
        // #3005: on the finalize-completed (idle-confirmed) reason the first
        // attempt skips the 2s pre-sleep so a queued follow-up can drain right
        // after the turn settles; all subsequent attempts keep the 2s cadence.
        let initial_presleep = deferred_idle_queue_initial_presleep(immediate_once);
        if !initial_presleep.is_zero() {
            tokio::time::sleep(initial_presleep).await;
        }
        let mut consecutive_zero_start_drains = 0usize;
        for attempt in 1..=DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS {
            if let (Some(ctx), Some(tok)) = (
                shared.cached_serenity_ctx.get(),
                shared.cached_bot_token.get(),
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🚀 Deferred drain: kicking off idle queues for channel {} ({reason}, attempt {attempt}/{})",
                    channel_id,
                    DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                );
                let started = super::kickoff_idle_queues(ctx, &shared, tok, &provider).await;
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
                let target_still_pending = !super::mailbox_snapshot(shared.as_ref(), channel_id)
                    .await
                    .intervention_queue
                    .is_empty();
                let should_probe_abandoned_claim = note_zero_start_deferred_drain(
                    &mut consecutive_zero_start_drains,
                    started,
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
                        let final_started =
                            super::kickoff_idle_queues(ctx, &shared, tok, &provider).await;
                        let final_pending = !super::mailbox_snapshot(shared.as_ref(), channel_id)
                            .await
                            .intervention_queue
                            .is_empty();
                        if final_started == 0 && final_pending {
                            tracing::warn!(
                                provider = provider.as_str(),
                                channel_id = channel_id.get(),
                                reason,
                                attempt,
                                issue = "#3333",
                                "Deferred drain: final one-shot drain after abandoned synthetic-start clear started zero turns; stopping bounded retry loop"
                            );
                        }
                        return;
                    }
                    consecutive_zero_start_drains = 0;
                }
                if started == 0
                    && target_still_pending
                    && should_retry_deferred_idle_queue_kickoff(attempt)
                {
                    tracing::info!(
                        "  [{ts}] ⏳ Deferred drain: channel {} still queued after zero-start drain ({reason}, attempt {attempt}/{}); retrying",
                        channel_id,
                        DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                    );
                    tokio::time::sleep(DEFERRED_IDLE_QUEUE_KICKOFF_RETRY_DELAY).await;
                    continue;
                }
                return;
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            if !should_retry_deferred_idle_queue_kickoff(attempt) {
                tracing::warn!(
                    "  [{ts}] ⚠ Deferred drain: missing cached context for channel {} after {} attempts ({reason}); queued items remain persisted for next kickoff",
                    channel_id,
                    DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                );
                break;
            }
            tracing::info!(
                "  [{ts}] ⚠ Deferred drain: missing cached context for channel {} ({reason}, attempt {attempt}/{}); retrying",
                channel_id,
                DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
            );
            tokio::time::sleep(DEFERRED_IDLE_QUEUE_KICKOFF_RETRY_DELAY).await;
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
    fn zero_start_abandoned_claim_probe_fires_on_third_consecutive_pending_zero() {
        let mut consecutive = 0usize;
        assert!(!note_zero_start_deferred_drain(&mut consecutive, 0, true));
        assert!(!note_zero_start_deferred_drain(&mut consecutive, 0, true));
        assert!(note_zero_start_deferred_drain(&mut consecutive, 0, true));
        assert_eq!(
            consecutive, DEFERRED_IDLE_QUEUE_ZERO_START_ABANDONED_CLAIM_PROBE_AFTER,
            "the #3333 abandoned-claim probe must fire after 3 zero-start drains, not after the 150-attempt cap"
        );

        assert!(!note_zero_start_deferred_drain(&mut consecutive, 1, true));
        assert_eq!(consecutive, 0, "a non-zero drain resets the zero-start run");
        assert!(!note_zero_start_deferred_drain(&mut consecutive, 0, false));
        assert_eq!(consecutive, 0, "an empty target queue is a normal exit");
    }

    // Sync test + explicit block_on: the std-mutex test-env guards live only in
    // this sync scope and never span an await, so no await_holding_lock allow is
    // needed (#3034 ratchet stays frozen at its baseline).
    #[test]
    fn abandoned_presence_clear_releases_idle_queue_gate_but_keeps_durable_record() {
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
            let snapshot = mailbox_snapshot(&shared, channel_id).await;
            assert!(
                !idle_queue_snapshot_has_kickable_backlog(
                    &shared,
                    &provider,
                    channel_id,
                    &snapshot
                ),
                "pending synthetic-start presence must block the idle queue before the #3333 clear"
            );

            assert!(
                super::super::tui_direct_pending_start::clear_abandoned_synthetic_start_presence(
                    provider.as_str(),
                    channel_id.get(),
                ),
                "the abandoned durable record should allow presence-only clearing"
            );
            assert!(
                idle_queue_snapshot_has_kickable_backlog(&shared, &provider, channel_id, &snapshot),
                "after presence-only clear, the queued item is kickable by the final one-shot drain"
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
