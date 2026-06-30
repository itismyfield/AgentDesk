//! Timeout-policy reducer (#3916).
//!
//! `decide_timeout` resolves a `TransitionEvent::TimeoutExpired` against the
//! state's typed `TimeoutConfig` policy (`max_retries` / `backoff` /
//! `on_failure` / `on_exhaust_policy`) into a `TransitionDecision`. Extracted
//! from `transition.rs` so that reducer stays under the giant-file threshold.
//!
//! NOTE (#3916 scope boundary): this reducer is reached only via
//! `TransitionEvent::TimeoutExpired`, which the *live* timeout sweep
//! (`policies/timeouts/card-timeouts.js` + `dispatch-maintenance.js`) does NOT
//! yet emit — that sweep still escalates/retries directly with a hardcoded
//! budget. So the policy resolved here does not yet affect live cards; routing
//! the live sweep through this reducer is the deferred follow-up epic. These are
//! the reducer-level semantics the future live wiring will build on.

use super::transition::{
    ForceIntent, TransitionContext, TransitionDecision, TransitionIntent, TransitionOutcome,
    decide_pipeline_transition,
};
use crate::pipeline::{OnExhaustPolicy, OnFailurePolicy, TimeoutConfig};

/// Terminal action once a timeout can no longer be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutExhaustAction {
    /// Escalate: transition to the `on_exhaust` target (or audit when unset).
    Escalate,
    /// Record the exhaustion without changing card state.
    Notify,
    /// Fail: terminate the card by forcing it to a terminal state.
    Fail,
}

/// Timeout reducer. `attempt` is the number of retries already performed for
/// `state`. Honors the typed `TimeoutConfig` policy when one is configured, and
/// otherwise preserves the legacy immediate `on_exhaust` transition (additive —
/// default/None unchanged). See the module doc for the live-path scope boundary.
pub(crate) fn decide_timeout(
    ctx: &TransitionContext,
    state: &str,
    attempt: u32,
) -> TransitionDecision {
    let card = &ctx.card;
    // Stale event guard: the card already moved on.
    if card.status != state {
        return TransitionDecision {
            outcome: TransitionOutcome::NoOp,
            intents: vec![],
        };
    }

    let Some(timeout) = ctx.pipeline.timeouts.get(state) else {
        return TransitionDecision {
            outcome: TransitionOutcome::NoOp,
            intents: vec![],
        };
    };

    // Additive guard: with no typed retry/failure/exhaust policy, keep the
    // legacy behavior of transitioning immediately to the `on_exhaust` target.
    if !timeout.retry_policy_engaged() {
        return legacy_timeout_transition(ctx, timeout);
    }

    // Post-exhaustion / direct exhaust action from the typed `on_exhaust_policy`.
    let exhaust_action = match timeout.effective_on_exhaust_policy() {
        OnExhaustPolicy::Escalate => TimeoutExhaustAction::Escalate,
        OnExhaustPolicy::Notify => TimeoutExhaustAction::Notify,
        OnExhaustPolicy::Fail => TimeoutExhaustAction::Fail,
    };

    match timeout.effective_on_failure() {
        OnFailurePolicy::RetryWithBackoff => {
            let max_retries = timeout.effective_max_retries();
            if attempt < max_retries {
                // Re-issue dispatch after the configured backoff instead of
                // transitioning. `backoff_delay_seconds` is 1-indexed.
                let next_attempt = attempt + 1;
                let delay_seconds = timeout.backoff_delay_seconds(next_attempt);
                return TransitionDecision {
                    outcome: TransitionOutcome::Allowed,
                    intents: vec![TransitionIntent::ScheduleStageRetry {
                        card_id: card.id.clone(),
                        state: state.to_string(),
                        attempt: next_attempt,
                        delay_seconds,
                    }],
                };
            }
            // Retries exhausted → apply the typed exhaust policy.
            apply_timeout_exhaust(ctx, timeout, exhaust_action, max_retries)
        }
        OnFailurePolicy::FallbackStage => match timeout.on_failure_target.as_deref() {
            Some(target) => {
                decide_pipeline_transition(ctx, target, "timeout", ForceIntent::None, "timeout")
            }
            // Misconfigured fallback (no target) → escalate via on_exhaust.
            None => apply_timeout_exhaust(ctx, timeout, TimeoutExhaustAction::Escalate, 0),
        },
        // Escalate immediately to the on_exhaust target (no retries).
        OnFailurePolicy::Escalate => {
            apply_timeout_exhaust(ctx, timeout, TimeoutExhaustAction::Escalate, 0)
        }
        // `Fail` resolves from an explicit `on_failure: fail` (→ terminal
        // failure), or from engagement via `on_exhaust_policy` ALONE with no
        // retry configured (→ apply that exhaust policy directly, 0 retries).
        OnFailurePolicy::Fail => {
            if timeout.on_failure.is_some() {
                apply_timeout_exhaust(ctx, timeout, TimeoutExhaustAction::Fail, 0)
            } else {
                apply_timeout_exhaust(ctx, timeout, exhaust_action, 0)
            }
        }
    }
}

/// Legacy timeout behavior: transition to the `on_exhaust` target if present,
/// else no-op. Preserved verbatim for configs without a typed policy.
fn legacy_timeout_transition(
    ctx: &TransitionContext,
    timeout: &TimeoutConfig,
) -> TransitionDecision {
    match timeout.on_exhaust.as_deref() {
        Some(target) => {
            decide_pipeline_transition(ctx, target, "timeout", ForceIntent::None, "timeout")
        }
        None => TransitionDecision {
            outcome: TransitionOutcome::NoOp,
            intents: vec![],
        },
    }
}

/// Apply the timeout action once retries are exhausted or skipped.
fn apply_timeout_exhaust(
    ctx: &TransitionContext,
    timeout: &TimeoutConfig,
    action: TimeoutExhaustAction,
    max_retries: u32,
) -> TransitionDecision {
    let card = &ctx.card;
    match action {
        // Notify: observe the exhaustion, leave card state untouched.
        TimeoutExhaustAction::Notify => TransitionDecision {
            outcome: TransitionOutcome::Allowed,
            intents: vec![TransitionIntent::AuditLog {
                card_id: card.id.clone(),
                from: card.status.clone(),
                to: card.status.clone(),
                source: "timeout".to_string(),
                message: format!(
                    "timeout exhausted after {max_retries} retr{} — notify (no state change)",
                    if max_retries == 1 { "y" } else { "ies" }
                ),
            }],
        },
        // Escalate: transition to the configured `on_exhaust` target.
        TimeoutExhaustAction::Escalate => match timeout.on_exhaust.as_deref() {
            Some(target) => {
                decide_pipeline_transition(ctx, target, "timeout", ForceIntent::None, "timeout")
            }
            None => TransitionDecision {
                outcome: TransitionOutcome::Allowed,
                intents: vec![TransitionIntent::AuditLog {
                    card_id: card.id.clone(),
                    from: card.status.clone(),
                    to: card.status.clone(),
                    source: "timeout".to_string(),
                    message: "timeout exhausted — escalate (no on_exhaust target configured)"
                        .to_string(),
                }],
            },
        },
        // Fail: terminate the card by forcing it to a terminal state. Failure is
        // a system-driven move, so force past the rule/gate guards (#3916 P1-3).
        TimeoutExhaustAction::Fail => {
            if let Some(terminal) = ctx.pipeline.states.iter().find(|s| s.terminal) {
                if card.status != terminal.id {
                    return decide_pipeline_transition(
                        ctx,
                        &terminal.id,
                        "timeout",
                        ForceIntent::SystemRecovery,
                        "timeout",
                    );
                }
            }
            // No distinct terminal state (or already terminal) → fall back to the
            // on_exhaust target, else record the terminal failure in the audit log.
            match timeout.on_exhaust.as_deref() {
                Some(target) => {
                    decide_pipeline_transition(ctx, target, "timeout", ForceIntent::None, "timeout")
                }
                None => TransitionDecision {
                    outcome: TransitionOutcome::Allowed,
                    intents: vec![TransitionIntent::AuditLog {
                        card_id: card.id.clone(),
                        from: card.status.clone(),
                        to: card.status.clone(),
                        source: "timeout".to_string(),
                        message: "timeout exhausted — fail (no terminal/on_exhaust target)"
                            .to_string(),
                    }],
                },
            }
        }
    }
}
