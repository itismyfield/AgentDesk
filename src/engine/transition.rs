//! Kanban state transition reducer (#155).
//!
//! Pure-function `decide_transition` takes a `TransitionContext` and a
//! `TransitionEvent`, and returns a `TransitionDecision` containing the
//! outcome (allowed / blocked) plus an ordered list of `TransitionIntent`s.
//!
//! The Executor (`execute_decision`) applies intents against the database.
//! No direct SQL UPDATEs to `kanban_cards.status`, `review_status`, or
//! `latest_dispatch_id` should happen outside this module.

use crate::pipeline::{
    KNOWN_BUILTIN_GATE_CHECKS, KNOWN_GATE_TYPES, PipelineConfig, TransitionType,
};
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

    if !ctx.pipeline.is_valid_state(target) {
        return TransitionDecision {
            outcome: TransitionOutcome::Blocked(format!(
                "target status '{target}' is not defined in the effective pipeline"
            )),
            intents: vec![TransitionIntent::AuditLog {
                card_id: card.id.clone(),
                from: card.status.clone(),
                to: target.to_string(),
                source: "pmd".to_string(),
                message: "BLOCKED: target status not in effective pipeline".to_string(),
            }],
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

    if !pipeline.is_valid_state(target) {
        return TransitionDecision {
            outcome: TransitionOutcome::Blocked(format!(
                "target status '{target}' is not defined in the effective pipeline"
            )),
            intents: vec![TransitionIntent::AuditLog {
                card_id: card.id.clone(),
                from: card.status.clone(),
                to: target.to_string(),
                source: source.to_string(),
                message: "BLOCKED: target status not in effective pipeline".to_string(),
            }],
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
                // Evaluate gates fail-closed (#3595). A gate that the FSM cannot
                // positively evaluate to "pass" must BLOCK the transition rather
                // than silently fall through (former `_ => None` / skip-on-miss
                // fail-open). The force path above (L321) already bypasses this.
                let block = |msg: &str| TransitionDecision {
                    outcome: TransitionOutcome::Blocked(format!(
                        "Status transition {} → {} failed gate evaluation: {}",
                        card.status, target, msg
                    )),
                    intents: vec![TransitionIntent::AuditLog {
                        card_id: card.id.clone(),
                        from: card.status.clone(),
                        to: target.to_string(),
                        source: source.to_string(),
                        message: msg.to_string(),
                    }],
                };

                for gate_name in &t.gates {
                    // Point A: gate referenced by the transition is not declared
                    // in the gates map. validate() already rejects this for any
                    // resolved pipeline, so reaching here means a tampered/
                    // out-of-band state — fail closed instead of waving it through.
                    let Some(gate) = pipeline.gates.get(gate_name.as_str()) else {
                        return block(&format!("BLOCKED: unknown/unwired gate '{gate_name}'"));
                    };

                    // Point B: gate type the FSM cannot evaluate.
                    if !KNOWN_GATE_TYPES.contains(&gate.gate_type.as_str()) {
                        return block(&format!(
                            "BLOCKED: gate '{}' has unsupported type '{}'",
                            gate_name, gate.gate_type
                        ));
                    }

                    if gate.gate_type != "builtin" {
                        // `policy` is a documented extension point with no FSM
                        // evaluator yet. Pass it through (no policy gates exist in
                        // any shipped pipeline) but warn so it is never silently
                        // assumed to have been enforced.
                        tracing::warn!(
                            card_id = %card.id,
                            gate = %gate_name,
                            gate_type = %gate.gate_type,
                            "gate type has no FSM evaluator; passing through un-enforced"
                        );
                        continue;
                    }

                    // Point C: builtin gate — the check MUST be one the FSM knows.
                    let blocked_msg = match gate.check.as_deref() {
                        Some("has_active_dispatch") => (!ctx.gates.has_active_dispatch)
                            .then_some("BLOCKED: no active dispatch"),
                        Some("review_verdict_pass") => (!ctx.gates.review_verdict_pass)
                            .then_some("BLOCKED: no review pass verdict for current round"),
                        Some("review_verdict_rework") => (!ctx.gates.review_verdict_rework)
                            .then_some("BLOCKED: no review rework verdict for current round"),
                        Some(check) => {
                            debug_assert!(
                                !KNOWN_BUILTIN_GATE_CHECKS.contains(&check),
                                "known builtin check '{check}' missing a match arm"
                            );
                            return block(&format!(
                                "BLOCKED: builtin gate '{gate_name}' references unknown check '{check}'"
                            ));
                        }
                        None => {
                            return block(&format!(
                                "BLOCKED: builtin gate '{gate_name}' is missing a check"
                            ));
                        }
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

#[cfg(test)]
mod gate_fail_closed_tests {
    //! #3595: the `Gated` transition branch must fail closed. A gate the FSM
    //! cannot positively evaluate to "pass" must BLOCK the transition.
    use super::*;
    use crate::pipeline::{
        GateConfig, PhaseGateConfig, PipelineConfig, StateConfig, TransitionConfig, TransitionType,
    };
    use std::collections::HashMap;

    /// Build a 2-state pipeline (`a` → `b`) with a single Gated transition
    /// referencing `gate_names`, and the supplied gate definitions.
    fn pipeline_with_gates(
        gate_names: Vec<&str>,
        gates: Vec<(&str, GateConfig)>,
    ) -> PipelineConfig {
        PipelineConfig {
            name: "test".to_string(),
            version: 1,
            states: vec![
                StateConfig {
                    id: "a".to_string(),
                    label: "A".to_string(),
                    terminal: false,
                },
                StateConfig {
                    id: "b".to_string(),
                    label: "B".to_string(),
                    terminal: false,
                },
            ],
            transitions: vec![TransitionConfig {
                from: "a".to_string(),
                to: "b".to_string(),
                transition_type: TransitionType::Gated,
                gates: gate_names.into_iter().map(str::to_string).collect(),
            }],
            gates: gates
                .into_iter()
                .map(|(name, cfg)| (name.to_string(), cfg))
                .collect(),
            hooks: HashMap::new(),
            events: HashMap::new(),
            clocks: HashMap::new(),
            timeouts: HashMap::new(),
            phase_gate: PhaseGateConfig::default(),
        }
    }

    fn builtin(check: &str) -> GateConfig {
        GateConfig {
            gate_type: "builtin".to_string(),
            check: Some(check.to_string()),
            description: None,
        }
    }

    fn ctx(pipeline: PipelineConfig, gates: GateSnapshot) -> TransitionContext {
        TransitionContext {
            card: CardState {
                id: "card-1".to_string(),
                status: "a".to_string(),
                review_status: None,
                latest_dispatch_id: None,
            },
            pipeline,
            gates,
        }
    }

    fn blocked_reason(decision: &TransitionDecision) -> &str {
        match &decision.outcome {
            TransitionOutcome::Blocked(msg) => msg.as_str(),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    /// Point A: transition references a gate absent from the gates map.
    #[test]
    fn gated_transition_blocks_on_unwired_gate() {
        let pipeline = pipeline_with_gates(vec!["ghost"], vec![]);
        let decision = decide_status_transition(
            &ctx(pipeline, GateSnapshot::default()),
            "b",
            "test",
            ForceIntent::None,
        );
        let reason = blocked_reason(&decision);
        assert!(reason.contains("unknown/unwired gate 'ghost'"), "{reason}");
    }

    /// Point B: gate type the FSM has no evaluator for.
    #[test]
    fn gated_transition_blocks_on_unsupported_gate_type() {
        let gate = GateConfig {
            gate_type: "webhook".to_string(),
            check: None,
            description: None,
        };
        let pipeline = pipeline_with_gates(vec!["g"], vec![("g", gate)]);
        let decision = decide_status_transition(
            &ctx(pipeline, GateSnapshot::default()),
            "b",
            "test",
            ForceIntent::None,
        );
        let reason = blocked_reason(&decision);
        assert!(reason.contains("unsupported type 'webhook'"), "{reason}");
    }

    /// Point C: builtin gate whose check string is unknown to the FSM.
    #[test]
    fn gated_transition_blocks_on_unknown_builtin_check() {
        let gate = GateConfig {
            gate_type: "builtin".to_string(),
            check: Some("bogus".to_string()),
            description: None,
        };
        let pipeline = pipeline_with_gates(vec!["g"], vec![("g", gate)]);
        let decision = decide_status_transition(
            &ctx(pipeline, GateSnapshot::default()),
            "b",
            "test",
            ForceIntent::None,
        );
        let reason = blocked_reason(&decision);
        assert!(reason.contains("unknown check 'bogus'"), "{reason}");
    }

    /// A `policy` gate has no FSM evaluator: it passes through (un-enforced)
    /// rather than blocking, since no shipped pipeline declares one.
    #[test]
    fn gated_transition_passes_through_policy_gate() {
        let gate = GateConfig {
            gate_type: "policy".to_string(),
            check: None,
            description: None,
        };
        let pipeline = pipeline_with_gates(vec!["g"], vec![("g", gate)]);
        let decision = decide_status_transition(
            &ctx(pipeline, GateSnapshot::default()),
            "b",
            "test",
            ForceIntent::None,
        );
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    /// Regression guard: a known builtin check whose condition is satisfied
    /// still allows the transition (normal pass path unchanged).
    #[test]
    fn gated_transition_passes_on_known_check_satisfied() {
        let pipeline = pipeline_with_gates(vec!["g"], vec![("g", builtin("has_active_dispatch"))]);
        let gates = GateSnapshot {
            has_active_dispatch: true,
            ..Default::default()
        };
        let decision =
            decide_status_transition(&ctx(pipeline, gates), "b", "test", ForceIntent::None);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    /// Regression guard: a known builtin check whose condition is NOT satisfied
    /// keeps the pre-existing per-check BLOCKED message (behaviour unchanged).
    #[test]
    fn gated_transition_blocks_on_known_check_unsatisfied() {
        let pipeline = pipeline_with_gates(vec!["g"], vec![("g", builtin("has_active_dispatch"))]);
        let decision = decide_status_transition(
            &ctx(pipeline, GateSnapshot::default()),
            "b",
            "test",
            ForceIntent::None,
        );
        let reason = blocked_reason(&decision);
        assert!(reason.contains("no active dispatch"), "{reason}");
    }

    /// Force override bypasses gate evaluation entirely — even an unwired gate
    /// must not block a forced transition (force path unchanged).
    #[test]
    fn forced_transition_skips_gate_evaluation() {
        let pipeline = pipeline_with_gates(vec!["ghost"], vec![]);
        let decision = decide_status_transition(
            &ctx(pipeline, GateSnapshot::default()),
            "b",
            "test",
            ForceIntent::OperatorOverride,
        );
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }
}
