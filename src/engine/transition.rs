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
}

/// Everything the pure reducer needs — no DB handle.
#[derive(Debug, Clone)]
pub struct TransitionContext {
    pub card: CardState,
    pub pipeline: PipelineConfig,
    pub gates: GateSnapshot,
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
    if ctx.pipeline.is_terminal(target) {
        intents.push(TransitionIntent::ClearTerminalFields {
            card_id: card.id.clone(),
        });
        intents.push(TransitionIntent::SyncAutoQueue {
            card_id: card.id.clone(),
        });
    }
    intents.push(TransitionIntent::SyncReviewState {
        card_id: card.id.clone(),
        state: review_state_for(target, &ctx.pipeline),
    });
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
    force: bool,
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
    if pipeline.is_terminal(&card.status) && !force {
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
            TransitionType::Gated if force => {}
            TransitionType::Gated => {
                // Evaluate gates
                for gate_name in &t.gates {
                    if let Some(gate) = pipeline.gates.get(gate_name.as_str()) {
                        if gate.gate_type == "builtin" {
                            if let Some("has_active_dispatch") = gate.check.as_deref() {
                                if !ctx.gates.has_active_dispatch {
                                    return TransitionDecision {
                                        outcome: TransitionOutcome::Blocked(format!(
                                            "Status transition {} → {} requires an active dispatch (gate: {})",
                                            card.status, target, gate_name
                                        )),
                                        intents: vec![TransitionIntent::AuditLog {
                                            card_id: card.id.clone(),
                                            from: card.status.clone(),
                                            to: target.to_string(),
                                            source: source.to_string(),
                                            message: "BLOCKED: no active dispatch".to_string(),
                                        }],
                                    };
                                }
                            }
                        }
                    }
                }
            }
            TransitionType::ForceOnly if force => {}
            TransitionType::ForceOnly => {
                return TransitionDecision {
                    outcome: TransitionOutcome::Blocked(format!(
                        "Transition {} → {} requires force=true (PMD/policy only)",
                        card.status, target
                    )),
                    intents: vec![TransitionIntent::AuditLog {
                        card_id: card.id.clone(),
                        from: card.status.clone(),
                        to: target.to_string(),
                        source: source.to_string(),
                        message: "BLOCKED: force required".to_string(),
                    }],
                };
            }
        },
        None if force => {}
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
    if pipeline.is_terminal(target) {
        intents.push(TransitionIntent::ClearTerminalFields {
            card_id: card.id.clone(),
        });
        intents.push(TransitionIntent::SyncAutoQueue {
            card_id: card.id.clone(),
        });
    }
    intents.push(TransitionIntent::SyncReviewState {
        card_id: card.id.clone(),
        state: review_state_for(target, pipeline),
    });
    intents.push(TransitionIntent::AuditLog {
        card_id: card.id.clone(),
        from: card.status.clone(),
        to: target.to_string(),
        source: source.to_string(),
        message: "OK".to_string(),
    });

    TransitionDecision {
        outcome: TransitionOutcome::Allowed,
        intents,
    }
}

/// Public wrapper for pipeline-driven transition decisions.
/// Used by `transition_status_with_opts` after migrating to the reducer pattern.
pub fn decide_status_transition(
    ctx: &TransitionContext,
    target: &str,
    source: &str,
    force: bool,
) -> TransitionDecision {
    decide_pipeline_transition(ctx, target, source, force)
}

fn decide_dispatch_attached(
    ctx: &TransitionContext,
    dispatch_id: &str,
    dispatch_type: &str,
    kickoff_state: Option<&str>,
) -> TransitionDecision {
    let card = &ctx.card;
    let is_review_type = matches!(dispatch_type, "review" | "review-decision" | "rework");

    let mut intents = vec![];

    // Always set latest_dispatch_id
    intents.push(TransitionIntent::SetLatestDispatchId {
        card_id: card.id.clone(),
        dispatch_id: Some(dispatch_id.to_string()),
    });

    // Non-review dispatches transition to kickoff state
    if !is_review_type {
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
                intents.push(TransitionIntent::ClearTerminalFields {
                    card_id: card.id.clone(),
                });
                intents.push(TransitionIntent::SyncAutoQueue {
                    card_id: card.id.clone(),
                });
                intents.push(TransitionIntent::SyncReviewState {
                    card_id: card.id.clone(),
                    state: "idle".to_string(),
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
            return decide_pipeline_transition(ctx, target, "timeout", false);
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

// ── Executor ─────────────────────────────────────────────────

/// Execute a `TransitionDecision` against the database.
///
/// Returns `Ok(true)` if the decision was Allowed and intents executed,
/// `Ok(false)` if NoOp, and `Err` if Blocked.
pub fn execute_decision(db: &crate::db::Db, decision: &TransitionDecision) -> anyhow::Result<bool> {
    match &decision.outcome {
        TransitionOutcome::Blocked(reason) => {
            // Execute audit log intents even for blocked decisions
            let conn = db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            for intent in &decision.intents {
                if let TransitionIntent::AuditLog {
                    card_id,
                    from,
                    to,
                    source,
                    message,
                } = intent
                {
                    execute_audit_log(&conn, card_id, from, to, source, message);
                }
            }
            Err(anyhow::anyhow!("{}", reason))
        }
        TransitionOutcome::NoOp => Ok(false),
        TransitionOutcome::Allowed => {
            let conn = db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            conn.execute_batch("BEGIN")
                .map_err(|e| anyhow::anyhow!("BEGIN: {e}"))?;
            let exec_result = (|| -> anyhow::Result<()> {
                for intent in &decision.intents {
                    execute_intent(&conn, intent)?;
                }
                Ok(())
            })();
            if let Err(e) = exec_result {
                conn.execute_batch("ROLLBACK").ok();
                return Err(e);
            }
            conn.execute_batch("COMMIT")
                .map_err(|e| anyhow::anyhow!("COMMIT: {e}"))?;
            Ok(true)
        }
    }
}

/// Execute a single intent against an already-locked connection.
///
/// Public for use by callers that manage their own DB lock (e.g., `kanban.rs`).
pub fn execute_intent_on_conn(
    conn: &rusqlite::Connection,
    intent: &TransitionIntent,
) -> anyhow::Result<()> {
    execute_intent(conn, intent)
}

fn execute_intent(conn: &rusqlite::Connection, intent: &TransitionIntent) -> anyhow::Result<()> {
    match intent {
        TransitionIntent::UpdateStatus {
            card_id,
            from: _,
            to,
        } => {
            conn.execute(
                "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![to, card_id],
            )?;
        }
        TransitionIntent::SetLatestDispatchId {
            card_id,
            dispatch_id,
        } => {
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![dispatch_id, card_id],
            )?;
        }
        TransitionIntent::SetReviewStatus {
            card_id,
            review_status,
        } => {
            conn.execute(
                "UPDATE kanban_cards SET review_status = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![review_status, card_id],
            )?;
        }
        TransitionIntent::ApplyClock { card_id, clock, .. } => {
            if let Some(clock) = clock {
                let clock_sql = if clock.mode.as_deref() == Some("coalesce") {
                    format!(
                        "UPDATE kanban_cards SET {} = COALESCE({}, datetime('now')), updated_at = datetime('now') WHERE id = ?1",
                        clock.set, clock.set
                    )
                } else {
                    format!(
                        "UPDATE kanban_cards SET {} = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
                        clock.set
                    )
                };
                conn.execute(&clock_sql, [card_id])?;
            }
        }
        TransitionIntent::ClearTerminalFields { card_id } => {
            conn.execute(
                "UPDATE kanban_cards SET review_status = NULL, suggestion_pending_at = NULL, \
                 review_entered_at = NULL, awaiting_dod_at = NULL, updated_at = datetime('now') WHERE id = ?1",
                [card_id],
            )?;
        }
        TransitionIntent::SyncAutoQueue { card_id } => {
            conn.execute(
                "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') \
                 WHERE kanban_card_id = ?1 AND status = 'dispatched'",
                [card_id],
            )
            .ok();
        }
        TransitionIntent::SyncReviewState { card_id, state } => {
            // #158: Route all card_review_state mutations through unified entrypoint.
            // "clear_verdict" is a synthetic state that only clears last_verdict on work re-entry.
            if state == "clear_verdict" {
                conn.execute(
                    "UPDATE card_review_state SET last_verdict = NULL, updated_at = datetime('now') WHERE card_id = ?1",
                    [card_id],
                ).ok();
            } else {
                crate::engine::ops::review_state_sync_on_conn(
                    conn,
                    &serde_json::json!({"card_id": card_id, "state": state}).to_string(),
                );
            }
        }
        TransitionIntent::AuditLog {
            card_id,
            from,
            to,
            source,
            message,
        } => {
            execute_audit_log(conn, card_id, from, to, source, message);
        }
        TransitionIntent::CancelDispatch { dispatch_id } => {
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(conn, dispatch_id, None)
                .ok();
        }
    }
    Ok(())
}

fn execute_audit_log(
    conn: &rusqlite::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    message: &str,
) {
    // Use the canonical table `kanban_audit_logs` (plural) with column `result`,
    // matching the existing schema in kanban.rs (#155 review fix).
    if let Err(e) = conn.execute(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![card_id, from, to, source, message],
    ) {
        tracing::warn!("[transition] audit_logs insert failed: {e}");
    }
    // Dual-write to audit_logs for parity with kanban.rs::log_audit.
    conn.execute(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor) \
         VALUES ('kanban_card', ?1, ?2, ?3)",
        rusqlite::params![card_id, format!("{from}->{to} ({message})"), source],
    )
    .ok();
}

// ── Unit tests ───────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{
        ClockConfig, GateConfig, HookBindings, PipelineConfig, StateConfig, TransitionConfig,
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
            },
        }
    }

    // ── Happy path transitions ───────────────────────────────

    #[test]
    fn free_transition_allowed() {
        let ctx = test_ctx("backlog", false);
        let decision = decide_status_transition(&ctx, "ready", "api", false);
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
        let decision = decide_status_transition(&ctx, "in_progress", "api", false);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    #[test]
    fn noop_when_same_status() {
        let ctx = test_ctx("ready", false);
        let decision = decide_status_transition(&ctx, "ready", "api", false);
        assert_eq!(decision.outcome, TransitionOutcome::NoOp);
        assert!(decision.intents.is_empty());
    }

    // ── Blocked transitions ──────────────────────────────────

    #[test]
    fn terminal_blocks_transition() {
        let ctx = test_ctx("done", false);
        let decision = decide_status_transition(&ctx, "review", "api", false);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
    }

    #[test]
    fn gated_blocks_without_dispatch() {
        let ctx = test_ctx("requested", false);
        let decision = decide_status_transition(&ctx, "in_progress", "api", false);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
    }

    #[test]
    fn no_rule_blocks_transition() {
        let ctx = test_ctx("backlog", false);
        let decision = decide_status_transition(&ctx, "done", "api", false);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
    }

    // ── Force override ───────────────────────────────────────

    #[test]
    fn force_bypasses_terminal_guard() {
        let ctx = test_ctx("done", false);
        let decision = decide_status_transition(&ctx, "review", "pmd", true);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    #[test]
    fn force_bypasses_gate() {
        let ctx = test_ctx("requested", false);
        let decision = decide_status_transition(&ctx, "in_progress", "pmd", true);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    #[test]
    fn force_bypasses_missing_rule() {
        let ctx = test_ctx("backlog", false);
        let decision = decide_status_transition(&ctx, "done", "pmd", true);
        assert_eq!(decision.outcome, TransitionOutcome::Allowed);
    }

    // ── Terminal state intents ────────────────────────────────

    #[test]
    fn terminal_transition_includes_cleanup() {
        let ctx = test_ctx("review", false);
        let decision = decide_status_transition(&ctx, "done", "api", false);
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
        let decision = decide_status_transition(&ctx, "review", "api", false);
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
        let decision = decide_status_transition(&ctx, "review", "api", false);
        assert!(matches!(decision.outcome, TransitionOutcome::Blocked(_)));
        assert!(
            decision
                .intents
                .iter()
                .any(|i| matches!(i, TransitionIntent::AuditLog { .. }))
        );
    }

    // ── #158: SyncReviewState executor routes through unified entrypoint ──

    fn test_db() -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn insert_test_card(conn: &rusqlite::Connection, card_id: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Test', '111')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES (?1, 'Test', 'in_progress', 'agent-1', datetime('now'), datetime('now'))",
            [card_id],
        ).unwrap();
    }

    #[test]
    fn execute_sync_review_state_idle() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_test_card(&conn, "card-exec-1");
        execute_intent(
            &conn,
            &TransitionIntent::SyncReviewState {
                card_id: "card-exec-1".to_string(),
                state: "idle".to_string(),
            },
        )
        .unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM card_review_state WHERE card_id = 'card-exec-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "idle");
    }

    #[test]
    fn execute_sync_review_state_reviewing() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_test_card(&conn, "card-exec-2");
        execute_intent(
            &conn,
            &TransitionIntent::SyncReviewState {
                card_id: "card-exec-2".to_string(),
                state: "reviewing".to_string(),
            },
        )
        .unwrap();
        let (state, entered): (String, String) = conn
            .query_row(
                "SELECT state, review_entered_at FROM card_review_state WHERE card_id = 'card-exec-2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "reviewing");
        assert!(!entered.is_empty(), "review_entered_at should be set");
    }

    #[test]
    fn execute_sync_review_state_clear_verdict() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_test_card(&conn, "card-exec-3");
        // Set up state with a verdict
        crate::engine::ops::review_state_sync_on_conn(
            &conn,
            &serde_json::json!({
                "card_id": "card-exec-3",
                "state": "suggestion_pending",
                "last_verdict": "improve",
            })
            .to_string(),
        );

        // Clear verdict
        execute_intent(
            &conn,
            &TransitionIntent::SyncReviewState {
                card_id: "card-exec-3".to_string(),
                state: "clear_verdict".to_string(),
            },
        )
        .unwrap();

        let verdict: Option<String> = conn
            .query_row(
                "SELECT last_verdict FROM card_review_state WHERE card_id = 'card-exec-3'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            verdict.is_none(),
            "clear_verdict must set last_verdict to NULL"
        );
    }
}
