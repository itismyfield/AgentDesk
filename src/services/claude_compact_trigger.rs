//! #3262: AgentDesk-side `/compact` injection for the Claude TUI.
//!
//! Claude Code IGNORES the `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` env var AgentDesk
//! exports (services/claude.rs), so a configured `context_compact_percent_claude`
//! threshold never actually changes when Claude auto-compacts — it only compacts
//! at its own internal default. Codex, by contrast, honours a real
//! `model_auto_compact_token_limit` launch knob; Claude's TUI has no equivalent.
//!
//! The Claude TUI *does* respond to a user typing `/compact`, and AgentDesk
//! already delivers `/compact` into the live pane on demand (the manual
//! `ClaudeSlashPassthrough::Compact` command → `claude_tui::input::send_followup_prompt`).
//! This module fires that exact injection AUTOMATICALLY when live context usage
//! crosses the configured threshold, at a safe (turn-idle) point.
//!
//! Guards (see the issue's Phase-2 design):
//!   * **claude-only** — the caller passes the provider and we no-op for anything
//!     but `ProviderKind::Claude` (Codex compacts natively).
//!   * **threshold-gated, degrade-safe** — a `0`/unset threshold short-circuits
//!     to "no inject" without touching the latch. A `0`/low usage signal does NOT
//!     short-circuit: it is the post-compact re-arm signal and never injects on
//!     its own (`should_inject_compact` requires `usage >= threshold`).
//!   * **once-per-fill-cycle** — a per-channel armed flag is consumed on inject
//!     and only RE-ARMS after usage drops back below a hysteresis margin (which
//!     happens when a compact resets the context, including a drop to ~0), so we
//!     never re-inject every poll while still parked above the threshold. The
//!     latch is consumed optimistically and restored if the send fails, so a
//!     busy/dead-pane rejection retries on a later idle poll rather than being
//!     silently swallowed.
//!   * **idle-only** — the caller invokes this at the turn-completion boundary
//!     (pane idle); the injection itself rides `send_followup_prompt`, whose
//!     `wait_for_prompt_ready` gate refuses to submit into a busy pane.
//!   * **no Discord leak** — `send_followup_prompt` records the prompt as
//!     Discord-originated (`record_discord_originated_prompt`), and the observed
//!     `/compact` echo is suppressed by the #3153 machine-slash-command
//!     classifier, so the control string never surfaces as user-visible prose.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::services::provider::ProviderKind;

/// Hysteresis margin (percentage points): once we inject at/above the threshold,
/// the channel only RE-ARMS after usage falls to `threshold - REARM_MARGIN_PCT`
/// or below. A successful `/compact` drops usage far below the threshold, so this
/// re-arms naturally on the next turn; a small jitter around the threshold does
/// not. Keeping the margin modest means a genuine post-compact drop always
/// re-arms while a same-cycle re-cross never does.
const REARM_MARGIN_PCT: u64 = 5;

/// Per-channel "armed" state for the once-per-fill-cycle guard. `true` (the
/// default, via `entry().or_insert(true)`) means the next threshold crossing is
/// allowed to inject; injecting flips it to `false` until usage drops below the
/// re-arm point.
static ARMED_BY_CHANNEL: LazyLock<Mutex<HashMap<u64, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Pure decision: should we inject `/compact` for this poll?
///
/// `armed` carries the once-per-cycle latch. Returns `true` only when the
/// threshold is meaningful (`> 0`), the live usage has reached it, and the
/// channel is still armed for this fill cycle.
pub(crate) fn should_inject_compact(usage_pct: u64, threshold_pct: u64, armed: bool) -> bool {
    threshold_pct > 0 && armed && usage_pct >= threshold_pct
}

/// Pure re-arm decision: after a channel has fired (disarmed), should usage at
/// `usage_pct` re-arm it? Re-arm once usage falls to the hysteresis floor
/// (`threshold - REARM_MARGIN_PCT`, saturating) or below — i.e. a real context
/// reset occurred, not a same-cycle jitter at the threshold.
pub(crate) fn should_rearm(usage_pct: u64, threshold_pct: u64) -> bool {
    usage_pct <= threshold_pct.saturating_sub(REARM_MARGIN_PCT)
}

/// Update the per-channel armed latch from the latest usage observation and
/// report whether THIS observation should inject. Combines [`should_inject_compact`]
/// (consuming the latch on a yes) with [`should_rearm`] (restoring it after a
/// post-compact drop), so callers get a single edge-triggered answer.
///
/// Disarming here is *optimistic*: it consumes the latch the moment we decide to
/// inject, so two near-simultaneous idle polls cannot both fire (no
/// double-injection on success). If the subsequent send fails, the caller calls
/// [`rearm_after_failed_inject`] to restore the latch so a later idle poll
/// retries while usage is still parked above the threshold (issue #1).
///
/// The re-arm check runs FIRST and is independent of `usage_pct == 0`: a
/// post-compact drop to (or near) zero MUST re-arm the latch. Callers therefore
/// must NOT short-circuit on `usage_pct == 0` before reaching here, or a
/// disarmed channel could never re-arm (issue #4).
fn observe_and_decide(channel_id: u64, usage_pct: u64, threshold_pct: u64) -> bool {
    let mut guard = ARMED_BY_CHANNEL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let armed = *guard.entry(channel_id).or_insert(true);
    if !armed && should_rearm(usage_pct, threshold_pct) {
        guard.insert(channel_id, true);
        return false;
    }
    if should_inject_compact(usage_pct, threshold_pct, armed) {
        // Optimistically consume the latch so concurrent polls don't double-fire.
        // Restored by `rearm_after_failed_inject` if the send fails.
        guard.insert(channel_id, false);
        return true;
    }
    false
}

/// Restore the armed latch after an injection attempt failed (busy/dead pane or a
/// rejected send), so a later idle poll retries `/compact` while usage remains
/// high. Setting the channel back to armed is idempotent — if a concurrent
/// post-compact drop already re-armed it, this is a harmless no-op write.
fn rearm_after_failed_inject(channel_id: u64) {
    let mut guard = ARMED_BY_CHANNEL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.insert(channel_id, true);
}

/// Claude-only: when the latest (turn-idle) context usage crosses the configured
/// threshold for the first time this fill cycle, inject `/compact` into the live
/// TUI via the proven `send_followup_prompt` path on a blocking thread.
///
/// `usage_pct` is the live context occupancy percentage already computed for the
/// status panel; `threshold_pct` is `compact_pct_for(Claude)`. No-ops (and never
/// touches the latch) for any non-Claude provider or a `0` threshold, so an unset
/// threshold degrades safely. A `0`/low `usage_pct` is NOT short-circuited: it is
/// the post-compact re-arm signal and is handled by `observe_and_decide` (which
/// only injects when usage has reached the threshold).
pub(crate) fn maybe_inject_compact(
    channel_id: u64,
    tmux_session_name: &str,
    provider: &ProviderKind,
    usage_pct: u64,
    threshold_pct: u64,
) {
    // Degrade-safe no-ops: a non-Claude provider (Codex compacts natively) or an
    // unset/zero threshold (feature off) never inject AND never touch the latch.
    //
    // NOTE: `usage_pct == 0` is deliberately NOT short-circuited here. A drop to
    // (or near) zero is exactly the post-compact signal that must RE-ARM a
    // disarmed channel; `observe_and_decide` runs the re-arm check first and only
    // injects when `usage_pct >= threshold_pct`, so usage 0 still cannot inject
    // (issue #4).
    if !matches!(provider, ProviderKind::Claude) || threshold_pct == 0 {
        return;
    }
    if !observe_and_decide(channel_id, usage_pct, threshold_pct) {
        return;
    }
    // The latch was just consumed (disarmed) by `observe_and_decide`. From here
    // the latch is only left disarmed if the send SUCCEEDS; any failure re-arms
    // it via `rearm_after_failed_inject` so a later idle poll retries (issue #1).
    let tmux_session_name = tmux_session_name.to_string();
    // `send_followup_prompt` is blocking (it polls the pane for readiness and
    // drives tmux send-keys), so it must not run on the async watcher runtime.
    tokio::task::spawn_blocking(move || {
        tracing::info!(
            tmux_session_name = %tmux_session_name,
            usage_pct,
            threshold_pct,
            "#3262 auto-injecting /compact: live Claude context usage crossed configured threshold at turn-idle"
        );
        match inject_compact_under_submit_gate(&tmux_session_name) {
            Ok(()) => tracing::info!(
                tmux_session_name = %tmux_session_name,
                "#3262 auto /compact injected into live Claude TUI"
            ),
            Err(error) => {
                // Send failed (busy/dead pane or rejected submit): re-arm so a
                // later idle poll retries `/compact` while usage stays high.
                rearm_after_failed_inject(channel_id);
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    error = %error,
                    "#3262 auto /compact injection failed (pane busy/unready or send failed); latch re-armed, will retry on a later turn"
                );
            }
        }
    });
}

/// Drive the `/compact` submit through the SAME per-session turn lock that a
/// normal Discord follow-up holds (issue #2), so the auto-injection and a user
/// follow-up can never interleave their readiness check + tmux send against one
/// live pane. The lock is a `std::sync::Mutex` and `send_followup_prompt` is
/// blocking (no `.await`), so the guard is never held across an await — matching
/// the established normal-path pattern in `claude::execute_streaming_local_tui_tmux`.
#[cfg(unix)]
fn inject_compact_under_submit_gate(tmux_session_name: &str) -> Result<(), String> {
    crate::services::claude::with_claude_tui_session_turn_lock(tmux_session_name, || {
        crate::services::claude_tui::input::send_followup_prompt(
            tmux_session_name,
            "/compact",
            None,
        )
    })
}

/// Non-unix builds have no live tmux TUI (the per-session turn lock and the TUI
/// driver are `#[cfg(unix)]`), so this path is unreachable in practice. Keep a
/// degrade-safe direct call so the module still compiles everywhere.
#[cfg(not(unix))]
fn inject_compact_under_submit_gate(tmux_session_name: &str) -> Result<(), String> {
    crate::services::claude_tui::input::send_followup_prompt(tmux_session_name, "/compact", None)
}

#[cfg(test)]
pub(crate) fn reset_armed_state_for_test() {
    ARMED_BY_CHANNEL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

/// Test seam (#3262 issue #4): expose the pure latch transition so a CALL-SITE
/// integration test (in `adk_session`) can prove a turn-completion observation
/// with `usage_pct == 0` reaches the re-arm — i.e. that the call site does not
/// short-circuit zero usage before this latch runs. Returns whether THIS
/// observation injects (the re-arm itself returns `false`). Side-effecting on the
/// shared latch, so callers must hold [`state_test_guard`].
#[cfg(test)]
pub(crate) fn observe_and_decide_for_test(
    channel_id: u64,
    usage_pct: u64,
    threshold_pct: u64,
) -> bool {
    observe_and_decide(channel_id, usage_pct, threshold_pct)
}

/// The latch lives in a single process-global map and `reset_armed_state_for_test`
/// clears it wholesale, so stateful tests (in THIS module and in any cross-module
/// call-site integration test) must not interleave. Serialize every test that
/// touches the shared map behind this lock.
#[cfg(test)]
pub(crate) static STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the shared-latch test serialization guard AND reset the latch, so a
/// stateful test starts from a known-clean armed map. Used by this module's tests
/// and by the `adk_session` call-site integration test so both serialize against
/// the same global latch.
#[cfg(test)]
pub(crate) fn state_test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_armed_state_for_test();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    // Serialize stateful tests behind the module-level `STATE_TEST_LOCK` (lifted to
    // `pub(crate)` so the `adk_session` call-site integration test serializes
    // against the SAME global latch). `state_guard` also resets the latch.
    fn state_guard() -> MutexGuard<'static, ()> {
        state_test_guard()
    }

    // A threshold crossing while armed injects.
    #[test]
    fn injects_when_threshold_crossed_and_armed() {
        assert!(should_inject_compact(60, 60, true));
        assert!(should_inject_compact(95, 60, true));
    }

    // Below the threshold never injects, armed or not.
    #[test]
    fn does_not_inject_below_threshold() {
        assert!(!should_inject_compact(59, 60, true));
        assert!(!should_inject_compact(0, 60, true));
    }

    // Already fired this cycle (disarmed) never re-injects, even far above.
    #[test]
    fn does_not_inject_when_already_fired_this_cycle() {
        assert!(!should_inject_compact(99, 60, false));
        assert!(!should_inject_compact(60, 60, false));
    }

    // An unset / zero threshold degrades to no-inject.
    #[test]
    fn zero_threshold_never_injects() {
        assert!(!should_inject_compact(100, 0, true));
    }

    // Re-arm only after usage drops to the hysteresis floor (post-compact),
    // not on a same-cycle jitter right at/just-below the threshold.
    #[test]
    fn rearms_only_after_post_compact_drop() {
        // threshold 60, margin 5 → re-arm floor is 55.
        assert!(should_rearm(55, 60));
        assert!(should_rearm(20, 60)); // typical post-compact usage
        assert!(!should_rearm(56, 60));
        assert!(!should_rearm(60, 60));
    }

    // Full per-channel cycle: cross → inject once → stay disarmed while parked
    // above → re-arm after a post-compact drop → inject again next cross.
    #[test]
    fn full_cycle_once_per_fill_then_rearm() {
        let _g = state_guard();
        let ch = 4242;
        let threshold = 60;
        // Below threshold: no inject, stays armed.
        assert!(!observe_and_decide(ch, 50, threshold));
        // First crossing: injects, disarms.
        assert!(observe_and_decide(ch, 62, threshold));
        // Still parked above on subsequent polls: NO re-inject (once-per-cycle).
        assert!(!observe_and_decide(ch, 70, threshold));
        assert!(!observe_and_decide(ch, 99, threshold));
        // A small dip that does NOT reach the re-arm floor still no-ops.
        assert!(!observe_and_decide(ch, 58, threshold));
        // Post-compact drop reaches the floor: re-arms (and does not inject).
        assert!(!observe_and_decide(ch, 18, threshold));
        // Next genuine crossing injects again.
        assert!(observe_and_decide(ch, 65, threshold));
    }

    // Channels are independent: one channel's latch does not gate another's.
    #[test]
    fn per_channel_latch_is_independent() {
        let _g = state_guard();
        let threshold = 80;
        assert!(observe_and_decide(1, 85, threshold));
        // Channel 1 is now disarmed, but channel 2 is still armed.
        assert!(!observe_and_decide(1, 90, threshold));
        assert!(observe_and_decide(2, 85, threshold));
    }

    // Issue #1: a FAILED send must leave the latch ARMED so a later idle poll
    // retries `/compact` while usage is still parked above the threshold.
    #[test]
    fn failed_send_rearms_for_retry() {
        let _g = state_guard();
        let ch = 9001;
        let threshold = 60;
        // First crossing injects and (optimistically) disarms.
        assert!(observe_and_decide(ch, 80, threshold));
        // While disarmed, a re-cross does NOT inject (once-per-cycle).
        assert!(!observe_and_decide(ch, 85, threshold));
        // Simulate the send failing: re-arm.
        rearm_after_failed_inject(ch);
        // Now a re-cross at the same (still-high) usage injects again (retry).
        assert!(observe_and_decide(ch, 85, threshold));
    }

    // Issue #4: re-arm must be reachable even when usage drops to 0 (post-compact).
    // The usage==0 case is the canonical re-arm signal and must NOT be skipped.
    #[test]
    fn usage_zero_rearms_disarmed_channel() {
        let _g = state_guard();
        let ch = 9002;
        let threshold = 60;
        // Fire once, disarm.
        assert!(observe_and_decide(ch, 80, threshold));
        assert!(!observe_and_decide(ch, 90, threshold));
        // Post-compact drop to 0 re-arms (and does not inject).
        assert!(!observe_and_decide(ch, 0, threshold));
        // Next genuine crossing injects again.
        assert!(observe_and_decide(ch, 70, threshold));
    }

    // Issue #4: a small threshold (<= REARM_MARGIN_PCT) underflows the re-arm
    // floor to 0 via saturating_sub. Re-arm must then require usage to fall to 0,
    // which a post-compact drop reaches — the latch must NOT be permanently stuck.
    #[test]
    fn small_threshold_rearms_only_at_zero() {
        // threshold 3, margin 5 → saturating floor is 0.
        assert_eq!(3u64.saturating_sub(REARM_MARGIN_PCT), 0);
        // Only usage 0 reaches the floor; any non-zero usage does not re-arm.
        assert!(should_rearm(0, 3));
        assert!(!should_rearm(1, 3));
        assert!(!should_rearm(2, 3));

        let _g = state_guard();
        let ch = 9003;
        let threshold = 3;
        // Crossing at/above 3 injects and disarms.
        assert!(observe_and_decide(ch, 5, threshold));
        // Still parked above: no re-inject.
        assert!(!observe_and_decide(ch, 9, threshold));
        // A small dip that does not reach 0 does NOT re-arm.
        assert!(!observe_and_decide(ch, 1, threshold));
        // Post-compact drop to 0 re-arms.
        assert!(!observe_and_decide(ch, 0, threshold));
        // Next crossing injects again — latch was never permanently blocked.
        assert!(observe_and_decide(ch, 4, threshold));
    }

    // Issue #4 corollary: threshold == 1 (margin underflows) still re-arms at 0.
    #[test]
    fn threshold_one_rearms_at_zero() {
        assert_eq!(1u64.saturating_sub(REARM_MARGIN_PCT), 0);
        assert!(should_rearm(0, 1));
        assert!(!should_rearm(1, 1));

        let _g = state_guard();
        let ch = 9004;
        let threshold = 1;
        assert!(observe_and_decide(ch, 1, threshold));
        assert!(!observe_and_decide(ch, 50, threshold));
        assert!(!observe_and_decide(ch, 0, threshold)); // re-arm
        assert!(observe_and_decide(ch, 1, threshold)); // injects again
    }

    // The end-to-end no-op guards still hold: a zero threshold never injects and
    // never disarms, so the latch is untouched (degrade-safe / feature off).
    #[test]
    fn zero_threshold_maybe_inject_is_noop() {
        let _g = state_guard();
        let ch = 9005;
        // threshold 0 → maybe_inject_compact returns before observe_and_decide.
        maybe_inject_compact(ch, "irrelevant-session", &ProviderKind::Claude, 100, 0);
        // Latch untouched: still armed (default), so a real threshold later fires.
        assert!(observe_and_decide(ch, 80, 60));
    }

    // Non-Claude providers never inject and never touch the latch.
    #[test]
    fn non_claude_provider_maybe_inject_is_noop() {
        let _g = state_guard();
        let ch = 9006;
        maybe_inject_compact(ch, "irrelevant-session", &ProviderKind::Codex, 100, 60);
        assert!(observe_and_decide(ch, 80, 60));
    }
}
