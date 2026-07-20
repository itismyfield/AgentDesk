//! #3038 decision_route decomposition: phases 1-2 of `submit_review_decision`
//! — input validation and pending review-decision dispatch resolution
//! (by-id recovery, scope_mismatch_closed idempotent resume, stale-state
//! idempotency). Function bodies are verbatim moves from the former
//! `decision_route.rs` monolith.

use axum::{Json, http::StatusCode};
use serde_json::json;

use crate::app_state::AppState;
use crate::error::AppError;
use crate::services::review_decision::ReviewDecisionBody;

use super::adapters::{dismiss_review_cleanup_pg_first, emit_card_updated};
use super::repo_card::{
    card_exists_pg_first, card_lifecycle_snapshot_pg_first,
    load_review_decision_card_context_pg_first, normalize_optional_commit_sha,
    recent_scope_mismatch_finalized_pg_first, resolve_effective_pipeline_pg_first,
    transition_status_pg_first,
};
use super::repo_dispatch::{
    ReviewDecisionDispatchLookup, claim_review_decision_side_effects_resume_pg_first,
    finalized_review_decision_info_pg_first, lookup_review_decision_dispatch_by_id,
};
use super::review_state_repo::update_card_review_state;
use super::worktree_stale::cancel_stale_review_dispatches_for_scope_mismatch_pg_first;
use super::{DecisionInput, DecisionResponse};

/// Phase 1 of `submit_review_decision`: input classification + existence guards.
///
/// Pure extraction of the original inline preamble. Returns `Err(response)` to
/// short-circuit the handler with the identical early-return body, or
/// `Ok(DecisionInput)` to continue. Side-effect ordering (decision whitelist →
/// commit normalization → card existence) is preserved exactly.
pub(super) async fn decision_route_validate_input(
    state: &AppState,
    body: &ReviewDecisionBody,
) -> Result<DecisionInput, DecisionResponse> {
    let valid = ["accept", "dispute", "dismiss"];
    if !valid.contains(&body.decision.as_str()) {
        return Err(AppError::bad_request(format!(
            "decision must be one of: {}",
            valid.join(", ")
        ))
        .into_json_response());
    }

    let submitted_commit = match normalize_optional_commit_sha(body.commit_sha.as_deref()) {
        Ok(commit) => commit,
        Err(error) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": error, "field": "commit_sha"})),
            ));
        }
    };

    if !card_exists_pg_first(state, &body.card_id).await {
        return Err(AppError::not_found("card not found").into_json_response());
    }

    Ok(DecisionInput { submitted_commit })
}

/// Phase 2 of `submit_review_decision`: pending review-decision dispatch
/// resolution + idempotency recovery.
///
/// Pure extraction of the original `pending_rd_id.is_none()` recovery block
/// (#2200 sub-fix 4 by-id recovery, #2341/#2200 sub-3 scope_mismatch_closed
/// idempotent resume, and #2200 sub-fix 1 stale-state idempotency). Behavior,
/// branch ordering, logging, and side-effect ordering are identical to the
/// inline original.
///
/// `pending_rd_id` is the canonical pending lookup result; this helper may
/// recover it from the submitted `dispatch_id` or claim a stale side-effects
/// resume. On success returns the (possibly updated) `pending_rd_id` together
/// with `resume_side_effects_pending`; `Err(response)` short-circuits the
/// handler with the identical early-return body.
pub(super) async fn decision_route_resolve_pending(
    state: &AppState,
    body: &ReviewDecisionBody,
    mut pending_rd_id: Option<String>,
) -> Result<(Option<String>, bool), DecisionResponse> {
    // #2200 sub-fix 4 (`stale-dispatch-mismatch`):
    // If the caller submitted an explicit `dispatch_id` and the canonical
    // pending lookup missed it, fall back to a by-id lookup scoped to the
    // same card and `dispatch_type = 'review-decision'`. This recovers the
    // case where the originating dispatch row is still `dispatched` but the
    // `card_review_state.pending_dispatch_id` / `kanban_cards.latest_dispatch_id`
    // links were cleared (e.g. by a follow-up dispatch that did not finalize
    // the predecessor).
    //
    // Authorization layering (see `lookup_review_decision_dispatch_by_id`):
    //   - Cross-card / cross-type ids return `NotFound` → 404.
    //   - Older live rows superseded by a newer live row return
    //     `LiveButSuperseded` → 409 (blocks replay of stale same-card ids).
    //   - Only the most-recent live row is honored (`LiveAndCurrent`).
    //   - Terminal rows fall through to the canonical "no pending" 409,
    //     leaving room for PR #2280 sub-fix 1's proven-finalized idempotent
    //     path to compose without short-circuit.
    let mut resume_side_effects_pending = false;

    if pending_rd_id.is_none() {
        decision_route_resolve_pending_by_id_recovery(state, body, &mut pending_rd_id).await?;
    }

    if pending_rd_id.is_none() {
        // #2341 / #2200 sub-3 idempotent resume (scope_mismatch_closed) runs
        // first; it either short-circuits with an early return (handled inside
        // the helper) or falls through. The #2200 sub-fix 1 stale-state branch
        // then runs in the SAME order it did inline.
        decision_route_resolve_pending_scope_mismatch_resume(state, body).await?;
        decision_route_resolve_pending_stale_state(
            state,
            body,
            &mut pending_rd_id,
            &mut resume_side_effects_pending,
        )
        .await?;
    }

    Ok((pending_rd_id, resume_side_effects_pending))
}

/// Sub-phase of `decision_route_resolve_pending` (#3038): #2200 sub-fix 4
/// (`stale-dispatch-mismatch`) by-id recovery. Pure extraction of the original
/// `if let Some(ref submitted_did) = body.dispatch_id { match lookup_...by_id }`
/// block. The only mutation that escapes is setting `pending_rd_id` to the
/// honored dispatch id on `LiveAndCurrent`; every other variant either returns
/// `Err` (propagated via `?`) or falls through with `Ok(())`. The caller only
/// invokes this when `pending_rd_id.is_none()`, exactly as the inline guard did.
async fn decision_route_resolve_pending_by_id_recovery(
    state: &AppState,
    body: &ReviewDecisionBody,
    pending_rd_id: &mut Option<String>,
) -> Result<(), DecisionResponse> {
    if let Some(ref submitted_did) = body.dispatch_id {
        match lookup_review_decision_dispatch_by_id(state, &body.card_id, submitted_did).await {
            ReviewDecisionDispatchLookup::LiveAndCurrent => {
                tracing::info!(
                    card_id = %body.card_id,
                    dispatch_id = %submitted_did,
                    "[review-decision] #2200 sub-fix 4: honoring submitted dispatch_id whose link rows were cleared but dispatch is still live and current"
                );
                *pending_rd_id = Some(submitted_did.clone());
            }
            ReviewDecisionDispatchLookup::LiveButSuperseded => {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "review-decision dispatch is superseded by a newer live dispatch for this card",
                        "card_id": body.card_id,
                        "dispatch_id": submitted_did,
                    })),
                ));
            }
            ReviewDecisionDispatchLookup::Terminal => {
                // Intentional fall-through: the row is terminal, which is
                // sub-fix 1's territory (PR #2280 proven-finalized).
                // Returning the canonical 409 here keeps the response
                // shape compatible with sub-1 and lets that branch
                // promote to 200 already_finalized once merged.
            }
            ReviewDecisionDispatchLookup::NotFound => {
                if !finalized_review_decision_info_pg_first(state, &body.card_id)
                    .await
                    .has_originating_dispatch()
                {
                    return Err((
                        StatusCode::NOT_FOUND,
                        Json(json!({
                            "error": "review-decision dispatch not found for this card",
                            "card_id": body.card_id,
                            "dispatch_id": submitted_did,
                        })),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Sub-phase of `decision_route_resolve_pending` (#3038): #2341 / #2200 sub-3
/// idempotent resume of a prior `scope_mismatch_closed` dispute. Pure
/// extraction of the original
/// `if body.decision == "dispute" && body.out_of_scope == Some(true) { if let
/// Some(prior) = recent_scope_mismatch_finalized_pg_first(...) { ... } }` block.
/// Every path inside the matched-`prior` arm short-circuited the original with
/// an early `return Err(..)`/`return Ok(..)`-shaped `DecisionResponse`; those
/// are reproduced verbatim as `return Err(..)` and propagated via `?`. When the
/// gate condition is false, or no prior `scope_mismatch_closed` proof exists,
/// the helper falls through with `Ok(())` — identical to the inline fall-through
/// into the stale-state branch. The DB read/write ordering (recent-finalized
/// lookup → card-context/pipeline reads → lifecycle-generation guard →
/// cancel-stale → transition → dismiss-cleanup → update_card_review_state →
/// emit_card_updated) is preserved exactly.
async fn decision_route_resolve_pending_scope_mismatch_resume(
    state: &AppState,
    body: &ReviewDecisionBody,
) -> Result<(), DecisionResponse> {
    if body.decision == "dispute" && body.out_of_scope == Some(true) {
        if let Some(prior) = recent_scope_mismatch_finalized_pg_first(state, &body.card_id).await {
            // dispatch_id must match the finalized dispatch — closes the
            // probing oracle that would let a caller learn which
            // dispatch_id terminalized this card.
            if let Some(submitted) = body.dispatch_id.as_deref() {
                if submitted != prior.dispatch_id {
                    return Err((
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": format!(
                                "out_of_scope retry dispatch_id mismatch: submitted {submitted} but prior finalized scope_mismatch_closed is {}",
                                prior.dispatch_id
                            ),
                            "card_id": body.card_id,
                        })),
                    ));
                }
            } else {
                // No dispatch_id supplied — refuse with the generic 409
                // rather than disclosing the prior close.
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "no pending review-decision dispatch for this card",
                        "card_id": body.card_id,
                    })),
                ));
            }

            // Determine whether the card already reached terminal in a
            // prior successful call. Terminal cleanup clears
            // `kanban_cards.latest_dispatch_id` (Codex round-2 [medium]),
            // which would otherwise make the stored lifecycle generation
            // diverge from the current snapshot for a fully successful
            // close — leading to a spurious 409 on retry. So:
            //   - If the card IS terminal: the prior close completed;
            //     skip the strict generation comparison, return
            //     already_finalized.
            //   - If the card is NOT terminal: the prior close was
            //     partial (tx committed but transition / cleanup did
            //     not run). Strict generation comparison is then the
            //     correct guard against terminalizing a re-opened card.
            let card_ctx = load_review_decision_card_context_pg_first(state, &body.card_id).await;
            let effective_pipeline = resolve_effective_pipeline_pg_first(
                state,
                card_ctx.repo_id.as_deref(),
                card_ctx.agent_id.as_deref(),
            )
            .await;
            let current_status = card_ctx.status.clone().unwrap_or_default();
            let terminal_state = effective_pipeline
                .states
                .iter()
                .find(|s| s.terminal)
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "done".to_string());
            let card_is_terminal = effective_pipeline.is_terminal(&current_status);

            if !card_is_terminal {
                // Generation marker: enforce only on the non-terminal
                // resume path. Terminalizing a re-opened card from a
                // stale closure is the failure mode HIGH 2 warned
                // about. The dispatch_id match above already proved
                // dispatch-scope authorization; lifecycle proves the
                // card is the same generation we closed against.
                let Some(expected) = prior.lifecycle_generation.clone() else {
                    tracing::warn!(
                        card_id = %body.card_id,
                        pending_rd_id = %prior.dispatch_id,
                        "[review-decision] #2341 idempotent resume refused: prior scope_mismatch_closed proof has no lifecycle_generation"
                    );
                    return Err((
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "prior scope_mismatch_closed proof is missing lifecycle_generation; refusing idempotent close on a non-terminal card",
                            "card_id": body.card_id,
                            "pending_dispatch_id": prior.dispatch_id,
                            "reason": "missing_lifecycle_generation",
                        })),
                    ));
                };
                let actual = card_lifecycle_snapshot_pg_first(state, &body.card_id).await;
                if actual != expected {
                    tracing::warn!(
                        card_id = %body.card_id,
                        ?expected,
                        ?actual,
                        "[review-decision] #2341 idempotent resume refused: card lifecycle advanced since prior scope_mismatch_closed (non-terminal card)"
                    );
                    return Err((
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "card lifecycle has advanced since the prior scope_mismatch_closed; refusing idempotent close on a re-opened card",
                            "card_id": body.card_id,
                            "pending_dispatch_id": prior.dispatch_id,
                            "reason": "lifecycle_generation_mismatch",
                        })),
                    ));
                }
            }

            let mut resumed_steps: Vec<&'static str> = Vec::new();
            if !card_is_terminal {
                // Resume: cancel stale + transition + cleanup. We
                // already verified lifecycle generation above, so the
                // card is still the same generation we closed against.
                tracing::warn!(
                    card_id = %body.card_id,
                    pending_rd_id = %prior.dispatch_id,
                    current_status = %current_status,
                    terminal_state = %terminal_state,
                    "[review-decision] #2341 resuming partial-close: dispatch was scope_mismatch_closed but card never reached terminal"
                );

                let expected = prior
                    .lifecycle_generation
                    .as_ref()
                    .expect("non-terminal scope_mismatch resume already required lifecycle");
                let cancelled_stale =
                    match cancel_stale_review_dispatches_for_scope_mismatch_pg_first(
                        state,
                        &body.card_id,
                        "scope_mismatch_closed_resume",
                        expected,
                    )
                    .await
                    {
                        Ok(count) => count,
                        Err(error) => {
                            return Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({
                                    "error": error,
                                    "card_id": body.card_id,
                                    "pending_dispatch_id": prior.dispatch_id,
                                    "resumed_steps": resumed_steps,
                                })),
                            ));
                        }
                    };
                if cancelled_stale > 0 {
                    resumed_steps.push("cancelled_stale");
                }

                match transition_status_pg_first(
                    state,
                    &body.card_id,
                    &terminal_state,
                    "dispute_scope_mismatch_closed_resume",
                    crate::engine::transition::ForceIntent::SystemRecovery,
                )
                .await
                {
                    Ok(_) => {
                        resumed_steps.push("transition_terminal");
                    }
                    Err(e) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %prior.dispatch_id,
                            terminal_state = %terminal_state,
                            error = %e,
                            "[review-decision] #2341 resume failed to transition card to terminal"
                        );
                        return Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!(
                                    "scope_mismatch_closed resume: card transition to {terminal_state} failed: {e}"
                                ),
                                "card_id": body.card_id,
                                "pending_dispatch_id": prior.dispatch_id,
                                "resumed_steps": resumed_steps,
                            })),
                        ));
                    }
                }

                if let Err(error) = dismiss_review_cleanup_pg_first(state, &body.card_id).await {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": error,
                            "card_id": body.card_id,
                            "pending_dispatch_id": prior.dispatch_id,
                            "resumed_steps": resumed_steps,
                        })),
                    ));
                }
                resumed_steps.push("dismiss_cleanup");

                if let Err(error) = update_card_review_state(
                    state.pg_pool_ref(),
                    &body.card_id,
                    "dispute_scope_mismatch_closed",
                    Some(&prior.dispatch_id),
                ) {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": error,
                            "card_id": body.card_id,
                            "pending_dispatch_id": prior.dispatch_id,
                            "resumed_steps": resumed_steps,
                        })),
                    ));
                }

                emit_card_updated(state, &body.card_id).await;
            }

            tracing::info!(
                card_id = %body.card_id,
                pending_rd_id = %prior.dispatch_id,
                card_was_terminal = card_is_terminal,
                resumed_steps = ?resumed_steps,
                "[review-decision] #2341 idempotent: returning 200 already_finalized for retried scope_mismatch_closed"
            );
            return Err((
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "card_id": body.card_id,
                    "decision": "dispute",
                    "outcome": "scope_mismatch_closed",
                    "pending_dispatch_id": prior.dispatch_id,
                    "review_dispatch_id": prior.review_dispatch_id,
                    "reviewed_commit": prior.reviewed_commit,
                    "resumed": !card_is_terminal,
                    "resumed_steps": resumed_steps,
                    "message": if card_is_terminal {
                        "scope_mismatch_closed already finalized; idempotent no-op"
                    } else {
                        "scope_mismatch_closed resumed: card transitioned to terminal after prior partial close"
                    },
                })),
            ));
        }
    }
    Ok(())
}

/// Sub-phase of `decision_route_resolve_pending` (#3038): #2200 sub-fix 1
/// (`stale-state`) idempotency branch. Pure extraction of the remainder of the
/// original `if pending_rd_id.is_none()` block that ran after the
/// scope_mismatch_closed resume fell through. Every original early
/// `return Err(..)`/`return Err((StatusCode::OK, ..))` is reproduced verbatim
/// and propagated via `?`. The two escaping mutations — setting `pending_rd_id`
/// to the submitted dispatch id and flipping `resume_side_effects_pending` to
/// `true` on a successful side-effects-resume claim — are threaded through
/// `&mut` params so the caller observes them exactly as the inline code did.
/// The DB read/write ordering (finalized-info lookup → latest-id match guard →
/// pending-side-effects-decision branch with its staleness gate and
/// claim_..._resume write → else proven-finalized idempotent branch → final
/// `!resume_side_effects_pending` legacy-409 guard) is preserved exactly.
async fn decision_route_resolve_pending_stale_state(
    state: &AppState,
    body: &ReviewDecisionBody,
    pending_rd_id: &mut Option<String>,
    resume_side_effects_pending: &mut bool,
) -> Result<(), DecisionResponse> {
    {
        // No pending review-decision dispatch → stale or duplicate call.
        // No dispatch_id to disambiguate either.
        //
        // #2200 sub-fix 1 (`stale-state`): when the originating review-decision
        // dispatch is missing because a follow-up (rework/review) or the
        // auto-accept policy already consumed it, idempotently short-circuit
        // instead of rejecting with 409 — but ONLY when:
        //   1. The caller supplied a `dispatch_id` that names the most-recent
        //      originating review-decision dispatch for this card (dispatch-
        //      scoped — closes the probing oracle described in Codex review).
        //      Callers without dispatch_id continue to see the legacy 409.
        //   2. The latest dispatch carries dispatch-scoped proof of the
        //      finalized decision (status + recognized completion_source +
        //      recorded decision). We never trust unscoped card-level
        //      `last_decision` alone (it can be stale from a prior round).
        //   3. The submitted decision matches the proven prior decision (a
        //      caller cannot flip a finalized decision by re-POSTing a
        //      different verdict — preserves legacy 409 for that case).

        // Without dispatch_id, return the generic legacy 409 — no card-
        // history-specific body shapes, no probing oracle.
        let Some(submitted_did) = body.dispatch_id.as_deref() else {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "no pending review-decision dispatch for this card",
                    "card_id": body.card_id,
                })),
            ));
        };

        let finalized = finalized_review_decision_info_pg_first(state, &body.card_id).await;

        // dispatch_id must match the latest originating review-decision
        // dispatch on file. Mismatch or no originating dispatch at all →
        // return the generic legacy 409 (no history disclosure).
        let matches_latest = finalized
            .latest_dispatch_id
            .as_deref()
            .is_some_and(|id| id == submitted_did);
        if !matches_latest {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "no pending review-decision dispatch for this card",
                    "card_id": body.card_id,
                })),
            ));
        }

        if let Some(pending_decision) = finalized.pending_side_effects_decision() {
            if pending_decision == body.decision.as_str() {
                if !finalized.side_effects_resume_is_stale_enough() {
                    return Err((
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "review-decision side effects are already in progress",
                            "card_id": body.card_id,
                            "dispatch_id": submitted_did,
                        })),
                    ));
                }
                tracing::warn!(
                    card_id = %body.card_id,
                    submitted_decision = %body.decision,
                    latest_dispatch_id = ?finalized.latest_dispatch_id,
                    latest_dispatch_updated_at = ?finalized.latest_dispatch_updated_at,
                    "[review-decision] resuming review-decision side effects after prior in-progress completion proof"
                );
                match claim_review_decision_side_effects_resume_pg_first(
                    state,
                    &body.card_id,
                    submitted_did,
                    &body.decision,
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err((
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "review-decision side effects are already in progress",
                                "card_id": body.card_id,
                                "dispatch_id": submitted_did,
                            })),
                        ));
                    }
                    Err(error) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            dispatch_id = %submitted_did,
                            decision = %body.decision,
                            %error,
                            "[review-decision] failed to claim stale side-effects resume"
                        );
                        return Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!(
                                    "failed to claim review-decision side-effects resume: {error}"
                                ),
                                "card_id": body.card_id,
                                "dispatch_id": submitted_did,
                            })),
                        ));
                    }
                }
                *pending_rd_id = Some(submitted_did.to_string());
                *resume_side_effects_pending = true;
            } else {
                tracing::warn!(
                    card_id = %body.card_id,
                    submitted_decision = %body.decision,
                    pending_decision = %pending_decision,
                    "[review-decision] rejecting decision-mismatch replay against side-effects-pending dispatch"
                );
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "no pending review-decision dispatch for this card",
                        "card_id": body.card_id,
                    })),
                ));
            }
        } else if let Some(proven) = finalized.proven_finalized_decision() {
            if proven == body.decision.as_str() {
                tracing::info!(
                    card_id = %body.card_id,
                    submitted_decision = %body.decision,
                    latest_dispatch_id = ?finalized.latest_dispatch_id,
                    latest_dispatch_status = ?finalized.latest_dispatch_status,
                    review_state = ?finalized.review_state,
                    "[review-decision] #2200 stale-state: returning already_finalized for idempotent re-POST"
                );
                return Err((
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "card_id": body.card_id,
                        "decision": body.decision,
                        "outcome": "already_finalized",
                        "message": "review-decision was already finalized; idempotent no-op",
                    })),
                ));
            }
            tracing::warn!(
                card_id = %body.card_id,
                submitted_decision = %body.decision,
                proven_decision = %proven,
                "[review-decision] #2200 stale-state: rejecting decision-mismatch replay against finalized dispatch"
            );
        }

        if !*resume_side_effects_pending {
            // Originating dispatch matches but proof of finalization is missing
            // (e.g. status=failed, missing completion_source, or recorded decision
            // does not match). Return the legacy 409.
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "no pending review-decision dispatch for this card",
                    "card_id": body.card_id,
                })),
            ));
        }
    }

    Ok(())
}
