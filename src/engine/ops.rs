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
    register_dispatch_ops(ctx, db)?;

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

    let conn = match db.lock() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"__error":"db lock: {e}"}}"#),
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

    let conn = match db.lock() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"__error":"db lock: {e}"}}"#),
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
                let conn = match db_c.lock() {
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
// Only allows 127.0.0.1 targets for security.

fn register_http_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let http_obj = Object::new(ctx.clone())?;

    http_obj.set(
        "__post_raw",
        Function::new(ctx.clone(), |url: String, body_json: String| -> String {
            if !url.starts_with("http://127.0.0.1") {
                return r#"{"error":"only localhost allowed"}"#.to_string();
            }
            match ureq::post(&url)
                .set("Content-Type", "application/json")
                .send_string(&body_json)
            {
                Ok(resp) => resp.into_string().unwrap_or_else(|_| "{}".to_string()),
                Err(e) => format!(r#"{{"error":"{}"}}"#, e),
            }
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
                // deadlocks the tokio runtime because the health server shares the same runtime.
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
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    let conn = match db.lock() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"DB lock: {}"}}"#, e),
    };

    // Insert dispatch
    if let Err(e) = conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, '{}', datetime('now'), datetime('now'))",
        rusqlite::params![dispatch_id, card_id, agent_id, dispatch_type, title],
    ) {
        return format!(r#"{{"error":"INSERT dispatch: {}"}}"#, e);
    }

    // Update kanban card — only set status to 'requested' for non-review dispatches.
    // Review/rework dispatches should not change the card status (it stays in 'review').
    let is_review = dispatch_type == "review" || dispatch_type == "review-decision" || dispatch_type == "rework";
    let sql = if is_review {
        "UPDATE kanban_cards SET latest_dispatch_id = ?1, updated_at = datetime('now') WHERE id = ?2"
    } else {
        "UPDATE kanban_cards SET latest_dispatch_id = ?1, status = 'requested', requested_at = datetime('now'), updated_at = datetime('now') WHERE id = ?2"
    };
    if let Err(e) = conn.execute(sql, rusqlite::params![dispatch_id, card_id]) {
        return format!(r#"{{"error":"UPDATE card: {}"}}"#, e);
    }

    // Get issue URL for Discord message
    let issue_url: Option<String> = conn
        .query_row(
            "SELECT github_issue_url FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    #[test]
    fn test_engine_db_query_op() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
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

        let conn = db.lock().unwrap();
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
            let conn = db.lock().unwrap();
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
            let conn = db.lock().unwrap();
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
}
