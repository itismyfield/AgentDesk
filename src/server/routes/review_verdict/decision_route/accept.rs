//! #3038 decision_route decomposition: phase 3a of `submit_review_decision` —
//! the `accept` decision branch (#195 rework dispatch, #246
//! direct-review-skip-rework, #483 terminal auto-approval, #339 fail-closed
//! follow-up guard). Function bodies are verbatim moves from the former
//! `decision_route.rs` monolith.

use axum::{Json, http::StatusCode};
use serde_json::json;

use crate::app_state::AppState;
use crate::services::review_decision::ReviewDecisionBody;

use super::super::review_state_repo::update_card_review_state;
use super::super::tuning_aggregate::record_decision_tuning;
use super::DecisionResponse;
use super::adapters::{
    consume_pending_review_decision_or_response, emit_card_updated,
    finalize_accept_cleanup_pg_first, mark_consumed_review_decision_complete_or_response,
    spawn_review_tuning_aggregate_pg_first,
};
use super::repo_card::{
    ActiveAcceptFollowups, active_accept_followups_pg_first, current_card_status_pg_first,
    evaluate_accept_skip_rework, load_review_decision_card_context_pg_first,
    resolve_effective_pipeline_pg_first, restamp_latest_active_review_target_pg_first,
    review_state_db, transition_status_pg_first,
};
use super::repo_dispatch::{
    dispatch_status_and_result_pg_first, mark_next_review_round_advance_pg_first,
};

/// Phase 3a of `submit_review_decision`: the `accept` decision branch (#195
/// rework dispatch + #246 direct-review-skip-rework + #483 terminal
/// auto-approval). Pure extraction of the original `"accept" =>` match arm;
/// every path returns a `DecisionResponse` exactly as before. Behavior,
/// logging, and side-effect ordering are unchanged.
pub(super) async fn decision_route_accept(
    state: &AppState,
    body: &ReviewDecisionBody,
    submitted_commit: &Option<String>,
    pending_rd_id: &Option<String>,
    resume_side_effects_pending: bool,
) -> DecisionResponse {
    // #195: Agent accepts review feedback — create a rework dispatch so the
    // agent can address the findings. When the rework dispatch completes,
    // OnDispatchCompleted (kanban-rules.js) transitions to review for re-review.
    let card_ctx = load_review_decision_card_context_pg_first(state, &body.card_id).await;
    let card_status_now = card_ctx.status.clone().unwrap_or_default();
    let card_repo_id = card_ctx.repo_id.clone();
    let card_agent_id = card_ctx.agent_id.clone();
    let card_title = card_ctx.title.clone();
    let effective_pipeline = resolve_effective_pipeline_pg_first(
        state,
        card_repo_id.as_deref(),
        card_agent_id.as_deref(),
    )
    .await;

    // Guard: terminal card
    if effective_pipeline.is_terminal(&card_status_now) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "card is terminal, cannot accept review feedback",
                "card_id": body.card_id,
            })),
        );
    }

    let rd_consumed = if resume_side_effects_pending {
        true
    } else {
        match consume_pending_review_decision_or_response(
            state,
            &body.card_id,
            pending_rd_id.as_deref(),
            "accept",
        )
        .await
        {
            Ok(consumed) => consumed,
            Err(response) => return response,
        }
    };

    // Find rework target via review_rework gate (same logic as timeouts.js section E)
    let rework_target = effective_pipeline
        .transitions
        .iter()
        .find(|t| {
            t.from == card_status_now
                && t.transition_type == crate::pipeline::TransitionType::Gated
                && t.gates.iter().any(|g| g == "review_rework")
        })
        .map(|t| t.to.clone())
        .unwrap_or_else(|| {
            effective_pipeline
                .dispatchable_states()
                .first()
                .map(|s| s.to_string())
                .unwrap_or_else(|| effective_pipeline.initial_state().to_string())
        });

    // #246: Check if the agent already committed new work during the
    // review-decision turn. If the worktree HEAD differs from the
    // reviewed_commit of the last review, skip rework and go straight
    // to review (the agent already addressed the feedback).
    let skip_rework_diagnostics =
        evaluate_accept_skip_rework(state, &body.card_id, submitted_commit.as_deref()).await;
    let remote_visibility_block = skip_rework_diagnostics.skip_rework
        && skip_rework_diagnostics.current_commit_remote_visible == Some(false);
    let skip_rework = skip_rework_diagnostics.skip_rework && !remote_visibility_block;

    let mut accept_failures = Vec::new();
    if remote_visibility_block {
        accept_failures.push(format!(
                    "direct review suppressed: accepted commit {} is not visible on origin mainline from {}",
                    skip_rework_diagnostics.current_commit.as_deref().unwrap_or("(unknown)"),
                    skip_rework_diagnostics.current_commit_repo.as_deref().unwrap_or("(unknown repo)")
                ));
    }
    let mut direct_review_auto_approved = false;

    // #246: If agent already committed new work, skip rework and re-enter
    // review via a two-step transition (rework_target → review) so that
    // OnReviewEnter fires naturally (increments review_round, sets
    // review_status, creates review dispatch via review-automation.js).
    let direct_review_attempted = skip_rework;
    let mut direct_review_created = decision_route_accept_direct_review_reentry(
        state,
        body,
        &effective_pipeline,
        &rework_target,
        &card_status_now,
        skip_rework,
        &mut accept_failures,
        &mut direct_review_auto_approved,
    )
    .await;

    // Create rework dispatch on the normal accept path, or as a fallback when
    // direct review re-entry fails / produces no active review dispatch.
    decision_route_accept_rework_dispatch(
        state,
        body,
        &rework_target,
        direct_review_created,
        direct_review_auto_approved,
        card_agent_id.as_deref(),
        card_title.as_deref(),
        &mut accept_failures,
    )
    .await;

    let followups = active_accept_followups_pg_first(state, &body.card_id).await;
    direct_review_created = followups.review > 0;
    if direct_review_created
        && skip_rework
        && let Some(current_commit) = skip_rework_diagnostics.current_commit.as_deref()
        && let Err(error) = restamp_latest_active_review_target_pg_first(
            state,
            &body.card_id,
            current_commit,
            skip_rework_diagnostics.current_commit_repo.as_deref(),
        )
        .await
    {
        accept_failures.push(format!(
            "direct review target restamp failed for {current_commit}: {error}"
        ));
        tracing::warn!(
            card_id = %body.card_id,
            current_commit,
            %error,
            "[review-decision] failed to restamp direct review target after accept"
        );
    }
    let rework_dispatch_created = followups.rework > 0;
    let terminal_auto_approved = direct_review_attempted
        && (direct_review_auto_approved
            || (!direct_review_created
                && !rework_dispatch_created
                && current_card_status_pg_first(state, &body.card_id)
                    .await
                    .as_deref()
                    .map(|status| effective_pipeline.is_terminal(status))
                    .unwrap_or(false)));

    if !followups.has_followup() && !terminal_auto_approved {
        let card_status_after = current_card_status_pg_first(state, &body.card_id).await;
        tracing::error!(
            card_id = %body.card_id,
            pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
            card_status_before = %card_status_now,
            card_status_after = card_status_after.as_deref().unwrap_or("(unknown)"),
            rework_target = %rework_target,
            skip_rework,
            direct_review_attempted,
            direct_review_created,
            rework_dispatch_created,
            active_review = followups.review,
            active_rework = followups.rework,
            active_review_decision = followups.review_decision,
            failures = ?accept_failures,
            "[review-decision] #339 accept failed closed: no follow-up dispatch created"
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "review-decision accept failed: no follow-up dispatch created",
                "card_id": body.card_id,
                "pending_dispatch_id": pending_rd_id,
                "skip_rework": skip_rework,
                "skip_rework_diagnostics": skip_rework_diagnostics.to_json(),
                "card_status_before": card_status_now,
                "card_status_after": card_status_after,
                "rework_target": rework_target,
                "followups": {
                    "review": followups.review,
                    "rework": followups.rework,
                    "review_decision": followups.review_decision,
                },
                "failures": accept_failures,
            })),
        );
    }

    // Clear suggestion_pending_at (always) and review_status (rework path only).
    // #266: review_status was left as "suggestion_pending" because the
    // review→in_progress rework transition is non-terminal and
    // ClearTerminalFields never fires.
    // Guard: when direct_review_created, OnReviewEnter already set
    // review_status='reviewing' — clearing it would break the live review.
    if let Err(error) = finalize_accept_cleanup_pg_first(
        state,
        &body.card_id,
        !direct_review_created && !terminal_auto_approved,
    )
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": error,
                "card_id": body.card_id,
                "pending_dispatch_id": pending_rd_id,
            })),
        );
    }

    // #119: Record tuning outcome
    if let Err(error) = record_decision_tuning(
        state.pg_pool_ref(),
        &body.card_id,
        "accept",
        pending_rd_id.as_deref(),
    )
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": error,
                "card_id": body.card_id,
                "pending_dispatch_id": pending_rd_id,
            })),
        );
    }
    spawn_review_tuning_aggregate_pg_first(state);

    // #117: Update canonical review state.
    // For direct review: OnReviewEnter already set the state, so skip the
    // rework_pending override that would conflict with the live review dispatch.
    if !direct_review_created && !terminal_auto_approved {
        if let Err(error) = update_card_review_state(
            review_state_db(state),
            state.pg_pool_ref(),
            &body.card_id,
            "accept",
            pending_rd_id.as_deref(),
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": error,
                    "card_id": body.card_id,
                    "pending_dispatch_id": pending_rd_id,
                })),
            );
        }
    }

    if let Err(response) = decision_route_accept_finalize_pending_dispatch(
        state,
        body,
        pending_rd_id,
        rd_consumed,
        terminal_auto_approved,
        followups,
    )
    .await
    {
        return response;
    }

    if let Err(response) = mark_consumed_review_decision_complete_or_response(
        state,
        &body.card_id,
        pending_rd_id.as_deref(),
        "accept",
        rd_consumed,
        if resume_side_effects_pending {
            "side_effects_resuming"
        } else {
            "side_effects_pending"
        },
    )
    .await
    {
        return response;
    }

    emit_card_updated(state, &body.card_id).await;
    let message = if terminal_auto_approved {
        "Review-decision accepted, review auto-approved (no alternate reviewer)"
    } else if direct_review_created {
        "Review-decision accepted, direct review dispatch created (rework skipped)"
    } else {
        "Review-decision accepted, rework dispatch created"
    };
    return (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "card_id": body.card_id,
            "decision": "accept",
            "rework_dispatch_created": rework_dispatch_created,
            "direct_review_created": direct_review_created,
            "review_auto_approved": terminal_auto_approved,
            "skip_rework": skip_rework,
            "skip_rework_diagnostics": skip_rework_diagnostics.to_json(),
            "message": message,
        })),
    );
}

/// Sub-phase of `decision_route_accept` (#246/#483/#339): the direct-review
/// re-entry block. Pure extraction of the original
/// `let mut direct_review_created = if skip_rework { ... } else { false };`
/// initializer expression. Returns the same `direct_review_created` bool and
/// mutates `accept_failures` / `direct_review_auto_approved` through `&mut`
/// exactly as the inline block did — so control flow, side-effect ordering and
/// the resulting locals are identical.
#[allow(clippy::too_many_arguments)]
async fn decision_route_accept_direct_review_reentry(
    state: &AppState,
    body: &ReviewDecisionBody,
    effective_pipeline: &crate::pipeline::PipelineConfig,
    rework_target: &str,
    card_status_now: &str,
    skip_rework: bool,
    accept_failures: &mut Vec<String>,
    direct_review_auto_approved: &mut bool,
) -> bool {
    if skip_rework {
        // Find the review state from the pipeline (gated transition from rework_target)
        let review_state = effective_pipeline
            .transitions
            .iter()
            .find(|t| {
                t.from == rework_target
                    && t.transition_type == crate::pipeline::TransitionType::Gated
            })
            .map(|t| t.to.clone());

        if let Some(ref review_st) = review_state {
            if let Err(error) = mark_next_review_round_advance_pg_first(state, &body.card_id).await
            {
                accept_failures.push(format!(
                    "failed to mark review round advance before direct review: {error}"
                ));
                tracing::warn!(
                    "[review-decision] failed to mark direct-review round advance for card {}: {}",
                    body.card_id,
                    error
                );
            }
            // Step 1: Transition to rework_target (e.g., in_progress)
            match transition_status_pg_first(
                state,
                &body.card_id,
                rework_target,
                "review_decision_accept_skip_rework_step1",
                crate::engine::transition::ForceIntent::SystemRecovery,
            )
            .await
            {
                Ok(_) => {
                    // Step 2: Transition to review — fires OnReviewEnter
                    match transition_status_pg_first(
                        state,
                        &body.card_id,
                        review_st,
                        "review_decision_accept_skip_rework_step2",
                        crate::engine::transition::ForceIntent::SystemRecovery,
                    )
                    .await
                    {
                        Ok(_) => {
                            // Materialize any follow-up transitions queued by
                            // OnReviewEnter (for example, single-provider
                            // auto-approval to terminal) before checking
                            // whether a live review dispatch exists.
                            crate::kanban::drain_hook_side_effects_with_backends(
                                None,
                                &state.engine,
                            );
                            let followups =
                                active_accept_followups_pg_first(state, &body.card_id).await;
                            if followups.review > 0 {
                                tracing::info!(
                                    "[review-decision] #246 Direct review re-entry for card {}: {} → {} → {} (rework skipped)",
                                    body.card_id,
                                    card_status_now,
                                    rework_target,
                                    review_st
                                );
                                true
                            } else if current_card_status_pg_first(state, &body.card_id)
                                .await
                                .as_deref()
                                .map(|status| effective_pipeline.is_terminal(status))
                                .unwrap_or(false)
                            {
                                *direct_review_auto_approved = true;
                                tracing::info!(
                                    "[review-decision] #483 Direct review re-entry for card {} auto-approved without review dispatch (no alternate reviewer)",
                                    body.card_id
                                );
                                false
                            } else {
                                accept_failures.push(format!(
                                        "direct review transition reached {} but no active review dispatch was created",
                                        review_st
                                    ));
                                tracing::warn!(
                                    "[review-decision] #339 Direct review re-entry for card {} reached {} but no active review dispatch exists",
                                    body.card_id,
                                    review_st
                                );
                                false
                            }
                        }
                        Err(e) => {
                            accept_failures.push(format!(
                                "direct review step2 transition to {} failed: {e}",
                                review_st
                            ));
                            tracing::warn!(
                                "[review-decision] #246 Step 2 transition to {} failed for card {}: {e}",
                                review_st,
                                body.card_id
                            );
                            false
                        }
                    }
                }
                Err(e) => {
                    accept_failures.push(format!(
                        "direct review step1 transition to {} failed: {e}",
                        rework_target
                    ));
                    tracing::warn!(
                        "[review-decision] #339 Step 1 transition to {} failed for card {} during direct review: {e}",
                        rework_target,
                        body.card_id
                    );
                    false
                }
            }
        } else {
            accept_failures.push(format!(
                "skip_rework requested but no review state could be resolved from rework target {}",
                rework_target
            ));
            false
        }
    } else {
        false
    }
}

/// Sub-phase of `decision_route_accept` (#195): the rework-dispatch creation
/// block, run on the normal accept path or as a fallback when direct review
/// re-entry fails / produces no active review dispatch. Pure extraction of the
/// original inline block; it performs the same side effects in the same order
/// and pushes to `accept_failures` through `&mut` exactly as before.
#[allow(clippy::too_many_arguments)]
async fn decision_route_accept_rework_dispatch(
    state: &AppState,
    body: &ReviewDecisionBody,
    rework_target: &str,
    direct_review_created: bool,
    direct_review_auto_approved: bool,
    card_agent_id: Option<&str>,
    card_title: Option<&str>,
    accept_failures: &mut Vec<String>,
) {
    let existing_followups_before_rework =
        active_accept_followups_pg_first(state, &body.card_id).await;
    if !existing_followups_before_rework.has_followup()
        && !direct_review_created
        && !direct_review_auto_approved
    {
        let card_status_before_rework = current_card_status_pg_first(state, &body.card_id).await;
        let rework_transition_ready = card_status_before_rework.as_deref() == Some(rework_target)
            || match transition_status_pg_first(
                state,
                &body.card_id,
                rework_target,
                "review_decision_accept",
                crate::engine::transition::ForceIntent::SystemRecovery,
            )
            .await
            {
                Ok(_) => true,
                Err(e) => {
                    accept_failures.push(format!(
                        "transition to rework target {} failed: {e}",
                        rework_target
                    ));
                    tracing::warn!(
                        "[review-decision] #195 Transition to rework target failed for card {}: {e}",
                        body.card_id
                    );
                    false
                }
            };

        if rework_transition_ready {
            if let Some(agent_id) = card_agent_id {
                let rework_title = format!("[Rework] {}", card_title.unwrap_or(&body.card_id));
                let rework_dispatch_result = if let Some(pool) = state.pg_pool_ref() {
                    crate::dispatch::create_dispatch_with_options_pg_only(
                        pool,
                        &state.engine,
                        &body.card_id,
                        agent_id,
                        "rework",
                        &rework_title,
                        &json!({}),
                        crate::dispatch::DispatchCreateOptions::default(),
                    )
                } else {
                    {
                        Err(anyhow::anyhow!(
                            "postgres pool unavailable for rework dispatch"
                        ))
                    }
                };
                match rework_dispatch_result {
                    Ok(dispatch) => {
                        let dispatch_id = dispatch
                            .get("id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("(unknown)");
                        tracing::info!(
                            "[review-decision] #195 Rework dispatch created: card={} dispatch={}",
                            body.card_id,
                            dispatch_id
                        );
                    }
                    Err(e) => {
                        accept_failures.push(format!("rework dispatch creation failed: {e}"));
                        tracing::warn!(
                            "[review-decision] #195 Rework dispatch creation failed for card {}: {e}",
                            body.card_id
                        );
                    }
                }
            } else {
                accept_failures.push(format!(
                    "no assigned agent for rework dispatch on card {}",
                    body.card_id
                ));
                tracing::warn!(
                    "[review-decision] #195 No agent assigned to card {} — cannot create rework dispatch",
                    body.card_id
                );
            }
        }
    }
}

/// Sub-phase of `decision_route_accept` (#339/#483): finalize the pending
/// review-decision dispatch (mark `completed`) after a follow-up dispatch was
/// created. Pure extraction of the original
/// `if !rd_consumed && let Some(rd_id) = pending_rd_id { ... }` block. Each
/// original early `return <DecisionResponse>` becomes `return Err(<same
/// response>)`; the success / no-op paths return `Ok(())`. The caller
/// propagates `Err` immediately, so the net return + short-circuit points are
/// identical.
async fn decision_route_accept_finalize_pending_dispatch(
    state: &AppState,
    body: &ReviewDecisionBody,
    pending_rd_id: &Option<String>,
    rd_consumed: bool,
    terminal_auto_approved: bool,
    followups: ActiveAcceptFollowups,
) -> Result<(), DecisionResponse> {
    if !rd_consumed && let Some(rd_id) = pending_rd_id {
        let status_db = None;
        match crate::dispatch::set_dispatch_status_with_backends(
            status_db,
            state.pg_pool_ref(),
            rd_id,
            "completed",
            Some(&json!({"decision": "accept", "completion_source": "review_decision_api"})),
            "mark_dispatch_completed",
            Some(&["pending", "dispatched"]),
            true,
        ) {
            Ok(1) => {}
            Ok(_) => {
                let dispatch_consumed_by_terminal_cleanup = terminal_auto_approved
                    && dispatch_status_and_result_pg_first(state, rd_id)
                        .await
                        .map(|(status, result)| {
                            if status == "completed" {
                                return true;
                            }
                            if status != "cancelled" {
                                return false;
                            }
                            result
                                .as_deref()
                                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                                .and_then(|value| {
                                    value
                                        .get("reason")
                                        .and_then(|reason| reason.as_str())
                                        .map(str::to_string)
                                })
                                .as_deref()
                                .is_some_and(|reason| {
                                    reason == "auto_cancelled_on_terminal_card"
                                        || reason == "js_terminal_cleanup"
                                })
                        })
                        .unwrap_or(false);
                let dispatch_no_longer_active = terminal_auto_approved
                    && active_accept_followups_pg_first(state, &body.card_id)
                        .await
                        .review_decision
                        == 0;
                if dispatch_consumed_by_terminal_cleanup || dispatch_no_longer_active {
                    tracing::info!(
                        "[review-decision] #483 pending review-decision {} for card {} was already consumed by terminal auto-approval",
                        rd_id,
                        body.card_id
                    );
                } else {
                    let live_dispatches =
                        active_accept_followups_pg_first(state, &body.card_id).await;
                    tracing::error!(
                        card_id = %body.card_id,
                        pending_rd_id = %rd_id,
                        active_review = live_dispatches.review,
                        active_rework = live_dispatches.rework,
                        active_review_decision = live_dispatches.review_decision,
                        "[review-decision] #339 accept created a follow-up dispatch but failed to finalize the pending review-decision"
                    );
                    return Err((
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "failed to finalize pending review-decision after follow-up dispatch creation",
                            "card_id": body.card_id,
                            "pending_dispatch_id": rd_id,
                        })),
                    ));
                }
            }
            Err(e) => {
                tracing::error!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    active_review = followups.review,
                    active_rework = followups.rework,
                    error = %e,
                    "[review-decision] #339 accept created a follow-up dispatch but mark_dispatch_completed errored"
                );
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": format!("failed to finalize pending review-decision: {e}"),
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                    })),
                ));
            }
        }
    }
    Ok(())
}
