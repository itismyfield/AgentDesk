//! #3038 decision_route decomposition: phase 3b of `submit_review_decision` —
//! the `dispute` decision branch, covering the #2341/#2200 sub-3 out-of-scope
//! scope_mismatch_closed close path and the regular re-review path (#229
//! cancel-stale, OnReviewEnter re-fire, #491 fail-closed target check).
//! Function bodies are verbatim moves from the former `decision_route.rs`
//! monolith.

use axum::{Json, http::StatusCode};
use serde_json::json;

use crate::app_state::AppState;
use crate::error::AppError;
use crate::services::review_decision::ReviewDecisionBody;

use super::DecisionResponse;
use super::adapters::{
    cancel_dispatch_pg_first, commit_belongs_to_card_issue_pg_first,
    consume_pending_review_decision_or_response, dismiss_review_cleanup_pg_first,
    emit_card_updated, mark_consumed_review_decision_complete_or_response,
    prepare_dispute_review_entry_pg_first, spawn_review_tuning_aggregate_pg_first,
};
use super::repo_card::{
    ScopeMismatchCloseError, atomic_finalize_scope_mismatch_close_pg,
    card_lifecycle_snapshot_pg_first, commit_belongs_to_card_issue_pg_first_tri,
    current_card_status_pg_first, load_review_decision_card_context_pg_first,
    resolve_effective_pipeline_pg_first, transition_status_pg_first,
};
use super::repo_dispatch::has_pending_reviewish_dispatch_pg_first;
use super::review_state_repo::update_card_review_state;
use super::tuning_aggregate::record_decision_tuning;
use super::worktree_stale::{
    SourceReviewLookup, cancel_stale_review_dispatches_for_scope_mismatch_pg_first,
    cancel_stale_review_dispatches_required_pg_first, latest_active_review_dispatch_pg_first,
    source_review_dispatch_for_decision_pg_first,
};

/// Phase 3b of `submit_review_decision`: the `dispute` decision branch. Covers
/// both the #2341/#2200 sub-3 `out_of_scope` scope_mismatch_closed close path
/// and the normal re-review path (#229 cancel-stale, OnReviewEnter re-fire,
/// #491 fail-closed re-review-target verification). Pure extraction of the
/// original `"dispute" =>` match arm; every path returns a `DecisionResponse`
/// exactly as before, with identical ordering and logging.
pub(super) async fn decision_route_dispute(
    state: &AppState,
    body: &ReviewDecisionBody,
    pending_rd_id: &Option<String>,
    resume_side_effects_pending: bool,
) -> DecisionResponse {
    // #2341 / #2200 sub-3 redesign: out-of-scope dispute close path.
    //
    // Production reality (per #2341 Codex round-3): at /api/review-decision
    // time the review dispatch is **completed** (not active/pending).
    // PR #2336's close path bound to `latest_active_review_dispatch`
    // and therefore never fired in production. This redesign binds
    // to the latest **completed** review dispatch, verifies its
    // `reviewed_commit` is proven out-of-scope (fail-closed on
    // Unknown — carried forward from PR #2336 HIGH 1), captures a
    // card-lifecycle generation marker (HIGH 2 reworked to bind to
    // card lifecycle, not just dispatch existence), and runs the
    // finalize + cancel-stale + transition + cleanup sequence
    // atomically with a stale re-check inside the close
    // transaction.
    if body.out_of_scope == Some(true) {
        return decision_route_dispute_scope_mismatch_close(state, body, pending_rd_id).await;
    }

    decision_route_dispute_re_review(state, body, pending_rd_id, resume_side_effects_pending).await
}

/// Phase 3b-i of `submit_review_decision` dispute branch: the out-of-scope
/// (`body.out_of_scope == Some(true)`) close path. Pure extraction of the
/// original `if body.out_of_scope == Some(true) { ... }` block (steps 1–8).
/// Every original early `return` inside the block is preserved verbatim as a
/// `return` from this helper; the parent dispatches to it unconditionally
/// inside the same `if`, so control flow, side-effect ordering, and error
/// paths are identical.
async fn decision_route_dispute_scope_mismatch_close(
    state: &AppState,
    body: &ReviewDecisionBody,
    pending_rd_id: &Option<String>,
) -> DecisionResponse {
    {
        // 1. Caller must prove ownership of the pending review-decision
        //    dispatch via `dispatch_id` matching `pending_rd_id`.
        let rd_id = match (body.dispatch_id.as_deref(), pending_rd_id.as_deref()) {
            (Some(submitted), Some(pending)) if submitted == pending => submitted.to_string(),
            (Some(submitted), Some(pending)) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": format!(
                            "dispatch_id mismatch: submitted {submitted} but pending is {pending}"
                        ),
                        "card_id": body.card_id,
                    })),
                );
            }
            (None, _) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "out_of_scope dispute requires dispatch_id to prove ownership of the pending review-decision",
                        "card_id": body.card_id,
                    })),
                );
            }
            (Some(_), None) => {
                // Already guarded above; defensive only.
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "no pending review-decision dispatch for this card",
                        "card_id": body.card_id,
                    })),
                );
            }
        };

        // 2. Bind to the **source** review dispatch that produced THIS
        //    review-decision (loaded by id from the review-decision's
        //    `context.source_review_dispatch_id`), not to the latest
        //    completed review for the card. This closes Codex r1 [medium]:
        //    a duplicate or delayed completed review row could otherwise
        //    bind the close to the wrong reviewed_commit.
        //    Codex r2 [medium]: if the source id is present but does
        //    not resolve, fail closed — no silent latest-completed
        //    fallback.
        let completed_review = match source_review_dispatch_for_decision_pg_first(
            state,
            &body.card_id,
            &rd_id,
        )
        .await
        {
            SourceReviewLookup::ResolvedById(d) => d,
            SourceReviewLookup::LegacyFallback(Some(d)) => d,
            SourceReviewLookup::LegacyFallback(None) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "out_of_scope dispute requires a completed review dispatch whose reviewed_commit can be verified against the card issue",
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                    })),
                );
            }
            SourceReviewLookup::UnresolvedSourceId(srid) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "out_of_scope dispute refused: review-decision context references a source review that does not resolve to a completed review row; cannot verify scope",
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                        "source_review_dispatch_id": srid,
                        "reason": "source_review_unresolved",
                    })),
                );
            }
        };
        let reviewed_commit = match completed_review.reviewed_commit.clone() {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "out_of_scope dispute requires the completed review to expose reviewed_commit for scope verification",
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                        "review_dispatch_id": completed_review.id,
                    })),
                );
            }
        };

        // 3. HIGH 1 fail-closed: tri-state scope verification. Only a
        //    proven OutOfScope is allowed to take the close shortcut.
        //    Unknown — transient PG/git failure — refuses with 503.
        match commit_belongs_to_card_issue_pg_first_tri(
            state,
            &body.card_id,
            &reviewed_commit,
            completed_review.target_repo.as_deref(),
        )
        .await
        {
            crate::dispatch::ScopeCheck::OutOfScope => {}
            crate::dispatch::ScopeCheck::InScope => {
                tracing::warn!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    review_dispatch_id = %completed_review.id,
                    reviewed_commit = %reviewed_commit,
                    "[review-decision] #2341 rejected out_of_scope claim: reviewed_commit belongs to the card issue"
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "out_of_scope dispute refused: reviewed_commit belongs to this card's issue; submit a regular dispute instead",
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                        "review_dispatch_id": completed_review.id,
                        "reviewed_commit": reviewed_commit,
                    })),
                );
            }
            crate::dispatch::ScopeCheck::Unknown => {
                tracing::warn!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    review_dispatch_id = %completed_review.id,
                    reviewed_commit = %reviewed_commit,
                    target_repo = %completed_review.target_repo.as_deref().unwrap_or(""),
                    "[review-decision] #2341 refused out_of_scope: scope verification inconclusive (repo/git transient failure); fail-closed"
                );
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "error": "scope verification inconclusive; cannot close as out-of-scope. Retry once the repo is reachable, or submit a regular dispute.",
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                        "review_dispatch_id": completed_review.id,
                        "reviewed_commit": reviewed_commit,
                        "reason": "scope_check_unknown",
                    })),
                );
            }
        }

        // 4. Capture the card lifecycle generation snapshot. The
        //    atomic close re-reads this inside its transaction and
        //    rolls back if it has changed (= card re-opened between
        //    snapshot and tx).
        let lifecycle_snapshot = card_lifecycle_snapshot_pg_first(state, &body.card_id).await;

        // 5. Atomic finalize: in one tx, re-check lifecycle + flip
        //    the review-decision dispatch to completed +
        //    scope_mismatch_closed + update card_review_state.
        match atomic_finalize_scope_mismatch_close_pg(
            state,
            &body.card_id,
            &rd_id,
            &completed_review.id,
            &reviewed_commit,
            &lifecycle_snapshot,
        )
        .await
        {
            Ok(_) => {}
            Err(ScopeMismatchCloseError::LifecycleStale { expected, actual }) => {
                tracing::warn!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    review_dispatch_id = %completed_review.id,
                    ?expected,
                    ?actual,
                    "[review-decision] #2341 refused out_of_scope close: card lifecycle generation changed (card re-opened)"
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "card lifecycle has advanced since the completed review; refusing to close as out-of-scope",
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                        "review_dispatch_id": completed_review.id,
                        "reason": "lifecycle_generation_mismatch",
                    })),
                );
            }
            Err(ScopeMismatchCloseError::DispatchConsumed) => {
                tracing::warn!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    "[review-decision] #2341 race: pending review-decision dispatch was already consumed before scope_mismatch_closed could finalize it"
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "race: pending review-decision dispatch was already consumed",
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                    })),
                );
            }
            Err(ScopeMismatchCloseError::Internal(e)) => {
                tracing::error!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    error = %e,
                    "[review-decision] #2341 atomic finalize failed for scope_mismatch_closed"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": format!(
                            "failed to atomically finalize scope_mismatch_closed: {e}"
                        ),
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                    })),
                );
            }
        }

        // 6. Re-check lifecycle BEFORE any destructive action.
        //    Codex round-2 [high]: cancel-stale must NOT run before
        //    the lifecycle re-check, otherwise a re-open that
        //    happened between the tx commit and this point would
        //    have its fresh review dispatch cancelled by
        //    stale_review_dispatch_ids_pg_first before we discover
        //    the re-open and refuse. Re-checking first means the
        //    409 refusal triggers before any side effects.
        let post_tx_lifecycle = card_lifecycle_snapshot_pg_first(state, &body.card_id).await;
        if post_tx_lifecycle != lifecycle_snapshot {
            tracing::warn!(
                card_id = %body.card_id,
                pending_rd_id = %rd_id,
                ?lifecycle_snapshot,
                ?post_tx_lifecycle,
                "[review-decision] #2341 lifecycle changed after tx commit but before cleanup; leaving dispatch finalized for idempotent resume, no destructive actions taken"
            );
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "card lifecycle advanced after scope_mismatch_closed finalize; transition refused",
                    "card_id": body.card_id,
                    "pending_dispatch_id": rd_id,
                    "review_dispatch_id": completed_review.id,
                    "reason": "lifecycle_generation_mismatch_post_tx",
                })),
            );
        }

        // 7. Cancel stale review/review-decision dispatches so the
        //    dedup guard doesn't strand them. Outside the tx because
        //    cancel touches multiple rows + may dispatch outbox
        //    messages. Safe to run now: the post-tx lifecycle
        //    re-check above guaranteed no fresh generation exists.
        let cancelled_stale = match cancel_stale_review_dispatches_for_scope_mismatch_pg_first(
            state,
            &body.card_id,
            "scope_mismatch_closed",
            &lifecycle_snapshot,
        )
        .await
        {
            Ok(count) => count,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": error,
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                    })),
                );
            }
        };

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
        match transition_status_pg_first(
            state,
            &body.card_id,
            &terminal_state,
            "dispute_scope_mismatch_closed",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::error!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    terminal_state = %terminal_state,
                    error = %e,
                    "[review-decision] #2341 finalized review-decision but failed to transition card to terminal"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": format!(
                            "scope_mismatch_closed: review-decision finalized but card transition to {terminal_state} failed: {e}"
                        ),
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                    })),
                );
            }
        }

        // 8. Reuse dismiss cleanup to clear any leftover pending
        //    review dispatches and review_status.
        if let Err(error) = dismiss_review_cleanup_pg_first(state, &body.card_id).await {
            return AppError::internal(error).into_json_response();
        }

        if let Err(error) = record_decision_tuning(
            state.pg_pool_ref(),
            &body.card_id,
            "dispute_scope_mismatch_closed",
            Some(&rd_id),
        )
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": error,
                    "card_id": body.card_id,
                    "pending_dispatch_id": rd_id,
                })),
            );
        }
        spawn_review_tuning_aggregate_pg_first(state);

        emit_card_updated(state, &body.card_id).await;
        tracing::info!(
            card_id = %body.card_id,
            pending_rd_id = %rd_id,
            review_dispatch_id = %completed_review.id,
            reviewed_commit = %reviewed_commit,
            cancelled_stale,
            "[review-decision] #2341 closed dispute as scope_mismatch_closed (completed-review-context binding)"
        );
        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "card_id": body.card_id,
                "decision": "dispute",
                "outcome": "scope_mismatch_closed",
                "pending_dispatch_id": rd_id,
                "review_dispatch_id": completed_review.id,
                "reviewed_commit": reviewed_commit,
                "cancelled_stale_dispatches": cancelled_stale,
                "message": "Dispute closed: completed review verified as out-of-scope for this card",
            })),
        );
    }
}

/// Phase 3b-ii of `submit_review_decision` dispute branch: the regular
/// (non-out-of-scope) re-review path. Pure extraction of the original code
/// that followed the `if body.out_of_scope` block — consume the pending
/// review-decision, prepare the dispute re-review entry, record tuning,
/// cancel stale dispatches, re-fire `OnReviewEnter`, validate the live
/// review dispatch, update review state, finalize the pending
/// review-decision, and return. Every original early `return` is preserved
/// verbatim; the trailing value (originally the function tail) becomes this
/// helper's tail and is returned by the parent. Control flow, side-effect
/// ordering, and error paths are identical.
async fn decision_route_dispute_re_review(
    state: &AppState,
    body: &ReviewDecisionBody,
    pending_rd_id: &Option<String>,
    resume_side_effects_pending: bool,
) -> DecisionResponse {
    let rd_consumed = if resume_side_effects_pending {
        true
    } else {
        match consume_pending_review_decision_or_response(
            state,
            &body.card_id,
            pending_rd_id.as_deref(),
            "dispute",
        )
        .await
        {
            Ok(consumed) => consumed,
            Err(response) => return response,
        }
    };

    if let Err(error) = prepare_dispute_review_entry_pg_first(state, &body.card_id).await {
        return AppError::internal(error).into_json_response();
    }

    // #119: Record tuning outcome BEFORE OnReviewEnter (which increments review_round)
    if let Err(error) = record_decision_tuning(
        state.pg_pool_ref(),
        &body.card_id,
        "dispute",
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

    // #229: Cancel stale pending/dispatched review dispatches for this card.
    // Without this, the dispatch-core dedup guard blocks
    // OnReviewEnter from creating a fresh review dispatch after dispute.
    let cancelled = match cancel_stale_review_dispatches_required_pg_first(
        state,
        &body.card_id,
        "superseded_by_dispute_re_review",
    )
    .await
    {
        Ok(count) => count,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": error,
                    "card_id": body.card_id,
                    "pending_dispatch_id": pending_rd_id,
                })),
            );
        }
    };
    if cancelled > 0 {
        tracing::info!(
            "[review-decision] #229 Cancelled {} stale review dispatch(es) for card {} before dispute re-review",
            cancelled,
            body.card_id
        );
    }

    // Fire on_enter hooks for current state (should be a review-like state with OnReviewEnter)
    let dispute_status = current_card_status_pg_first(state, &body.card_id)
        .await
        .unwrap_or_else(|| "review".to_string());
    crate::kanban::fire_enter_hooks_with_backends(&state.engine, &body.card_id, &dispute_status);

    // #108: Drain all pending intents and transitions from OnReviewEnter hooks.
    // drain_hook_side_effects handles both transition processing (e.g. setStatus
    // for review/manual-intervention follow-up on max rounds) and Discord notifications for any
    // dispatches created by the hooks, eliminating the previous manual drain loop
    // that only handled transitions and missed dispatch notifications.
    crate::kanban::drain_hook_side_effects_with_backends(&state.engine);

    // #229: Safety net — if card is still in a review-like state but no
    // pending review dispatch exists (OnReviewEnter hook may have failed
    // due to lock contention or JS error), re-fire with blocking lock.
    {
        let card_ctx = load_review_decision_card_context_pg_first(state, &body.card_id).await;
        let has_review_dispatch =
            has_pending_reviewish_dispatch_pg_first(state, &body.card_id).await;
        let effective_pipeline = resolve_effective_pipeline_pg_first(
            state,
            card_ctx.repo_id.as_deref(),
            card_ctx.agent_id.as_deref(),
        )
        .await;
        let needs_review = card_ctx.status.as_deref().is_some_and(|status| {
            effective_pipeline
                .hooks_for_state(status)
                .is_some_and(|hooks| hooks.on_enter.iter().any(|name| name == "OnReviewEnter"))
        }) && !has_review_dispatch;

        if needs_review {
            tracing::warn!(
                "[review-decision] Card {} in review state but no review dispatch after dispute — re-firing OnReviewEnter (#229)",
                body.card_id
            );
            let _ = state
                .engine
                .fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": body.card_id }));
            crate::kanban::drain_hook_side_effects_with_backends(&state.engine);
        }
    }

    let live_review = match latest_active_review_dispatch_pg_first(state, &body.card_id).await {
        Some(dispatch) => dispatch,
        None => {
            tracing::error!(
                card_id = %body.card_id,
                pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
                "[review-decision] #491 dispute failed closed: no live review dispatch after re-review entry"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "review-decision dispute failed: no follow-up review dispatch created",
                    "card_id": body.card_id,
                    "pending_dispatch_id": pending_rd_id,
                })),
            );
        }
    };

    if let Some(ref reviewed_commit) = live_review.reviewed_commit {
        if !commit_belongs_to_card_issue_pg_first(
            state,
            &body.card_id,
            reviewed_commit,
            live_review.target_repo.as_deref(),
        )
        .await
        {
            let _ = cancel_dispatch_pg_first(
                state,
                &live_review.id,
                Some("invalid_dispute_rereview_target"),
            )
            .await;
            tracing::error!(
                card_id = %body.card_id,
                pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
                review_dispatch_id = %live_review.id,
                reviewed_commit = %reviewed_commit,
                "[review-decision] #491 dispute failed closed: re-review target does not belong to the card issue"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "review-decision dispute failed: re-review target is stale or unrelated to the card issue",
                    "card_id": body.card_id,
                    "pending_dispatch_id": pending_rd_id,
                    "review_dispatch_id": live_review.id,
                    "reviewed_commit": reviewed_commit,
                })),
            );
        }
    }

    // #117: Update canonical review state before returning
    if let Err(error) = update_card_review_state(
        state.pg_pool_ref(),
        &body.card_id,
        "dispute",
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

    if !rd_consumed && let Some(rd_id) = pending_rd_id {
        match crate::dispatch::set_dispatch_status_with_backends(
            state.pg_pool_ref(),
            rd_id,
            "completed",
            Some(&json!({"decision": "dispute", "completion_source": "review_decision_api"})),
            "mark_dispatch_completed",
            Some(&["pending", "dispatched"]),
            true,
        ) {
            Ok(1) => {}
            Ok(_) => {
                tracing::error!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    review_dispatch_id = %live_review.id,
                    "[review-decision] #491 dispute created a follow-up review dispatch but failed to finalize the pending review-decision"
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "failed to finalize pending review-decision after re-review dispatch creation",
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                        "review_dispatch_id": live_review.id,
                    })),
                );
            }
            Err(e) => {
                tracing::error!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    review_dispatch_id = %live_review.id,
                    error = %e,
                    "[review-decision] #491 dispute created a follow-up review dispatch but mark_dispatch_completed errored"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": format!("failed to finalize pending review-decision: {e}"),
                        "card_id": body.card_id,
                        "pending_dispatch_id": rd_id,
                        "review_dispatch_id": live_review.id,
                    })),
                );
            }
        }
    }

    if let Err(response) = mark_consumed_review_decision_complete_or_response(
        state,
        &body.card_id,
        pending_rd_id.as_deref(),
        "dispute",
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
    return (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "card_id": body.card_id,
            "decision": "dispute",
            "review_dispatch_id": live_review.id,
            "reviewed_commit": live_review.reviewed_commit,
            "message": "Re-review dispatched to counter-model",
        })),
    );
}
