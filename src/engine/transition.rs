//! Kanban state transition reducer (#155).
//!
//! Pure-function `decide_transition` takes a `TransitionContext` and a
//! `TransitionEvent`, and returns a `TransitionDecision` containing the
//! outcome (allowed / blocked) plus an ordered list of `TransitionIntent`s.
//!
//! The Executor (`execute_decision`) applies intents against the database.
//! No direct SQL UPDATEs to `kanban_cards.status`, `review_status`, or
//! `latest_dispatch_id` should happen outside this module.

use crate::pipeline::{PipelineConfig, TransitionType};
use serde::{Deserialize, Serialize};

// ── Context types ────────────────────────────────────────────

/// Snapshot of the card's current state — assembled by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardState {
    pub id: String,
    pub status: String,
    pub review_status: Option<String>,
    pub latest_dispatch_id: Option<String>,
}

/// Gate checks pre-evaluated by the caller (DB queries done before calling decide).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GateSnapshot {
    /// Whether the card has at least one pending/dispatched dispatch.
    pub has_active_dispatch: bool,
    /// Whether the latest completed review dispatch has verdict=pass/approved.
    pub review_verdict_pass: bool,
    /// Whether the latest completed review dispatch has verdict=rework/improve.
    pub review_verdict_rework: bool,
}

/// Everything the pure reducer needs — no DB handle.
#[derive(Debug, Clone)]
pub struct TransitionContext {
    pub card: CardState,
    pub pipeline: PipelineConfig,
    pub gates: GateSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ForceIntent {
    #[default]
    None,
    OperatorOverride,
    SystemRecovery,
}

impl ForceIntent {
    pub fn is_forced(self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn audit_value(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::OperatorOverride => "operator_override",
            Self::SystemRecovery => "system_recovery",
        }
    }

    fn audit_reason(self, source: &str) -> Option<String> {
        match self {
            Self::None => None,
            Self::OperatorOverride => Some(format!("explicit operator override via {source}")),
            Self::SystemRecovery => Some(format!("system recovery via {source}")),
        }
    }
}

// ── Events ───────────────────────────────────────────────────

/// What happened that might cause a state transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransitionEvent {
    /// A dispatch was created and attached to the card.
    DispatchAttached {
        dispatch_id: String,
        dispatch_type: String,
        kickoff_state: Option<String>,
    },
    /// A dispatch finished executing.
    DispatchCompleted { dispatch_id: String },
    /// A review verdict was submitted (pass / improve / reject).
    ReviewVerdict { verdict: String },
    /// A review-decision dispatch completed with accept/dispute/dismiss.
    ReviewDecision { decision: String },
    /// A timeout expired for the current state.
    TimeoutExpired { state: String },
    /// PMD/admin manually moves the card (force=true).
    OperatorOverride { target_status: String },
    /// Card is reopened from a terminal state.
    ReopenRequested { target_status: String },
    /// Redispatch requested — cancel current, restart.
    RedispatchRequested,
}

// ── Decision types ───────────────────────────────────────────

/// The outcome of `decide_transition`.
#[derive(Debug, Clone)]
pub struct TransitionDecision {
    pub outcome: TransitionOutcome,
    /// Ordered list of side-effects to execute. Empty if blocked.
    pub intents: Vec<TransitionIntent>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TransitionOutcome {
    /// Transition allowed.
    Allowed,
    /// No state change needed (e.g., already in target state).
    NoOp,
    /// Transition blocked with reason.
    Blocked(String),
}

/// A single side-effect produced by the reducer. The Executor applies these in order.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionIntent {
    /// Update kanban_cards.status (the primary column).
    UpdateStatus {
        card_id: String,
        from: String,
        to: String,
    },
    /// Set kanban_cards.latest_dispatch_id.
    SetLatestDispatchId {
        card_id: String,
        dispatch_id: Option<String>,
    },
    /// Set kanban_cards.review_status.
    SetReviewStatus {
        card_id: String,
        review_status: Option<String>,
    },
    /// Apply clock field for a state (e.g., started_at = datetime('now')).
    /// Clock config is pre-resolved from the effective pipeline by the reducer,
    /// so the executor doesn't need to re-fetch the global default (#155 review fix).
    ApplyClock {
        card_id: String,
        state: String,
        clock: Option<crate::pipeline::ClockConfig>,
    },
    /// Clear review-related fields on terminal entry.
    ClearTerminalFields { card_id: String },
    /// Sync auto_queue_entries to 'done' for terminal states.
    SyncAutoQueue { card_id: String },
    /// Sync card_review_state canonical record.
    SyncReviewState {
        card_id: String,
        state: String, // "idle", "reviewing", "rework_pending", etc.
    },
    /// Write an audit log entry.
    AuditLog {
        card_id: String,
        from: String,
        to: String,
        source: String,
        message: String,
    },
    /// Cancel a dispatch (set status='cancelled').
    CancelDispatch { dispatch_id: String },
}

// ── Pure reducer ─────────────────────────────────────────────

/// Pure function: given context + event, decide the transition and list intents.
///
/// This function performs NO I/O. All DB state is pre-loaded into `TransitionContext`.
pub fn decide_transition(ctx: &TransitionContext, event: &TransitionEvent) -> TransitionDecision {
    match event {
        TransitionEvent::OperatorOverride { target_status } => {
            decide_operator_override(ctx, target_status)
        }
        TransitionEvent::ReopenRequested { target_status } => decide_reopen(ctx, target_status),
        TransitionEvent::DispatchAttached {
            dispatch_id,
            dispatch_type,
            kickoff_state,
        } => decide_dispatch_attached(ctx, dispatch_id, dispatch_type, kickoff_state.as_deref()),
        TransitionEvent::RedispatchRequested => decide_redispatch(ctx),
        TransitionEvent::ReviewVerdict { verdict } => decide_review_verdict(ctx, verdict),
        TransitionEvent::ReviewDecision { decision } => decide_review_decision(ctx, decision),
        TransitionEvent::DispatchCompleted { dispatch_id } => {
            decide_dispatch_completed(ctx, dispatch_id)
        }
        TransitionEvent::TimeoutExpired { state } => decide_timeout(ctx, state),
    }
}

// ── Individual event handlers (all pure) ─────────────────────

/// OperatorOverride: PMD/admin force move. Bypasses gates and terminal guard.
fn decide_operator_override(ctx: &TransitionContext, target: &str) -> TransitionDecision {
    let card = &ctx.card;
    if card.status == target {
        return TransitionDecision {
            outcome: TransitionOutcome::NoOp,
            intents: vec![],
        };
    }

    let mut intents = vec![];
    intents.push(TransitionIntent::UpdateStatus {
        card_id: card.id.clone(),
        from: card.status.clone(),
        to: target.to_string(),
    });
    intents.push(TransitionIntent::ApplyClock {
        card_id: card.id.clone(),
        state: target.to_string(),
        clock: ctx.pipeline.clock_for_state(target).cloned(),
    });
    if !ctx.pipeline.is_terminal(target) {
        intents.push(TransitionIntent::SetReviewStatus {
            card_id: card.id.clone(),
            review_status: review_status_for(target, &ctx.pipeline),
        });
    }
    intents.push(TransitionIntent::SyncReviewState {
        card_id: card.id.clone(),
        state: review_state_for(target, &ctx.pipeline),
    });
    if ctx.pipeline.is_terminal(target) {
        intents.push(TransitionIntent::ClearTerminalFields {
            card_id: card.id.clone(),
        });
        intents.push(TransitionIntent::SyncAutoQueue {
            card_id: card.id.clone(),
        });
    }
    intents.push(TransitionIntent::AuditLog {
        card_id: card.id.clone(),
        from: card.status.clone(),
        to: target.to_string(),
        source: "pmd".to_string(),
        message: "OK (force)".to_string(),
    });

    TransitionDecision {
        outcome: TransitionOutcome::Allowed,
        intents,
    }
}

/// Standard status transition triggered by pipeline rules.
/// This is the core path used by `transition_status_with_opts`.
fn decide_pipeline_transition(
    ctx: &TransitionContext,
    target: &str,
    source: &str,
    force_intent: ForceIntent,
    caller: &str,
) -> TransitionDecision {
    let card = &ctx.card;
    let pipeline = &ctx.pipeline;

    if card.status == target {
        return TransitionDecision {
            outcome: TransitionOutcome::NoOp,
            intents: vec![],
        };
    }

    // Terminal guard
    if pipeline.is_terminal(&card.status) && !force_intent.is_forced() {
        return TransitionDecision {
            outcome: TransitionOutcome::Blocked(format!(
                "cannot revert terminal card: {} → {} is not allowed",
                card.status, target
            )),
            intents: vec![TransitionIntent::AuditLog {
                card_id: card.id.clone(),
                from: card.status.clone(),
                to: target.to_string(),
                source: source.to_string(),
                message: "BLOCKED: cannot revert terminal card".to_string(),
            }],
        };
    }

    // Transition rule lookup
    let rule = pipeline.find_transition(&card.status, target);
    match rule {
        Some(t) => match t.transition_type {
            TransitionType::Free => {}
            TransitionType::Gated if force_intent.is_forced() => {}
            TransitionType::ForceOnly if force_intent.is_forced() => {}
            TransitionType::Gated => {
                // Evaluate gates
                for gate_name in &t.gates {
                    if let Some(gate) = pipeline.gates.get(gate_name.as_str()) {
                        if gate.gate_type == "builtin" {
                            let blocked_msg = match gate.check.as_deref() {
                                Some("has_active_dispatch") if !ctx.gates.has_active_dispatch => {
                                    Some("BLOCKED: no active dispatch")
                                }
                                Some("review_verdict_pass") if !ctx.gates.review_verdict_pass => {
                                    Some("BLOCKED: no review pass verdict for current round")
                                }
                                Some("review_verdict_rework")
                                    if !ctx.gates.review_verdict_rework =>
                                {
                                    Some("BLOCKED: no review rework verdict for current round")
                                }
                                _ => None,
                            };
                            if let Some(msg) = blocked_msg {
                                return TransitionDecision {
                                    outcome: TransitionOutcome::Blocked(format!(
                                        "Status transition {} → {} failed gate '{}': {}",
                                        card.status, target, gate_name, msg
                                    )),
                                    intents: vec![TransitionIntent::AuditLog {
                                        card_id: card.id.clone(),
                                        from: card.status.clone(),
                                        to: target.to_string(),
                                        source: source.to_string(),
                                        message: msg.to_string(),
                                    }],
                                };
                            }
                        }
                    }
                }
            }
            TransitionType::ForceOnly => {
                return TransitionDecision {
                    outcome: TransitionOutcome::Blocked(format!(
                        "Status transition {} → {} requires force",
                        card.status, target
                    )),
                    intents: vec![TransitionIntent::AuditLog {
                        card_id: card.id.clone(),
                        from: card.status.clone(),
                        to: target.to_string(),
                        source: source.to_string(),
                        message: "BLOCKED: force_only transition requires force".to_string(),
                    }],
                };
            }
        },
        None if force_intent.is_forced() => {
            tracing::info!(
                card_id = %card.id,
                from = %card.status,
                to = %target,
                source = %source,
                force_intent = %force_intent.audit_value(),
                caller = caller,
                "force transition without rule: {} → {}",
                card.status,
                target
            );
        }
        None => {
            return TransitionDecision {
                outcome: TransitionOutcome::Blocked(format!(
                    "No transition rule from {} to {} in pipeline definition",
                    card.status, target
                )),
                intents: vec![TransitionIntent::AuditLog {
                    card_id: card.id.clone(),
                    from: card.status.clone(),
                    to: target.to_string(),
                    source: source.to_string(),
                    message: "BLOCKED: no transition rule".to_string(),
                }],
            };
        }
    }

    // Allowed — build intents
    let mut intents = vec![];
    intents.push(TransitionIntent::UpdateStatus {
        card_id: card.id.clone(),
        from: card.status.clone(),
        to: target.to_string(),
    });
    intents.push(TransitionIntent::ApplyClock {
        card_id: card.id.clone(),
        state: target.to_string(),
        clock: pipeline.clock_for_state(target).cloned(),
    });
    intents.push(TransitionIntent::SyncReviewState {
        card_id: card.id.clone(),
        state: review_state_for(target, pipeline),
    });
    if pipeline.is_terminal(target) {
        intents.push(TransitionIntent::ClearTerminalFields {
            card_id: card.id.clone(),
        });
        intents.push(TransitionIntent::SyncAutoQueue {
            card_id: card.id.clone(),
        });
    }
    intents.push(TransitionIntent::AuditLog {
        card_id: card.id.clone(),
        from: card.status.clone(),
        to: target.to_string(),
        source: source.to_string(),
        message: format_audit_message("OK", force_intent, caller, source),
    });

    TransitionDecision {
        outcome: TransitionOutcome::Allowed,
        intents,
    }
}

/// Public wrapper for pipeline-driven transition decisions.
/// Used by `transition_status_with_opts` after migrating to the reducer pattern.
pub(crate) fn decide_status_transition_with_caller(
    ctx: &TransitionContext,
    target: &str,
    source: &str,
    force_intent: ForceIntent,
    caller: &str,
) -> TransitionDecision {
    decide_pipeline_transition(ctx, target, source, force_intent, caller)
}

#[track_caller]
pub fn decide_status_transition(
    ctx: &TransitionContext,
    target: &str,
    source: &str,
    force_intent: ForceIntent,
) -> TransitionDecision {
    let caller = std::panic::Location::caller();
    let caller = format!("{}:{}", caller.file(), caller.line());
    decide_status_transition_with_caller(ctx, target, source, force_intent, &caller)
}

fn decide_dispatch_attached(
    ctx: &TransitionContext,
    dispatch_id: &str,
    dispatch_type: &str,
    kickoff_state: Option<&str>,
) -> TransitionDecision {
    let card = &ctx.card;
    let is_review_type = matches!(dispatch_type, "review" | "review-decision" | "rework");
    let skip_kickoff = is_review_type || dispatch_type == "consultation";

    let mut intents = vec![];

    // Always set latest_dispatch_id
    intents.push(TransitionIntent::SetLatestDispatchId {
        card_id: card.id.clone(),
        dispatch_id: Some(dispatch_id.to_string()),
    });

    // Non-review and non-consultation dispatches transition to kickoff state.
    // Consultation dispatches stay in requested (side-path, not implementation).
    if !skip_kickoff {
        if let Some(kickoff) = kickoff_state {
            if card.status != kickoff {
                intents.push(TransitionIntent::UpdateStatus {
                    card_id: card.id.clone(),
                    from: card.status.clone(),
                    to: kickoff.to_string(),
                });
                intents.push(TransitionIntent::ApplyClock {
                    card_id: card.id.clone(),
                    state: kickoff.to_string(),
                    clock: ctx.pipeline.clock_for_state(kickoff).cloned(),
                });
            }
        }
    }

    TransitionDecision {
        outcome: TransitionOutcome::Allowed,
        intents,
    }
}

fn decide_redispatch(ctx: &TransitionContext) -> TransitionDecision {
    let card = &ctx.card;
    let mut intents = vec![];

    // Cancel existing dispatch
    if let Some(ref did) = card.latest_dispatch_id {
        intents.push(TransitionIntent::CancelDispatch {
            dispatch_id: did.clone(),
        });
    }

    // Clear review_status and latest_dispatch_id
    intents.push(TransitionIntent::SetReviewStatus {
        card_id: card.id.clone(),
        review_status: None,
    });
    intents.push(TransitionIntent::SetLatestDispatchId {
        card_id: card.id.clone(),
        dispatch_id: None,
    });
    intents.push(TransitionIntent::SyncReviewState {
        card_id: card.id.clone(),
        state: "idle".to_string(),
    });

    TransitionDecision {
        outcome: TransitionOutcome::Allowed,
        intents,
    }
}

fn decide_review_verdict(ctx: &TransitionContext, verdict: &str) -> TransitionDecision {
    let card = &ctx.card;
    let mut intents = vec![];

    match verdict {
        "pass" => {
            // Find terminal state from pipeline
            if let Some(terminal) = ctx.pipeline.states.iter().find(|s| s.terminal) {
                intents.push(TransitionIntent::UpdateStatus {
                    card_id: card.id.clone(),
                    from: card.status.clone(),
                    to: terminal.id.clone(),
                });
                intents.push(TransitionIntent::SyncReviewState {
                    card_id: card.id.clone(),
                    state: "idle".to_string(),
                });
                intents.push(TransitionIntent::ClearTerminalFields {
                    card_id: card.id.clone(),
                });
                intents.push(TransitionIntent::SyncAutoQueue {
                    card_id: card.id.clone(),
                });
                intents.push(TransitionIntent::AuditLog {
                    card_id: card.id.clone(),
                    from: card.status.clone(),
                    to: terminal.id.clone(),
                    source: "review".to_string(),
                    message: "review passed".to_string(),
                });
            }
        }
        "improve" => {
            intents.push(TransitionIntent::SetReviewStatus {
                card_id: card.id.clone(),
                review_status: Some("rework_pending".to_string()),
            });
        }
        _ => {}
    }

    TransitionDecision {
        outcome: TransitionOutcome::Allowed,
        intents,
    }
}

fn decide_review_decision(ctx: &TransitionContext, decision: &str) -> TransitionDecision {
    let card = &ctx.card;
    let mut intents = vec![];

    match decision {
        "accept" => {
            intents.push(TransitionIntent::SyncReviewState {
                card_id: card.id.clone(),
                state: "rework_pending".to_string(),
            });
        }
        "dismiss" => {
            intents.push(TransitionIntent::SyncReviewState {
                card_id: card.id.clone(),
                state: "idle".to_string(),
            });
        }
        "dispute" => {
            intents.push(TransitionIntent::SyncReviewState {
                card_id: card.id.clone(),
                state: "reviewing".to_string(),
            });
        }
        _ => {}
    }

    TransitionDecision {
        outcome: TransitionOutcome::Allowed,
        intents,
    }
}

fn decide_dispatch_completed(ctx: &TransitionContext, _dispatch_id: &str) -> TransitionDecision {
    // Dispatch completion itself doesn't change card status — the hooks do.
    // This event is here for completeness; the actual transition is triggered
    // by the OnDispatchCompleted hook producing a TransitionCard intent.
    let _ = ctx;
    TransitionDecision {
        outcome: TransitionOutcome::NoOp,
        intents: vec![],
    }
}

fn decide_timeout(ctx: &TransitionContext, state: &str) -> TransitionDecision {
    // Timeout handling is managed by the timeout sweep + pipeline config.
    // The reducer acknowledges the event but the actual transition target
    // comes from pipeline.timeouts[state].on_exhaust.
    let card = &ctx.card;
    if card.status != state {
        return TransitionDecision {
            outcome: TransitionOutcome::NoOp,
            intents: vec![],
        };
    }

    // Look up on_exhaust target from pipeline
    if let Some(timeout) = ctx.pipeline.timeouts.get(state) {
        if let Some(ref target) = timeout.on_exhaust {
            return decide_pipeline_transition(
                ctx,
                target,
                "timeout",
                ForceIntent::None,
                "timeout",
            );
        }
    }

    TransitionDecision {
        outcome: TransitionOutcome::NoOp,
        intents: vec![],
    }
}

fn decide_reopen(ctx: &TransitionContext, target: &str) -> TransitionDecision {
    // Reopen is an OperatorOverride variant for terminal→non-terminal.
    decide_operator_override(ctx, target)
}

// ── Helpers ──────────────────────────────────────────────────

/// Determine the canonical review state for a given pipeline status.
fn review_state_for(status: &str, pipeline: &PipelineConfig) -> String {
    if pipeline.is_terminal(status) {
        return "idle".to_string();
    }
    let has_hooks = pipeline
        .hooks_for_state(status)
        .map_or(false, |h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
    if !has_hooks {
        return "idle".to_string();
    }
    let is_review_enter = pipeline
        .hooks_for_state(status)
        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
    if is_review_enter {
        "reviewing".to_string()
    } else {
        "clear_verdict".to_string()
    }
}

fn review_status_for(status: &str, pipeline: &PipelineConfig) -> Option<String> {
    match review_state_for(status, pipeline).as_str() {
        "reviewing" => Some("reviewing".to_string()),
        _ => None,
    }
}

fn format_audit_message(
    base: &str,
    force_intent: ForceIntent,
    caller: &str,
    source: &str,
) -> String {
    let Some(reason) = force_intent.audit_reason(source) else {
        return base.to_string();
    };
    let audit_meta = serde_json::json!({
        "force_intent": force_intent.audit_value(),
        "caller": caller,
        "reason": reason,
    });
    format!("{base} {audit_meta}")
}

// ── Unit tests ───────────────────────────────────────────────

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::pipeline::{
        ClockConfig, GateConfig, HookBindings, PhaseGateConfig, PipelineConfig, StateConfig,
        TransitionConfig,
    };
    use std::collections::HashMap;

    fn test_pipeline() -> PipelineConfig {
        PipelineConfig {
            name: "test".to_string(),
            version: 1,
            states: vec![
                StateConfig {
                    id: "backlog".to_string(),
                    label: "Backlog".to_string(),
                    terminal: false,
                },
                StateConfig {
                    id: "ready".to_string(),
                    label: "Ready".to_string(),
                    terminal: false,
                },
                StateConfig {
                    id: "requested".to_string(),
                    label: "Requested".to_string(),
                    terminal: false,
                },
                StateConfig {
                    id: "in_progress".to_string(),
                    label: "In Progress".to_string(),
                    terminal: false,
                },
                StateConfig {
                    id: "review".to_string(),
                    label: "Review".to_string(),
                    terminal: false,
                },
                StateConfig {
                    id: "done".to_string(),
                    label: "Done".to_string(),
                    terminal: true,
                },
            ],
            transitions: vec![
                TransitionConfig {
                    from: "backlog".to_string(),
                    to: "ready".to_string(),
                    transition_type: TransitionType::Free,
                    gates: vec![],
                },
                TransitionConfig {
                    from: "ready".to_string(),
                    to: "requested".to_string(),
                    transition_type: TransitionType::Free,
                    gates: vec![],
                },
                TransitionConfig {
                    from: "requested".to_string(),
                    to: "in_progress".to_string(),
                    transition_type: TransitionType::Gated,
                    gates: vec!["dispatch_required".to_string()],
                },
                TransitionConfig {
                    from: "in_progress".to_string(),
                    to: "review".to_string(),
                    transition_type: TransitionType::Free,
                    gates: vec![],
                },
                TransitionConfig {
                    from: "review".to_string(),
                    to: "done".to_string(),
                    transition_type: TransitionType::Free,
                    gates: vec![],
                },
                TransitionConfig {
                    from: "review".to_string(),
                    to: "in_progress".to_string(),
                    transition_type: TransitionType::Free,
                    gates: vec![],
                },
            ],
            gates: {
                let mut m = HashMap::new();
                m.insert(
                    "dispatch_required".to_string(),
                    GateConfig {
                        gate_type: "builtin".to_string(),
                        check: Some("has_active_dispatch".to_string()),
                        description: Some("Requires active dispatch".to_string()),
                    },
                );
                m
            },
            hooks: {
                let mut m = HashMap::new();
                m.insert(
                    "review".to_string(),
                    HookBindings {
                        on_enter: vec!["OnReviewEnter".to_string()],
                        on_exit: vec![],
                    },
                );
                m
            },
            events: HashMap::new(),
            clocks: {
                let mut m = HashMap::new();
                m.insert(
                    "in_progress".to_string(),
                    ClockConfig {
                        set: "started_at".to_string(),
                        mode: Some("coalesce".to_string()),
                    },
                );
                m.insert(
                    "done".to_string(),
                    ClockConfig {
                        set: "completed_at".to_string(),
                        mode: None,
                    },
                );
                m
            },
            timeouts: HashMap::new(),
            phase_gate: PhaseGateConfig::default(),
        }
    }

    fn test_ctx(status: &str, has_dispatch: bool) -> TransitionContext {
        TransitionContext {
            card: CardState {
                id: "card-1".to_string(),
                status: status.to_string(),
                review_status: None,
                latest_dispatch_id: if has_dispatch {
                    Some("dispatch-1".to_string())
                } else {
                    None
                },
            },
            pipeline: test_pipeline(),
            gates: GateSnapshot {
                has_active_dispatch: has_dispatch,
                review_verdict_pass: false,
                review_verdict_rework: false,
            },
        }
    }

    // ── Happy path transitions ───────────────────────────────

    #[test]
    fn free_transition_allowed() {
        let ctx = test_ctx("backlog", false);
        let decision = decide_status_transition(&ctx, "ready", "api", ForceIntent::None);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
        assert!(
            decision
                .intents
                .iter()
                .any(|i| matches!(i, TransitionIntent::UpdateStatus { to, .. } if to == "ready"))
        );
    }

    #[test]
    fn gated_transition_allowed_with_dispatch() {
        let ctx = test_ctx("requested", true);
        let decision = decide_status_transition(&ctx, "in_progress", "api", ForceIntent::None);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    #[test]
    fn noop_when_same_status() {
        let ctx = test_ctx("ready", false);
        let decision = decide_status_transition(&ctx, "ready", "api", ForceIntent::None);
        assert_eq!(decision.outcome, TransitionOutcome::NoOp);
        assert!(decision.intents.is_empty());
    }

    // ── Blocked transitions ──────────────────────────────────

    #[test]
    fn terminal_blocks_transition() {
        let ctx = test_ctx("done", false);
        let decision = decide_status_transition(&ctx, "review", "api", ForceIntent::None);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
    }

    #[test]
    fn gated_blocks_without_dispatch() {
        let ctx = test_ctx("requested", false);
        let decision = decide_status_transition(&ctx, "in_progress", "api", ForceIntent::None);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
    }

    #[test]
    fn no_rule_blocks_transition() {
        let ctx = test_ctx("backlog", false);
        let decision = decide_status_transition(&ctx, "done", "api", ForceIntent::None);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
    }

    // ── Force override ───────────────────────────────────────

    #[test]
    fn force_bypasses_terminal_guard() {
        let ctx = test_ctx("done", false);
        let decision =
            decide_status_transition(&ctx, "review", "pmd", ForceIntent::OperatorOverride);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    #[test]
    fn force_bypasses_gate() {
        let ctx = test_ctx("requested", false);
        let decision =
            decide_status_transition(&ctx, "in_progress", "pmd", ForceIntent::OperatorOverride);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    #[test]
    fn force_bypasses_missing_rule() {
        let ctx = test_ctx("backlog", false);
        let decision = decide_status_transition(&ctx, "done", "pmd", ForceIntent::OperatorOverride);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    #[test]
    fn force_intent_none_blocks_ruleless_transition() {
        let ctx = test_ctx("backlog", false);
        let decision = decide_status_transition(&ctx, "done", "api", ForceIntent::None);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
    }

    // ── Terminal state intents ────────────────────────────────

    #[test]
    fn terminal_transition_includes_cleanup() {
        let ctx = test_ctx("review", false);
        let decision = decide_status_transition(&ctx, "done", "api", ForceIntent::None);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
        assert!(
            decision
                .intents
                .iter()
                .any(|i| matches!(i, TransitionIntent::ClearTerminalFields { .. }))
        );
        assert!(
            decision
                .intents
                .iter()
                .any(|i| matches!(i, TransitionIntent::SyncAutoQueue { .. }))
        );
    }

    // ── Review state sync ────────────────────────────────────

    #[test]
    fn review_enter_syncs_reviewing_state() {
        let ctx = test_ctx("in_progress", true);
        let decision = decide_status_transition(&ctx, "review", "api", ForceIntent::None);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
        assert!(decision.intents.iter().any(
            |i| matches!(i, TransitionIntent::SyncReviewState { state, .. } if state == "reviewing")
        ));
    }

    // ── OperatorOverride ─────────────────────────────────────

    #[test]
    fn operator_override_allows_any_transition() {
        let ctx = test_ctx("done", false);
        let decision = decide_transition(
            &ctx,
            &TransitionEvent::OperatorOverride {
                target_status: "backlog".to_string(),
            },
        );
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    #[test]
    fn operator_override_clears_stale_review_status_on_non_review_target() {
        let mut ctx = test_ctx("review", false);
        ctx.card.review_status = Some("reviewing".to_string());
        let decision = decide_transition(
            &ctx,
            &TransitionEvent::OperatorOverride {
                target_status: "requested".to_string(),
            },
        );
        assert!(decision.intents.iter().any(|intent| matches!(
            intent,
            TransitionIntent::SetReviewStatus {
                review_status: None,
                ..
            }
        )));
    }

    #[test]
    fn operator_override_primes_review_status_on_review_target() {
        let ctx = test_ctx("done", false);
        let decision = decide_transition(
            &ctx,
            &TransitionEvent::OperatorOverride {
                target_status: "review".to_string(),
            },
        );
        assert!(decision.intents.iter().any(|intent| matches!(
            intent,
            TransitionIntent::SetReviewStatus {
                review_status: Some(status),
                ..
            } if status == "reviewing"
        )));
    }

    // ── DispatchAttached ─────────────────────────────────────

    #[test]
    fn dispatch_attached_sets_latest_dispatch_id() {
        let ctx = test_ctx("ready", false);
        let decision = decide_transition(
            &ctx,
            &TransitionEvent::DispatchAttached {
                dispatch_id: "d-1".to_string(),
                dispatch_type: "implementation".to_string(),
                kickoff_state: Some("requested".to_string()),
            },
        );
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
        assert!(decision.intents.iter().any(
            |i| matches!(i, TransitionIntent::SetLatestDispatchId { dispatch_id: Some(id), .. } if id == "d-1")
        ));
        assert!(
            decision.intents.iter().any(
                |i| matches!(i, TransitionIntent::UpdateStatus { to, .. } if to == "requested")
            )
        );
    }

    #[test]
    fn review_dispatch_does_not_change_status() {
        let ctx = test_ctx("review", true);
        let decision = decide_transition(
            &ctx,
            &TransitionEvent::DispatchAttached {
                dispatch_id: "d-review".to_string(),
                dispatch_type: "review".to_string(),
                kickoff_state: None,
            },
        );
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
        assert!(
            !decision
                .intents
                .iter()
                .any(|i| matches!(i, TransitionIntent::UpdateStatus { .. }))
        );
    }

    // ── RedispatchRequested ──────────────────────────────────

    #[test]
    fn redispatch_cancels_and_clears() {
        let mut ctx = test_ctx("in_progress", true);
        ctx.card.latest_dispatch_id = Some("old-dispatch".to_string());
        let decision = decide_transition(&ctx, &TransitionEvent::RedispatchRequested);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
        assert!(decision.intents.iter().any(
            |i| matches!(i, TransitionIntent::CancelDispatch { dispatch_id } if dispatch_id == "old-dispatch")
        ));
        assert!(decision.intents.iter().any(|i| matches!(
            i,
            TransitionIntent::SetLatestDispatchId {
                dispatch_id: None,
                ..
            }
        )));
        assert!(decision.intents.iter().any(|i| matches!(
            i,
            TransitionIntent::SetReviewStatus {
                review_status: None,
                ..
            }
        )));
    }

    // ── Audit log always present ─────────────────────────────

    #[test]
    fn blocked_decision_includes_audit_log() {
        let ctx = test_ctx("done", false);
        let decision = decide_status_transition(&ctx, "review", "api", ForceIntent::None);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
        assert!(
            decision
                .intents
                .iter()
                .any(|i| matches!(i, TransitionIntent::AuditLog { .. }))
        );
    }

    /// #821 (6): `decide_pipeline_transition` must refuse a transition that
    /// has no matching pipeline rule unless the caller passes the explicit
    /// `force` flag. This locks the behaviour proven at
    /// `decide_pipeline_transition` around the `None if force { ... }`
    /// branch — without the flag, the `None` arm must return `Blocked` with
    /// a `BLOCKED: no transition rule` audit entry. Companion to the
    /// existing `no_rule_blocks_transition` and `force_bypasses_missing_rule`
    /// tests: this one asserts the audit payload on the blocked path plus
    /// the allowed-with-force symmetric case in a single guard, so a refactor
    /// that relaxes the no-rule check without also flipping the audit must
    /// break this test.
    #[test]
    fn force_transition_without_rule_requires_explicit_flag() {
        // No transition rule exists from `backlog` to `done` in the test
        // pipeline. Without force: blocked + audited as "no transition rule".
        let ctx = test_ctx("backlog", false);
        let blocked = decide_status_transition(&ctx, "done", "api", ForceIntent::None);
        assert!(
            matches!(blocked.outcome, TransitionOutcome::Blocked(_)),
            "no-rule transition must block without force (got {:?})",
            blocked.outcome
        );
        let audited_no_rule = blocked.intents.iter().any(|intent| {
            matches!(
                intent,
                TransitionIntent::AuditLog { message, .. }
                    if message.contains("no transition rule")
            )
        });
        assert!(
            audited_no_rule,
            "blocked no-rule transition must emit a `no transition rule` audit log entry \
             (intents: {:?})",
            blocked.intents
        );
        // Blocked decisions must not produce an UpdateStatus intent.
        assert!(
            !blocked
                .intents
                .iter()
                .any(|intent| matches!(intent, TransitionIntent::UpdateStatus { .. })),
            "blocked no-rule transition must not emit an UpdateStatus intent"
        );

        // With the explicit force flag: allowed, and at least one
        // UpdateStatus intent is emitted. This mirrors the PMD/admin
        // override path (the only legal way to move a card across a
        // missing rule).
        let allowed = decide_status_transition(&ctx, "done", "pmd", ForceIntent::OperatorOverride);
        assert_eq!(
            allowed.outcome,
            TransitionOutcome::Allowed,
            "force flag must unblock the no-rule transition"
        );
        assert!(
            allowed.intents.iter().any(|intent| matches!(
                intent,
                TransitionIntent::UpdateStatus { to, .. } if to == "done"
            )),
            "force-allowed no-rule transition must emit UpdateStatus to the target"
        );
        let forced_audit = allowed.intents.iter().any(|intent| {
            matches!(
                intent,
                TransitionIntent::AuditLog { message, .. }
                    if message.contains("\"force_intent\":\"operator_override\"")
                        && message.contains("\"reason\":\"explicit operator override via pmd\"")
            )
        });
        assert!(
            forced_audit,
            "forced transition must carry force-intent audit metadata (intents: {:?})",
            allowed.intents
        );
    }

    /// #1081: canonical-transition lint.
    ///
    /// The FSM is the single writer of `kanban_cards.status`. Only the FSM
    /// PG executor (`transition_executor_pg.rs`), the
    /// JS-side pipeline-aware raw setter (`ops/kanban_ops.rs`), and tests
    /// may issue a raw `UPDATE kanban_cards SET status`. Every other
    /// production call site must route through
    /// `kanban::transition_status_with_opts[_pg]` or
    /// `transition_executor_pg::execute_pg_transition_intent`.
    ///
    /// This test scans the whole `src/` tree and fails if any file outside
    /// the allowlist contains a raw `UPDATE kanban_cards SET status`
    /// statement. Intended to be cheap (<50ms) and hermetic — no network,
    /// no DB, just filesystem reads.
    #[test]
    fn lint_no_direct_kanban_status_update_in_production() {
        use std::path::{Path, PathBuf};

        // Canonical writers: the FSM executor itself, the PG twin, the
        // JS-side pipeline-aware setter (which carries its own #839
        // migration TODO), and the kanban module that owns this invariant
        // (it declares the pattern in tests and docs).
        //
        // Test files are excluded via the `is_test_context` check below,
        // which skips `tests.rs` / `*_tests.rs` / `integration_tests.rs`
        // and any file that matches test fixture conventions.
        const ALLOWED: &[&str] = &[
            "src/engine/transition.rs",
            "src/engine/transition_executor_pg.rs",
            "src/engine/intent.rs",
            "src/engine/ops/kanban_ops.rs",
            "src/kanban/state_machine.rs",
        ];

        fn is_test_context(path: &Path) -> bool {
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if file_name == "tests.rs"
                || file_name == "integration_tests.rs"
                || file_name.ends_with("_tests.rs")
                || file_name.ends_with("_test.rs")
            {
                return true;
            }
            path.components().any(|c| {
                let seg = c.as_os_str().to_string_lossy();
                seg == "tests" || seg == "__tests__" || seg == "test_support"
            })
        }

        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                    out.push(path);
                }
            }
        }

        let src_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src_root, &mut files);
        assert!(
            !files.is_empty(),
            "lint: found no .rs files under {src_root:?}"
        );

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let needle_variants = ["UPDATE kanban_cards SET status"];

        let mut offenders = Vec::new();
        for file in &files {
            let rel = file.strip_prefix(&manifest).unwrap_or(file);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if ALLOWED.iter().any(|a| rel_str == *a) {
                continue;
            }
            if is_test_context(file) {
                continue;
            }

            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            // Skip any #[cfg(all(test, feature = "legacy-sqlite-tests"))] / #[test] gated sections by scanning
            // line-wise: if a match sits inside a `#[cfg(all(test, feature = "legacy-sqlite-tests"))]` mod or a
            // `#[cfg(all(test, feature = "legacy-sqlite-tests"))]` / `#[test]` annotated item we accept it.
            let mut in_test_cfg = false;
            let mut brace_depth: i32 = 0;
            let mut test_cfg_depth: Option<i32> = None;
            for (idx, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with(r#"#[cfg(all(test, feature = "legacy-sqlite-tests"))]"#)
                    || trimmed.starts_with("#[test]")
                {
                    in_test_cfg = true;
                }
                // Track brace depth so we know when a test-gated block ends.
                for ch in line.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                        if in_test_cfg && test_cfg_depth.is_none() {
                            test_cfg_depth = Some(brace_depth);
                            in_test_cfg = false;
                        }
                    } else if ch == '}' {
                        brace_depth -= 1;
                        if Some(brace_depth) < test_cfg_depth {
                            test_cfg_depth = None;
                        }
                    }
                }
                if test_cfg_depth.is_some() {
                    continue;
                }
                for needle in &needle_variants {
                    if line.contains(needle) {
                        offenders.push(format!("{}:{}: {}", rel_str, idx + 1, line.trim()));
                    }
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "#1081 lint: production code must not issue raw \
             `UPDATE kanban_cards SET status` — route through the FSM \
             executor (`kanban::transition_status_with_opts[_pg]` or \
             `transition_executor_pg::execute_pg_transition_intent`). Offenders:\n  {}",
            offenders.join("\n  ")
        );
    }
}
