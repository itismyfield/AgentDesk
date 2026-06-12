//! #3038 decision_route decomposition: phases 3c-4 of `submit_review_decision`
//! — the `dismiss` decision branch (transition-before-cancel ordering) and the
//! shared fall-through finalize. Function bodies are verbatim moves from the
//! former `decision_route.rs` monolith.

use axum::{Json, http::StatusCode};
use serde_json::json;

use crate::app_state::AppState;
use crate::services::review_decision::ReviewDecisionBody;

use super::DecisionResponse;
use super::adapters::{
    consume_pending_review_decision_or_response, dismiss_review_cleanup_pg_first,
    emit_card_updated, mark_consumed_review_decision_complete_or_response,
    spawn_review_tuning_aggregate_pg_first,
};
use super::repo_card::{
    load_review_decision_card_context_pg_first, resolve_effective_pipeline_pg_first,
    review_state_db, transition_status_pg_first,
};
use super::review_state_repo::update_card_review_state;
use super::tuning_aggregate::record_decision_tuning;

/// Phase 3c of `submit_review_decision`: the `dismiss` decision branch (move to
/// terminal, then cancel stale pending review dispatches). Pure extraction of
/// the original `"dismiss" =>` match arm. Unlike accept/dispute this branch
/// does NOT terminate the handler: it returns `Ok(rd_consumed)` to continue to
/// the shared finalize phase, or `Err(response)` to short-circuit with the
/// identical early-return body. Side-effect ordering (transition before
/// cleanup) is preserved exactly.
pub(super) async fn decision_route_dismiss(
    state: &AppState,
    body: &ReviewDecisionBody,
    pending_rd_id: &Option<String>,
    resume_side_effects_pending: bool,
) -> Result<bool, DecisionResponse> {
    // Agent dismisses review → transition to terminal state, then clean up stale state.
    // Order matters: transition_status requires an active dispatch, so we must
    // transition BEFORE cancelling pending dispatches.
    let card_ctx = load_review_decision_card_context_pg_first(state, &body.card_id).await;
    let effective_pipeline = resolve_effective_pipeline_pg_first(
        state,
        card_ctx.repo_id.as_deref(),
        card_ctx.agent_id.as_deref(),
    )
    .await;
    let terminal_state = effective_pipeline
        .states
        .iter()
        .find(|state| state.terminal)
        .map(|state| state.id.clone())
        .unwrap_or_else(|| "done".to_string());
    let rd_consumed = if resume_side_effects_pending {
        true
    } else {
        match consume_pending_review_decision_or_response(
            state,
            &body.card_id,
            pending_rd_id.as_deref(),
            "dismiss",
        )
        .await
        {
            Ok(consumed) => consumed,
            Err(response) => return Err(response),
        }
    };
    let current_status = card_ctx.status.clone().unwrap_or_default();
    if !effective_pipeline.is_terminal(&current_status)
        && let Err(error) = transition_status_pg_first(
            state,
            &body.card_id,
            &terminal_state,
            "dismiss",
            crate::engine::transition::ForceIntent::SystemRecovery, // dismiss bypasses review_passed gate
        )
        .await
    {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!(
                    "dismiss: card transition to {terminal_state} failed: {error}"
                ),
                "card_id": body.card_id,
                "pending_dispatch_id": pending_rd_id,
            })),
        ));
    }

    // Post-transition cleanup: cancel remaining pending review dispatches to prevent
    // stale dispatches from re-triggering review loops after dismiss.
    if let Err(error) = dismiss_review_cleanup_pg_first(state, &body.card_id).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ));
    }
    return Ok(rd_consumed);
}

/// Phase 4 of `submit_review_decision`: shared finalize for branches that fall
/// through (currently `dismiss` and the no-op `_` arm). Updates canonical review
/// state (#117), records tuning (#119), marks the consumed review-decision
/// complete for the dismiss path, emits the card-updated event, and returns the
/// generic 200 body. Pure extraction; ordering and early-return bodies are
/// identical to the original tail of the function.
pub(super) async fn decision_route_finalize(
    state: &AppState,
    body: &ReviewDecisionBody,
    pending_rd_id: &Option<String>,
    resume_side_effects_pending: bool,
    fallthrough_rd_consumed: Option<bool>,
) -> DecisionResponse {
    // #117: Update canonical review state for all decision paths
    if let Err(error) = update_card_review_state(
        review_state_db(state),
        state.pg_pool_ref(),
        &body.card_id,
        &body.decision,
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
    // #119: Record tuning outcome (dismiss falls through here; accept/dispute call helper before returning)
    if let Err(error) = record_decision_tuning(
        state.pg_pool_ref(),
        &body.card_id,
        &body.decision,
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

    if let Some(rd_consumed) = fallthrough_rd_consumed {
        if let Err(response) = mark_consumed_review_decision_complete_or_response(
            state,
            &body.card_id,
            pending_rd_id.as_deref(),
            &body.decision,
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
    }

    emit_card_updated(state, &body.card_id).await;

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "card_id": body.card_id,
            "decision": body.decision,
        })),
    )
}
