//! Model-aware Claude `/compact` triggering and busy-turn steering.
//!
//! This trigger keeps main's proven *stateless observable-usage* shape: a single
//! per-pane `armed` bool, consumed once per fill cycle at a turn-completion
//! boundary and re-armed only after observable occupancy drops back below a
//! hysteresis floor (which is exactly what a real compaction does). It never owns
//! an identity/epoch/ticket lifecycle authority; the model-aware context window is
//! resolved fresh each turn by the caller (`claude_compact_context`) and only the
//! numeric threshold changes when the model/window changes.
//!
//! What #4591 keeps on top of main's shape:
//!   * a *token* threshold (`compact_threshold`) instead of a percentage, so the
//!     model-aware CTW resolution drives an exact absolute trigger, and
//!   * the steering primitive (`send_compact_while_busy` under the narrow
//!     per-pane composer lock) so an auto `/compact` steers a busy pane without
//!     waiting behind a normal turn's readiness phase.
//!
//! Lock discipline (the freeze-bug fix): the per-pane `armed` state lock is a
//! LEAF. It is taken only for a brief flip/read in [`observe_and_decide`],
//! [`pane_still_disarmed_for_send`], [`rearm_for_retry`], and [`clear_for_tmux`],
//! and is NEVER held across tmux I/O. Only the per-pane composer lock is held
//! across the send. This removes the old global-latch-held-across-submit
//! linearization that froze every pane's observation for the duration of a send.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::services::claude_compact_context::{CompactThreshold, compact_threshold};
use crate::services::claude_tui::input::CompactSubmitOutcome;
use crate::services::provider::ProviderKind;

/// Per-pane key for the once-per-fill-cycle armed flag. Keyed by both the Discord
/// channel and the physical tmux pane so [`clear_for_tmux`] can forget every
/// channel's flag for a recreated pane name (launch/teardown hygiene).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CompactPaneKey {
    channel_id: u64,
    tmux_session_name: String,
}

/// Per-pane "armed" state for the once-per-fill-cycle guard. `true` (the default,
/// via `entry().or_insert(true)`) means the next threshold crossing may inject;
/// injecting flips it to `false` until observable occupancy drops below the
/// re-arm floor. This is the entire persistent state of the trigger — no identity
/// tuple, epoch, or ticket.
static ARMED_BY_PANE: LazyLock<Mutex<HashMap<CompactPaneKey, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Update the per-pane armed flag from the latest turn-completion occupancy and
/// report whether THIS observation should inject `/compact`.
///
/// The re-arm check runs FIRST and is edge-triggered on an observable occupancy
/// drop (`occupied <= rearm_floor_tokens`): a real context reset — a landed
/// compaction — drops occupancy far below the threshold and re-arms; a small
/// jitter around the threshold does not. The inject check consumes the flag
/// optimistically the moment we decide to fire, so two near-simultaneous
/// completion observations cannot both inject. A non-confirmed send restores the
/// flag via [`rearm_for_retry`] so a later turn retries while usage stays high.
///
/// `occupied` is the observable USAGE occupancy (`context_occupancy_input_tokens`
/// = input + cache_create + cache_read). Idempotency is keyed on this reliable
/// signal, never on a cosmetic `auto_compacted` string heuristic.
fn observe_and_decide(pane: &CompactPaneKey, occupied: u64, threshold: CompactThreshold) -> bool {
    let mut latches = ARMED_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let armed = *latches.entry(pane.clone()).or_insert(true);
    // Re-arm first, independent of the inject check: a post-compact occupancy
    // drop (possibly to ~0) must re-arm a disarmed pane. It never injects.
    if !armed && occupied <= threshold.rearm_floor_tokens {
        latches.insert(pane.clone(), true);
        return false;
    }
    if armed && occupied >= threshold.effective_tokens {
        // Optimistically consume the flag so concurrent completion observations
        // do not double-inject. Restored by `rearm_for_retry` on a non-confirmed
        // send; re-armed naturally by the drop branch above after a real compact.
        latches.insert(pane.clone(), false);
        return true;
    }
    false
}

/// Observable pre-send revalidation, performed under the composer lock right
/// before the tmux mutation. This replaces the removed epoch/identity ticket
/// match: a queued worker proceeds only when the pane is still present AND still
/// disarmed for THIS fill cycle — i.e. no observable occupancy drop re-armed it
/// (a compaction/usage reset was seen) and no teardown/policy-clear removed it
/// since the flag was consumed. The leaf lock is read and released here; it is
/// NEVER held across the send.
fn pane_still_disarmed_for_send(pane: &CompactPaneKey) -> bool {
    let latches = ARMED_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // `Some(false)` = present and disarmed for this crossing → send.
    // `Some(true)` = re-armed by an observed occupancy drop → stale, bail.
    // `None` = torn down / cleared → bail.
    latches.get(pane).copied() == Some(false)
}

/// Restore the armed flag after a non-confirmed send so a later turn-completion
/// retries `/compact` while usage stays high — observable retry. Idempotent and
/// resurrection-safe: a pane the teardown path already removed stays removed, so
/// a late worker cannot revive a stale entry. If a concurrent post-compact drop
/// already re-armed the entry, this is a harmless no-op write.
fn rearm_for_retry(pane: &CompactPaneKey) {
    let mut latches = ARMED_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(armed) = latches.get_mut(pane) {
        *armed = true;
    }
}

/// Run the observable pre-send recheck and, if the world is unchanged, the
/// compact submit — both inside a single per-pane composer critical section. The
/// composer lock (not the leaf state lock) is the only lock held across the tmux
/// mutation, so a queued worker may wait behind another composer mutation but
/// never carries the leaf state lock into `submit`. `None` means the pre-send
/// recheck bailed (stale/torn-down) and no mutation was attempted.
fn submit_under_composer_lock(
    pane: &CompactPaneKey,
    submit: impl FnOnce() -> CompactSubmitOutcome,
) -> Option<CompactSubmitOutcome> {
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
        &pane.tmux_session_name,
        || {
            if !pane_still_disarmed_for_send(pane) {
                return None;
            }
            Some(submit())
        },
    )
}

/// Claude-only: at a watcher-completed (pane-idle) turn boundary, inject
/// `/compact` into the live TUI when observable context occupancy first crosses
/// the model-aware token threshold this fill cycle.
///
/// `usage_tokens` is the observable occupancy (`context_occupancy_input_tokens`);
/// `actual_window_tokens` is the launch-provenance-resolved Claude context window
/// for this turn (`None` when the window cannot be proven — fail closed, never
/// invent a native fallback). The percentage/lower-bound are combined into an
/// absolute token threshold each turn, so a model/window change simply changes
/// the number on the next turn.
///
/// Degrade-safe no-ops that never touch the armed flag: a non-Claude provider
/// (Codex compacts natively), an unresolvable window, or a `0`/disabled percent.
/// A `0`/low `usage_tokens` is deliberately NOT short-circuited: it is the
/// post-compact re-arm signal handled inside [`observe_and_decide`].
pub(crate) fn maybe_inject_compact(
    channel_id: u64,
    tmux_session_name: &str,
    provider: &ProviderKind,
    usage_tokens: u64,
    actual_window_tokens: Option<u64>,
    compact_percent: u64,
    lower_bound_tokens: u64,
) {
    if !matches!(provider, ProviderKind::Claude) {
        return;
    }
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return;
    }
    // A window this turn could not prove is fail-closed to no-inject (the caller
    // already applied the model-aware [1m] same-family guard). Unresolvable
    // window / zero-percent degrade safely WITHOUT touching the armed flag, just
    // like main's `threshold_pct == 0` short-circuit.
    let Some(actual_window_tokens) = actual_window_tokens else {
        return;
    };
    let Some(threshold) =
        compact_threshold(actual_window_tokens, compact_percent, lower_bound_tokens)
    else {
        return;
    };
    let pane = CompactPaneKey {
        channel_id,
        tmux_session_name: tmux_session_name.to_string(),
    };
    if !observe_and_decide(&pane, usage_tokens, threshold) {
        return;
    }

    // The flag was just consumed, so at most one blocking worker exists per
    // crossing. This worker holds ONLY the per-pane composer lock across the tmux
    // mutation (never the leaf state lock) and performs no turn-readiness wait.
    tokio::task::spawn_blocking(move || {
        let outcome = submit_under_composer_lock(&pane, || {
            crate::services::claude_tui::input::send_compact_while_busy(&pane.tmux_session_name)
        });
        match outcome {
            None => tracing::debug!(
                tmux_session_name = %pane.tmux_session_name,
                "skipping stale Claude auto compact before tmux mutation (occupancy drop re-armed or pane torn down)"
            ),
            Some(CompactSubmitOutcome::AcceptedOrQueued) => tracing::info!(
                tmux_session_name = %pane.tmux_session_name,
                usage_tokens,
                threshold_tokens = threshold.effective_tokens,
                "Claude auto compact accepted or queued"
            ),
            Some(CompactSubmitOutcome::PreMutationRefused) => {
                // No mutation happened (pane was not in an empty-composer state).
                // Re-arm so a later idle turn retries while usage stays high.
                rearm_for_retry(&pane);
                tracing::debug!(
                    tmux_session_name = %pane.tmux_session_name,
                    "Claude auto compact refused before mutation; armed flag restored for retry"
                );
            }
            Some(CompactSubmitOutcome::AmbiguousAfterMutation) => {
                // Observable retry (replaces the old never-re-arm rule that could
                // permanently disable auto-compact and let context grow without
                // bound): re-arm, then `observe_and_decide` next turn only
                // re-injects when usage is still high AND no compaction was
                // observed. If the ambiguous send actually landed, occupancy
                // drops and the re-arm branch keeps it armed without re-injecting.
                rearm_for_retry(&pane);
                tracing::warn!(
                    tmux_session_name = %pane.tmux_session_name,
                    "Claude auto compact outcome ambiguous after tmux mutation; armed flag restored for observable retry next turn"
                );
            }
        }
    });
}

/// Forget every compact armed flag tied to a physical tmux pane. This runs when a
/// new hosted Claude pane is prepared and when runtime bindings are torn down, so
/// a recreated pane cannot inherit an old disarmed flag under a reused name.
pub(crate) fn clear_for_tmux(tmux_session_name: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return;
    }
    ARMED_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .retain(|pane, _| pane.tmux_session_name != tmux_session_name);
}

#[cfg(test)]
fn reset_for_test() {
    ARMED_BY_PANE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// The armed flag is process-global test state. Keep every stateful test
/// serialized so one fixture cannot clear another under normal parallel runs.
#[cfg(test)]
pub(crate) static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the shared state guard and reset all compact-trigger test state while
/// the guard is held. The caller retains the guard for its entire test.
#[cfg(test)]
fn state_test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    reset_for_test();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> CompactPaneKey {
        CompactPaneKey {
            channel_id: 42,
            tmux_session_name: "tmux-4591".to_string(),
        }
    }

    // window 1_000_000, percent 50, lower 300_000 → effective 500_000,
    // rearm_floor = 500_000 - 5% * 1_000_000 = 450_000.
    fn threshold_for(window: u64) -> CompactThreshold {
        compact_threshold(window, 50, 300_000).expect("valid threshold fixture")
    }

    fn armed_state(pane: &CompactPaneKey) -> Option<bool> {
        ARMED_BY_PANE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(pane)
            .copied()
    }

    /// Mutation guard: the optimistic consume (`insert(pane, false)`) in the
    /// inject branch of `observe_and_decide`. Reverting it (not disarming on
    /// inject) makes the second/third parked observation inject again, so the
    /// `assert!(!observe_and_decide(...))` lines below fail with a double
    /// injection while usage stays parked above the threshold.
    #[test]
    fn armed_bool_consumed_once_per_fill_cycle() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        assert!(observe_and_decide(&pane, 600_000, threshold));
        assert!(!observe_and_decide(&pane, 600_000, threshold));
        assert!(!observe_and_decide(&pane, 999_000, threshold));
        assert_eq!(armed_state(&pane), Some(false));
    }

    /// Mutation guard: the `occupied <= rearm_floor_tokens` condition on the
    /// re-arm branch. Reverting it (re-arm whenever `!armed`, without an
    /// observable occupancy drop) re-arms on the first parked poll, so the
    /// second parked `observe_and_decide(&pane, 600_000, ...)` finds the pane
    /// re-armed and injects — the `assert!(!...)` on that line fails, i.e. a
    /// `/compact` flood every turn with no genuine compaction between them.
    #[test]
    fn rearm_requires_observable_occupancy_drop() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        // First crossing injects and disarms.
        assert!(observe_and_decide(&pane, 600_000, threshold));
        // Parked above the re-arm floor: no re-arm, and therefore no re-inject on
        // any later poll while still parked.
        assert!(!observe_and_decide(&pane, 600_000, threshold));
        // A dip that does NOT reach the floor (460_000 > 450_000) still no-ops.
        assert!(!observe_and_decide(&pane, 460_000, threshold));
        assert!(!observe_and_decide(&pane, 600_000, threshold));
        assert_eq!(armed_state(&pane), Some(false));
        // A genuine occupancy drop to/below the floor re-arms (compaction seen)
        // and does not itself inject.
        assert!(!observe_and_decide(&pane, 450_000, threshold));
        assert_eq!(armed_state(&pane), Some(true));
        // The next genuine crossing injects again.
        assert!(observe_and_decide(&pane, 600_000, threshold));
    }

    /// Mutation guard: `pane_still_disarmed_for_send` (the observable pre-send
    /// recheck). After the latch is consumed for a crossing, an observed
    /// occupancy drop re-arms the pane (a compaction landed). Reverting the
    /// recheck to `true` (or to `matches!(get, Some(_))`, ignoring the disarmed
    /// bool) makes the final assert fail — the queued worker would send a STALE
    /// second `/compact` after the context was already reset.
    #[test]
    fn pre_send_recheck_bails_when_occupancy_drop_rearmed_the_pane() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        // Cross → consume; the queued worker would still see the pane disarmed.
        assert!(observe_and_decide(&pane, 500_000, threshold));
        assert!(pane_still_disarmed_for_send(&pane));
        // A later completion observes a compaction (occupancy drop) → re-arm.
        assert!(!observe_and_decide(&pane, 400_000, threshold));
        // The pre-send recheck must now bail: the world changed.
        assert!(!pane_still_disarmed_for_send(&pane));
    }

    /// Mutation guard: the `None` (teardown) arm of `pane_still_disarmed_for_send`.
    /// A pane torn down while a worker was queued must not send. Reverting the
    /// recheck lets the worker send into a recreated/absent pane.
    #[test]
    fn pre_send_recheck_bails_after_teardown_clear() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        assert!(observe_and_decide(&pane, 600_000, threshold));
        assert!(pane_still_disarmed_for_send(&pane));
        clear_for_tmux(&pane.tmux_session_name);
        assert!(!pane_still_disarmed_for_send(&pane));
    }

    /// Lock-discipline + pre-send-recheck integration. Proves (a) the composer
    /// lock — not the leaf state lock — serializes the queued worker (it cannot
    /// validate or send until the held composer lock releases), and (b) a teardown
    /// that lands while the worker is queued makes the pre-send recheck bail with
    /// zero sends. Reverting the recheck lets the worker send after teardown
    /// (sends == 1, outcome != None). Holding the leaf lock across `submit`
    /// instead of the composer lock would let the worker run immediately, failing
    /// the `recv_timeout(25ms).is_err()` assertion.
    #[cfg(unix)]
    #[test]
    fn queued_worker_revalidates_under_composer_lock_and_bails_on_teardown() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, mpsc};
        use std::time::Duration;

        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        assert!(observe_and_decide(&pane, 500_000, threshold));
        let sends = Arc::new(AtomicUsize::new(0));
        let (queued_tx, queued_rx) = mpsc::channel();
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let worker_pane = pane.clone();
        let worker_sends = Arc::clone(&sends);

        crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
            &pane.tmux_session_name,
            || {
                let worker = std::thread::spawn(move || {
                    queued_tx.send(()).expect("signal queued compact worker");
                    let outcome = submit_under_composer_lock(&worker_pane, || {
                        worker_sends.fetch_add(1, Ordering::SeqCst);
                        CompactSubmitOutcome::AcceptedOrQueued
                    });
                    outcome_tx.send(outcome).expect("return compact outcome");
                });
                queued_rx
                    .recv_timeout(Duration::from_millis(250))
                    .expect("worker must queue behind the held composer lock");
                assert!(
                    outcome_rx.recv_timeout(Duration::from_millis(25)).is_err(),
                    "the queued worker must not validate or send before the composer lock releases"
                );
                clear_for_tmux(&pane.tmux_session_name);
                worker
            },
        )
        .join()
        .expect("compact worker thread");

        assert_eq!(
            outcome_rx
                .recv_timeout(Duration::from_millis(250))
                .expect("worker outcome"),
            None
        );
        assert_eq!(sends.load(Ordering::SeqCst), 0);
    }

    /// Observable retry: a non-confirmed send (`PreMutationRefused` or
    /// `AmbiguousAfterMutation`) restores the flag so the next turn re-injects
    /// only while usage stays high, and a real compaction (occupancy drop) is
    /// still observed instead of re-firing. Mutation guard: `rearm_for_retry`.
    /// Reverting it (leave disarmed on ambiguous) permanently disables
    /// auto-compact for a stuck-high pane — the `observe_and_decide(&pane,
    /// 700_000, ...)` re-inject assert fails.
    #[test]
    fn ambiguous_after_mutation_rearms_for_observable_retry() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        assert!(observe_and_decide(&pane, 700_000, threshold));
        assert_eq!(armed_state(&pane), Some(false));
        // Simulate the worker's ambiguous-after-mutation outcome.
        rearm_for_retry(&pane);
        assert_eq!(armed_state(&pane), Some(true));
        // Usage still high and no compaction observed → retry next turn.
        assert!(observe_and_decide(&pane, 700_000, threshold));
    }

    /// A pane the teardown path removed must not be resurrected by a late
    /// worker's re-arm. Mutation guard: the `if let Some(..)` presence check in
    /// `rearm_for_retry`. Reverting it to an unconditional insert re-creates a
    /// stale entry, so the `armed_state(&pane).is_none()` assertion fails.
    #[test]
    fn rearm_after_teardown_does_not_resurrect_a_cleared_pane() {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        assert!(observe_and_decide(&pane, 600_000, threshold));
        clear_for_tmux(&pane.tmux_session_name);
        rearm_for_retry(&pane);
        assert!(armed_state(&pane).is_none());
    }

    /// `clear_for_tmux` forgets every channel's flag for a recreated pane name.
    #[test]
    fn clear_for_tmux_removes_every_channel_flag_for_recreated_pane() {
        let _guard = state_test_guard();
        let first_pane = pane();
        let second_pane = CompactPaneKey {
            channel_id: 43,
            tmux_session_name: first_pane.tmux_session_name.clone(),
        };
        let threshold = threshold_for(1_000_000);
        assert!(observe_and_decide(&first_pane, 600_000, threshold));
        assert!(observe_and_decide(&second_pane, 600_000, threshold));
        clear_for_tmux(&first_pane.tmux_session_name);
        assert!(
            ARMED_BY_PANE
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );
        // A recreated pane starts armed again.
        assert!(observe_and_decide(&first_pane, 600_000, threshold));
    }

    /// Degrade-safe no-inject paths never touch the armed flag and never spawn a
    /// worker (so these are safe to call without a Tokio runtime): a non-Claude
    /// provider, an unresolvable window, and a zero/disabled percent.
    #[test]
    fn maybe_inject_degrades_safely_without_touching_the_flag() {
        let _guard = state_test_guard();
        let pane = pane();
        maybe_inject_compact(
            pane.channel_id,
            &pane.tmux_session_name,
            &ProviderKind::Codex,
            600_000,
            Some(1_000_000),
            50,
            300_000,
        );
        assert!(armed_state(&pane).is_none());
        maybe_inject_compact(
            pane.channel_id,
            &pane.tmux_session_name,
            &ProviderKind::Claude,
            600_000,
            None,
            50,
            300_000,
        );
        assert!(armed_state(&pane).is_none());
        maybe_inject_compact(
            pane.channel_id,
            &pane.tmux_session_name,
            &ProviderKind::Claude,
            600_000,
            Some(1_000_000),
            0,
            300_000,
        );
        assert!(armed_state(&pane).is_none());
    }

    /// Empty tmux session names are ignored (no flag entry created).
    #[test]
    fn blank_tmux_session_name_is_ignored() {
        let _guard = state_test_guard();
        maybe_inject_compact(
            42,
            "   ",
            &ProviderKind::Claude,
            600_000,
            Some(1_000_000),
            50,
            300_000,
        );
        assert!(
            ARMED_BY_PANE
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );
    }
}
