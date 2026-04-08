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

    // ── agentdesk.__pendingIntents — intent accumulator for deferred mutations (#121)
    ctx.eval::<(), _>(r#"agentdesk.__pendingIntents = [];"#)?;

    // ── agentdesk.__generateId — UUID v4 generation from Rust
    let gen_id = Function::new(ctx.clone(), || -> String {
        uuid::Uuid::new_v4().to_string()
    })?;
    {
        let ad: Object<'_> = ctx.globals().get("agentdesk")?;
        ad.set("__generateId", gen_id)?;
    }

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
    let db_for_pipeline = db.clone();
    let db_for_dm_reply = db.clone();
    let db_for_agents = db.clone();
    register_message_ops(ctx, db)?;

    // ── agentdesk.exec ──────────────────────────────────────────
    register_exec_ops(ctx)?;

    // ── agentdesk.pipeline ────────────────────────────────────────
    register_pipeline_ops(ctx, db_for_pipeline)?;

    // ── agentdesk.dmReply ────────────────────────────────────
    register_dm_reply_ops(ctx, db_for_dm_reply)?;

    // ── agentdesk.agents ─────────────────────────────────────────
    register_agent_ops(ctx, db_for_agents)?;

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
                // #155: Block direct mutations on kanban_cards CardState fields.
                // status, review_status, latest_dispatch_id must go through controlled helpers.
                if (/UPDATE\s+kanban_cards\b/i.test(sql)) {
                    if (/(?<![_a-z])status\s*=/i.test(sql)) {
                        throw new Error("Direct kanban_cards status UPDATE is blocked. Use agentdesk.kanban.setStatus(cardId, newStatus) instead.");
                    }
                    if (/review_status\s*=/i.test(sql)) {
                        throw new Error("Direct kanban_cards review_status UPDATE is blocked. Use agentdesk.kanban.setReviewStatus(cardId, status, opts) instead.");
                    }
                    if (/latest_dispatch_id\s*=/i.test(sql)) {
                        throw new Error("Direct kanban_cards latest_dispatch_id UPDATE is blocked. Use the dispatch API instead.");
                    }
                }
                // Block direct INSERT/UPDATE on task_dispatches — use agentdesk.dispatch.create() instead.
                if (/(?:INSERT\s+INTO|UPDATE)\s+task_dispatches\b/i.test(sql)) {
                    throw new Error("Direct task_dispatches mutation is blocked. Use agentdesk.dispatch.create() instead.");
                }
                // Block direct INSERT/REPLACE/UPDATE/DELETE on card_review_state — use agentdesk.reviewState.sync() instead (#158).
                if (/(?:INSERT(?:\s+OR\s+REPLACE)?\s+INTO|REPLACE\s+INTO|UPDATE|DELETE\s+FROM)\s+card_review_state\b/i.test(sql)) {
                    throw new Error("Direct card_review_state mutation is blocked. Use agentdesk.reviewState.sync(cardId, state, opts) instead.");
                }
                // Direct write — db.execute remains synchronous by design.
                // dispatch.create and kanban.setStatus use intent/transition model;
                // converting db.execute to intents requires typed intents for each
                // mutation pattern (card_review_state, kv_meta, agents, etc.).
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

    log_obj.set(
        "debug",
        Function::new(ctx.clone(), |msg: String| {
            tracing::debug!(target: "policy", "{}", msg);
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

    // #248: __dispatch_create_sync(card_id, agent_id, dispatch_type, title) → json_string
    // Synchronous DB INSERT — no deferred intent.
    let db_d = db.clone();
    dispatch_obj.set(
        "__create_sync",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(
                move |card_id: String,
                      agent_id: String,
                      dispatch_type: String,
                      title: String|
                      -> String {
                    dispatch_create_sync(&db_d, &card_id, &agent_id, &dispatch_type, &title)
                },
            ),
        )?,
    )?;

    // __mark_failed_raw(dispatch_id, reason) → json_string
    // Marks a dispatch as failed. Used by timeout handlers.
    let db_mf = db.clone();
    dispatch_obj.set(
        "__mark_failed_raw",
        Function::new(ctx.clone(), move |dispatch_id: String, reason: String| -> String {
            let conn = match db_mf.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
            };
            match conn.execute(
                "UPDATE task_dispatches SET status = 'failed', result = ?1, updated_at = datetime('now') \
                 WHERE id = ?2 AND status IN ('pending', 'dispatched')",
                rusqlite::params![reason, dispatch_id],
            ) {
                Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
                Err(e) => format!(r#"{{"error":"sql: {}"}}"#, e),
            }
        })?,
    )?;

    // __mark_completed_raw(dispatch_id, result_json) → json_string
    // Marks a dispatch as completed. Used by orphan recovery.
    let db_mc = db.clone();
    dispatch_obj.set(
        "__mark_completed_raw",
        Function::new(ctx.clone(), move |dispatch_id: String, result_json: String| -> String {
            let conn = match db_mc.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
            };
            match conn.execute(
                "UPDATE task_dispatches SET status = 'completed', result = ?1, updated_at = datetime('now') \
                 WHERE id = ?2 AND status IN ('pending', 'dispatched')",
                rusqlite::params![result_json, dispatch_id],
            ) {
                Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
                Err(e) => format!(r#"{{"error":"sql: {}"}}"#, e),
            }
        })?,
    )?;

    // __set_retry_count_raw(dispatch_id, count) → json_string
    // Updates retry_count for auto-retry tracking.
    let db_rc = db;
    dispatch_obj.set(
        "__set_retry_count_raw",
        Function::new(
            ctx.clone(),
            move |dispatch_id: String, count: i32| -> String {
                let conn = match db_rc.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
                };
                match conn.execute(
                    "UPDATE task_dispatches SET retry_count = ?1 WHERE id = ?2",
                    rusqlite::params![count, dispatch_id],
                ) {
                    Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
                    Err(e) => format!(r#"{{"error":"sql: {}"}}"#, e),
                }
            },
        )?,
    )?;

    ad.set("dispatch", dispatch_obj)?;

    // JS wrapper — synchronous dispatch creation with Rust-side validation/INSERT
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var sync = agentdesk.dispatch.__create_sync;
            agentdesk.dispatch.create = function(cardId, agentId, dispatchType, title) {
                var dt = dispatchType || "implementation";
                var t = title || "Dispatch";
                // #248: Synchronous DB INSERT — no deferred intent.
                // Validation + INSERT happen atomically in Rust.
                var result = JSON.parse(sync(cardId, agentId, dt, t));
                if (result.error) throw new Error(result.error);
                var dispatchId = result.dispatch_id;
                return dispatchId;
            };
            var rawFail = agentdesk.dispatch.__mark_failed_raw;
            agentdesk.dispatch.markFailed = function(dispatchId, reason) {
                var result = JSON.parse(rawFail(dispatchId, reason || ""));
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.markFailed] no rows affected for " + dispatchId + " — already terminal or missing");
                }
                return result;
            };
            var rawComplete = agentdesk.dispatch.__mark_completed_raw;
            agentdesk.dispatch.markCompleted = function(dispatchId, resultJson) {
                var result = JSON.parse(rawComplete(dispatchId, resultJson || "{}"));
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.markCompleted] no rows affected for " + dispatchId + " — already terminal or missing");
                }
                return result;
            };
            var rawRetry = agentdesk.dispatch.__set_retry_count_raw;
            agentdesk.dispatch.setRetryCount = function(dispatchId, count) {
                var result = JSON.parse(rawRetry(dispatchId, count));
                if (result.error) throw new Error(result.error);
                if (result.rows_affected === 0) {
                    agentdesk.log.warn("[dispatch.setRetryCount] no rows affected for " + dispatchId + " — missing");
                }
                return result;
            };
        })();
    "#,
    )?;

    Ok(())
}

/// #248/#249: Synchronous dispatch creation — validates and inserts into DB
/// immediately. The notify outbox row is now inserted atomically inside
/// `create_dispatch_core`, so no JS-side outbox buffering is needed.
fn dispatch_create_sync(
    db: &Db,
    card_id: &str,
    agent_id: &str,
    dispatch_type: &str,
    title: &str,
) -> String {
    let context = serde_json::json!({});
    match crate::dispatch::create_dispatch_core(
        db,
        card_id,
        agent_id,
        dispatch_type,
        title,
        &context,
    ) {
        Ok((dispatch_id, _old_status, reused)) => {
            if reused {
                return format!(r#"{{"dispatch_id":"{dispatch_id}","reused":true}}"#);
            }
            // #117/#158: Update card_review_state for review-decision dispatches
            if dispatch_type == "review-decision" {
                review_state_sync(
                    db,
                    &serde_json::json!({
                        "card_id": card_id,
                        "state": "suggestion_pending",
                        "pending_dispatch_id": dispatch_id,
                    })
                    .to_string(),
                );
            }
            format!(r#"{{"dispatch_id":"{dispatch_id}"}}"#)
        }
        Err(e) => {
            format!(r#"{{"error":"{}"}}"#, e.to_string().replace('"', "'"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
// agentdesk.kanban.setStatus(cardId, newStatus, force?) — updates card status
// and fires appropriate hooks (OnCardTransition, OnCardTerminal, OnReviewEnter).
// This replaces direct SQL UPDATEs in policies to ensure hooks always fire.

fn register_kanban_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let kanban_obj = Object::new(ctx.clone())?;

    let db_set = db.clone();
    kanban_obj.set(
        "__setStatusRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, new_status: String, force: Option<bool>| -> String {
            let conn = match db_set.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB lock: {}"}}"#, e),
            };
            let force = force.unwrap_or(false);

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
            // Resolve effective pipeline for this card (repo + agent overrides)
            crate::pipeline::ensure_loaded();
            let repo_id: Option<String> = conn
                .query_row("SELECT repo_id FROM kanban_cards WHERE id = ?1", [&card_id], |r| r.get(0))
                .ok()
                .flatten();
            let agent_id: Option<String> = conn
                .query_row("SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1", [&card_id], |r| r.get(0))
                .ok()
                .flatten();
            let effective = crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
            let pipeline = &effective;

            // Guard: prevent reverting terminal cards
            if pipeline.is_terminal(&old_status) && old_status != new_status {
                return format!(
                    r#"{{"error":"cannot revert terminal card from {} to {}"}}"#,
                    old_status, new_status
                );
            }

            // #228: Enforce review_verdict_pass gate on transitions to terminal states.
            // Only this specific gate is checked — other gates (has_active_dispatch,
            // review_rework) and force_only transitions are used legitimately by
            // policies and must not be blocked here. PMD bypasses via force-transition API.
            if pipeline.is_terminal(&new_status) && !force {
                if let Some(t) = pipeline.find_transition(&old_status, &new_status) {
                    let needs_review_pass = t.gates.iter().any(|g| {
                        pipeline.gates.get(g.as_str())
                            .map_or(false, |gc| gc.check.as_deref() == Some("review_verdict_pass"))
                    });
                    if needs_review_pass {
                        // Mirror kanban.rs:112-125 — check the LATEST completed review
                        // dispatch only, not any historical pass. A card with pass R1
                        // then rework R2 must not skip the current review round.
                        let latest_verdict: Option<String> = conn
                            .query_row(
                                "SELECT json_extract(result, '$.verdict') FROM task_dispatches \
                                 WHERE kanban_card_id = ?1 AND dispatch_type = 'review' \
                                 AND status = 'completed' \
                                 ORDER BY updated_at DESC LIMIT 1",
                                [&card_id],
                                |row| row.get(0),
                            )
                            .ok()
                            .flatten();
                        let has_pass = matches!(
                            latest_verdict.as_deref(),
                            Some("pass") | Some("approved")
                        );
                        if !has_pass {
                            return format!(
                                r#"{{"error":"gate blocked: review_verdict_pass — no review pass verdict","from":"{}","to":"{}"}}"#,
                                old_status, new_status
                            );
                        }
                    }
                }
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
                sync_auto_queue_terminal_on_conn(&conn, &card_id);
            }

            // #117/#158: Sync canonical review state via unified entrypoint
            let has_hooks = pipeline.hooks_for_state(&new_status).map_or(false, |h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
            let is_review_enter = pipeline.hooks_for_state(&new_status).map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
            if pipeline.is_terminal(&new_status) || !has_hooks {
                review_state_sync_on_conn(&conn, &serde_json::json!({"card_id": card_id, "state": "idle"}).to_string());
            } else if is_review_enter {
                review_state_sync_on_conn(&conn, &serde_json::json!({"card_id": card_id, "state": "reviewing"}).to_string());
            }

            format!(
                r#"{{"ok":true,"changed":true,"from":"{}","to":"{}","card_id":"{}"}}"#,
                old_status, new_status, card_id
            )
        })?,
    )?;

    let db_reopen = db.clone();
    kanban_obj.set(
        "__reopenRaw",
        Function::new(ctx.clone(), move |card_id: String, new_status: String| -> String {
            let conn = match db_reopen.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB lock: {}"}}"#, e),
            };

            let old_status: String = match conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            ) {
                Ok(s) => s,
                Err(_) => return r#"{"error":"card not found"}"#.to_string(),
            };

            crate::pipeline::ensure_loaded();
            let repo_id: Option<String> = conn
                .query_row(
                    "SELECT repo_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let effective =
                crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
            let pipeline = &effective;

            if !pipeline.is_terminal(&old_status) {
                return format!(
                    r#"{{"error":"reopen requires terminal card (current: {})"}}"#,
                    old_status
                );
            }
            if pipeline.is_terminal(&new_status) {
                return format!(
                    r#"{{"error":"reopen target must be non-terminal (target: {})"}}"#,
                    new_status
                );
            }
            if old_status == new_status {
                return format!(r#"{{"ok":true,"changed":false,"status":"{}"}}"#, new_status);
            }

            let clock_extra = match pipeline.clock_for_state(&new_status) {
                Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
                    format!(", {} = COALESCE({}, datetime('now'))", clock.set, clock.set)
                }
                Some(clock) => format!(", {} = datetime('now')", clock.set),
                None => String::new(),
            };

            let sql = format!(
                "UPDATE kanban_cards SET status = ?1, completed_at = NULL, updated_at = datetime('now'){} WHERE id = ?2",
                clock_extra
            );
            if let Err(e) = conn.execute(&sql, rusqlite::params![new_status, card_id]) {
                return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
            }

            conn.execute(
                "UPDATE auto_queue_entries SET status = 'dispatched', completed_at = NULL \
                 WHERE kanban_card_id = ?1 AND status = 'done'",
                [&card_id],
            )
            .ok();

            crate::kanban::correct_tn_to_fn_on_reopen(&db_reopen, &card_id);

            let has_hooks = pipeline
                .hooks_for_state(&new_status)
                .map_or(false, |h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
            let is_review_enter = pipeline.hooks_for_state(&new_status).map_or(false, |h| {
                h.on_enter.iter().any(|n| n == "OnReviewEnter")
            });
            if !has_hooks {
                review_state_sync_on_conn(
                    &conn,
                    &serde_json::json!({"card_id": card_id, "state": "idle"}).to_string(),
                );
            } else if is_review_enter {
                review_state_sync_on_conn(
                    &conn,
                    &serde_json::json!({"card_id": card_id, "state": "reviewing"}).to_string(),
                );
            }

            format!(
                r#"{{"ok":true,"changed":true,"from":"{}","to":"{}","card_id":"{}","reopened":true}}"#,
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

    // #155: setReviewStatus — controlled path for review_status + clock updates.
    // Replaces direct SQL UPDATEs so the ExecuteSQL guard can block bare review_status writes.
    let db_review = db.clone();
    kanban_obj.set(
        "__setReviewStatusRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, opts_json: String| -> String {
                let opts: serde_json::Value = match serde_json::from_str(&opts_json) {
                    Ok(v) => v,
                    Err(e) => return format!(r#"{{"error":"bad opts: {}"}}"#, e),
                };
                let conn = match db_review.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
                };

                // Build dynamic SET clause
                let mut sets = vec!["updated_at = datetime('now')".to_string()];
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];

                if let Some(rs) = opts.get("review_status") {
                    if rs.is_null() {
                        sets.push("review_status = NULL".to_string());
                    } else if let Some(s) = rs.as_str() {
                        params.push(Box::new(s.to_string()));
                        sets.push(format!("review_status = ?{}", params.len()));
                    }
                }
                if let Some(v) = opts.get("suggestion_pending_at") {
                    if v.is_null() {
                        sets.push("suggestion_pending_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("suggestion_pending_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("review_entered_at") {
                    if v.is_null() {
                        sets.push("review_entered_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("review_entered_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("awaiting_dod_at") {
                    if v.is_null() {
                        sets.push("awaiting_dod_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("awaiting_dod_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("blocked_reason") {
                    if v.is_null() {
                        sets.push("blocked_reason = NULL".to_string());
                    } else if let Some(s) = v.as_str() {
                        params.push(Box::new(s.to_string()));
                        sets.push(format!("blocked_reason = ?{}", params.len()));
                    }
                }

                // Optional terminal guard: only update if status != terminal
                let where_clause = if let Some(excl) = opts.get("exclude_status") {
                    if let Some(s) = excl.as_str() {
                        params.push(Box::new(s.to_string()));
                        params.push(Box::new(card_id.clone()));
                        format!(
                            "WHERE id = ?{} AND status != ?{}",
                            params.len(),
                            params.len() - 1
                        )
                    } else {
                        params.push(Box::new(card_id.clone()));
                        format!("WHERE id = ?{}", params.len())
                    }
                } else {
                    params.push(Box::new(card_id.clone()));
                    format!("WHERE id = ?{}", params.len())
                };

                let sql = format!(
                    "UPDATE kanban_cards SET {} {}",
                    sets.join(", "),
                    where_clause
                );
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();
                if let Err(e) = conn.execute(&sql, param_refs.as_slice()) {
                    return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
                }

                // #117/#158: Sync card_review_state via unified entrypoint
                if let Some(rs) = opts.get("review_status") {
                    let review_state = if rs.is_null() {
                        Some("idle")
                    } else {
                        rs.as_str()
                    };
                    if let Some(s) = review_state {
                        review_state_sync_on_conn(
                            &conn,
                            &serde_json::json!({"card_id": card_id, "state": s}).to_string(),
                        );
                    }
                }

                r#"{"ok":true}"#.to_string()
            },
        )?,
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
            var reopenRaw = agentdesk.kanban.__reopenRaw;
            var getRaw = agentdesk.kanban.__getCardRaw;
            agentdesk.kanban.__pendingTransitions = [];
            agentdesk.kanban.setStatus = function(cardId, newStatus, force) {
                var result = JSON.parse(raw(cardId, newStatus, !!force));
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
            agentdesk.kanban.reopen = function(cardId, newStatus) {
                var result = JSON.parse(reopenRaw(cardId, newStatus));
                if (result.error) throw new Error(result.error);
                if (result.changed) {
                    agentdesk.kanban.__pendingTransitions.push({
                        card_id: result.card_id,
                        from: result.from,
                        to: result.to
                    });
                    agentdesk.log.info("[reopen] " + result.card_id + " " + result.from + " -> " + result.to + " (pendingLen=" + agentdesk.kanban.__pendingTransitions.length + ")");
                } else {
                    agentdesk.log.info("[reopen] " + cardId + " -> " + newStatus + " (no-change)");
                }
                return result;
            };
            agentdesk.kanban.getCard = function(cardId) {
                var result = JSON.parse(getRaw(cardId));
                if (result.error) return null;
                return result;
            };
            var reviewRaw = agentdesk.kanban.__setReviewStatusRaw;
            agentdesk.kanban.setReviewStatus = function(cardId, reviewStatus, opts) {
                var o = opts || {};
                o.review_status = reviewStatus;
                var result = JSON.parse(reviewRaw(cardId, JSON.stringify(o)));
                if (result.error) throw new Error(result.error);
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
    let db_del = db.clone();
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

    // ── agentdesk.reviewState — typed bridge for card_review_state mutations (#158) ──
    // Replaces direct SQL INSERT/UPDATE on card_review_state from JS policies.
    // All review-state mutations go through this single entrypoint.
    {
        let db_rs = db.clone();
        let sync_raw = Function::new(ctx.clone(), move |json_str: String| -> String {
            review_state_sync(&db_rs, &json_str)
        })?;

        let _: rquickjs::Value = ctx.eval(
            r#"
            (function() {
                agentdesk.reviewState = {
                    __syncRaw: null,
                    sync: function(cardId, state, opts) {
                        opts = opts || {};
                        var payload = JSON.stringify({
                            card_id: cardId,
                            state: state,
                            review_round: opts.review_round || null,
                            last_verdict: opts.last_verdict || null,
                            last_decision: opts.last_decision || null,
                            pending_dispatch_id: opts.pending_dispatch_id || null,
                            approach_change_round: opts.approach_change_round || null,
                            review_entered_at: opts.review_entered_at || null
                        });
                        var result = JSON.parse(agentdesk.reviewState.__syncRaw(payload));
                        if (result.error) throw new Error(result.error);
                        return result;
                    }
                };
            })();
        "#,
        )?;

        let rs_obj: rquickjs::Value = ctx.eval("agentdesk.reviewState")?;
        let rs_obj: Object = rs_obj.into_object().unwrap();
        rs_obj.set("__syncRaw", sync_raw)?;
    }

    Ok(())
}

/// Rust implementation of card_review_state sync (#158).
/// Single entrypoint for all review-state mutations.
/// Used by both the JS bridge and Rust route handlers.
pub fn review_state_sync(db: &Db, json_str: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db error: {}"}}"#, e),
    };
    review_state_sync_on_conn(&conn, json_str)
}

/// Best-effort auto-queue cleanup for terminal cards.
///
/// When a card finishes, its active dispatch entry should become `done` and any
/// stale pending copies in active or paused runs should be skipped so they do
/// not block other runs.
pub(crate) fn sync_auto_queue_terminal_on_conn(conn: &rusqlite::Connection, card_id: &str) {
    conn.execute(
        "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') \
         WHERE kanban_card_id = ?1 AND status = 'dispatched'",
        [card_id],
    )
    .ok();
    conn.execute(
        "UPDATE auto_queue_entries SET status = 'skipped', completed_at = datetime('now') \
         WHERE kanban_card_id = ?1 AND status = 'pending' \
         AND run_id IN (SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused'))",
        [card_id],
    )
    .ok();
}

/// Skip live auto-queue entries for a card after PMD explicitly backs the card out.
///
/// Only active/paused runs are touched. Generated or future runs stay intact so
/// PMD can intentionally re-queue the card later after fixing prerequisites.
pub(crate) fn skip_live_auto_queue_entries_for_card_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE auto_queue_entries \
         SET status = 'skipped', dispatch_id = NULL, dispatched_at = NULL, completed_at = datetime('now') \
         WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched') \
         AND run_id IN (SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused'))",
        [card_id],
    )
}

/// Same as `review_state_sync` but operates on an already-acquired connection.
/// Use this inside transactions or when a lock is already held (#158).
pub fn review_state_sync_on_conn(conn: &rusqlite::Connection, json_str: &str) -> String {
    let params: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid JSON: {}"}}"#, e),
    };

    let card_id = params["card_id"].as_str().unwrap_or("");
    let state = params["state"].as_str().unwrap_or("");
    if card_id.is_empty() || state.is_empty() {
        return r#"{"error":"card_id and state are required"}"#.to_string();
    }

    // Special case: clear_verdict only NULLs last_verdict without changing state
    if state == "clear_verdict" {
        let result = conn.execute(
            "UPDATE card_review_state SET last_verdict = NULL, updated_at = datetime('now') WHERE card_id = ?1",
            rusqlite::params![card_id],
        );
        return match result {
            Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
            Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
        };
    }

    // Build dynamic SET clause based on provided fields
    let review_round = params["review_round"].as_i64();
    let last_verdict = params["last_verdict"].as_str();
    let last_decision = params["last_decision"].as_str();
    let pending_dispatch_id = params["pending_dispatch_id"].as_str();
    let approach_change_round = params["approach_change_round"].as_i64();
    let review_entered_at = params["review_entered_at"].as_str();

    // UPSERT: INSERT OR REPLACE with all fields
    let result = conn.execute(
        "INSERT INTO card_review_state (card_id, state, review_round, last_verdict, last_decision, pending_dispatch_id, approach_change_round, review_entered_at, updated_at) \
         VALUES (?1, ?2, COALESCE(?3, 0), ?4, ?5, ?6, ?7, COALESCE(?8, CASE WHEN ?2 = 'reviewing' THEN datetime('now') ELSE NULL END), datetime('now')) \
         ON CONFLICT(card_id) DO UPDATE SET \
         state = ?2, \
         review_round = COALESCE(?3, review_round), \
         last_verdict = COALESCE(?4, last_verdict), \
         last_decision = COALESCE(?5, last_decision), \
         pending_dispatch_id = CASE \
             WHEN ?6 IS NOT NULL THEN ?6 \
             WHEN ?2 = 'suggestion_pending' THEN pending_dispatch_id \
             ELSE NULL \
         END, \
         approach_change_round = COALESCE(?7, approach_change_round), \
         review_entered_at = COALESCE(?8, CASE WHEN ?2 = 'reviewing' THEN datetime('now') ELSE review_entered_at END), \
         updated_at = datetime('now')",
        rusqlite::params![
            card_id,
            state,
            review_round,
            last_verdict,
            last_decision,
            pending_dispatch_id,
            approach_change_round,
            review_entered_at,
        ],
    );

    match result {
        Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
        Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
    }
}

// ── Exec ops ──────────────────────────────────────────────────────
//
// agentdesk.exec(command, args) → stdout string
// Runs a local command synchronously. Limited to safe commands.

fn exec_override_env_var(cmd: &str) -> String {
    format!(
        "AGENTDESK_{}_PATH",
        cmd.replace('-', "_").to_ascii_uppercase()
    )
}

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
            let command_path = std::env::var_os(exec_override_env_var(&cmd))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| std::ffi::OsString::from(&cmd));
            match std::process::Command::new(&command_path)
                .args(&args)
                .output()
            {
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
                for provider in &["claude", "codex", "gemini", "qwen"] {
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
                                        // session_key, dispatch_id → for long-turn detection (#130)
                                        let channel_name = data.get("channel_name").and_then(|v| v.as_str()).unwrap_or("");
                                        let tmux_name = data.get("tmux_session_name").and_then(|v| v.as_str()).unwrap_or("");
                                        results.push(serde_json::json!({
                                            "channel_id": channel_id,
                                            "provider": provider,
                                            "started_at": data.get("started_at").and_then(|v| v.as_str()).unwrap_or(""),
                                            "channel_name": channel_name,
                                            "tmux_session_name": tmux_name,
                                            "session_id": data.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
                                            "session_key": data.get("session_key").and_then(|v| v.as_str()).unwrap_or(""),
                                            "dispatch_id": data.get("dispatch_id").and_then(|v| v.as_str()).unwrap_or(""),
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
                // session_key may be "hostname:tmux_name"
                let tmux_name = session_key
                    .split_once(':')
                    .map(|(_, name)| name)
                    .unwrap_or(&session_key);
                match crate::services::platform::tmux::send_keys(tmux_name, &[&command, "Enter"]) {
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
            crate::services::termination_audit::record_termination_for_tmux(
                tmux_name,
                None,
                "policy_engine",
                "session_kill_api",
                Some("force-kill via agentdesk.session.kill()"),
                None,
            );
            match crate::services::platform::tmux::kill_session_output(tmux_name) {
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

// ── Pipeline ops ─────────────────────────────────────────────────
//
// Exposes pipeline config to JS policies so they can look up transitions,
// terminal states, etc. instead of hardcoding state names.

fn register_pipeline_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let pipeline_obj = Object::new(ctx.clone())?;

    // __getConfigRaw(): returns the full default pipeline config as JSON
    pipeline_obj.set(
        "__getConfigRaw",
        Function::new(ctx.clone(), || -> String {
            crate::pipeline::ensure_loaded();
            match crate::pipeline::try_get() {
                Some(p) => {
                    serde_json::to_string(&p.to_json()).unwrap_or_else(|_| "null".to_string())
                }
                None => "null".to_string(),
            }
        })?,
    )?;

    // __resolveForCardRaw(cardId): returns the effective pipeline for a card
    let db_resolve = db;
    pipeline_obj.set(
        "__resolveForCardRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            crate::pipeline::ensure_loaded();
            let conn = match db_resolve.separate_conn() {
                Ok(c) => c,
                Err(_) => {
                    return crate::pipeline::try_get()
                        .map(|p| {
                            serde_json::to_string(&p.to_json())
                                .unwrap_or_else(|_| "null".to_string())
                        })
                        .unwrap_or_else(|| "null".to_string());
                }
            };
            let repo_id: Option<String> = conn
                .query_row(
                    "SELECT repo_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let effective =
                crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
            serde_json::to_string(&effective.to_json()).unwrap_or_else(|_| "null".to_string())
        })?,
    )?;

    ad.set("pipeline", pipeline_obj)?;

    // JS wrapper with convenience methods
    ctx.eval::<(), _>(r#"
        (function() {
            var rawConfig = agentdesk.pipeline.__getConfigRaw;
            var rawResolve = agentdesk.pipeline.__resolveForCardRaw;

            agentdesk.pipeline.getConfig = function() {
                return JSON.parse(rawConfig());
            };

            agentdesk.pipeline.resolveForCard = function(cardId) {
                return JSON.parse(rawResolve(cardId));
            };

            agentdesk.pipeline.isTerminal = function(state, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return state === "done";
                for (var i = 0; i < cfg.states.length; i++) {
                    if (cfg.states[i].id === state && cfg.states[i].terminal) return true;
                }
                return false;
            };

            agentdesk.pipeline.terminalState = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return "done";
                for (var i = 0; i < cfg.states.length; i++) {
                    if (cfg.states[i].terminal) return cfg.states[i].id;
                }
                return "done";
            };

            agentdesk.pipeline.initialState = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return "backlog";
                for (var i = 0; i < cfg.states.length; i++) {
                    if (!cfg.states[i].terminal) return cfg.states[i].id;
                }
                return "backlog";
            };

            // kickoffState: the first gated-inbound state (dispatch entry, e.g. "requested").
            agentdesk.pipeline.kickoffState = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states || !cfg.transitions) return "requested";
                for (var si = 0; si < cfg.states.length; si++) {
                    var s = cfg.states[si];
                    if (s.terminal) continue;
                    var hasGatedOut = false, allInboundFree = true;
                    for (var ti = 0; ti < cfg.transitions.length; ti++) {
                        var t = cfg.transitions[ti];
                        if (t.from === s.id && t.type === "gated") hasGatedOut = true;
                        if (t.to === s.id && t.type !== "free") allInboundFree = false;
                    }
                    if (hasGatedOut && allInboundFree) {
                        for (var ti2 = 0; ti2 < cfg.transitions.length; ti2++) {
                            var t2 = cfg.transitions[ti2];
                            if (t2.from === s.id && t2.type === "gated") return t2.to;
                        }
                    }
                }
                return "requested";
            };

            agentdesk.pipeline.findTransition = function(from, to, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.transitions) return null;
                for (var i = 0; i < cfg.transitions.length; i++) {
                    var t = cfg.transitions[i];
                    if (t.from === from && t.to === to) return t;
                }
                return null;
            };

            agentdesk.pipeline.nextGatedTarget = function(from, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.transitions) return null;
                for (var i = 0; i < cfg.transitions.length; i++) {
                    var t = cfg.transitions[i];
                    if (t.from === from && t.type === "gated") return t.to;
                }
                return null;
            };

            agentdesk.pipeline.nextGatedTargetWithGate = function(from, gateName, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.transitions) return null;
                for (var i = 0; i < cfg.transitions.length; i++) {
                    var t = cfg.transitions[i];
                    if (t.from === from && t.type === "gated" && t.gates && t.gates.indexOf(gateName) >= 0) {
                        return t.to;
                    }
                }
                return null;
            };

            agentdesk.pipeline.forceOnlyTargets = function(from, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.transitions) return [];
                var targets = [];
                for (var i = 0; i < cfg.transitions.length; i++) {
                    var t = cfg.transitions[i];
                    if (t.from === from && t.type === "force_only") targets.push(t.to);
                }
                return targets;
            };

            agentdesk.pipeline.getTimeout = function(state, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.timeouts) return null;
                return cfg.timeouts[state] || null;
            };

            agentdesk.pipeline.hasState = function(state, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return false;
                for (var i = 0; i < cfg.states.length; i++) {
                    if (cfg.states[i].id === state) return true;
                }
                return false;
            };

            agentdesk.pipeline.dispatchableStates = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return [];
                var result = [];
                for (var i = 0; i < cfg.states.length; i++) {
                    if (cfg.states[i].dispatchable) result.push(cfg.states[i].id);
                }
                return result;
            };
        })();
    "#)?;

    Ok(())
}

// ── DM reply tracking ops ────────────────────────────────────────
// agentdesk.dmReply.register(sourceAgent, userId, context, ttlSeconds?)
// agentdesk.dmReply.consume(userId)
// agentdesk.dmReply.pending(userId)

fn register_dm_reply_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let dm_obj = Object::new(ctx.clone())?;

    // __register_raw(source_agent, user_id, channel_id, context, ttl_seconds) → json
    let db_reg = db.clone();
    dm_obj.set(
        "__register_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(
                move |source_agent: String,
                      user_id: String,
                      channel_id: String,
                      context: String,
                      ttl_seconds: i64|
                      -> String {
                    dm_reply_register_raw(
                        &db_reg,
                        &source_agent,
                        &user_id,
                        &channel_id,
                        &context,
                        ttl_seconds,
                    )
                },
            ),
        )?,
    )?;

    // __consume_raw(user_id) → json
    let db_con = db.clone();
    dm_obj.set(
        "__consume_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_consume_raw(&db_con, &user_id)
            }),
        )?,
    )?;

    // __pending_raw(user_id) → json
    let db_pend = db.clone();
    dm_obj.set(
        "__pending_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_pending_raw(&db_pend, &user_id)
            }),
        )?,
    )?;

    // __read_consumed_raw(user_id) → json (most recent consumed entry with _answer)
    let db_read = db.clone();
    dm_obj.set(
        "__read_consumed_raw",
        Function::new(
            ctx.clone(),
            rquickjs::function::MutFn::from(move |user_id: String| -> String {
                dm_reply_read_consumed_raw(&db_read, &user_id)
            }),
        )?,
    )?;

    ad.set("dmReply", dm_obj)?;

    ctx.eval::<(), _>(
        r#"
        agentdesk.dmReply.register = function(sourceAgent, userId, context, ttlSeconds) {
            var channelId = (context && context.channelId) ? String(context.channelId) : "";
            return JSON.parse(agentdesk.dmReply.__register_raw(
                sourceAgent || "",
                String(userId || ""),
                channelId,
                JSON.stringify(context || {}),
                ttlSeconds || 3600
            ));
        };
        agentdesk.dmReply.consume = function(userId) {
            return JSON.parse(agentdesk.dmReply.__consume_raw(String(userId || "")));
        };
        agentdesk.dmReply.pending = function(userId) {
            return JSON.parse(agentdesk.dmReply.__pending_raw(String(userId || "")));
        };
        agentdesk.dmReply.readConsumed = function(userId) {
            return JSON.parse(agentdesk.dmReply.__read_consumed_raw(String(userId || "")));
        };
        "#,
    )?;

    Ok(())
}

fn dm_reply_register_raw(
    db: &Db,
    source_agent: &str,
    user_id: &str,
    channel_id: &str,
    context: &str,
    ttl_seconds: i64,
) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    let expires_at = if ttl_seconds > 0 {
        format!("datetime('now', '+{ttl_seconds} seconds')")
    } else {
        "NULL".to_string()
    };
    let ch = if channel_id.is_empty() {
        None
    } else {
        Some(channel_id)
    };
    let sql = format!(
        "INSERT INTO pending_dm_replies (source_agent, user_id, channel_id, context, expires_at) \
         VALUES (?1, ?2, ?3, ?4, {expires_at})"
    );
    match conn.execute(&sql, rusqlite::params![source_agent, user_id, ch, context]) {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 📩 dmReply.register → user={user_id} agent={source_agent} (id={id})"
            );
            format!(r#"{{"ok":true,"id":{id}}}"#)
        }
        Err(e) => format!(r#"{{"error":"insert failed: {e}"}}"#),
    }
}

fn dm_reply_consume_raw(db: &Db, user_id: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    // FIFO: consume the oldest pending entry for this user that hasn't expired
    let result: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'pending' \
         AND (expires_at IS NULL OR expires_at > datetime('now')) \
         ORDER BY created_at ASC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    match result {
        Ok((id, source_agent, context, channel_id)) => {
            // CAS: only mark consumed if still pending (guards against race)
            let updated = conn.execute(
                "UPDATE pending_dm_replies SET status = 'consumed', consumed_at = datetime('now') \
                 WHERE id = ?1 AND status = 'pending'",
                rusqlite::params![id],
            );
            match updated {
                Ok(0) => {
                    return r#"{"ok":false,"reason":"already_consumed"}"#.to_string();
                }
                Err(e) => {
                    return format!(r#"{{"error":"update failed: {e}"}}"#);
                }
                _ => {}
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ✉️ dmReply.consume → user={user_id} agent={source_agent} (id={id})");
            let ctx: serde_json::Value =
                serde_json::from_str(&context).unwrap_or(serde_json::json!({}));
            let mut resp = serde_json::json!({
                "ok": true,
                "id": id,
                "sourceAgent": source_agent,
                "context": ctx,
            });
            if let Some(ch) = channel_id {
                resp["channelId"] = serde_json::Value::String(ch);
            }
            serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":"serialize"}"#.to_string())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            r#"{"ok":false,"reason":"no_pending"}"#.to_string()
        }
        Err(e) => format!(r#"{{"error":"query failed: {e}"}}"#),
    }
}

fn dm_reply_pending_raw(db: &Db, user_id: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    let result: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'pending' \
         AND (expires_at IS NULL OR expires_at > datetime('now')) \
         ORDER BY created_at ASC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    match result {
        Ok((id, source_agent, context, channel_id)) => {
            let ctx: serde_json::Value =
                serde_json::from_str(&context).unwrap_or(serde_json::json!({}));
            let mut resp = serde_json::json!({
                "ok": true,
                "id": id,
                "sourceAgent": source_agent,
                "context": ctx,
            });
            if let Some(ch) = channel_id {
                resp["channelId"] = serde_json::Value::String(ch);
            }
            serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":"serialize"}"#.to_string())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => r#"{"ok":false}"#.to_string(),
        Err(e) => format!(r#"{{"error":"query failed: {e}"}}"#),
    }
}

fn dm_reply_read_consumed_raw(db: &Db, user_id: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db connection: {e}"}}"#),
    };
    let result: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'consumed' \
         ORDER BY consumed_at DESC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    match result {
        Ok((id, source_agent, context, channel_id)) => {
            let ctx: serde_json::Value =
                serde_json::from_str(&context).unwrap_or(serde_json::json!({}));
            let mut resp = serde_json::json!({
                "ok": true,
                "id": id,
                "sourceAgent": source_agent,
                "context": ctx,
            });
            if let Some(ch) = channel_id {
                resp["channelId"] = serde_json::Value::String(ch);
            }
            serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":"serialize"}"#.to_string())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => r#"{"ok":false}"#.to_string(),
        Err(e) => format!(r#"{{"error":"query failed: {e}"}}"#),
    }
}

// ── Agent channel resolution ops (#304) ─────────────────────────
//
// Exposes Rust channel resolution logic to JS policies so they don't
// query legacy columns directly.

fn register_agent_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let agents_obj = Object::new(ctx.clone())?;

    // __resolvePrimaryChannel(agentId) -> channelId | ""
    let db_primary = db.clone();
    agents_obj.set(
        "__resolvePrimaryChannel",
        Function::new(ctx.clone(), move |agent_id: String| -> String {
            match db_primary.separate_conn() {
                Ok(conn) => {
                    match crate::db::agents::resolve_agent_primary_channel_on_conn(&conn, &agent_id)
                    {
                        Ok(Some(ch)) => ch,
                        _ => String::new(),
                    }
                }
                Err(_) => String::new(),
            }
        })?,
    )?;

    // __resolveCounterModelChannel(agentId) -> channelId | ""
    let db_counter = db.clone();
    agents_obj.set(
        "__resolveCounterModelChannel",
        Function::new(ctx.clone(), move |agent_id: String| -> String {
            match db_counter.separate_conn() {
                Ok(conn) => {
                    match crate::db::agents::resolve_agent_counter_model_channel_on_conn(
                        &conn, &agent_id,
                    ) {
                        Ok(Some(ch)) => ch,
                        _ => String::new(),
                    }
                }
                Err(_) => String::new(),
            }
        })?,
    )?;

    // __resolveDispatchChannel(agentId, dispatchType) -> channelId | ""
    let db_dispatch = db;
    agents_obj.set(
        "__resolveDispatchChannel",
        Function::new(
            ctx.clone(),
            move |agent_id: String, dispatch_type: String| -> String {
                match db_dispatch.separate_conn() {
                    Ok(conn) => {
                        let dtype = if dispatch_type.is_empty() {
                            None
                        } else {
                            Some(dispatch_type.as_str())
                        };
                        match crate::db::agents::resolve_agent_dispatch_channel_on_conn(
                            &conn, &agent_id, dtype,
                        ) {
                            Ok(Some(ch)) => ch,
                            _ => String::new(),
                        }
                    }
                    Err(_) => String::new(),
                }
            },
        )?,
    )?;

    ad.set("agents", agents_obj)?;

    // JS convenience wrappers
    ctx.eval::<(), _>(
        r#"
(function() {
    var a = agentdesk.agents;
    a.resolvePrimaryChannel = function(agentId) {
        var ch = a.__resolvePrimaryChannel(agentId);
        return ch || null;
    };
    a.resolveCounterModelChannel = function(agentId) {
        var ch = a.__resolveCounterModelChannel(agentId);
        return ch || null;
    };
    a.resolveDispatchChannel = function(agentId, dispatchType) {
        var ch = a.__resolveDispatchChannel(agentId, dispatchType || "");
        return ch || null;
    };
})();
"#,
    )?;

    Ok(())
}
