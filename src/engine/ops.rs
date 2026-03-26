//! Bridge operations: Rust functions exposed to JS as `agentdesk.*`.
//!
//! Strategy: register simple Rust callbacks that accept String/i32 args,
//! then create JS wrappers that do the marshaling. This avoids rquickjs
//! lifetime issues with Value<'js> in MutFn closures.

use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

/// Register all `agentdesk.*` globals in the given JS context.
pub fn register_globals(ctx: &Ctx<'_>, db: Db) -> JsResult<()> {
    let globals = ctx.globals();

    let ad = Object::new(ctx.clone())?;

    // ── agentdesk.registerPolicy (placeholder) ───────────────────
    let noop = Function::new(ctx.clone(), || -> JsResult<()> { Ok(()) })?;
    ad.set("registerPolicy", noop)?;

    // Set the global first so JS wrapper code can reference it
    globals.set("agentdesk", ad)?;

    // ── agentdesk.db ─────────────────────────────────────────────
    register_db_ops(ctx, db.clone())?;

    // ── agentdesk.log ────────────────────────────────────────────
    register_log_ops(ctx)?;

    // ── agentdesk.config ─────────────────────────────────────────
    register_config_ops(ctx, db.clone())?;

    // ── agentdesk.http ────────────────────────────────────────────
    register_http_ops(ctx)?;

    // ── agentdesk.dispatch ────────────────────────────────────────
    register_dispatch_ops(ctx, db.clone())?;

    // ── agentdesk.kanban ────────────────────────────────────────
    register_kanban_ops(ctx, db.clone())?;

    // ── agentdesk.kv ─────────────────────────────────────────────
    register_kv_ops(ctx, db.clone())?;

    // ── agentdesk.message ────────────────────────────────────────
    register_message_ops(ctx, db)?;

    // ── agentdesk.exec ──────────────────────────────────────────
    register_exec_ops(ctx)?;

    Ok(())
}

// ── DB ops ───────────────────────────────────────────────────────
//
// We use a JSON-string bridge: Rust receives (sql, params_json_string)
// and returns a json_string. A thin JS wrapper does JSON.parse/stringify.

fn register_db_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let db_obj = Object::new(ctx.clone())?;

    // Internal: __db_query_raw(sql, params_json) → json_string
    let db_q = db.clone();
    let query_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(move |sql: String, params_json: String| -> String {
            db_query_raw(&db_q, &sql, &params_json)
        }),
    )?;
    db_obj.set("__query_raw", query_raw)?;

    // Internal: __db_execute_raw(sql, params_json) → json_string
    let db_e = db.clone();
    let execute_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(move |sql: String, params_json: String| -> String {
            db_execute_raw(&db_e, &sql, &params_json)
        }),
    )?;
    db_obj.set("__execute_raw", execute_raw)?;

    ad.set("db", db_obj)?;

    // JS wrappers that do JSON marshaling
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var rawQuery = agentdesk.db.__query_raw;
            var rawExec = agentdesk.db.__execute_raw;

            agentdesk.db.query = function(sql, params) {
                var json = rawQuery(sql, JSON.stringify(params || []));
                return JSON.parse(json);
            };
            agentdesk.db.execute = function(sql, params) {
                // Block direct status updates on kanban_cards — use agentdesk.kanban.setStatus() instead
                // Only blocks "status =" but not "review_status =", "blocked_reason =" etc.
                if (/UPDATE\s+kanban_cards\b/i.test(sql) && /(?<![_a-z])status\s*=/i.test(sql)) {
                    throw new Error("Direct kanban_cards status UPDATE is blocked. Use agentdesk.kanban.setStatus(cardId, newStatus) instead.");
                }
                var json = rawExec(sql, JSON.stringify(params || []));
                return JSON.parse(json);
            };
        })();
        undefined;
    "#,
    )?;

    Ok(())
}

fn db_query_raw(db: &Db, sql: &str, params_json: &str) -> String {
    let params: Vec<serde_json::Value> = serde_json::from_str(params_json).unwrap_or_default();
    let bind: Vec<rusqlite::types::Value> = params.iter().map(json_to_sqlite).collect();

    // Use a separate read-only connection to avoid blocking the write Mutex.
    // This prevents deadlock when onTick (holding engine lock) queries DB
    // while request handlers hold the write lock.
    let conn = match db.read_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"__error":"db read: {e}"}}"#),
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => return format!(r#"{{"__error":"prepare: {e}"}}"#),
    };

    let col_count = stmt.column_count();
    let col_names: Vec<std::string::String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = bind
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = match stmt.query_map(params_ref.as_slice(), |row| {
        let mut map = serde_json::Map::new();
        for i in 0..col_count {
            let val: rusqlite::types::Value = row.get(i)?;
            let jv = sqlite_to_json(&val);
            map.insert(col_names[i].clone(), jv);
        }
        Ok(serde_json::Value::Object(map))
    }) {
        Ok(r) => r,
        Err(e) => return format!(r#"{{"__error":"query: {e}"}}"#),
    };

    let result: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();
    serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string())
}

fn db_execute_raw(db: &Db, sql: &str, params_json: &str) -> String {
    let params: Vec<serde_json::Value> = serde_json::from_str(params_json).unwrap_or_default();
    let bind: Vec<rusqlite::types::Value> = params.iter().map(json_to_sqlite).collect();

    // Use a separate read-write connection to avoid holding the main
    // Rust Mutex that request handlers need. SQLite WAL serializes
    // concurrent writers via busy_timeout (5s).
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"__error":"db conn: {e}"}}"#),
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = bind
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let changes = match conn.execute(sql, params_ref.as_slice()) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{"__error":"execute: {e}"}}"#),
    };

    format!(r#"{{"changes":{changes}}}"#)
}

fn json_to_sqlite(val: &serde_json::Value) -> rusqlite::types::Value {
    match val {
        serde_json::Value::Null => rusqlite::types::Value::Null,
        serde_json::Value::Bool(b) => rusqlite::types::Value::Integer(if *b { 1 } else { 0 }),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rusqlite::types::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                rusqlite::types::Value::Real(f)
            } else {
                rusqlite::types::Value::Null
            }
        }
        serde_json::Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        _ => rusqlite::types::Value::Text(val.to_string()),
    }
}

fn sqlite_to_json(val: &rusqlite::types::Value) -> serde_json::Value {
    match val {
        rusqlite::types::Value::Null => serde_json::Value::Null,
        rusqlite::types::Value::Integer(i) => serde_json::json!(*i),
        rusqlite::types::Value::Real(f) => serde_json::json!(*f),
        rusqlite::types::Value::Text(s) => serde_json::Value::String(s.clone()),
        rusqlite::types::Value::Blob(b) => {
            let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b);
            serde_json::Value::String(encoded)
        }
    }
}

// ── Log ops ──────────────────────────────────────────────────────

fn register_log_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let log_obj = Object::new(ctx.clone())?;

    log_obj.set(
        "info",
        Function::new(ctx.clone(), |msg: String| {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] 📜 [policy] {msg}");
            tracing::info!(target: "policy", "{}", msg);
        })?,
    )?;

    log_obj.set(
        "warn",
        Function::new(ctx.clone(), |msg: String| {
            tracing::warn!(target: "policy", "{}", msg);
        })?,
    )?;

    log_obj.set(
        "error",
        Function::new(ctx.clone(), |msg: String| {
            tracing::error!(target: "policy", "{}", msg);
        })?,
    )?;

    ad.set("log", log_obj)?;
    Ok(())
}

// ── Config ops ───────────────────────────────────────────────────

fn register_config_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let config_obj = Object::new(ctx.clone())?;

    // __config_get_raw(key) → JSON string: "null" or "\"value\""
    let db_c = db;
    config_obj.set(
        "__get_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |key: String| -> String {
                let conn = match db_c.separate_conn() {
                    Ok(c) => c,
                    Err(_) => return "null".to_string(),
                };
                match conn.query_row("SELECT value FROM kv_meta WHERE key = ?1", [&key], |row| {
                    row.get::<_, String>(0)
                }) {
                    Ok(val) => serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string()),
                    Err(_) => "null".to_string(),
                }
            }),
        )?,
    )?;

    ad.set("config", config_obj)?;

    // JS wrapper
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var rawGet = agentdesk.config.__get_raw;
            agentdesk.config.get = function(key) {
                return JSON.parse(rawGet(key));
            };
        })();
        undefined;
    "#,
    )?;

    Ok(())
}

// ── HTTP ops ────────────────────────────────────────────────────
//
// agentdesk.http.post(url, body) → response_string
// Synchronous HTTP POST for localhost API calls from policy JS.
// Only allows loopback targets for security.

fn register_http_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let http_obj = Object::new(ctx.clone())?;

    http_obj.set(
        "__post_raw",
        Function::new(ctx.clone(), |url: String, body_json: String| -> String {
            let loopback_prefix = format!("http://{}", crate::config::loopback());
            if !url.starts_with(&loopback_prefix) {
                return r#"{"error":"only localhost allowed"}"#.to_string();
            }
            // Run on a dedicated thread to avoid blocking the tokio I/O
            // driver.  ureq is synchronous — if called directly on a tokio
            // worker it can self-deadlock when the target is our own HTTP
            // server (the worker blocks on recv while no other worker is
            // available to handle the incoming request).
            let handle = std::thread::spawn(move || {
                match ureq::AgentBuilder::new()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .post(&url)
                    .set("Content-Type", "application/json")
                    .send_string(&body_json)
                {
                    Ok(resp) => resp.into_string().unwrap_or_else(|_| "{}".to_string()),
                    Err(e) => format!(r#"{{"error":"{}"}}"#, e),
                }
            });
            handle
                .join()
                .unwrap_or_else(|_| r#"{"error":"thread panic"}"#.to_string())
        })?,
    )?;

    ad.set("http", http_obj)?;

    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var raw = agentdesk.http.__post_raw;
            agentdesk.http.post = function(url, body) {
                return JSON.parse(raw(url, JSON.stringify(body)));
            };
        })();
    "#,
    )?;

    Ok(())
}

// ── Dispatch ops ────────────────────────────────────────────────
//
// agentdesk.dispatch.create(cardId, agentId, dispatchType, title) → dispatchId
// Creates a task_dispatch row + updates kanban card to "requested".
// Discord notification is handled by posting to the local /api/send endpoint.

fn register_dispatch_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let dispatch_obj = Object::new(ctx.clone())?;

    // __dispatch_create_raw(card_id, agent_id, dispatch_type, title) → json_string
    let db_d = db;
    dispatch_obj.set(
        "__create_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(
                move |card_id: String,
                      agent_id: String,
                      dispatch_type: String,
                      title: String|
                      -> String {
                    dispatch_create_raw(&db_d, &card_id, &agent_id, &dispatch_type, &title)
                },
            ),
        )?,
    )?;

    ad.set("dispatch", dispatch_obj)?;

    // JS wrapper
    let _: rquickjs::Value = ctx.eval(r#"
        (function() {
            var raw = agentdesk.dispatch.__create_raw;
            agentdesk.dispatch.create = function(cardId, agentId, dispatchType, title) {
                var result = JSON.parse(raw(cardId, agentId, dispatchType || "implementation", title || "Dispatch"));
                if (result.error) throw new Error(result.error);
                // Discord notification is handled by the Rust handler after fire_hook returns
                // (async via send_dispatch_to_discord). Do NOT call ureq from QuickJS — it
                // deadlocks the tokio runtime because the unified axum API shares the same runtime.
                return result.dispatch_id;
            };
        })();
    "#)?;

    Ok(())
}

fn dispatch_create_raw(
    db: &Db,
    card_id: &str,
    agent_id: &str,
    dispatch_type: &str,
    title: &str,
) -> String {
    // Delegate to the single authoritative dispatch creation path (no hooks —
    // hooks are fired by the Rust caller after fire_hook returns).
    let context = serde_json::json!({});
    match crate::dispatch::create_dispatch_core(
        db,
        card_id,
        agent_id,
        dispatch_type,
        title,
        &context,
    ) {
        Ok((dispatch_id, _old_status)) => {
            // #117: Update card_review_state.pending_dispatch_id for review-decision
            if dispatch_type == "review-decision" {
                if let Ok(conn) = db.separate_conn() {
                    conn.execute(
                        "INSERT INTO card_review_state (card_id, state, pending_dispatch_id, updated_at) \
                         VALUES (?1, 'suggestion_pending', ?2, datetime('now')) \
                         ON CONFLICT(card_id) DO UPDATE SET pending_dispatch_id = ?2, updated_at = datetime('now')",
                        rusqlite::params![card_id, dispatch_id],
                    ).ok();
                }
            }
            // Get issue URL for Discord message
            let issue_url: Option<String> = db.separate_conn().ok().and_then(|conn| {
                conn.query_row(
                    "SELECT github_issue_url FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten()
            });
            format!(
                r#"{{"dispatch_id":"{}","card_id":"{}","agent_id":"{}","issue_url":{}}}"#,
                dispatch_id,
                card_id,
                agent_id,
                issue_url
                    .map(|u| format!("\"{}\"", u))
                    .unwrap_or_else(|| "null".to_string()),
            )
        }
        Err(e) => format!(r#"{{"error":"{}"}}"#, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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
            ).unwrap();
        }

        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            register_globals(&ctx, db.clone()).unwrap();
            let result: String = ctx
                .eval(r#"
                    var rows = agentdesk.db.query("SELECT id, name FROM agents WHERE id = ?", ["a1"]);
                    rows[0].name;
                "#)
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
                .eval(r#"
                    var r = agentdesk.db.execute(
                        "INSERT INTO agents (id, name, provider, status, xp) VALUES (?, ?, 'claude', 'idle', 0)",
                        ["b1", "Bot1"]
                    );
                    r.changes;
                "#)
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
            ).unwrap();
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
            ).unwrap();
            // Card in review with NULL started_at (first entry via rework)
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, started_at, created_at, updated_at)
                 VALUES ('card-js', 'Test', 'review', 'a1', NULL, datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            // Active dispatch to authorize transition
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
                 VALUES ('d-js', 'card-js', 'a1', 'rework', 'pending', 'Rework', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
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
}

// ── Message queue ops ─────────────────────────────────────────────
// agentdesk.message.queue(target, content, bot?, source?)
// Enqueues a message for async delivery — avoids self-referential HTTP deadlock (#120)

fn register_message_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let msg_obj = Object::new(ctx.clone())?;

    // __queue_raw(target, content, bot, source) → json_string
    let db_clone = db.clone();
    let queue_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(
            move |target: String, content: String, bot: String, source: String| -> String {
                message_queue_raw(&db_clone, &target, &content, &bot, &source)
            },
        ),
    )?;
    msg_obj.set("__queue_raw", queue_raw)?;

    ad.set("message", msg_obj)?;

    // JS wrapper: agentdesk.message.queue(target, content, bot?, source?)
    ctx.eval::<(), _>(
        r#"
        agentdesk.message.queue = function(target, content, bot, source) {
            return JSON.parse(agentdesk.message.__queue_raw(
                target || "",
                content || "",
                bot || "announce",
                source || "system"
            ));
        };
        "#,
    )?;

    Ok(())
}

fn message_queue_raw(db: &Db, target: &str, content: &str, bot: &str, source: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    match conn.execute(
        "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![target, content, bot, source],
    ) {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] 📨 message.queue → {target} (bot={bot}, id={id})");
            format!(r#"{{"ok":true,"id":{id}}}"#)
        }
        Err(e) => format!(r#"{{"error":"insert failed: {e}"}}"#),
    }
}

// ── Kanban ops ────────────────────────────────────────────────────
//
// agentdesk.kanban.setStatus(cardId, newStatus) — updates card status
// and fires appropriate hooks (OnCardTransition, OnCardTerminal, OnReviewEnter).
// This replaces direct SQL UPDATEs in policies to ensure hooks always fire.

fn register_kanban_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let kanban_obj = Object::new(ctx.clone())?;

    let db_set = db.clone();
    kanban_obj.set(
        "__setStatusRaw",
        Function::new(ctx.clone(), move |card_id: String, new_status: String| -> String {
            let conn = match db_set.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB lock: {}"}}"#, e),
            };

            // Get current status
            let old_status: String = match conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            ) {
                Ok(s) => s,
                Err(_) => return r#"{"error":"card not found"}"#.to_string(),
            };

            if old_status == new_status {
                return format!(r#"{{"ok":true,"changed":false,"status":"{}"}}"#, new_status);
            }

            // Pipeline-driven guard and clock fields (#106 P5)
            crate::pipeline::ensure_loaded();
            let Some(pipeline) = crate::pipeline::try_get() else {
                return r#"{"error":"pipeline not loaded"}"#.to_string();
            };

            // Guard: prevent reverting terminal cards
            if pipeline.is_terminal(&old_status) && old_status != new_status {
                return format!(
                    r#"{{"error":"cannot revert terminal card from {} to {}"}}"#,
                    old_status, new_status
                );
            }

            // Clock fields from pipeline config
            let clock_extra = match pipeline.clock_for_state(&new_status) {
                Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
                    format!(", {} = COALESCE({}, datetime('now'))", clock.set, clock.set)
                }
                Some(clock) => format!(", {} = datetime('now')", clock.set),
                None => String::new(),
            };
            // Terminal cleanup: clear review-related fields
            let terminal_cleanup = if pipeline.is_terminal(&new_status) {
                ", review_status = NULL, suggestion_pending_at = NULL, review_entered_at = NULL, awaiting_dod_at = NULL"
            } else {
                ""
            };
            let extra = format!("{clock_extra}{terminal_cleanup}");
            let sql = format!(
                "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now'){} WHERE id = ?2",
                extra
            );
            if let Err(e) = conn.execute(&sql, rusqlite::params![new_status, card_id]) {
                return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
            }

            // Also update auto_queue_entries if terminal
            if pipeline.is_terminal(&new_status) {
                conn.execute(
                    "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') WHERE kanban_card_id = ?1 AND status = 'dispatched'",
                    [&card_id],
                ).ok();
            }

            // #117: Sync canonical review state on status transitions (pipeline-driven)
            let has_hooks = pipeline.hooks_for_state(&new_status).map_or(false, |h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
            let is_review_enter = pipeline.hooks_for_state(&new_status).map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
            if pipeline.is_terminal(&new_status) || !has_hooks {
                conn.execute(
                    "INSERT INTO card_review_state (card_id, state, updated_at) VALUES (?1, 'idle', datetime('now')) \
                     ON CONFLICT(card_id) DO UPDATE SET state = 'idle', pending_dispatch_id = NULL, updated_at = datetime('now')",
                    [&card_id],
                ).ok();
            } else if is_review_enter {
                conn.execute(
                    "INSERT INTO card_review_state (card_id, state, review_entered_at, updated_at) VALUES (?1, 'reviewing', datetime('now'), datetime('now')) \
                     ON CONFLICT(card_id) DO UPDATE SET state = 'reviewing', review_entered_at = datetime('now'), updated_at = datetime('now')",
                    [&card_id],
                ).ok();
            }

            format!(
                r#"{{"ok":true,"changed":true,"from":"{}","to":"{}","card_id":"{}"}}"#,
                old_status, new_status, card_id
            )
        })?,
    )?;

    let db_get = db.clone();
    kanban_obj.set(
        "__getCardRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            let conn = match db_get.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            match conn.query_row(
                "SELECT id, status, assigned_agent_id, title, review_status, review_round, latest_dispatch_id FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_, String>(0)?,
                        "status": row.get::<_, String>(1)?,
                        "assigned_agent_id": row.get::<_, Option<String>>(2)?,
                        "title": row.get::<_, Option<String>>(3)?,
                        "review_status": row.get::<_, Option<String>>(4)?,
                        "review_round": row.get::<_, Option<i64>>(5)?,
                        "latest_dispatch_id": row.get::<_, Option<String>>(6)?,
                    }))
                },
            ) {
                Ok(card) => card.to_string(),
                Err(_) => r#"{"error":"card not found"}"#.to_string(),
            }
        })?,
    )?;

    ad.set("kanban", kanban_obj)?;

    // JS wrapper that parses JSON and accumulates transitions for post-hook processing.
    // setStatus only updates the DB — transition hooks (OnCardTransition, OnReviewEnter,
    // OnCardTerminal) cannot fire from within a hook because the engine is not reentrant.
    // Instead, transitions are collected in __pendingTransitions and the Rust caller
    // processes them after the hook returns via drain_pending_transitions().
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var raw = agentdesk.kanban.__setStatusRaw;
            var getRaw = agentdesk.kanban.__getCardRaw;
            agentdesk.kanban.__pendingTransitions = [];
            agentdesk.kanban.setStatus = function(cardId, newStatus) {
                var result = JSON.parse(raw(cardId, newStatus));
                if (result.error) throw new Error(result.error);
                if (result.changed) {
                    agentdesk.kanban.__pendingTransitions.push({
                        card_id: result.card_id,
                        from: result.from,
                        to: result.to
                    });
                    agentdesk.log.info("[setStatus] " + result.card_id + " " + result.from + " -> " + result.to + " (pendingLen=" + agentdesk.kanban.__pendingTransitions.length + ")");
                } else {
                    agentdesk.log.info("[setStatus] " + cardId + " -> " + newStatus + " (no-change)");
                }
                return result;
            };
            agentdesk.kanban.getCard = function(cardId) {
                var result = JSON.parse(getRaw(cardId));
                if (result.error) return null;
                return result;
            };
        })();
    "#,
    )?;

    Ok(())
}

// ── KV ops (#126) ─────────────────────────────────────────────────
//
// agentdesk.kv.set(key, value, ttlSeconds) — set with optional TTL
// agentdesk.kv.get(key) → value or null (filters expired)
// agentdesk.kv.delete(key) — delete a key

fn register_kv_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let kv_obj = Object::new(ctx.clone())?;

    // __kvSetRaw(key, value, ttlSeconds) — Rust raw impl, always 3 args
    let db_set = db.clone();
    kv_obj.set(
        "__setRaw",
        Function::new(
            ctx.clone(),
            move |key: String, value: String, ttl_seconds: i64| -> String {
                let conn = match db_set.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
                };
                let result = if ttl_seconds > 0 {
                    conn.execute(
                        &format!(
                            "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?1, ?2, datetime('now', '+{} seconds'))",
                            ttl_seconds
                        ),
                        rusqlite::params![key, value],
                    )
                } else {
                    conn.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?1, ?2, NULL)",
                        rusqlite::params![key, value],
                    )
                };
                match result {
                    Ok(_) => r#"{"ok":true}"#.to_string(),
                    Err(e) => format!(r#"{{"error":"{}"}}"#, e),
                }
            },
        )?,
    )?;

    // __kvGetRaw(key) → JSON: {"found":true,"value":"..."} or {"found":false}
    let db_get = db.clone();
    kv_obj.set(
        "__getRaw",
        Function::new(ctx.clone(), move |key: String| -> String {
            let conn = match db_get.separate_conn() {
                Ok(c) => c,
                Err(_) => return r#"{"found":false}"#.to_string(),
            };
            match conn.query_row(
                "SELECT value FROM kv_meta WHERE key = ?1 AND (expires_at IS NULL OR expires_at > datetime('now'))",
                [&key],
                |row| row.get::<_, String>(0),
            ) {
                Ok(v) => format!(r#"{{"found":true,"value":{}}}"#, serde_json::json!(v)),
                Err(_) => r#"{"found":false}"#.to_string(),
            }
        })?,
    )?;

    // kv.delete(key)
    let db_del = db;
    kv_obj.set(
        "delete",
        Function::new(ctx.clone(), move |key: String| -> String {
            let conn = match db_del.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            match conn.execute("DELETE FROM kv_meta WHERE key = ?1", [&key]) {
                Ok(_) => r#"{"ok":true}"#.to_string(),
                Err(e) => format!(r#"{{"error":"{}"}}"#, e),
            }
        })?,
    )?;

    ad.set("kv", kv_obj)?;

    // JS wrappers for optional TTL and null semantics
    ctx.eval::<(), _>(
        r#"
        (function() {
            var raw = agentdesk.kv;
            agentdesk.kv.set = function(key, value, ttlSeconds) {
                return JSON.parse(raw.__setRaw(key, value, ttlSeconds || 0));
            };
            agentdesk.kv.get = function(key) {
                var r = JSON.parse(raw.__getRaw(key));
                return r.found ? r.value : null;
            };
        })();
    "#,
    )?;

    Ok(())
}

// ── Exec ops ──────────────────────────────────────────────────────
//
// agentdesk.exec(command, args) → stdout string
// Runs a local command synchronously. Limited to safe commands.

fn register_exec_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;

    ad.set(
        "exec",
        Function::new(ctx.clone(), |cmd: String, args_json: String| -> String {
            // Only allow safe commands (tmux for read-only session queries)
            let allowed = ["gh", "git", "tmux"];
            if !allowed.contains(&cmd.as_str()) {
                return format!("ERROR: command '{}' not allowed", cmd);
            }

            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            match std::process::Command::new(&cmd).args(&args).output() {
                Ok(output) if output.status.success() => {
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    format!("ERROR: {}", stderr.trim())
                }
                Err(e) => format!("ERROR: {}", e),
            }
        })?,
    )?;

    // JS wrapper to accept array directly
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var rawExec = agentdesk.exec;
            agentdesk.exec = function(cmd, args) {
                return rawExec(cmd, JSON.stringify(args || []));
            };
        })();
    "#,
    )?;

    // agentdesk.inflight.list() — list active inflight turns with started_at
    let inflight_obj = rquickjs::Object::new(ctx.clone())?;
    inflight_obj.set(
        "list",
        Function::new(ctx.clone(), || -> String {
            let mut results = Vec::new();
            if let Some(root) = crate::cli::agentdesk_runtime_root() {
                let inflight_dir = root.join("runtime/discord_inflight");
                for provider in &["claude", "codex"] {
                    let dir = inflight_dir.join(provider);
                    if let Ok(entries) = std::fs::read_dir(&dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().map(|e| e == "json").unwrap_or(false) {
                                let channel_id = path.file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("")
                                    .to_string();
                                if let Ok(content) = std::fs::read_to_string(&path) {
                                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
                                        // Map inflight file fields to output:
                                        // channel_name → for agent identification
                                        // tmux_session_name → for diagnostics
                                        // session_id → Claude session ID
                                        let channel_name = data.get("channel_name").and_then(|v| v.as_str()).unwrap_or("");
                                        let tmux_name = data.get("tmux_session_name").and_then(|v| v.as_str()).unwrap_or("");
                                        results.push(serde_json::json!({
                                            "channel_id": channel_id,
                                            "provider": provider,
                                            "started_at": data.get("started_at").and_then(|v| v.as_str()).unwrap_or(""),
                                            "channel_name": channel_name,
                                            "tmux_session_name": tmux_name,
                                            "session_id": data.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
                                        }));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string())
        }),
    )?;
    inflight_obj.set(
        "remove",
        Function::new(
            ctx.clone(),
            |provider: String, channel_id: String| -> String {
                if let Some(root) = crate::cli::agentdesk_runtime_root() {
                    let path = root
                        .join("runtime/discord_inflight")
                        .join(&provider)
                        .join(format!("{channel_id}.json"));
                    if path.exists() {
                        let _ = std::fs::remove_file(&path);
                        return format!(r#"{{"ok":true,"removed":"{}"}}"#, path.display());
                    }
                }
                r#"{"ok":false,"error":"not found"}"#.to_string()
            },
        ),
    )?;
    ad.set("inflight", inflight_obj)?;

    // JS wrapper
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var rawList = agentdesk.inflight.list;
            var rawRemove = agentdesk.inflight.remove;
            agentdesk.inflight.list = function() {
                return JSON.parse(rawList());
            };
            agentdesk.inflight.remove = function(provider, channelId) {
                return JSON.parse(rawRemove(provider, "" + channelId));
            };
        })();
    "#,
    )?;

    // agentdesk.session.sendCommand(sessionKey, command) — inject a slash command into a tmux session
    let session_obj = rquickjs::Object::new(ctx.clone())?;
    session_obj.set(
        "sendCommand",
        rquickjs::Function::new(
            ctx.clone(),
            |session_key: String, command: String| -> String {
                let result = std::process::Command::new("tmux")
                    .args(["send-keys", "-t", &session_key, &command, "Enter"])
                    .output();
                match result {
                    Ok(out) if out.status.success() => {
                        format!(
                            r#"{{"ok":true,"session":"{}","command":"{}"}}"#,
                            session_key, command
                        )
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        format!(r#"{{"ok":false,"error":"tmux: {}"}}"#, stderr.trim())
                    }
                    Err(e) => {
                        format!(r#"{{"ok":false,"error":"{}"}}"#, e)
                    }
                }
            },
        ),
    )?;

    // agentdesk.session.kill(sessionKey) — force-kill a tmux session (for deadlock recovery)
    session_obj.set(
        "kill",
        rquickjs::Function::new(ctx.clone(), |session_key: String| -> String {
            // session_key is "hostname:tmux_name"; tmux interprets colon as
            // session:window separator, so extract only the tmux_name part.
            let tmux_name = session_key
                .split_once(':')
                .map(|(_, name)| name)
                .unwrap_or(&session_key);
            let result = std::process::Command::new("tmux")
                .args(["kill-session", "-t", tmux_name])
                .output();
            match result {
                Ok(out) if out.status.success() => {
                    format!(r#"{{"ok":true,"session":"{}"}}"#, session_key)
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    format!(r#"{{"ok":false,"error":"tmux: {}"}}"#, stderr.trim())
                }
                Err(e) => {
                    format!(r#"{{"ok":false,"error":"{}"}}"#, e)
                }
            }
        }),
    )?;

    ad.set("session", session_obj)?;

    Ok(())
}
