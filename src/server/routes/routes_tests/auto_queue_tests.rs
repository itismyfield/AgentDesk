//! Domain-split routes tests — `auto_queue` group.
//! Extracted verbatim from the original `routes_tests.rs` (test bodies unchanged).
//! Helpers and the `sqlite_params!` macro live in `super::common` and the parent
//! `routes_tests` module respectively (the macro is in lexical scope through
//! the parent `mod.rs`).

#![allow(unused_imports)]

// Reach into the parent `routes` module for the symbols the original
// `use super::*;` brought in, plus the shared test helpers in `common`.
use super::super::*;
use super::common::*;

use axum::body::{Body, HttpBody as _};
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::Row;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::MutexGuard;
use tower::ServiceExt;

#[tokio::test]
async fn resume_requested_pg_creates_single_notify_backed_dispatch() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ($1, $1, '111', '222')",
    )
    .bind("agent-resume")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, $5, NOW(), NOW()
        )",
    )
    .bind("card-resume")
    .bind("Resume Card")
    .bind("requested")
    .bind("medium")
    .bind("agent-resume")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-resume/resume")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dispatch_id = json["action"]["dispatch_id"].as_str().unwrap().to_string();
    assert_eq!(json["action"]["type"], "new_implementation_dispatch");

    let row: (String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT td.dispatch_type, td.status, td.context, kc.latest_dispatch_id
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.id = $1",
    )
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (dispatch_type, dispatch_status, context, latest_dispatch_id) = row;
    assert_eq!(dispatch_type, "implementation");
    assert_eq!(dispatch_status, "pending");
    assert_eq!(latest_dispatch_id.as_deref(), Some(dispatch_id.as_str()));
    let context_json: serde_json::Value =
        serde_json::from_str(context.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(context_json["resume"], true);
    assert_eq!(context_json["resumed_from"], "requested");

    let notify_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
    )
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        notify_count, 1,
        "resume(requested) must create exactly one notify outbox row via canonical core"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
fn auto_queue_schema_migration_drops_legacy_max_concurrent_per_agent_column() {
    let db = test_db();
    let conn = db.separate_conn().unwrap();
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE kanban_cards (id TEXT PRIMARY KEY);
         CREATE TABLE task_dispatches (
            id TEXT PRIMARY KEY,
            kanban_card_id TEXT,
            to_agent_id TEXT,
            dispatch_type TEXT,
            created_at DATETIME
         );",
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TABLE auto_queue_runs (
            id          TEXT PRIMARY KEY,
            repo        TEXT,
            agent_id    TEXT,
            status      TEXT DEFAULT 'active',
            ai_model    TEXT,
            ai_rationale TEXT,
            timeout_minutes INTEGER DEFAULT 120,
            unified_thread INTEGER DEFAULT 0,
            unified_thread_id TEXT,
            unified_thread_channel_id TEXT,
            max_concurrent_threads INTEGER DEFAULT 1,
            max_concurrent_per_agent INTEGER DEFAULT 1,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME
        );
        CREATE TABLE auto_queue_entries (
            id              TEXT PRIMARY KEY,
            run_id          TEXT REFERENCES auto_queue_runs(id),
            kanban_card_id  TEXT REFERENCES kanban_cards(id),
            agent_id        TEXT,
            priority_rank   INTEGER DEFAULT 0,
            reason          TEXT,
            status          TEXT DEFAULT 'pending',
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME
        );",
    )
    .unwrap();

    crate::db::schema::ensure_auto_queue_schema(&conn).unwrap();

    let has_legacy_column: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'max_concurrent_per_agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_max_threads: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'max_concurrent_threads'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_thread_group_count: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'thread_group_count'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_batch_phase: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_entries') WHERE name = 'batch_phase'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert!(!has_legacy_column);
    assert!(has_max_threads);
    assert!(has_thread_group_count);
    assert!(has_batch_phase);
}

#[test]
fn on_tick5min_stalled_timeout_uses_latest_activity_timestamp() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-stalled");

    seed_in_progress_stall_case(
        &db,
        "card-fresh-dispatch",
        "Fresh Dispatch",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-fresh", "-10 minutes")),
    );
    seed_in_progress_stall_case(
        &db,
        "card-reentered",
        "Re-entered",
        "agent-stalled",
        "-3 hours",
        "-10 minutes",
        Some(("dispatch-old", "-3 hours")),
    );
    seed_in_progress_stall_case(
        &db,
        "card-truly-stalled",
        "Truly Stalled",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-stale", "-3 hours")),
    );

    let _ = engine.try_fire_hook_by_name("OnTick5min", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let rows: std::collections::HashMap<String, (String, Option<String>)> = conn
        .prepare("SELECT id, status, blocked_reason FROM kanban_cards ORDER BY id ASC")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();

    assert_eq!(
        rows.get("card-fresh-dispatch").map(|row| row.0.as_str()),
        Some("in_progress"),
        "fresh dispatch must reset the stalled timer"
    );
    assert_eq!(
        rows.get("card-reentered").map(|row| row.0.as_str()),
        Some("in_progress"),
        "in_progress re-entry must reset the stalled timer even if latest dispatch is older"
    );
    assert_eq!(
        rows.get("card-truly-stalled").map(|row| row.0.as_str()),
        Some("in_progress"),
        "manual-intervention escalation keeps the card in_progress while attaching blocked_reason"
    );
    assert!(
        rows.get("card-truly-stalled")
            .and_then(|row| row.1.as_deref())
            .map(|reason| reason.contains("Stalled: no activity"))
            .unwrap_or(false),
        "truly stale card must carry the stalled blocked_reason"
    );
}

#[test]
fn on_tick1min_orphan_review_treats_e2e_dispatch_as_active() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-e2e");

    seed_review_e2e_case(
        &db,
        "card-e2e-review",
        "E2E Review",
        "agent-e2e",
        "-10 minutes",
        "dispatch-e2e",
        "dispatched",
        "-10 minutes",
    );

    let _ = engine.try_fire_hook_by_name("OnTick1min", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let (status, blocked_reason): (String, Option<String>) = conn
        .query_row(
            "SELECT status, blocked_reason FROM kanban_cards WHERE id = 'card-e2e-review'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-e2e'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        status, "review",
        "active e2e-test dispatch must keep the card out of orphan review recovery"
    );
    assert!(
        blocked_reason.is_none(),
        "protected review card must not gain an orphan-review blocked_reason"
    );
    assert_eq!(
        dispatch_status, "dispatched",
        "e2e-test dispatch should stay active after onTick1min orphan review sweep"
    );
}

#[test]
fn on_tick1min_orphan_review_skips_recently_completed_review_gap() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-review-gap");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                review_entered_at, created_at, updated_at
            ) VALUES (
                'card-review-gap', 'Review Gap', 'review', 'medium', 'agent-review-gap', 'test-repo',
                datetime('now', '-10 minutes'), datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at, completed_at
            ) VALUES (
                'dispatch-review-gap', 'card-review-gap', 'agent-review-gap', 'review', 'completed', 'Review Gap R1',
                datetime('now', '-10 minutes'), datetime('now', '-30 seconds'), datetime('now', '-30 seconds')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = 'dispatch-review-gap' WHERE id = 'card-review-gap'",
            [],
        )
        .unwrap();
    }

    let _ = engine.try_fire_hook_by_name("OnTick1min", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let (status, blocked_reason): (String, Option<String>) = conn
        .query_row(
            "SELECT status, blocked_reason FROM kanban_cards WHERE id = 'card-review-gap'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        status, "review",
        "recently completed review dispatch must protect the review-decision creation gap"
    );
    assert!(
        blocked_reason.is_none(),
        "recent review completion gap must not leave an orphan-review blocked_reason"
    );
}

#[test]
fn on_tick30s_orphan_dispatch_recovers_true_orphan_without_regression() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-orphan-330");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, started_at, created_at, updated_at
            ) VALUES (
                'card-orphan-330', 'True Orphan #330', 'in_progress', 'medium', 'agent-orphan-330', 'test-repo',
                330, 'dispatch-orphan-330', datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-orphan-330', 'card-orphan-330', 'agent-orphan-330', 'implementation', 'pending',
                'orphan impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let first_card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-orphan-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-orphan-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_confirm_marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = ?1",
            [format!(
                "runtime_supervisor:orphan_confirm:{}",
                "dispatch-orphan-330"
            )],
            |row| row.get(0),
        )
        .unwrap();
    let (first_action, first_note): (String, Option<String>) = conn
        .query_row(
            "SELECT chosen_action, json_extract(evidence_json, '$.supervisor_note')
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-orphan-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(conn);

    assert_eq!(
        first_card_status, "in_progress",
        "first orphan tick must wait for confirm instead of rolling the card back immediately"
    );
    assert_eq!(
        first_dispatch_status, "pending",
        "first orphan tick must keep the dispatch pending until confirm succeeds"
    );
    assert!(
        first_confirm_marker_count > 0,
        "first orphan tick must persist a confirm marker"
    );
    assert_eq!(first_action, "Probe");
    assert!(
        first_note
            .as_deref()
            .unwrap_or("")
            .contains("awaiting confirm"),
        "first orphan tick must record that confirm is still pending"
    );

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-orphan-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (dispatch_status, dispatch_result): (String, Option<String>) = conn
        .query_row(
            "SELECT status, result FROM task_dispatches WHERE id = 'dispatch-orphan-330'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let (decision_signal, chosen_action, audit_dispatch_id): (String, String, Option<String>) =
        conn.query_row(
            "SELECT signal, chosen_action, dispatch_id
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-orphan-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let review_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches
             WHERE kanban_card_id = 'card-orphan-330' AND dispatch_type = 'review'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_confirm_marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = ?1",
            [format!(
                "runtime_supervisor:orphan_confirm:{}",
                "dispatch-orphan-330"
            )],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        card_status, "requested",
        "true orphan implementation dispatch must roll the card back to the dispatchable preflight state"
    );
    assert_eq!(
        dispatch_status, "failed",
        "true orphan implementation dispatch must be failed when no agent work was observed"
    );
    assert!(
        dispatch_result
            .as_deref()
            .unwrap_or("")
            .contains("orphan_recovery_rollback"),
        "true orphan recovery must keep the orphan_recovery rollback marker"
    );
    assert_ne!(
        card_status, "review",
        "true orphan implementation dispatch must not auto-promote the card into review"
    );
    assert_eq!(
        review_dispatch_count, 0,
        "true orphan recovery must not create a follow-up review dispatch"
    );
    assert_eq!(decision_signal, "OrphanCandidate");
    assert_eq!(chosen_action, "Resume");
    assert_eq!(audit_dispatch_id.as_deref(), Some("dispatch-orphan-330"));
    assert!(
        remaining_confirm_marker_count == 0,
        "confirmed orphan recovery must clear the confirm marker"
    );
}

#[test]
fn on_tick30s_orphan_dispatch_skips_card_that_moved_to_backlog_mid_recovery() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-orphan-race");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, started_at, created_at, updated_at
            ) VALUES (
                'card-race-330', 'Orphan Race #330', 'in_progress', 'medium', 'agent-orphan-race', 'test-repo',
                330, 'dispatch-race-330', datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-race-330', 'card-race-330', 'agent-orphan-race', 'implementation', 'pending',
                'race impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'backlog')",
            [format!(
                "test:runtime_supervisor:orphan_post_complete_override:{}",
                "dispatch-race-330"
            )],
        )
        .unwrap();
    }

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let first_card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_confirm_marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = ?1",
            [format!(
                "runtime_supervisor:orphan_confirm:{}",
                "dispatch-race-330"
            )],
            |row| row.get(0),
        )
        .unwrap();
    let (first_action, first_note): (String, Option<String>) = conn
        .query_row(
            "SELECT chosen_action, json_extract(evidence_json, '$.supervisor_note')
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-race-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(conn);

    assert_eq!(first_card_status, "in_progress");
    assert_eq!(first_dispatch_status, "pending");
    assert!(first_confirm_marker_count > 0);
    assert_eq!(first_action, "Probe");
    assert!(
        first_note
            .as_deref()
            .unwrap_or("")
            .contains("awaiting confirm"),
        "race path must also wait for confirm on the first orphan tick"
    );

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let review_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches
             WHERE kanban_card_id = 'card-race-330' AND dispatch_type = 'review'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (chosen_action, decision_note): (String, Option<String>) = conn
        .query_row(
            "SELECT chosen_action, json_extract(evidence_json, '$.supervisor_note')
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-race-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let remaining_confirm_marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = ?1",
            [format!(
                "runtime_supervisor:orphan_confirm:{}",
                "dispatch-race-330"
            )],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        card_status, "backlog",
        "post-complete race guard must keep a backlogged card from reviving into review"
    );
    assert_eq!(
        dispatch_status, "failed",
        "the orphan implementation dispatch must fail instead of completing without work evidence"
    );
    assert_eq!(
        review_dispatch_count, 0,
        "skipped orphan recovery must not create a follow-up review dispatch"
    );
    assert_eq!(
        chosen_action, "Resume",
        "supervisor should still choose resume before the post-complete race guard trips"
    );
    assert!(
        decision_note
            .as_deref()
            .unwrap_or("")
            .contains("card moved to status=backlog"),
        "runtime_decisions audit must explain why the resume transition was skipped"
    );
    assert!(
        remaining_confirm_marker_count == 0,
        "race-guarded orphan recovery must clear the confirm marker after confirm completes"
    );
}

#[tokio::test]
async fn stalled_cards_and_stats_pg_use_latest_activity_timestamp() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_agent_pg(&pool, "agent-stalled").await;
    seed_repo_pg(&pool, "test-repo").await;

    async fn seed_stall_case_pg(
        pool: &sqlx::PgPool,
        card_id: &str,
        title: &str,
        agent_id: &str,
        started_offset: &str,
        updated_offset: &str,
        latest_dispatch: Option<(&str, &str)>,
    ) {
        let started = format!("NOW() + INTERVAL '{started_offset}'");
        let updated = format!("NOW() + INTERVAL '{updated_offset}'");
        let sql = format!(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                started_at, created_at, updated_at
            ) VALUES (
                $1, $2, 'in_progress', 'medium', $3, 'test-repo',
                {started}, {started}, {updated}
            )"
        );
        sqlx::query(&sql)
            .bind(card_id)
            .bind(title)
            .bind(agent_id)
            .execute(pool)
            .await
            .unwrap();

        if let Some((dispatch_id, dispatch_offset)) = latest_dispatch {
            let dispatch_at = format!("NOW() + INTERVAL '{dispatch_offset}'");
            let sql = format!(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                ) VALUES (
                    $1, $2, $3, 'implementation', 'dispatched', $4, {dispatch_at}, {dispatch_at}
                )"
            );
            sqlx::query(&sql)
                .bind(dispatch_id)
                .bind(card_id)
                .bind(agent_id)
                .bind(format!("{title} Dispatch"))
                .execute(pool)
                .await
                .unwrap();
            sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
                .bind(dispatch_id)
                .bind(card_id)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    seed_stall_case_pg(
        &pool,
        "card-fresh-dispatch",
        "Fresh Dispatch",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-fresh", "-10 minutes")),
    )
    .await;
    seed_stall_case_pg(
        &pool,
        "card-reentered",
        "Re-entered",
        "agent-stalled",
        "-3 hours",
        "-10 minutes",
        Some(("dispatch-old", "-3 hours")),
    )
    .await;
    seed_stall_case_pg(
        &pool,
        "card-truly-stalled",
        "Truly Stalled",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-stale", "-3 hours")),
    )
    .await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let stalled_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/stalled")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stalled_resp.status(), StatusCode::OK);
    let stalled_body = axum::body::to_bytes(stalled_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stalled_json: serde_json::Value = serde_json::from_slice(&stalled_body).unwrap();
    let stalled_ids: Vec<String> = stalled_json
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|card| card["id"].as_str().map(ToString::to_string))
        .collect();
    assert_eq!(
        stalled_ids,
        vec!["card-truly-stalled".to_string()],
        "stalled endpoint must ignore fresh-dispatch and re-entered cards"
    );

    let stats_resp = app
        .oneshot(
            Request::builder()
                .uri("/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_resp.status(), StatusCode::OK);
    let stats_body = axum::body::to_bytes(stats_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stats_json: serde_json::Value = serde_json::from_slice(&stats_body).unwrap();
    assert_eq!(
        stats_json["kanban"]["stale_in_progress"],
        serde_json::json!(1),
        "stats stale_in_progress count must match latest-activity stalled detection"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #1442 — `/api/queue/generate` response must include the structured
/// `skipped_due_to_active_dispatch` array when a requested issue already has
/// an active dispatch (the silent-skip case that drove the original bug).
#[tokio::test]
async fn auto_queue_generate_response_breaks_down_skipped_active_dispatch_pg_1442() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-aq-1442").await;

    // Card #1 — eligible (status ready).
    seed_auto_queue_card_pg(
        &pool,
        "card-aq-1442-ready",
        1442001,
        "ready",
        "agent-aq-1442",
    )
    .await;
    // Card #2 — ineligible because it has an active dispatch (status
    // in_progress + latest_dispatch_id pointing at a dispatched row).
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-aq-1442-active")
    .bind("Active dispatch 1442")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-aq-1442")
    .bind("test-repo")
    .bind(1442002_i64)
    .bind("dispatch-aq-1442-active")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW(), NOW()
         )",
    )
    .bind("dispatch-aq-1442-active")
    .bind("card-aq-1442-active")
    .bind("agent-aq-1442")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] active 1442")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"repo":"test-repo","issue_numbers":[1442001,1442002]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "auto-queue generate must succeed; got {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let skipped_active = json["skipped_due_to_active_dispatch"]
        .as_array()
        .expect("skipped_due_to_active_dispatch must be an array");
    assert!(
        skipped_active
            .iter()
            .any(|entry| entry["issue_number"] == 1442002
                && entry["existing_dispatch_id"] == "dispatch-aq-1442-active"),
        "issue 1442002 must be reported under skipped_due_to_active_dispatch with its existing dispatch id: {body_text}"
    );
    assert!(
        json["skipped_due_to_dependency"].is_array(),
        "skipped_due_to_dependency key must be present as an array (even if empty): {body_text}"
    );
    assert!(
        json["skipped_due_to_filter"].is_array(),
        "skipped_due_to_filter key must be present as an array (even if empty): {body_text}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_generate_skips_card_with_active_dispatch_pg_1444() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-aq-skip-1444").await;

    // Card #1: in `ready` (an enqueueable state) BUT carries an active
    // dispatch — exactly the #1444 race where redispatch already created a
    // dispatch and a follow-up generate would otherwise queue a duplicate.
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW()
         )",
    )
    .bind("card-aq-skip-1444-ready-but-active")
    .bind("Ready+active 1444")
    .bind("ready")
    .bind("medium")
    .bind("agent-aq-skip-1444")
    .bind("test-repo")
    .bind(1444011_i64)
    .bind("dispatch-aq-skip-1444-active")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW(), NOW()
         )",
    )
    .bind("dispatch-aq-skip-1444-active")
    .bind("card-aq-skip-1444-ready-but-active")
    .bind("agent-aq-skip-1444")
    .bind("implementation")
    .bind("dispatched")
    .bind("[Impl] live 1444")
    .execute(&pool)
    .await
    .unwrap();

    // Card #2: clean ready card — should make it into the run.
    seed_auto_queue_card_pg(
        &pool,
        "card-aq-skip-1444-clean",
        1444012,
        "ready",
        "agent-aq-skip-1444",
    )
    .await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"repo":"test-repo","issue_numbers":[1444011,1444012]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "generate must respond 200: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let skipped_active = json["skipped_due_to_active_dispatch"]
        .as_array()
        .expect("skipped_due_to_active_dispatch must be an array");
    assert!(
        skipped_active
            .iter()
            .any(|entry| entry["issue_number"] == 1444011
                && entry["existing_dispatch_id"] == "dispatch-aq-skip-1444-active"),
        "issue 1444011 must be reported in skipped_due_to_active_dispatch: {body_text}"
    );

    // The clean card should still make it into the run.
    let entries = json["entries"]
        .as_array()
        .expect("entries must be an array");
    assert!(
        entries
            .iter()
            .any(|entry| { entry["card_id"].as_str() == Some("card-aq-skip-1444-clean") }),
        "clean card must still appear in entries: {body_text}"
    );
    // The skipped card must NOT be in entries.
    assert!(
        entries.iter().all(|entry| {
            entry["card_id"].as_str() != Some("card-aq-skip-1444-ready-but-active")
        }),
        "skipped card must not appear in entries: {body_text}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_dispatch_prepares_backlog_cards_and_auto_assigns_agent() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "project-agentdesk");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-423', 'Issue #423', 'backlog', 'high', NULL, 'test-repo', 423,
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-405', 'Issue #405', 'ready', 'medium', NULL, 'test-repo', 405,
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-407', 'Issue #407', 'requested', 'medium', NULL, 'test-repo', 407,
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "groups": [
                            {"issues": [423, 405], "sequential": true},
                            {"issues": [407]}
                        ],
                        "activate": false
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["activated"], false);
    assert_eq!(json["requested"]["auto_assign_agent"], true);
    assert_eq!(json["run"]["status"], "generated");

    let run_id = json["run"]["id"]
        .as_str()
        .expect("dispatch run id must be present");
    let entries = json["entries"]
        .as_array()
        .expect("dispatch snapshot must include entries");
    assert_eq!(entries.len(), 3);

    let conn = db.lock().unwrap();
    let assigned_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards
             WHERE id IN ('card-dq-423', 'card-dq-405', 'card-dq-407')
               AND assigned_agent_id = 'project-agentdesk'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(assigned_count, 3);

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-dq-423'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backlog_status, "ready");

    let entry_layout: Vec<(i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.thread_group, e.priority_rank
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = ?1
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    assert_eq!(entry_layout, vec![(405, 0, 1), (407, 1, 0), (423, 0, 0)]);
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_dispatch_persists_review_mode_in_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-review-mode");
    seed_repo(&db, "test-repo");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-review-mode", 4966, "ready", "agent-review-mode");

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-review-mode",
                        "groups": [
                            {"issues": [4966]}
                        ],
                        "review_mode": "disabled",
                        "activate": false
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["run"]["review_mode"], "disabled");

    let run_id = json["run"]["id"]
        .as_str()
        .expect("dispatch run id must be present");
    let conn = db.lock().unwrap();
    let stored_review_mode: String = conn
        .query_row(
            "SELECT review_mode FROM auto_queue_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_review_mode, "disabled");
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_dispatch_rejects_when_live_run_exists_without_force() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-dispatch-existing",
        4901,
        "ready",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-dispatch-backlog",
        4902,
        "backlog",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-dispatch-ready",
        4903,
        "ready",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count, created_at
            ) VALUES (
                'run-dispatch-active', 'test-repo', 'project-agentdesk', 'active', 1, 1, datetime('now', '-1 minute')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-dispatch-existing', 'run-dispatch-active', 'card-dispatch-existing', 'project-agentdesk', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "groups": [
                            {"issues": [4903], "thread_group": 0, "batch_phase": 1},
                            {"issues": [4902], "thread_group": 7, "batch_phase": 3}
                        ],
                        "activate": false
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["existing_run_id"], "run-dispatch-active");
    assert_eq!(json["existing_run_status"], "active");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("run_id=run-dispatch-active"),
        "conflict response must include the existing run id: {json}"
    );

    let conn = db.lock().unwrap();
    let total_runs: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_runs", [], |row| row.get(0))
        .unwrap();
    let active_runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total_runs, 1, "dispatch conflict must not create a new run");
    assert_eq!(
        active_runs, 1,
        "dispatch conflict must leave the original live run untouched"
    );

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-dispatch-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        backlog_status, "backlog",
        "409 conflict must happen before backlog auto-promotion"
    );

    let entry_layout: Vec<(i64, i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.thread_group, e.priority_rank, COALESCE(e.batch_phase, 0)
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = 'run-dispatch-active'
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    };
    assert_eq!(
        entry_layout,
        vec![(4901, 0, 0, 0)],
        "dispatch conflict must not enqueue new entries into the existing run"
    );

    let run_meta: (i64, i64) = conn
        .query_row(
            "SELECT max_concurrent_threads, thread_group_count
             FROM auto_queue_runs
             WHERE id = 'run-dispatch-active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_meta, (1, 1));
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_dispatch_force_cancels_live_run_and_creates_new_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-dispatch-force-existing",
        4911,
        "ready",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-dispatch-force-backlog",
        4912,
        "backlog",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count, created_at
            ) VALUES (
                'run-dispatch-force-old', 'test-repo', 'project-agentdesk', 'active', 1, 1, datetime('now', '-1 minute')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-dispatch-force-existing', 'run-dispatch-force-old', 'card-dispatch-force-existing', 'project-agentdesk', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "groups": [
                            {"issues": [4912], "thread_group": 2, "batch_phase": 3}
                        ],
                        "activate": false,
                        "force": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let new_run_id = json["run"]["id"].as_str().unwrap_or("");
    assert!(
        !new_run_id.is_empty() && new_run_id != "run-dispatch-force-old",
        "force dispatch must create a replacement run: {json}"
    );
    assert_eq!(json["run"]["status"], "generated");

    let conn = db.lock().unwrap();
    let old_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-dispatch-force-old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_run_status, "cancelled");

    let old_entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-dispatch-force-existing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_entry_status, "skipped");

    let new_entry_layout: Vec<(i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.thread_group, COALESCE(e.batch_phase, 0)
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = ?1
                 ORDER BY e.priority_rank ASC, kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([new_run_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    };
    assert_eq!(new_entry_layout, vec![(4912, 2, 3)]);

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-dispatch-force-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backlog_status, "ready");
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_add_run_entry_creates_pending_entry_for_active_run() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-run-entry-existing",
        4921,
        "ready",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-run-entry-new",
        4922,
        "ready",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count, created_at
            ) VALUES (
                'run-add-entry-active', 'test-repo', 'project-agentdesk', 'active', 1, 1, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
            ) VALUES (
                'entry-run-entry-existing', 'run-add-entry-active', 'card-run-entry-existing',
                'project-agentdesk', 'pending', 0, 0, 1
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-add-entry-active/entries")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&json!({
                        "issue_number": 4922,
                        "batch_phase": 4,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["run_id"], "run-add-entry-active");
    assert_eq!(json["thread_group"], 1);
    assert_eq!(json["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let inserted: (i64, i64, i64, String) = conn
        .query_row(
            "SELECT priority_rank, thread_group, batch_phase, status
             FROM auto_queue_entries
             WHERE run_id = 'run-add-entry-active'
               AND kanban_card_id = 'card-run-entry-new'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(inserted, (0, 1, 4, "pending".to_string()));

    let run_meta: (i64, i64) = conn
        .query_row(
            "SELECT thread_group_count, max_concurrent_threads
             FROM auto_queue_runs
             WHERE id = 'run-add-entry-active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_meta, (2, 2));
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_add_run_entry_rejects_non_active_runs() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-run-entry-cancelled",
        4923,
        "ready",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-add-entry-cancelled', 'test-repo', 'project-agentdesk', 'cancelled', datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-add-entry-cancelled/entries")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&json!({
                        "issue_number": 4923,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("status=cancelled"),
        "inactive runs must be rejected with status details: {json}"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_add_run_entry_rejects_non_ready_cards() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-run-entry-backlog",
        4924,
        "backlog",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-add-entry-ready-only', 'test-repo', 'project-agentdesk', 'active', datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-add-entry-ready-only/entries")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&json!({
                        "issue_number": 4924,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("must be in ready status"),
        "run-entry add must reject non-ready cards: {json}"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_update_entry_moves_pending_entry_and_syncs_run_groups() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-update");
    seed_auto_queue_card(&db, "card-update-1", 1801, "ready", "agent-update");
    seed_auto_queue_card(&db, "card-update-2", 1802, "ready", "agent-update");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-entry', 'test-repo', 'agent-update', 'generated', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-update-1', 'run-update-entry', 'card-update-1', 'agent-update', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-update-2', 'run-update-entry', 'card-update-2', 'agent-update', 'pending', 1, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/entries/entry-update-2")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "thread_group": 3,
                        "priority_rank": 0
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["thread_group"], 3);
    assert_eq!(json["entry"]["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let run_meta: (i64, i64) = conn
        .query_row(
            "SELECT max_concurrent_threads, thread_group_count
             FROM auto_queue_runs
             WHERE id = 'run-update-entry'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_meta, (2, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_update_entry_restores_skipped_entry_to_pending() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-update");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-update-restore", 1699, "ready", "agent-update");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-entry-restore', 'test-repo', 'agent-update', 'cancelled', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank,
                thread_group, dispatch_id, slot_index, completed_at
            ) VALUES (
                'entry-update-restore', 'run-update-entry-restore', 'card-update-restore',
                'agent-update', 'skipped', 5, 0, 'dispatch-old', 0, datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/entries/entry-update-restore")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "status": "pending",
                        "thread_group": 2,
                        "priority_rank": 0
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["status"], "pending");
    assert_eq!(json["entry"]["thread_group"], 2);
    assert_eq!(json["entry"]["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let (status, dispatch_id, slot_index, completed_at, thread_group, priority_rank): (
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index, completed_at, thread_group, priority_rank
             FROM auto_queue_entries
             WHERE id = 'entry-update-restore'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(status, "pending");
    assert!(dispatch_id.is_none());
    assert!(slot_index.is_none());
    assert!(completed_at.is_none());
    assert_eq!(thread_group, 2);
    assert_eq!(priority_rank, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_update_entry_updates_batch_phase_only_and_with_priority_rank() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-update-phase");
    seed_auto_queue_card(
        &db,
        "card-update-phase",
        1810,
        "ready",
        "agent-update-phase",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-phase', 'test-repo', 'agent-update-phase', 'generated', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
            ) VALUES (
                'entry-update-phase', 'run-update-phase', 'card-update-phase',
                'agent-update-phase', 'pending', 3, 0, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/entries/entry-update-phase")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "batch_phase": 2
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["batch_phase"], 2);

    {
        let conn = db.lock().unwrap();
        let batch_phase: i64 = conn
            .query_row(
                "SELECT batch_phase FROM auto_queue_entries WHERE id = 'entry-update-phase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(batch_phase, 2);
    }

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/entries/entry-update-phase")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "batch_phase": 1,
                        "priority_rank": 0
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["batch_phase"], 1);
    assert_eq!(json["entry"]["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let entry_meta: (i64, i64) = conn
        .query_row(
            "SELECT batch_phase, priority_rank
             FROM auto_queue_entries
             WHERE id = 'entry-update-phase'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_meta, (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_update_run_updates_max_concurrent_threads_only_and_with_status() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-max', 'test-repo', 'generated', 1, 4
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/runs/run-update-max")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "max_concurrent_threads": 4
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    {
        let conn = db.lock().unwrap();
        let max_concurrent_threads: i64 = conn
            .query_row(
                "SELECT max_concurrent_threads
                 FROM auto_queue_runs
                 WHERE id = 'run-update-max'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(max_concurrent_threads, 4);
    }

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/runs/run-update-max")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "status": "completed",
                        "max_concurrent_threads": 2
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let run_meta: (String, i64, Option<String>) = conn
        .query_row(
            "SELECT status, max_concurrent_threads, completed_at
             FROM auto_queue_runs
             WHERE id = 'run-update-max'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(run_meta.0, "completed");
    assert_eq!(run_meta.1, 2);
    assert!(run_meta.2.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_update_run_pg_updates_max_concurrent_threads_only_and_with_status() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5
         )",
    )
    .bind("run-update-max-pg")
    .bind("test-repo")
    .bind("generated")
    .bind(1_i64)
    .bind(4_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/runs/run-update-max-pg")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "max_concurrent_threads": 4
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let max_concurrent_threads = sqlx::query_scalar::<_, i64>(
        "SELECT max_concurrent_threads::BIGINT
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind("run-update-max-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(max_concurrent_threads, 4);

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/runs/run-update-max-pg")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "status": "completed",
                        "max_concurrent_threads": 2
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let run_meta: (String, i64, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT status,
                max_concurrent_threads::BIGINT,
                completed_at
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind("run-update-max-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(run_meta.0, "completed");
    assert_eq!(run_meta.1, 2);
    assert!(run_meta.2.is_some());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_rebind_slot_assigns_run_and_updates_dispatched_entry_slot() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-rebind");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-rebind", 1700, "in_progress", "agent-rebind");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-rebind', 'test-repo', 'agent-rebind', 'active', 2, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, thread_group
            ) VALUES (
                'entry-rebind', 'run-rebind', 'card-rebind', 'agent-rebind',
                'dispatched', 'dispatch-rebind', 3
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/slots/agent-rebind/1/rebind")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-rebind",
                        "thread_group": 3
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["updated_entries"], 1);

    let conn = db.lock().unwrap();
    let slot_binding: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-rebind'
               AND slot_index = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot_binding.0.as_deref(), Some("run-rebind"));
    assert_eq!(slot_binding.1, Some(3));

    let entry_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-rebind'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_slot, Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_restore_run_restores_skipped_entries_by_card_state() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-restore");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-restore-pending", 1801, "ready", "agent-restore");
    seed_auto_queue_card(&db, "card-restore-done", 1802, "done", "agent-restore");
    seed_auto_queue_card(&db, "card-restore-live", 1803, "requested", "agent-restore");
    seed_auto_queue_card(
        &db,
        "card-restore-new",
        1804,
        "in_progress",
        "agent-restore",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-restore', 'test-repo', 'agent-restore', 'cancelled', 4, 4
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-pending', 'run-restore', 'card-restore-pending', 'agent-restore', 'skipped', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-done', 'run-restore', 'card-restore-done', 'agent-restore', 'skipped', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-live', 'run-restore', 'card-restore-live', 'agent-restore', 'skipped', 2, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-new', 'run-restore', 'card-restore-new', 'agent-restore', 'skipped', 3, 3
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-restore-old-done', 'card-restore-done', 'agent-restore',
                'cancelled', 'Old Done Dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-restore-live', 'card-restore-live', 'agent-restore',
                'dispatched', 'Live Dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-restore-old-new', 'card-restore-new', 'agent-restore',
                'cancelled', 'Old New Dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET latest_dispatch_id = 'dispatch-restore-live'
             WHERE id = 'card-restore-live'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET latest_dispatch_id = 'dispatch-restore-old-new'
             WHERE id = 'card-restore-new'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES ('entry-restore-done', 'dispatch-restore-old-done', 'seed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES ('entry-restore-live', 'dispatch-restore-live', 'seed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES ('entry-restore-new', 'dispatch-restore-old-new', 'seed')",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-restore/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["run_status"], "active");
    assert_eq!(json["restored_pending"], 1);
    assert_eq!(json["restored_done"], 1);
    assert_eq!(json["restored_dispatched"], 2);
    assert_eq!(json["created_dispatches"], 1);
    assert_eq!(json["rebound_slots"], 2);
    assert_eq!(json["unbound_dispatches"], 0);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-restore'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "active");

    let pending_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_status, "pending");

    let done_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-done'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(done_status, "done");

    let live_entry: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-restore-live'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(live_entry.0, "dispatched");
    assert_eq!(live_entry.1.as_deref(), Some("dispatch-restore-live"));
    assert!(live_entry.2.is_some());

    let new_entry: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-restore-new'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(new_entry.0, "dispatched");
    assert!(new_entry.1.is_some());
    assert_ne!(new_entry.1.as_deref(), Some("dispatch-restore-old-new"));
    assert!(new_entry.2.is_some());

    let rebound_slots: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM auto_queue_slots
             WHERE assigned_run_id = 'run-restore'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rebound_slots, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_restore_reloads_user_cancelled_entries() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-restore-user-cancelled");
    seed_auto_queue_card(
        &db,
        "card-restore-user-cancelled",
        1810,
        "in_progress",
        "agent-restore-user-cancelled",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-restore-user-cancelled', 'test-repo', 'agent-restore-user-cancelled', 'paused', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
            ) VALUES (
                'entry-restore-user-cancelled', 'run-restore-user-cancelled',
                'card-restore-user-cancelled', 'agent-restore-user-cancelled',
                'user_cancelled', 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let cancel_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel?run_id=run-restore-user-cancelled")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(cancel_response.status(), StatusCode::OK);
    let cancel_body = axum::body::to_bytes(cancel_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let cancel_json: serde_json::Value = serde_json::from_slice(&cancel_body).unwrap();
    assert_eq!(cancel_json["cancelled_runs"], 1);
    assert_eq!(cancel_json["cancelled_entries"], 1);

    {
        let conn = db.lock().unwrap();
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-user-cancelled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            entry_status, "skipped",
            "run cancel must sweep user_cancelled entries into the restorable skipped bucket"
        );
    }

    let restore_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-restore-user-cancelled/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(restore_response.status(), StatusCode::OK);
    let restore_body = axum::body::to_bytes(restore_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let restore_json: serde_json::Value = serde_json::from_slice(&restore_body).unwrap();
    assert_eq!(restore_json["ok"], true);
    assert_eq!(restore_json["run_status"], "active");
    assert_eq!(restore_json["restored_pending"], 1);
    assert_eq!(restore_json["restored_done"], 0);
    assert_eq!(restore_json["restored_dispatched"], 0);

    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-user-cancelled'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        entry_status, "pending",
        "restore must reload swept user_cancelled entries instead of leaving them stranded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_restore_run_rejects_active_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-restore-reject");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-restore-active', 'test-repo', 'agent-restore-reject', 'active')",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-restore-active/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("already active")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_restore_run_retries_from_restoring_after_partial_failure() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-restore-retry");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-restore-retry-ok",
        1811,
        "ready",
        "agent-restore-retry",
    );
    seed_auto_queue_card(
        &db,
        "card-restore-retry-fail",
        1812,
        "ready",
        "agent-restore-retry",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-restore-retry', 'test-repo', 'agent-restore-retry', 'cancelled', 2, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-retry-ok', 'run-restore-retry', 'card-restore-retry-ok',
                'agent-restore-retry', 'skipped', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-retry-fail', 'run-restore-retry', 'card-restore-retry-fail',
                'agent-restore-retry', 'skipped', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_restore_retry_entry
             BEFORE UPDATE OF status ON auto_queue_entries
             WHEN OLD.id = 'entry-restore-retry-fail'
               AND NEW.status != OLD.status
             BEGIN
                 SELECT RAISE(ABORT, 'restore retry blocked');
             END;",
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-restore-retry/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["run_status"], "cancelled");
    assert_eq!(json["restored_pending"], 0);
    assert!(
        json["errors"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|value| value
                .as_str()
                .unwrap_or_default()
                .contains("entry-restore-retry-fail")),
        "restore response must surface the skipped entry that still needs recovery"
    );

    {
        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-restore-retry'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_status, "cancelled");

        let restored_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-retry-ok'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(restored_status, "skipped");

        let missing_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-retry-fail'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missing_status, "skipped");
        conn.execute("DROP TRIGGER fail_restore_retry_entry", [])
            .unwrap();
    }

    let retry_app = test_api_router(db.clone(), test_engine(&db), None);
    let retry_response = retry_app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-restore-retry/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(retry_response.status(), StatusCode::OK);
    let retry_body = axum::body::to_bytes(retry_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let retry_json: serde_json::Value = serde_json::from_slice(&retry_body).unwrap();
    assert_eq!(retry_json["ok"], true);
    assert_eq!(retry_json["run_status"], "active");
    assert_eq!(retry_json["restored_pending"], 2);

    let conn = db.lock().unwrap();
    let final_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-restore-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(final_run_status, "active");

    let entry_states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status
                 FROM auto_queue_entries
                 WHERE run_id = 'run-restore-retry'
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        entry_states,
        vec![
            (
                "entry-restore-retry-fail".to_string(),
                "pending".to_string(),
            ),
            ("entry-restore-retry-ok".to_string(), "pending".to_string()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_run_id_does_not_dispatch_restoring_runs() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-restoring-activate");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-restoring-activate",
        1700,
        "ready",
        "agent-restoring-activate",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-restoring-activate', 'test-repo', 'agent-restoring-activate', 'restoring', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-restoring-activate', 'run-restoring-activate', 'card-restoring-activate', 'agent-restoring-activate', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-restoring-activate",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 0);
    assert_eq!(json["message"], "Run is restoring");

    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-restoring-activate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "pending");

    let dispatch_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM task_dispatches", [], |row| row.get(0))
        .unwrap();
    assert_eq!(dispatch_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_active_only_does_not_promote_generated_runs() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-active-only");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-active-run", 1701, "ready", "agent-active-only");
    seed_auto_queue_card(
        &db,
        "card-generated-run",
        1702,
        "ready",
        "agent-active-only",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-active', 'test-repo', 'agent-active-only', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-generated', 'test-repo', 'agent-active-only', 'generated', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-active', 'run-active', 'card-active-run', 'agent-active-only', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-generated', 'run-generated', 'card-generated-run', 'agent-active-only', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-active-only",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror body={json}"
    );
    assert_eq!(json["dispatched"][0]["card_id"], "card-active-run");

    let conn = db.lock().unwrap();
    let generated_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let generated_entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let active_dispatch_card: String = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(generated_run_status, "generated");
    assert_eq!(generated_entry_status, "pending");
    assert_eq!(active_dispatch_card, "card-active-run");
}

/// #162: A card in 'requested' state, assigned to the same agent, must not
/// be blocked by the busy-agent guard when that card itself is the dispatch target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_requested_card_not_blocked_by_own_status() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-req-self");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-req-self", 1630, "requested", "agent-req-self");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-req-self', 'test-repo', 'agent-req-self', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-req-self', 'run-req-self', 'card-req-self', 'agent-req-self', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-req-self",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "activate must succeed — busy guard must exclude the card being dispatched"
    );

    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-req-self'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
}

/// #162/#500: A card in 'backlog' (non-dispatchable) state must be walked
/// to the dispatchable state via canonical transitions before dispatch creation.
/// The walk must preserve the same requested-state hook side-effects as a
/// manual transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_walks_backlog_card_to_dispatchable_state() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-walk");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-walk-bl", 1631, "backlog", "agent-walk");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET description = ?1
             WHERE id = 'card-walk-bl'",
            ["DoD: keep auto-queue walk hook parity and preserve activation behavior."],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-walk', 'test-repo', 'agent-walk', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-walk', 'run-walk', 'card-walk-bl', 'agent-walk', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-walk",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "activate must succeed for backlog card via silent walk"
    );

    // Verify the card was walked through free transitions and dispatch was created
    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-walk'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");

    // Card should have been dispatched (moved past backlog via silent walk)
    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-walk-bl'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 1,
        "exactly one dispatch must be created for the walked card"
    );

    let metadata: Option<String> = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = 'card-walk-bl'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata.as_deref().expect("walk must persist metadata")).unwrap();
    assert_eq!(
        metadata_json["preflight_status"], "clear",
        "requested-state preflight hook must run during auto-queue walk"
    );
}

/// #500: If the requested-state hook decides the card is already applied,
/// activate() must respect that side-effect instead of creating a new dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_walk_respects_requested_hook_skip() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-walk-skip");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-walk-skip", 1632, "backlog", "agent-walk-skip");
    let _gh = install_mock_gh_issue_view_closed(1632, "itismyfield/AgentDesk");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET github_issue_url = 'https://github.com/itismyfield/AgentDesk/issues/1632'
             WHERE id = 'card-walk-skip'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at, completed_at
            ) VALUES (
                'dispatch-walk-skip', 'card-walk-skip', 'agent-walk-skip', 'implementation', 'completed',
                'Existing implementation', datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-walk-skip', 'test-repo', 'agent-walk-skip', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-walk-skip', 'run-walk-skip', 'card-walk-skip', 'agent-walk-skip', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-walk-skip",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 0,
        "activate must not create a new dispatch when requested-state preflight skips the card"
    );

    let conn = db.lock().unwrap();
    let (card_status, entry_status): (String, String) = conn
        .query_row(
            "SELECT
                (SELECT status FROM kanban_cards WHERE id = 'card-walk-skip'),
                (SELECT status FROM auto_queue_entries WHERE id = 'entry-walk-skip')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(card_status, "done");
    assert_eq!(entry_status, "skipped");

    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-walk-skip'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 1,
        "hook-driven skip must not create an additional dispatch"
    );
}

/// #430: legacy unified_thread runs still dispatch, but via slot pooling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_legacy_unified_thread_run_dispatches_via_slot_pool() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-unified");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-unified-1", 1625, "ready", "agent-unified");
    seed_auto_queue_card(&db, "card-unified-2", 1626, "ready", "agent-unified");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, unified_thread) \
             VALUES ('run-unified', 'test-repo', 'agent-unified', 'active', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-u1', 'run-unified', 'card-unified-1', 'agent-unified', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-u2', 'run-unified', 'card-unified-2', 'agent-unified', 'pending', 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-unified",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 1, "first activate dispatches one entry");
    assert_eq!(json["dispatched"][0]["card_id"], "card-unified-1");

    // Verify dispatch was created and entry was linked
    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-u1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    let dispatch_id = dispatch_id.expect("entry must have linked dispatch_id");

    // Verify the dispatch references the correct card
    let dispatch_card: String = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_card, "card-unified-1");
    let notify_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        notify_count, 1,
        "auto-queue activation must use canonical notify persistence"
    );

    // Second entry stays pending (sequential within group)
    let entry2_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-u2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry2_status, "pending");

    // Run stays active (not prematurely completed)
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-unified'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_consult_required_creates_consultation_dispatch() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-consult");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-consult", 1720, "ready", "agent-consult");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-consult'",
            [serde_json::json!({
                "keep": "yes",
                "preflight_status": "consult_required",
                "preflight_summary": "need counter review"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-consult', 'test-repo', 'agent-consult', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-consult', 'run-consult', 'card-consult', 'agent-consult', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-consult",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "consultation dispatch should count as dispatched"
    );

    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-consult'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    let dispatch_id = dispatch_id.expect("consultation dispatch id must be stored");

    let (dispatch_type, to_agent_id, dispatch_context_raw): (String, String, Option<String>) = conn
        .query_row(
            "SELECT dispatch_type, to_agent_id, context FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "consultation");
    assert_eq!(to_agent_id, "agent-consult");
    let dispatch_context: serde_json::Value =
        serde_json::from_str(dispatch_context_raw.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(dispatch_context["auto_queue"], true);
    assert_eq!(dispatch_context["entry_id"], "entry-consult");
    assert_eq!(dispatch_context["thread_group"], 0);
    assert_eq!(dispatch_context["slot_index"], serde_json::Value::Null);
    assert_eq!(dispatch_context["run_id"], "run-consult");
    assert_eq!(dispatch_context["batch_phase"], 0);

    let metadata_raw: String = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = 'card-consult'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata_raw).unwrap();
    assert_eq!(metadata["keep"], "yes");
    assert_eq!(metadata["preflight_status"], "consult_required");
    assert_eq!(metadata["consultation_status"], "pending");
    assert_eq!(metadata["consultation_dispatch_id"], dispatch_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_consult_required_prefers_registry_counterpart_provider() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-qwen");
    seed_agent(&db, "agent-codex");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-consult-qwen", 1721, "ready", "agent-qwen");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE agents SET provider = 'qwen' WHERE id = 'agent-qwen'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE agents SET provider = 'codex' WHERE id = 'agent-codex'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-consult-qwen'",
            [serde_json::json!({
                "preflight_status": "consult_required",
                "preflight_summary": "need external consultation"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-consult-qwen', 'test-repo', 'agent-qwen', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-consult-qwen', 'run-consult-qwen', 'card-consult-qwen', 'agent-qwen', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-consult-qwen",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let to_agent_id: String = conn
        .query_row(
            "SELECT to_agent_id
             FROM task_dispatches
             WHERE kanban_card_id = 'card-consult-qwen'
             ORDER BY created_at DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(to_agent_id, "agent-codex");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_already_applied_skips_entry_and_completes_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-skip");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-skip", 1721, "ready", "agent-skip");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-skip'",
            [serde_json::json!({
                "preflight_status": "already_applied",
                "preflight_summary": "nothing to do"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-skip', 'test-repo', 'agent-skip', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-skip', 'run-skip', 'card-skip', 'agent-skip', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-skip",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 0,
        "already_applied entry should be skipped, not dispatched"
    );

    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-skip'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "skipped");
    assert!(
        dispatch_id.is_none(),
        "skipped entry must not create a dispatch"
    );

    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-skip'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_reuses_released_slot_for_next_group() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-slot");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-slot-0", 1722, "ready", "agent-slot");
    seed_auto_queue_card(&db, "card-slot-1", 1723, "ready", "agent-slot");
    seed_auto_queue_card(&db, "card-slot-2", 1724, "ready", "agent-slot");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-slot', 0, ?1)",
            [json!({"111": "222000000000000001"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-slot', 1, ?1)",
            [json!({"111": "222000000000000002"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-slot-thread-0', 'agent-slot', 'claude', 'turn_active',
                'slot 0 seed', 41, 'dispatch-slot-old-0', '222000000000000001', 'claude-slot-0',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-slot-thread-1', 'agent-slot', 'claude', 'turn_active',
                'slot 1 seed', 73, 'dispatch-slot-old-1', '222000000000000002', 'claude-slot-1',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-slot', 'test-repo', 'agent-slot', 'active', 1, 2, 3
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-0', 'run-slot', 'card-slot-0', 'agent-slot', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-1', 'run-slot', 'card-slot-1', 'agent-slot', 'pending', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-2', 'run-slot', 'card-slot-2', 'agent-slot', 'pending', 2, 2)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(
        first_json["count"], 2,
        "first activation must dispatch two groups in parallel when two slots are available"
    );

    {
        let conn = db.lock().unwrap();
        let first_slot_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let second_slot_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000002'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            first_slot_session.0, "idle",
            "slot session status must be idle after release"
        );
        assert_eq!(
            first_slot_session.1, None,
            "active_dispatch_id must be cleared on slot release"
        );
        assert_eq!(
            first_slot_session.2, 0,
            "tokens must be cleared so the slot starts from a fresh session"
        );
        assert!(
            first_slot_session.3.is_none(),
            "claude_session_id must be cleared on slot release"
        );
        assert_eq!(
            second_slot_session.0, "idle",
            "a reused sibling slot must also be reset before dispatch"
        );
        assert_eq!(
            second_slot_session.1, None,
            "a reused sibling slot must clear its prior dispatch context"
        );
        assert_eq!(
            second_slot_session.2, 0,
            "a reused sibling slot must clear prior token state"
        );
        assert!(
            second_slot_session.3.is_none(),
            "a reused sibling slot must clear claude_session_id"
        );
        let first_slot: Option<i64> = conn
            .query_row(
                "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let second_slot: Option<i64> = conn
            .query_row(
                "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_slot, Some(0));
        assert_eq!(second_slot, Some(1));
        conn.execute(
            "UPDATE sessions
             SET status = 'turn_active',
                 session_info = 'slot 0 in-progress context',
                 tokens = 99,
                 active_dispatch_id = 'dispatch-slot-in-progress',
                 claude_session_id = 'claude-slot-0-rehydrated'
             WHERE thread_channel_id = '222000000000000001'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions
             SET status = 'turn_active',
                 session_info = 'slot 1 should stay hot',
                 tokens = 123,
                 active_dispatch_id = 'dispatch-slot-1-hot',
                 claude_session_id = 'claude-slot-1-hot'
             WHERE thread_channel_id = '222000000000000002'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') WHERE id = 'entry-slot-0'",
            [],
        )
        .unwrap();
    }

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot",
                        "thread_group": 2,
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 1,
        "released slot should allow next group dispatch"
    );

    let conn = db.lock().unwrap();
    let recycled_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        recycled_slot,
        Some(0),
        "completed group slot must be reused for the next group"
    );
    let recycled_dispatch_context: Option<String> = conn
        .query_row(
            "SELECT td.context
             FROM task_dispatches td
             JOIN auto_queue_entries e ON e.dispatch_id = td.id
             WHERE e.id = 'entry-slot-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let recycled_dispatch_context =
        serde_json::from_str::<serde_json::Value>(&recycled_dispatch_context.unwrap()).unwrap();
    assert_eq!(
        recycled_dispatch_context["reset_slot_thread_before_reuse"].as_bool(),
        Some(true),
        "independent group reuse must force a fresh slot-thread reset"
    );

    let slot_zero_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group FROM auto_queue_slots WHERE agent_id = 'agent-slot' AND slot_index = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let slot_one_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group FROM auto_queue_slots WHERE agent_id = 'agent-slot' AND slot_index = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(slot_zero_group, Some(2));
    assert_eq!(slot_one_group, Some(1));

    let recycled_slot_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let untouched_slot_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000002'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        recycled_slot_session.0, "idle",
        "status must be idle after slot release"
    );
    assert_eq!(
        recycled_slot_session.1, None,
        "active_dispatch_id must be cleared"
    );
    assert_eq!(
        recycled_slot_session.2, 0,
        "recycled slot must clear prior token counts before the next dispatch"
    );
    assert!(
        recycled_slot_session.3.is_none(),
        "recycled slot must clear claude_session_id before the next dispatch"
    );
    assert_eq!(
        untouched_slot_session,
        (
            "turn_active".to_string(),
            Some("dispatch-slot-1-hot".to_string()),
            123,
            Some("claude-slot-1-hot".to_string())
        ),
        "active sibling group must not be cleared while it is still reusing its own context"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_dispatch_create_failure_releases_reserved_slot() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-dispatch-fail");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-dispatch-fail",
        4170,
        "ready",
        "agent-dispatch-fail",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-dispatch-fail', 'test-repo', 'agent-dispatch-fail', 'active', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-dispatch-fail', 'run-dispatch-fail', 'card-dispatch-fail',
                'agent-dispatch-fail', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_task_dispatch_insert
             BEFORE INSERT ON task_dispatches
             BEGIN
                 SELECT RAISE(ABORT, 'dispatch insert blocked');
             END;",
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-dispatch-fail",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 0,
        "failed create_dispatch must not report a dispatched group"
    );

    let conn = db.lock().unwrap();
    let entry_row: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-dispatch-fail'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(entry_row.0, "pending");
    assert!(entry_row.1.is_none());
    assert!(entry_row.2.is_none());

    let slot_row: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-dispatch-fail' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        slot_row,
        (None, None),
        "failed create_dispatch must release the reserved slot"
    );

    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-dispatch-fail'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_reuses_same_group_slot_with_fresh_session_each_time() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-same-group");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-same-group-0", 1822, "ready", "agent-same-group");
    seed_auto_queue_card(&db, "card-same-group-1", 1823, "ready", "agent-same-group");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group', 0, ?1)",
            [json!({"111": "222000000000000101"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-same-group-thread', 'agent-same-group', 'claude', 'turn_active',
                'slot seed', 17, 'dispatch-same-group-seed', '222000000000000101', 'claude-same-group-seed',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-same-group', 'test-repo', 'agent-same-group', 'active', 1, 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-0', 'run-same-group', 'card-same-group-0', 'agent-same-group', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-1', 'run-same-group', 'card-same-group-1', 'agent-same-group', 'pending', 1, 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);

    {
        let conn = db.lock().unwrap();
        let cleared_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000101'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            cleared_session,
            ("idle".to_string(), None, 0, None),
            "slot release must clear provider session continuity before dispatch"
        );
        conn.execute(
            "UPDATE sessions
             SET status = 'turn_active',
                 session_info = 'group context retained',
                 tokens = 77,
                 active_dispatch_id = 'dispatch-same-group-hot',
                 claude_session_id = 'claude-same-group-hot'
             WHERE thread_channel_id = '222000000000000101'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET status = 'done', completed_at = datetime('now')
             WHERE id = 'entry-same-group-0'",
            [],
        )
        .unwrap();
    }

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let continued_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000101'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let slot_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-same-group' AND slot_index = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        continued_session,
        ("idle".to_string(), None, 0, None),
        "same-group continuation must keep the slot assignment but start from a fresh session"
    );
    let continued_dispatch_context: Option<String> = conn
        .query_row(
            "SELECT td.context
             FROM task_dispatches td
             JOIN auto_queue_entries e ON e.dispatch_id = td.id
             WHERE e.id = 'entry-same-group-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let continued_dispatch_context =
        serde_json::from_str::<serde_json::Value>(&continued_dispatch_context.unwrap()).unwrap();
    assert!(
        continued_dispatch_context
            .get("reset_slot_thread_before_reuse")
            .is_none(),
        "same-group continuation must not force an independent slot-thread reset"
    );
    assert_eq!(
        slot_group,
        Some(0),
        "same-group continuation must keep the original slot assignment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_does_not_dispatch_same_group_follow_up_while_prior_is_active() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-same-group-guard");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-0",
        4160,
        "ready",
        "agent-same-group-guard",
    );
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-1",
        4161,
        "ready",
        "agent-same-group-guard",
    );
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-2",
        4162,
        "ready",
        "agent-same-group-guard",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group-guard', 0, ?1)",
            [json!({"111": "222000000000000201"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group-guard', 1, ?1)",
            [json!({"111": "222000000000000202"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-same-group-guard', 'test-repo', 'agent-same-group-guard', 'active', 1, 2, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-0', 'run-same-group-guard', 'card-same-group-guard-0', 'agent-same-group-guard', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-1', 'run-same-group-guard', 'card-same-group-guard-1', 'agent-same-group-guard', 'pending', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-2', 'run-same-group-guard', 'card-same-group-guard-2', 'agent-same-group-guard', 'pending', 0, 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group-guard",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(
        first_json["count"], 2,
        "first activate must dispatch both groups in parallel when slots are available"
    );

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group-guard",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 0,
        "same-group follow-up must stay pending while the prior entry is still dispatched"
    );

    let conn = db.lock().unwrap();
    let guard_entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-same-group-guard-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        guard_entry_status, "pending",
        "follow-up entry must not be marked dispatched before prior same-group work completes"
    );
    let sibling_group_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-same-group-guard-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sibling_group_status, "dispatched",
        "different thread_group for the same agent must dispatch in parallel when slots are available"
    );

    let guard_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM task_dispatches
             WHERE kanban_card_id = 'card-same-group-guard-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        guard_dispatch_count, 0,
        "no dispatch row should be created for the blocked same-group follow-up"
    );
    let sibling_group_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM task_dispatches
             WHERE kanban_card_id = 'card-same-group-guard-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sibling_group_dispatch_count, 1,
        "a sibling group should create its own dispatch while the same-group follow-up stays blocked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_expands_slot_pool_to_run_max_concurrency() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-slot-expand");
    ensure_auto_queue_tables(&db);

    for issue_number in 0..4 {
        seed_auto_queue_card(
            &db,
            &format!("card-slot-expand-{issue_number}"),
            1900 + issue_number,
            "ready",
            "agent-slot-expand",
        );
    }

    {
        let conn = db.lock().unwrap();
        for slot_index in 0..3 {
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES (?1, ?2, '{}')",
                sqlite_params!["agent-slot-expand", slot_index],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-slot-expand', 'test-repo', 'agent-slot-expand', 'active', 1, 4, 4
            )",
            [],
        )
        .unwrap();
        for (priority_rank, thread_group) in (0..4).enumerate() {
            conn.execute(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
                ) VALUES (?1, 'run-slot-expand', ?2, 'agent-slot-expand', 'pending', ?3, ?4)",
                sqlite_params![
                    format!("entry-slot-expand-{thread_group}"),
                    format!("card-slot-expand-{thread_group}"),
                    priority_rank as i64,
                    thread_group as i64,
                ],
            )
            .unwrap();
        }
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot-expand",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 4,
        "activate must dispatch all groups in parallel when slots are available"
    );
    assert_eq!(json["active_groups"], 4);

    let conn = db.lock().unwrap();
    let slot_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_slots WHERE agent_id = 'agent-slot-expand'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        slot_count, 4,
        "slot pool should expand from 3 seeded rows to match run max_concurrent_threads"
    );

    let mut stmt = conn
        .prepare(
            "SELECT slot_index
             FROM auto_queue_entries
             WHERE run_id = 'run-slot-expand'
             ORDER BY priority_rank ASC",
        )
        .unwrap();
    let mut assigned_slots = stmt
        .query_map([], |row| row.get::<_, Option<i64>>(0))
        .unwrap()
        .filter_map(|row| row.ok().flatten())
        .collect::<Vec<_>>();
    assigned_slots.sort_unstable();
    assert_eq!(assigned_slots, vec![0, 1, 2, 3]);

    let fourth_slot_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-slot-expand' AND slot_index = 3",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        fourth_slot_group,
        Some(3),
        "newly expanded slot must be assigned when parallel dispatch fills all slots"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_allows_same_agent_parallel_across_runs_when_free_slot_exists() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-cross-run");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-cross-run-active",
        1962,
        "requested",
        "agent-cross-run",
    );
    seed_auto_queue_card(&db, "card-cross-run-next", 1963, "ready", "agent-cross-run");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cross-run-active', 'test-repo', 'agent-cross-run', 'active', 2, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cross-run-next', 'test-repo', 'agent-cross-run', 'active', 2, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cross-run-active', 'run-cross-run-active', 'card-cross-run-active', 'agent-cross-run',
                'dispatched', 'dispatch-cross-run-active', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cross-run-next', 'run-cross-run-next', 'card-cross-run-next', 'agent-cross-run',
                'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group
            ) VALUES (
                'agent-cross-run', 0, 'run-cross-run-active', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cross-run-active', 'card-cross-run-active', 'agent-cross-run',
                'implementation', 'pending', 'Cross-run active dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-cross-run-next",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "a second run for the same agent should dispatch when another slot is free"
    );

    let conn = db.lock().unwrap();
    let next_entry: (String, Option<i64>) = conn
        .query_row(
            "SELECT status, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-cross-run-next'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(next_entry.0, "dispatched");
    assert_eq!(next_entry.1, Some(1));

    let next_slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-cross-run' AND slot_index = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(next_slot.0.as_deref(), Some("run-cross-run-next"));
    assert_eq!(next_slot.1, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_keeps_single_slot_agent_single_dispatched_group() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-single-slot");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-single-slot-0",
        1960,
        "ready",
        "agent-single-slot",
    );
    seed_auto_queue_card(
        &db,
        "card-single-slot-1",
        1961,
        "ready",
        "agent-single-slot",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-single-slot', 'test-repo', 'agent-single-slot', 'active', 1, 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-single-slot-0', 'run-single-slot', 'card-single-slot-0', 'agent-single-slot', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-single-slot-1', 'run-single-slot', 'card-single-slot-1', 'agent-single-slot', 'pending', 1, 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-single-slot",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "a single-slot run must still dispatch only one same-agent group"
    );
    assert_eq!(json["active_groups"], 1);

    let conn = db.lock().unwrap();
    let dispatched_entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries
             WHERE run_id = 'run-single-slot' AND status = 'dispatched'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatched_entries, 1);

    let pending_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-single-slot-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_slot, None,
        "the second group must stay unassigned until the lone slot becomes free"
    );

    let slot_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_slots WHERE agent_id = 'agent-single-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(slot_count, 1);
}

#[tokio::test]
async fn smart_generate_pg_creates_correct_thread_groups_and_batch_phases() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_parallel_test_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "max_concurrent_threads": 3,
                        "max_concurrent_per_agent": 3,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(entries.len(), 7, "all 7 cards should be queued");

    // Verify run keeps the requested concurrency cap while ignoring the
    // legacy max_concurrent_per_agent input.
    let run = &json["run"];
    assert_eq!(run["max_concurrent_threads"], 3);
    assert!(run.get("max_concurrent_per_agent").is_none());
    assert_eq!(run["ai_model"], "smart-planner");

    // Collect thread_group assignments per issue number.
    let mut groups: std::collections::HashMap<i64, Vec<(i64, i64, i64)>> =
        std::collections::HashMap::new();
    for entry in entries {
        let issue_num = entry["github_issue_number"].as_i64().unwrap();
        let thread_group = entry["thread_group"].as_i64().unwrap();
        let priority_rank = entry["priority_rank"].as_i64().unwrap();
        let batch_phase = entry["batch_phase"].as_i64().unwrap();
        groups
            .entry(thread_group)
            .or_default()
            .push((issue_num, priority_rank, batch_phase));
    }

    let group_count = run["thread_group_count"].as_i64().unwrap();
    assert_eq!(
        group_count,
        groups.len() as i64,
        "thread_group_count must match actual distinct groups"
    );

    // Independent cards (issues 1, 2, 3) should each be in their own group (size 1)
    let mut independent_groups = 0;
    let mut chain_group = None;
    for (group_num, members) in &groups {
        if members.len() == 1 {
            let issue = members[0].0;
            assert!(
                [1, 2, 3].contains(&issue),
                "single-member group should be an independent card, got issue #{issue}"
            );
            assert_eq!(
                members[0].2, 0,
                "independent cards should start in batch phase 0"
            );
            independent_groups += 1;
        } else {
            // This must be the dependency chain group
            assert!(
                chain_group.is_none(),
                "only one multi-member group expected"
            );
            chain_group = Some(*group_num);
        }
    }
    assert_eq!(independent_groups, 3, "3 independent cards → 3 groups");

    // Verify the chain group: issues 4,5,6,7 in topological order
    let chain = chain_group.expect("dependency chain group must exist");
    let mut chain_members = groups[&chain].clone();
    chain_members.sort_by_key(|(_, rank, _)| *rank);
    let chain_issues: Vec<i64> = chain_members.iter().map(|(num, _, _)| *num).collect();
    let chain_phases: Vec<i64> = chain_members.iter().map(|(_, _, phase)| *phase).collect();
    // Issue #4 must come first (rank 0), #5 second, then #6 and #7 (order between 6,7 may vary
    // since #7 depends on both #5 and #6, making #6 and #7 at different levels)
    assert_eq!(chain_issues[0], 4, "chain start (#4) must have lowest rank");
    assert_eq!(chain_issues[1], 5, "#5 depends on #4, must be second");
    // #6 depends on #5, #7 depends on #5 and #6 — so #6 before #7
    assert_eq!(chain_issues[2], 6, "#6 depends on #5, must be third");
    assert_eq!(chain_issues[3], 7, "#7 depends on #5 and #6, must be last");
    assert_eq!(
        chain_phases,
        vec![0, 1, 2, 3],
        "dependency chain should advance one batch phase at a time"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_rolls_back_when_entry_insert_fails() {
    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_parallel_test_cards(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_auto_queue_entry_insert
             BEFORE INSERT ON auto_queue_entries
             BEGIN
                 SELECT RAISE(ABORT, 'entry insert blocked');
             END;",
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "max_concurrent_threads": 3,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("create auto-queue entry"),
        "generate must expose entry insert failure instead of silently succeeding"
    );

    let conn = db.lock().unwrap();
    let run_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_runs", [], |row| row.get(0))
        .unwrap();
    let entry_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(run_count, 0, "failed generate must roll back run creation");
    assert_eq!(
        entry_count, 0,
        "failed generate must not leave partial entries"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_status_exposes_explicit_thread_links_from_configured_channels() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-thread-links");
    seed_auto_queue_card(
        &db,
        "card-thread-links",
        4131,
        "review",
        "agent-thread-links",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET channel_thread_map = ?1
             WHERE id = 'card-thread-links'",
            [json!({
                "111": "222000000000000001",
                "222": "222000000000000002"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-thread-links', 'test-repo', 'agent-thread-links', 'active', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-thread-links', 'run-thread-links', 'card-thread-links',
                'agent-thread-links', 'dispatched', 0
            )",
            [],
        )
        .unwrap();
    }

    let mut config = crate::config::Config::default();
    config.discord.guild_id = Some("1490141479707086938".to_string());
    let app = test_api_router_with_config(db.clone(), engine, config, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/queue/status?agent_id=agent-thread-links")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(thread_links.len(), 2);
    assert_eq!(thread_links[0]["label"], "work");
    assert_eq!(thread_links[0]["channel_id"], "111");
    assert_eq!(thread_links[0]["thread_id"], "222000000000000001");
    assert_eq!(
        thread_links[0]["url"],
        "https://discord.com/channels/1490141479707086938/222000000000000001"
    );
    assert_eq!(thread_links[1]["label"], "review");
    assert_eq!(thread_links[1]["channel_id"], "222");
    assert_eq!(thread_links[1]["thread_id"], "222000000000000002");
    assert_eq!(
        thread_links[1]["url"],
        "https://discord.com/channels/1490141479707086938/222000000000000002"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_history_returns_recent_runs_with_summary_metrics() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-history");
    seed_auto_queue_card(&db, "card-history-done", 5131, "done", "agent-history");
    seed_auto_queue_card(&db, "card-history-skipped", 5132, "done", "agent-history");
    seed_auto_queue_card(&db, "card-history-pending", 5133, "review", "agent-history");
    seed_auto_queue_card(
        &db,
        "card-history-dispatched",
        5134,
        "review",
        "agent-history",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at, completed_at
            ) VALUES (
                'run-history-completed', 'test-repo', 'agent-history', 'completed',
                datetime('now', '-20 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-history-active', 'test-repo', 'agent-history', 'active',
                datetime('now', '-5 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-history-done', 'run-history-completed', 'card-history-done',
                'agent-history', 'done', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-history-skipped', 'run-history-completed', 'card-history-skipped',
                'agent-history', 'skipped', 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-history-pending', 'run-history-active', 'card-history-pending',
                'agent-history', 'pending', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-history-dispatched', 'run-history-active', 'card-history-dispatched',
                'agent-history', 'dispatched', 1
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/queue/history?repo=test-repo&limit=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let runs = json["runs"].as_array().expect("runs must be an array");

    assert_eq!(json["summary"]["total_runs"], 2);
    assert_eq!(json["summary"]["completed_runs"], 1);
    assert_eq!(json["summary"]["success_rate"], 0.25);
    assert_eq!(json["summary"]["failure_rate"], 0.75);
    assert_eq!(runs.len(), 2);

    assert_eq!(runs[0]["id"], "run-history-active");
    assert_eq!(runs[0]["status"], "active");
    assert_eq!(runs[0]["entry_count"], 2);
    assert_eq!(runs[0]["done_count"], 0);
    assert_eq!(runs[0]["pending_count"], 1);
    assert_eq!(runs[0]["dispatched_count"], 1);
    assert_eq!(runs[0]["success_rate"], 0.0);
    assert_eq!(runs[0]["failure_rate"], 1.0);
    assert!(runs[0]["duration_ms"].as_i64().unwrap() >= 0);
    assert!(runs[0]["completed_at"].is_null());

    assert_eq!(runs[1]["id"], "run-history-completed");
    assert_eq!(runs[1]["status"], "completed");
    assert_eq!(runs[1]["entry_count"], 2);
    assert_eq!(runs[1]["done_count"], 1);
    assert_eq!(runs[1]["skipped_count"], 1);
    assert_eq!(runs[1]["success_rate"], 0.5);
    assert_eq!(runs[1]["failure_rate"], 0.5);
    assert!(runs[1]["duration_ms"].as_i64().unwrap() > 0);
    assert!(runs[1]["completed_at"].as_i64().unwrap() > runs[1]["created_at"].as_i64().unwrap());
}

#[tokio::test]
async fn auto_queue_status_pg_exposes_explicit_thread_links_from_configured_channels() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("agent-thread-links-pg")
    .bind("Agent Thread Links PG")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, channel_thread_map
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8::jsonb
         )",
    )
    .bind("card-thread-links-pg")
    .bind("test-repo")
    .bind("Issue #4131")
    .bind("review")
    .bind("medium")
    .bind("agent-thread-links-pg")
    .bind(4131_i64)
    .bind(
        json!({
            "111": "222000000000000001",
            "222": "222000000000000002"
        })
        .to_string(),
    )
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-thread-links-pg")
    .bind("test-repo")
    .bind("agent-thread-links-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("entry-thread-links-pg")
    .bind("run-thread-links-pg")
    .bind("card-thread-links-pg")
    .bind("agent-thread-links-pg")
    .bind("dispatched")
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let mut config = crate::config::Config::default();
    config.discord.guild_id = Some("1490141479707086938".to_string());
    let app = test_api_router_with_pg(db, engine, config, None, pg_pool.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/queue/status?agent_id=agent-thread-links-pg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(thread_links.len(), 2);
    assert_eq!(thread_links[0]["label"], "work");
    assert_eq!(thread_links[0]["channel_id"], "111");
    assert_eq!(thread_links[0]["thread_id"], "222000000000000001");
    assert_eq!(
        thread_links[0]["url"],
        "https://discord.com/channels/1490141479707086938/222000000000000001"
    );
    assert_eq!(thread_links[1]["label"], "review");
    assert_eq!(thread_links[1]["channel_id"], "222");
    assert_eq!(thread_links[1]["thread_id"], "222000000000000002");
    assert_eq!(
        thread_links[1]["url"],
        "https://discord.com/channels/1490141479707086938/222000000000000002"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_status_pg_repairs_thread_links_from_dispatch_history() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (
            id, name, provider, discord_channel_id, discord_channel_alt,
            discord_channel_cc, discord_channel_cdx
         ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind("agent-thread-repair-pg")
    .bind("Agent Thread Repair PG")
    .bind("codex")
    .bind("111")
    .bind("222")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id,
            github_issue_number, active_thread_id, channel_thread_map
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9::jsonb
         )",
    )
    .bind("card-thread-repair-pg")
    .bind("test-repo")
    .bind("Issue #1470")
    .bind("done")
    .bind("medium")
    .bind("agent-thread-repair-pg")
    .bind(1470_i64)
    .bind("1501968633650483273")
    .bind(json!({"111": "1501968633650483273"}).to_string())
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title,
            thread_id, updated_at
         ) VALUES
            ($1, $2, $3, 'implementation', 'completed', $4, $5, NOW() - INTERVAL '2 minutes'),
            ($6, $2, $3, 'review', 'cancelled', $7, $8, NOW() - INTERVAL '1 minute')",
    )
    .bind("dispatch-thread-repair-work")
    .bind("card-thread-repair-pg")
    .bind("agent-thread-repair-pg")
    .bind("Work dispatch")
    .bind("1501968633650483272")
    .bind("dispatch-thread-repair-review")
    .bind("Cancelled review dispatch")
    .bind("1501968633650483273")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-thread-repair-pg")
    .bind("test-repo")
    .bind("agent-thread-repair-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("entry-thread-repair-pg")
    .bind("run-thread-repair-pg")
    .bind("card-thread-repair-pg")
    .bind("agent-thread-repair-pg")
    .bind("done")
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let mut config = crate::config::Config::default();
    config.discord.guild_id = Some("1490141479707086938".to_string());
    let app = test_api_router_with_pg(db, engine, config, None, pg_pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/queue/status?agent_id=agent-thread-repair-pg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(
        thread_links,
        &vec![json!({
            "role": "work",
            "label": "work",
            "channel_id": "222",
            "thread_id": "1501968633650483272",
            "url": "https://discord.com/channels/1490141479707086938/1501968633650483272"
        })],
        "status should recover the completed work thread and suppress the cancelled review thread"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_history_pg_returns_recent_runs_with_summary_metrics() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name)
         VALUES ($1, $2)",
    )
    .bind("agent-history-pg")
    .bind("Agent History PG")
    .execute(&pg_pool)
    .await
    .unwrap();

    for (card_id, issue_number, status) in [
        ("card-history-done-pg", 5131_i64, "done"),
        ("card-history-skipped-pg", 5132_i64, "done"),
        ("card-history-pending-pg", 5133_i64, "review"),
        ("card-history-dispatched-pg", 5134_i64, "review"),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind(status)
        .bind("medium")
        .bind("agent-history-pg")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, created_at, completed_at
         ) VALUES (
            $1, $2, $3, $4, NOW() - INTERVAL '20 minutes', NOW() - INTERVAL '10 minutes'
         )",
    )
    .bind("run-history-completed-pg")
    .bind("test-repo")
    .bind("agent-history-pg")
    .bind("completed")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, created_at
         ) VALUES (
            $1, $2, $3, $4, NOW() - INTERVAL '5 minutes'
         )",
    )
    .bind("run-history-active-pg")
    .bind("test-repo")
    .bind("agent-history-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();

    for (entry_id, run_id, card_id, status, priority_rank) in [
        (
            "entry-history-done-pg",
            "run-history-completed-pg",
            "card-history-done-pg",
            "done",
            0_i64,
        ),
        (
            "entry-history-skipped-pg",
            "run-history-completed-pg",
            "card-history-skipped-pg",
            "skipped",
            1_i64,
        ),
        (
            "entry-history-pending-pg",
            "run-history-active-pg",
            "card-history-pending-pg",
            "pending",
            0_i64,
        ),
        (
            "entry-history-dispatched-pg",
            "run-history-active-pg",
            "card-history-dispatched-pg",
            "dispatched",
            1_i64,
        ),
    ] {
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
             ) VALUES (
                $1, $2, $3, $4, $5, $6
             )",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(card_id)
        .bind("agent-history-pg")
        .bind(status)
        .bind(priority_rank)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/queue/history?repo=test-repo&limit=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let runs = json["runs"].as_array().expect("runs must be an array");

    assert_eq!(json["summary"]["total_runs"], 2);
    assert_eq!(json["summary"]["completed_runs"], 1);
    assert_eq!(json["summary"]["success_rate"], 0.25);
    assert_eq!(json["summary"]["failure_rate"], 0.75);
    assert_eq!(runs.len(), 2);

    assert_eq!(runs[0]["id"], "run-history-active-pg");
    assert_eq!(runs[0]["status"], "active");
    assert_eq!(runs[0]["entry_count"], 2);
    assert_eq!(runs[0]["done_count"], 0);
    assert_eq!(runs[0]["pending_count"], 1);
    assert_eq!(runs[0]["dispatched_count"], 1);
    assert_eq!(runs[0]["success_rate"], 0.0);
    assert_eq!(runs[0]["failure_rate"], 1.0);
    assert!(runs[0]["duration_ms"].as_i64().unwrap() >= 0);
    assert!(runs[0]["completed_at"].is_null());

    assert_eq!(runs[1]["id"], "run-history-completed-pg");
    assert_eq!(runs[1]["status"], "completed");
    assert_eq!(runs[1]["entry_count"], 2);
    assert_eq!(runs[1]["done_count"], 1);
    assert_eq!(runs[1]["skipped_count"], 1);
    assert_eq!(runs[1]["success_rate"], 0.5);
    assert_eq!(runs[1]["failure_rate"], 0.5);
    assert!(runs[1]["duration_ms"].as_i64().unwrap() > 0);
    assert!(runs[1]["completed_at"].as_i64().unwrap() > runs[1]["created_at"].as_i64().unwrap());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_reset_pg_preserves_active_runs_on_global_reset() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-reset-pg")
        .bind("Agent Reset PG")
        .execute(&pg_pool)
        .await
        .unwrap();

    for (card_id, issue_number) in [
        ("card-reset-active-pg", 6231_i64),
        ("card-reset-generated-pg", 6232_i64),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind("ready")
        .bind("medium")
        .bind("agent-reset-pg")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4), ($5, $6, $7, $8)",
    )
    .bind("run-reset-active-pg")
    .bind("test-repo")
    .bind("agent-reset-pg")
    .bind("active")
    .bind("run-reset-generated-pg")
    .bind("test-repo")
    .bind("agent-reset-pg")
    .bind("generated")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         ), (
            $7, $8, $9, $10, $11, $12
         )",
    )
    .bind("entry-reset-active-pg")
    .bind("run-reset-active-pg")
    .bind("card-reset-active-pg")
    .bind("agent-reset-pg")
    .bind("pending")
    .bind(0_i64)
    .bind("entry-reset-generated-pg")
    .bind("run-reset-generated-pg")
    .bind("card-reset-generated-pg")
    .bind("agent-reset-pg")
    .bind("pending")
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/reset-global")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"confirmation_token":"confirm-global-reset"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["ok"], true);
    assert_eq!(json["deleted_entries"], 1);
    assert_eq!(json["completed_runs"], 1);
    assert_eq!(json["protected_active_runs"], 1);
    assert_eq!(
        json["warning"],
        "global reset preserved 1 active run(s); use agent_id to reset a specific queue"
    );

    let active_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-reset-active-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    let generated_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-reset-generated-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    let active_entry_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_entries WHERE run_id = $1",
    )
    .bind("run-reset-active-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    let generated_entry_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_entries WHERE run_id = $1",
    )
    .bind("run-reset-generated-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();

    assert_eq!(active_status, "active");
    assert_eq!(generated_status, "completed");
    assert_eq!(active_entry_count, 1);
    assert_eq!(generated_entry_count, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_reorder_pg_updates_priority_ranks() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-reorder-pg")
        .bind("Agent Reorder PG")
        .execute(&pg_pool)
        .await
        .unwrap();

    for (card_id, issue_number) in [
        ("card-reorder-1-pg", 6331_i64),
        ("card-reorder-2-pg", 6332_i64),
        ("card-reorder-3-pg", 6333_i64),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind("ready")
        .bind("medium")
        .bind("agent-reorder-pg")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-reorder-pg")
    .bind("test-repo")
    .bind("agent-reorder-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    for (entry_id, card_id, priority_rank) in [
        ("entry-reorder-1-pg", "card-reorder-1-pg", 0_i64),
        ("entry-reorder-2-pg", "card-reorder-2-pg", 1_i64),
        ("entry-reorder-3-pg", "card-reorder-3-pg", 2_i64),
    ] {
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
             ) VALUES (
                $1, $2, $3, $4, $5, $6
             )",
        )
        .bind(entry_id)
        .bind("run-reorder-pg")
        .bind(card_id)
        .bind("agent-reorder-pg")
        .bind("pending")
        .bind(priority_rank)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/queue/reorder")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "ordered_ids": [
                            "entry-reorder-3-pg",
                            "entry-reorder-1-pg",
                            "entry-reorder-2-pg"
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);

    let ordered_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY priority_rank ASC, created_at ASC, id ASC",
    )
    .bind("run-reorder-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();

    assert_eq!(
        ordered_ids,
        vec![
            "entry-reorder-3-pg".to_string(),
            "entry-reorder-1-pg".to_string(),
            "entry-reorder-2-pg".to_string(),
        ]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_add_run_entry_pg_creates_pending_entry_for_active_run() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("project-agentdesk")
        .bind("Project AgentDesk")
        .execute(&pg_pool)
        .await
        .unwrap();

    for (card_id, issue_number) in [
        ("card-run-entry-existing-pg", 7121_i64),
        ("card-run-entry-new-pg", 7122_i64),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind("ready")
        .bind("medium")
        .bind("project-agentdesk")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("run-add-entry-active-pg")
    .bind("test-repo")
    .bind("project-agentdesk")
    .bind("active")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8
         )",
    )
    .bind("entry-run-entry-existing-pg")
    .bind("run-add-entry-active-pg")
    .bind("card-run-entry-existing-pg")
    .bind("project-agentdesk")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-add-entry-active-pg/entries")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "issue_number": 7122,
                        "batch_phase": 4,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["run_id"], "run-add-entry-active-pg");
    assert_eq!(json["thread_group"], 1);
    assert_eq!(json["priority_rank"], 0);
    assert_eq!(json["entry"]["card_id"], "card-run-entry-new-pg");

    let inserted: (i64, i64, i64, String) = sqlx::query_as(
        "SELECT priority_rank::BIGINT,
                COALESCE(thread_group, 0)::BIGINT,
                COALESCE(batch_phase, 0)::BIGINT,
                status
         FROM auto_queue_entries
         WHERE run_id = $1
           AND kanban_card_id = $2",
    )
    .bind("run-add-entry-active-pg")
    .bind("card-run-entry-new-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(inserted, (0, 1, 4, "pending".to_string()));

    let run_meta: (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(thread_group_count, 0)::BIGINT,
                COALESCE(max_concurrent_threads, 0)::BIGINT
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind("run-add-entry-active-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(run_meta, (2, 2));

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_submit_order_pg_activates_pending_run_and_skips_non_dispatchable_cards() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let dispatchable_state = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("project-agentdesk")
        .bind("Project AgentDesk")
        .execute(&pg_pool)
        .await
        .unwrap();

    for (card_id, issue_number, status) in [
        ("card-order-backlog-pg", 7421_i64, "backlog".to_string()),
        (
            "card-order-ready-a-pg",
            7422_i64,
            dispatchable_state.clone(),
        ),
        (
            "card-order-ready-b-pg",
            7423_i64,
            dispatchable_state.clone(),
        ),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind(status)
        .bind("medium")
        .bind("project-agentdesk")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-submit-order-pg")
    .bind("test-repo")
    .bind("project-agentdesk")
    .bind("pending")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-submit-order-pg/order")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .header("x-agent-id", "project-agentdesk")
                .body(Body::from(
                    json!({
                        "order": [7421, "card-order-ready-b-pg", 7422],
                        "rationale": "manual pg ordering",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["created"], 2);
    assert_eq!(json["run_id"], "run-submit-order-pg");

    let run_status: (String, Option<String>) = sqlx::query_as(
        "SELECT status, ai_rationale
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind("run-submit-order-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(run_status.0, "active");
    assert_eq!(run_status.1.as_deref(), Some("manual pg ordering"));

    let entries: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kanban_card_id, priority_rank::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY priority_rank ASC, id ASC",
    )
    .bind("run-submit-order-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        entries,
        vec![
            ("card-order-ready-b-pg".to_string(), 1),
            ("card-order-ready-a-pg".to_string(), 2),
        ]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_status_legacy_thread_falls_back_to_active_label_without_url_guessing() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-thread-links-legacy");
    seed_auto_queue_card(
        &db,
        "card-thread-links-legacy",
        4132,
        "in_progress",
        "agent-thread-links-legacy",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET active_thread_id = '333000000000000009'
             WHERE id = 'card-thread-links-legacy'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-thread-links-legacy', 'test-repo', 'agent-thread-links-legacy',
                'active', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-thread-links-legacy', 'run-thread-links-legacy',
                'card-thread-links-legacy', 'agent-thread-links-legacy', 'pending', 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/queue/status?agent_id=agent-thread-links-legacy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(thread_links.len(), 1);
    assert_eq!(thread_links[0]["label"], "active");
    assert_eq!(thread_links[0]["role"], "active");
    assert_eq!(thread_links[0]["thread_id"], "333000000000000009");
    assert_eq!(thread_links[0]["url"], serde_json::Value::Null);
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_status_scopes_global_run_entries_by_repo_and_agent() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_repo(&db, "other-repo");
    seed_agent(&db, "agent-scope-a");
    seed_agent(&db, "agent-scope-b");
    seed_auto_queue_card(&db, "card-scope-a", 4201, "ready", "agent-scope-a");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
             ) VALUES (
                'card-scope-b', 'Issue #4202', 'ready', 'medium', 'agent-scope-b', 'other-repo',
                4202, datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, status, created_at)
             VALUES ('run-scope-global', 'active', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, reason
             ) VALUES (
                'entry-scope-a', 'run-scope-global', 'card-scope-a', 'agent-scope-a',
                'pending', 0, 0, 'scope group a'
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, reason
             ) VALUES (
                'entry-scope-b', 'run-scope-global', 'card-scope-b', 'agent-scope-b',
                'dispatched', 1, 1, 'scope group b'
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/queue/status?repo=test-repo&agent_id=agent-scope-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["run"]["id"], "run-scope-global");
    let entries = json["entries"]
        .as_array()
        .expect("entries must be an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "entry-scope-a");

    let agents = json["agents"]
        .as_object()
        .expect("agents must be an object");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents["agent-scope-a"]["pending"], 1);
    assert!(agents.get("agent-scope-b").is_none());

    let thread_groups = json["thread_groups"]
        .as_object()
        .expect("thread_groups must be an object");
    assert_eq!(thread_groups.len(), 1);
    assert_eq!(thread_groups["0"]["pending"], 1);
    assert_eq!(thread_groups["0"]["status"], "pending");
    assert_eq!(thread_groups["0"]["reason"], "scope group a");
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_issue_numbers_filters_cards_and_promotes_backlog() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-327");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-ready', 'Generate Ready #327', 'ready', 'high', 'agent-generate-327', 'test-repo', 3271, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-backlog', 'Generate Backlog #327', 'backlog', 'medium', 'agent-generate-327', 'test-repo', 3272, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-extra', 'Generate Extra', 'ready', 'urgent', 'agent-generate-327', 'test-repo', 3999, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-327",
                        "issue_numbers": [3271, 3272],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = json["entries"].as_array().unwrap();
    let queued_issues: Vec<i64> = entries
        .iter()
        .filter_map(|entry| entry["github_issue_number"].as_i64())
        .collect();
    assert_eq!(entries.len(), 2);
    assert!(queued_issues.contains(&3271));
    assert!(queued_issues.contains(&3272));
    assert!(!queued_issues.contains(&3999));

    let conn = db.lock().unwrap();
    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-gen-327-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        backlog_status, "ready",
        "selected backlog card must be promoted before queue generation"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_rejects_when_live_run_exists_without_force() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-generate-conflict");
    seed_repo(&db, "test-repo");
    seed_auto_queue_card(
        &db,
        "card-gen-conflict-backlog",
        3281,
        "backlog",
        "agent-generate-conflict",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-generate-conflict', 'test-repo', 'agent-generate-conflict', 'active', datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-conflict",
                        "issue_numbers": [3281],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["existing_run_id"], "run-generate-conflict");
    assert_eq!(json["existing_run_status"], "active");

    let conn = db.lock().unwrap();
    let run_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        run_count, 1,
        "generate conflict must not create a second run"
    );

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-gen-conflict-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        backlog_status, "backlog",
        "generate conflict must happen before backlog auto-promotion"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_empty_state_reports_filtered_counts() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-counts");
    seed_agent(&db, "other-agent");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number,
                created_at, updated_at
            ) VALUES (
                'card-generate-counts-backlog', 'Generate Counts Backlog', 'backlog', 'medium',
                'agent-generate-counts', 'test-repo', 5410, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number,
                created_at, updated_at
            ) VALUES (
                'card-generate-counts-other-agent', 'Other Agent Ready', 'ready', 'high',
                'other-agent', 'test-repo', 5411, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-counts",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["message"], "No dispatchable cards found");
    assert_eq!(json["counts"]["backlog"], 1);
    assert_eq!(json["counts"]["ready"], 0);
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_accepts_but_ignores_unified_thread_flag() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-unified");
    seed_repo(&db, "test-repo");
    seed_auto_queue_card(
        &db,
        "card-generate-unified",
        3881,
        "ready",
        "agent-generate-unified",
    );

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-unified",
                        "unified_thread": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["run"]["unified_thread"], serde_json::json!(false));

    let run_id = json["run"]["id"]
        .as_str()
        .expect("generated run id must be present");
    let conn = db.lock().unwrap();
    let stored_unified_thread: i64 = conn
        .query_row(
            "SELECT unified_thread FROM auto_queue_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_unified_thread, 0,
        "generate must ignore unified_thread and keep slot pooling enabled"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_entries_payload_persists_batch_phases() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-phase");
    seed_repo(&db, "test-repo");
    seed_auto_queue_card(
        &db,
        "card-generate-phase-1",
        4231,
        "ready",
        "agent-generate-phase",
    );
    seed_auto_queue_card(
        &db,
        "card-generate-phase-2",
        4232,
        "ready",
        "agent-generate-phase",
    );

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-phase",
                        "entries": [
                            { "issue_number": 4232, "batch_phase": 2 },
                            { "issue_number": 4231, "batch_phase": 1 }
                        ],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = json["entries"].as_array().expect("entries must be array");

    let phases_by_issue: std::collections::HashMap<i64, i64> = entries
        .iter()
        .filter_map(|entry| {
            Some((
                entry["github_issue_number"].as_i64()?,
                entry["batch_phase"].as_i64()?,
            ))
        })
        .collect();

    assert_eq!(phases_by_issue.get(&4231), Some(&1));
    assert_eq!(phases_by_issue.get(&4232), Some(&2));

    let conn = db.lock().unwrap();
    let stored_phases: std::collections::HashMap<i64, i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, COALESCE(e.batch_phase, 0)
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .unwrap()
            .filter_map(|row| row.ok())
            .collect()
    };
    assert_eq!(stored_phases.get(&4231), Some(&1));
    assert_eq!(stored_phases.get(&4232), Some(&2));
}

#[tokio::test]
async fn generate_smart_planner_pg_groups_by_file_paths_and_recommends_threads() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_similarity_group_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(
        entries.len(),
        5,
        "all similarity test cards should be queued"
    );
    assert_eq!(
        run["thread_group_count"].as_i64().unwrap(),
        5,
        "similarity-only cards stay in distinct groups and are staggered by phase"
    );
    assert_eq!(
        run["max_concurrent_threads"].as_i64().unwrap(),
        4,
        "recommended concurrency is capped even when smart planner emits more groups"
    );
    assert_eq!(run["ai_model"].as_str().unwrap(), "smart-planner");

    let staggered_entries = entries
        .iter()
        .filter(|entry| {
            entry["batch_phase"]
                .as_i64()
                .map(|phase| phase > 0)
                .unwrap_or(false)
        })
        .count();
    assert!(
        staggered_entries >= 2,
        "similarity signals should still stagger conflicting work into later phases"
    );

    let status_resp = app
        .oneshot(
            Request::builder()
                .uri("/queue/status?repo=test-repo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    let thread_groups = status_json["thread_groups"]
        .as_object()
        .expect("thread_groups must be present");
    assert_eq!(
        thread_groups.len(),
        5,
        "status should expose all planner-emitted thread groups"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generate_smart_planner_pg_without_file_paths_uses_dependency_only_groups() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_parallel_test_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(
        entries.len(),
        7,
        "all dependency-seed cards should be queued"
    );
    assert_eq!(
        run["thread_group_count"].as_i64().unwrap(),
        4,
        "without file paths, smart planner must fall back to dependency-only grouping"
    );
    assert_eq!(run["ai_model"].as_str().unwrap(), "smart-planner");
    assert!(
        run["ai_rationale"]
            .as_str()
            .map(|text| text.contains("파일 경로 신호 없이"))
            .unwrap_or(false),
        "rationale should explain the dependency-only fallback"
    );
    assert!(
        entries.iter().all(|entry| {
            entry["reason"]
                .as_str()
                .map(|reason| !reason.contains("유사도 그룹"))
                .unwrap_or(true)
        }),
        "fallback path should not stamp similarity reasons"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn generate_pg_ignores_non_dependency_issue_references_in_description() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-context").await;
    seed_auto_queue_card_pg(&pool, "card-context-only", 497, "ready", "agent-context").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-referenced-open",
        494,
        "backlog",
        "agent-context",
    )
    .await;
    sqlx::query(
        "UPDATE kanban_cards
         SET description = $1
         WHERE id = 'card-context-only'",
    )
    .bind("## 컨텍스트\n관련 작업: #494")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(
        entries.len(),
        1,
        "context-only references must not exclude the card"
    );
    assert_eq!(entries[0]["github_issue_number"].as_i64(), Some(497));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn generate_pg_excludes_card_with_explicit_external_dependency() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-dependency").await;
    seed_auto_queue_card_pg(&pool, "card-explicit-dep", 497, "ready", "agent-dependency").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-explicit-target",
        494,
        "backlog",
        "agent-dependency",
    )
    .await;
    sqlx::query(
        "UPDATE kanban_cards
         SET description = $1
         WHERE id = 'card-explicit-dep'",
    )
    .bind("Depends on #494")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["run"].is_null(),
        "explicit unresolved dependencies should prevent queue generation"
    );
    assert_eq!(
        json["message"].as_str(),
        Some("No cards available (1개 외부 의존성 미충족으로 제외)")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn generate_pg_ignores_legacy_mode_and_still_uses_smart_planner() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_similarity_group_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "mode": "pm-assisted",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(run["thread_group_count"], 5);
    assert_eq!(run["max_concurrent_threads"], 4);
    assert_eq!(run["ai_model"], "smart-planner");
    assert!(
        !entries.is_empty(),
        "legacy mode input should be ignored rather than triggering PM-assisted flow"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn generate_pg_ignores_legacy_parallel_toggle_and_keeps_smart_groups() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_parallel_test_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "parallel": false,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let entries = json["entries"].as_array().unwrap();
    let run = &json["run"];

    let distinct_groups = entries
        .iter()
        .map(|entry| entry["thread_group"].as_i64().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(run["thread_group_count"], 4);
    assert_eq!(run["max_concurrent_threads"], 4);
    assert_eq!(run["ai_model"], "smart-planner");
    assert_eq!(
        distinct_groups.len(),
        4,
        "legacy parallel=false should be ignored in favor of smart grouping"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activate_waits_for_current_batch_phase_pg_before_dispatching_next_phase() {
    crate::pipeline::ensure_loaded();

    let (repo, _repo_guard) = setup_test_repo();
    let config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config_guard = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &config_dir.path().join("agentdesk.yaml"),
    );

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-phase-a").await;
    seed_agent_pg(&pool, "agent-phase-b").await;
    seed_auto_queue_card_pg(&pool, "card-phase-1-a", 4241, "ready", "agent-phase-a").await;
    seed_auto_queue_card_pg(&pool, "card-phase-1-b", 4242, "ready", "agent-phase-b").await;
    seed_auto_queue_card_pg(&pool, "card-phase-2-a", 4243, "ready", "agent-phase-a").await;
    seed_auto_queue_card_pg(&pool, "card-phase-2-b", 4244, "ready", "agent-phase-b").await;

    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, status, max_concurrent_threads, thread_group_count
        ) VALUES (
            'run-batch-phase', 'test-repo', 'active', 2, 2
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
        ) VALUES
        ('entry-phase-1-a', 'run-batch-phase', 'card-phase-1-a', 'agent-phase-a', 'pending', 0, 0, 1),
        ('entry-phase-1-b', 'run-batch-phase', 'card-phase-1-b', 'agent-phase-b', 'pending', 1, 1, 1),
        ('entry-phase-2-a', 'run-batch-phase', 'card-phase-2-a', 'agent-phase-a', 'pending', 2, 0, 2),
        ('entry-phase-2-b', 'run-batch-phase', 'card-phase-2-b', 'agent-phase-b', 'pending', 3, 1, 2)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(first_json["count"], 2);

    let dispatched_phases: Vec<(String, i64)> = sqlx::query_as(
        "SELECT id, COALESCE(batch_phase, 0)::BIGINT
         FROM auto_queue_entries
         WHERE status = 'dispatched'
         ORDER BY id ASC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let dispatched_phases: std::collections::HashMap<String, i64> =
        dispatched_phases.into_iter().collect();
    assert_eq!(dispatched_phases.len(), 2);
    assert_eq!(dispatched_phases.get("entry-phase-1-a"), Some(&1));
    assert_eq!(dispatched_phases.get("entry-phase-1-b"), Some(&1));

    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'done', dispatch_id = NULL, completed_at = NOW()
         WHERE id = 'entry-phase-1-a'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let second_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 0,
        "phase 2 must stay blocked while phase 1 still has an in-flight entry"
    );

    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'done', dispatch_id = NULL, completed_at = NOW()
         WHERE id = 'entry-phase-1-b'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let third_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(third_response.status(), StatusCode::OK);
    let third_body = axum::body::to_bytes(third_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let third_json: serde_json::Value = serde_json::from_slice(&third_body).unwrap();
    assert_eq!(
        third_json["count"], 2,
        "next batch phase should become dispatchable once phase 1 is complete"
    );

    let phase_two_dispatched = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_entries
         WHERE status = 'dispatched' AND COALESCE(batch_phase, 0) = 2",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(phase_two_dispatched, 2);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_pause_soft_does_not_cancel_live_dispatches_or_release_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-dispatched",
        4496,
        "in_progress",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-pending", 4497, "ready", "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-phase-gate-anchor",
        4498,
        "ready",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-orphan", 4499, "ready", "agent-pause-slot");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-pause-slot', 'test-repo', 'agent-pause-slot', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-pause-slot', 0, 'run-pause-slot', 0, ?1
            )",
            [json!({"111": "222000000000004496"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-pause-slot', 'card-pause-dispatched', 'agent-pause-slot',
                'implementation', 'dispatched', 'Pause slot dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
            ) VALUES (
                'dispatch-pause-phase-gate', 'card-pause-phase-gate-anchor', 'agent-pause-slot',
                'review', 'dispatched', 'Pause phase gate', ?1, datetime('now'), datetime('now')
            )",
            [json!({
                "slot_index": 0,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-pause-slot",
                    "batch_phase": 1,
                    "next_phase": 2,
                    "anchor_card_id": "card-pause-phase-gate-anchor"
                }
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-dispatched', 'run-pause-slot', 'card-pause-dispatched',
                'agent-pause-slot', 'dispatched', 'dispatch-pause-slot', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-orphan', 'run-pause-slot', 'card-pause-orphan',
                'agent-pause-slot', 'dispatched', NULL, NULL, 2, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-pause-pending', 'run-pause-slot', 'card-pause-pending',
                'agent-pause-slot', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict, next_phase, final_phase, anchor_card_id, created_at, updated_at
            ) VALUES (
                'run-pause-slot', 1, 'pending', 'dispatch-pause-phase-gate',
                'phase_gate_passed', 2, 0, 'card-pause-phase-gate-anchor', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-slot', 'agent-pause-slot', 'claude', 'turn_active',
                'pause slot seed', 19, 'dispatch-pause-slot', '222000000000004496', 'claude-pause-slot',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, claude_session_id, last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-sidecar', 'agent-pause-slot', 'claude', 'turn_active',
                'pause sidecar seed', 7, 'dispatch-pause-phase-gate', 'claude-pause-sidecar',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/pause")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["paused_runs"], 1);
    assert_eq!(json["cancelled_dispatches"], 0);
    assert_eq!(json["released_slots"], 0);
    assert_eq!(json["cleared_slot_sessions"], 0);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "paused");

    let dispatched_entry: (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id, dispatched_at
             FROM auto_queue_entries
             WHERE id = 'entry-pause-dispatched'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatched_entry.0, "dispatched");
    assert_eq!(dispatched_entry.1, Some("dispatch-pause-slot".to_string()));
    assert!(
        dispatched_entry.2.is_some(),
        "soft pause must leave the in-flight dispatch timestamp untouched"
    );

    let pending_entry: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-pause-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_entry, "pending");

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "dispatched");

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-pause-slot' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (Some("run-pause-slot".to_string()), Some(0)));

    let session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-pause-slot'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        session,
        (
            "turn_active".to_string(),
            Some("dispatch-pause-slot".to_string()),
            19,
            Some("claude-pause-slot".to_string()),
        )
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_pause_force_cancels_live_dispatches_and_releases_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-dispatched",
        4496,
        "in_progress",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-pending", 4497, "ready", "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-phase-gate-anchor",
        4498,
        "ready",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-orphan", 4499, "ready", "agent-pause-slot");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-pause-slot', 'test-repo', 'agent-pause-slot', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-pause-slot', 0, 'run-pause-slot', 0, ?1
            )",
            [json!({"111": "222000000000004496"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-pause-slot', 'card-pause-dispatched', 'agent-pause-slot',
                'implementation', 'dispatched', 'Pause slot dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
            ) VALUES (
                'dispatch-pause-phase-gate', 'card-pause-phase-gate-anchor', 'agent-pause-slot',
                'review', 'dispatched', 'Pause phase gate', ?1, datetime('now'), datetime('now')
            )",
            [json!({
                "slot_index": 0,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-pause-slot",
                    "batch_phase": 1,
                    "next_phase": 2,
                    "anchor_card_id": "card-pause-phase-gate-anchor"
                }
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-dispatched', 'run-pause-slot', 'card-pause-dispatched',
                'agent-pause-slot', 'dispatched', 'dispatch-pause-slot', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-orphan', 'run-pause-slot', 'card-pause-orphan',
                'agent-pause-slot', 'dispatched', NULL, NULL, 2, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-pause-pending', 'run-pause-slot', 'card-pause-pending',
                'agent-pause-slot', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict, next_phase, final_phase, anchor_card_id, created_at, updated_at
            ) VALUES (
                'run-pause-slot', 1, 'pending', 'dispatch-pause-phase-gate',
                'phase_gate_passed', 2, 0, 'card-pause-phase-gate-anchor', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-slot', 'agent-pause-slot', 'claude', 'turn_active',
                'pause slot seed', 19, 'dispatch-pause-slot', '222000000000004496', 'claude-pause-slot',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, claude_session_id, last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-sidecar', 'agent-pause-slot', 'claude', 'turn_active',
                'pause sidecar seed', 7, 'dispatch-pause-phase-gate', 'claude-pause-sidecar',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/pause")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({"force": true})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["paused_runs"], 1);
    assert_eq!(json["cancelled_dispatches"], 2);
    assert_eq!(json["released_slots"], 1);
    assert_eq!(json["cleared_slot_sessions"], 2);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "paused");

    let dispatched_entry_status: String = conn
        .query_row(
            "SELECT status
             FROM auto_queue_entries
             WHERE id = 'entry-pause-dispatched'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatched_entry_status, "skipped");

    let pending_entry: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-pause-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_entry, "pending");

    let orphan_entry: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-pause-orphan'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(orphan_entry.0, "pending");
    assert!(orphan_entry.1.is_none());
    assert!(orphan_entry.2.is_none());

    let dispatch_statuses: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, status
             FROM task_dispatches
             WHERE id IN ('dispatch-pause-slot', 'dispatch-pause-phase-gate')
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        dispatch_statuses,
        vec![
            (
                "dispatch-pause-phase-gate".to_string(),
                "cancelled".to_string(),
            ),
            ("dispatch-pause-slot".to_string(), "cancelled".to_string()),
        ]
    );

    let phase_gate_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = 'run-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase_gate_rows, 0);

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-pause-slot' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (None, None));

    let session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-pause-slot'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(session.0, "idle");
    assert_eq!(session.1, None);
    assert_eq!(session.2, 0);
    assert_eq!(session.3, None);

    let sidecar_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-pause-sidecar'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(sidecar_session.0, "idle");
    assert_eq!(sidecar_session.1, None);
    assert_eq!(sidecar_session.2, 0);
    assert_eq!(sidecar_session.3, None);
}

#[tokio::test]
async fn auto_queue_pause_pg_soft_does_not_cancel_live_dispatches_or_release_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-pause-slot-pg")
    .bind("Agent Pause Slot PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-pause-dispatched-pg")
    .bind("test-repo")
    .bind("Pause dispatched PG")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-pause-slot-pg")
    .bind(5496_i64)
    .bind("card-pause-pending-pg")
    .bind("test-repo")
    .bind("Pause pending PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-pause-slot-pg")
    .bind(5497_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("run-pause-slot-pg")
    .bind("test-repo")
    .bind("agent-pause-slot-pg")
    .bind("active")
    .bind(1_i64)
    .bind(2_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
         ) VALUES (
            $1, $2, $3, $4, $5::jsonb
         )",
    )
    .bind("agent-pause-slot-pg")
    .bind(0_i64)
    .bind("run-pause-slot-pg")
    .bind(0_i64)
    .bind(json!({"111": "222000000000054496"}).to_string())
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("dispatch-pause-slot-pg")
    .bind("card-pause-dispatched-pg")
    .bind("agent-pause-slot-pg")
    .bind("implementation")
    .bind("dispatched")
    .bind("Pause slot dispatch PG")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, NOW()
         ), (
            $10, $11, $12, $13, $14, NULL, NULL, $15, $16, NULL
         )",
    )
    .bind("entry-pause-dispatched-pg")
    .bind("run-pause-slot-pg")
    .bind("card-pause-dispatched-pg")
    .bind("agent-pause-slot-pg")
    .bind("dispatched")
    .bind("dispatch-pause-slot-pg")
    .bind(0_i64)
    .bind(0_i64)
    .bind(0_i64)
    .bind("entry-pause-pending-pg")
    .bind("run-pause-slot-pg")
    .bind("card-pause-pending-pg")
    .bind("agent-pause-slot-pg")
    .bind("pending")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, session_info, tokens,
            active_dispatch_id, thread_channel_id, claude_session_id
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9
         )",
    )
    .bind("host:AgentDesk-claude-pause-slot-pg")
    .bind("agent-pause-slot-pg")
    .bind("claude")
    .bind("turn_active")
    .bind("pause slot seed pg")
    .bind(19_i64)
    .bind("dispatch-pause-slot-pg")
    .bind("222000000000054496")
    .bind("claude-pause-slot-pg")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/pause")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["paused_runs"], 1);
    assert_eq!(json["cancelled_dispatches"], 0);
    assert_eq!(json["released_slots"], 0);
    assert_eq!(json["cleared_slot_sessions"], 0);

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-pause-slot-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "paused");

    let dispatched_entry: (
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT status, dispatch_id, dispatched_at
             FROM auto_queue_entries
             WHERE id = $1",
    )
    .bind("entry-pause-dispatched-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(dispatched_entry.0, "dispatched");
    assert_eq!(
        dispatched_entry.1,
        Some("dispatch-pause-slot-pg".to_string())
    );
    assert!(
        dispatched_entry.2.is_some(),
        "soft pause must leave the postgres dispatch timestamp untouched"
    );

    let dispatch_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pause-slot-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "dispatched");

    let slot: (Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT assigned_run_id, assigned_thread_group
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind("agent-pause-slot-pg")
    .bind(0_i64)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(slot, (Some("run-pause-slot-pg".to_string()), Some(0)));

    let session: (String, Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, tokens::BIGINT, claude_session_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind("host:AgentDesk-claude-pause-slot-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        session,
        (
            "turn_active".to_string(),
            Some("dispatch-pause-slot-pg".to_string()),
            19,
            Some("claude-pause-slot-pg".to_string()),
        )
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_reset_slot_thread_pg_clears_slot_binding_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ($1, $2, $3)",
    )
    .bind("agent-reset-slot-pg")
    .bind("Agent Reset Slot PG")
    .bind("claude")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
         ) VALUES (
            $1, $2, $3, $4, $5::jsonb
         )",
    )
    .bind("agent-reset-slot-pg")
    .bind(0_i64)
    .bind("run-reset-slot-pg")
    .bind(0_i64)
    .bind("{}")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/slots/agent-reset-slot-pg/0/reset-thread")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["agent_id"], "agent-reset-slot-pg");
    assert_eq!(json["slot_index"], 0);
    assert_eq!(json["archived_threads"], 0);
    assert_eq!(json["cleared_sessions"], 0);
    assert_eq!(json["cleared_bindings"], 1);

    let slot_map = sqlx::query_scalar::<_, Option<String>>(
        "SELECT thread_id_map::text
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind("agent-reset-slot-pg")
    .bind(0_i64)
    .fetch_one(&pg_pool)
    .await
    .unwrap()
    .unwrap();
    assert_eq!(slot_map, "{}");

    let sqlite_slot_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_slots WHERE agent_id = 'agent-reset-slot-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_slot_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_cancels_live_dispatches_skips_entries_and_releases_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-slot");
    seed_auto_queue_card(
        &db,
        "card-cancel-dispatched",
        4596,
        "in_progress",
        "agent-cancel-slot",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-pending",
        4597,
        "ready",
        "agent-cancel-slot",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cancel-slot', 'test-repo', 'agent-cancel-slot', 'paused', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-cancel-slot', 0, 'run-cancel-slot', 0, ?1
            )",
            [json!({"111": "222000000000004597"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-slot', 'card-cancel-dispatched', 'agent-cancel-slot',
                'implementation', 'dispatched', 'Cancel slot dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cancel-dispatched', 'run-cancel-slot', 'card-cancel-dispatched',
                'agent-cancel-slot', 'dispatched', 'dispatch-cancel-slot', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-pending', 'run-cancel-slot', 'card-cancel-pending',
                'agent-cancel-slot', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-cancel-slot', 'agent-cancel-slot', 'claude', 'turn_active',
                'cancel slot seed', 23, 'dispatch-cancel-slot', '222000000000004597', 'claude-cancel-slot',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 2);
    assert_eq!(json["cancelled_dispatches"], 1);
    assert_eq!(json["deleted_phase_gates"], 0);
    assert_eq!(json["remaining_live_dispatches"], 0);
    assert_eq!(json["released_slots"], 1);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-cancel-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "cancelled");

    let entries: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT status, dispatch_id, completed_at
                 FROM auto_queue_entries
                 WHERE run_id = 'run-cancel-slot'
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|(status, dispatch_id, completed_at)| status == "skipped"
                && dispatch_id.is_none()
                && completed_at.is_some()),
        "cancel must skip every active/pending queue entry and stamp completed_at"
    );

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-cancel-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-cancel-slot' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (None, None));

    let session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-cancel-slot'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(session.0, "idle");
    assert_eq!(session.1, None);
    assert_eq!(session.2, 0);
    assert_eq!(session.3, None);
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_includes_restoring_runs() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-restoring");
    seed_auto_queue_card(
        &db,
        "card-cancel-restoring-pending",
        4598,
        "ready",
        "agent-cancel-restoring",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-restoring-skipped",
        4599,
        "ready",
        "agent-cancel-restoring",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-cancel-restoring', 'test-repo', 'agent-cancel-restoring', 'restoring', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-restoring-pending', 'run-cancel-restoring', 'card-cancel-restoring-pending',
                'agent-cancel-restoring', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-restoring-skipped', 'run-cancel-restoring', 'card-cancel-restoring-skipped',
                'agent-cancel-restoring', 'skipped', 1, 1
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-cancel-restoring'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "cancelled");

    let entry_states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status
                 FROM auto_queue_entries
                 WHERE run_id = 'run-cancel-restoring'
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        entry_states,
        vec![
            (
                "entry-cancel-restoring-pending".to_string(),
                "skipped".to_string(),
            ),
            (
                "entry-cancel-restoring-skipped".to_string(),
                "skipped".to_string(),
            ),
        ]
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_targets_only_requested_run() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-target");
    seed_auto_queue_card(
        &db,
        "card-cancel-target-a",
        4601,
        "ready",
        "agent-cancel-target",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-target-b",
        4602,
        "ready",
        "agent-cancel-target",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-cancel-target-a', 'test-repo', 'agent-cancel-target', 'active', datetime('now', '-1 minute')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-cancel-target-b', 'test-repo', 'agent-cancel-target', 'paused', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-target-a', 'run-cancel-target-a', 'card-cancel-target-a',
                'agent-cancel-target', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-target-b', 'run-cancel-target-b', 'card-cancel-target-b',
                'agent-cancel-target', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel?run_id=run-cancel-target-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let conn = db.lock().unwrap();
    let run_states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status
                 FROM auto_queue_runs
                 WHERE id IN ('run-cancel-target-a', 'run-cancel-target-b')
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        run_states,
        vec![
            ("run-cancel-target-a".to_string(), "active".to_string()),
            ("run-cancel-target-b".to_string(), "cancelled".to_string()),
        ]
    );

    let entry_states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status
                 FROM auto_queue_entries
                 WHERE id IN ('entry-cancel-target-a', 'entry-cancel-target-b')
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        entry_states,
        vec![
            ("entry-cancel-target-a".to_string(), "pending".to_string()),
            ("entry-cancel-target-b".to_string(), "skipped".to_string()),
        ]
    );
}

#[tokio::test]
async fn auto_queue_cancel_pg_targets_only_requested_run() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ($1, $2, $3)",
    )
    .bind("agent-cancel-target-pg")
    .bind("Agent Cancel Target PG")
    .bind("claude")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-cancel-target-a-pg")
    .bind("test-repo")
    .bind("Cancel target A PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-target-pg")
    .bind(5601_i64)
    .bind("card-cancel-target-b-pg")
    .bind("test-repo")
    .bind("Cancel target B PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-target-pg")
    .bind(5602_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, created_at
         ) VALUES
         ($1, $2, $3, $4, NOW() - INTERVAL '1 minute'),
         ($5, $6, $7, $8, NOW())",
    )
    .bind("run-cancel-target-a-pg")
    .bind("test-repo")
    .bind("agent-cancel-target-pg")
    .bind("active")
    .bind("run-cancel-target-b-pg")
    .bind("test-repo")
    .bind("agent-cancel-target-pg")
    .bind("paused")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("entry-cancel-target-a-pg")
    .bind("run-cancel-target-a-pg")
    .bind("card-cancel-target-a-pg")
    .bind("agent-cancel-target-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .bind("entry-cancel-target-b-pg")
    .bind("run-cancel-target-b-pg")
    .bind("card-cancel-target-b-pg")
    .bind("agent-cancel-target-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel?run_id=run-cancel-target-b-pg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "auto_queue_cancel_pg_targets_only_requested_run status={} body={}",
        status,
        body_text
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let run_states: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM auto_queue_runs
         WHERE id IN ($1, $2)
         ORDER BY id ASC",
    )
    .bind("run-cancel-target-a-pg")
    .bind("run-cancel-target-b-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        run_states,
        vec![
            ("run-cancel-target-a-pg".to_string(), "active".to_string()),
            (
                "run-cancel-target-b-pg".to_string(),
                "cancelled".to_string()
            ),
        ]
    );

    let entry_states: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM auto_queue_entries
         WHERE id IN ($1, $2)
         ORDER BY id ASC",
    )
    .bind("entry-cancel-target-a-pg")
    .bind("entry-cancel-target-b-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        entry_states,
        vec![
            (
                "entry-cancel-target-a-pg".to_string(),
                "pending".to_string()
            ),
            (
                "entry-cancel-target-b-pg".to_string(),
                "skipped".to_string()
            ),
        ]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_cancel_pg_cancels_live_dispatches_skips_entries_and_releases_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-cancel-slot-pg")
    .bind("Agent Cancel Slot PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-cancel-dispatched-pg")
    .bind("test-repo")
    .bind("Cancel dispatched PG")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-cancel-slot-pg")
    .bind(6596_i64)
    .bind("card-cancel-pending-pg")
    .bind("test-repo")
    .bind("Cancel pending PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-slot-pg")
    .bind(6597_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("run-cancel-slot-pg")
    .bind("test-repo")
    .bind("agent-cancel-slot-pg")
    .bind("paused")
    .bind(1_i64)
    .bind(2_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
         ) VALUES (
            $1, $2, $3, $4, $5::jsonb
         )",
    )
    .bind("agent-cancel-slot-pg")
    .bind(0_i64)
    .bind("run-cancel-slot-pg")
    .bind(0_i64)
    .bind(json!({"111": "222000000000065001"}).to_string())
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("dispatch-cancel-slot-pg")
    .bind("card-cancel-dispatched-pg")
    .bind("agent-cancel-slot-pg")
    .bind("implementation")
    .bind("dispatched")
    .bind("Cancel slot dispatch PG")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, NOW()
         ), (
            $10, $11, $12, $13, $14, NULL, NULL, $15, $16, NULL
         )",
    )
    .bind("entry-cancel-dispatched-pg")
    .bind("run-cancel-slot-pg")
    .bind("card-cancel-dispatched-pg")
    .bind("agent-cancel-slot-pg")
    .bind("dispatched")
    .bind("dispatch-cancel-slot-pg")
    .bind(0_i64)
    .bind(0_i64)
    .bind(0_i64)
    .bind("entry-cancel-pending-pg")
    .bind("run-cancel-slot-pg")
    .bind("card-cancel-pending-pg")
    .bind("agent-cancel-slot-pg")
    .bind("pending")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, session_info, tokens,
            active_dispatch_id, thread_channel_id, claude_session_id
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9
         )",
    )
    .bind("host:AgentDesk-claude-cancel-slot-pg")
    .bind("agent-cancel-slot-pg")
    .bind("claude")
    .bind("turn_active")
    .bind("cancel slot seed pg")
    .bind(23_i64)
    .bind("dispatch-cancel-slot-pg")
    .bind("222000000000065001")
    .bind("claude-cancel-slot-pg")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "auto_queue_cancel_pg_cancels_live_dispatches_skips_entries_and_releases_slots status={} body={}",
        status,
        body_text
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 2);
    assert_eq!(json["cancelled_dispatches"], 1);
    assert_eq!(json["deleted_phase_gates"], 0);
    assert_eq!(json["remaining_live_dispatches"], 0);
    assert_eq!(json["released_slots"], 1);
    assert_eq!(json["cleared_slot_sessions"], 1);

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-cancel-slot-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "cancelled");

    let entries: Vec<(
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT status, dispatch_id, completed_at
             FROM auto_queue_entries
             WHERE run_id = $1
             ORDER BY id ASC",
    )
    .bind("run-cancel-slot-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|(status, dispatch_id, completed_at)| status == "skipped"
                && dispatch_id.is_none()
                && completed_at.is_some()),
        "cancel must skip every active/pending PG queue entry and stamp completed_at"
    );

    let dispatch_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-cancel-slot-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let slot: (Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT assigned_run_id, assigned_thread_group
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind("agent-cancel-slot-pg")
    .bind(0_i64)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(slot, (None, None));

    let session: (String, Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, tokens::BIGINT, claude_session_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind("host:AgentDesk-claude-cancel-slot-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(session.0, "idle");
    assert_eq!(session.1, None);
    assert_eq!(session.2, 0);
    assert_eq!(session.3, None);

    let sqlite_run_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_runs WHERE id = 'run-cancel-slot-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_run_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_cancel_pg_includes_restoring_runs() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ($1, $2, $3)",
    )
    .bind("agent-cancel-restoring-pg")
    .bind("Agent Cancel Restoring PG")
    .bind("claude")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-cancel-restoring-pending-pg")
    .bind("test-repo")
    .bind("Cancel restoring pending PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-restoring-pg")
    .bind(6598_i64)
    .bind("card-cancel-restoring-skipped-pg")
    .bind("test-repo")
    .bind("Cancel restoring skipped PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-restoring-pg")
    .bind(6599_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-cancel-restoring-pg")
    .bind("test-repo")
    .bind("agent-cancel-restoring-pg")
    .bind("restoring")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("entry-cancel-restoring-pending-pg")
    .bind("run-cancel-restoring-pg")
    .bind("card-cancel-restoring-pending-pg")
    .bind("agent-cancel-restoring-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .bind("entry-cancel-restoring-skipped-pg")
    .bind("run-cancel-restoring-pg")
    .bind("card-cancel-restoring-skipped-pg")
    .bind("agent-cancel-restoring-pg")
    .bind("skipped")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "auto_queue_cancel_pg_includes_restoring_runs status={} body={}",
        status,
        body_text
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-cancel-restoring-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "cancelled");

    let entry_states: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY id ASC",
    )
    .bind("run-cancel-restoring-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        entry_states,
        vec![
            (
                "entry-cancel-restoring-pending-pg".to_string(),
                "skipped".to_string(),
            ),
            (
                "entry-cancel-restoring-skipped-pg".to_string(),
                "skipped".to_string(),
            ),
        ]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_cancel_pg_sweeps_user_cancelled_entries() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ($1, $2, $3)",
    )
    .bind("agent-cancel-user-cancelled-pg")
    .bind("Agent Cancel User Cancelled PG")
    .bind("claude")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind("card-cancel-user-cancelled-pg")
    .bind("test-repo")
    .bind("Cancel user_cancelled PG")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-cancel-user-cancelled-pg")
    .bind(6600_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind("run-cancel-user-cancelled-pg")
    .bind("test-repo")
    .bind("agent-cancel-user-cancelled-pg")
    .bind("paused")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())",
    )
    .bind("entry-cancel-user-cancelled-pg")
    .bind("run-cancel-user-cancelled-pg")
    .bind("card-cancel-user-cancelled-pg")
    .bind("agent-cancel-user-cancelled-pg")
    .bind("user_cancelled")
    .bind(0_i64)
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel?run_id=run-cancel-user-cancelled-pg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "auto_queue_cancel_pg_sweeps_user_cancelled_entries status={} body={}",
        status,
        body_text
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-cancel-user-cancelled-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "cancelled");

    let entry_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
            .bind("entry-cancel-user-cancelled-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(
        entry_status, "skipped",
        "PG run cancel must sweep user_cancelled entries into skipped so restore semantics stay consistent"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_surfaces_warning_when_slot_release_fails() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-warn");
    seed_auto_queue_card(
        &db,
        "card-cancel-warn-dispatched",
        4603,
        "in_progress",
        "agent-cancel-warn",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-warn-pending",
        4604,
        "ready",
        "agent-cancel-warn",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cancel-warn', 'test-repo', 'agent-cancel-warn', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-cancel-warn', 0, 'run-cancel-warn', 0, ?1
            )",
            [json!({"111": "222000000000004603"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-warn', 'card-cancel-warn-dispatched', 'agent-cancel-warn',
                'implementation', 'dispatched', 'Cancel warning dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cancel-warn-dispatched', 'run-cancel-warn', 'card-cancel-warn-dispatched',
                'agent-cancel-warn', 'dispatched', 'dispatch-cancel-warn', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-warn-pending', 'run-cancel-warn', 'card-cancel-warn-pending',
                'agent-cancel-warn', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_cancel_initial_slot_release
             BEFORE UPDATE OF assigned_run_id ON auto_queue_slots
             WHEN OLD.assigned_run_id = 'run-cancel-warn'
               AND NEW.assigned_run_id IS NULL
               AND (
                   SELECT COUNT(*)
                   FROM auto_queue_entries
                   WHERE run_id = 'run-cancel-warn'
                     AND status IN ('pending', 'dispatched')
               ) > 1
             BEGIN
                 SELECT RAISE(ABORT, 'cancel slot release blocked');
             END;",
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["released_slots"], 0);
    assert!(
        json["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("failed to release slots for run run-cancel-warn"),
        "cancel response must surface slot release failures"
    );

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-cancel-warn'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "cancelled");
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_also_cancels_phase_gate_dispatches_and_deletes_gate_rows() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-phase-gate");
    seed_auto_queue_card(
        &db,
        "card-cancel-phase-gate-live",
        4606,
        "in_progress",
        "agent-cancel-phase-gate",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-phase-gate-pending",
        4607,
        "ready",
        "agent-cancel-phase-gate",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-phase-gate-anchor",
        4608,
        "reviewing",
        "agent-cancel-phase-gate",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cancel-phase-gate', 'test-repo', 'agent-cancel-phase-gate', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-cancel-phase-gate', 0, 'run-cancel-phase-gate', 0, ?1
            )",
            [json!({"111": "222000000000004608"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-phase-live', 'card-cancel-phase-gate-live', 'agent-cancel-phase-gate',
                'implementation', 'dispatched', 'Cancel run dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-phase-gate', 'card-cancel-phase-gate-anchor', 'agent-cancel-phase-gate',
                'review', 'dispatched', 'Cancel phase gate', ?1, datetime('now'), datetime('now')
            )",
            [json!({
                "slot_index": 0,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-cancel-phase-gate",
                    "batch_phase": 1,
                    "next_phase": 2,
                    "anchor_card_id": "card-cancel-phase-gate-anchor"
                }
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cancel-phase-live', 'run-cancel-phase-gate', 'card-cancel-phase-gate-live',
                'agent-cancel-phase-gate', 'dispatched', 'dispatch-cancel-phase-live', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-phase-pending', 'run-cancel-phase-gate', 'card-cancel-phase-gate-pending',
                'agent-cancel-phase-gate', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict, next_phase, final_phase, anchor_card_id, created_at, updated_at
            ) VALUES (
                'run-cancel-phase-gate', 1, 'pending', 'dispatch-cancel-phase-gate',
                'phase_gate_passed', 2, 0, 'card-cancel-phase-gate-anchor', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 2);
    assert_eq!(json["cancelled_dispatches"], 2);
    assert_eq!(json["deleted_phase_gates"], 1);
    assert_eq!(json["remaining_live_dispatches"], 0);
    assert_eq!(json["released_slots"], 1);

    let conn = db.lock().unwrap();
    let statuses: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, status
             FROM task_dispatches
             WHERE id IN ('dispatch-cancel-phase-live', 'dispatch-cancel-phase-gate')
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        statuses,
        vec![
            (
                "dispatch-cancel-phase-gate".to_string(),
                "cancelled".to_string(),
            ),
            (
                "dispatch-cancel-phase-live".to_string(),
                "cancelled".to_string(),
            ),
        ]
    );

    let phase_gate_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = 'run-cancel-phase-gate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase_gate_rows, 0);

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-cancel-phase-gate' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (None, None));
    let active_slot_dispatches: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0
             FROM task_dispatches
             WHERE to_agent_id = 'agent-cancel-phase-gate'
               AND status IN ('pending', 'dispatched')
               AND CAST(json_extract(COALESCE(context, '{}'), '$.slot_index') AS INTEGER) = 0
               AND COALESCE(CAST(json_extract(COALESCE(context, '{}'), '$.sidecar_dispatch') AS INTEGER), 0) = 0
               AND json_type(COALESCE(context, '{}'), '$.phase_gate') IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !active_slot_dispatches,
        "cancelled phase-gate dispatches must not keep the slot blocked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activate_run_id_blocks_phase_gate_paused_runs_pg_path() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-phase-gate-pg")
    .bind("Agent Phase Gate PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("card-phase-gate-paused-pg")
    .bind("test-repo")
    .bind("Phase gate paused PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-phase-gate-pg")
    .bind(64381_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-phase-gate-paused-pg")
    .bind("test-repo")
    .bind("agent-phase-gate-pg")
    .bind("paused")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("entry-phase-gate-paused-pg")
    .bind("run-phase-gate-paused-pg")
    .bind("card-phase-gate-paused-pg")
    .bind("agent-phase-gate-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(2_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (
            run_id, phase, status, dispatch_id, pass_verdict
         ) VALUES (
            $1, $2, $3, NULL, $4
         )",
    )
    .bind("run-phase-gate-paused-pg")
    .bind(1_i64)
    .bind("pending")
    .bind("phase_gate_passed")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-phase-gate-paused-pg",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 0);
    assert_eq!(json["message"], "Run is waiting on phase gate");

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-phase-gate-paused-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "paused");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-activate-pg-only")
    .bind("Agent Activate PG Only")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("card-activate-pg-only")
    .bind("test-repo")
    .bind("Activate PG Only")
    .bind("ready")
    .bind("medium")
    .bind("agent-activate-pg-only")
    .bind(64384_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("run-activate-pg-only")
    .bind("test-repo")
    .bind("agent-activate-pg-only")
    .bind("active")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8
         )",
    )
    .bind("entry-activate-pg-only")
    .bind("run-activate-pg-only")
    .bind("card-activate-pg-only")
    .bind("agent-activate-pg-only")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-activate-pg-only",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"],
        serde_json::json!(1),
        "auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror body={json}"
    );

    let sqlite_run_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_runs WHERE id = 'run-activate-pg-only'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_run_count, 0,
        "test must not rely on SQLite mirror state"
    );

    let entry_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
            .bind("entry-activate-pg-only")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(entry_status, "dispatched");

    let dispatch_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'implementation'",
    )
    .bind("card-activate-pg-only")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(dispatch_count, 1);

    let latest_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT latest_dispatch_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-activate-pg-only")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert!(
        latest_dispatch_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_run_skips_phase_gate_blocked_runs_pg_path() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES
         ($1, $2, $3, $4, $5),
         ($6, $7, $8, $9, $10)",
    )
    .bind("agent-resume-gate-pg")
    .bind("Agent Resume Gate PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .bind("agent-resume-free-pg")
    .bind("Agent Resume Free PG")
    .bind("claude")
    .bind("333")
    .bind("444")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-resume-gate-pg")
    .bind("test-repo")
    .bind("Resume gate PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-resume-gate-pg")
    .bind(64382_i64)
    .bind("card-resume-free-pg")
    .bind("test-repo")
    .bind("Resume free PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-resume-free-pg")
    .bind(64383_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES
         ($1, $2, $3, $4),
         ($5, $6, $7, $8)",
    )
    .bind("run-resume-gate-pg")
    .bind("test-repo")
    .bind("agent-resume-gate-pg")
    .bind("paused")
    .bind("run-resume-free-pg")
    .bind("test-repo")
    .bind("agent-resume-free-pg")
    .bind("paused")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("entry-resume-gate-pg")
    .bind("run-resume-gate-pg")
    .bind("card-resume-gate-pg")
    .bind("agent-resume-gate-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(2_i64)
    .bind("entry-resume-free-pg")
    .bind("run-resume-free-pg")
    .bind("card-resume-free-pg")
    .bind("agent-resume-free-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (
            run_id, phase, status, dispatch_id, pass_verdict
         ) VALUES (
            $1, $2, $3, NULL, $4
         )",
    )
    .bind("run-resume-gate-pg")
    .bind(1_i64)
    .bind("failed")
    .bind("phase_gate_passed")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/resume")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["resumed_runs"], 1);
    assert_eq!(json["blocked_runs"], 1);

    let blocked_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-resume-gate-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    let resumed_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-resume-free-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(blocked_status, "paused");
    assert_eq!(resumed_status, "active");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_phase_gate_reevaluates_failed_terminal_dispatch_pg_path() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-repair-gate-pg")
    .bind("Agent Repair Gate PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("card-repair-gate-pg")
    .bind("test-repo")
    .bind("Repair gate PG")
    .bind("review")
    .bind("medium")
    .bind("agent-repair-gate-pg")
    .bind(64385_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-repair-gate-pg")
    .bind("test-repo")
    .bind("agent-repair-gate-pg")
    .bind("paused")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("entry-repair-gate-pg")
    .bind("run-repair-gate-pg")
    .bind("card-repair-gate-pg")
    .bind("agent-repair-gate-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(2_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result
         ) VALUES (
            $1, $2, $3, $4, $5, $6, CAST($7 AS jsonb), CAST($8 AS jsonb)
         )",
    )
    .bind("dispatch-repair-gate-pg")
    .bind("card-repair-gate-pg")
    .bind("agent-repair-gate-pg")
    .bind("phase-gate")
    .bind("completed")
    .bind("Repair phase gate PG")
    .bind(
        json!({
            "phase_gate": {
                "run_id": "run-repair-gate-pg",
                "batch_phase": 1,
                "pass_verdict": "phase_gate_passed",
                "next_phase": 2,
                "final_phase": false
            }
        })
        .to_string(),
    )
    .bind(json!({"verdict": "phase_gate_passed"}).to_string())
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (
            run_id, phase, status, verdict, dispatch_id, pass_verdict, failure_reason
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("run-repair-gate-pg")
    .bind(1_i64)
    .bind("failed")
    .bind("phase_gate_failed")
    .bind("dispatch-repair-gate-pg")
    .bind("phase_gate_passed")
    .bind("operator patched result after failure")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/runs/run-repair-gate-pg/phase-gates/repair")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"phase":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["candidate_dispatches"], 1);
    assert_eq!(json["cleared_gates"], 1);
    assert_eq!(json["blocking_gates_remaining"], 0);
    assert_eq!(json["run_status"], "active");
    assert_eq!(json["outcomes"][0]["outcome"], "cleared");
    assert_eq!(json["outcomes"][0]["run_resumed"], true);

    let remaining_gates = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_phase_gates
         WHERE run_id = $1",
    )
    .bind("run-repair-gate-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(remaining_gates, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

/// Regression test for #191: onTick1min recovery must reset stuck auto-queue
/// entries that are 'dispatched' but have orphan (NULL), phantom (missing row),
/// or cancelled/failed dispatch_ids — while leaving valid dispatches untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_ignores_legacy_max_concurrent_per_agent() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();

    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-393");
    seed_auto_queue_card(&db, "card-393-1", 3931, "ready", "agent-393");
    seed_auto_queue_card(&db, "card-393-2", 3932, "ready", "agent-393");

    let app = test_api_router(db.clone(), engine.clone(), None);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-393",
                        "parallel": true,
                        "max_concurrent_threads": 2,
                        "max_concurrent_per_agent": 1,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let generated_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let run = &generated_json["run"];
    assert_eq!(run["max_concurrent_threads"], 2);
    assert!(run.get("max_concurrent_per_agent").is_none());

    {
        let conn = db.lock().unwrap();
        let run_id = run["id"].as_str().unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET thread_group = CASE id
                 WHEN ?1 THEN 0
                 WHEN ?2 THEN 1
                 ELSE thread_group
             END
             WHERE run_id = ?3",
            sqlite_params![
                generated_json["entries"][0]["id"].as_str().unwrap(),
                generated_json["entries"][1]["id"].as_str().unwrap(),
                run_id
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_runs SET thread_group_count = 2 WHERE id = ?1",
            [run_id],
        )
        .unwrap();
    }

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-393",
                        "unified_thread": false,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let activate_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(activate_json["count"], 2);
    assert_eq!(activate_json["active_groups"], 2);
}

#[tokio::test]
async fn auto_queue_recovery_resets_orphan_phantom_and_cancelled_entries_pg() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-recovery").await;
    seed_auto_queue_card_pg(&pool, "card-orphan", 9001, "in_progress", "agent-recovery").await;
    seed_auto_queue_card_pg(&pool, "card-phantom", 9002, "in_progress", "agent-recovery").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-cancelled",
        9003,
        "in_progress",
        "agent-recovery",
    )
    .await;
    seed_auto_queue_card_pg(&pool, "card-valid", 9004, "in_progress", "agent-recovery").await;

    // Active run
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-recovery', 'test-repo', 'agent-recovery', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Entry A: dispatched + dispatch_id=NULL (orphan — should be reset)
    // #214: dispatched_at must be >2min ago to pass grace period
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES ('entry-orphan', 'run-recovery', 'card-orphan', 'agent-recovery', 'dispatched', NULL, NOW() - INTERVAL '3 minutes')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Entry B: dispatched + phantom dispatch_id (not in task_dispatches — should be reset)
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES ('entry-phantom', 'run-recovery', 'card-phantom', 'agent-recovery', 'dispatched', 'phantom-id-999', NOW() - INTERVAL '3 minutes')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Entry C: dispatched + cancelled dispatch (should be reset)
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
         VALUES ('dispatch-cancelled', 'card-cancelled', 'agent-recovery', 'implementation', 'cancelled', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES ('entry-cancelled', 'run-recovery', 'card-cancelled', 'agent-recovery', 'dispatched', 'dispatch-cancelled', NOW() - INTERVAL '3 minutes')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Entry D: dispatched + valid active dispatch (must NOT be reset)
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
         VALUES ('dispatch-valid', 'card-valid', 'agent-recovery', 'implementation', 'dispatched', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES ('entry-valid', 'run-recovery', 'card-valid', 'agent-recovery', 'dispatched', 'dispatch-valid', NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Fire onTick1min — triggers recovery path 2
    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    // A: orphan (NULL dispatch_id) → reset to pending
    let (status_a, did_a): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-orphan'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status_a, "pending", "orphan entry must be reset to pending");
    assert!(did_a.is_none(), "orphan entry dispatch_id must stay NULL");

    // B: phantom dispatch_id → reset to pending
    let (status_b, did_b): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-phantom'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status_b, "pending",
        "phantom dispatch entry must be reset to pending"
    );
    assert!(
        did_b.is_none(),
        "phantom entry dispatch_id must be cleared to NULL"
    );

    // C: cancelled dispatch → reset to pending
    let (status_c, did_c): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-cancelled'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status_c, "pending",
        "cancelled dispatch entry must be reset to pending"
    );
    assert!(
        did_c.is_none(),
        "cancelled entry dispatch_id must be cleared to NULL"
    );

    // D: valid active dispatch → must remain dispatched
    let (status_d, did_d): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-valid'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status_d, "dispatched",
        "valid dispatch entry must NOT be reset"
    );
    assert_eq!(
        did_d.as_deref(),
        Some("dispatch-valid"),
        "valid entry dispatch_id must be preserved"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn orphan_recovery_rollback_terminalizes_auto_queue_entry_pg() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-orphan-aq").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-orphan-aq",
        9101,
        "in_progress",
        "agent-orphan-aq",
    )
    .await;
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
        ) VALUES (
            'dispatch-orphan-aq', 'card-orphan-aq', 'agent-orphan-aq',
            'implementation', 'pending', 'orphan aq',
            NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes'
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE kanban_cards
         SET latest_dispatch_id = 'dispatch-orphan-aq'
         WHERE id = 'card-orphan-aq'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-orphan-aq', 'test-repo', 'agent-orphan-aq', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, dispatched_at
        ) VALUES (
            'entry-orphan-aq', 'run-orphan-aq', 'card-orphan-aq', 'agent-orphan-aq',
            'dispatched', 'dispatch-orphan-aq', 0, NOW() - INTERVAL '10 minutes'
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    let supervisor = crate::supervisor::RuntimeSupervisor::new(Some(pool.clone()), engine);
    assert_eq!(
        supervisor
            .mark_dispatch_failed_for_test("dispatch-orphan-aq")
            .unwrap(),
        1
    );
    let (dispatch_status, entry_status, entry_dispatch_id): (String, String, Option<String>) =
        sqlx::query_as(
            "SELECT td.status, aq.status, aq.dispatch_id
         FROM task_dispatches td
         JOIN auto_queue_entries aq ON aq.dispatch_id = td.id
         WHERE td.id = 'dispatch-orphan-aq'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dispatch_status, "failed");
    assert_eq!(
        entry_status, "failed",
        "orphan rollback must not leave the auto-queue entry dispatched"
    );
    assert_eq!(entry_dispatch_id.as_deref(), Some("dispatch-orphan-aq"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_recovery_honors_stale_dispatch_runtime_config_pg() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-recovery-config").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-expired-old",
        9011,
        "in_progress",
        "agent-recovery-config",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-expired-recent",
        9012,
        "in_progress",
        "agent-recovery-config",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-orphan-config",
        9013,
        "in_progress",
        "agent-recovery-config",
    )
    .await;

    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES
            ('staleDispatchedGraceMin', '5'),
            ('staleDispatchedTerminalStatuses', 'expired'),
            ('staleDispatchedRecoverNullDispatch', 'false'),
            ('staleDispatchedRecoverMissingDispatch', 'false')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-recovery-config', 'test-repo', 'agent-recovery-config', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
         VALUES ('dispatch-expired-old', 'card-expired-old', 'agent-recovery-config', 'implementation', 'expired', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
         VALUES ('dispatch-expired-recent', 'card-expired-recent', 'agent-recovery-config', 'implementation', 'expired', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES
         ('entry-expired-old', 'run-recovery-config', 'card-expired-old', 'agent-recovery-config', 'dispatched', 'dispatch-expired-old', NOW() - INTERVAL '6 minutes'),
         ('entry-expired-recent', 'run-recovery-config', 'card-expired-recent', 'agent-recovery-config', 'dispatched', 'dispatch-expired-recent', NOW() - INTERVAL '4 minutes'),
         ('entry-orphan-config', 'run-recovery-config', 'card-orphan-config', 'agent-recovery-config', 'dispatched', NULL, NOW() - INTERVAL '6 minutes')",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let expired_old: (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-expired-old'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(expired_old.0, "pending");
    assert!(expired_old.1.is_none());

    let expired_recent: (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-expired-recent'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(expired_recent.0, "dispatched");
    assert_eq!(expired_recent.1.as_deref(), Some("dispatch-expired-recent"));

    let orphan_config: (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-orphan-config'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphan_config.0, "dispatched");
    assert!(orphan_config.1.is_none());

    pool.close().await;
    pg_db.drop().await;
}

/// Regression test for #295: onTick1min must backstop terminal cards that still
/// have pending auto-queue entries in active/paused runs.
#[tokio::test]
async fn auto_queue_recovery_skips_terminal_pending_entries_pg() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-terminal-recovery").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-terminal-active",
        9011,
        "done",
        "agent-terminal-recovery",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-terminal-paused",
        9012,
        "done",
        "agent-terminal-recovery",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-terminal-generated",
        9013,
        "done",
        "agent-terminal-recovery",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-nonterminal-active",
        9014,
        "requested",
        "agent-terminal-recovery",
    )
    .await;

    for (run_id, status) in [
        ("run-terminal-active", "active"),
        ("run-terminal-paused", "paused"),
        ("run-terminal-generated", "generated"),
    ] {
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ($1, 'test-repo', 'agent-terminal-recovery', $2)",
        )
        .bind(run_id)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }

    for (entry_id, run_id, card_id) in [
        (
            "entry-terminal-active",
            "run-terminal-active",
            "card-terminal-active",
        ),
        (
            "entry-terminal-paused",
            "run-terminal-paused",
            "card-terminal-paused",
        ),
        (
            "entry-terminal-generated",
            "run-terminal-generated",
            "card-terminal-generated",
        ),
        (
            "entry-nonterminal-active",
            "run-terminal-active",
            "card-nonterminal-active",
        ),
    ] {
        sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status) \
             VALUES ($1, $2, $3, 'agent-terminal-recovery', 'pending')",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, status FROM auto_queue_entries ORDER BY id ASC")
            .fetch_all(&pool)
            .await
            .unwrap();
    let statuses: std::collections::HashMap<String, String> = rows.into_iter().collect();

    assert_eq!(
        statuses.get("entry-terminal-active").map(String::as_str),
        Some("skipped")
    );
    assert_eq!(
        statuses.get("entry-terminal-paused").map(String::as_str),
        Some("skipped")
    );
    assert_eq!(
        statuses.get("entry-terminal-generated").map(String::as_str),
        Some("pending"),
        "generated runs are not part of #295 terminal cleanup scope"
    );
    assert_ne!(
        statuses.get("entry-nonterminal-active").map(String::as_str),
        Some("skipped"),
        "non-terminal pending work must not be swept by #295 terminal cleanup"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_recovery_completes_finished_non_phase_gate_runs_and_releases_slots_pg() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-finished-recovery").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-finished-done",
        9015,
        "done",
        "agent-finished-recovery",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-finished-skipped",
        9016,
        "done",
        "agent-finished-recovery",
    )
    .await;

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-finished-recovery', 'test-repo', 'agent-finished-recovery', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
        ) VALUES (
            'entry-finished-done', 'run-finished-recovery', 'card-finished-done',
            'agent-finished-recovery', 'done', 0, 0, NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
        ) VALUES (
            'entry-finished-skipped', 'run-finished-recovery', 'card-finished-skipped',
            'agent-finished-recovery', 'skipped', 1, 1, NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
        ) VALUES (
            'agent-finished-recovery', 0, 'run-finished-recovery', 0, '{}'::jsonb
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
        ) VALUES (
            'agent-finished-recovery', 1, 'run-finished-recovery', 1, '{}'::jsonb
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let run_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-finished-recovery'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        run_status, "completed",
        "finished non-phase-gate run must be completed by onTick1min backstop"
    );

    for slot_index in [0_i32, 1_i32] {
        let slot: (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-finished-recovery' AND slot_index = $1",
        )
        .bind(slot_index)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            slot,
            (None, None),
            "completed run must release slot {slot_index}"
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

#[test]
fn auto_queue_recovery_keeps_user_cancelled_runs_active() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    seed_agent(&db, "agent-user-cancelled-recovery");
    seed_auto_queue_card(
        &db,
        "card-user-cancelled-recovery",
        9017,
        "in_progress",
        "agent-user-cancelled-recovery",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-user-cancelled-recovery', 'test-repo', 'agent-user-cancelled-recovery', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
            ) VALUES (
                'entry-user-cancelled-recovery', 'run-user-cancelled-recovery',
                'card-user-cancelled-recovery', 'agent-user-cancelled-recovery',
                'user_cancelled', 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-user-cancelled-recovery'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        run_status, "active",
        "user_cancelled entries must block onTick1min from auto-completing the run"
    );
}

#[test]
fn auto_queue_recovery_keeps_finished_phase_gate_runs_blocked_until_gate_resolves() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    seed_agent(&db, "agent-finished-gate");
    seed_auto_queue_card(
        &db,
        "card-finished-gate",
        9017,
        "done",
        "agent-finished-gate",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-finished-gate', 'test-repo', 'agent-finished-gate', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase, completed_at
            ) VALUES (
                'entry-finished-gate', 'run-finished-gate', 'card-finished-gate',
                'agent-finished-gate', 'done', 0, 0, 1, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-finished-gate', 0, 'run-finished-gate', 0, '{}'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES (?1, ?2, ?3, NULL, 'phase_gate_passed')",
            sqlite_params!["run-finished-gate", 1, "pending",],
        )
        .unwrap();
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-finished-gate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        run_status, "paused",
        "finished phase-gate run must stay paused until the gate resolves"
    );

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-finished-gate' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        slot,
        (Some("run-finished-gate".to_string()), Some(0)),
        "phase-gate blocked run must retain its slot assignment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_reset_completes_generated_and_pending_runs() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-reset-generated", 1711, "ready", "agent-reset");
    seed_auto_queue_card(&db, "card-reset-pending", 1712, "ready", "agent-reset");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-generated', 'test-repo', 'agent-reset', 'generated', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-pending', 'test-repo', 'agent-reset', 'pending', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-generated', 'run-reset-generated', 'card-reset-generated', 'agent-reset', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-pending', 'run-reset-pending', 'card-reset-pending', 'agent-reset', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .header("content-type", "application/json")
                .uri("/queue/reset")
                .body(Body::from(r#"{"agent_id":"agent-reset"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deleted_entries"], 2);
    assert_eq!(json["completed_runs"], 2);

    let status_response = app
        .oneshot(
            Request::builder()
                .uri("/queue/status?agent_id=agent-reset")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    assert_eq!(status_json["run"]["id"], "run-reset-pending");
    assert_eq!(status_json["run"]["status"], "completed");
    assert_eq!(
        status_json["entries"]
            .as_array()
            .map(|entries| entries.len()),
        Some(0)
    );

    let conn = db.lock().unwrap();
    let generated_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let pending_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(generated_run_status, "completed");
    assert_eq!(pending_run_status, "completed");
    assert_eq!(remaining_entries, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_reset_with_agent_id_only_clears_matching_agent_scope() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset-a");
    seed_agent(&db, "agent-reset-b");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-reset-a-generated",
        1713,
        "ready",
        "agent-reset-a",
    );
    seed_auto_queue_card(&db, "card-reset-a-active", 1714, "ready", "agent-reset-a");
    seed_auto_queue_card(&db, "card-reset-b-active", 1715, "ready", "agent-reset-b");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-a-generated', 'test-repo', 'agent-reset-a', 'generated', datetime('now', '-3 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-a-active', 'test-repo', 'agent-reset-a', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-b-active', 'test-repo', 'agent-reset-b', 'active', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-a-generated', 'run-reset-a-generated', 'card-reset-a-generated', 'agent-reset-a', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-a-active', 'run-reset-a-active', 'card-reset-a-active', 'agent-reset-a', 'dispatched', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-b-active', 'run-reset-b-active', 'card-reset-b-active', 'agent-reset-b', 'dispatched', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/reset")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"agent-reset-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deleted_entries"], 2);
    assert_eq!(json["completed_runs"], 2);
    assert_eq!(json["protected_active_runs"], 0);

    let conn = db.lock().unwrap();
    let run_a_generated: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-a-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let run_a_active: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-a-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let run_b_active: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-b-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_agent_b_entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE agent_id = 'agent-reset-b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);

    assert_eq!(run_a_generated, "completed");
    assert_eq!(run_a_active, "completed");
    assert_eq!(run_b_active, "active");
    assert_eq!(remaining_agent_b_entries, 1);
    assert_eq!(remaining_entries, 1);

    let status_response = app
        .oneshot(
            Request::builder()
                .uri("/queue/status?agent_id=agent-reset-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    assert_eq!(status_json["run"]["id"], "run-reset-b-active");
    assert_eq!(status_json["run"]["status"], "active");
    assert_eq!(
        status_json["entries"]
            .as_array()
            .map(|entries| entries.len()),
        Some(1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_reset_requires_agent_id_and_reset_global_requires_confirmation() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset-global-active");
    seed_agent(&db, "agent-reset-global-pending");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-reset-global-active",
        1716,
        "ready",
        "agent-reset-global-active",
    );
    seed_auto_queue_card(
        &db,
        "card-reset-global-pending",
        1717,
        "ready",
        "agent-reset-global-pending",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-global-active', 'test-repo', 'agent-reset-global-active', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-global-pending', 'test-repo', 'agent-reset-global-pending', 'pending', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-global-active', 'run-reset-global-active', 'card-reset-global-active', 'agent-reset-global-active', 'dispatched', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-global-pending', 'run-reset-global-pending', 'card-reset-global-pending', 'agent-reset-global-pending', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let rejection = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/reset")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);
    let rejection_body = axum::body::to_bytes(rejection.into_body(), usize::MAX)
        .await
        .unwrap();
    let rejection_json: serde_json::Value = serde_json::from_slice(&rejection_body).unwrap();
    assert_eq!(rejection_json["error"], "agent_id is required for reset");

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/reset-global")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"confirmation_token":"confirm-global-reset"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deleted_entries"], 1);
    assert_eq!(json["completed_runs"], 1);
    assert_eq!(json["protected_active_runs"], 1);
    assert_eq!(
        json["warning"],
        "global reset preserved 1 active run(s); use agent_id to reset a specific queue"
    );

    let conn = db.lock().unwrap();
    let active_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-global-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let pending_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-global-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let active_entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = 'run-reset-global-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);

    assert_eq!(active_run_status, "active");
    assert_eq!(pending_run_status, "completed");
    assert_eq!(active_entries, 1);
    assert_eq!(remaining_entries, 1);
}

#[tokio::test]
async fn phase_gate_catalog_endpoint_returns_kinds_and_default() {
    // #2125 — dashboard + agents both pull from this endpoint, so its shape is
    // a contract. Lock down the exact field set so silent changes break here
    // rather than at the dashboard runtime.
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db.clone(), engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/queue/phase-gates/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["default_kind"], "pr-confirm");

    let kinds = json["kinds"]
        .as_array()
        .expect("catalog response must include kinds array");
    assert!(
        kinds.len() >= 2,
        "catalog should start with at least pr-confirm and deploy-gate"
    );

    let ids: Vec<&str> = kinds
        .iter()
        .filter_map(|kind| kind["id"].as_str())
        .collect();
    assert!(ids.contains(&"pr-confirm"));
    assert!(ids.contains(&"deploy-gate"));

    let first = &kinds[0];
    assert!(first["label"]["ko"].is_string());
    assert!(first["label"]["en"].is_string());
    assert!(first["description"].is_string());
    assert!(first["checks"].is_array());
}

#[tokio::test]
async fn request_generate_rejects_empty_issue_numbers() {
    // #2126 — input validation must happen before reaching the Discord send
    // path so callers get a clean 400 even in standalone test mode.
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db.clone(), engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/request-generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "itismyfield/AgentDesk",
                        "agent_id": "project-agentdesk",
                        "issue_numbers": [],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("issue_numbers"),
        "error must point at the offending field, got: {json}"
    );
}

#[tokio::test]
async fn request_generate_rejects_unknown_allowed_gate_kind() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db.clone(), engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/request-generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "itismyfield/AgentDesk",
                        "agent_id": "project-agentdesk",
                        "issue_numbers": [42],
                        "allowed_gate_kinds": ["ship-it"],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("ship-it") && error.contains("phase_gate_kind"),
        "error must name the offending kind, got: {error}"
    );
}

#[tokio::test]
async fn request_generate_returns_503_without_discord() {
    // Without a HealthRegistry attached we expect a clean 503 explaining
    // Discord is unavailable, not a panic or a misleading 500 (#2126).
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db.clone(), engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/request-generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "itismyfield/AgentDesk",
                        "agent_id": "project-agentdesk",
                        "issue_numbers": [2120, 2121],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("discord")
    );
}

#[tokio::test]
async fn generate_rejects_unknown_phase_gate_kind() {
    // #2125 — entries with a phase_gate_kind not in the catalog must fail
    // with 400 so callers fix the value rather than silently fall back.
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db.clone(), engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "entries": [
                            {"issue_number": 4242, "phase_gate_kind": "ship-it"}
                        ],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let error = json["error"]
        .as_str()
        .expect("error message must be a string");
    assert!(
        error.contains("phase_gate_kind") && error.contains("ship-it"),
        "error must name the offending field/value, got: {error}"
    );
}
