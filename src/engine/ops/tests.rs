use crate::db::Db;
use std::{ffi::OsString, fs, path::PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::{
    register_globals, register_globals_with_supervisor, review_state_sync,
    review_state_sync_on_conn,
};

fn test_db() -> Db {
    let conn = libsql_rusqlite::Connection::open_in_memory().unwrap(); // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    crate::db::schema::migrate(&conn).unwrap();
    crate::db::wrap_conn(conn)
}

fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct EnvVarOverride {
    _guard: std::sync::MutexGuard<'static, ()>,
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarOverride {
    fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let guard = test_env_lock();
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self {
            _guard: guard,
            key,
            previous,
        }
    }
}

impl Drop for EnvVarOverride {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
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

/// #1007: Guard the first migrated slice — `ci-recovery.js` must not
/// regress to raw `agentdesk.db.query/execute` callsites.
#[test]
fn policies_raw_db_ci_recovery_slice_stays_typed() {
    let policy_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies/ci-recovery.js");
    let policy = fs::read_to_string(&policy_path).expect("ci-recovery policy must be readable");
    assert!(
        !policy.contains("agentdesk.db.query") && !policy.contains("agentdesk.db.execute"),
        "ci-recovery.js must stay on typed facades (agentdesk.ciRecovery.*): {policy_path:?}"
    );
    // The in-body slice marker must also still be present so future edits
    // around the escalation status check keep the typed call.
    assert!(
        policy.contains("// typed-facade-slice:start ci-recovery"),
        "ci-recovery.js must keep the typed-facade slice start marker"
    );
    assert!(
        policy.contains("// typed-facade-slice:end ci-recovery"),
        "ci-recovery.js must keep the typed-facade slice end marker"
    );
}

/// #1007: Budget guard — the total count of `agentdesk.db.query` and
/// `agentdesk.db.execute` callsites across `policies/*.js` must not grow
/// beyond the whitelist captured at the time of the first migration slice.
///
/// New policies / new callsites MUST either:
///   1. migrate to a typed facade under `agentdesk.<domain>.*`, or
///   2. annotate the raw-db callsite with the escape-hatch marker
///      `/* legacy-raw-db: policy=<name> capability=<intent> source_event=<hook> */`
///      so the audit log at `policy.raw_db_audit` can attribute them.
///
/// If this test fails after a legitimate raw-db addition, either lift the
/// budget here with a PR comment explaining why migration isn't possible
/// yet, or add the escape-hatch marker and update
/// `RAW_DB_ESCAPE_HATCH_ALLOWANCE` below.
#[test]
fn policies_raw_db_count_stays_within_budget() {
    // Captured after ci-recovery migration (#1007 first slice). See
    // docs/generated/policy-db-inventory.md for the classified listing.
    const RAW_DB_BUDGET: usize = 189;
    // Number of callsites that are currently annotated with the
    // escape-hatch marker (`/* legacy-raw-db: ... */`). Starts at 0 and
    // grows only when a caller explicitly justifies a raw callsite.
    const RAW_DB_ESCAPE_HATCH_ALLOWANCE: usize = 0;

    let policies_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    let mut total_callsites = 0usize;
    let mut marked_callsites = 0usize;
    let mut unmarked_callsites = 0usize;

    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // Skip test fixtures
                    if path.file_name().and_then(|n| n.to_str()) == Some("__tests__") {
                        continue;
                    }
                    walk(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("js") {
                    out.push(path);
                }
            }
        }
    }

    let mut files = Vec::new();
    walk(&policies_dir, &mut files);

    for file in &files {
        let src = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (lineno, line) in src.lines().enumerate() {
            if line.contains("agentdesk.db.query") || line.contains("agentdesk.db.execute") {
                total_callsites += 1;
                // Check whether the callsite (this line or the 3 preceding
                // lines) carries an escape-hatch marker comment.
                let marker_window_start = lineno.saturating_sub(3);
                let window: Vec<&str> = src
                    .lines()
                    .skip(marker_window_start)
                    .take(lineno - marker_window_start + 1)
                    .collect();
                let window_text = window.join("\n");
                if window_text.contains("legacy-raw-db:") {
                    marked_callsites += 1;
                } else {
                    unmarked_callsites += 1;
                }
            }
        }
    }

    assert!(
        unmarked_callsites <= RAW_DB_BUDGET,
        "#1007 raw-db budget exceeded: unmarked callsites={} budget={} (total={} marked={}). \
         Either migrate the new callsite to a typed facade (see src/engine/ops/ci_recovery_ops.rs for an example) \
         or annotate it with /* legacy-raw-db: policy=<name> capability=<intent> source_event=<hook> */ \
         and bump RAW_DB_ESCAPE_HATCH_ALLOWANCE in this test.",
        unmarked_callsites,
        RAW_DB_BUDGET,
        total_callsites,
        marked_callsites
    );

    assert!(
        marked_callsites <= RAW_DB_ESCAPE_HATCH_ALLOWANCE,
        "#1007 escape-hatch allowance exceeded: marked={} allowance={}. \
         Bump RAW_DB_ESCAPE_HATCH_ALLOWANCE only if the new annotated callsite is genuinely justified.",
        marked_callsites,
        RAW_DB_ESCAPE_HATCH_ALLOWANCE
    );
}

#[test]
fn auto_queue_log_context_hydrates_agent_id_without_redundant_reloads() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('ag-queue', 'Queue Agent', '111', '222')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES ('card-log', 'Queue Card', 'in_progress', 'ag-queue', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-log', 'itismyfield/AgentDesk', 'ag-queue', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at) \
             VALUES (?1, ?2, ?3, 'implementation', 'dispatched', 'Queue Dispatch', ?4, datetime('now'), datetime('now'))",
            libsql_rusqlite::params![ // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
                "dispatch-log",
                "card-log",
                "ag-queue",
                r#"{"entry_id":"entry-log","agent_id":"ag-queue"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, dispatch_id, status, thread_group, batch_phase, slot_index) \
             VALUES ('entry-log', 'run-log', 'card-log', 'ag-queue', 'dispatch-log', 'dispatched', 2, 3, 4)",
            [],
        )
        .unwrap();
    }

    let policy_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies/auto-queue.js");
    let policy = fs::read_to_string(&policy_path).expect("auto-queue policy must be readable");

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let _: rquickjs::Value = ctx.eval(r#"agentdesk.registerPolicy = function(_) {};"#).unwrap();
        let _: rquickjs::Value = ctx.eval(policy.as_str()).unwrap();
        let result: String = ctx
            .eval(
                r#"
                (function() {
                    var tracked = [];
                    var originalQuery = agentdesk.db.query;
                    agentdesk.db.query = function(sql, params) {
                        if (sql.indexOf("FROM auto_queue_entries") >= 0 || sql.indexOf("FROM task_dispatches") >= 0) {
                            tracked.push(sql);
                        }
                        return originalQuery.call(agentdesk.db, sql, params);
                    };

                    var ctx = _normalizeAutoQueueLogContext({
                        entry_id: "entry-log",
                        dispatch_id: "dispatch-log"
                    });

                    return JSON.stringify({
                        ctx: ctx,
                        query_count: tracked.length,
                        formatted: _formatAutoQueueLogContext(ctx)
                    });
                })()
                "#,
            )
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ctx"]["agent_id"], "ag-queue");
        assert_eq!(parsed["ctx"]["run_id"], "run-log");
        assert_eq!(parsed["ctx"]["thread_group"], 2);
        assert_eq!(parsed["query_count"], 2);

        let formatted = parsed["formatted"].as_str().unwrap_or("");
        assert!(
            formatted.contains("agent_id=ag-queue"),
            "formatted context must include agent_id: {formatted}"
        );
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

#[test]
fn js_set_status_warns_when_bypassing_active_dispatch_gate() {
    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('a1', 'Bot', '111', '222')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-js-warn', 'Warn Test', 'requested', 'a1', datetime('now'), datetime('now'))",
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
                (() => {
                    var warnings = [];
                    agentdesk.log.warn = function(msg) { warnings.push(msg); };
                    var response = agentdesk.kanban.setStatus("card-js-warn", "in_progress");
                    return JSON.stringify({ response: response, warnings: warnings });
                })()
                "#,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let warning = parsed["response"]["warning"].as_str().unwrap_or("");
        assert!(
            warning.contains("has_active_dispatch"),
            "raw response must surface the missing active dispatch warning: {parsed}"
        );
        let logged_warning = parsed["warnings"][0].as_str().unwrap_or("");
        assert!(
            logged_warning.contains("has_active_dispatch"),
            "setStatus wrapper must emit a warn log for missing active dispatch: {parsed}"
        );
    });

    let status: String = db
        .separate_conn()
        .unwrap()
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-js-warn'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "in_progress");
}

/// Seed a minimal kanban_cards row for FK satisfaction in review state tests.
fn seed_card_for_review(conn: &libsql_rusqlite::Connection, card_id: &str) {
    // TODO(#839): sqlite compatibility retained for out-of-scope callers or legacy tests.
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

    let engine = crate::engine::PolicyEngine::new_with_legacy_db(
        &crate::config::Config::default(),
        db.clone(),
    )
    .unwrap();
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

#[test]
fn js_auto_queue_run_status_bridge_updates_run_and_releases_slots() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id) \
             VALUES ('aq-run-agent', 'AQ Run Agent', 'claude', 'idle', '123456789012345678')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, created_at, updated_at
            ) VALUES (
                'aq-run-card', 'AQ Run Card', 'ready', 'medium', 'aq-run-agent',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('aq-run-status', 'repo-1', 'aq-run-agent', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'aq-run-entry', 'aq-run-status', 'aq-run-card',
                'aq-run-agent', 'done', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group
            ) VALUES (
                'aq-run-agent', 0, 'aq-run-status', 0
            )",
            [],
        )
        .unwrap();
    }

    let engine = crate::engine::PolicyEngine::new_with_legacy_db(
        &crate::config::Config::default(),
        db.clone(),
    )
    .unwrap();
    let bridge = crate::supervisor::BridgeHandle::new();
    bridge.attach_engine(&engine);

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals_with_supervisor(&ctx, db.clone(), bridge.clone()).unwrap();
        let raw: String = ctx
            .eval(
                r#"
                JSON.stringify((function() {
                    var paused = agentdesk.autoQueue.pauseRun("aq-run-status", "test_pause");
                    var slotAfterPauseRows = agentdesk.db.query(
                        "SELECT assigned_run_id FROM auto_queue_slots " +
                        "WHERE agent_id = 'aq-run-agent' AND slot_index = 0"
                    );
                    var resumed = agentdesk.autoQueue.resumeRun("aq-run-status", "test_resume");
                    var slotAfterResumeRows = agentdesk.db.query(
                        "SELECT assigned_run_id FROM auto_queue_slots " +
                        "WHERE agent_id = 'aq-run-agent' AND slot_index = 0"
                    );
                    var completed = agentdesk.autoQueue.completeRun(
                        "aq-run-status",
                        "test_complete",
                        { releaseSlots: true }
                    );
                    return {
                        paused: paused.changed,
                        slotAfterPause: slotAfterPauseRows.length > 0
                            ? slotAfterPauseRows[0].assigned_run_id
                            : "__missing__",
                        resumed: resumed.changed,
                        slotAfterResume: slotAfterResumeRows.length > 0
                            ? slotAfterResumeRows[0].assigned_run_id
                            : "__missing__",
                        completed: completed.changed
                    };
                })())
                "#,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["paused"], true);
        assert!(
            parsed["slotAfterPause"].is_null(),
            "pauseRun must release the slot immediately"
        );
        assert_eq!(parsed["resumed"], true);
        assert!(
            parsed["slotAfterResume"].is_null(),
            "resumeRun must not silently keep the old slot binding"
        );
        assert_eq!(parsed["completed"], true);
    });

    let conn = db.separate_conn().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'aq-run-status'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let slot_run: Option<String> = conn
        .query_row(
            "SELECT assigned_run_id FROM auto_queue_slots WHERE agent_id = 'aq-run-agent' AND slot_index = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let message_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
        .unwrap();
    assert_eq!(run_status, "completed");
    assert!(slot_run.is_none());
    assert_eq!(message_count, 1);
}

#[test]
fn js_auto_queue_consultation_bridge_updates_card_metadata_and_entry_status() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id) \
             VALUES ('aq-consult-agent', 'AQ Consult Agent', 'claude', 'idle', '123456789012345678')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, metadata, created_at, updated_at
            ) VALUES (
                'aq-consult-card', 'AQ Consult Card', 'requested', 'medium', 'aq-consult-agent',
                ?1, datetime('now'), datetime('now')
            )",
            [serde_json::json!({
                "keep": "yes",
                "preflight_status": "consult_required"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('aq-consult-run', 'repo-1', 'aq-consult-agent', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'aq-consult-entry', 'aq-consult-run', 'aq-consult-card',
                'aq-consult-agent', 'pending', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context) \
             VALUES ('dispatch-consult-1', 'aq-consult-agent', 'dispatched', '{}')",
            [],
        )
        .unwrap();
    }

    let engine = crate::engine::PolicyEngine::new_with_legacy_db(
        &crate::config::Config::default(),
        db.clone(),
    )
    .unwrap();
    let bridge = crate::supervisor::BridgeHandle::new();
    bridge.attach_engine(&engine);

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals_with_supervisor(&ctx, db.clone(), bridge.clone()).unwrap();
        let wrapper_type: String = ctx
            .eval(r#"typeof agentdesk.autoQueue.recordConsultationDispatch"#)
            .unwrap();
        assert_eq!(wrapper_type, "function");
        let raw: String = ctx
            .eval(
                r#"
                (function() {
                    return agentdesk.autoQueue.__recordConsultationDispatchRaw(
                        "aq-consult-entry",
                        "aq-consult-card",
                        "dispatch-consult-1",
                        "test_consultation_bridge",
                        JSON.stringify({
                            keep: "yes",
                            preflight_status: "consult_required"
                        })
                    );
                })()
                "#,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            parsed.get("error").is_none(),
            "raw consultation bridge error: {raw}"
        );
        assert_eq!(parsed["changed"], true);
        assert_eq!(parsed["metadata"]["keep"], "yes");
        assert_eq!(parsed["metadata"]["consultation_status"], "pending");
        assert_eq!(
            parsed["metadata"]["consultation_dispatch_id"],
            "dispatch-consult-1"
        );
    });

    let conn = db.separate_conn().unwrap();
    let metadata_raw: String = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = 'aq-consult-card'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata_raw).unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'aq-consult-entry'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(metadata["keep"], "yes");
    assert_eq!(metadata["preflight_status"], "consult_required");
    assert_eq!(metadata["consultation_status"], "pending");
    assert_eq!(metadata["consultation_dispatch_id"], "dispatch-consult-1");
    assert_eq!(entry_status, "dispatched");
    assert_eq!(dispatch_id.as_deref(), Some("dispatch-consult-1"));
}

#[test]
fn js_auto_queue_phase_gate_bridge_saves_and_clears_rows() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id) \
             VALUES ('aq-phase-agent', 'AQ Phase Agent', 'claude', 'idle', '123456789012345678')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, created_at, updated_at
            ) VALUES (
                'aq-phase-card', 'AQ Phase Card', 'ready', 'medium', 'aq-phase-agent',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('aq-phase-run', 'repo-1', 'aq-phase-agent', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context) \
             VALUES ('aq-phase-valid-1', 'aq-phase-agent', 'dispatched', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context) \
             VALUES ('aq-phase-valid-2', 'aq-phase-agent', 'dispatched', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context) \
             VALUES ('aq-phase-stale', 'aq-phase-agent', 'dispatched', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
            ) VALUES (
                'aq-phase-run', 2, 'pending', 'aq-phase-stale', 'phase_gate_passed'
            )",
            [],
        )
        .unwrap();
    }

    let engine = crate::engine::PolicyEngine::new_with_legacy_db(
        &crate::config::Config::default(),
        db.clone(),
    )
    .unwrap();
    let bridge = crate::supervisor::BridgeHandle::new();
    bridge.attach_engine(&engine);

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals_with_supervisor(&ctx, db.clone(), bridge.clone()).unwrap();
        let raw: String = ctx
            .eval(
                r#"
                JSON.stringify((function() {
                    var saved = agentdesk.autoQueue.savePhaseGateState("aq-phase-run", 2, {
                        status: "failed",
                        verdict: "phase_gate_failed",
                        dispatch_ids: [
                            "aq-phase-valid-1",
                            "aq-phase-valid-1",
                            "aq-phase-missing",
                            "aq-phase-valid-2"
                        ],
                        pass_verdict: "phase_gate_passed",
                        next_phase: 3,
                        final_phase: true,
                        anchor_card_id: "aq-phase-card",
                        failure_reason: "phase gate failed",
                        created_at: "2026-04-15 00:00:00"
                    });
                    var rows = agentdesk.db.query(
                        "SELECT dispatch_id, status, verdict, next_phase, final_phase, anchor_card_id, failure_reason " +
                        "FROM auto_queue_phase_gates WHERE run_id = ? AND phase = ? ORDER BY COALESCE(dispatch_id, '')",
                        ["aq-phase-run", 2]
                    );
                    var cleared = agentdesk.autoQueue.clearPhaseGateState("aq-phase-run", 2);
                    var remaining = agentdesk.db.query(
                        "SELECT COUNT(*) AS cnt FROM auto_queue_phase_gates WHERE run_id = ? AND phase = ?",
                        ["aq-phase-run", 2]
                    )[0].cnt;
                    return {
                        saved: saved,
                        rows: rows,
                        cleared: cleared.changed,
                        remaining: remaining
                    };
                })())
                "#,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed["saved"]["dispatch_ids"],
            serde_json::json!(["aq-phase-valid-1", "aq-phase-valid-2"])
        );
        assert_eq!(parsed["saved"]["removed_stale_rows"], 1);
        assert_eq!(parsed["rows"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["rows"][0]["dispatch_id"], "aq-phase-valid-1");
        assert_eq!(parsed["rows"][1]["dispatch_id"], "aq-phase-valid-2");
        assert_eq!(parsed["rows"][0]["status"], "failed");
        assert_eq!(parsed["rows"][0]["verdict"], "phase_gate_failed");
        assert_eq!(parsed["rows"][0]["next_phase"], 3);
        assert_eq!(parsed["rows"][0]["final_phase"], 1);
        assert_eq!(parsed["rows"][0]["anchor_card_id"], "aq-phase-card");
        assert_eq!(parsed["rows"][0]["failure_reason"], "phase gate failed");
        assert_eq!(parsed["cleared"], true);
        assert_eq!(parsed["remaining"], 0);
    });
}

#[test]
fn js_auto_queue_continue_run_after_entry_passes_agent_id_to_activate() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let policy_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies/auto-queue.js");
    let policy = fs::read_to_string(&policy_path).expect("auto-queue policy must be readable");

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let _: rquickjs::Value = ctx.eval(r#"agentdesk.registerPolicy = function(_) {};"#).unwrap();
        let _: rquickjs::Value = ctx.eval(policy.as_str()).unwrap();
        let captured: String = ctx
            .eval(
                r#"
                (function() {
                    var activateCalls = [];
                    agentdesk.autoQueue.activate = function(body) {
                        activateCalls.push(body);
                        return { count: 0, dispatched: [] };
                    };

                    var originalQuery = agentdesk.db.query;
                    agentdesk.db.query = function(sql, params) {
                        if (sql.indexOf("SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND status IN ('pending', 'dispatched')") === 0) {
                            return [{ cnt: 1 }];
                        }
                        if (sql.indexOf("SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND COALESCE(thread_group, 0) = ? AND status IN ('pending', 'dispatched')") === 0) {
                            return [{ cnt: 1 }];
                        }
                        if (sql.indexOf("SELECT COUNT(*) as cnt FROM kanban_cards WHERE assigned_agent_id = ? AND status IN (") === 0) {
                            return [{ cnt: 0 }];
                        }
                        return originalQuery.call(agentdesk.db, sql, params);
                    };

                    continueRunAfterEntry("run-continue", "agent-continue", 3, 0, null);
                    return JSON.stringify(activateCalls[0] || null);
                })()
                "#,
            )
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(parsed["run_id"], "run-continue");
        assert_eq!(parsed["active_only"], true);
        assert_eq!(parsed["agent_id"], "agent-continue");
        assert_eq!(parsed["thread_group"], 3);
    });
}

#[cfg(unix)]
#[test]
fn test_runtime_refresh_inventory_docs_executes_generator_in_worktree() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("scripts")).unwrap();
    fs::create_dir_all(repo.path().join("docs/generated")).unwrap();
    fs::write(
        repo.path().join("scripts/generate_inventory_docs.py"),
        "print('placeholder inventory generator')\n",
    )
    .unwrap();

    let fake_python_dir = tempfile::tempdir().unwrap();
    let fake_python = fake_python_dir.path().join("python3");
    write_executable(
        &fake_python,
        r#"#!/bin/sh
set -eu
script="$1"
test -f "$script"
mkdir -p docs/generated
printf 'module refreshed\n' > docs/generated/module-inventory.md
printf 'route refreshed\n' > docs/generated/route-inventory.md
printf 'worker refreshed\n' > docs/generated/worker-inventory.md
echo 'inventory refreshed'
"#,
    );
    let _python_override = EnvVarOverride::set_path("AGENTDESK_PYTHON3_PATH", &fake_python);

    let db = test_db();
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    let repo_path = serde_json::to_string(repo.path().to_str().unwrap()).unwrap();
    let script = format!(
        r#"
                JSON.stringify(
                    agentdesk.runtime.refreshInventoryDocs({repo_path}, {{ timeout_ms: 5000 }})
                )
                "#,
    );
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();
        let raw: String = ctx.eval(script.clone()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["ok"], true, "refresh op must succeed: {parsed}");
        assert_eq!(
            parsed["stdout"].as_str().unwrap_or_default(),
            "inventory refreshed"
        );
    });

    assert_eq!(
        fs::read_to_string(repo.path().join("docs/generated/module-inventory.md")).unwrap(),
        "module refreshed\n"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("docs/generated/route-inventory.md")).unwrap(),
        "route refreshed\n"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("docs/generated/worker-inventory.md")).unwrap(),
        "worker refreshed\n"
    );
}

#[test]
fn js_wrappers_keep_resolving_raw_functions_after_gc_cycles() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('gc_safe_config_key', 'still_here')",
            [],
        )
        .unwrap();
    }

    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        register_globals(&ctx, db.clone()).unwrap();

        let baseline: String = ctx
            .eval(
                r#"
                JSON.stringify({
                    config: agentdesk.config.get("gc_safe_config_key"),
                    pipeline_initial: agentdesk.pipeline.initialState(),
                    review_active: agentdesk.review.hasActiveWork("missing-card"),
                    dispatch_active: agentdesk.dispatch.hasActiveWork("missing-card"),
                    agent_primary: agentdesk.agents.resolvePrimaryChannel("missing-agent"),
                    card_missing: agentdesk.cards.get("missing-card") === null,
                    kanban_missing: agentdesk.kanban.getCard("missing-card") === null,
                    queue_pending_type: typeof agentdesk.queue.status().dispatches.pending,
                    http_error: agentdesk.http.post("http://example.com", {}).error,
                    exec_error: agentdesk.exec("definitely-not-allowed", []),
                    inflight_is_array: Array.isArray(agentdesk.inflight.list())
                })
                "#,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&baseline).unwrap();
        assert_eq!(parsed["config"], "still_here");
        assert_eq!(parsed["pipeline_initial"], "backlog");
        assert_eq!(parsed["review_active"], false);
        assert_eq!(parsed["dispatch_active"], false);
        assert_eq!(parsed["agent_primary"], serde_json::Value::Null);
        assert_eq!(parsed["card_missing"], true);
        assert_eq!(parsed["kanban_missing"], true);
        assert_eq!(parsed["queue_pending_type"], "number");
        assert_eq!(parsed["http_error"], "only localhost allowed");
        assert!(
            parsed["exec_error"]
                .as_str()
                .unwrap_or("")
                .contains("not allowed"),
            "exec wrapper should still call the raw function: {parsed}"
        );
        assert_eq!(parsed["inflight_is_array"], true);
    });

    for _ in 0..8 {
        ctx.with(|ctx| {
            let _: rquickjs::Value = ctx
                .eval(
                    r#"
                    globalThis.__gc_stress = [];
                    for (var i = 0; i < 5000; i++) {
                        __gc_stress.push({
                            index: i,
                            label: "gc-" + i,
                            payload: { nested: [i, i + 1, i + 2] }
                        });
                    }
                    undefined;
                    "#,
                )
                .unwrap();
        });
        rt.run_gc();
    }

    ctx.with(|ctx| {
        let after_gc: String = ctx
            .eval(
                r#"
                JSON.stringify({
                    config: agentdesk.config.get("gc_safe_config_key"),
                    pipeline_initial: agentdesk.pipeline.initialState(),
                    review_active: agentdesk.review.hasActiveWork("missing-card"),
                    dispatch_active: agentdesk.dispatch.hasActiveWork("missing-card"),
                    agent_primary: agentdesk.agents.resolvePrimaryChannel("missing-agent"),
                    card_missing: agentdesk.cards.get("missing-card") === null,
                    kanban_missing: agentdesk.kanban.getCard("missing-card") === null,
                    queue_pending_type: typeof agentdesk.queue.status().dispatches.pending,
                    http_error: agentdesk.http.post("http://example.com", {}).error,
                    exec_error: agentdesk.exec("definitely-not-allowed", []),
                    inflight_is_array: Array.isArray(agentdesk.inflight.list())
                })
                "#,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&after_gc).unwrap();
        assert_eq!(parsed["config"], "still_here");
        assert_eq!(parsed["pipeline_initial"], "backlog");
        assert_eq!(parsed["review_active"], false);
        assert_eq!(parsed["dispatch_active"], false);
        assert_eq!(parsed["agent_primary"], serde_json::Value::Null);
        assert_eq!(parsed["card_missing"], true);
        assert_eq!(parsed["kanban_missing"], true);
        assert_eq!(parsed["queue_pending_type"], "number");
        assert_eq!(parsed["http_error"], "only localhost allowed");
        assert!(
            parsed["exec_error"]
                .as_str()
                .unwrap_or("")
                .contains("not allowed"),
            "exec wrapper should survive GC cycles: {parsed}"
        );
        assert_eq!(parsed["inflight_is_array"], true);
    });
}
