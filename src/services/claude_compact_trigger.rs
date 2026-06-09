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
//!   * **threshold-gated, degrade-safe** — a `0`/unset threshold or a `0`
//!     usage/window signal short-circuits to "no inject".
//!   * **once-per-fill-cycle** — a per-channel armed flag is consumed on inject
//!     and only RE-ARMS after usage drops back below a hysteresis margin (which
//!     happens when a compact resets the context), so we never re-inject every
//!     poll while still parked above the threshold.
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
        guard.insert(channel_id, false);
        return true;
    }
    false
}

/// Claude-only: when the latest (turn-idle) context usage crosses the configured
/// threshold for the first time this fill cycle, inject `/compact` into the live
/// TUI via the proven `send_followup_prompt` path on a blocking thread.
///
/// `usage_pct` is the live context occupancy percentage already computed for the
/// status panel; `threshold_pct` is `compact_pct_for(Claude)`. No-ops (and never
/// disarms) for any non-Claude provider, a `0` threshold, or a `0` usage signal,
/// so an unset threshold or a missing usage/window degrades safely.
pub(crate) fn maybe_inject_compact(
    channel_id: u64,
    tmux_session_name: &str,
    provider: &ProviderKind,
    usage_pct: u64,
    threshold_pct: u64,
) {
    if !matches!(provider, ProviderKind::Claude) || threshold_pct == 0 || usage_pct == 0 {
        return;
    }
    if !observe_and_decide(channel_id, usage_pct, threshold_pct) {
        return;
    }
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
        match crate::services::claude_tui::input::send_followup_prompt(
            &tmux_session_name,
            "/compact",
            None,
        ) {
            Ok(()) => tracing::info!(
                tmux_session_name = %tmux_session_name,
                "#3262 auto /compact injected into live Claude TUI"
            ),
            Err(error) => tracing::warn!(
                tmux_session_name = %tmux_session_name,
                error = %error,
                "#3262 auto /compact injection skipped (pane busy/unready or send failed); will retry on a later turn"
            ),
        }
    });
}

#[cfg(test)]
pub(crate) fn reset_armed_state_for_test() {
    ARMED_BY_CHANNEL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;

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
        reset_armed_state_for_test();
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
        reset_armed_state_for_test();
        let threshold = 80;
        assert!(observe_and_decide(1, 85, threshold));
        // Channel 1 is now disarmed, but channel 2 is still armed.
        assert!(!observe_and_decide(1, 90, threshold));
        assert!(observe_and_decide(2, 85, threshold));
    }
}
