use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::engine::hooks::Hook;

/// Write a review-passed marker file for the reviewed commit.
/// `promote-release.sh` checks this before allowing release promotion.
///
/// When `reviewed_commit` is provided, stamp that exact commit (the one that
/// was actually reviewed). Falls back to current HEAD for backwards compat.
fn stamp_review_passed_marker(reviewed_commit: Option<&str>) {
    let commit = if let Some(c) = reviewed_commit {
        c.to_string()
    } else {
        let repo_dir = std::env::var("AGENTDESK_REPO_DIR")
            .unwrap_or_else(|_| format!("{}/AgentDesk", env!("HOME")));
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo_dir)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let Some(c) = out else { return };
        c
    };
    let root = std::env::var("AGENTDESK_ROOT_DIR")
        .unwrap_or_else(|_| format!("{}/.adk/release", env!("HOME")));
    let dir = format!("{}/runtime/review_passed", root);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(format!("{}/{}", dir, commit), "");
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

    if let (Some(from_p), Some(_target_p)) = (from_provider, target_provider) {
        // This is a counter-model review dispatch with provider tracking.
        // Require provider field to prevent bypass of self-review check.
        match body.provider {
            Some(ref submitter) if submitter == from_p => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!(
                            "self-review rejected: submitting provider '{}' matches implementing provider",
                            submitter
                        )
                    })),
                );
            }
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "provider field is required for counter-model review verdicts"
                    })),
                );
            }
            _ => {} // cross-provider → allowed
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

    // Update dispatch with verdict result — only if still pending/dispatched.
    // Cancelled dispatches (e.g. after dismiss) must NOT be promoted to completed,
    // as that would re-trigger OnDispatchCompleted hooks and cause review loops (#80).
    let updated = match conn.execute(
        "UPDATE task_dispatches SET status = 'completed', result = ?2, updated_at = datetime('now') \
         WHERE id = ?1 AND status IN ('pending', 'dispatched')",
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
        // Check if dispatch exists but was cancelled/completed
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
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": msg})),
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

    // When review passes, stamp a marker so promote-release.sh can verify
    if body.overall == "pass" || body.overall == "approved" {
        stamp_review_passed_marker(effective_commit.as_deref());
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
                &state.db,
                &state.engine,
                &body.card_id,
                "in_progress",
            );
            // Set review_status separately (transition_status handles core status only)
            if let Ok(conn) = state.db.lock() {
                conn.execute(
                    "UPDATE kanban_cards SET review_status = 'rework_pending' WHERE id = ?1",
                    [&body.card_id],
                )
                .ok();
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
                            &db_clone,
                            &agent_id_c,
                            &title,
                            &card_id,
                            &dispatch_id,
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
            // Agent dismisses review → transition to done, then clean up stale state.
            // Order matters: transition_status requires an active dispatch, so we must
            // transition BEFORE cancelling pending dispatches.
            drop(conn);
            let _ =
                crate::kanban::transition_status(&state.db, &state.engine, &body.card_id, "done");

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
                // Belt-and-suspenders: ensure review_status is cleared even if transition_status
                // failed silently (the `let _ =` above discards errors).
                conn.execute(
                    "UPDATE kanban_cards SET review_status = NULL WHERE id = ?1",
                    [&body.card_id],
                )
                .ok();
            }
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
            health_registry: None,
        };

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
    async fn submit_verdict_improve_creates_review_decision_dispatch() {
        let db = test_db();
        seed_review_card(&db, "dispatch-improve");
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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

        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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

        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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
        assert!(body.0["error"].as_str().unwrap().contains("review-decision"));
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

        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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
        assert_eq!(review_status, None, "review_status should be cleared after dismiss");
        assert_eq!(dispatch_status, "cancelled", "pending review-decision dispatch should be cancelled");
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

        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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

        assert_eq!(status, StatusCode::CONFLICT, "cancelled dispatch should not accept verdict");
        assert!(body.0["error"].as_str().unwrap().contains("cancelled"));

        let conn = db.lock().unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-canc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_status, "cancelled", "dispatch must remain cancelled");
    }

    /// Seed a review dispatch with provider tracking in context (counter-model review).
    fn seed_counter_model_review(db: &Db, dispatch_id: &str, from_provider: &str, target_provider: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-cm', 'Agent CM', 'ch-cc', 'ch-cdx')",
            [],
        ).unwrap();
        let context = serde_json::json!({
            "from_provider": from_provider,
            "target_provider": target_provider,
        }).to_string();
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
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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
        assert!(body.0["error"].as_str().unwrap().contains("provider field is required"));
    }

    #[tokio::test]
    async fn reverse_cross_provider_verdict_allowed() {
        let db = test_db();
        seed_counter_model_review(&db, "dispatch-rev-cross", "codex", "claude");
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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
    async fn legacy_dispatch_without_provider_tracking_allows_no_provider() {
        // Legacy dispatches without from_provider/target_provider in context
        // should still allow verdicts without provider field
        let db = test_db();
        seed_review_card(&db, "dispatch-legacy");
        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            health_registry: None,
        };

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
}
