//! #3869: restart-time routing-orphan finalization.
//!
//! `recovery_engine::restore_inflight_turns` re-validates each persisted
//! in-flight turn's bot/channel routing on boot. When that validation fails the
//! row is either GENUINELY ORPHANED (its channel was re-bound to a different
//! provider while dcserver was down, so no same-provider sibling bot can adopt
//! it) or merely RE-ROUTABLE (a `is_expected_cross_bot_skip` failure — a sibling
//! bot owns the channel/agent and recovers the row from the SAME persisted
//! state). The old code did a bare `continue` for BOTH, silently stranding the
//! orphaned row's placeholder until the ~1800s sweeper reaped it.
//!
//! This leaf module owns the orphan-vs-skip decision and the finalize path,
//! keeping the giant `recovery_engine.rs` root unchanged at its call sites.

use std::sync::Arc;

// Mirror the discord module's `serenity` alias (the crate-wide convention) so
// `serenity::Http` resolves to the poise re-export, matching the call sites.
use poise::serenity_prelude as serenity;

use crate::services::discord::SharedData;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::recovery_paths::restart::dispose_recovery_relay_outcome;
use crate::services::discord::settings::BotChannelRoutingGuardFailure;
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::ProviderKind;

/// Route a restart-time routing-validation failure for an in-flight row.
///
/// - GENUINELY ORPHANED (`orphans_inflight_on_restart`) → finalize + notify via
///   [`cleanup_routing_orphaned_inflight`] instead of stranding the row.
/// - EXPECTED CROSS-BOT SKIP → PRESERVE the row for the owning sibling bot (the
///   original bare-`continue` behavior). `log_expected_cross_bot_skip` keeps the
///   per-call-site logging behavior byte-equivalent: the restart-report site
///   suppressed expected-skip noise (`false`); the other sites logged every skip
///   at `info` (`true`).
pub(super) async fn route_recovery_skip(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    tmux_session_name: Option<&str>,
    reason: BotChannelRoutingGuardFailure,
    log_expected_cross_bot_skip: bool,
) {
    if reason.orphans_inflight_on_restart() {
        cleanup_routing_orphaned_inflight(http, shared, provider, state, tmux_session_name, reason)
            .await;
        return;
    }
    // Expected cross-bot skip — a same-provider sibling bot owns/recovers this
    // row; preserve it untouched.
    if log_expected_cross_bot_skip {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⏭ inflight recovery skip for channel {} — {reason}",
            state.channel_id,
        );
    }
}

/// The disposition guard's `tmux_alive` for an orphaned row, derived from the
/// THREE-state pane probe ([`PaneLiveness`]).
///
/// The two-state `has_live_pane` collapses BOTH "session absent" and "probe
/// failed" to `false`. Feeding that to the destructive disposition matrix would
/// let a transient tmux `ProbeError` masquerade as death and budget-clear a LIVE
/// re-bound pane after transient terminal-notice failures — a data-loss
/// regression worse than the orphan it fixes. Per the `PaneLiveness` contract a
/// `ProbeError` is NOT proof of death: only a DEFINITIVE `DeadOrAbsent` pane may
/// permit the force-clear path; `Live` and `ProbeError` both preserve (the same
/// conservative direction the rest of the recovery code uses for unknown
/// liveness).
fn routing_orphan_pane_alive(liveness: PaneLiveness) -> bool {
    !matches!(liveness, PaneLiveness::DeadOrAbsent)
}

/// Finalize a restart-time inflight row whose bot/channel routing CHANGED while
/// dcserver was down (e.g. the channel was re-bound to a different provider).
/// Such a row is genuinely orphaned — no same-provider sibling bot will adopt
/// it. The old behavior was a bare `continue` that stranded the placeholder/row
/// until the ~1800s sweeper reaped it, so the user's in-flight turn was silently
/// lost in the meantime.
///
/// Finalize it the way every other non-recoverable recovery row is handled:
/// deliver the interrupted terminal notice to the placeholder and then finish +
/// clear the durable inflight state via the shared disposition matrix
/// ([`dispose_recovery_relay_outcome`]) — which preserves-and-retries on a
/// transient relay failure (so the user notice is not itself dropped) and
/// force-clears on a permanent failure / exhausted budget.
///
/// The `tmux_alive` fed to that DESTRUCTIVE disposition guard is derived from
/// the THREE-state pane probe ([`routing_orphan_pane_alive`]): a transient tmux
/// `ProbeError` must NOT be mistaken for death, so it preserves the row instead
/// of permitting the budget force-clear that would tear down a live, recoverable
/// re-bound pane.
async fn cleanup_routing_orphaned_inflight(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    tmux_session_name: Option<&str>,
    reason: BotChannelRoutingGuardFailure,
) {
    // Conservative liveness for the destructive disposition guard: only a
    // DEFINITIVE dead/absent pane (or no session name at all) permits the budget
    // force-clear path; a transient probe ERROR is treated as maybe-alive and
    // preserves the row (re-notify next boot) — never budget-clear a live pane.
    let tmux_alive = tmux_session_name.map_or(false, |name| {
        routing_orphan_pane_alive(
            crate::services::tmux_diagnostics::tmux_session_pane_liveness(name),
        )
    });
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] 🧹 recovery: inflight routing changed for channel {} ({reason}) — finalizing orphaned turn instead of stranding it for the sweeper (#3869)",
        state.channel_id,
    );
    // A restart report (the restart-report recovery sites) is no longer
    // actionable once the row is being finalized — clear it so a later boot does
    // not re-enter the restart path for a channel this bot no longer routes.
    // Idempotent when none exists.
    crate::services::discord::restart_report::clear_restart_report(provider, state.channel_id);
    let text = super::interrupted_recovery_message(state, &state.full_response);
    let outcome = super::relay_recovery_terminal_notice(http, shared, provider, state, &text).await;
    dispose_recovery_relay_outcome(
        shared,
        provider,
        state,
        outcome,
        tmux_alive,
        "recovery_routing_orphaned",
        "routing_orphaned",
        &state.full_response,
        false,
    )
    .await;
}

/// #3869 codex-rework regression: the orphaned-row cleanup must derive its
/// DESTRUCTIVE `tmux_alive` disposition guard from the THREE-state pane probe.
/// A transient tmux `ProbeError` is not proof of death, so it must PRESERVE the
/// row (re-notify next boot) rather than let the budget force-clear tear down a
/// live, recoverable re-bound pane — only a DEFINITIVE dead/absent pane permits
/// the clean-up. These tests pin both the pure mapping and its end-to-end effect
/// through the shared disposition matrix, with no http or real tmux.
#[cfg(test)]
mod tests {
    use super::routing_orphan_pane_alive;
    use crate::services::discord::inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET;
    use crate::services::discord::recovery_paths::shared::{
        RecoveryRelayOutcome, RowDisposition, unrecoverable_relay_disposition,
    };
    use crate::services::platform::tmux::PaneLiveness;

    #[test]
    fn probe_error_is_treated_as_alive_not_dead() {
        // The crux of the codex finding: a transient probe error is NOT death.
        assert!(
            routing_orphan_pane_alive(PaneLiveness::ProbeError),
            "a transient tmux ProbeError must preserve (maybe-alive), not budget-clear",
        );
        assert!(
            routing_orphan_pane_alive(PaneLiveness::Live),
            "a live pane must preserve",
        );
        assert!(
            !routing_orphan_pane_alive(PaneLiveness::DeadOrAbsent),
            "only a definitively dead/absent pane may permit the force-clear path",
        );
    }

    #[test]
    fn orphan_with_probe_error_is_preserved_while_dead_pane_is_cleaned() {
        // An orphaned row whose terminal notice transiently failed, with the
        // restart relay budget already exhausted: the ONLY thing standing between
        // the row and a destructive force-clear is the `tmux_alive` guard.
        let attempts_at_budget_edge = RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET.saturating_sub(1);

        // ProbeError → tmux_alive=true → PRESERVE (re-notify next boot); never
        // tear down a possibly-live re-bound pane. This is the regression guard.
        assert_eq!(
            unrecoverable_relay_disposition(
                RecoveryRelayOutcome::TransientFailure,
                attempts_at_budget_edge,
                RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                routing_orphan_pane_alive(PaneLiveness::ProbeError),
            ),
            RowDisposition::PreserveAndCount,
            "#3869 codex-rework: a probe-erroring orphan must be preserved, not budget-cleared",
        );

        // Live → tmux_alive=true → never budget-clears, even at/over budget.
        assert_eq!(
            unrecoverable_relay_disposition(
                RecoveryRelayOutcome::TransientFailure,
                RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                routing_orphan_pane_alive(PaneLiveness::Live),
            ),
            RowDisposition::PreserveAndCount,
            "a live pane must never be budget-cleared",
        );

        // DeadOrAbsent → tmux_alive=false → once the relay budget is exhausted,
        // the definitively-dead orphan IS force-cleared (cleaned, not stranded).
        assert_eq!(
            unrecoverable_relay_disposition(
                RecoveryRelayOutcome::TransientFailure,
                attempts_at_budget_edge,
                RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                routing_orphan_pane_alive(PaneLiveness::DeadOrAbsent),
            ),
            RowDisposition::ClearBudgetExhausted,
            "a definitively-dead orphan must be cleaned once the relay budget is exhausted",
        );
    }
}
