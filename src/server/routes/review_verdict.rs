use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::services::provider::ProviderKind;

/// #119: Convenience wrapper — queries review state and records a tuning outcome.
/// Called from each decision branch (accept, dispute, dismiss) to avoid
/// relying on code after the match block that early-returning branches skip.
fn record_decision_tuning(
    db: &crate::db::Db,
    card_id: &str,
    decision: &str,
    dispatch_id: Option<&str>,
) {
    let (review_round, last_verdict, finding_cats) = db
        .lock()
        .ok()
        .map(|conn| {
            let round: Option<i64> = conn
                .query_row(
                    "SELECT review_round FROM card_review_state WHERE card_id = ?1",
                    [card_id],
                    |row| row.get(0),
                )
                .ok();
            let verdict: Option<String> = conn
                .query_row(
                    "SELECT last_verdict FROM card_review_state WHERE card_id = ?1",
                    [card_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            let cats: Option<String> = conn
                .query_row(
                    "SELECT td.result FROM task_dispatches td \
                     WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' \
                     AND td.status = 'completed' ORDER BY td.rowid DESC LIMIT 1",
                    [card_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
                .and_then(|r| {
                    serde_json::from_str::<serde_json::Value>(&r)
                        .ok()
                        .and_then(|v| {
                            v["items"].as_array().map(|items| {
                                let cats: Vec<String> = items
                                    .iter()
                                    .filter_map(|it| it["category"].as_str().map(|s| s.to_string()))
                                    .collect();
                                serde_json::to_string(&cats).unwrap_or_default()
                            })
                        })
                });
            (round, verdict, cats)
        })
        .unwrap_or((None, None, None));

    let outcome = match decision {
        "accept" => "true_positive",
        "dismiss" => "false_positive",
        "dispute" => "disputed",
        _ => "unknown",
    };
    record_tuning_outcome(
        db,
        card_id,
        dispatch_id,
        review_round,
        last_verdict.as_deref().unwrap_or("unknown"),
        Some(decision),
        outcome,
        finding_cats.as_deref(),
    );
}

/// #119: Record a review tuning outcome for FP/FN aggregation.
fn record_tuning_outcome(
    db: &crate::db::Db,
    card_id: &str,
    dispatch_id: Option<&str>,
    review_round: Option<i64>,
    verdict: &str,
    decision: Option<&str>,
    outcome: &str,
    finding_categories: Option<&str>,
) {
    if let Ok(conn) = db.lock() {
        conn.execute(
            "INSERT INTO review_tuning_outcomes \
             (card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                card_id,
                dispatch_id,
                review_round,
                verdict,
                decision,
                outcome,
                finding_categories,
            ],
        )
        .ok();
        tracing::info!(
            "[review-tuning] #119 recorded outcome: card={card_id} verdict={verdict} decision={} outcome={outcome}",
            decision.unwrap_or("none")
        );
    }
}

/// #117: Update the canonical card_review_state record after a review-decision action.
fn update_card_review_state(
    db: &crate::db::Db,
    card_id: &str,
    decision: &str,
    dispatch_id: Option<&str>,
) {
    let state = match decision {
        "accept" => "rework_pending",
        "dispute" => "reviewing",
        "dismiss" => "idle",
        _ => return,
    };
    if let Ok(conn) = db.lock() {
        conn.execute(
            "INSERT INTO card_review_state (card_id, state, last_decision, decided_at, pending_dispatch_id, updated_at)
             VALUES (?1, ?2, ?3, datetime('now'), NULL, datetime('now'))
             ON CONFLICT(card_id) DO UPDATE SET
               state = ?2,
               last_decision = ?3,
               decided_by = NULL,
               decided_at = datetime('now'),
               pending_dispatch_id = NULL,
               approach_change_round = NULL,
               updated_at = datetime('now')",
            rusqlite::params![card_id, state, decision],
        ).ok();
    }
}

/// Write a review-passed marker file for the reviewed commit.
/// `promote-release.sh` checks this before allowing release promotion.
///
/// When `reviewed_commit` is provided, stamp that exact commit (the one that
/// was actually reviewed). Falls back to current HEAD for backwards compat.
/// Returns `Err` only when HOME directory cannot be resolved (environment
/// misconfiguration).  Git or filesystem failures are logged but not fatal
/// — the marker is best-effort when commit is not explicitly provided.
fn stamp_review_passed_marker(reviewed_commit: Option<&str>) -> Result<(), String> {
    let resolve_home = || -> Result<std::path::PathBuf, String> {
        dirs::home_dir().ok_or_else(|| {
            "HOME directory not found; set AGENTDESK_REPO_DIR and AGENTDESK_ROOT_DIR".to_string()
        })
    };

    let commit = if let Some(c) = reviewed_commit {
        c.to_string()
    } else {
        let repo_dir = match std::env::var("AGENTDESK_REPO_DIR") {
            Ok(d) => d,
            Err(_) => resolve_home()?
                .join("AgentDesk")
                .to_string_lossy()
                .into_owned(),
        };
        match crate::services::platform::git_head_commit(&repo_dir) {
            Some(c) => c,
            None => {
                eprintln!("stamp_review_passed_marker: git rev-parse HEAD failed, skipping marker");
                return Ok(());
            }
        }
    };
    let root = match std::env::var("AGENTDESK_ROOT_DIR") {
        Ok(d) => d,
        Err(_) => resolve_home()?
            .join(".adk/release")
            .to_string_lossy()
            .into_owned(),
    };
    let dir = format!("{}/runtime/review_passed", root);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("stamp_review_passed_marker: failed to create dir: {e}");
    }
    if let Err(e) = std::fs::write(format!("{}/{}", dir, commit), "") {
        eprintln!("stamp_review_passed_marker: failed to write marker: {e}");
    }
    Ok(())
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
    /// Provider identifier (e.g. "claude", "codex") of the verdict submitter.
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
    //    implementation/rework dispatches have their own completion path (session idle auto-complete),
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
                                    "unknown provider '{}' — expected 'claude' or 'codex'",
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

    let effective_commit: Option<String> = match (&body.commit, &stored_reviewed_commit) {
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
    let result_str = result_json.to_string();

    // #143: Mark dispatch completed via shared helper (DB-only, no OnDispatchCompleted).
    // Review verdict fires OnReviewVerdict — specialized hook, not the generic completion hook.
    // Cancelled dispatches must NOT be promoted to completed (review loop guard #80).
    drop(conn);
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
            )
        }
    };

    if updated == 0 {
        let conn = state.db.lock().unwrap();
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
                let _ = conn.execute(
                    "UPDATE task_dispatches SET status = 'dispatched', updated_at = datetime('now') WHERE id = ?1",
                    [&body.dispatch_id],
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

        // Drain pending transitions: processVerdict may call setStatus("done"/"pending_decision")
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

        super::dispatches::queue_dispatch_followup(&state.db, &body.dispatch_id);
    }

    // #119: TN is recorded when a pass-reviewed card reaches done (see kanban.rs
    // record_true_negative_if_pass). FN (false_negative = pass but post-pass bug found)
    // requires an external bug-report signal that does not yet exist in the system.

    // #100: release marker was already stamped before dispatch completion (above).

    // Emit kanban_card_updated for real-time dashboard
    if let Ok(conn) = state.db.lock() {
        if let Ok(card) = conn.query_row(
            &format!("{} WHERE kc.id = ?1", super::kanban::CARD_SELECT),
            [&card_id],
            |row| super::kanban::card_row_to_json(row),
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

// ── Review Decision (agent's response to counter-model review) ──────────────

#[derive(Debug, Deserialize)]
pub struct ReviewDecisionBody {
    pub card_id: String,
    pub decision: String, // "accept", "dispute", "dismiss"
    pub comment: Option<String>,
}

/// POST /api/review-decision
///
/// Agent's decision on counter-model review feedback.
/// - accept: agent will rework based on review → card to in_progress
/// - dispute: agent disagrees, sends back for re-review → new review dispatch
/// - dismiss: agent ignores review → card to done
pub async fn submit_review_decision(
    State(state): State<AppState>,
    Json(body): Json<ReviewDecisionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let valid = ["accept", "dispute", "dismiss"];
    if !valid.contains(&body.decision.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("decision must be one of: {}", valid.join(", "))})),
        );
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Verify card exists
    let card_status: Option<String> = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&body.card_id],
            |row| row.get(0),
        )
        .ok();

    if card_status.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    }

    // #117: Look up pending review-decision via canonical card_review_state first,
    // falling back to legacy latest_dispatch_id for cards not yet in the canonical table.
    let pending_rd_id: Option<String> = conn
        .query_row(
            "SELECT td.id FROM task_dispatches td \
             JOIN card_review_state crs ON crs.pending_dispatch_id = td.id \
             WHERE crs.card_id = ?1 AND td.dispatch_type = 'review-decision' \
             AND td.status IN ('pending', 'dispatched')",
            [&body.card_id],
            |row| row.get(0),
        )
        .ok()
        .or_else(|| {
            // Fallback: legacy path via latest_dispatch_id
            conn.query_row(
                "SELECT td.id FROM task_dispatches td \
                 JOIN kanban_cards kc ON kc.latest_dispatch_id = td.id \
                 WHERE kc.id = ?1 AND td.dispatch_type = 'review-decision' \
                 AND td.status IN ('pending', 'dispatched')",
                [&body.card_id],
                |row| row.get(0),
            )
            .ok()
        });

    if pending_rd_id.is_none() {
        // No pending review-decision dispatch → stale or duplicate call
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "no pending review-decision dispatch for this card",
                "card_id": body.card_id,
            })),
        );
    }

    match body.decision.as_str() {
        "accept" => {
            // Agent accepts review feedback — the review-decision dispatch itself
            // serves as the rework turn. No separate rework dispatch is created.
            // The agent already read the review comments and committed fixes in
            // the same turn that called this API.
            drop(conn);

            // #143: Complete review-decision dispatch via shared helper
            if let Some(ref rd_id) = pending_rd_id {
                crate::dispatch::mark_dispatch_completed(
                    &state.db,
                    rd_id,
                    &json!({"decision": "accept", "completion_source": "review_decision_api"}),
                )
                .ok();
            }

            // #119: Record tuning outcome
            record_decision_tuning(&state.db, &body.card_id, "accept", pending_rd_id.as_deref());
            spawn_aggregate_if_needed(&state.db);

            // Transition card back to review for re-review of the rework
            let (card_status_now, card_repo_id, card_agent_id): (
                String,
                Option<String>,
                Option<String>,
            ) = state
                .db
                .lock()
                .ok()
                .and_then(|c| {
                    c.query_row(
                        "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                        [&body.card_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .ok()
                })
                .unwrap_or_default();
            crate::pipeline::ensure_loaded();
            let effective_pipeline = {
                let conn = state.db.lock().unwrap();
                crate::pipeline::resolve_for_card(
                    &conn,
                    card_repo_id.as_deref(),
                    card_agent_id.as_deref(),
                )
            };

            // Find the review state from pipeline transitions
            let review_target = effective_pipeline
                .transitions
                .iter()
                .find(|t| {
                    // Find the gated transition that leads to a state with OnReviewEnter hook
                    let target_has_review = effective_pipeline
                        .hooks_for_state(&t.to)
                        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
                    t.from == card_status_now && target_has_review
                })
                .map(|t| t.to.clone())
                .or_else(|| {
                    // Fallback: look for review_rework gated transition
                    effective_pipeline
                        .transitions
                        .iter()
                        .find(|t| {
                            t.from == card_status_now
                                && t.transition_type == crate::pipeline::TransitionType::Gated
                                && t.gates.iter().any(|g| g == "review_rework")
                        })
                        .map(|t| t.to.clone())
                })
                .unwrap_or_else(|| "review".to_string());

            let _ = crate::kanban::transition_status(
                &state.db,
                &state.engine,
                &body.card_id,
                &review_target,
            );

            // #117: Update canonical review state
            update_card_review_state(&state.db, &body.card_id, "accept", pending_rd_id.as_deref());

            // Emit kanban_card_updated for real-time dashboard
            if let Ok(conn) = state.db.lock() {
                if let Ok(card) = conn.query_row(
                    &format!("{} WHERE kc.id = ?1", super::kanban::CARD_SELECT),
                    [&body.card_id],
                    |row| super::kanban::card_row_to_json(row),
                ) {
                    crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
                }
            }
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "card_id": body.card_id,
                    "decision": "accept",
                    "message": "Review-decision accepted, card moved to re-review",
                })),
            );
        }
        "dispute" => {
            // #143: Agent disputes → complete review-decision dispatch via shared helper
            if let Some(ref rd_id) = pending_rd_id {
                crate::dispatch::mark_dispatch_completed(
                    &state.db,
                    rd_id,
                    &json!({"decision": "dispute", "completion_source": "review_decision_api"}),
                )
                .ok();
            }
            // #155: Use intents for review_status mutation (executor boundary)
            use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
            let dispute_intents = vec![
                TransitionIntent::SetReviewStatus { card_id: body.card_id.clone(), review_status: Some("reviewing".to_string()) },
                TransitionIntent::SyncReviewState { card_id: body.card_id.clone(), state: "reviewing".to_string() },
            ];
            for intent in &dispute_intents {
                execute_intent_on_conn(&conn, intent).ok();
            }
            conn.execute(
                "UPDATE kanban_cards SET review_entered_at = datetime('now') WHERE id = ?1",
                [&body.card_id],
            ).ok();
            drop(conn);

            // #119: Record tuning outcome BEFORE OnReviewEnter (which increments review_round)
            record_decision_tuning(
                &state.db,
                &body.card_id,
                "dispute",
                pending_rd_id.as_deref(),
            );
            spawn_aggregate_if_needed(&state.db);

            // Fire on_enter hooks for current state (should be a review-like state with OnReviewEnter)
            let dispute_status: String = state
                .db
                .lock()
                .ok()
                .and_then(|c| {
                    c.query_row(
                        "SELECT status FROM kanban_cards WHERE id = ?1",
                        [&body.card_id],
                        |r| r.get(0),
                    )
                    .ok()
                })
                .unwrap_or_else(|| "review".to_string());
            crate::kanban::fire_enter_hooks(
                &state.db,
                &state.engine,
                &body.card_id,
                &dispute_status,
            );

            // Drain: onReviewEnter may call setStatus (e.g. pending_decision on max rounds)
            loop {
                let transitions = state.engine.drain_pending_transitions();
                if transitions.is_empty() {
                    break;
                }
                for (t_card_id, old_s, new_s) in &transitions {
                    crate::kanban::fire_transition_hooks(
                        &state.db,
                        &state.engine,
                        &t_card_id,
                        &old_s,
                        &new_s,
                    );
                }
            }

            // #117: Update canonical review state before returning
            update_card_review_state(
                &state.db,
                &body.card_id,
                "dispute",
                pending_rd_id.as_deref(),
            );

            // Send newly created review dispatch to Discord (created by OnReviewEnter hook)
            if let Ok(conn) = state.db.lock() {
                let new_review: Option<(String, String, String)> = conn
                    .query_row(
                        "SELECT td.id, COALESCE(td.to_agent_id, ''), COALESCE(td.title, '') \
                         FROM task_dispatches td \
                         WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' AND td.status = 'pending' \
                         ORDER BY td.rowid DESC LIMIT 1",
                        [&body.card_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .ok();
                drop(conn);
                if let Some((did, aid, title)) = new_review {
                    super::dispatches::queue_dispatch_notify(
                        &state.db,
                        &did,
                        &aid,
                        &body.card_id,
                        &title,
                    );
                }
            }

            // Emit kanban_card_updated for real-time dashboard
            if let Ok(conn) = state.db.lock() {
                if let Ok(card) = conn.query_row(
                    &format!("{} WHERE kc.id = ?1", super::kanban::CARD_SELECT),
                    [&body.card_id],
                    |row| super::kanban::card_row_to_json(row),
                ) {
                    crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
                }
            }
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "card_id": body.card_id,
                    "decision": "dispute",
                    "message": "Re-review dispatched to counter-model",
                })),
            );
        }
        "dismiss" => {
            // Agent dismisses review → transition to terminal state, then clean up stale state.
            // Order matters: transition_status requires an active dispatch, so we must
            // transition BEFORE cancelling pending dispatches.
            drop(conn);
            crate::pipeline::ensure_loaded();
            let terminal_state = {
                let conn_lock = state.db.lock().unwrap();
                let (repo_id, agent_id): (Option<String>, Option<String>) = conn_lock
                    .query_row(
                        "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                        [&body.card_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap_or((None, None));
                let eff = crate::pipeline::resolve_for_card(
                    &conn_lock,
                    repo_id.as_deref(),
                    agent_id.as_deref(),
                );
                eff.states
                    .iter()
                    .find(|s| s.terminal)
                    .map(|s| s.id.clone())
                    .unwrap_or_else(|| "done".to_string())
            };
            let _ = crate::kanban::transition_status(
                &state.db,
                &state.engine,
                &body.card_id,
                &terminal_state,
            );

            // Post-transition cleanup: cancel remaining pending review dispatches to prevent
            // stale dispatches from re-triggering review loops after dismiss.
            if let Ok(conn) = state.db.lock() {
                conn.execute(
                    "UPDATE task_dispatches SET status = 'cancelled', updated_at = datetime('now') \
                     WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched') \
                     AND dispatch_type IN ('review', 'review-decision')",
                    [&body.card_id],
                )
                .ok();
                // #155: Belt-and-suspenders via intent (executor boundary)
                use crate::engine::transition::{TransitionIntent, execute_intent_on_conn};
                execute_intent_on_conn(&conn, &TransitionIntent::SetReviewStatus {
                    card_id: body.card_id.clone(),
                    review_status: None,
                }).ok();
                // Clear thread mappings so dismissed review threads are not reused.
                super::dispatches::clear_all_threads(&conn, &body.card_id);
            }
        }
        _ => {}
    }

    // #117: Update canonical review state for all decision paths
    update_card_review_state(
        &state.db,
        &body.card_id,
        &body.decision,
        pending_rd_id.as_deref(),
    );

    // #119: Record tuning outcome (dismiss falls through here; accept/dispute call helper before returning)
    record_decision_tuning(
        &state.db,
        &body.card_id,
        &body.decision,
        pending_rd_id.as_deref(),
    );
    spawn_aggregate_if_needed(&state.db);

    // Emit kanban_card_updated for real-time dashboard
    if let Ok(conn) = state.db.lock() {
        if let Ok(card) = conn.query_row(
            &format!("{} WHERE kc.id = ?1", super::kanban::CARD_SELECT),
            [&body.card_id],
            |row| super::kanban::card_row_to_json(row),
        ) {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "card_id": body.card_id,
            "decision": body.decision,
        })),
    )
}

// ── #119: Review tuning aggregation ──────────────────────────────────────────

/// Minimum total outcomes required before generating any guidance.
/// Prevents misleading guidance from tiny sample sizes.
const MIN_OUTCOMES_FOR_GUIDANCE: i64 = 5;

/// Minimum outcomes per category before including it in guidance.
const MIN_CATEGORY_OUTCOMES: i64 = 3;

/// Spawn a background task to re-aggregate review tuning data.
/// Uses a lightweight debounce: skips if the last aggregate was < 60s ago.
pub fn spawn_aggregate_if_needed(db: &crate::db::Db) {
    let db = db.clone();
    tokio::spawn(async move {
        // Debounce: check if guidance file was written recently
        let path = review_tuning_guidance_path();
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if modified.elapsed().map_or(false, |d| d.as_secs() < 60) {
                    return; // aggregated < 60s ago, skip
                }
            }
        }
        aggregate_review_tuning_core(&db);
    });
}

/// Core aggregation logic shared by the HTTP endpoint and background trigger.
fn aggregate_review_tuning_core(db: &crate::db::Db) -> (i64, i64, i64, i64, i64, usize) {
    let conn = match db.lock() {
        Ok(c) => c,
        Err(_) => return (0, 0, 0, 0, 0, 0),
    };

    let mut total_tp = 0i64;
    let mut total_fp = 0i64;
    let mut total_tn = 0i64;
    let mut total_fn = 0i64;
    let mut total_disputed = 0i64;
    let mut fp_categories: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let mut tp_categories: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let mut fn_categories: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();

    {
        let mut stmt = match conn.prepare(
            "SELECT outcome, finding_categories \
             FROM review_tuning_outcomes \
             WHERE created_at > datetime('now', '-30 days')",
        ) {
            Ok(s) => s,
            Err(_) => return (0, 0, 0, 0, 0, 0),
        };

        let rows: Vec<(String, Option<String>)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .ok()
            .into_iter()
            .flat_map(|r| r.flatten())
            .collect();

        for (outcome, cats_json) in &rows {
            match outcome.as_str() {
                "true_positive" => total_tp += 1,
                "false_positive" => total_fp += 1,
                "true_negative" => total_tn += 1,
                "false_negative" => total_fn += 1,
                "disputed" => total_disputed += 1,
                _ => {}
            }
            if let Some(cats) = cats_json {
                if let Ok(cats_arr) = serde_json::from_str::<Vec<String>>(cats) {
                    let target = match outcome.as_str() {
                        "false_positive" => Some(&mut fp_categories),
                        "true_positive" => Some(&mut tp_categories),
                        "false_negative" => Some(&mut fn_categories),
                        _ => None,
                    };
                    if let Some(map) = target {
                        for cat in cats_arr {
                            *map.entry(cat).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
    }

    let total = total_tp + total_fp + total_tn + total_fn + total_disputed;
    let mut guidance_lines: Vec<String> = Vec::new();

    // Only generate guidance when we have enough data to be meaningful
    if total >= MIN_OUTCOMES_FOR_GUIDANCE {
        let actionable = total_tp + total_fp;
        let fp_rate = if actionable > 0 {
            total_fp as f64 / actionable as f64
        } else {
            0.0
        };

        guidance_lines.push(format!(
            "지난 30일 리뷰 통계: 전체 {}건 (정탐 {}건, 오탐 {}건, 정상 {}건, 미탐 {}건, 반박 {}건, 오탐률 {:.0}%)",
            total, total_tp, total_fp, total_tn, total_fn, total_disputed, fp_rate * 100.0
        ));

        // High FP categories (min sample guard)
        let mut fp_sorted: Vec<_> = fp_categories.iter().collect();
        fp_sorted.sort_by(|a, b| b.1.cmp(a.1));
        for (cat, count) in fp_sorted.iter().take(5) {
            let tp_count = tp_categories.get(*cat).copied().unwrap_or(0);
            let cat_total = *count + tp_count;
            if cat_total >= MIN_CATEGORY_OUTCOMES && **count as f64 / cat_total as f64 > 0.5 {
                guidance_lines.push(format!(
                    "- 과도 지적 카테고리 '{}': 오탐 {}건/전체 {}건 — 이 유형은 엄격도를 낮춰라",
                    cat, count, cat_total
                ));
            }
        }

        // High TP categories (min sample guard)
        let mut tp_sorted: Vec<_> = tp_categories.iter().collect();
        tp_sorted.sort_by(|a, b| b.1.cmp(a.1));
        for (cat, count) in tp_sorted.iter().take(3) {
            let fp_count = fp_categories.get(*cat).copied().unwrap_or(0);
            let cat_total = *count + fp_count;
            if cat_total >= MIN_CATEGORY_OUTCOMES && **count as f64 / cat_total as f64 > 0.7 {
                guidance_lines.push(format!(
                    "- 정탐 빈출 카테고리 '{}': 정탐 {}건/전체 {}건 — 이 유형은 계속 주의 깊게 확인하라",
                    cat, count, cat_total
                ));
            }
        }

        // FN categories — patterns the reviewer missed (reopen after pass)
        if total_fn > 0 {
            let mut fn_sorted: Vec<_> = fn_categories.iter().collect();
            fn_sorted.sort_by(|a, b| b.1.cmp(a.1));
            for (cat, count) in fn_sorted.iter().take(3) {
                guidance_lines.push(format!(
                    "- 미탐 카테고리 '{}': {}건 — 이 패턴은 리뷰에서 놓쳤다, 반드시 확인하라",
                    cat, count
                ));
            }
        }
    }

    let guidance = if guidance_lines.is_empty() {
        String::new()
    } else {
        guidance_lines.join("\n")
    };

    // Store in kv_meta
    conn.execute(
        "INSERT INTO kv_meta (key, value) VALUES ('review_tuning_guidance', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [&guidance],
    )
    .ok();

    // Write to file for prompt_builder to read
    let guidance_path = review_tuning_guidance_path();
    if let Some(parent) = guidance_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&guidance_path, &guidance);

    let lines = guidance_lines.len();
    tracing::info!(
        "[review-tuning] #119 aggregation: tp={total_tp} fp={total_fp} tn={total_tn} fn={total_fn} disputed={total_disputed}, {lines} guidance lines → {}",
        guidance_path.display()
    );

    (
        total_tp,
        total_fp,
        total_tn,
        total_fn,
        total_disputed,
        lines,
    )
}

/// POST /api/review-tuning/aggregate
///
/// Aggregates review tuning outcomes (FP/FN rates per finding category)
/// and writes tuning guidance to kv_meta + a file for prompt injection.
pub async fn aggregate_review_tuning(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (total_tp, total_fp, total_tn, total_fn, total_disputed, guidance_lines) =
        aggregate_review_tuning_core(&state.db);
    let total = total_tp + total_fp + total_tn + total_fn + total_disputed;
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "total": total,
            "true_positive": total_tp,
            "false_positive": total_fp,
            "true_negative": total_tn,
            "false_negative": total_fn,
            "disputed": total_disputed,
            "guidance_lines": guidance_lines,
        })),
    )
}

/// Well-known path for review tuning guidance file.
/// Uses ~/.adk/release/runtime/ (same logic as agentdesk_runtime_root).
pub fn review_tuning_guidance_path() -> std::path::PathBuf {
    let root = std::env::var("AGENTDESK_ROOT_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".adk").join("release")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    root.join("runtime").join("review-tuning-guidance.txt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn seed_review_card(db: &Db, dispatch_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
             VALUES ('card-1', 'Review Target', 'review', 'agent-1', ?1, 'reviewing', datetime('now'), datetime('now'))",
            [dispatch_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES (?1, 'card-1', 'agent-1', 'review', 'pending', '[Review R1] card-1', datetime('now'), datetime('now'))",
            [dispatch_id],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn submit_verdict_pass_marks_done_and_clears_review_status() {
        let db = test_db();
        seed_review_card(&db, "dispatch-pass");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, _) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-pass".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let conn = db.lock().unwrap();
        let (card_status, review_status): (String, Option<String>) = conn
            .query_row(
                "SELECT status, review_status FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-pass'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(dispatch_status, "completed");
        assert_eq!(card_status, "done");
        assert_eq!(review_status, None);
    }

    #[tokio::test]
    #[ignore] // CI: handle_completed_dispatch_followups -> send_review_result_to_primary early-returns without ADK runtime
    async fn submit_verdict_improve_creates_review_decision_dispatch() {
        let db = test_db();
        seed_review_card(&db, "dispatch-improve");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, _) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-improve".to_string(),
                overall: "improve".to_string(),
                items: None,
                notes: Some("Please tighten validation".to_string()),
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let conn = db.lock().unwrap();
        let (card_status, review_status, latest_dispatch_id): (String, Option<String>, String) = conn
            .query_row(
                "SELECT status, review_status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let (dispatch_type, dispatch_status, context): (String, String, Option<String>) = conn
            .query_row(
                "SELECT dispatch_type, status, context FROM task_dispatches WHERE id = ?1",
                [&latest_dispatch_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(card_status, "review");
        assert_eq!(review_status.as_deref(), Some("suggestion_pending"));
        assert_ne!(latest_dispatch_id, "dispatch-improve");
        assert_eq!(dispatch_type, "review-decision");
        assert_eq!(dispatch_status, "pending");
        // Context may come from Rust (with verdict) or policy (without) — both are valid
        if let Some(ref ctx) = context {
            assert!(ctx.contains("\"verdict\":\"improve\""));
        }
    }

    #[tokio::test]
    async fn review_verdict_allows_same_agent_submission() {
        let db = test_db();
        seed_review_card(&db, "dispatch-counter");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-counter".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let ok = body.0.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        assert!(ok, "same-agent review verdict should be allowed");
    }

    #[tokio::test]
    async fn implementation_dispatch_verdict_rejected() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-self', 'Self', '111', '222')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-self', 'Self Test', 'in_progress', 'agent-self', 'dispatch-self', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-self', 'card-self', 'agent-self', 'implementation', 'pending', 'Self Task', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        drop(conn);

        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-self".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("implementation"));
    }

    #[tokio::test]
    async fn review_decision_dispatch_verdict_rejected() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-rd', 'RD', '333', '444')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
             VALUES ('card-rd', 'RD Test', 'review', 'agent-rd', 'dispatch-rd', 'suggestion_pending', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-rd', 'card-rd', 'agent-rd', 'review-decision', 'pending', '[Decision] card-rd', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        drop(conn);

        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-rd".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.0["error"]
                .as_str()
                .unwrap()
                .contains("review-decision")
        );
    }

    #[tokio::test]
    async fn dismiss_clears_review_status_and_cancels_pending_dispatches() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-d', 'D', '555', '666')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
             VALUES ('card-d', 'Dismiss Test', 'review', 'agent-d', 'dispatch-rd', 'suggestion_pending', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        // Pending review-decision dispatch (should be cancelled on dismiss)
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-rd', 'card-d', 'agent-d', 'review-decision', 'pending', '[Decision] card-d', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        drop(conn);

        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_review_decision(
            State(state),
            Json(ReviewDecisionBody {
                card_id: "card-d".to_string(),
                decision: "dismiss".to_string(),
                comment: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["decision"].as_str().unwrap(), "dismiss");

        let conn = db.lock().unwrap();
        let (card_status, review_status): (String, Option<String>) = conn
            .query_row(
                "SELECT status, review_status FROM kanban_cards WHERE id = 'card-d'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-rd'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(card_status, "done", "card should be done after dismiss");
        assert_eq!(
            review_status, None,
            "review_status should be cleared after dismiss"
        );
        assert_eq!(
            dispatch_status, "cancelled",
            "pending review-decision dispatch should be cancelled"
        );
    }

    /// Regression test: cancelled dispatch must not be promoted to completed via verdict API.
    #[tokio::test]
    async fn verdict_on_cancelled_dispatch_rejected() {
        let db = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-c', 'C', '777', '888')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-c', 'Cancelled Test', 'done', 'agent-c', 'dispatch-canc', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-canc', 'card-c', 'agent-c', 'review', 'cancelled', '[Review R1] card-c', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        drop(conn);

        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-canc".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "cancelled dispatch should not accept verdict"
        );
        assert!(body.0["error"].as_str().unwrap().contains("cancelled"));

        let conn = db.lock().unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-canc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dispatch_status, "cancelled",
            "dispatch must remain cancelled"
        );
    }

    /// Seed a review dispatch with provider tracking in context (counter-model review).
    fn seed_counter_model_review(
        db: &Db,
        dispatch_id: &str,
        from_provider: &str,
        target_provider: &str,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-cm', 'Agent CM', 'ch-cc', 'ch-cdx')",
            [],
        ).unwrap();
        let context = serde_json::json!({
            "from_provider": from_provider,
            "target_provider": target_provider,
        })
        .to_string();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
             VALUES ('card-cm', 'Counter Model Test', 'review', 'agent-cm', ?1, 'reviewing', datetime('now'), datetime('now'))",
            [dispatch_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES (?1, 'card-cm', 'agent-cm', 'review', 'pending', '[Review R1] card-cm', ?2, datetime('now'), datetime('now'))",
            rusqlite::params![dispatch_id, context],
        ).unwrap();
    }

    #[tokio::test]
    async fn cross_provider_verdict_allowed() {
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-cross", "claude", "codex");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        // CDX (codex) submitting verdict for a review where from=claude, target=codex → allowed
        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-cross".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: Some("codex".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.0.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
    }

    #[tokio::test]
    async fn same_provider_verdict_rejected() {
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-self-prov", "claude", "codex");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        // CC (claude) submitting verdict for own work → self-review rejection
        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-self-prov".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: Some("claude".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("self-review"));
    }

    #[tokio::test]
    async fn verdict_without_provider_rejected_for_counter_model_dispatch() {
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-no-prov", "claude", "codex");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        // No provider specified on counter-model dispatch → rejected to prevent bypass
        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-no-prov".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.0["error"]
                .as_str()
                .unwrap()
                .contains("provider field is required")
        );
    }

    #[tokio::test]
    async fn reverse_cross_provider_verdict_allowed() {
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-rev-cross", "codex", "claude");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        // CC (claude) submitting verdict where from=codex, target=claude → allowed
        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-rev-cross".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: Some("claude".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.0.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
    }

    #[tokio::test]
    async fn casing_variant_self_review_rejected() {
        // "Claude" (capitalized) submitting for from=claude → should normalize and reject
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-case-self", "claude", "codex");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-case-self".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: Some("Claude".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("self-review"));
    }

    #[tokio::test]
    async fn casing_variant_cross_provider_allowed() {
        // "Codex" (capitalized) submitting for from=claude, target=codex → normalize and allow
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-case-cross", "claude", "codex");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-case-cross".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: Some("Codex".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.0.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
    }

    #[tokio::test]
    async fn unknown_provider_string_rejected() {
        // "gemini" or random string → reject as unknown provider
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-unknown-prov", "claude", "codex");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-unknown-prov".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: Some("gemini".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.0["error"]
                .as_str()
                .unwrap()
                .contains("unknown provider")
        );
    }

    #[tokio::test]
    async fn reverse_self_review_rejected() {
        // from=codex, target=claude, submitter=codex → self-review blocked (submitter == from)
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-mismatch", "codex", "claude");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-mismatch".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: Some("codex".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("self-review"));
    }

    #[tokio::test]
    async fn provider_mismatch_branch_rejected() {
        // from=claude, target=claude, submitter=codex → mismatch (not self-review, not target match)
        // This exercises line 341-351 (mismatch branch), not 329-339 (self-review branch)
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-mismatch2", "claude", "claude");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-mismatch2".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: Some("codex".to_string()),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.0["error"]
                .as_str()
                .unwrap()
                .contains("provider mismatch")
        );
    }

    #[tokio::test]
    async fn legacy_dispatch_without_provider_tracking_allows_no_provider() {
        // Legacy dispatches without from_provider/target_provider in context
        // should still allow verdicts without provider field
        let db = test_db();
        seed_review_card(&db, "dispatch-legacy");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-legacy".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.0.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
    }

    #[tokio::test]
    async fn accept_on_done_card_fails_closed_without_stranding() {
        let db = test_db();
        let engine = test_engine(&db);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
                 VALUES ('card-done', 'Done Card', 'done', 'agent-1', 'dispatch-orig', 'reviewed', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('dispatch-orig', 'card-done', 'agent-1', 'review-decision', 'pending', '[Review Decision]', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);

        let (status, _body) = submit_review_decision(
            State(state),
            Json(ReviewDecisionBody {
                card_id: "card-done".to_string(),
                decision: "accept".to_string(),
                comment: None,
            }),
        )
        .await;

        // Dispatch creation should fail (done terminal guard) → 500
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

        // Card must NOT have moved to in_progress — it should stay done
        let conn = db.lock().unwrap();
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            card_status, "done",
            "card must stay done, not stranded in in_progress"
        );
    }

    #[tokio::test]
    async fn dismiss_then_late_accept_does_not_reopen() {
        let db = test_db();
        let engine = test_engine(&db);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).unwrap();
            // Card already moved to done via dismiss
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
                 VALUES ('card-dismissed', 'Dismissed Card', 'done', 'agent-1', 'dispatch-rd', NULL, datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('dispatch-rd', 'card-dismissed', 'agent-1', 'review-decision', 'completed', '[Review Decision]', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);

        let (status, _) = submit_review_decision(
            State(state),
            Json(ReviewDecisionBody {
                card_id: "card-dismissed".to_string(),
                decision: "accept".to_string(),
                comment: Some("late accept after dismiss".to_string()),
            }),
        )
        .await;

        // Should fail — no pending review-decision dispatch (already completed by dismiss)
        assert_eq!(status, StatusCode::CONFLICT);

        let conn = db.lock().unwrap();
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-dismissed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            card_status, "done",
            "dismissed card must stay done on late accept"
        );
    }

    #[tokio::test]
    async fn duplicate_accept_returns_conflict() {
        let db = test_db();
        let engine = test_engine(&db);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
                 VALUES ('card-dup', 'Dup Test', 'review', 'agent-1', 'dispatch-rd', 'suggestion_pending', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('dispatch-rd', 'card-dup', 'agent-1', 'review-decision', 'pending', '[Review Decision]', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);

        // First accept should succeed
        let (status1, _) = submit_review_decision(
            State(state.clone()),
            Json(ReviewDecisionBody {
                card_id: "card-dup".to_string(),
                decision: "accept".to_string(),
                comment: None,
            }),
        )
        .await;
        assert_eq!(status1, StatusCode::OK);

        // Second accept should fail — dispatch already consumed
        let (status2, _) = submit_review_decision(
            State(state),
            Json(ReviewDecisionBody {
                card_id: "card-dup".to_string(),
                decision: "accept".to_string(),
                comment: None,
            }),
        )
        .await;
        assert_eq!(status2, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn accept_then_dispute_returns_conflict() {
        let db = test_db();
        let engine = test_engine(&db);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
                 VALUES ('card-ad', 'AD Test', 'review', 'agent-1', 'dispatch-rd2', 'suggestion_pending', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('dispatch-rd2', 'card-ad', 'agent-1', 'review-decision', 'pending', '[Review Decision]', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);

        // Accept consumes the dispatch
        let (status1, _) = submit_review_decision(
            State(state.clone()),
            Json(ReviewDecisionBody {
                card_id: "card-ad".to_string(),
                decision: "accept".to_string(),
                comment: None,
            }),
        )
        .await;
        assert_eq!(status1, StatusCode::OK);

        // Subsequent dispute should be rejected
        let (status2, _) = submit_review_decision(
            State(state),
            Json(ReviewDecisionBody {
                card_id: "card-ad".to_string(),
                decision: "dispute".to_string(),
                comment: None,
            }),
        )
        .await;
        assert_eq!(status2, StatusCode::CONFLICT);
    }

    /// #110: submit_verdict with "pass" must drain pending transitions so that
    /// OnCardTerminal fires immediately (not deferred to next tick).
    /// This ensures auto-queue continuation path is triggered.
    #[tokio::test]
    async fn submit_verdict_pass_fires_terminal_hook_via_drain() {
        let db = test_db();
        seed_review_card(&db, "dispatch-drain");

        // Create auto-queue tables and entry to verify terminal hook fires
        {
            let conn = db.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS auto_queue_runs (
                    id TEXT PRIMARY KEY, repo TEXT, agent_id TEXT,
                    status TEXT DEFAULT 'active', ai_model TEXT, ai_rationale TEXT,
                    timeout_minutes INTEGER DEFAULT 120,
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP, completed_at DATETIME
                );
                CREATE TABLE IF NOT EXISTS auto_queue_entries (
                    id TEXT PRIMARY KEY, run_id TEXT REFERENCES auto_queue_runs(id),
                    kanban_card_id TEXT, agent_id TEXT,
                    priority_rank INTEGER DEFAULT 0, reason TEXT,
                    status TEXT DEFAULT 'pending',
                    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                    dispatched_at DATETIME, completed_at DATETIME
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, status, agent_id) VALUES ('run-drain', 'active', 'agent-1')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
                 VALUES ('entry-drain', 'run-drain', 'card-1', 'agent-1', 'dispatched', 1)",
                [],
            ).unwrap();
        }

        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, _) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-drain".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);

        let conn = db.lock().unwrap();

        // Card must be done
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(card_status, "done");

        // completed_at must be set (proves OnCardTerminal or transition_status fired)
        let completed_at: Option<String> = conn
            .query_row(
                "SELECT completed_at FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            completed_at.is_some(),
            "completed_at must be set — proves terminal hook fired via drain"
        );

        // auto_queue_entry must be 'done' (proves OnCardTerminal → auto-queue.js ran)
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-drain'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            entry_status, "done",
            "auto_queue_entry must be marked done by terminal hook"
        );
    }

    /// #116: accept is not a valid counter-model verdict — only pass/approved/improve/reject/rework.
    #[tokio::test]
    async fn accept_verdict_is_rejected_by_submit_verdict() {
        let db = test_db();
        seed_review_card(&db, "dispatch-accept-v");
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-accept-v".to_string(),
                overall: "accept".to_string(),
                items: None,
                notes: None,
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "accept should be rejected as a verdict"
        );
        let err = body.0["error"].as_str().unwrap_or("");
        assert!(
            err.contains("must be one of"),
            "error should list valid verdicts: {}",
            err
        );
    }

    /// #116: Creating a new review-decision cancels any existing pending ones for the same card.
    #[tokio::test]
    async fn new_review_decision_cancels_previous_pending() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
                 VALUES ('card-dup', 'Dup Test', 'review', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            // First pending review-decision
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('rd-old', 'card-dup', 'agent-1', 'review-decision', 'pending', 'Old RD', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = 'rd-old' WHERE id = 'card-dup'",
                [],
            )
            .unwrap();
        }

        // Creating a new review-decision should cancel the old one
        let result = crate::dispatch::create_dispatch_core(
            &db,
            "card-dup",
            "agent-1",
            "review-decision",
            "[New RD]",
            &serde_json::json!({"verdict": "improve"}),
        );
        assert!(
            result.is_ok(),
            "new review-decision creation should succeed"
        );

        let conn = db.lock().unwrap();

        // Old review-decision should be cancelled
        let old_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'rd-old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            old_status, "cancelled",
            "old review-decision must be cancelled"
        );

        // Only 1 pending review-decision should exist for this card
        let pending_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-dup' AND dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pending_count, 1,
            "exactly 1 pending review-decision per card"
        );
    }

    /// #117: card_review_state is updated when review-decision is consumed (accept path).
    #[tokio::test]
    async fn accept_updates_canonical_review_state() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at)
                 VALUES ('card-rs', 'Review State Test', 'review', 'agent-1', 'rd-rs', 'suggestion_pending', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('rd-rs', 'card-rs', 'agent-1', 'review-decision', 'pending', 'RD', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, _) = submit_review_decision(
            State(state),
            Json(ReviewDecisionBody {
                card_id: "card-rs".to_string(),
                decision: "accept".to_string(),
                comment: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "accept should succeed");

        // Verify card_review_state was updated
        let conn = db.lock().unwrap();
        let (rs_state, last_decision): (String, Option<String>) = conn
            .query_row(
                "SELECT state, last_decision FROM card_review_state WHERE card_id = 'card-rs'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            rs_state, "rework_pending",
            "canonical state should be rework_pending after accept"
        );
        assert_eq!(
            last_decision.as_deref(),
            Some("accept"),
            "last_decision should be accept"
        );
    }
}
