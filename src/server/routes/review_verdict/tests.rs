use super::*;
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::server::routes::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use std::path::PathBuf;
use std::sync::MutexGuard;
use std::time::Duration;

macro_rules! sqlite_params {
    ($($param:expr),* $(,)?) => {
        ($(&$param,)*)
    };
}

fn test_db() -> Db {
    crate::db::test_db()
}

fn test_engine(db: &Db) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
}

fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
}

/// Per-test Postgres database lifecycle for the #1238 PG-fixture migration of
/// review_verdict / review-decision handler tests. After PR #1306 these
/// handlers are PG-only so the corresponding tests need a PG pool wired into
/// `AppState`.
struct ReviewVerdictPgDatabase {
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl ReviewVerdictPgDatabase {
    async fn create() -> Self {
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
        let admin_url = pg_test_admin_database_url();
        let database_name = format!("agentdesk_review_verdict_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", pg_test_base_database_url(), database_name);
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "review_verdict handler pg",
        )
        .await
        .expect("create review_verdict postgres test db");

        Self {
            _lifecycle: lifecycle,
            admin_url,
            database_name,
            database_url,
        }
    }

    async fn migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(
            &self.database_url,
            "review_verdict handler pg",
        )
        .await
        .expect("connect + migrate review_verdict postgres test db")
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "review_verdict handler pg",
        )
        .await
        .expect("drop review_verdict postgres test db");
    }
}

fn pg_test_base_database_url() -> String {
    if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
        let trimmed = base.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }
    let user = std::env::var("PGUSER")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "postgres".to_string());
    let password = std::env::var("PGPASSWORD")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let host = std::env::var("PGHOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let port = std::env::var("PGPORT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "5432".to_string());
    match password {
        Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
        None => format!("postgresql://{user}@{host}:{port}"),
    }
}

fn pg_test_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", pg_test_base_database_url(), admin_db)
}

async fn seed_review_card_pg(pool: &sqlx::PgPool, dispatch_id: &str) {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
         VALUES ('agent-1', 'Agent 1', '123', '456')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
         review_status, created_at, updated_at) \
         VALUES ('card-1', 'Review Target', 'review', 'agent-1', $1, 'reviewing', NOW(), NOW())",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
         title, created_at, updated_at) \
         VALUES ($1, 'card-1', 'agent-1', 'review', 'pending', '[Review R1] card-1', NOW(), NOW())",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .unwrap();
}

fn env_lock() -> MutexGuard<'static, ()> {
    crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct WorktreeCommitOverrideGuard {
    _lock: MutexGuard<'static, ()>,
}

impl WorktreeCommitOverrideGuard {
    fn set(commit: &str) -> Self {
        let lock = env_lock();
        super::decision_route::set_test_worktree_commit_override(Some(commit.to_string()));
        Self { _lock: lock }
    }
}

impl Drop for WorktreeCommitOverrideGuard {
    fn drop(&mut self) {
        super::decision_route::clear_test_worktree_commit_override();
    }
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

fn count_active_dispatches(db: &Db, card_id: &str, dispatch_type: &str) -> i64 {
    db.lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = ?2 \
             AND status IN ('pending', 'dispatched')",
            sqlite_params![card_id, dispatch_type],
            |row| row.get(0),
        )
        .unwrap()
}

#[tokio::test]
async fn submit_verdict_pass_pg_marks_done_and_clears_review_status() {
    let pg_db = ReviewVerdictPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    seed_review_card_pg(&pool, "dispatch-pass").await;
    let state =
        AppState::test_state_with_pg(db.clone(), test_engine_with_pg(pool.clone()), pool.clone());

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

    let (card_status, review_status): (String, Option<String>) =
        sqlx::query_as("SELECT status, review_status FROM kanban_cards WHERE id = 'card-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = 'dispatch-pass'")
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(dispatch_status, "completed");
    assert_eq!(card_status, "done");
    assert_eq!(review_status, None);

    pool.close().await;
    pg_db.drop().await;
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
async fn repeated_findings_after_approach_change_pg_creates_session_reset_rework_dispatch() {
    let pg_db = ReviewVerdictPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    seed_review_card_pg(&pool, "dispatch-reset").await;
    sqlx::query(
        "UPDATE kanban_cards
         SET title = 'Reset Test',
             review_round = 3,
             review_notes = 'same validation failure',
             github_issue_number = 420,
             updated_at = NOW()
         WHERE id = 'card-1'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, state, review_round, approach_change_round, updated_at
         ) VALUES (
            'card-1', 'reviewing', 3, 2, NOW()
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    let state =
        AppState::test_state_with_pg(db.clone(), test_engine_with_pg(pool.clone()), pool.clone());
    let (status, _) = submit_verdict(
        State(state),
        Json(SubmitVerdictBody {
            dispatch_id: "dispatch-reset".to_string(),
            overall: "improve".to_string(),
            items: None,
            notes: Some("same validation failure".to_string()),
            feedback: None,
            commit: None,
            provider: None,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (card_status, review_status, latest_dispatch_id): (String, Option<String>, String) =
        sqlx::query_as(
            "SELECT status, review_status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    let rework_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_dispatches
         WHERE kanban_card_id = 'card-1' AND dispatch_type = 'rework'
         AND status IN ('pending', 'dispatched')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(card_status, "in_progress");
    assert_eq!(review_status.as_deref(), Some("rework_pending"));
    assert_eq!(rework_count, 1);

    let (dispatch_type, dispatch_status, title, context): (String, String, String, Option<String>) =
        sqlx::query_as(
            "SELECT dispatch_type, status, title, context FROM task_dispatches WHERE id = $1",
        )
        .bind(&latest_dispatch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dispatch_type, "rework");
    assert_eq!(dispatch_status, "pending");
    assert!(title.contains("[Session Reset R3]"));
    assert!(title.contains("직전 리뷰 피드백"));
    assert!(title.contains("현재 리뷰 피드백"));
    let context_json: serde_json::Value =
        serde_json::from_str(context.as_deref().expect("rework context should exist")).unwrap();
    assert_eq!(context_json["force_new_session"], true);
    assert_eq!(context_json["reset_provider_state"], true);
    assert_eq!(context_json["recreate_tmux"], false);

    let (review_state, session_reset_round): (String, Option<i64>) = sqlx::query_as(
        "SELECT state, session_reset_round FROM card_review_state WHERE card_id = 'card-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(review_state, "rework_pending");
    assert_eq!(session_reset_round, Some(3));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn repeated_findings_after_session_reset_escalates_to_dilemma_pending() {
    let db = test_db();
    seed_review_card(&db, "dispatch-reset-escalate");
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET title = 'Reset Escalation Test',
                 review_round = 4,
                 review_notes = 'same validation failure',
                 github_issue_number = 420,
                 updated_at = datetime('now')
             WHERE id = 'card-1'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, review_round, approach_change_round, session_reset_round, updated_at
             ) VALUES (
                'card-1', 'reviewing', 4, 2, 3, datetime('now')
             )",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), test_engine(&db));
    let (status, _) = submit_verdict(
        State(state),
        Json(SubmitVerdictBody {
            dispatch_id: "dispatch-reset-escalate".to_string(),
            overall: "improve".to_string(),
            items: None,
            notes: Some("same validation failure".to_string()),
            feedback: None,
            commit: None,
            provider: None,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let conn = db.lock().unwrap();
    let (card_status, review_status, blocked_reason): (String, Option<String>, Option<String>) =
        conn.query_row(
            "SELECT status, review_status, blocked_reason FROM kanban_cards WHERE id = 'card-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let rework_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches
             WHERE kanban_card_id = 'card-1' AND dispatch_type = 'rework'
             AND status IN ('pending', 'dispatched')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(review_status.as_deref(), Some("dilemma_pending"));
    assert!(
        blocked_reason
            .as_deref()
            .unwrap_or("")
            .contains("세션 리셋 후에도 동일 finding 반복")
    );
    assert_eq!(card_status, "review");
    assert_eq!(rework_count, 0);

    let (review_state, session_reset_round): (String, Option<i64>) = conn
        .query_row(
            "SELECT state, session_reset_round FROM card_review_state WHERE card_id = 'card-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(review_state, "dilemma_pending");
    assert_eq!(session_reset_round, Some(3));
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
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
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
        sqlite_params![dispatch_id, context],
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
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
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
async fn accept_skip_rework_auto_approves_when_direct_review_has_no_alternate_reviewer() {
    let _worktree_override = WorktreeCommitOverrideGuard::set("bbb2222");
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-nocm', 'Agent No Counter', '123', '')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
             review_status, suggestion_pending_at, github_issue_number, created_at, updated_at) \
             VALUES ('card-skip-fallback', 'Skip Rework Fallback', 'review', 'agent-nocm', \
             'rd-skip-fallback', 'suggestion_pending', datetime('now', '-10 minutes'), 246, \
             datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, context, completed_at, created_at, updated_at) \
             VALUES ('review-skip-fallback', 'card-skip-fallback', 'agent-nocm', 'review', \
             'completed', '[Review R1]', '{\"reviewed_commit\":\"aaa1111\"}', \
             datetime('now', '-5 minutes'), datetime('now', '-10 minutes'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, created_at, updated_at) \
             VALUES ('rd-skip-fallback', 'card-skip-fallback', 'agent-nocm', 'review-decision', \
             'pending', '[Decision] card-skip-fallback', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-skip-fallback".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: Some("bbb2222".to_string()),
            dispatch_id: Some("rd-skip-fallback".to_string()),
            out_of_scope: None,
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "single-provider skip_rework accept should auto-approve: {body:?}"
    );
    assert_eq!(
        body.0["direct_review_created"],
        serde_json::Value::Bool(false),
        "single-provider auto-approve should not report a direct review dispatch"
    );
    assert_eq!(
        body.0["rework_dispatch_created"],
        serde_json::Value::Bool(false),
        "single-provider auto-approve must not create a rework dispatch: {}",
        body.0
    );
    assert_eq!(
        body.0["review_auto_approved"],
        serde_json::Value::Bool(true),
        "single-provider auto-approve must be reported explicitly"
    );

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-skip-fallback'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let rd_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'rd-skip-fallback'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let rd_result: Option<String> = conn
        .query_row(
            "SELECT result FROM task_dispatches WHERE id = 'rd-skip-fallback'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);

    assert_eq!(card_status, "done");
    assert_eq!(rd_status, "cancelled");
    assert!(
        rd_result.as_deref() == Some("{\"reason\":\"auto_cancelled_on_terminal_card\"}")
            || rd_result.as_deref() == Some("{\"reason\":\"js_terminal_cleanup\"}"),
        "cancellation reason must be a terminal cleanup: got {:?}",
        rd_result
    );
    assert_eq!(
        count_active_dispatches(&db, "card-skip-fallback", "review"),
        0,
        "single-provider auto-approve must not leave an active review dispatch behind"
    );
    assert_eq!(
        count_active_dispatches(&db, "card-skip-fallback", "rework"),
        0,
        "single-provider auto-approve must not create a rework dispatch"
    );
    assert_eq!(
        count_active_dispatches(&db, "card-skip-fallback", "review-decision"),
        0,
        "single-provider auto-approve must consume the pending review-decision"
    );
}

#[tokio::test]
async fn accept_rework_failure_keeps_review_decision_pending() {
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, review_status, \
             created_at, updated_at) \
             VALUES ('card-no-agent', 'No Agent Rework Failure', 'review', 'rd-no-agent', \
             'suggestion_pending', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, created_at, updated_at) \
             VALUES ('rd-no-agent', 'card-no-agent', 'ghost-agent', 'review-decision', 'pending', \
             '[Decision] card-no-agent', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-no-agent".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: Some("rd-no-agent".to_string()),
            out_of_scope: None,
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "accept must fail closed when rework dispatch creation is impossible"
    );
    assert!(
        body.0["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no follow-up dispatch created")
    );
    assert_eq!(
        body.0["pending_dispatch_id"],
        serde_json::Value::String("rd-no-agent".to_string())
    );

    let conn = db.lock().unwrap();
    let rd_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'rd-no-agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);

    assert_eq!(
        rd_status, "pending",
        "fail-closed accept must leave the review-decision dispatch pending"
    );
    assert_eq!(count_active_dispatches(&db, "card-no-agent", "review"), 0);
    assert_eq!(count_active_dispatches(&db, "card-no-agent", "rework"), 0);
    assert_eq!(
        count_active_dispatches(&db, "card-no-agent", "review-decision"),
        1
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
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
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
    let (status1, body1) = submit_review_decision(
        State(state.clone()),
        Json(ReviewDecisionBody {
            card_id: "card-dup".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
        }),
    )
    .await;
    assert_eq!(
        status1,
        StatusCode::OK,
        "unexpected first accept body: {}",
        body1.0
    );

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
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
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
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
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
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
        }),
    )
    .await;
    assert_eq!(status2, StatusCode::CONFLICT);
}

/// #110: submit_verdict with "pass" must drain pending transitions so that
/// OnCardTerminal fires immediately (not deferred to next tick).
/// This ensures auto-queue continuation path is triggered.
#[tokio::test]
async fn submit_verdict_pass_pg_fires_terminal_hook_via_drain() {
    let pg_db = ReviewVerdictPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();
    seed_review_card_pg(&pool, "dispatch-drain").await;

    // PG migrations already create auto_queue_runs and auto_queue_entries — only seed the rows.
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, status, agent_id) \
         VALUES ('run-drain', 'active', 'agent-1')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
         VALUES ('entry-drain', 'run-drain', 'card-1', 'agent-1', 'dispatched', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let state =
        AppState::test_state_with_pg(db.clone(), test_engine_with_pg(pool.clone()), pool.clone());

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

    // Card must be done
    let card_status: String =
        sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'card-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(card_status, "done");

    // completed_at must be set (proves OnCardTerminal or transition_status fired)
    let completed_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT completed_at FROM kanban_cards WHERE id = 'card-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        completed_at.is_some(),
        "completed_at must be set — proves terminal hook fired via drain"
    );

    // auto_queue_entry must be 'done' (proves OnCardTerminal → auto-queue.js ran)
    let entry_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-drain'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        entry_status, "done",
        "auto_queue_entry must be marked done by terminal hook"
    );

    pool.close().await;
    pg_db.drop().await;
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
    let result = crate::dispatch::create_dispatch_record_sqlite_test(
        &db,
        "card-dup",
        "agent-1",
        "review-decision",
        "[New RD]",
        &serde_json::json!({"verdict": "improve"}),
        crate::dispatch::DispatchCreateOptions::default(),
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
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
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
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
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

#[test]
fn latest_completed_review_lookup_prefers_completed_at() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-rv', 'Agent RV', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES ('card-review-ts', 'Review Timestamp Card', 'review', 'agent-rv', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, completed_at, created_at, updated_at) \
             VALUES ('review-older-finish', 'card-review-ts', 'agent-rv', 'review', 'completed', '[Review R1]', \
             '{\"reviewed_commit\":\"old1111\"}', datetime('now', '-20 minutes'), datetime('now', '-30 minutes'), datetime('now', '-1 minute'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, completed_at, created_at, updated_at) \
             VALUES ('review-newer-finish', 'card-review-ts', 'agent-rv', 'review', 'completed', '[Review R2]', \
             '{\"reviewed_commit\":\"new2222\"}', datetime('now', '-5 minutes'), datetime('now', '-40 minutes'), datetime('now', '-10 minutes'))",
            [],
        )
        .unwrap();
    }

    let conn = db.lock().unwrap();
    let latest_context: Option<String> = conn
        .query_row(
            "SELECT context FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review' AND status = 'completed' \
             ORDER BY COALESCE(completed_at, updated_at) DESC, updated_at DESC, rowid DESC LIMIT 1",
            ["card-review-ts"],
            |row| row.get(0),
        )
        .unwrap();
    let latest_reviewed_commit = latest_context
        .and_then(|ctx| serde_json::from_str::<serde_json::Value>(&ctx).ok())
        .and_then(|value| {
            value
                .get("reviewed_commit")
                .and_then(|commit| commit.as_str())
                .map(str::to_string)
        });

    assert_eq!(
        latest_reviewed_commit,
        Some("new2222".to_string()),
        "skip_rework lookup must follow completed_at rather than a stale updated_at"
    );
}

/// #1977: review-decision accept may know the commit explicitly from the
/// agent's final response. That commit must be used before worktree inference.
#[tokio::test]
async fn accept_skip_rework_diagnostics_prefers_explicit_commit() {
    let _worktree_override = WorktreeCommitOverrideGuard::set("aaa1111");
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, github_issue_number, created_at, updated_at) \
             VALUES ('card-1977-diag', 'Commit diagnostics', 'review', 1977, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, title, context, completed_at, created_at, updated_at) \
             VALUES ('review-1977-diag', 'card-1977-diag', 'review', 'completed', '[Review R1]', \
             '{\"reviewed_commit\":\"aaa1111\"}', datetime('now'), datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), test_engine(&db));

    let diagnostics = evaluate_accept_skip_rework(&state, "card-1977-diag", Some("bbb2222")).await;

    assert!(diagnostics.skip_rework);
    assert_eq!(diagnostics.current_commit.as_deref(), Some("bbb2222"));
    assert_eq!(diagnostics.current_commit_source, Some("request"));
    assert_eq!(
        diagnostics.reason,
        "current_commit_differs_from_reviewed_commit"
    );
}

/// #266: When the agent already committed new work during the review-decision
/// turn (skip_rework / direct_review_created path), OnReviewEnter sets
/// review_status='reviewing'. The accept cleanup must NOT clear it.
#[tokio::test]
async fn accept_direct_review_pg_preserves_reviewing_status() {
    // #1977: an explicit review-decision commit must win over worktree
    // inference. If inference won here, it would match the reviewed commit and
    // incorrectly create a rework dispatch.
    let _worktree_override = WorktreeCommitOverrideGuard::set("aaa1111");
    let pg_db = ReviewVerdictPgDatabase::create().await;
    let pool = pg_db.migrate().await;
    let db = test_db();

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
         VALUES ('agent-1', 'Agent 1', '123', '456')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
         review_status, review_round, suggestion_pending_at, github_issue_number, created_at, updated_at) \
         VALUES ('card-266dr', 'Direct Review Path', 'review', 'agent-1', 'rd-266dr', \
         'suggestion_pending', 1, NOW() - INTERVAL '10 minutes', 266, NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Completed review dispatch with reviewed_commit (needed for skip_rework detection)
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, \
         context, completed_at, created_at, updated_at) \
         VALUES ('review-prev', 'card-266dr', 'agent-1', 'review', 'completed', '[Review R1]', \
         '{\"reviewed_commit\":\"aaa1111\"}', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '10 minutes', NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Pending review-decision dispatch
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
         VALUES ('rd-266dr', 'card-266dr', 'agent-1', 'review-decision', 'pending', 'RD #266 direct', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let state =
        AppState::test_state_with_pg(db.clone(), test_engine_with_pg(pool.clone()), pool.clone());

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-266dr".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: Some("bbb2222".to_string()),
            dispatch_id: None,
            out_of_scope: None,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "accept should succeed");
    let resp: serde_json::Value = serde_json::from_value(body.0).unwrap();
    assert_eq!(
        resp.get("direct_review_created"),
        Some(&serde_json::Value::Bool(true)),
        "skip_rework accept must create a direct review dispatch"
    );
    assert_eq!(
        resp.get("rework_dispatch_created"),
        Some(&serde_json::Value::Bool(false)),
        "skip_rework accept must not create a rework dispatch"
    );
    assert_eq!(
        resp.get("skip_rework"),
        Some(&serde_json::Value::Bool(true)),
        "explicit commit_sha should trigger skip_rework when it differs from reviewed_commit"
    );
    assert_eq!(
        resp.pointer("/skip_rework_diagnostics/current_commit_source")
            .and_then(|value| value.as_str()),
        Some("request"),
        "commit_sha in the request must take precedence over inferred worktree HEAD"
    );
    assert_eq!(
        resp.pointer("/skip_rework_diagnostics/current_commit")
            .and_then(|value| value.as_str()),
        Some("bbb2222")
    );

    let review_status: Option<String> =
        sqlx::query_scalar("SELECT review_status FROM kanban_cards WHERE id = 'card-266dr'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        review_status,
        Some("reviewing".to_string()),
        "#266: direct-review accept must preserve review_status='reviewing' set by OnReviewEnter"
    );
    let review_round: i64 =
        sqlx::query_scalar("SELECT review_round FROM kanban_cards WHERE id = 'card-266dr'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        review_round, 2,
        "#487: direct-review accept must advance review_round for the new review cycle"
    );
    let review_title: String = sqlx::query_scalar(
        "SELECT title FROM task_dispatches \
         WHERE kanban_card_id = 'card-266dr' AND dispatch_type = 'review' \
         AND status IN ('pending', 'dispatched') \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        review_title, "[Review R2] card-266dr",
        "#487: direct-review accept must create an R2 review dispatch title"
    );

    pool.close().await;
    pg_db.drop().await;
}

// ===========================================================================
// #2341 / #2200 sub-3 REDESIGN tests (completed-review-context binding).
//
// These tests exercise the PRODUCTION flow: a review dispatch that has
// already completed by the time `/api/review-decision` is called, plus a
// pending review-decision dispatch awaiting the operator's verdict. This
// replaces PR #2336's tests which artificially seeded an active review
// dispatch — that branch never fires in production (per #2341 Codex r3).
// ===========================================================================

/// Build a real on-disk git repo whose HEAD commit subject references an
/// unrelated issue. The tri-state scope check returns `OutOfScope` when the
/// reviewed_commit lives in this repo and the card's issue is something
/// else. Reused from PR #2336.
fn init_test_git_repo_out_of_scope_2341(card_issue_number: i64) -> (tempfile::TempDir, String) {
    fn run_git(repo: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
        if !output.status.success() {
            panic!(
                "git {:?} failed: {}\n{}",
                args,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
    let other_issue = card_issue_number + 4242;
    let tempdir = tempfile::tempdir().unwrap();
    let repo = tempdir.path();
    run_git(repo, &["init", "-q"]);
    run_git(repo, &["config", "user.email", "test@example.invalid"]);
    run_git(repo, &["config", "user.name", "Review Verdict Test"]);
    run_git(repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "out-of-scope fixture\n").unwrap();
    run_git(repo, &["add", "README.md"]);
    run_git(
        repo,
        &[
            "commit",
            "-q",
            "-m",
            &format!("fix: unrelated work (#{other_issue})"),
        ],
    );
    let commit = run_git(repo, &["rev-parse", "HEAD"]);
    (tempdir, commit)
}

/// Seed a card with the production flow shape:
///   * `card_review_state` row with `review_round = 1`, `review_entered_at = now()`
///   * one **completed** `review` dispatch (with context pointing at the
///     out-of-scope commit + repo) — this is what `latest_completed_review_dispatch_pg_first`
///     surfaces in the close path.
///   * one **pending** `review-decision` dispatch — the operator is about to
///     submit dispute+out_of_scope.
fn seed_completed_review_then_pending_decision(
    db: &Db,
    card_id: &str,
    card_issue_number: i64,
    rv_id: &str,
    rd_id: &str,
    reviewed_commit: &str,
    repo_dir_path: &str,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
         VALUES ('agent-oos2', 'OOS2', '101', '202')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
         review_status, github_issue_number, created_at, updated_at) \
         VALUES (?1, 'OOS Card', 'review', 'agent-oos2', ?2, 'suggestion_pending', ?3, datetime('now'), datetime('now'))",
        sqlite_params![card_id, rd_id, card_issue_number],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO card_review_state (card_id, review_round, state, review_entered_at, updated_at) \
         VALUES (?1, 1, 'reviewing', datetime('now'), datetime('now'))",
        sqlite_params![card_id],
    )
    .unwrap();
    let context_json =
        format!(r#"{{"reviewed_commit":"{reviewed_commit}","target_repo":"{repo_dir_path}"}}"#);
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
         title, context, completed_at, created_at, updated_at) \
         VALUES (?1, ?2, 'agent-oos2', 'review', 'completed', '[Review R1] completed', ?3, \
                 datetime('now'), datetime('now'), datetime('now'))",
        sqlite_params![rv_id, card_id, context_json.as_str()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
         title, created_at, updated_at) \
         VALUES (?1, ?2, 'agent-oos2', 'review-decision', 'pending', '[Decision] pending', \
                 datetime('now'), datetime('now'))",
        sqlite_params![rd_id, card_id],
    )
    .unwrap();
}

/// HAPPY PATH: dispute+out_of_scope against a card whose completed review
/// reviewed an out-of-scope commit must:
///   * return 200 OK with outcome = scope_mismatch_closed
///   * finalize the pending review-decision dispatch (status=completed)
///   * embed the lifecycle generation in result for idempotent retry
///   * transition the card to terminal
#[tokio::test]
async fn redesign_dispute_oos_with_completed_review_closes_scope_mismatch() {
    let (repo_dir, commit_sha) = init_test_git_repo_out_of_scope_2341(2341001);
    let repo_dir_path = repo_dir.path().to_string_lossy().into_owned();
    let db = test_db();
    let engine = test_engine(&db);
    seed_completed_review_then_pending_decision(
        &db,
        "card-2341-happy",
        2341001,
        "rv-2341-happy-completed",
        "rd-2341-happy-pending",
        &commit_sha,
        &repo_dir_path,
    );

    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-2341-happy".to_string(),
            decision: "dispute".to_string(),
            comment: Some("Out-of-scope finding from stacked branch".to_string()),
            commit_sha: None,
            dispatch_id: Some("rd-2341-happy-pending".to_string()),
            out_of_scope: Some(true),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "happy-path out_of_scope close must return 200 against a completed review; body = {:?}",
        body.0
    );
    assert_eq!(body.0["outcome"].as_str().unwrap(), "scope_mismatch_closed");
    assert_eq!(
        body.0["review_dispatch_id"].as_str().unwrap(),
        "rv-2341-happy-completed",
        "must surface the completed review dispatch id (the production-flow binding)"
    );

    let conn = db.lock().unwrap();
    let (card_status, review_status): (String, Option<String>) = conn
        .query_row(
            "SELECT status, review_status FROM kanban_cards WHERE id = 'card-2341-happy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let rd_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'rd-2341-happy-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let rd_result: String = conn
        .query_row(
            "SELECT result FROM task_dispatches WHERE id = 'rd-2341-happy-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(card_status, "done");
    assert_eq!(review_status, None);
    assert_eq!(rd_status, "completed");
    let rd_result_json: serde_json::Value = serde_json::from_str(&rd_result).unwrap();
    assert_eq!(
        rd_result_json["outcome"].as_str().unwrap(),
        "scope_mismatch_closed"
    );
    assert!(
        rd_result_json.get("lifecycle_generation").is_some(),
        "lifecycle generation must be embedded in result for idempotent retry"
    );
    assert_eq!(
        rd_result_json["review_dispatch_id"].as_str().unwrap(),
        "rv-2341-happy-completed"
    );
}

/// REFUSE — Unknown scope verification (transient PG/git error): when the
/// scope check returns Unknown (e.g. repo dir unavailable in this test
/// because we didn't seed target_repo), the close path must refuse with 503
/// and leave the card UNCHANGED. A transient failure must NEVER terminalize
/// a card.
#[tokio::test]
async fn redesign_dispute_oos_unknown_scope_returns_503_card_unchanged() {
    let db = test_db();
    let engine = test_engine(&db);
    // Seed without a real target_repo so the scope check returns Unknown.
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-u', 'U', '301', '302')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
             review_status, github_issue_number, created_at, updated_at) \
             VALUES ('card-2341-unk', 'Unknown Test', 'review', 'agent-u', \
                     'rd-2341-unk-pending', 'suggestion_pending', 2341002, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // target_repo points to a non-existent path → repo_dir unavailable → Unknown.
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, context, completed_at, created_at, updated_at) \
             VALUES ('rv-2341-unk-completed', 'card-2341-unk', 'agent-u', 'review', 'completed', \
                     '[Review R1] completed', \
                     '{\"reviewed_commit\":\"abc1234567890abcdef\",\"target_repo\":\"/nonexistent/path/xyz\"}', \
                     datetime('now'), datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, created_at, updated_at) \
             VALUES ('rd-2341-unk-pending', 'card-2341-unk', 'agent-u', 'review-decision', 'pending', \
                     '[Decision] pending', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-2341-unk".to_string(),
            decision: "dispute".to_string(),
            comment: Some("Unknown scope check".to_string()),
            commit_sha: None,
            dispatch_id: Some("rd-2341-unk-pending".to_string()),
            out_of_scope: Some(true),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "Unknown scope verification must refuse with 503; body = {:?}",
        body.0
    );
    assert_eq!(body.0["reason"].as_str().unwrap(), "scope_check_unknown");

    // Card and dispatches must be unchanged.
    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-2341-unk'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let rd_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'rd-2341-unk-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(card_status, "review", "card must be unchanged on Unknown");
    assert_eq!(
        rd_status, "pending",
        "pending review-decision must be unchanged on Unknown"
    );
}

/// REFUSE — card re-opened after review dispatch completed: between the
/// snapshot and the close tx, a new dispatch was created (= card is no
/// longer the same generation). Close must refuse with 409 stale.
#[tokio::test]
async fn redesign_dispute_oos_card_reopened_returns_409_stale() {
    let (repo_dir, commit_sha) = init_test_git_repo_out_of_scope_2341(2341003);
    let repo_dir_path = repo_dir.path().to_string_lossy().into_owned();
    let db = test_db();
    let engine = test_engine(&db);
    seed_completed_review_then_pending_decision(
        &db,
        "card-2341-stale",
        2341003,
        "rv-2341-stale-completed",
        "rd-2341-stale-pending",
        &commit_sha,
        &repo_dir_path,
    );
    // Simulate a re-open: bump review_round on card_review_state to make the
    // snapshot taken inside the atomic close diverge from the snapshot taken
    // before the close. Because the sqlite test path doesn't have FOR UPDATE,
    // we simulate the post-snapshot drift by leaving the snapshot capture
    // out of date by mutating the card right between the helper's read and
    // its atomic check. The simplest test is: change the review_round
    // BEFORE calling the API so the close-tx re-read mismatches the pre-tx
    // snapshot. Both snapshots will agree under the test fixture (no
    // concurrent mutation), so we instead force a divergence by patching
    // the result-recorded lifecycle generation in the idempotent path.
    //
    // The cleanest sqlite-fixture test of lifecycle stale: after a
    // SUCCESSFUL close, simulate a re-open (advance review_round + set a
    // new latest_dispatch_id) and retry — the idempotent resume path must
    // refuse because the recorded lifecycle generation no longer matches.
    let state = AppState::test_state(db.clone(), engine);

    // First call: succeed.
    let (status1, _body1) = submit_review_decision(
        State(state.clone()),
        Json(ReviewDecisionBody {
            card_id: "card-2341-stale".to_string(),
            decision: "dispute".to_string(),
            comment: Some("First close".to_string()),
            commit_sha: None,
            dispatch_id: Some("rd-2341-stale-pending".to_string()),
            out_of_scope: Some(true),
        }),
    )
    .await;
    assert_eq!(status1, StatusCode::OK, "first close must succeed");

    // Simulate a card re-open: advance review_round, set a fresh
    // latest_dispatch_id, AND put the card back into a non-terminal
    // status. The non-terminal status is what tells the idempotent path
    // "the prior close was partial (or the card was reopened) — apply the
    // strict generation marker check before resuming". A terminal card
    // that retains the recorded generation matches the successful-close
    // idempotent path instead (Codex round-2 [medium] fix).
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE card_review_state SET review_round = 2, review_entered_at = datetime('now', '+1 second') WHERE card_id = 'card-2341-stale'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = 'rd-reopened-new', status = 'review' WHERE id = 'card-2341-stale'",
            [],
        )
        .unwrap();
    }

    // Retry the idempotent path: must refuse with 409 (lifecycle generation
    // mismatch). The dispatch is still terminal so the pending-lookup
    // returns None and we fall into the idempotent-resume branch.
    let (status2, body2) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-2341-stale".to_string(),
            decision: "dispute".to_string(),
            comment: Some("Stale retry on re-opened card".to_string()),
            commit_sha: None,
            dispatch_id: Some("rd-2341-stale-pending".to_string()),
            out_of_scope: Some(true),
        }),
    )
    .await;

    assert_eq!(
        status2,
        StatusCode::CONFLICT,
        "stale retry on re-opened card must refuse with 409; body = {:?}",
        body2.0
    );
    assert_eq!(
        body2.0["reason"].as_str().unwrap(),
        "lifecycle_generation_mismatch",
        "must report lifecycle_generation_mismatch"
    );
}

/// IDEMPOTENT: second call with the same payload after a successful close
/// must return 200 already-finalized. Composes with sub-fix 1's
/// proof-of-finalization model (the recorded
/// `outcome = scope_mismatch_closed` is sufficient proof; sub-1's branch
/// does not fire because we detect the outcome first).
#[tokio::test]
async fn redesign_dispute_oos_idempotent_retry_returns_200_already_finalized() {
    let (repo_dir, commit_sha) = init_test_git_repo_out_of_scope_2341(2341004);
    let repo_dir_path = repo_dir.path().to_string_lossy().into_owned();
    let db = test_db();
    let engine = test_engine(&db);
    seed_completed_review_then_pending_decision(
        &db,
        "card-2341-idem",
        2341004,
        "rv-2341-idem-completed",
        "rd-2341-idem-pending",
        &commit_sha,
        &repo_dir_path,
    );
    let state = AppState::test_state(db.clone(), engine);

    let (status1, _body1) = submit_review_decision(
        State(state.clone()),
        Json(ReviewDecisionBody {
            card_id: "card-2341-idem".to_string(),
            decision: "dispute".to_string(),
            comment: Some("first call".to_string()),
            commit_sha: None,
            dispatch_id: Some("rd-2341-idem-pending".to_string()),
            out_of_scope: Some(true),
        }),
    )
    .await;
    assert_eq!(status1, StatusCode::OK, "first close must succeed");

    // Replay with the same payload. The pending-lookup now misses (the
    // review-decision is completed), but the idempotent-resume branch must
    // detect the prior scope_mismatch_closed finalize and return 200.
    let (status2, body2) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-2341-idem".to_string(),
            decision: "dispute".to_string(),
            comment: Some("second call".to_string()),
            commit_sha: None,
            dispatch_id: Some("rd-2341-idem-pending".to_string()),
            out_of_scope: Some(true),
        }),
    )
    .await;

    assert_eq!(
        status2,
        StatusCode::OK,
        "idempotent retry must return 200 already_finalized; body = {:?}",
        body2.0
    );
    assert_eq!(
        body2.0["outcome"].as_str().unwrap(),
        "scope_mismatch_closed"
    );
    assert_eq!(
        body2.0["pending_dispatch_id"].as_str().unwrap(),
        "rd-2341-idem-pending"
    );
    assert!(
        body2.0["message"]
            .as_str()
            .unwrap()
            .contains("already finalized")
            || body2.0["message"]
                .as_str()
                .unwrap()
                .contains("already_finalized")
            || body2.0["message"].as_str().unwrap().contains("idempotent"),
        "must indicate idempotent already-finalized response; got: {}",
        body2.0["message"].as_str().unwrap_or("(missing message)")
    );
}

/// REGRESSION: in-scope dispute (no out_of_scope flag) must take the legacy
/// path unchanged. The new shortcut must NOT silently swallow this.
#[tokio::test]
async fn redesign_dispute_without_out_of_scope_flag_uses_legacy_path() {
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-l', 'L', '401', '402')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
             review_status, created_at, updated_at) \
             VALUES ('card-2341-leg', 'Legacy Test', 'review', 'agent-l', 'rd-2341-leg-pending', \
                     'suggestion_pending', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, created_at, updated_at) \
             VALUES ('rd-2341-leg-pending', 'card-2341-leg', 'agent-l', 'review-decision', 'pending', \
                     '[Decision] pending', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status_none, body_none) = submit_review_decision(
        State(state.clone()),
        Json(ReviewDecisionBody {
            card_id: "card-2341-leg".to_string(),
            decision: "dispute".to_string(),
            comment: Some("legacy in-scope dispute".to_string()),
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: None,
        }),
    )
    .await;
    assert_ne!(
        status_none,
        StatusCode::OK,
        "legacy in-scope dispute path must not silently take the scope_mismatch_closed shortcut; got 200 with body {:?}",
        body_none.0
    );
    assert!(
        body_none.0.get("outcome").and_then(|v| v.as_str()) != Some("scope_mismatch_closed"),
        "in-scope dispute must NOT report scope_mismatch_closed; body = {:?}",
        body_none.0
    );

    let (status_false, body_false) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-2341-leg".to_string(),
            decision: "dispute".to_string(),
            comment: Some("explicit out_of_scope=false".to_string()),
            commit_sha: None,
            dispatch_id: None,
            out_of_scope: Some(false),
        }),
    )
    .await;
    assert_ne!(
        status_false,
        StatusCode::OK,
        "out_of_scope=false must behave like the legacy path"
    );
    assert!(
        body_false.0.get("outcome").and_then(|v| v.as_str()) != Some("scope_mismatch_closed"),
        "out_of_scope=false must NOT report scope_mismatch_closed; body = {:?}",
        body_false.0
    );
}

/// PARTIAL-CLOSE RESUME (Codex finding [high] fix): if the first call
/// flipped the dispatch to scope_mismatch_closed but never transitioned the
/// card (e.g. transition failure between tx commit and the transition
/// call), a retry must FINISH the close — not falsely report
/// already-finalized while leaving the card in `review`.
///
/// We simulate the partial state by:
///   1. Manually flipping the review-decision dispatch to scope_mismatch_closed
///      with a recorded lifecycle_generation matching the current card state.
///   2. Leaving the card in `review` (no transition).
///   3. Calling the API; it should detect non-terminal card + prior
///      finalize, resume transition + cleanup, and return 200 resumed=true.
#[tokio::test]
async fn redesign_dispute_oos_idempotent_retry_resumes_partial_close() {
    let (repo_dir, commit_sha) = init_test_git_repo_out_of_scope_2341(2341005);
    let _ = (repo_dir, commit_sha); // not needed; we synthesize the partial state directly
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-pc', 'PC', '501', '502')",
            [],
        )
        .unwrap();
        // Card stuck in `review` after partial close.
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
             review_status, github_issue_number, created_at, updated_at) \
             VALUES ('card-2341-pc', 'Partial Close', 'review', 'agent-pc', 'rd-2341-pc', \
                     'suggestion_pending', 2341005, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (card_id, review_round, state, review_entered_at, updated_at) \
             VALUES ('card-2341-pc', 1, 'reviewing', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // The completed review (id pinned so source_review_dispatch_id can resolve it).
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, context, completed_at, created_at, updated_at) \
             VALUES ('rv-2341-pc-completed', 'card-2341-pc', 'agent-pc', 'review', 'completed', \
                     '[Review R1] completed', \
                     '{\"reviewed_commit\":\"deadbeef1234567\",\"target_repo\":\"/nonexistent\"}', \
                     datetime('now'), datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // The review-decision dispatch already in scope_mismatch_closed state
        // (simulating "atomic tx committed; transition crashed").
        let result_json = serde_json::json!({
            "decision": "dispute",
            "outcome": "scope_mismatch_closed",
            "completion_source": "review_decision_api",
            "review_dispatch_id": "rv-2341-pc-completed",
            "reviewed_commit": "deadbeef1234567",
            "lifecycle_generation": {
                "latest_dispatch_id": "rd-2341-pc",
                "review_round": 1,
                "review_entered_at_iso": null
            }
        });
        // sqlite test stores TIMESTAMPTZ via datetime() which is a string;
        // the actual `review_entered_at_iso` extraction in `card_lifecycle_snapshot_pg_first`
        // for sqlite reads the column as a String, so we match by setting
        // review_entered_at to NULL on card_review_state to keep the
        // generation marker simple.
        conn.execute(
            "UPDATE card_review_state SET review_entered_at = NULL WHERE card_id = 'card-2341-pc'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, result, completed_at, created_at, updated_at) \
             VALUES ('rd-2341-pc', 'card-2341-pc', 'agent-pc', 'review-decision', 'completed', \
                     '[Decision] partial', ?1, datetime('now'), datetime('now'), datetime('now'))",
            sqlite_params![result_json.to_string()],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-2341-pc".to_string(),
            decision: "dispute".to_string(),
            comment: Some("retry after partial close".to_string()),
            commit_sha: None,
            dispatch_id: Some("rd-2341-pc".to_string()),
            out_of_scope: Some(true),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "partial-close retry must succeed; body = {:?}",
        body.0
    );
    assert_eq!(
        body.0["resumed"].as_bool().unwrap_or(false),
        true,
        "must report resumed=true so callers know terminalization advanced; body = {:?}",
        body.0
    );

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-2341-pc'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        card_status, "done",
        "card must reach terminal after partial-close resume; got {}",
        card_status
    );
}

/// Codex round-2 [medium] regression: if the review-decision context
/// includes a `source_review_dispatch_id` but that id does NOT resolve to
/// a completed review (missing / uncompleted / different card), the close
/// must FAIL CLOSED (409) instead of silently falling back to
/// latest-completed and binding to a different (potentially unrelated)
/// review row.
#[tokio::test]
async fn redesign_dispute_oos_unresolved_source_id_fails_closed() {
    let (repo_dir, commit_sha) = init_test_git_repo_out_of_scope_2341(2341006);
    let repo_dir_path = repo_dir.path().to_string_lossy().into_owned();
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-srid', 'SRID', '601', '602')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
             review_status, github_issue_number, created_at, updated_at) \
             VALUES ('card-2341-srid', 'SRID', 'review', 'agent-srid', 'rd-2341-srid', \
                     'suggestion_pending', 2341006, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (card_id, review_round, state, review_entered_at, updated_at) \
             VALUES ('card-2341-srid', 1, 'reviewing', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // A DIFFERENT (latest) completed review whose context references
        // commit_sha. This is what latest-completed-fallback would bind to.
        // If we silently fell back, the close would proceed (commit is
        // out-of-scope). Failing closed prevents that.
        let context_json =
            format!(r#"{{"reviewed_commit":"{commit_sha}","target_repo":"{repo_dir_path}"}}"#);
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, context, completed_at, created_at, updated_at) \
             VALUES ('rv-2341-srid-latest', 'card-2341-srid', 'agent-srid', 'review', 'completed', \
                     '[Review R1] latest', ?1, datetime('now'), datetime('now'), datetime('now'))",
            sqlite_params![context_json.as_str()],
        )
        .unwrap();
        // Review-decision context references an UNRESOLVED source id
        // (e.g. the original source row was deleted / failed / cancelled).
        let rd_context = r#"{"source_review_dispatch_id":"rv-deleted-source","verdict":"improve"}"#;
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
             title, context, created_at, updated_at) \
             VALUES ('rd-2341-srid', 'card-2341-srid', 'agent-srid', 'review-decision', 'pending', \
                     '[Decision] srid', ?1, datetime('now'), datetime('now'))",
            sqlite_params![rd_context],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-2341-srid".to_string(),
            decision: "dispute".to_string(),
            comment: Some("unresolved source id".to_string()),
            commit_sha: None,
            dispatch_id: Some("rd-2341-srid".to_string()),
            out_of_scope: Some(true),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "unresolved source_review_dispatch_id must fail closed; body = {:?}",
        body.0
    );
    assert_eq!(
        body.0["reason"].as_str().unwrap(),
        "source_review_unresolved",
        "must report source_review_unresolved reason"
    );

    // Card and dispatch must be unchanged.
    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-2341-srid'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let rd_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'rd-2341-srid'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        card_status, "review",
        "card must be unchanged on fail-closed"
    );
    assert_eq!(
        rd_status, "pending",
        "dispatch must be unchanged on fail-closed"
    );
}

// ---------------------------------------------------------------------------
// #2200 sub-fix 4 (`stale-dispatch-mismatch`) regression tests.
//
// Symptom: POST /api/review-decision returned 409 'no pending review-decision
// dispatch' even though the originating review-decision dispatch row was
// still status='dispatched'. The pending-lookup joins through
// card_review_state.pending_dispatch_id / kanban_cards.latest_dispatch_id; if
// those link rows were cleared while the dispatch row itself stayed alive,
// the lookup missed it. These tests pin the by-id fallback that recovers
// that case (and the negative paths that must stay 409 / 404).
// ---------------------------------------------------------------------------

/// Setup: insert a card whose `latest_dispatch_id` is `None`, plus a
/// review-decision dispatch row in status `dispatched` that points at the
/// card. Mirrors the stale-dispatch-mismatch production state: the dispatch
/// row is alive but neither `latest_dispatch_id` nor `card_review_state`
/// links to it any more.
fn seed_unlinked_dispatched_review_decision(
    db: &Db,
    card_id: &str,
    dispatch_id: &str,
    dispatch_status: &str,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
         VALUES ('agent-1', 'Agent 1', '123', '456')",
        [],
    )
    .unwrap();
    // Card has NO latest_dispatch_id link → canonical pending lookup will miss.
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at) \
         VALUES (?1, 'Stale Dispatch Card', 'review', 'agent-1', NULL, 'suggestion_pending', datetime('now'), datetime('now'))",
        sqlite_params![card_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
         VALUES (?1, ?2, 'agent-1', 'review-decision', ?3, '[Review Decision]', datetime('now'), datetime('now'))",
        sqlite_params![dispatch_id, card_id, dispatch_status],
    )
    .unwrap();
}

/// #2200 sub-fix 4: submitted dispatch_id matching a still-dispatched row
/// must be honored even when the canonical pending lookup misses it
/// (link rows cleared but dispatch row alive).
#[tokio::test]
async fn stale_dispatch_mismatch_dispatch_id_recovers_live_dispatched_row() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_unlinked_dispatched_review_decision(
        &db,
        "card-stale-dd",
        "051cbf56-8e95-4bee-97d4-18268a3802e3",
        "dispatched",
    );
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-stale-dd".to_string(),
            decision: "accept".to_string(),
            comment: Some("recovered via dispatch_id".to_string()),
            commit_sha: None,
            dispatch_id: Some("051cbf56-8e95-4bee-97d4-18268a3802e3".to_string()),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "by-id fallback must turn the 409 stale-dispatch-mismatch into a 200 accept: {}",
        body.0
    );

    // Originating review-decision dispatch must end up completed.
    let conn = db.lock().unwrap();
    let rd_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            ["051cbf56-8e95-4bee-97d4-18268a3802e3"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        rd_status, "completed",
        "the recovered review-decision dispatch must be finalized by accept"
    );
}

/// Negative: caller supplies a dispatch_id that does not exist (or points at
/// a different card / non-review-decision type). Must be 404, not 200 / 409.
#[tokio::test]
async fn stale_dispatch_mismatch_unknown_dispatch_id_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        // Card exists but has no live review-decision dispatch at all.
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at) \
             VALUES ('card-stale-nf', 'No Dispatch Card', 'review', 'agent-1', NULL, NULL, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-stale-nf".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: Some("00000000-0000-0000-0000-000000000000".to_string()),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "unknown dispatch_id must be 404, not 409 or 200: {}",
        body.0
    );
}

/// Negative: caller supplies a dispatch_id that points at a different card's
/// review-decision dispatch. Authorization gate must reject as 404 (we don't
/// confirm "exists but belongs to another card" — that would leak whether a
/// UUID is bound).
#[tokio::test]
async fn stale_dispatch_mismatch_cross_card_dispatch_id_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        // Two cards. Dispatch belongs to card A; caller submits decision for
        // card B with card A's dispatch_id.
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at) \
             VALUES ('card-stale-A', 'A', 'review', 'agent-1', NULL, NULL, datetime('now'), datetime('now')), \
                    ('card-stale-B', 'B', 'review', 'agent-1', NULL, NULL, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-A', 'card-stale-A', 'agent-1', 'review-decision', 'dispatched', '[Review Decision]', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-stale-B".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: Some("dispatch-A".to_string()),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "dispatch_id belonging to another card must be rejected as 404: {}",
        body.0
    );

    // Critically: dispatch-A must remain `dispatched`, not silently consumed.
    let conn = db.lock().unwrap();
    let rd_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-A'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        rd_status, "dispatched",
        "cross-card binding attempt must NOT alter the other card's dispatch"
    );
}

/// Negative: caller supplies a dispatch_id pointing at a row that is already
/// terminal (completed/failed/cancelled). Sub-fix 4 deliberately falls
/// through to the canonical "no pending" 409 here rather than emitting a
/// terminal-specific 409, so PR #2280 sub-fix 1 can compose on top without
/// shape conflicts (its proven-finalized check promotes this same state to
/// 200 already_finalized when the dispatch carries a recognized completion
/// proof). Without sub-1 merged, the response must remain the canonical
/// generic 409.
#[tokio::test]
async fn stale_dispatch_mismatch_terminal_dispatch_id_falls_through_to_generic_conflict() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_unlinked_dispatched_review_decision(&db, "card-stale-term", "dispatch-term", "completed");
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-stale-term".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: Some("dispatch-term".to_string()),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "terminal dispatch_id must fall through to canonical 409: {}",
        body.0
    );
    let body_str = body.0.to_string();
    assert!(
        body_str.contains("no pending review-decision dispatch"),
        "expected canonical 'no pending' 409 (sub-1 compose-safe), got: {body_str}"
    );
    assert!(
        !body_str.contains("dispatch_status"),
        "terminal fall-through must NOT leak dispatch_status (sub-1 keeps body generic): {body_str}"
    );
}

/// Negative (Codex round 1): caller supplies an OLDER live same-card
/// dispatch_id while a NEWER live review-decision dispatch exists for the
/// same card. Honoring the older id would consume the wrong dispatch.
/// Must return 409 with a clear "superseded" message.
///
/// Note: production schema enforces a partial unique index
/// (`idx_single_active_review_decision`, src/db/schema.rs:478) preventing
/// two concurrently-live review-decision rows per card — this should be
/// unreachable under normal operation. The runtime check + this test exist
/// as defense-in-depth against schema drift / race windows / migration
/// gaps. We temporarily drop the unique index for this test so the seed
/// can construct the adversarial state.
#[tokio::test]
async fn stale_dispatch_mismatch_superseded_live_dispatch_id_returns_conflict() {
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        // Drop the protective unique index so we can stage the adversarial
        // two-live-rows state that the runtime check defends against.
        conn.execute_batch("DROP INDEX IF EXISTS idx_single_active_review_decision;")
            .unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        // Card with NO latest_dispatch_id link → canonical pending misses.
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at) \
             VALUES ('card-superseded', 'Superseded Card', 'review', 'agent-1', NULL, 'suggestion_pending', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // Older live dispatch (caller will submit this id).
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-old', 'card-superseded', 'agent-1', 'review-decision', 'dispatched', '[Review Decision R1]', datetime('now', '-1 hour'), datetime('now', '-1 hour'))",
            [],
        )
        .unwrap();
        // Newer live dispatch — the actual current originating dispatch.
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-new', 'card-superseded', 'agent-1', 'review-decision', 'dispatched', '[Review Decision R2]', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-superseded".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: Some("dispatch-old".to_string()),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "older live same-card dispatch_id must be rejected as superseded: {}",
        body.0
    );
    let body_str = body.0.to_string();
    assert!(
        body_str.contains("superseded"),
        "expected 'superseded' message, got: {body_str}"
    );

    // Critically: neither dispatch must have been mutated.
    let conn = db.lock().unwrap();
    let (old_status, new_status): (String, String) = conn
        .query_row(
            "SELECT (SELECT status FROM task_dispatches WHERE id = 'dispatch-old'), \
                    (SELECT status FROM task_dispatches WHERE id = 'dispatch-new')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(old_status, "dispatched", "stale older dispatch must remain");
    assert_eq!(
        new_status, "dispatched",
        "newer dispatch must NOT be consumed by the stale id"
    );
}

/// Codex round-2 [medium]: equal-timestamp ties must also fail closed.
/// When two live same-card review-decision rows share `created_at`, neither
/// id is uniquely "latest" — both should be rejected as superseded.
#[tokio::test]
async fn stale_dispatch_mismatch_equal_timestamp_tie_rejected() {
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute_batch("DROP INDEX IF EXISTS idx_single_active_review_decision;")
            .unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at) \
             VALUES ('card-tie', 'Tie Card', 'review', 'agent-1', NULL, 'suggestion_pending', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // Two live dispatches with identical created_at — neither is
        // strictly latest, so honoring either would be a 50/50 gamble.
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-tie-a', 'card-tie', 'agent-1', 'review-decision', 'dispatched', '[Review Decision A]', '2026-01-01 00:00:00', '2026-01-01 00:00:00'), \
                    ('dispatch-tie-b', 'card-tie', 'agent-1', 'review-decision', 'dispatched', '[Review Decision B]', '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
            [],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-tie".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: Some("dispatch-tie-a".to_string()),
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "equal-timestamp tie must fail closed (no unique latest): {}",
        body.0
    );
    let body_str = body.0.to_string();
    assert!(
        body_str.contains("superseded"),
        "expected 'superseded' message for tie, got: {body_str}"
    );

    // Neither dispatch may have been consumed.
    let conn = db.lock().unwrap();
    let (a, b): (String, String) = conn
        .query_row(
            "SELECT (SELECT status FROM task_dispatches WHERE id = 'dispatch-tie-a'), \
                    (SELECT status FROM task_dispatches WHERE id = 'dispatch-tie-b')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(a, "dispatched");
    assert_eq!(b, "dispatched");
}

/// Regression: caller did NOT submit a dispatch_id AND there is no pending
/// review-decision dispatch via the canonical link tables. Must stay 409
/// with the original generic message (the by-id fallback only triggers when
/// a dispatch_id is explicitly provided).
#[tokio::test]
async fn stale_dispatch_mismatch_no_dispatch_id_no_pending_still_returns_conflict() {
    let db = test_db();
    let engine = test_engine(&db);
    // Seed an unlinked-but-still-dispatched row so we can prove the by-id
    // path is gated on `dispatch_id` being present — without dispatch_id we
    // must NOT silently pick it up.
    seed_unlinked_dispatched_review_decision(&db, "card-stale-ndi", "dispatch-ndi", "dispatched");
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-stale-ndi".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: None,
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "without dispatch_id, an unlinked dispatched row must NOT be auto-bound: {}",
        body.0
    );
    let body_str = body.0.to_string();
    assert!(
        body_str.contains("no pending review-decision dispatch"),
        "expected canonical 409 message, got: {body_str}"
    );

    // And the unlinked dispatch row must still be dispatched (not consumed).
    let conn = db.lock().unwrap();
    let rd_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-ndi'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rd_status, "dispatched");
}

/// Regression / happy path: canonical pending lookup hits, and caller does
/// not supply `dispatch_id`. Must remain 200 (no behavior change relative to
/// existing `duplicate_accept_returns_conflict` happy path; this nails it
/// down as part of the sub-fix 4 contract).
#[tokio::test]
async fn stale_dispatch_mismatch_happy_path_pending_dispatch_still_accepts() {
    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_status, created_at, updated_at) \
             VALUES ('card-stale-hp', 'HP', 'review', 'agent-1', 'dispatch-hp', 'suggestion_pending', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-hp', 'card-stale-hp', 'agent-1', 'review-decision', 'pending', '[Review Decision]', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }
    let state = AppState::test_state(db.clone(), engine);

    let (status, body) = submit_review_decision(
        State(state),
        Json(ReviewDecisionBody {
            card_id: "card-stale-hp".to_string(),
            decision: "accept".to_string(),
            comment: None,
            commit_sha: None,
            dispatch_id: None,
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "canonical pending path must still 200 after sub-fix 4: {}",
        body.0
    );
}
