use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::super::AppState;
use crate::services::provider::ProviderKind;

/// Write a review-passed marker file for the reviewed commit.
/// `promote-release.sh` checks this before allowing release promotion.
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

fn enforce_session_reset_dilemma_fallback(
    db: &crate::db::Db,
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

    let Ok(conn) = db.lock() else {
        return;
    };

    let snapshot: Option<(String, Option<String>, Option<String>, i64, Option<i64>)> = conn
        .query_row(
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
        .ok();

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

    let payload = serde_json::json!({
        "card_id": card_id,
        "state": "dilemma_pending",
        "last_verdict": verdict,
        "session_reset_round": reset_round,
    });
    let _ = crate::engine::ops::review_state_sync_on_conn(&conn, &payload.to_string());
}

#[derive(Debug, Deserialize)]
pub struct VerdictItem {
    pub category: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize)]
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

    let effective_commit: Option<String> = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        // A: Validate dispatch_type — only 'review' dispatches should go through the verdict API.
        //    implementation/rework dispatches have their own completion path (turn_bridge explicit completion),
        //    review-decision dispatches should use /api/review-decision (accept/dispute/dismiss).
        let dispatch_type: Option<String> = conn
            .query_row(
                "SELECT dispatch_type FROM task_dispatches WHERE id = ?1",
                [&body.dispatch_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        match dispatch_type.as_deref() {
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
        let dispatch_context_str: Option<String> = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [&body.dispatch_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        let dispatch_context: serde_json::Value = dispatch_context_str
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(json!({}));

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
    let updated = match crate::dispatch::mark_dispatch_completed(
        &state.db,
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
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("database lock poisoned: {e}")})),
                );
            }
        };
        let current_status: Option<String> = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [&body.dispatch_id],
                |row| row.get(0),
            )
            .ok();
        let msg = match current_status.as_deref() {
            Some("cancelled") => "dispatch was cancelled (card may have been dismissed)",
            Some("completed") => "dispatch already completed",
            _ => "dispatch not found",
        };
        return (StatusCode::CONFLICT, Json(json!({"error": msg})));
    }

    // Find associated card
    let card_id: Option<String> = state.db.separate_conn().ok().and_then(|conn| {
        conn.query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [&body.dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    });

    // #100: stamp release marker AFTER dispatch update confirmed, BEFORE hooks.
    // This ensures: (1) stale/duplicate submissions don't write markers (updated==0 already returned),
    // (2) marker failure prevents hooks from firing (no partial state).
    if body.overall == "pass" || body.overall == "approved" {
        if let Err(e) = stamp_review_passed_marker(effective_commit.as_deref()) {
            // Roll back the dispatch status since we can't complete the pass flow
            if let Ok(conn) = state.db.lock() {
                let _ = crate::dispatch::set_dispatch_status_on_conn(
                    &conn,
                    &body.dispatch_id,
                    "dispatched",
                    None,
                    "review_verdict_marker_rollback",
                    Some(&["completed"]),
                    false,
                );
            }
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
            &state.db,
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
                    &state.db,
                    &state.engine,
                    t_card_id,
                    old_s,
                    new_s,
                );
            }
        }

        enforce_session_reset_dilemma_fallback(
            &state.db,
            cid,
            &body.overall,
            body.notes.as_deref().or(body.feedback.as_deref()),
        );

        if let Some(pool) = state.pg_pool.as_ref() {
            if let Err(error) =
                super::super::dispatches::queue_dispatch_followup_pg(pool, &body.dispatch_id).await
            {
                tracing::warn!(
                    dispatch_id = %body.dispatch_id,
                    "failed to enqueue review followup: {error}"
                );
            }
        } else {
            super::super::dispatches::queue_dispatch_followup(&state.db, &body.dispatch_id);
        }
    }

    // #119: TN is recorded when a pass-reviewed card reaches done (see kanban.rs
    // record_true_negative_if_pass). FN (false_negative = pass but post-pass bug found)
    // requires an external bug-report signal that does not yet exist in the system.

    // #100: release marker was already stamped before dispatch completion (above).

    // Emit kanban_card_updated for real-time dashboard
    if let Ok(conn) = state.db.lock() {
        if let Ok(card) = conn.query_row(
            &format!("{} WHERE kc.id = ?1", super::super::kanban::CARD_SELECT),
            [&card_id],
            |row| super::super::kanban::card_row_to_json(row),
        ) {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
        }
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
