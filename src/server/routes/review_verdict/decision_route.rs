use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;

use super::super::AppState;
// #3037: `ReviewDecisionBody` relocated to the services layer so service-side
// loopback callers no longer reach back into `crate::server`. axum `Json<T>`
// extraction is location-independent; the handler references the services path.
use crate::services::review_decision::ReviewDecisionBody;

// #3038 slice 1: `decision_route.rs` is the module root — the
// `submit_review_decision` orchestrator plus the shared `DecisionResponse` /
// `DecisionInput` types. The former 4.4k-line monolith body lives in the
// submodules below as verbatim function moves:
//   * `repo_card` — kanban-card / review-state PG reads + scope-mismatch close tx
//   * `repo_dispatch` — review-decision dispatch lookup/consume/claim CAS repo
//   * `worktree_stale` — worktree/mainline resolution + stale-review cleanup
//   * `adapters` — service/event adapters and the `or_response` wrappers
//   * `pending` — phases 1-2: input validation + pending-dispatch resolution
//   * `accept` / `dispute` / `dismiss_finalize` — phase 3-4 decision branches
mod accept;
mod adapters;
mod dismiss_finalize;
mod dispute;
mod pending;
mod repo_card;
mod repo_dispatch;
mod worktree_stale;

use accept::decision_route_accept;
use dismiss_finalize::{decision_route_dismiss, decision_route_finalize};
use dispute::decision_route_dispute;
use pending::{decision_route_resolve_pending, decision_route_validate_input};
use repo_dispatch::pending_review_decision_dispatch_id_pg_first;

// ── Review Decision (agent's response to counter-model review) ──────────────

/// Shared response shape for the `submit_review_decision` handler and its
/// extracted phase helpers: `(HTTP status, JSON body)`.
type DecisionResponse = (StatusCode, Json<serde_json::Value>);

/// #3038 god-function decomposition: validated, normalized inputs threaded from
/// `decision_route_validate_input` into the rest of `submit_review_decision`.
/// Behavior-preserving — carries exactly the values the original inline
/// validation produced.
struct DecisionInput {
    /// Normalized `commit_sha` from the request body (`None` when absent).
    submitted_commit: Option<String>,
}

/// POST /api/reviews/decision
///
/// Agent's decision on counter-model review feedback.
/// - accept: agent will rework based on review → card to in_progress
/// - dispute: agent disagrees, sends back for re-review → new review dispatch
/// - dismiss: agent ignores review → card to done
///
/// #3038: this is the orchestrator. Each logical phase lives in a
/// `decision_route_*` helper; behavior, control flow, side-effect ordering, and
/// response bodies are identical to the original monolithic implementation.
pub async fn submit_review_decision(
    State(state): State<AppState>,
    Json(body): Json<ReviewDecisionBody>,
) -> DecisionResponse {
    let submitted_commit = match decision_route_validate_input(&state, &body).await {
        Ok(input) => input.submitted_commit,
        Err(response) => return response,
    };

    let pending_rd_id = pending_review_decision_dispatch_id_pg_first(&state, &body.card_id).await;

    let (pending_rd_id, resume_side_effects_pending) =
        match decision_route_resolve_pending(&state, &body, pending_rd_id).await {
            Ok(resolved) => resolved,
            Err(response) => return response,
        };

    // #109: When dispatch_id is provided, validate it matches the pending
    // review-decision dispatch. This prevents replayed or stale decisions from
    // consuming a different dispatch than the one they were issued for.
    //
    // After #2200 sub-fix 4: if we just recovered `pending_rd_id` from the
    // submitted `dispatch_id` via `lookup_review_decision_dispatch_by_id`,
    // they are guaranteed equal — this branch is a no-op in that case but is
    // kept for the canonical "pending lookup populated it" path.
    if let Some(ref submitted_did) = body.dispatch_id {
        if pending_rd_id.as_deref() != Some(submitted_did.as_str()) {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "dispatch_id mismatch: submitted {} but pending is {}",
                        submitted_did,
                        pending_rd_id.as_deref().unwrap_or("(none)")
                    ),
                    "card_id": body.card_id,
                })),
            );
        }
    }
    let fallthrough_rd_consumed: Option<bool> = match body.decision.as_str() {
        "accept" => {
            return decision_route_accept(
                &state,
                &body,
                &submitted_commit,
                &pending_rd_id,
                resume_side_effects_pending,
            )
            .await;
        }
        "dispute" => {
            return decision_route_dispute(
                &state,
                &body,
                &pending_rd_id,
                resume_side_effects_pending,
            )
            .await;
        }
        "dismiss" => {
            match decision_route_dismiss(&state, &body, &pending_rd_id, resume_side_effects_pending)
                .await
            {
                Ok(rd_consumed) => Some(rd_consumed),
                Err(response) => return response,
            }
        }
        _ => None,
    };

    decision_route_finalize(
        &state,
        &body,
        &pending_rd_id,
        resume_side_effects_pending,
        fallthrough_rd_consumed,
    )
    .await
}
