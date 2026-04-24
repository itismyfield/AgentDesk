use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::super::AppState;
use crate::services::provider::ProviderKind;

fn legacy_db(state: &AppState) -> &crate::db::Db {
    match state {
        AppState { db, .. } => db,
    }
}

/// Write a review-passed marker file for the reviewed commit.
/// `deploy-release.sh` checks this before allowing release deploy.
///
/// When `reviewed_commit` is provided, stamp that exact commit (the one that
/// was actually reviewed). Falls back to current HEAD for backwards compat.
/// Returns `Err` only when HOME directory cannot be resolved (environment
/// misconfiguration).  Git or filesystem failures are logged but not fatal
/// — the marker is best-effort when commit is not explicitly provided.
fn stamp_review_passed_marker(reviewed_commit: Option<&str>) -> Result<(), String> {
    let commit = if let Some(c) = reviewed_commit {
        c.to_string()
    } else {
        let repo_dir = crate::services::platform::resolve_repo_dir()
            .ok_or_else(|| "Cannot resolve repo dir; set AGENTDESK_REPO_DIR".to_string())?;
        match crate::services::platform::git_head_commit(&repo_dir) {
            Some(c) => c,
            None => {
                tracing::warn!(
                    "stamp_review_passed_marker: git rev-parse HEAD failed, skipping marker"
                );
                return Ok(());
            }
        }
    };
    let root = crate::config::runtime_root().ok_or_else(|| {
        "runtime root not found; set AGENTDESK_ROOT_DIR or ensure HOME exists".to_string()
    })?;
    let dir = root.join("runtime").join("review_passed");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("stamp_review_passed_marker: failed to create dir: {e}");
    }
    if let Err(e) = std::fs::write(dir.join(&commit), "") {
        tracing::warn!("stamp_review_passed_marker: failed to write marker: {e}");
    }
    Ok(())
}

fn normalize_review_notes(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_lowercase()
}

fn review_state_sync_pg_first(state: &AppState, payload: &serde_json::Value) -> String {
    crate::engine::ops::review_state_sync_with_backends(
        state.pg_pool_ref().is_none().then_some(legacy_db(state)),
        state.pg_pool_ref(),
        &payload.to_string(),
    )
}

async fn enforce_session_reset_dilemma_fallback(
    state: &AppState,
    card_id: &str,
    verdict: &str,
    new_notes: Option<&str>,
) {
    if !matches!(verdict, "improve" | "reject" | "rework") {
        return;
    }

    let Some(new_notes) = new_notes
        .map(normalize_review_notes)
        .filter(|notes| !notes.is_empty())
    else {
        return;
    };

    let snapshot: Option<(String, Option<String>, Option<String>, i64, Option<i64>)> = if let Some(
        pool,
    ) =
        state.pg_pool_ref()
    {
        sqlx::query_as::<_, (String, Option<String>, Option<String>, i64, Option<i64>)>(
            "SELECT c.status,
                        c.review_status,
                        c.review_notes,
                        COALESCE(c.review_round, 0)::BIGINT,
                        rs.session_reset_round::BIGINT
                 FROM kanban_cards c
                 LEFT JOIN card_review_state rs ON rs.card_id = c.id
                 WHERE c.id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
    } else {
        let Ok(conn) = legacy_db(state).lock() else {
            return;
        };
        conn.query_row(
                "SELECT c.status, c.review_status, c.review_notes, COALESCE(c.review_round, 0), rs.session_reset_round
                 FROM kanban_cards c
                 LEFT JOIN card_review_state rs ON rs.card_id = c.id
                 WHERE c.id = ?1",
                [card_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .ok()
    };

    let Some((card_status, review_status, previous_notes, current_round, session_reset_round)) =
        snapshot
    else {
        return;
    };

    if card_status != "review" || review_status.as_deref() != Some("reviewing") || current_round < 2
    {
        return;
    }

    let Some(reset_round) = session_reset_round else {
        return;
    };

    let Some(previous_notes) = previous_notes
        .as_deref()
        .map(normalize_review_notes)
        .filter(|notes| !notes.is_empty())
    else {
        return;
    };

    if previous_notes != new_notes {
        return;
    }

    let blocked_reason = format!(
        "세션 리셋 후에도 동일 finding 반복 (R{}→R{}) — PM 판단 필요",
        reset_round, current_round
    );

    if let Some(pool) = state.pg_pool_ref() {
        let _ = sqlx::query(
            "UPDATE kanban_cards
             SET review_status = 'dilemma_pending',
                 blocked_reason = $1,
                 suggestion_pending_at = NULL,
                 awaiting_dod_at = NULL,
                 updated_at = NOW()
             WHERE id = $2",
        )
        .bind(&blocked_reason)
        .bind(card_id)
        .execute(pool)
        .await;
    } else if let Ok(conn) = legacy_db(state).lock() {
        let _ = conn.execute(
            "UPDATE kanban_cards
             SET review_status = 'dilemma_pending',
                 blocked_reason = ?1,
                 suggestion_pending_at = NULL,
                 awaiting_dod_at = NULL,
                 updated_at = datetime('now')
             WHERE id = ?2",
            libsql_rusqlite::params![blocked_reason, card_id],
        );
    } else {
        return;
    }

    let payload = serde_json::json!({
        "card_id": card_id,
        "state": "dilemma_pending",
        "last_verdict": verdict,
        "session_reset_round": reset_round,
    });
    let _ = review_state_sync_pg_first(state, &payload);
}

async fn emit_card_updated(state: &AppState, card_id: &str) {
    if let Some(pool) = state.pg_pool_ref() {
        match super::super::kanban::load_card_json_pg(pool, card_id).await {
            Ok(Some(card)) => {
                crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
                return;
            }
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-verdict] falling back to sqlite kanban_card_updated emit"
                );
            }
        }
    }

    if let Ok(conn) = legacy_db(state).lock() {
        if let Ok(card) = conn.query_row(
            &format!("{} WHERE kc.id = ?1", super::super::kanban::CARD_SELECT),
            [card_id],
            |row| super::super::kanban::card_row_to_json(row),
        ) {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VerdictItem {
    pub category: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SubmitVerdictBody {
    pub dispatch_id: String,
    pub overall: String,
    pub items: Option<Vec<VerdictItem>>,
    pub notes: Option<String>,
    pub feedback: Option<String>,
    /// The commit SHA that was actually reviewed. When provided, the
    /// review-passed marker stamps this commit instead of the current HEAD.
    pub commit: Option<String>,
    /// Provider identifier (e.g. "claude", "codex", "gemini") of the verdict submitter.
    /// Used for cross-provider validation in counter-model reviews.
    pub provider: Option<String>,
}

/// POST /api/review-verdict
///
/// Accepts a review verdict and delegates processing to the policy engine
/// via OnReviewVerdict hook. No hardcoded card state changes.
pub async fn submit_verdict(
    State(state): State<AppState>,
    Json(body): Json<SubmitVerdictBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // #116: accept removed — it's a review-decision action, not a counter-model verdict.
    let valid_verdicts = ["pass", "improve", "reject", "rework", "approved"];
    if !valid_verdicts.contains(&body.overall.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": format!("overall must be one of: {}", valid_verdicts.join(", "))}),
            ),
        );
    }

    let dispatch = match crate::dispatch::load_dispatch_row_pg_first(
        legacy_db(&state),
        state.pg_pool_ref(),
        &body.dispatch_id,
    ) {
        Ok(Some(dispatch)) => dispatch,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "dispatch not found"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load dispatch: {error}")})),
            );
        }
    };

    let effective_commit: Option<String> = {
        // A: Validate dispatch_type — only 'review' dispatches should go through the verdict API.
        //    implementation/rework dispatches have their own completion path (turn_bridge explicit completion),
        //    review-decision dispatches should use /api/review-decision (accept/dispute/dismiss).
        match dispatch
            .get("dispatch_type")
            .and_then(|value| value.as_str())
        {
            Some("review") => {} // allowed
            Some(dtype) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!("review-verdict only accepts 'review' dispatches, got '{}'", dtype)
                    })),
                );
            }
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "dispatch not found"})),
                );
            }
        }

        // B: Cross-provider validation for counter-model reviews.
        //    When a review dispatch has from_provider/target_provider in context,
        //    reject same-provider verdict submissions (self-review).
        let dispatch_context: serde_json::Value = dispatch
            .get("context")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let from_provider = dispatch_context
            .get("from_provider")
            .and_then(|v| v.as_str());
        let target_provider = dispatch_context
            .get("target_provider")
            .and_then(|v| v.as_str());

        if let (Some(from_p), Some(target_p)) = (from_provider, target_provider) {
            // This is a counter-model review dispatch with provider tracking.
            // Require provider field and normalize via ProviderKind.
            match &body.provider {
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": "provider field is required for counter-model review verdicts"
                        })),
                    );
                }
                Some(raw_submitter) => {
                    let submitter = ProviderKind::from_str(raw_submitter);
                    let from_kind = ProviderKind::from_str(from_p);
                    let target_kind = ProviderKind::from_str(target_p);

                    match submitter {
                        None => {
                            // Unknown/unsupported provider string
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(json!({
                                    "error": format!(
                                        "unknown provider '{}' — expected a supported provider like 'claude', 'codex', 'gemini', or 'qwen'",
                                        raw_submitter
                                    )
                                })),
                            );
                        }
                        Some(ref s) if Some(s) == from_kind.as_ref() => {
                            // Same provider as implementer → self-review blocked
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(json!({
                                    "error": format!(
                                        "self-review rejected: submitting provider '{}' matches implementing provider",
                                        s.as_str()
                                    )
                                })),
                            );
                        }
                        Some(ref s) if target_kind.is_some() && Some(s) != target_kind.as_ref() => {
                            // Known provider but doesn't match expected reviewer
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(json!({
                                    "error": format!(
                                        "provider mismatch: expected '{}' but got '{}'",
                                        target_p, s.as_str()
                                    )
                                })),
                            );
                        }
                        _ => {} // Normalized cross-provider match → allowed
                    }
                }
            }
        }

        // C: Validate reviewed commit — the dispatch context stores the HEAD that was
        //    actually sent for review. Reject mismatched commits to prevent arbitrary SHA injection.
        let stored_reviewed_commit: Option<String> = dispatch_context
            .get("reviewed_commit")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match (&body.commit, &stored_reviewed_commit) {
            (Some(submitted), Some(stored)) => {
                if submitted != stored {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": format!(
                                "commit mismatch: submitted {} but dispatch was created for {}",
                                submitted, stored
                            )
                        })),
                    );
                }
                Some(stored.clone())
            }
            // body.commit is None → use stored reviewed_commit (no HEAD fallback)
            (None, stored) => stored.clone(),
            // No stored commit (legacy dispatch) → accept body.commit as-is
            (submitted, None) => submitted.clone(),
        }
    };

    // Build result JSON
    let result_json = json!({
        "verdict": body.overall,
        "items": body.items.as_ref().map(|items| {
            items.iter().map(|it| json!({
                "category": it.category,
                "summary": it.summary,
            })).collect::<Vec<_>>()
        }).unwrap_or_default(),
        "notes": body.notes,
        "feedback": body.feedback,
    });
    let _result_str = result_json.to_string();

    // #143: Mark dispatch completed via shared helper (DB-only, no OnDispatchCompleted).
    // Review verdict fires OnReviewVerdict — specialized hook, not the generic completion hook.
    // Cancelled dispatches must NOT be promoted to completed (review loop guard #80).
    let updated = match crate::dispatch::mark_dispatch_completed_pg_first(
        legacy_db(&state),
        state.pg_pool_ref(),
        &body.dispatch_id,
        &result_json,
    ) {
        Ok(n) => n,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("update dispatch: {e}")})),
            );
        }
    };

    if updated == 0 {
        let current_status = crate::dispatch::load_dispatch_row_pg_first(
            legacy_db(&state),
            state.pg_pool_ref(),
            &body.dispatch_id,
        )
        .ok()
        .flatten()
        .and_then(|dispatch| {
            dispatch
                .get("status")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        });
        let msg = match current_status.as_deref() {
            Some("cancelled") => "dispatch was cancelled (card may have been dismissed)",
            Some("completed") => "dispatch already completed",
            _ => "dispatch not found",
        };
        return (StatusCode::CONFLICT, Json(json!({"error": msg})));
    }

    // Find associated card
    let card_id = crate::dispatch::load_dispatch_row_pg_first(
        legacy_db(&state),
        state.pg_pool_ref(),
        &body.dispatch_id,
    )
    .ok()
    .flatten()
    .and_then(|dispatch| {
        dispatch
            .get("kanban_card_id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
    });

    // #100: stamp release marker AFTER dispatch update confirmed, BEFORE hooks.
    // This ensures: (1) stale/duplicate submissions don't write markers (updated==0 already returned),
    // (2) marker failure prevents hooks from firing (no partial state).
    if body.overall == "pass" || body.overall == "approved" {
        if let Err(e) = stamp_review_passed_marker(effective_commit.as_deref()) {
            // Roll back the dispatch status since we can't complete the pass flow
            let _ = crate::dispatch::set_dispatch_status_pg_first(
                legacy_db(&state),
                state.pg_pool_ref(),
                &body.dispatch_id,
                "dispatched",
                None,
                "review_verdict_marker_rollback",
                Some(&["completed"]),
                false,
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": format!("failed to write release marker: {e}"),
                })),
            );
        }
    }

    // Fire event hooks for review verdict (#134 — pipeline-defined events)
    if let Some(ref cid) = card_id {
        crate::kanban::fire_event_hooks(
            legacy_db(&state),
            &state.engine,
            "on_review_verdict",
            "OnReviewVerdict",
            json!({
                "card_id": cid,
                "dispatch_id": body.dispatch_id,
                "verdict": body.overall,
                "notes": body.notes,
                "feedback": body.feedback,
            }),
        );

        // Drain pending transitions: processVerdict may call setStatus("done"/follow-up state)
        // which queues transitions in __pendingTransitions. Without draining, OnCardTerminal
        // (auto-queue continuation) won't fire until some unrelated event drains the queue (#110).
        loop {
            let transitions = state.engine.drain_pending_transitions();
            if transitions.is_empty() {
                break;
            }
            for (t_card_id, old_s, new_s) in &transitions {
                crate::kanban::fire_transition_hooks(
                    legacy_db(&state),
                    &state.engine,
                    t_card_id,
                    old_s,
                    new_s,
                );
            }
        }

        enforce_session_reset_dilemma_fallback(
            &state,
            cid,
            &body.overall,
            body.notes.as_deref().or(body.feedback.as_deref()),
        )
        .await;

        if let Some(pool) = state.pg_pool_ref() {
            if let Err(error) = crate::services::dispatches_followup::queue_dispatch_followup_pg(
                pool,
                &body.dispatch_id,
            )
            .await
            {
                tracing::warn!(
                    dispatch_id = %body.dispatch_id,
                    "failed to enqueue review followup: {error}"
                );
            }
        } else {
            crate::services::dispatches_followup::queue_dispatch_followup(
                legacy_db(&state),
                &body.dispatch_id,
            );
        }
    }

    // #119: TN is recorded when a pass-reviewed card reaches done (see kanban.rs
    // record_true_negative_if_pass). FN (false_negative = pass but post-pass bug found)
    // requires an external bug-report signal that does not yet exist in the system.

    // #100: release marker was already stamped before dispatch completion (above).

    // Emit kanban_card_updated for real-time dashboard
    if let Some(ref cid) = card_id {
        emit_card_updated(&state, cid).await;
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "dispatch_id": body.dispatch_id,
            "overall": body.overall,
            "card_id": card_id,
        })),
    )
}
