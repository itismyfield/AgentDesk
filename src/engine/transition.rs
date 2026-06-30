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
    GateConfig, KNOWN_BUILTIN_GATE_CHECKS, KNOWN_GATE_TYPES, PipelineConfig, TransitionType,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    /// A timeout expired for the current state. `attempt` is the number of
    /// retries already performed for this state (0 on the first expiry); it is
    /// supplied by the caller from the persisted per-card retry budget and lets
    /// `decide_timeout` honor `TimeoutConfig` retry/backoff (#3916).
    TimeoutExpired {
        state: String,
        #[serde(default)]
        attempt: u32,
    },
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
    /// Backoff-aware stage retry decision instead of a transition (#3916).
    /// Emitted by `decide_timeout` when `OnFailurePolicy::RetryWithBackoff`
    /// still has attempts remaining; the card stays in `state`. The current
    /// executor records the decision in the audit trail. NOTE: the actual
    /// re-dispatch + due-time persistence + timeout-clock re-arm + idempotent
    /// suppression of stale retries is the deferred live-wiring follow-up to
    /// #3916 — this intent captures the reducer-level decision it will execute.
    ScheduleStageRetry {
        card_id: String,
        state: String,
        attempt: u32,
        delay_seconds: u64,
    },
}

// ── Shared gate evaluation (#3595) ───────────────────────────

/// Outcome of evaluating a `Gated` transition's gates against a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum GateEvaluation {
    /// Every gate the transition declares evaluated to "pass".
    Allowed,
    /// At least one gate could not be positively evaluated to "pass".
    Blocked {
        /// Name of the gate that blocked the transition. The caller wraps the
        /// final reason as `failed gate '<gate>': <message>` so the offending
        /// gate is always named in the externally-surfaced reason — this is the
        /// pre-#3595 (origin/main) behaviour, preserved byte-for-byte for the
        /// known-builtin "check unsatisfied" path.
        gate: String,
        /// Canonical fail-closed message (already prefixed `BLOCKED: …`). For the
        /// known-builtin unsatisfied path this is the original per-check string
        /// (e.g. `BLOCKED: no active dispatch`) so the audit message stays
        /// identical to origin/main.
        message: String,
    },
}

/// Single source of truth for `Gated` transition gate evaluation (#3595,
/// fail-closed). Given the gate names a transition declares, the effective
/// pipeline's gate definitions, and a pre-collected [`GateSnapshot`], decide
/// whether the transition is permitted.
///
/// This is intentionally a *pure* helper with no I/O: the caller collects the
/// snapshot however it likes (the reducer from a pre-loaded
/// `TransitionContext`) and the decision lives in one place so additional
/// gate-eval paths cannot diverge from it. (#3603 wires the `set_status_raw_pg`
/// path in `engine/ops/kanban_ops.rs` onto this same helper.) A gate the FSM
/// cannot positively confirm as "pass" returns [`GateEvaluation::Blocked`]
/// rather than silently passing through (no `_ => pass` / skip-on-miss).
///
/// The force / admin bypass is the caller's responsibility — this helper is
/// only invoked once a transition is known to be `Gated` and *not* forced.
pub fn evaluate_gates(
    gate_names: &[String],
    gates: &HashMap<String, GateConfig>,
    snapshot: &GateSnapshot,
) -> GateEvaluation {
    for gate_name in gate_names {
        // Point A: gate referenced by the transition is not declared in the
        // gates map. validate() already rejects this for any resolved pipeline,
        // so reaching here means a tampered / out-of-band state — fail closed.
        let Some(gate) = gates.get(gate_name.as_str()) else {
            return GateEvaluation::Blocked {
                gate: gate_name.clone(),
                message: "BLOCKED: unknown/unwired gate".to_string(),
            };
        };

        // Point B: gate type the FSM cannot evaluate. After #3595 `policy` was
        // removed from KNOWN_GATE_TYPES, so the only known type is `builtin`;
        // anything else (including a `policy` override) is blocked here.
        if !KNOWN_GATE_TYPES.contains(&gate.gate_type.as_str()) {
            return GateEvaluation::Blocked {
                gate: gate_name.clone(),
                message: format!("BLOCKED: unsupported type '{}'", gate.gate_type),
            };
        }

        // Point C: builtin gate — the check MUST be one the FSM knows. Every
        // entry of KNOWN_BUILTIN_GATE_CHECKS needs a match arm below (the
        // debug_assert guards against a check being added to the list without a
        // corresponding arm, which would otherwise fail-closed in production).
        match gate.check.as_deref() {
            // Known-builtin checks whose condition is unsatisfied: this is the
            // normal block path and MUST stay behaviour-preserving against
            // origin/main. The `message` is the original per-check string (no
            // gate name) and the caller wraps it as
            // `failed gate '<gate>': <message>`, reproducing the pre-#3595
            // reason byte-for-byte (the gate name lives in the `gate` field).
            Some("has_active_dispatch") => {
                if !snapshot.has_active_dispatch {
                    return GateEvaluation::Blocked {
                        gate: gate_name.clone(),
                        message: "BLOCKED: no active dispatch".to_string(),
                    };
                }
            }
            Some("review_verdict_pass") => {
                if !snapshot.review_verdict_pass {
                    return GateEvaluation::Blocked {
                        gate: gate_name.clone(),
                        message: "BLOCKED: no review pass verdict for current round".to_string(),
                    };
                }
            }
            Some("review_verdict_rework") => {
                if !snapshot.review_verdict_rework {
                    return GateEvaluation::Blocked {
                        gate: gate_name.clone(),
                        message: "BLOCKED: no review rework verdict for current round".to_string(),
                    };
                }
            }
            // Fail-closed cases new in #3595 (no origin/main equivalent). The
            // gate name is carried in the `gate` field and surfaced by the
            // caller's `failed gate '<gate>': …` wrapper, so it is omitted from
            // `message` to avoid duplicating it.
            Some(check) => {
                debug_assert!(
                    !KNOWN_BUILTIN_GATE_CHECKS.contains(&check),
                    "known builtin check '{check}' missing a match arm"
                );
                return GateEvaluation::Blocked {
                    gate: gate_name.clone(),
                    message: format!("BLOCKED: references unknown check '{check}'"),
                };
            }
            None => {
                return GateEvaluation::Blocked {
                    gate: gate_name.clone(),
                    message: "BLOCKED: builtin gate is missing a check".to_string(),
                };
            }
        }
    }

    GateEvaluation::Allowed
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
        TransitionEvent::TimeoutExpired { state, attempt } => {
            super::transition_timeout::decide_timeout(ctx, state, *attempt)
        }
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
pub(crate) fn decide_pipeline_transition(
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
                // Evaluate gates fail-closed (#3595) via the shared
                // `evaluate_gates` helper — the single source of truth for gate
                // evaluation (#3603 routes `set_status_raw_pg` through it too so
                // the paths can never diverge). A gate the FSM cannot positively
                // evaluate to "pass" BLOCKs the transition rather than silently
                // falling through. The forced-`Gated` match arm above already
                // bypasses this.
                if let GateEvaluation::Blocked { gate, message } =
                    evaluate_gates(&t.gates, &pipeline.gates, &ctx.gates)
                {
                    return TransitionDecision {
                        outcome: TransitionOutcome::Blocked(format!(
                            "Status transition {} → {} failed gate '{}': {}",
                            card.status, target, gate, message
                        )),
                        intents: vec![TransitionIntent::AuditLog {
                            card_id: card.id.clone(),
                            from: card.status.clone(),
                            to: target.to_string(),
                            source: source.to_string(),
                            message,
                        }],
                    };
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
    // #3605 (T2): scope-assessment is a side-path that records the issue's scale
    // (scope_depth) before implementation. Like consultation it must stay in
    // `requested` — attaching it must NOT kick the card into the in_progress
    // state, otherwise it would race the real implementation dispatch and break
    // the "side-path before implementation" intent. Shared predicate keeps this
    // in lockstep with dispatch_create / phase_gate.
    let skip_kickoff = crate::dispatch::dispatch_type_skips_kickoff(dispatch_type);

    let mut intents = vec![];

    // Always set latest_dispatch_id. NB: side-paths (consultation,
    // scope-assessment) deliberately become latest_dispatch_id too — exactly
    // like consultation — so the card↔dispatch link is consistent. The
    // protection against a side-path closing the card lives in the terminal-sync
    // guards (dispatch_status / turn_bridge completion), not here.
    intents.push(TransitionIntent::SetLatestDispatchId {
        card_id: card.id.clone(),
        dispatch_id: Some(dispatch_id.to_string()),
    });

    // Review-family and inert side-path dispatches stay in their current state
    // (requested); only real implementation work transitions to kickoff.
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
            other => panic!("expected Blocked, got {other:?}"), // agentdesk-audit: allow-unwrap test-only helper in #[cfg(test)] mod; panics solely on a violated test expectation
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
        assert!(
            reason.contains("failed gate 'ghost'") && reason.contains("unknown/unwired gate"),
            "{reason}"
        );
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

    /// #3595: a `policy` gate has no FSM evaluator. Since `policy` was removed
    /// from KNOWN_GATE_TYPES, it is now an unsupported type and must BLOCK
    /// (fail-closed) rather than passing through un-enforced.
    #[test]
    fn gated_transition_blocks_on_policy_gate() {
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
        let reason = blocked_reason(&decision);
        assert!(reason.contains("unsupported type 'policy'"), "{reason}");
    }

    /// Point C (FIX 3): a builtin gate whose `check` is `None` (missing check)
    /// at runtime must BLOCK — symmetric with the unwired/unsupported/unknown
    /// cases above. (validate() rejects this at write time, but the reducer
    /// must also fail closed if an out-of-band pipeline reaches it.)
    #[test]
    fn gated_transition_blocks_on_builtin_gate_with_no_check() {
        let gate = GateConfig {
            gate_type: "builtin".to_string(),
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
        // The offending gate is named by the caller's `failed gate '<gate>': …`
        // wrapper; the per-case message reports the missing check.
        assert!(reason.contains("failed gate 'g'"), "{reason}");
        assert!(reason.contains("missing a check"), "{reason}");
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
    /// keeps the pre-existing per-check BLOCKED message *and* the origin/main
    /// (pre-#3595) reason format that names the offending gate
    /// (`failed gate '<gate>': <message>`). This is the normal block path and
    /// must stay behaviour-preserving — see #3595 codex review.
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
        // Byte-identical to origin/main: from → to, gate name, and the
        // per-check BLOCKED message.
        assert_eq!(
            reason,
            "Status transition a → b failed gate 'g': BLOCKED: no active dispatch",
        );
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

#[cfg(test)]
mod dispatch_attached_tests {
    //! #3605 (T2): a scope-assessment dispatch is a side-path. Like consultation
    //! it must NOT kick the card into the kickoff (in_progress) state when
    //! attached, so the assessment can run while the card stays in `requested`
    //! and the real implementation dispatch is created separately.
    use super::*;
    use crate::pipeline::{
        PhaseGateConfig, PipelineConfig, StateConfig, TransitionConfig, TransitionType,
    };
    use std::collections::HashMap;

    /// Two-state pipeline `requested` → `in_progress` (in_progress is the kickoff
    /// target for `requested`).
    fn pipeline() -> PipelineConfig {
        PipelineConfig {
            name: "test".to_string(),
            version: 1,
            states: vec![
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
            ],
            transitions: vec![TransitionConfig {
                from: "requested".to_string(),
                to: "in_progress".to_string(),
                transition_type: TransitionType::Free,
                gates: vec![],
            }],
            gates: HashMap::new(),
            hooks: HashMap::new(),
            events: HashMap::new(),
            clocks: HashMap::new(),
            timeouts: HashMap::new(),
            phase_gate: PhaseGateConfig::default(),
        }
    }

    fn ctx_requested() -> TransitionContext {
        TransitionContext {
            card: CardState {
                id: "card-1".to_string(),
                status: "requested".to_string(),
                review_status: None,
                latest_dispatch_id: None,
            },
            pipeline: pipeline(),
            gates: GateSnapshot::default(),
        }
    }

    fn attach(dispatch_type: &str) -> TransitionDecision {
        decide_transition(
            &ctx_requested(),
            &TransitionEvent::DispatchAttached {
                dispatch_id: "d-1".to_string(),
                dispatch_type: dispatch_type.to_string(),
                kickoff_state: Some("in_progress".to_string()),
            },
        )
    }

    fn kicks_to_in_progress(decision: &TransitionDecision) -> bool {
        decision.intents.iter().any(|intent| {
            matches!(
                intent,
                TransitionIntent::UpdateStatus { to, .. } if to == "in_progress"
            )
        })
    }

    /// Control: an implementation dispatch DOES kick the card to in_progress.
    #[test]
    fn implementation_dispatch_kicks_to_in_progress() {
        assert!(kicks_to_in_progress(&attach("implementation")));
    }

    /// #3605: a scope-assessment dispatch must NOT kick the card to in_progress
    /// (skip_kickoff), mirroring consultation.
    #[test]
    fn scope_assessment_dispatch_stays_in_requested() {
        let decision = attach("scope-assessment");
        assert!(
            !kicks_to_in_progress(&decision),
            "scope-assessment must not advance the card to in_progress"
        );
        // It still records the latest dispatch id (shared with all attaches).
        assert!(decision.intents.iter().any(|intent| matches!(
            intent,
            TransitionIntent::SetLatestDispatchId { dispatch_id: Some(id), .. } if id == "d-1"
        )));
    }

    /// Equivalence guard: scope-assessment behaves like consultation here.
    #[test]
    fn consultation_dispatch_stays_in_requested() {
        assert!(!kicks_to_in_progress(&attach("consultation")));
    }
}

#[cfg(test)]
mod timeout_policy_tests {
    //! #3916: `decide_timeout` (the REDUCER) honors the typed `TimeoutConfig`
    //! retry/backoff + `OnFailurePolicy`, and preserves the legacy immediate
    //! `on_exhaust` transition when no typed policy is configured (additive —
    //! default/None unchanged). Pins the reducer-level DoD behaviors:
    //!   (i)   retry-with-backoff schedules retries with the correct attempt
    //!         count + backoff and only transitions once exhausted,
    //!   (ii)  a stage exceeding `TimeoutConfig` (retries exhausted) is aborted,
    //!   (iii) default/None policy = no retry/no timeout effect (unchanged),
    //!   plus escalate / fallback-stage / fail(→terminal) / notify and the
    //!   standalone `on_exhaust_policy` engagement (P1-3 / P1-4).
    //!
    //! These exercise `decide_transition`/`decide_timeout` directly. They do NOT
    //! prove a live-path effect: the production timeout sweep
    //! (policies/timeouts/card-timeouts.js) does not yet emit `TimeoutExpired`,
    //! so routing it through this reducer is the deferred follow-up to #3916.
    use super::*;
    use crate::pipeline::{
        BackoffPolicy, OnExhaustPolicy, OnFailurePolicy, PhaseGateConfig, PipelineConfig,
        StateConfig, TimeoutConfig, TransitionConfig, TransitionType,
    };
    use std::collections::HashMap;

    fn state(id: &str, terminal: bool) -> StateConfig {
        StateConfig {
            id: id.to_string(),
            label: id.to_string(),
            terminal,
        }
    }

    fn free(from: &str, to: &str) -> TransitionConfig {
        TransitionConfig {
            from: from.to_string(),
            to: to.to_string(),
            transition_type: TransitionType::Free,
            gates: vec![],
        }
    }

    /// Pipeline whose `in_progress` state carries the supplied timeout config,
    /// with Free transitions to `escalated` (the `on_exhaust` target) and
    /// `fallback`, plus a terminal `done` state (the `fail` target — reached via
    /// a forced transition, so no explicit rule is needed).
    fn pipeline_with_timeout(timeout: TimeoutConfig) -> PipelineConfig {
        let mut timeouts = HashMap::new();
        timeouts.insert("in_progress".to_string(), timeout);
        PipelineConfig {
            name: "test".to_string(),
            version: 1,
            states: vec![
                state("in_progress", false),
                state("escalated", false),
                state("fallback", false),
                state("done", true),
            ],
            transitions: vec![
                free("in_progress", "escalated"),
                free("in_progress", "fallback"),
            ],
            gates: HashMap::new(),
            hooks: HashMap::new(),
            events: HashMap::new(),
            clocks: HashMap::new(),
            timeouts,
            phase_gate: PhaseGateConfig::default(),
        }
    }

    fn ctx(pipeline: PipelineConfig) -> TransitionContext {
        TransitionContext {
            card: CardState {
                id: "card-1".to_string(),
                status: "in_progress".to_string(),
                review_status: None,
                latest_dispatch_id: None,
            },
            pipeline,
            gates: GateSnapshot::default(),
        }
    }

    /// Legacy-only timeout: a single `on_exhaust` string, no typed fields.
    fn base_timeout() -> TimeoutConfig {
        TimeoutConfig {
            duration: "2h".to_string(),
            clock: "started_at".to_string(),
            max_retries: None,
            on_exhaust: Some("escalated".to_string()),
            on_exhaust_policy: None,
            backoff: None,
            on_failure: None,
            on_failure_target: None,
            condition: None,
        }
    }

    fn timeout_event(attempt: u32) -> TransitionEvent {
        TransitionEvent::TimeoutExpired {
            state: "in_progress".to_string(),
            attempt,
        }
    }

    fn decide(timeout: TimeoutConfig, attempt: u32) -> TransitionDecision {
        decide_transition(
            &ctx(pipeline_with_timeout(timeout)),
            &timeout_event(attempt),
        )
    }

    fn retry(decision: &TransitionDecision) -> Option<(u32, u64)> {
        decision.intents.iter().find_map(|intent| match intent {
            TransitionIntent::ScheduleStageRetry {
                attempt,
                delay_seconds,
                ..
            } => Some((*attempt, *delay_seconds)),
            _ => None,
        })
    }

    fn transitioned_to(decision: &TransitionDecision) -> Option<String> {
        decision.intents.iter().find_map(|intent| match intent {
            TransitionIntent::UpdateStatus { to, .. } => Some(to.clone()),
            _ => None,
        })
    }

    fn retrying() -> TimeoutConfig {
        TimeoutConfig {
            max_retries: Some(3),
            backoff: Some(BackoffPolicy::Exponential),
            on_failure: Some(OnFailurePolicy::RetryWithBackoff),
            on_exhaust_policy: Some(OnExhaustPolicy::Escalate),
            ..base_timeout()
        }
    }

    // (i) retry-with-backoff: fails then retries with the exponential schedule
    // (1m → 5m → 15m), only transitioning once retries are exhausted.
    #[test]
    fn retry_with_backoff_schedules_retries_then_exhausts() {
        // attempt 0 → schedule retry #1 after 60s, NO transition.
        let d0 = decide(retrying(), 0);
        assert_eq!(retry(&d0), Some((1, 60)));
        assert_eq!(transitioned_to(&d0), None, "must retry, not transition");
        // attempt 1 → retry #2 after 300s (exponential observed).
        assert_eq!(retry(&decide(retrying(), 1)), Some((2, 300)));
        // attempt 2 → retry #3 after 900s (capped exponential).
        assert_eq!(retry(&decide(retrying(), 2)), Some((3, 900)));
        // attempt 3 == max_retries → exhausted → escalate transition.
        let d3 = decide(retrying(), 3);
        assert_eq!(retry(&d3), None);
        assert_eq!(transitioned_to(&d3).as_deref(), Some("escalated"));
    }

    // (ii) a stage exceeding TimeoutConfig (retries exhausted) is aborted away
    // from the timed-out state.
    #[test]
    fn timeout_exhausted_aborts_to_on_exhaust_target() {
        let one_retry = TimeoutConfig {
            max_retries: Some(1),
            ..retrying()
        };
        // First expiry retries.
        assert_eq!(retry(&decide(one_retry.clone(), 0)), Some((1, 60)));
        // attempt 1 (== max_retries) → aborted: transition away, no retry.
        let d1 = decide(one_retry, 1);
        assert_eq!(retry(&d1), None);
        assert_eq!(transitioned_to(&d1).as_deref(), Some("escalated"));
        assert!(matches!(d1.outcome, TransitionOutcome::Allowed));
    }

    // (iii) default/None policy = legacy immediate transition, no retry effect.
    #[test]
    fn no_typed_policy_keeps_legacy_immediate_transition() {
        let d = decide(base_timeout(), 0);
        assert_eq!(retry(&d), None, "legacy config must not schedule retries");
        assert_eq!(transitioned_to(&d).as_deref(), Some("escalated"));
    }

    #[test]
    fn no_timeout_config_is_noop() {
        let mut pipeline = pipeline_with_timeout(base_timeout());
        pipeline.timeouts.clear();
        let d = decide_transition(&ctx(pipeline), &timeout_event(0));
        assert!(matches!(d.outcome, TransitionOutcome::NoOp));
        assert!(d.intents.is_empty());
    }

    #[test]
    fn linear_backoff_uses_fixed_delay() {
        let linear = TimeoutConfig {
            backoff: Some(BackoffPolicy::Linear),
            ..retrying()
        };
        assert_eq!(retry(&decide(linear.clone(), 0)), Some((1, 300)));
        assert_eq!(retry(&decide(linear, 1)), Some((2, 300)));
    }

    // OnFailurePolicy honoring: escalate / fallback-stage / fail / notify.
    #[test]
    fn on_failure_escalate_transitions_immediately() {
        let timeout = TimeoutConfig {
            on_failure: Some(OnFailurePolicy::Escalate),
            ..base_timeout()
        };
        let d = decide(timeout, 0);
        assert_eq!(retry(&d), None);
        assert_eq!(transitioned_to(&d).as_deref(), Some("escalated"));
    }

    #[test]
    fn on_failure_fallback_jumps_to_fallback_target() {
        let timeout = TimeoutConfig {
            on_failure: Some(OnFailurePolicy::FallbackStage),
            on_failure_target: Some("fallback".to_string()),
            ..base_timeout()
        };
        assert_eq!(
            transitioned_to(&decide(timeout, 0)).as_deref(),
            Some("fallback")
        );
    }

    // P1-3: explicit `on_failure: fail` TERMINATES the card to a terminal
    // state, not the nonterminal `on_exhaust` target.
    #[test]
    fn on_failure_fail_terminates_to_terminal_state() {
        let timeout = TimeoutConfig {
            on_failure: Some(OnFailurePolicy::Fail),
            ..base_timeout()
        };
        let d = decide(timeout, 0);
        assert_eq!(
            transitioned_to(&d).as_deref(),
            Some("done"),
            "fail must move the card to the terminal state, not on_exhaust"
        );
    }

    // P1-4: a standalone typed `on_exhaust_policy` (no retry fields) engages and
    // applies the exhaust semantics directly — it must NOT fall back to legacy.
    #[test]
    fn standalone_on_exhaust_policy_notify_audits_without_transition() {
        let timeout = TimeoutConfig {
            on_exhaust_policy: Some(OnExhaustPolicy::Notify),
            ..base_timeout()
        };
        let d = decide(timeout, 0);
        assert_eq!(retry(&d), None);
        assert_eq!(
            transitioned_to(&d),
            None,
            "notify must not transition (P1-4 regression: legacy fallback would move to on_exhaust)"
        );
        assert!(matches!(d.outcome, TransitionOutcome::Allowed));
    }

    #[test]
    fn standalone_on_exhaust_policy_fail_terminates() {
        let timeout = TimeoutConfig {
            on_exhaust_policy: Some(OnExhaustPolicy::Fail),
            ..base_timeout()
        };
        assert_eq!(
            transitioned_to(&decide(timeout, 0)).as_deref(),
            Some("done")
        );
    }

    #[test]
    fn standalone_on_exhaust_policy_escalate_transitions_to_on_exhaust() {
        let timeout = TimeoutConfig {
            on_exhaust_policy: Some(OnExhaustPolicy::Escalate),
            ..base_timeout()
        };
        assert_eq!(
            transitioned_to(&decide(timeout, 0)).as_deref(),
            Some("escalated")
        );
    }

    #[test]
    fn notify_on_exhaust_records_audit_without_transition() {
        let timeout = TimeoutConfig {
            max_retries: Some(1),
            on_failure: Some(OnFailurePolicy::RetryWithBackoff),
            on_exhaust_policy: Some(OnExhaustPolicy::Notify),
            ..base_timeout()
        };
        // attempt 1 == max → exhausted → notify (audit, no state change).
        let d = decide(timeout, 1);
        assert_eq!(transitioned_to(&d), None);
        assert!(matches!(d.outcome, TransitionOutcome::Allowed));
        assert!(d.intents.iter().any(|intent| matches!(
            intent,
            TransitionIntent::AuditLog { source, .. } if source == "timeout"
        )));
    }

    #[test]
    fn stale_event_for_other_state_is_noop() {
        let d = decide_transition(
            &ctx(pipeline_with_timeout(retrying())),
            &TransitionEvent::TimeoutExpired {
                state: "escalated".to_string(),
                attempt: 0,
            },
        );
        assert!(matches!(d.outcome, TransitionOutcome::NoOp));
    }
}
