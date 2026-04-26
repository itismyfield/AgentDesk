use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

#[derive(Debug)]
struct ResumeCardSnapshot {
    status: String,
    review_status: Option<String>,
    latest_dispatch_id: Option<String>,
    assigned_agent_id: Option<String>,
    title: String,
    blocked_reason: Option<String>,
    repo_id: Option<String>,
}

fn required_pg_pool(state: &AppState) -> Result<&sqlx::PgPool, String> {
    state
        .pg_pool_ref()
        .ok_or_else(|| "postgres pool unavailable".to_string())
}

fn transition_status_pg(
    state: &AppState,
    card_id: &str,
    new_status: &str,
    source: &str,
) -> Result<(), String> {
    let pool = required_pg_pool(state)?;
    let engine = state.engine.clone();
    let card_id = card_id.to_string();
    let new_status = new_status.to_string();
    let source = source.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            crate::kanban::transition_status_with_opts_pg_only(
                &bridge_pool,
                &engine,
                &card_id,
                &new_status,
                &source,
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .await
            .map(|_| ())
            .map_err(|error| format!("{error}"))
        },
        |error| error,
    )
}

fn resolve_resume_card_id_pg_first(
    state: &AppState,
    raw_id: &str,
) -> Result<Option<String>, String> {
    if !raw_id.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Some(raw_id.to_string()));
    }

    let issue_num: i64 = raw_id.parse().unwrap_or(0);
    let pool = required_pg_pool(state)?;
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT id
                 FROM kanban_cards
                 WHERE github_issue_number = $1
                 ORDER BY updated_at DESC
                 LIMIT 1",
            )
            .bind(issue_num)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("lookup postgres card for issue #{issue_num}: {error}"))
        },
        |error| error,
    )
}

fn load_resume_card_snapshot_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<Option<ResumeCardSnapshot>, String> {
    let pool = required_pg_pool(state)?;
    let card_id_owned = card_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let row = sqlx::query_as::<
                _,
                (
                    String,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    String,
                    Option<String>,
                    Option<String>,
                ),
            >(
                "SELECT
                    status,
                    review_status,
                    latest_dispatch_id,
                    assigned_agent_id,
                    title,
                    blocked_reason,
                    repo_id
                 FROM kanban_cards
                 WHERE id = $1",
            )
            .bind(&card_id_owned)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres resume card {card_id_owned}: {error}"))?;

            Ok(row.map(
                |(
                    status,
                    review_status,
                    latest_dispatch_id,
                    assigned_agent_id,
                    title,
                    blocked_reason,
                    repo_id,
                )| ResumeCardSnapshot {
                    status,
                    review_status,
                    latest_dispatch_id,
                    assigned_agent_id,
                    title,
                    blocked_reason,
                    repo_id,
                },
            ))
        },
        |error| error,
    )
}

fn resolve_effective_pipeline_pg_first(
    state: &AppState,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig, String> {
    crate::pipeline::ensure_loaded();

    let pool = required_pg_pool(state)?;
    let repo_id_owned = repo_id.map(ToString::to_string);
    let agent_id_owned = agent_id.map(ToString::to_string);
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            Ok(crate::pipeline::resolve_for_card_pg(
                &bridge_pool,
                repo_id_owned.as_deref(),
                agent_id_owned.as_deref(),
            )
            .await)
        },
        |error| error,
    )
}

fn load_active_dispatch_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<Option<(String, String)>, String> {
    let pool = required_pg_pool(state)?;
    let card_id_owned = card_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query_as::<_, (String, String)>(
                "SELECT id, status
                 FROM task_dispatches
                 WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')
                 LIMIT 1",
            )
            .bind(&card_id_owned)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres active dispatch for {card_id_owned}: {error}"))
        },
        |error| error,
    )
}

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
    let requested_id = id.clone();
    let id = match resolve_resume_card_id_pg_first(&state, &id) {
        Ok(Some(card_id)) => card_id,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("no card found for issue #{requested_id}")})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };

    // 1. Load card state
    let card = match load_resume_card_snapshot_pg_first(&state, &id) {
        Ok(Some(card)) => card,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "card not found"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };
    let status = card.status.clone();
    let review_status = card.review_status.clone();
    let latest_dispatch_id = card.latest_dispatch_id.clone();
    let agent_id = card.assigned_agent_id.clone().unwrap_or_default();
    let card_title = card.title.clone();
    let blocked_reason = card.blocked_reason.clone();

    // 2. Terminal guard
    let effective = match resolve_effective_pipeline_pg_first(
        &state,
        card.repo_id.as_deref(),
        card.assigned_agent_id.as_deref(),
    ) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };
    if effective.is_terminal(&status) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "cannot resume terminal card", "status": status})),
        );
    }

    // 3. Check if there's already an active dispatch (card-wide, matching kanban.rs guard)
    if !force {
        let active_dispatch = match load_active_dispatch_pg_first(&state, &id) {
            Ok(dispatch) => dispatch,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };
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
        blocked_reason.as_deref(),
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
            let Some(pool) = state.pg_pool_ref() else {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "postgres pool unavailable"})),
                );
            };
            match super::kanban::load_card_json_pg(pool, &id).await {
                Ok(Some(card)) => {
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
                Ok(None) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "card not found after resume"})),
                ),
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
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
    blocked_reason: Option<&str>,
    latest_dispatch_id: &Option<String>,
    agent_id: &str,
    card_title: &str,
    force: bool,
    reason: &str,
) -> Result<serde_json::Value, String> {
    // Write audit log
    write_audit_log(state, card_id, status, reason);

    match status {
        "requested" => {
            if crate::manual_intervention::requires_manual_intervention(
                review_status,
                blocked_reason,
            ) {
                resume_from_requested_manual_intervention(
                    state, card_id, agent_id, card_title, force,
                )
            } else {
                resume_from_requested(state, card_id, agent_id, card_title, latest_dispatch_id)
            }
        }
        "in_progress" => {
            if crate::manual_intervention::requires_manual_intervention(
                review_status,
                blocked_reason,
            ) {
                resume_from_in_progress_manual_intervention(
                    state, card_id, agent_id, card_title, force,
                )
            } else {
                resume_from_in_progress(state, card_id, latest_dispatch_id)
            }
        }
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
            resume_from_pending_decision_legacy(state, card_id, agent_id, card_title, force)
        }
        "blocked" => resume_from_blocked_legacy(
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

/// requested + manual intervention → clear markers, create new implementation dispatch
fn resume_from_requested_manual_intervention(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    force: bool,
) -> Result<serde_json::Value, String> {
    if !force {
        return Err("requested manual intervention requires force=true to resume".to_string());
    }

    cancel_and_clear(state, card_id)?;
    clear_manual_intervention_markers(state, card_id)?;

    let dispatch = create_and_notify(
        state,
        card_id,
        agent_id,
        "implementation",
        card_title,
        &json!({"resume": true, "resumed_from": "requested_manual_intervention"}),
    )?;

    Ok(json!({
        "type": "resume_requested_manual_intervention",
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
    let dispatch_status = get_dispatch_status(state, latest_dispatch_id);
    match dispatch_status.as_deref() {
        Some("completed") | Some("failed") | Some("cancelled") | None => {
            // Transition to review via kanban transition
            transition_status_pg(state, card_id, "review", "resume")
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

/// in_progress + manual intervention → clear markers, re-request implementation
fn resume_from_in_progress_manual_intervention(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    force: bool,
) -> Result<serde_json::Value, String> {
    if !force {
        return Err("in_progress manual intervention requires force=true to resume".to_string());
    }

    cancel_and_clear(state, card_id)?;
    clear_manual_intervention_markers(state, card_id)?;

    transition_status_pg(
        state,
        card_id,
        "requested",
        "resume_from_in_progress_manual_intervention",
    )
    .map_err(|e| format!("transition from in_progress manual intervention failed: {e}"))?;

    let dispatch = create_and_notify(
        state,
        card_id,
        agent_id,
        "implementation",
        card_title,
        &json!({"resume": true, "resumed_from": "in_progress_manual_intervention"}),
    )?;

    Ok(json!({
        "type": "resume_in_progress_manual_intervention",
        "dispatch_id": dispatch["id"],
        "from_status": "in_progress",
    }))
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
            sync_review_state(state, card_id, Some("reviewing"), "reviewing")?;

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
                let card = load_resume_card_snapshot_pg_first(state, card_id)?
                    .ok_or_else(|| format!("card not found: {card_id}"))?;
                let effective = resolve_effective_pipeline_pg_first(
                    state,
                    card.repo_id.as_deref(),
                    card.assigned_agent_id.as_deref(),
                )?;
                // Find rework target via review_rework gate
                effective
                    .transitions
                    .iter()
                    .find(|t| {
                        t.from == card.status
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
            transition_status_pg(state, card_id, &rework_target, "resume_auto_accept")
                .map_err(|e| format!("transition to rework target failed: {e}"))?;

            // Update review status
            sync_review_state(state, card_id, Some("rework_pending"), "rework_pending")?;

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
            let dispatch_status = get_dispatch_status(state, latest_dispatch_id);
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
        Some("dilemma_pending") => {
            resume_from_review_dilemma_pending(state, card_id, agent_id, card_title, force)
        }

        Some(rs) => Err(format!(
            "review sub-state '{rs}' is not resumable via /resume"
        )),
    }
}

/// review + dilemma_pending → clear manual intervention and restart the appropriate flow
fn resume_from_review_dilemma_pending(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    force: bool,
) -> Result<serde_json::Value, String> {
    if !force {
        return Err("review dilemma_pending requires force=true to resume".to_string());
    }

    let last_dispatch_type = get_last_terminal_dispatch_type(state, card_id);

    cancel_and_clear(state, card_id)?;
    clear_manual_intervention_markers(state, card_id)?;

    match last_dispatch_type.as_deref() {
        Some("rework") => {
            let rework_target =
                resolve_rework_resume_target(state, card_id, "review", Some("pending_decision"))?;
            transition_status_pg(
                state,
                card_id,
                &rework_target,
                "resume_from_review_dilemma_pending",
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
                    "resumed_from": "review_dilemma_pending",
                    "previous_dispatch_type": "rework",
                }),
            )?;

            Ok(json!({
                "type": "resume_rework_from_review_dilemma_pending",
                "dispatch_id": dispatch["id"],
                "previous_dispatch_type": "rework",
                "rework_target": rework_target,
            }))
        }
        Some("review") | Some("review-decision") => {
            sync_review_state(state, card_id, Some("reviewing"), "reviewing")?;

            let dispatch = create_and_notify(
                state,
                card_id,
                agent_id,
                "review",
                &format!("[Review] {card_title}"),
                &json!({
                    "resume": true,
                    "resumed_from": "review_dilemma_pending",
                    "previous_dispatch_type": last_dispatch_type,
                }),
            )?;

            Ok(json!({
                "type": "resume_review_from_dilemma_pending",
                "dispatch_id": dispatch["id"],
                "previous_dispatch_type": last_dispatch_type,
            }))
        }
        _ => {
            transition_status_pg(
                state,
                card_id,
                "requested",
                "resume_from_review_dilemma_pending",
            )
            .map_err(|e| format!("transition to requested failed: {e}"))?;

            let dispatch = create_and_notify(
                state,
                card_id,
                agent_id,
                "implementation",
                card_title,
                &json!({
                    "resume": true,
                    "resumed_from": "review_dilemma_pending",
                    "previous_dispatch_type": last_dispatch_type,
                }),
            )?;

            Ok(json!({
                "type": "resume_from_review_dilemma_pending",
                "dispatch_id": dispatch["id"],
                "previous_dispatch_type": last_dispatch_type,
            }))
        }
    }
}

/// Legacy pending_decision → re-evaluate from dispatch history, create appropriate dispatch
fn resume_from_pending_decision_legacy(
    state: &AppState,
    card_id: &str,
    agent_id: &str,
    card_title: &str,
    force: bool,
) -> Result<serde_json::Value, String> {
    if !force {
        return Err("pending_decision requires force=true to resume".to_string());
    }

    let last_dispatch_type = get_last_terminal_dispatch_type(state, card_id);

    // Cancel all active dispatches for this card
    cancel_and_clear(state, card_id)?;

    // Clear pending_decision state
    clear_manual_intervention_markers(state, card_id)?;

    // Route based on last dispatch type
    match last_dispatch_type.as_deref() {
        // Rework was stuck → compute rework target from pipeline, create rework dispatch
        Some("rework") => {
            let rework_target =
                resolve_rework_resume_target(state, card_id, "review", Some("pending_decision"))?;

            transition_status_pg(
                state,
                card_id,
                &rework_target,
                "resume_from_pending_decision",
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
            transition_status_pg(state, card_id, "review", "resume_from_pending_decision")
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
            transition_status_pg(state, card_id, "requested", "resume_from_pending_decision")
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

/// Legacy blocked → transition to requested, create new implementation dispatch
fn resume_from_blocked_legacy(
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
    clear_manual_intervention_markers(state, card_id)?;

    transition_status_pg(state, card_id, "requested", "resume_from_blocked")
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
    let pool = required_pg_pool(state)?;
    let card_id_owned = card_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let mut tx = bridge_pool.begin().await.map_err(|error| {
                format!("begin postgres resume cleanup tx for {card_id_owned}: {error}")
            })?;

            let active_ids: Vec<String> = sqlx::query_scalar(
                "SELECT id
                 FROM task_dispatches
                 WHERE kanban_card_id = $1
                   AND status IN ('pending', 'dispatched')",
            )
            .bind(&card_id_owned)
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| {
                format!("load postgres active dispatches for {card_id_owned}: {error}")
            })?;

            for dispatch_id in &active_ids {
                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
                    &mut tx,
                    dispatch_id,
                    Some("resume"),
                )
                .await?;
            }

            crate::engine::transition_executor_pg::execute_pg_transition_intent(
                &mut tx,
                &crate::engine::transition::TransitionIntent::SetLatestDispatchId {
                    card_id: card_id_owned.clone(),
                    dispatch_id: None,
                },
            )
            .await?;

            tx.commit().await.map_err(|error| {
                format!("commit postgres resume cleanup for {card_id_owned}: {error}")
            })?;
            Ok(())
        },
        |error| error,
    )
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
    let pool = required_pg_pool(state)?;
    let card_id = card_id.to_string();
    let agent_id = agent_id.to_string();
    let dispatch_type = dispatch_type.to_string();
    let title = title.to_string();
    let context = context.clone();
    let dispatch_id = crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            crate::dispatch::create_dispatch_core(
                &bridge_pool,
                &card_id,
                &agent_id,
                &dispatch_type,
                &title,
                &context,
            )
            .await
            .map(|(dispatch_id, _, _)| dispatch_id)
            .map_err(|error| format!("dispatch creation failed: {error}"))
        },
        |error| error,
    )?;

    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            crate::dispatch::query_dispatch_row_pg(&bridge_pool, &dispatch_id)
                .await
                .map_err(|error| format!("dispatch query failed: {error}"))
        },
        |error| error,
    )
}

fn get_last_terminal_dispatch_type(state: &AppState, card_id: &str) -> Option<String> {
    let pool = state.pg_pool_ref()?;
    let card_id_owned = card_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT dispatch_type
                 FROM task_dispatches
                 WHERE kanban_card_id = $1
                   AND status IN ('completed', 'failed', 'cancelled')
                 ORDER BY updated_at DESC
                 LIMIT 1",
            )
            .bind(&card_id_owned)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| {
                format!("load postgres last terminal dispatch for {card_id_owned}: {error}")
            })
        },
        |error| error,
    )
    .ok()
    .flatten()
}

/// Get dispatch status by ID
fn get_dispatch_status(state: &AppState, dispatch_id: &Option<String>) -> Option<String> {
    let did = dispatch_id.as_ref()?;
    let pool = state.pg_pool_ref()?;
    let did_owned = did.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(&did_owned)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| format!("load postgres dispatch status for {did_owned}: {error}"))
        },
        |error| error,
    )
    .ok()
    .flatten()
}

fn resolve_rework_resume_target(
    state: &AppState,
    card_id: &str,
    fallback_status: &str,
    legacy_to_status: Option<&str>,
) -> Result<String, String> {
    let pool = required_pg_pool(state)?;
    let card_id_owned = card_id.to_string();
    let fallback_status_owned = fallback_status.to_string();
    let legacy_to_status_owned = legacy_to_status.map(ToString::to_string);
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let (repo_id, card_agent): (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT repo_id, assigned_agent_id
                     FROM kanban_cards
                     WHERE id = $1",
            )
            .bind(&card_id_owned)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| {
                format!("load postgres resume card context for {card_id_owned}: {error}")
            })?
            .unwrap_or((None, None));

            let prior_status = if let Some(to_status) = legacy_to_status_owned.as_deref() {
                sqlx::query_scalar::<_, String>(
                    "SELECT from_status
                     FROM kanban_audit_logs
                     WHERE card_id = $1 AND to_status = $2
                     ORDER BY created_at DESC, id DESC
                     LIMIT 1",
                )
                .bind(&card_id_owned)
                .bind(to_status)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| {
                    format!("load postgres prior resume status for {card_id_owned}: {error}")
                })?
            } else {
                None
            }
            .filter(|status| !status.trim().is_empty())
            .unwrap_or_else(|| fallback_status_owned.clone());

            crate::pipeline::ensure_loaded();
            let effective = crate::pipeline::resolve_for_card_pg(
                &bridge_pool,
                repo_id.as_deref(),
                card_agent.as_deref(),
            )
            .await;

            Ok(effective
                .transitions
                .iter()
                .find(|transition| {
                    transition.from == prior_status
                        && transition.transition_type == crate::pipeline::TransitionType::Gated
                        && transition.gates.iter().any(|gate| gate == "review_rework")
                })
                .map(|transition| transition.to.clone())
                .unwrap_or_else(|| {
                    effective
                        .dispatchable_states()
                        .first()
                        .map(|state| state.to_string())
                        .unwrap_or_else(|| "in_progress".to_string())
                }))
        },
        |error| error,
    )
}

fn clear_manual_intervention_markers(state: &AppState, card_id: &str) -> Result<(), String> {
    let pool = required_pg_pool(state)?;
    let card_id_owned = card_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sqlx::query(
                "UPDATE kanban_cards
                 SET blocked_reason = NULL, updated_at = NOW()
                 WHERE id = $1",
            )
            .bind(&card_id_owned)
            .execute(&bridge_pool)
            .await
            .map_err(|error| {
                format!("clear postgres blocked_reason for {card_id_owned}: {error}")
            })?;
            Ok(())
        },
        |error| error,
    )?;
    sync_review_state(state, card_id, None, "idle")
}

fn sync_review_state(
    state: &AppState,
    card_id: &str,
    review_status: Option<&str>,
    review_state: &str,
) -> Result<(), String> {
    let pool = required_pg_pool(state)?;
    let card_id_owned = card_id.to_string();
    let review_status_owned = review_status.map(ToString::to_string);
    let review_state_owned = review_state.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let mut tx = bridge_pool.begin().await.map_err(|error| {
                format!("begin postgres review state sync for {card_id_owned}: {error}")
            })?;

            crate::engine::transition_executor_pg::execute_pg_transition_intent(
                &mut tx,
                &crate::engine::transition::TransitionIntent::SetReviewStatus {
                    card_id: card_id_owned.clone(),
                    review_status: review_status_owned.clone(),
                },
            )
            .await?;
            crate::engine::transition_executor_pg::execute_pg_transition_intent(
                &mut tx,
                &crate::engine::transition::TransitionIntent::SyncReviewState {
                    card_id: card_id_owned.clone(),
                    state: review_state_owned.clone(),
                },
            )
            .await?;

            tx.commit().await.map_err(|error| {
                format!("commit postgres review state sync for {card_id_owned}: {error}")
            })?;
            Ok(())
        },
        |error| error,
    )
}

/// Write audit log entry for resume action
fn write_audit_log(state: &AppState, card_id: &str, from_status: &str, reason: &str) {
    if let Some(pool) = state.pg_pool_ref() {
        let card_id_owned = card_id.to_string();
        let from_status_owned = from_status.to_string();
        let reason_owned = reason.to_string();
        let _ = crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                sqlx::query(
                    "INSERT INTO kanban_audit_logs (
                        card_id, from_status, to_status, source, result
                     ) VALUES ($1, $2, $2, 'resume', $3)",
                )
                .bind(&card_id_owned)
                .bind(&from_status_owned)
                .bind(&reason_owned)
                .execute(&bridge_pool)
                .await
                .map_err(|error| {
                    format!("insert postgres resume audit for {card_id_owned}: {error}")
                })?;
                Ok(())
            },
            |error| error,
        );
        return;
    }
}
