use crate::db::Db;

use super::{register_globals, review_state_sync, review_state_sync_on_conn};

fn test_db() -> Db {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    crate::db::schema::migrate(&conn).unwrap();
    crate::db::wrap_conn(conn)
}

#[test]
fn test_engine_db_query_op() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'TestBot', 'claude', 'idle', 0)",
            [],
        )
        .unwrap();
    }

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let result: String = ctx
            .eval(
                r#"
                    var rows = agentdesk.db.query("SELECT id, name FROM agents WHERE id = ?", ["a1"]);
                    rows[0].name;
                "#,
            )
            .unwrap();
        assert_eq!(result, "TestBot");
    });
}

#[test]
fn test_engine_db_execute_op() {
    let db = test_db();
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let changes: i32 = ctx
            .eval(
                r#"
                    var r = agentdesk.db.execute(
                        "INSERT INTO agents (id, name, provider, status, xp) VALUES (?, ?, 'claude', 'idle', 0)",
                        ["b1", "Bot1"]
                    );
                    r.changes;
                "#,
            )
            .unwrap();
        assert_eq!(changes, 1);
    });

    let conn = db.separate_conn().unwrap();
    let name: String = conn
        .query_row("SELECT name FROM agents WHERE id = 'b1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "Bot1");
}

#[test]
fn test_engine_log_ops() {
    let db = test_db();
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let _: rquickjs::Value = ctx
            .eval(
                r#"
                    agentdesk.log.info("test info message");
                    agentdesk.log.warn("test warn message");
                    agentdesk.log.error("test error message");
                    null;
                "#,
            )
            .unwrap();
    });
}

#[test]
fn test_engine_config_get() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('test_key', 'test_value')",
            [],
        )
        .unwrap();
    }

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let val: String = ctx.eval(r#"agentdesk.config.get("test_key")"#).unwrap();
        assert_eq!(val, "test_value");

        let is_null: bool = ctx
            .eval(r#"agentdesk.config.get("nonexistent") === null"#)
            .unwrap();
        assert!(is_null);
    });
}

#[test]
fn test_engine_db_query_no_params() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, xp) VALUES ('z1', 'Zero', 'claude', 'idle', 10)",
            [],
        )
        .unwrap();
    }

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let xp: i32 = ctx
            .eval(r#"agentdesk.db.query("SELECT xp FROM agents")[0].xp"#)
            .unwrap();
        assert_eq!(xp, 10);
    });
}

/// #128: JS setStatus("in_progress") sets started_at.
/// With pipeline coalesce mode: preserves existing started_at.
/// Without pipeline (fallback): resets to now.
/// This test verifies the transition itself succeeds and started_at is set.
#[test]
fn js_set_status_resets_started_at_on_in_progress_reentry() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('a1', 'Bot', '111', '222')",
            [],
        )
        .unwrap();
        // Card in review with NULL started_at (first entry via rework)
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, started_at, created_at, updated_at)
             VALUES ('card-js', 'Test', 'review', 'a1', NULL, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // Active dispatch to authorize transition
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('d-js', 'card-js', 'a1', 'rework', 'pending', 'Rework', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
    }

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let result: String = ctx
            .eval(r#"JSON.stringify(agentdesk.kanban.setStatus("card-js", "in_progress"))"#)
            .unwrap();
        // Should not contain error
        assert!(
            !result.contains("error"),
            "setStatus should succeed: {}",
            result
        );
    });

    // Verify started_at was set (either reset or coalesced depending on pipeline config)
    let started_at: Option<String> = {
        let conn = db.separate_conn().unwrap();
        conn.query_row(
            "SELECT started_at FROM kanban_cards WHERE id = 'card-js'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(
        started_at.is_some(),
        "started_at should be set after transitioning to in_progress"
    );
}

/// Seed a minimal kanban_cards row for FK satisfaction in review state tests.
fn seed_card_for_review(conn: &rusqlite::Connection, card_id: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
         VALUES ('agent-t', 'Test', '0', '0')",
        [],
    )
    .ok();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
         VALUES (?1, 'T', 'review', 'agent-t', datetime('now'), datetime('now'))",
        [card_id],
    )
    .unwrap();
}

// #158: review_state_sync_on_conn — idle state sets state and clears pending_dispatch_id
#[test]
fn test_review_state_sync_idle() {
    let db = test_db();
    let conn = db.separate_conn().unwrap();
    seed_card_for_review(&conn, "rs-1");
    // Seed existing review state with pending_dispatch_id
    conn.execute(
        "INSERT INTO card_review_state (card_id, state, pending_dispatch_id, updated_at) \
         VALUES ('rs-1', 'suggestion_pending', 'disp-1', datetime('now'))",
        [],
    )
    .unwrap();

    let result = review_state_sync_on_conn(
        &conn,
        &serde_json::json!({"card_id": "rs-1", "state": "idle"}).to_string(),
    );
    assert!(
        result.contains("\"ok\":true"),
        "sync should succeed: {result}"
    );

    let (state, pd): (String, Option<String>) = conn
        .query_row(
            "SELECT state, pending_dispatch_id FROM card_review_state WHERE card_id = 'rs-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "idle");
    assert!(pd.is_none(), "idle should clear pending_dispatch_id");
}

// #158: leaving suggestion_pending must clear stale pending_dispatch_id
#[test]
fn test_review_state_sync_non_suggestion_pending_clears_pending_dispatch_id() {
    let db = test_db();
    let conn = db.separate_conn().unwrap();
    seed_card_for_review(&conn, "rs-1b");
    conn.execute(
        "INSERT INTO card_review_state (card_id, state, pending_dispatch_id, updated_at) \
         VALUES ('rs-1b', 'suggestion_pending', 'disp-2', datetime('now'))",
        [],
    )
    .unwrap();

    let result = review_state_sync_on_conn(
        &conn,
        &serde_json::json!({
            "card_id": "rs-1b",
            "state": "rework_pending",
            "last_decision": "pm_rework"
        })
        .to_string(),
    );
    assert!(
        result.contains("\"ok\":true"),
        "sync should succeed: {result}"
    );

    let (state, pd): (String, Option<String>) = conn
        .query_row(
            "SELECT state, pending_dispatch_id FROM card_review_state WHERE card_id = 'rs-1b'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "rework_pending");
    assert!(
        pd.is_none(),
        "non-suggestion_pending states must clear stale pending_dispatch_id"
    );
}

// #158: review_state_sync_on_conn — reviewing state auto-sets review_entered_at
#[test]
fn test_review_state_sync_reviewing() {
    let db = test_db();
    let conn = db.separate_conn().unwrap();
    seed_card_for_review(&conn, "rs-2");

    let result = review_state_sync_on_conn(
        &conn,
        &serde_json::json!({"card_id": "rs-2", "state": "reviewing", "review_round": 1})
            .to_string(),
    );
    assert!(result.contains("\"ok\":true"));

    let (state, rr, entered): (String, Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT state, review_round, review_entered_at FROM card_review_state WHERE card_id = 'rs-2'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "reviewing");
    assert_eq!(rr, Some(1));
    assert!(
        entered.is_some(),
        "reviewing should auto-set review_entered_at"
    );
}

// #158: review_state_sync_on_conn — clear_verdict only NULLs last_verdict
#[test]
fn test_review_state_sync_clear_verdict() {
    let db = test_db();
    let conn = db.separate_conn().unwrap();
    seed_card_for_review(&conn, "rs-3");
    conn.execute(
        "INSERT INTO card_review_state (card_id, state, last_verdict, updated_at) \
         VALUES ('rs-3', 'reviewing', 'improve', datetime('now'))",
        [],
    )
    .unwrap();

    let result = review_state_sync_on_conn(
        &conn,
        &serde_json::json!({"card_id": "rs-3", "state": "clear_verdict"}).to_string(),
    );
    assert!(
        result.contains("\"ok\":true"),
        "sync should succeed: {result}"
    );

    let (state, verdict): (String, Option<String>) = conn
        .query_row(
            "SELECT state, last_verdict FROM card_review_state WHERE card_id = 'rs-3'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "reviewing", "clear_verdict should not change state");
    assert!(verdict.is_none(), "clear_verdict should NULL last_verdict");
}

// #158: review_state_sync (JSON wrapper) — round-trip test
#[test]
fn test_review_state_sync_json_wrapper() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        seed_card_for_review(&conn, "rs-4");
    }
    let result = review_state_sync(
        &db,
        r#"{"card_id":"rs-4","state":"suggestion_pending","last_verdict":"improve","pending_dispatch_id":"d-99"}"#,
    );
    assert!(
        result.contains("\"ok\":true"),
        "sync should succeed: {result}"
    );

    let conn = db.separate_conn().unwrap();
    let (state, verdict, pd): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT state, last_verdict, pending_dispatch_id FROM card_review_state WHERE card_id = 'rs-4'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "suggestion_pending");
    assert_eq!(verdict.as_deref(), Some("improve"));
    assert_eq!(pd.as_deref(), Some("d-99"));
}
