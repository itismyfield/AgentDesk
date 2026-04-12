use crate::db::Db;
use std::{fs, path::PathBuf};

use super::{
    register_globals, register_globals_with_supervisor, review_state_sync,
    review_state_sync_on_conn,
};

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
fn test_engine_db_execute_warns_and_blocks_core_table_sql() {
    let db = test_db();
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let result: String = ctx
            .eval(
                r#"
                    (function() {
                        var warnings = [];
                        agentdesk.log.warn = function(msg) { warnings.push(msg); };
                        var outcome;
                        try {
                            agentdesk.db.execute("DELETE FROM task_dispatches WHERE id = ?", ["dispatch-1"]);
                            outcome = "unexpected_success";
                        } catch (e) {
                            outcome = e.message;
                        }
                        return JSON.stringify({
                            outcome: outcome,
                            warning: warnings[0] || null
                        });
                    })()
                "#,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            parsed["outcome"]
                .as_str()
                .unwrap_or("")
                .contains("task_dispatches"),
            "blocked error must mention task_dispatches: {parsed}"
        );
        let warning = parsed["warning"].as_str().unwrap_or("");
        assert!(
            warning.contains("[policy-sql-guard]"),
            "warning log must include guard prefix: {warning}"
        );
        assert!(
            warning.contains("task_dispatches"),
            "warning log must name the guarded table: {warning}"
        );
    });
}

#[test]
fn test_engine_db_query_raw_returns_unified_error_json() {
    let db = test_db();
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();

        let raw: String = ctx
            .eval(r#"agentdesk.db.__query_raw("SELECT nope FROM missing_table", "[]")"#)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["code"], "database");
        assert_eq!(value["context"]["operation"], "agentdesk.db.query.prepare");

        let err_text: String = ctx
            .eval(
                r#"
                (() => {
                    try {
                        agentdesk.db.query("SELECT nope FROM missing_table");
                        return "no-error";
                    } catch (error) {
                        return String(error);
                    }
                })()
                "#,
            )
            .unwrap();
        assert!(
            err_text.contains("missing_table") || err_text.contains("no such table"),
            "db.query should surface the database failure, got: {err_text}"
        );
    });
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

#[test]
fn test_cards_facade_get_list_assign_set_priority() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id) \
             VALUES ('ag-card', 'Card Bot', 'claude', 'idle', '111')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, priority, metadata, github_issue_number, github_issue_url, created_at, updated_at) \
             VALUES ('card-facade', 'Facade Card', 'backlog', 'medium', '{\"labels\":\"agent:card-bot priority:high\"}', 348, 'https://github.com/itismyfield/AgentDesk/issues/348', datetime('now'), datetime('now'))",
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
                (function() {
                    var found = agentdesk.cards.get("card-facade");
                    if (!found || found.metadata.labels !== "agent:card-bot priority:high") {
                        throw new Error("cards.get returned unexpected metadata");
                    }
                    var listed = agentdesk.cards.list({
                        status: "backlog",
                        unassigned: true,
                        metadata_present: true
                    });
                    if (listed.length !== 1 || listed[0].id !== "card-facade") {
                        throw new Error("cards.list filter did not match expected card");
                    }
                    agentdesk.cards.assign("card-facade", "ag-card");
                    agentdesk.cards.setPriority("card-facade", "urgent");
                    var updated = agentdesk.cards.get("card-facade");
                    return JSON.stringify({
                        assigned_agent_id: updated.assigned_agent_id,
                        priority: updated.priority
                    });
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            r#"{"assigned_agent_id":"ag-card","priority":"urgent"}"#
        );
    });
}

#[test]
fn test_agents_facade_get_and_primary_channel() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, xp, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx) \
             VALUES ('ag-agent', 'Agent Bot', 'codex', 'idle', 7, '111', '222', '333', '444')",
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
                (function() {
                    var agent = agentdesk.agents.get("ag-agent");
                    return JSON.stringify({
                        id: agent.id,
                        primary: agentdesk.agents.primaryChannel("ag-agent"),
                        counter: agent.counter_model_channel,
                        channels: agent.all_channels.length
                    });
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            r#"{"id":"ag-agent","primary":"444","counter":"333","channels":4}"#
        );
    });
}

#[test]
fn test_review_get_verdict_facade() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        seed_card_for_review(&conn, "card-review-facade");
        conn.execute(
            "INSERT INTO card_review_state (card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision, decided_by, decided_at, review_entered_at, updated_at) \
             VALUES ('card-review-facade', 2, 'suggestion_pending', 'dispatch-1', 'improve', 'accept', 'pmd', datetime('now'), datetime('now'), datetime('now'))",
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
                (function() {
                    var review = agentdesk.review.getVerdict("card-review-facade");
                    return JSON.stringify({
                        verdict: review.verdict,
                        state: review.state,
                        review_round: review.review_round,
                        source: review.source
                    });
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            r#"{"verdict":"improve","state":"suggestion_pending","review_round":2,"source":"review_state"}"#
        );
    });
}

#[test]
fn test_review_entry_context_and_record_entry_facade() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id, discord_channel_cc, discord_channel_cdx) \
             VALUES ('ag-review-entry', 'Review Bot', 'codex', 'idle', '111', '222', '333')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, review_round, created_at, updated_at) \
             VALUES ('card-review-entry', 'Review Entry Card', 'review', 'ag-review-entry', 1, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, completed_at, updated_at) \
             VALUES ('impl-review-entry', 'card-review-entry', 'ag-review-entry', 'implementation', 'completed', 'Implementation', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, completed_at, updated_at) \
             VALUES ('rework-review-entry', 'card-review-entry', 'ag-review-entry', 'rework', 'completed', 'Rework', datetime('now'), datetime('now'))",
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
                (function() {
                    var entry = agentdesk.review.entryContext("card-review-entry");
                    agentdesk.review.recordEntry("card-review-entry", {
                        review_round: entry.next_round,
                        exclude_status: "done"
                    });
                    var updated = agentdesk.cards.get("card-review-entry");
                    return JSON.stringify({
                        current_round: entry.current_round,
                        completed_work_count: entry.completed_work_count,
                        should_advance_round: entry.should_advance_round,
                        next_round: entry.next_round,
                        stored_round: updated.review_round
                    });
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            r#"{"current_round":1,"completed_work_count":2,"should_advance_round":true,"next_round":2,"stored_round":2}"#
        );
    });
}

#[test]
fn test_review_entry_hint_advances_round_once_and_clears_metadata() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id, discord_channel_cc, discord_channel_cdx) \
             VALUES ('ag-review-hint', 'Review Hint Bot', 'codex', 'idle', '111', '222', '333')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, review_round, metadata, created_at, updated_at) \
             VALUES ('card-review-hint', 'Review Hint Card', 'review', 'ag-review-hint', 1, ?1, datetime('now'), datetime('now'))",
            [serde_json::json!({
                crate::engine::ops::ADVANCE_REVIEW_ROUND_HINT_KEY: true,
                "keep": "value"
            })
            .to_string()],
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
                (function() {
                    var entry = agentdesk.review.entryContext("card-review-hint");
                    agentdesk.review.recordEntry("card-review-hint", {
                        review_round: entry.next_round,
                        exclude_status: "done"
                    });
                    var updated = agentdesk.cards.get("card-review-hint");
                    return JSON.stringify({
                        should_advance_round: entry.should_advance_round,
                        next_round: entry.next_round,
                        stored_round: updated.review_round
                    });
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            r#"{"should_advance_round":true,"next_round":2,"stored_round":2}"#
        );
    });

    let conn = db.separate_conn().unwrap();
    let metadata_raw: Option<String> = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = 'card-review-hint'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(metadata["keep"], "value");
    assert!(
        metadata
            .get(crate::engine::ops::ADVANCE_REVIEW_ROUND_HINT_KEY)
            .is_none(),
        "review entry hint must be consumed after recordEntry"
    );
}

#[test]
fn test_queue_status_facade() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('ag-queue', 'Queue Bot', '111')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES ('card-queue', 'Queue Card', 'in_progress', 'ag-queue', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES ('dispatch-queue', 'card-queue', 'ag-queue', 'implementation', 'pending', 'Queue Dispatch', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source, status) \
             VALUES ('channel:111', 'hello', 'announce', 'system', 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status) VALUES ('dispatch-queue', 'notify', 'failed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('run-1', 'itismyfield/AgentDesk', 'ag-queue', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status) \
             VALUES ('entry-1', 'run-1', 'card-queue', 'ag-queue', 'pending')",
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
                (function() {
                    var status = agentdesk.queue.status();
                    return JSON.stringify({
                        pending_dispatches: status.dispatches.pending,
                        pending_messages: status.message_outbox.pending,
                        failed_dispatch_outbox: status.dispatch_outbox.failed,
                        active_runs: status.auto_queue.active_runs,
                        pending_entries: status.auto_queue.pending_entries
                    });
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            r#"{"pending_dispatches":1,"pending_messages":1,"failed_dispatch_outbox":1,"active_runs":1,"pending_entries":1}"#
        );
    });
}

#[test]
fn test_review_entry_slice_blocks_raw_db_reintroduction() {
    let policy_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies/review-automation.js");
    let policy =
        fs::read_to_string(&policy_path).expect("review-automation policy must be readable");
    let start = policy
        .find("// typed-facade-slice:start review-entry")
        .expect("review-entry slice start marker must exist");
    let end = policy
        .find("// typed-facade-slice:end review-entry")
        .expect("review-entry slice end marker must exist");
    let slice = &policy[start..end];
    assert!(
        !slice.contains("agentdesk.db."),
        "review-entry slice must stay on typed facades: {policy_path:?}"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_auto_queue_activate_bridge_dispatches_without_server_port() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id) \
             VALUES ('aq-bridge-agent', 'AQ Bridge Agent', 'claude', 'idle', '123456789012345678')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, created_at, updated_at
            ) VALUES (
                'aq-bridge-card', 'AQ Bridge Card', 'ready', 'medium', 'aq-bridge-agent',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, agent_id, status) \
             VALUES ('aq-bridge-run', 'aq-bridge-agent', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'aq-bridge-entry', 'aq-bridge-run', 'aq-bridge-card',
                'aq-bridge-agent', 'pending', 0
            )",
            [],
        )
        .unwrap();
    }

    let engine =
        crate::engine::PolicyEngine::new(&crate::config::Config::default(), db.clone()).unwrap();
    let bridge = crate::supervisor::BridgeHandle::new();
    bridge.attach_engine(&engine);

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals_with_supervisor(&ctx, db.clone(), bridge.clone()).unwrap();
        let raw: String = ctx
            .eval(
                r#"
                JSON.stringify(agentdesk.autoQueue.activate("aq-bridge-run"))
                "#,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["dispatched"][0]["card_id"], "aq-bridge-card");
    });

    let conn = db.separate_conn().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'aq-bridge-entry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'aq-bridge-card'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    assert_eq!(dispatch_count, 1);
}
