use crate::db::Db;
use crate::engine::sql_guard::detect_core_table_write;
use crate::error::{AppError, ErrorCode};
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── DB ops ───────────────────────────────────────────────────────
//
// We use a JSON-string bridge: Rust receives (sql, params_json_string)
// and returns a json_string. A thin JS wrapper does JSON.parse/stringify.

pub(super) fn register_db_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
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

    let guard_raw = Function::new(ctx.clone(), move |sql: String| -> String {
        db_guard_raw(&sql, "agentdesk.db.execute")
    })?;
    db_obj.set("__guard_raw", guard_raw)?;

    ad.set("db", db_obj)?;

    // JS wrappers that do JSON marshaling
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            agentdesk.db.query = function(sql, params) {
                var result = JSON.parse(
                    agentdesk.db.__query_raw(sql, JSON.stringify(params || []))
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.db.execute = function(sql, params) {
                var guard = JSON.parse(agentdesk.db.__guard_raw(sql));
                if (guard.blocked) {
                    agentdesk.log.warn(guard.warning);
                    throw new Error(guard.error);
                }
                // Direct write — db.execute remains synchronous by design.
                // dispatch.create and kanban.setStatus use intent/transition model;
                // converting db.execute to intents requires typed intents for each
                // mutation pattern (card_review_state, kv_meta, agents, etc.).
                var result = JSON.parse(
                    agentdesk.db.__execute_raw(sql, JSON.stringify(params || []))
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        undefined;
    "#,
    )?;

    Ok(())
}

fn db_guard_raw(sql: &str, origin: &str) -> String {
    match detect_core_table_write(sql) {
        Some(violation) => serde_json::json!({
            "blocked": true,
            "error": violation.error_message(),
            "warning": violation.warning_message(origin, sql),
        })
        .to_string(),
        None => r#"{"blocked":false}"#.to_string(),
    }
}

fn db_query_raw(db: &Db, sql: &str, params_json: &str) -> String {
    let params: Vec<serde_json::Value> =
        match parse_params_json(params_json, "agentdesk.db.query.parse_params", sql) {
            Ok(params) => params,
            Err(error_json) => return error_json,
        };
    let bind: Vec<rusqlite::types::Value> = params.iter().map(json_to_sqlite).collect();

    // Use a separate read-only connection to avoid blocking the write Mutex.
    // This prevents deadlock when onTick (holding engine lock) queries DB
    // while request handlers hold the write lock.
    let conn = match db.read_conn() {
        Ok(c) => c,
        Err(e) => {
            return AppError::internal(format!("db read: {e}"))
                .with_code(ErrorCode::Database)
                .with_operation("agentdesk.db.query.read_conn")
                .with_context("sql", compact_sql(sql))
                .into_policy_json_string();
        }
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            return AppError::internal(format!("prepare: {e}"))
                .with_code(ErrorCode::Database)
                .with_operation("agentdesk.db.query.prepare")
                .with_context("sql", compact_sql(sql))
                .into_policy_json_string();
        }
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
        for (i, col_name) in col_names.iter().enumerate() {
            let val: rusqlite::types::Value = row.get(i)?;
            let jv = sqlite_to_json(&val);
            map.insert(col_name.clone(), jv);
        }
        Ok(serde_json::Value::Object(map))
    }) {
        Ok(r) => r,
        Err(e) => {
            return AppError::internal(format!("query: {e}"))
                .with_code(ErrorCode::Database)
                .with_operation("agentdesk.db.query.query_map")
                .with_context("sql", compact_sql(sql))
                .into_policy_json_string();
        }
    };

    let result: Vec<serde_json::Value> = match rows.collect::<Result<Vec<_>, _>>() {
        Ok(result) => result,
        Err(error) => {
            return AppError::internal(format!("row decode: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("agentdesk.db.query.collect_rows")
                .with_context("sql", compact_sql(sql))
                .into_policy_json_string();
        }
    };
    serde_json::to_string(&result).unwrap_or_else(|error| {
        AppError::internal(format!("serialize query result: {error}"))
            .with_code(ErrorCode::Policy)
            .with_operation("agentdesk.db.query.serialize")
            .with_context("sql", compact_sql(sql))
            .into_policy_json_string()
    })
}

fn db_execute_raw(db: &Db, sql: &str, params_json: &str) -> String {
    if let Some(violation) = detect_core_table_write(sql) {
        return serde_json::json!({ "__error": violation.error_message() }).to_string();
    }

    let params: Vec<serde_json::Value> =
        match parse_params_json(params_json, "agentdesk.db.execute.parse_params", sql) {
            Ok(params) => params,
            Err(error_json) => return error_json,
        };
    let bind: Vec<rusqlite::types::Value> = params.iter().map(json_to_sqlite).collect();

    // Use a separate read-write connection to avoid holding the main
    // Rust Mutex that request handlers need. SQLite WAL serializes
    // concurrent writers via busy_timeout (5s).
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return AppError::internal(format!("db conn: {e}"))
                .with_code(ErrorCode::Database)
                .with_operation("agentdesk.db.execute.separate_conn")
                .with_context("sql", compact_sql(sql))
                .into_policy_json_string();
        }
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = bind
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let changes = match conn.execute(sql, params_ref.as_slice()) {
        Ok(n) => n,
        Err(e) => {
            return AppError::internal(format!("execute: {e}"))
                .with_code(ErrorCode::Database)
                .with_operation("agentdesk.db.execute.execute")
                .with_context("sql", compact_sql(sql))
                .into_policy_json_string();
        }
    };

    format!(r#"{{"changes":{changes}}}"#)
}

fn parse_params_json(
    params_json: &str,
    operation: &str,
    sql: &str,
) -> Result<Vec<serde_json::Value>, String> {
    serde_json::from_str(params_json).map_err(|error| {
        AppError::bad_request(format!("invalid params_json: {error}"))
            .with_code(ErrorCode::Policy)
            .with_operation(operation)
            .with_context("sql", compact_sql(sql))
            .into_policy_json_string()
    })
}

fn compact_sql(sql: &str) -> String {
    const MAX_SQL_CONTEXT_LEN: usize = 120;

    let compact = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= MAX_SQL_CONTEXT_LEN {
        compact
    } else {
        format!("{}...", &compact[..MAX_SQL_CONTEXT_LEN])
    }
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
