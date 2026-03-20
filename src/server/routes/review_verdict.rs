use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::engine::hooks::Hook;

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
}

/// POST /api/review-verdict
///
/// Accepts a review verdict and delegates processing to the policy engine
/// via OnReviewVerdict hook. No hardcoded card state changes.
pub async fn submit_verdict(
    State(state): State<AppState>,
    Json(body): Json<SubmitVerdictBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let valid_verdicts = ["pass", "improve", "reject", "rework", "accept", "approved"];
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

    // Update dispatch with verdict result
    let updated = match conn.execute(
        "UPDATE task_dispatches SET status = 'completed', result = ?2, updated_at = datetime('now') WHERE id = ?1",
        rusqlite::params![body.dispatch_id, result_str],
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
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "dispatch not found"})),
        );
    }

    // Find associated card
    let card_id: Option<String> = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [&body.dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    drop(conn);

    // Fire OnReviewVerdict hook — policy engine handles all state transitions
    if let Some(ref cid) = card_id {
        let _ = state.engine.fire_hook(
            Hook::OnReviewVerdict,
            json!({
                "card_id": cid,
                "dispatch_id": body.dispatch_id,
                "verdict": body.overall,
                "notes": body.notes,
                "feedback": body.feedback,
            }),
        );
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
            )
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

    match body.decision.as_str() {
        "accept" => {
            // Agent accepts review feedback → go to in_progress for rework
            conn.execute(
                "UPDATE kanban_cards SET status = 'in_progress', review_status = 'rework_pending', updated_at = datetime('now') WHERE id = ?1",
                [&body.card_id],
            ).ok();
        }
        "dispute" => {
            // Agent disputes → increment review_round, create new review dispatch to counter-model
            conn.execute(
                "UPDATE kanban_cards SET review_status = 'reviewing', updated_at = datetime('now') WHERE id = ?1",
                [&body.card_id],
            ).ok();
            drop(conn);

            // Fire OnReviewEnter to create new review dispatch
            let _ = state.engine.fire_hook(
                Hook::OnReviewEnter,
                json!({
                    "card_id": body.card_id,
                    "from": "review",
                }),
            );

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
            // Agent dismisses review → go to done
            conn.execute(
                "UPDATE kanban_cards SET status = 'done', review_status = NULL, completed_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
                [&body.card_id],
            ).ok();
        }
        _ => {}
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
