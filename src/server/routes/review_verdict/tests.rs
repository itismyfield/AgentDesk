use super::*;
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::server::routes::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use std::path::PathBuf;
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
            dispatch_id: None,
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
async fn gemini_cross_provider_allowed() {
    let db = test_db();
    seed_counter_model_review(&db, "dispatch-gemini-cross", "claude", "gemini");
    let state = AppState::test_state(db.clone(), test_engine(&db));

    let (status, body) = submit_verdict(
        State(state),
        Json(SubmitVerdictBody {
            dispatch_id: "dispatch-gemini-cross".to_string(),
            overall: "pass".to_string(),
            items: None,
            notes: None,
            feedback: None,
            commit: None,
            provider: Some("gemini".to_string()),
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.0.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
}

#[tokio::test]
async fn unknown_provider_string_rejected() {
    // Unknown provider string → reject
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
            provider: Some("gpt".to_string()),
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
            dispatch_id: None,
        }),
    )
    .await;

    // #195: Terminal card guard returns 409 CONFLICT (was 500 before #195 refactor)
    assert_eq!(status, StatusCode::CONFLICT);

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

    // #155: Review-decision dispatch must still be pending (not consumed)
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-orig'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_status, "pending",
        "review-decision dispatch must stay pending when accept fails on terminal card"
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
            dispatch_id: None,
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
            dispatch_id: None,
        }),
    )
    .await;
    assert_eq!(status1, StatusCode::OK);

    let conn = db.lock().unwrap();
    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-dup'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (dispatch_type, notify_count): (String, i64) = conn
        .query_row(
            "SELECT td.dispatch_type, \
                    (SELECT COUNT(*) FROM dispatch_outbox o WHERE o.dispatch_id = td.id AND o.action = 'notify') \
             FROM task_dispatches td WHERE td.id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "rework");
    assert_eq!(
        notify_count, 1,
        "review-decision accept must create rework dispatch via canonical notify persistence"
    );
    drop(conn);

    // Second accept should fail — dispatch already consumed
    let (status2, _) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-dup".to_string(),
            decision: "accept".to_string(),
            comment: None,
            dispatch_id: None,
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
            dispatch_id: None,
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
            dispatch_id: None,
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
            dispatch_id: None,
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

    // #266: Verify kanban_cards.review_status is cleared to NULL after accept
    let review_status: Option<String> = conn
        .query_row(
            "SELECT review_status FROM kanban_cards WHERE id = 'card-rs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        review_status, None,
        "#266: review_status should be NULL after accept (was suggestion_pending)"
    );
}

/// #266: Regression test — suggestion_pending review_status must be cleared
/// when a review-decision accept triggers rework (non-terminal transition).
#[tokio::test]
async fn accept_clears_suggestion_pending_review_status() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
             review_status, suggestion_pending_at, created_at, updated_at) \
             VALUES ('card-266', 'Suggestion Pending Bug', 'review', 'agent-1', 'rd-266', \
             'suggestion_pending', datetime('now', '-10 minutes'), datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('rd-266', 'card-266', 'agent-1', 'review-decision', 'pending', 'RD #266', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));

    let (status, _) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-266".to_string(),
            decision: "accept".to_string(),
            comment: None,
            dispatch_id: None,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "accept should succeed");

    let conn = db.lock().unwrap();
    let (review_status, suggestion_pending_at): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT review_status, suggestion_pending_at FROM kanban_cards WHERE id = 'card-266'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        review_status, None,
        "#266: review_status must be NULL after accept, not suggestion_pending"
    );
    assert_eq!(
        suggestion_pending_at, None,
        "#266: suggestion_pending_at must be NULL after accept"
    );
}

/// #266: When the agent already committed new work during the review-decision
/// turn (skip_rework / direct_review_created path), OnReviewEnter sets
/// review_status='reviewing'. The accept cleanup must NOT clear it.
///
/// This test requires a real git worktree for `find_worktree_for_issue()` to
/// detect the differing commit and trigger skip_rework=true.
#[tokio::test]
#[ignore] // CI: requires a real git worktree with a newer commit than reviewed_commit
async fn accept_direct_review_preserves_reviewing_status() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
             review_status, suggestion_pending_at, github_issue_number, created_at, updated_at) \
             VALUES ('card-266dr', 'Direct Review Path', 'review', 'agent-1', 'rd-266dr', \
             'suggestion_pending', datetime('now', '-10 minutes'), 266, datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        // Completed review dispatch with reviewed_commit (needed for skip_rework detection)
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, \
             context, completed_at, created_at, updated_at) \
             VALUES ('review-prev', 'card-266dr', 'agent-1', 'review', 'completed', '[Review R1]', \
             '{\"reviewed_commit\":\"aaa1111\"}', datetime('now', '-5 minutes'), datetime('now', '-10 minutes'), datetime('now'))",
            [],
        ).unwrap();
        // Pending review-decision dispatch
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('rd-266dr', 'card-266dr', 'agent-1', 'review-decision', 'pending', 'RD #266 direct', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-266dr".to_string(),
            decision: "accept".to_string(),
            comment: None,
            dispatch_id: None,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "accept should succeed");
    let resp: serde_json::Value = serde_json::from_value(body.0).unwrap();

    // When skip_rework triggers, direct_review_created should be true
    if resp.get("direct_review_created") == Some(&serde_json::Value::Bool(true)) {
        let conn = db.lock().unwrap();
        let review_status: Option<String> = conn
            .query_row(
                "SELECT review_status FROM kanban_cards WHERE id = 'card-266dr'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_status,
            Some("reviewing".to_string()),
            "#266: direct-review accept must preserve review_status='reviewing' set by OnReviewEnter"
        );
    }
    // else: skip_rework was false (no real worktree found) — normal rework path tested above
}
