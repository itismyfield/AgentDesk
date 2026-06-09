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

fn should_retry_deferred_idle_queue_kickoff(attempt: usize) -> bool {
    attempt < DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
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
}
