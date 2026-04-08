use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use super::kanban::{CARD_SELECT, card_row_to_json};

#[derive(Debug, Deserialize)]
pub struct ResumeCardBody {
    /// If true, bypass pipeline gates (force transition)
    pub force: Option<bool>,
    /// Audit log reason
    pub reason: Option<String>,
}

/// POST /api/kanban-cards/:id/resume
///
/// Analyze the card's current state and latest dispatch history, then
/// automatically create the appropriate next dispatch to move the pipeline forward.
/// The `id` path parameter can be a card UUID or a GitHub issue number.
pub async fn resume_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ResumeCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let force = body.force.unwrap_or(false);
    let reason = body.reason.unwrap_or_else(|| "manual resume".to_string());

    // Resolve issue number to card ID if input is numeric
    let id = if id.chars().all(|c| c.is_ascii_digit()) {
        let issue_num: i64 = id.parse().unwrap_or(0);
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        match conn.query_row(
            "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 \
             ORDER BY updated_at DESC LIMIT 1",
            [issue_num],
            |row| row.get::<_, String>(0),
        ) {
            Ok(card_id) => card_id,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("no card found for issue #{id}")})),
                );
            }
        }
    } else {
        id
    };

    // 1. Load card state
    let card_info = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        conn.query_row(
            "SELECT status, review_status, latest_dispatch_id, \
             COALESCE(assigned_agent_id, ''), title, blocked_reason \
             FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .ok()
    };

    let (status, review_status, latest_dispatch_id, agent_id, card_title, _blocked_reason) =
        match card_info {
            Some(info) => info,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found"})),
                );
            }
        };

    // 2. Terminal guard
    {
        crate::pipeline::ensure_loaded();
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        let (repo_id, agent): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or((None, None));
        let effective =
            crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent.as_deref());
        if effective.is_terminal(&status) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "cannot resume terminal card", "status": status})),
            );
        }
    }

    // 3. Check if there's already an active dispatch (card-wide, matching kanban.rs guard)
    if !force {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        let active_dispatch: Option<(String, String)> = conn
            .query_row(
                "SELECT id, status FROM task_dispatches \
                 WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched') \
                 LIMIT 1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        drop(conn);
        if let Some((active_id, active_status)) = active_dispatch {
            return (
                StatusCode::OK,
                Json(json!({
                    "action": "noop",
                    "message": "card already has active dispatch",
                    "dispatch_id": active_id,
                    "dispatch_status": active_status,
                })),
            );
        }
    }

    // 4. No assigned agent → cannot create dispatch
    if agent_id.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "card has no assigned agent — cannot create dispatch"})),
        );
    }

    // 5. Determine resume action based on current state
    let resume_result = determine_and_execute_resume(
        &state,
        &id,
        &status,
        review_status.as_deref(),
        &latest_dispatch_id,
        &agent_id,
        &card_title,
        force,
        &reason,
    )
    .await;

    match resume_result {
        Ok(action) => {
            // Return updated card + action taken
            let conn = match state.db.lock() {
                Ok(c) => c,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{e}")})),
                    );
                }
            };
            match conn.query_row(&format!("{CARD_SELECT} WHERE kc.id = ?1"), [&id], |row| {
                card_row_to_json(row)
            }) {
                Ok(card) => {
                    crate::server::ws::emit_event(
                        &state.broadcast_tx,
                        "kanban_card_updated",
                        card.clone(),
                    );
                    (
                        StatusCode::OK,
                        Json(json!({"card": card, "action": action})),
                    )
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                ),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// Core resume logic: analyze state and execute the appropriate recovery action.
async fn determine_and_execute_resume(
    state: &AppState,
    card_id: &str,
    status: &str,
    review_status: Option<&str>,
    latest_dispatch_id: &Option<String>,
    agent_id: &str,
    card_title: &str,
    force: bool,
    reason: &str,
) -> Result<serde_json::Value, String> {
    // Write audit log
    write_audit_log(&state.db, card_id, status, reason);

    match status {
        "requested" => {
            resume_from_requested(state, card_id, agent_id, card_title, latest_dispatch_id)
        }
        "in_progress" => resume_from_in_progress(state, card_id, latest_dispatch_id),
        "review" => resume_from_review(
            state,
            card_id,
            agent_id,
            card_title,
            review_status,
            latest_dispatch_id,
            force,
        ),
        "pending_decision" => {
            resume_from_pending_decision(state, card_id, agent_id, card_title, force)
        }
        "blocked" => resume_from_blocked(
            state,
            card_id,
            agent_id,
            card_title,
            force,
            latest_dispatch_id,
        ),
        "backlog" | "ready" => {
            resume_from_pre_dispatch(state, card_id, agent_id, card_title, status)
        }
        other => Err(format!("unsupported resume from status '{other}'")),
    }
}

// ── State-specific resume handlers ──────────────────────────────

/// requested: cancel stale dispatch → create new implementation dispatch
fn resume_from_requested(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    _latest_dispatch_id: &Option<String>,
) -> Result<serde_json::Value, String> {
    cancel_and_clear(state, card_id)?;
    let dispatch = create_and_notify(
        state,
        card_id,
        agent_id,
        "implementation",
        card_title,
        &json!({"resume": true, "resumed_from": "requested"}),
    )?;
    Ok(json!({
        "type": "new_implementation_dispatch",
        "dispatch_id": dispatch["id"],
        "from_status": "requested",
    }))
}

/// in_progress (orphan completed): transition to review → OnReviewEnter creates review dispatch
fn resume_from_in_progress(
    state: &AppState,
    card_id: &str,
    latest_dispatch_id: &Option<String>,
) -> Result<serde_json::Value, String> {
    // Check if latest dispatch is actually completed/failed
    let dispatch_status = get_dispatch_status(&state.db, latest_dispatch_id);
    match dispatch_status.as_deref() {
        Some("completed") | Some("failed") | Some("cancelled") | None => {
            // Transition to review via kanban transition
            crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                card_id,
                "review",
                "resume",
                true, // force — in_progress→review is gated
            )
            .map_err(|e| format!("transition to review failed: {e}"))?;

            Ok(json!({
                "type": "transition_to_review",
                "from_status": "in_progress",
                "message": "OnReviewEnter hook will create review dispatch",
            }))
        }
        Some(s) => Err(format!(
            "in_progress card has dispatch in '{s}' status — not stuck"
        )),
    }
}

/// review: depends on review_status sub-state
fn resume_from_review(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    review_status: Option<&str>,
    latest_dispatch_id: &Option<String>,
    force: bool,
) -> Result<serde_json::Value, String> {
    match review_status {
        // reviewing but no active dispatch → create review dispatch
        Some("reviewing") | None => {
            // Cancel all active dispatches first (card-wide check)
            cancel_and_clear(state, card_id)?;

            // Use same agent_id for review dispatch — Rust routing layer
            // handles counter-model via discord_channel_alt (matching OnReviewEnter)
            let conn = state.db.lock().map_err(|e| format!("{e}"))?;
            use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
            execute_intent_on_conn(
                &conn,
                &TransitionIntent::SetReviewStatus {
                    card_id: card_id.to_string(),
                    review_status: Some("reviewing".to_string()),
                },
            )
            .map_err(|e| format!("{e}"))?;
            execute_intent_on_conn(
                &conn,
                &TransitionIntent::SyncReviewState {
                    card_id: card_id.to_string(),
                    state: "reviewing".to_string(),
                },
            )
            .map_err(|e| format!("{e}"))?;
            drop(conn);

            let dispatch = create_and_notify(
                state,
                card_id,
                agent_id,
                "review",
                &format!("[Review] {card_title}"),
                &json!({"resume": true, "resumed_from": "review_reviewing"}),
            )?;

            Ok(json!({
                "type": "new_review_dispatch",
                "dispatch_id": dispatch["id"],
            }))
        }

        // suggestion_pending → auto-accept, create rework dispatch
        Some("suggestion_pending") => {
            cancel_and_clear(state, card_id)?;

            // Get the rework target state from pipeline (same logic as review_verdict.rs)
            let rework_target = {
                let conn = state.db.lock().map_err(|e| format!("{e}"))?;
                let (repo_id, card_agent, card_status_now): (
                    Option<String>,
                    Option<String>,
                    String,
                ) = conn
                    .query_row(
                        "SELECT repo_id, assigned_agent_id, status FROM kanban_cards WHERE id = ?1",
                        [card_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(|e| format!("{e}"))?;
                crate::pipeline::ensure_loaded();
                let effective = crate::pipeline::resolve_for_card(
                    &conn,
                    repo_id.as_deref(),
                    card_agent.as_deref(),
                );
                // Find rework target via review_rework gate
                effective
                    .transitions
                    .iter()
                    .find(|t| {
                        t.from == card_status_now
                            && t.transition_type == crate::pipeline::TransitionType::Gated
                            && t.gates.iter().any(|g| g == "review_rework")
                    })
                    .map(|t| t.to.clone())
                    .unwrap_or_else(|| {
                        effective
                            .dispatchable_states()
                            .first()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "in_progress".to_string())
                    })
            };

            // Transition card to rework target
            crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                card_id,
                &rework_target,
                "resume_auto_accept",
                true,
            )
            .map_err(|e| format!("transition to rework target failed: {e}"))?;

            // Update review status
            {
                let conn = state.db.lock().map_err(|e| format!("{e}"))?;
                use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
                execute_intent_on_conn(
                    &conn,
                    &TransitionIntent::SetReviewStatus {
                        card_id: card_id.to_string(),
                        review_status: Some("rework_pending".to_string()),
                    },
                )
                .map_err(|e| format!("{e}"))?;
                execute_intent_on_conn(
                    &conn,
                    &TransitionIntent::SyncReviewState {
                        card_id: card_id.to_string(),
                        state: "rework_pending".to_string(),
                    },
                )
                .map_err(|e| format!("{e}"))?;
            }

            let dispatch = create_and_notify(
                state,
                card_id,
                agent_id,
                "rework",
                &format!("[Rework] {card_title}"),
                &json!({"resume": true, "resumed_from": "suggestion_pending", "auto_accept": true}),
            )?;

            Ok(json!({
                "type": "auto_accept_rework",
                "dispatch_id": dispatch["id"],
                "from_review_status": "suggestion_pending",
            }))
        }

        // rework_pending with failed/orphan dispatch → new rework dispatch
        Some("rework_pending") => {
            let dispatch_status = get_dispatch_status(&state.db, latest_dispatch_id);
            let is_active = matches!(
                dispatch_status.as_deref(),
                Some("pending") | Some("dispatched")
            );
            if is_active && !force {
                return Err(
                    "rework dispatch may still be active — use force=true to override".to_string(),
                );
            }

            cancel_and_clear(state, card_id)?;
            let dispatch = create_and_notify(
                state,
                card_id,
                agent_id,
                "rework",
                &format!("[Rework] {card_title}"),
                &json!({"resume": true, "resumed_from": "rework_pending"}),
            )?;

            Ok(json!({
                "type": "new_rework_dispatch",
                "dispatch_id": dispatch["id"],
                "from_review_status": "rework_pending",
            }))
        }

        // dilemma_pending / awaiting_dod → escalate
        Some(rs) => Err(format!(
            "review sub-state '{rs}' requires manual intervention (use force=true with pending_decision path)"
        )),
    }
}

/// pending_decision → re-evaluate from dispatch history, create appropriate dispatch
fn resume_from_pending_decision(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    force: bool,
) -> Result<serde_json::Value, String> {
    if !force {
        return Err("pending_decision requires force=true to resume".to_string());
    }

    // Look at the most recent completed dispatch to determine what to resume with
    let (last_dispatch_type, _last_dispatch_id): (Option<String>, Option<String>) = {
        let conn = state.db.lock().map_err(|e| format!("{e}"))?;
        conn.query_row(
            "SELECT dispatch_type, id FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND status IN ('completed', 'failed', 'cancelled') \
             ORDER BY updated_at DESC LIMIT 1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((None, None))
    };

    // Cancel all active dispatches for this card
    cancel_and_clear(state, card_id)?;

    // Clear pending_decision state
    {
        let conn = state.db.lock().map_err(|e| format!("{e}"))?;
        use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
        execute_intent_on_conn(
            &conn,
            &TransitionIntent::SetReviewStatus {
                card_id: card_id.to_string(),
                review_status: None,
            },
        )
        .map_err(|e| format!("{e}"))?;
        execute_intent_on_conn(
            &conn,
            &TransitionIntent::SyncReviewState {
                card_id: card_id.to_string(),
                state: "idle".to_string(),
            },
        )
        .map_err(|e| format!("{e}"))?;
    }

    // Route based on last dispatch type
    match last_dispatch_type.as_deref() {
        // Rework was stuck → compute rework target from pipeline, create rework dispatch
        Some("rework") => {
            let rework_target = {
                let conn = state.db.lock().map_err(|e| format!("{e}"))?;
                let (repo_id, card_agent): (Option<String>, Option<String>) = conn
                    .query_row(
                        "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                        [card_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap_or((None, None));

                // Find what status the card was in before pending_decision
                let prior_status: String = conn
                    .query_row(
                        "SELECT from_status FROM kanban_audit_logs \
                         WHERE card_id = ?1 AND to_status = 'pending_decision' \
                         ORDER BY created_at DESC LIMIT 1",
                        [card_id],
                        |row| row.get(0),
                    )
                    .unwrap_or_else(|_| "review".to_string());

                crate::pipeline::ensure_loaded();
                let effective = crate::pipeline::resolve_for_card(
                    &conn,
                    repo_id.as_deref(),
                    card_agent.as_deref(),
                );
                // Find rework target via review_rework gate, constrained by
                // the status the card was in before pending_decision (matching
                // review_verdict.rs:687-703 t.from == card_status_now pattern)
                effective
                    .transitions
                    .iter()
                    .find(|t| {
                        t.from == prior_status
                            && t.transition_type == crate::pipeline::TransitionType::Gated
                            && t.gates.iter().any(|g| g == "review_rework")
                    })
                    .map(|t| t.to.clone())
                    .unwrap_or_else(|| {
                        effective
                            .dispatchable_states()
                            .first()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "in_progress".to_string())
                    })
            };

            crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                card_id,
                &rework_target,
                "resume_from_pending_decision",
                true,
            )
            .map_err(|e| format!("transition failed: {e}"))?;

            let dispatch = create_and_notify(
                state,
                card_id,
                agent_id,
                "rework",
                &format!("[Rework] {card_title}"),
                &json!({
                    "resume": true,
                    "resumed_from": "pending_decision",
                    "previous_dispatch_type": "rework",
                }),
            )?;

            Ok(json!({
                "type": "resume_rework_from_pending_decision",
                "dispatch_id": dispatch["id"],
                "previous_dispatch_type": "rework",
                "rework_target": rework_target,
            }))
        }

        // Review/review-decision was stuck → back to review flow
        Some("review") | Some("review-decision") => {
            crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                card_id,
                "review",
                "resume_from_pending_decision",
                true,
            )
            .map_err(|e| format!("transition to review failed: {e}"))?;

            Ok(json!({
                "type": "transition_to_review",
                "from_status": "pending_decision",
                "previous_dispatch_type": last_dispatch_type,
                "message": "OnReviewEnter hook will create review dispatch",
            }))
        }

        // Default: back to requested → implementation
        _ => {
            crate::kanban::transition_status_with_opts(
                &state.db,
                &state.engine,
                card_id,
                "requested",
                "resume_from_pending_decision",
                true,
            )
            .map_err(|e| format!("transition from pending_decision failed: {e}"))?;

            let dispatch = create_and_notify(
                state,
                card_id,
                agent_id,
                "implementation",
                card_title,
                &json!({
                    "resume": true,
                    "resumed_from": "pending_decision",
                    "previous_dispatch_type": last_dispatch_type,
                }),
            )?;

            Ok(json!({
                "type": "resume_from_pending_decision",
                "dispatch_id": dispatch["id"],
                "previous_dispatch_type": last_dispatch_type,
            }))
        }
    }
}

/// blocked → transition to requested, create new implementation dispatch
fn resume_from_blocked(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    force: bool,
    _latest_dispatch_id: &Option<String>,
) -> Result<serde_json::Value, String> {
    if !force {
        return Err("blocked requires force=true to resume".to_string());
    }

    // Cancel the actual live dispatch (timeout B transitions to blocked without cancelling)
    cancel_and_clear(state, card_id)?;

    // Clear blocked state
    {
        let conn = state.db.lock().map_err(|e| format!("{e}"))?;
        conn.execute(
            "UPDATE kanban_cards SET blocked_reason = NULL, updated_at = datetime('now') WHERE id = ?1",
            [card_id],
        )
        .map_err(|e| format!("{e}"))?;

        use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
        execute_intent_on_conn(
            &conn,
            &TransitionIntent::SetReviewStatus {
                card_id: card_id.to_string(),
                review_status: None,
            },
        )
        .map_err(|e| format!("{e}"))?;
        execute_intent_on_conn(
            &conn,
            &TransitionIntent::SyncReviewState {
                card_id: card_id.to_string(),
                state: "idle".to_string(),
            },
        )
        .map_err(|e| format!("{e}"))?;
    }

    crate::kanban::transition_status_with_opts(
        &state.db,
        &state.engine,
        card_id,
        "requested",
        "resume_from_blocked",
        true,
    )
    .map_err(|e| format!("transition from blocked failed: {e}"))?;

    let dispatch = create_and_notify(
        state,
        card_id,
        agent_id,
        "implementation",
        card_title,
        &json!({"resume": true, "resumed_from": "blocked"}),
    )?;

    Ok(json!({
        "type": "resume_from_blocked",
        "dispatch_id": dispatch["id"],
    }))
}

/// backlog/ready → create implementation dispatch to advance into requested
fn resume_from_pre_dispatch(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    current_status: &str,
) -> Result<serde_json::Value, String> {
    let dispatch = create_and_notify(
        state,
        card_id,
        agent_id,
        "implementation",
        card_title,
        &json!({"resume": true, "resumed_from": current_status}),
    )?;

    Ok(json!({
        "type": "new_implementation_dispatch",
        "dispatch_id": dispatch["id"],
        "from_status": current_status,
    }))
}

// ── Helpers ─────────────────────────────────────────────────────

/// Cancel ALL active dispatches for the card and clear latest_dispatch_id.
/// Matches the card-wide guard in kanban.rs:101-108.
fn cancel_and_clear(state: &AppState, card_id: &str) -> Result<(), String> {
    let conn = state.db.lock().map_err(|e| format!("{e}"))?;

    // Collect all active dispatch IDs for proper cancellation + auto-queue reset
    let mut stmt = conn
        .prepare(
            "SELECT id FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
        )
        .map_err(|e| format!("{e}"))?;
    let active_ids: Vec<String> = stmt
        .query_map([card_id], |row| row.get(0))
        .map_err(|e| format!("{e}"))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);

    conn.execute_batch("BEGIN").map_err(|e| format!("{e}"))?;

    for did in &active_ids {
        if let Err(e) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
            &conn,
            did,
            Some("resume"),
        ) {
            conn.execute_batch("ROLLBACK").ok();
            return Err(format!("{e}"));
        }
    }

    use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
    if let Err(e) = execute_intent_on_conn(
        &conn,
        &TransitionIntent::SetLatestDispatchId {
            card_id: card_id.to_string(),
            dispatch_id: None,
        },
    ) {
        conn.execute_batch("ROLLBACK").ok();
        return Err(format!("{e}"));
    }

    conn.execute_batch("COMMIT").map_err(|e| {
        conn.execute_batch("ROLLBACK").ok();
        format!("{e}")
    })
}

/// Create a dispatch and queue Discord notification
fn create_and_notify(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let dispatch = crate::dispatch::create_dispatch(
        &state.db,
        &state.engine,
        card_id,
        agent_id,
        dispatch_type,
        title,
        context,
    )
    .map_err(|e| format!("dispatch creation failed: {e}"))?;

    Ok(dispatch)
}

/// Get dispatch status by ID
fn get_dispatch_status(db: &crate::db::Db, dispatch_id: &Option<String>) -> Option<String> {
    let did = dispatch_id.as_ref()?;
    let conn = db.lock().ok()?;
    conn.query_row(
        "SELECT status FROM task_dispatches WHERE id = ?1",
        [did.as_str()],
        |row| row.get(0),
    )
    .ok()
}

/// Write audit log entry for resume action
fn write_audit_log(db: &crate::db::Db, card_id: &str, from_status: &str, reason: &str) {
    if let Ok(conn) = db.lock() {
        conn.execute(
            "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result) \
             VALUES (?1, ?2, ?2, 'resume', ?3)",
            rusqlite::params![card_id, from_status, reason],
        )
        .ok();
    }
}
