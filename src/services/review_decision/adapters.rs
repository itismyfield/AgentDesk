//! #3038 decision_route decomposition: service/event adapters — tuning
//! aggregate spawn, WS card-updated emit, the consume/mark `or_response`
//! wrappers, and the `services::review_decision` / `dispatch` delegation
//! shims. Function bodies are verbatim moves from the former
//! `decision_route.rs` monolith (module-depth path adjustments only).

use axum::{Json, http::StatusCode};
use serde_json::json;

use crate::app_state::AppState;

use super::repo_dispatch::{
    consume_review_decision_dispatch_pg_first, mark_review_decision_side_effects_complete_pg_first,
};
use super::tuning_aggregate::spawn_aggregate_if_needed_with_pg;

pub(super) fn spawn_review_tuning_aggregate_pg_first(state: &AppState) {
    spawn_aggregate_if_needed_with_pg(state.pg_pool_ref().cloned());
}

pub(super) async fn emit_card_updated(state: &AppState, card_id: &str) {
    if let Some(pool) = state.pg_pool_ref() {
        match crate::db::kanban_cards::load_card_json_pg(pool, card_id).await {
            Ok(Some(card)) => {
                crate::eventbus::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
                return;
            }
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card for kanban_card_updated emit"
                );
                return;
            }
        }
    }
}

pub(super) async fn consume_pending_review_decision_or_response(
    state: &AppState,
    card_id: &str,
    pending_rd_id: Option<&str>,
    decision: &str,
) -> Result<bool, (StatusCode, Json<serde_json::Value>)> {
    let Some(rd_id) = pending_rd_id else {
        return Ok(false);
    };
    match consume_review_decision_dispatch_pg_first(state, card_id, rd_id, decision).await {
        Ok(true) => Ok(true),
        Ok(false) if state.pg_pool_ref().is_none() => Ok(false),
        Ok(false) => {
            tracing::warn!(
                card_id = %card_id,
                pending_rd_id = %rd_id,
                decision = %decision,
                "[review-decision] pending review-decision dispatch changed status before route could consume it"
            );
            Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "race: pending review-decision dispatch was already consumed",
                    "card_id": card_id,
                    "pending_dispatch_id": rd_id,
                })),
            ))
        }
        Err(error) => {
            tracing::error!(
                card_id = %card_id,
                pending_rd_id = %rd_id,
                decision = %decision,
                %error,
                "[review-decision] failed to consume pending review-decision dispatch"
            );
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!("failed to consume pending review-decision dispatch: {error}"),
                    "card_id": card_id,
                    "pending_dispatch_id": rd_id,
                })),
            ))
        }
    }
}

pub(super) async fn mark_consumed_review_decision_complete_or_response(
    state: &AppState,
    card_id: &str,
    pending_rd_id: Option<&str>,
    decision: &str,
    rd_consumed: bool,
    expected_completion_state: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if !rd_consumed {
        return Ok(());
    }
    let Some(rd_id) = pending_rd_id else {
        return Ok(());
    };

    match mark_review_decision_side_effects_complete_pg_first(
        state,
        card_id,
        rd_id,
        decision,
        expected_completion_state,
    )
    .await
    {
        Ok(true) => Ok(()),
        Ok(false) if state.pg_pool_ref().is_none() => Ok(()),
        Ok(false) => {
            tracing::error!(
                card_id = %card_id,
                pending_rd_id = %rd_id,
                decision = %decision,
                expected_completion_state,
                "[review-decision] consumed review-decision dispatch could not be promoted to completed proof after side effects"
            );
            Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "failed to finalize consumed review-decision after side effects",
                    "card_id": card_id,
                    "pending_dispatch_id": rd_id,
                })),
            ))
        }
        Err(error) => {
            tracing::error!(
                card_id = %card_id,
                pending_rd_id = %rd_id,
                decision = %decision,
                expected_completion_state,
                %error,
                "[review-decision] failed to promote consumed review-decision dispatch to completed proof"
            );
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!(
                        "failed to finalize consumed review-decision after side effects: {error}"
                    ),
                    "card_id": card_id,
                    "pending_dispatch_id": rd_id,
                })),
            ))
        }
    }
}

pub(super) async fn prepare_dispute_review_entry_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<(), String> {
    // #3038 A1/route_srp: pool extraction stays in the route layer; the tx
    // orchestration lives in `services::review_decision`. Error emission and
    // tx boundaries are preserved 1:1.
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| "postgres pool unavailable for dispute review-entry".to_string())?;
    crate::services::review_decision::prepare_dispute_review_entry(pool, card_id).await
}

pub(super) async fn finalize_accept_cleanup_pg_first(
    state: &AppState,
    card_id: &str,
    clear_review_status: bool,
) -> Result<(), String> {
    // #3038 A1/route_srp: see `prepare_dispute_review_entry_pg_first`.
    let Some(pool) = state.pg_pool_ref() else {
        return Err("postgres pool unavailable for accept cleanup".to_string());
    };
    crate::services::review_decision::finalize_accept_cleanup(pool, card_id, clear_review_status)
        .await
}

pub(super) async fn commit_belongs_to_card_issue_pg_first(
    state: &AppState,
    card_id: &str,
    commit_sha: &str,
    target_repo: Option<&str>,
) -> bool {
    if let Some(pool) = state.pg_pool_ref() {
        return crate::dispatch::commit_belongs_to_card_issue_pg(
            pool,
            card_id,
            commit_sha,
            target_repo,
        )
        .await;
    }

    false
}

pub(super) async fn cancel_dispatch_pg_first(
    state: &AppState,
    dispatch_id: &str,
    reason: Option<&str>,
) -> Result<usize, String> {
    if let Some(pool) = state.pg_pool_ref() {
        return crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            pool,
            dispatch_id,
            reason,
        )
        .await;
    }

    Err("postgres pool unavailable for cancel dispatch".to_string())
}

pub(super) async fn dismiss_review_cleanup_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<(), String> {
    // #3038 A1/route_srp: see `prepare_dispute_review_entry_pg_first`.
    let Some(pool) = state.pg_pool_ref() else {
        return Err("postgres pool unavailable for dismiss cleanup".to_string());
    };
    crate::services::review_decision::dismiss_review_cleanup(pool, card_id).await
}
