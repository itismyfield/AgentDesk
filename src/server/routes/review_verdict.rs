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
        let db_clone = state.db.clone();
        let dispatch_id = body.dispatch_id.clone();
        tokio::spawn(async move {
            super::dispatches::handle_completed_dispatch_followups(&db_clone, &dispatch_id).await;
        });
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
            // Agent accepts review feedback → create rework dispatch
            let (agent_id, title): (String, String) = conn
                .query_row(
                    "SELECT COALESCE(assigned_agent_id, ''), title FROM kanban_cards WHERE id = ?1",
                    [&body.card_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_default();

            drop(conn);
            let _ = crate::kanban::transition_status(
                &state.db, &state.engine, &body.card_id, "in_progress",
            );
            // Set review_status separately (transition_status handles core status only)
            if let Ok(conn) = state.db.lock() {
                conn.execute(
                    "UPDATE kanban_cards SET review_status = 'rework_pending' WHERE id = ?1",
                    [&body.card_id],
                ).ok();
            }

            // Create rework dispatch so agent gets a session to do the fix
            if !agent_id.is_empty() {
                let _ = crate::dispatch::create_dispatch(
                    &state.db,
                    &state.engine,
                    &body.card_id,
                    &agent_id,
                    "rework",
                    &format!("[Rework] {title}"),
                    &json!({"review_decision": "accept", "comment": body.comment}),
                );

                // Async Discord notification
                let db_clone = state.db.clone();
                let card_id = body.card_id.clone();
                let agent_id_c = agent_id.clone();
                tokio::spawn(async move {
                    let info: Option<(String, String)> = {
                        let conn = match db_clone.lock() {
                            Ok(c) => c,
                            Err(_) => return,
                        };
                        conn.query_row(
                            "SELECT latest_dispatch_id, title FROM kanban_cards WHERE id = ?1",
                            [&card_id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .ok()
                    };
                    if let Some((dispatch_id, title)) = info {
                        super::dispatches::send_dispatch_to_discord(
                            &db_clone, &agent_id_c, &title, &card_id, &dispatch_id,
                        )
                        .await;
                    }
                });
            }

            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "card_id": body.card_id,
                    "decision": "accept",
                    "message": "Rework dispatch created",
                })),
            );
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
            drop(conn);
            let _ = crate::kanban::transition_status(
                &state.db, &state.engine, &body.card_id, "done",
            );
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
        Arc::new(Mutex::new(conn))
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
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
        };

        let (status, _) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-pass".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: None,
                feedback: None,
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
    async fn submit_verdict_improve_creates_review_decision_dispatch() {
        let db = test_db();
        seed_review_card(&db, "dispatch-improve");
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
        };

        let (status, _) = submit_verdict(
            State(state),
            Json(SubmitVerdictBody {
                dispatch_id: "dispatch-improve".to_string(),
                overall: "improve".to_string(),
                items: None,
                notes: Some("Please tighten validation".to_string()),
                feedback: None,
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
        let (dispatch_type, dispatch_status, context): (String, String, String) = conn
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
        assert!(context.contains("\"verdict\":\"improve\""));
    }
}
