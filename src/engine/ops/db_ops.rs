use crate::db::Db;
use crate::engine::sql_guard::detect_core_table_write;
use crate::error::{AppError, ErrorCode};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgArguments, PgPool, PgTypeInfo};
use sqlx::{Column, Encode, Postgres, Row, Type, TypeInfo};
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

// ── DB ops ───────────────────────────────────────────────────────
//
// We use a JSON-string bridge: Rust receives (sql, params_json_string)
// and returns a json_string. A thin JS wrapper does JSON.parse/stringify.

const POLICY_DB_WARN_THRESHOLD: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
enum JsonColumnMode {
    LegacyString,
    Typed,
}

pub(super) fn register_db_ops<'js>(
    ctx: &Ctx<'js>,
    legacy_db: Option<Db>,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let db_obj = Object::new(ctx.clone())?;

    // Internal: __db_query_raw(sql, params_json) → json_string
    let legacy_q = legacy_db.clone();
    let pg_q = pg_pool.clone();
    let query_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(move |sql: String, params_json: String| -> String {
            db_query_raw(legacy_q.as_ref(), pg_q.clone(), &sql, &params_json)
        }),
    )?;
    db_obj.set("__query_raw", query_raw)?;

    // Internal: __db_query_json_raw(sql, params_json) → json_string
    let legacy_q_json = legacy_db.clone();
    let pg_q_json = pg_pool.clone();
    let query_json_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(move |sql: String, params_json: String| -> String {
            db_query_json_raw(
                legacy_q_json.as_ref(),
                pg_q_json.clone(),
                &sql,
                &params_json,
            )
        }),
    )?;
    db_obj.set("__query_json_raw", query_json_raw)?;

    // Internal: __db_execute_raw(sql, params_json) → json_string
    let legacy_e = legacy_db.clone();
    let pg_e = pg_pool.clone();
    let execute_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(move |sql: String, params_json: String| -> String {
            db_execute_raw(legacy_e.as_ref(), pg_e.clone(), &sql, &params_json)
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
            agentdesk.db.queryJson = function(sql, params) {
                var result = JSON.parse(
                    agentdesk.db.__query_json_raw(sql, JSON.stringify(params || []))
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

fn db_query_raw(
    legacy_db: Option<&Db>,
    pg_pool: Option<PgPool>,
    sql: &str,
    params_json: &str,
) -> String {
    db_query_raw_with_json_mode(
        legacy_db,
        pg_pool,
        sql,
        params_json,
        JsonColumnMode::LegacyString,
        "agentdesk.db.query",
    )
}

fn db_query_json_raw(
    legacy_db: Option<&Db>,
    pg_pool: Option<PgPool>,
    sql: &str,
    params_json: &str,
) -> String {
    db_query_raw_with_json_mode(
        legacy_db,
        pg_pool,
        sql,
        params_json,
        JsonColumnMode::Typed,
        "agentdesk.db.queryJson",
    )
}

fn db_query_raw_with_json_mode(
    _legacy_db: Option<&Db>,
    pg_pool: Option<PgPool>,
    sql: &str,
    params_json: &str,
    json_column_mode: JsonColumnMode,
    origin: &str,
) -> String {
    let started = std::time::Instant::now();
    emit_raw_db_audit(origin, sql);
    let parse_operation = format!("{origin}.parse_params");
    let params: Vec<serde_json::Value> = match parse_params_json(params_json, &parse_operation, sql)
    {
        Ok(params) => params,
        Err(error_json) => return error_json,
    };

    let Some(pg_pool) = pg_pool else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        if let Some(db) = _legacy_db {
            return db_query_raw_sqlite(db, sql, &params, started);
        }
        let backend_operation = format!("{origin}.pg_backend");
        return policy_db_error_json(
            &backend_operation,
            sql,
            format!("postgres backend is required for {origin}"),
        );
    };

    db_query_raw_pg_with_json_mode(&pg_pool, sql, &params, started, json_column_mode, origin)
}

#[cfg(test)]
fn db_query_raw_pg(
    pg_pool: &PgPool,
    sql: &str,
    params: &[serde_json::Value],
    started: std::time::Instant,
) -> String {
    db_query_raw_pg_with_json_mode(
        pg_pool,
        sql,
        params,
        started,
        JsonColumnMode::LegacyString,
        "agentdesk.db.query",
    )
}

fn db_query_raw_pg_with_json_mode(
    pg_pool: &PgPool,
    sql: &str,
    params: &[serde_json::Value],
    started: std::time::Instant,
    json_column_mode: JsonColumnMode,
    origin: &str,
) -> String {
    let prepared_sql = match prepare_policy_sql_for_pg(sql, params) {
        Ok(prepared) => prepared,
        Err(error) => {
            let operation = format!("{origin}.translate_pg");
            return policy_db_bad_request_json(&operation, sql, error);
        }
    };

    let pool = pg_pool.clone();
    let query_sql = prepared_sql.sql.clone();
    let bind_params = prepared_sql.params.clone();
    let rows = match run_async_bridge_pg(&pool, move |pool| async move {
        bind_policy_sql_params(sqlx::query(&query_sql), bind_params)
            .fetch_all(&pool)
            .await
            .map_err(|error| format!("query: {error}"))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            let operation = format!("{origin}.fetch_all_pg");
            return policy_db_error_json(&operation, sql, error);
        }
    };

    let mut result = Vec::with_capacity(rows.len());
    for row in &rows {
        match pg_row_to_json(row, json_column_mode) {
            Ok(value) => result.push(value),
            Err(error) => {
                let operation = format!("{origin}.collect_rows_pg");
                return policy_db_error_json(&operation, sql, error);
            }
        }
    }

    let elapsed = started.elapsed();
    if elapsed >= POLICY_DB_WARN_THRESHOLD {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis(),
            row_count = result.len(),
            sql = %compact_sql(sql),
            translated_sql = %compact_sql(&prepared_sql.sql),
            "policy db query slow"
        );
    }

    serde_json::to_string(&result).unwrap_or_else(|error| {
        let operation = format!("{origin}.serialize_pg");
        policy_db_error_json(&operation, sql, format!("serialize query result: {error}"))
    })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn db_query_raw_sqlite(
    db: &Db,
    sql: &str,
    params: &[serde_json::Value],
    started: std::time::Instant,
) -> String {
    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return policy_db_error_json(
                "agentdesk.db.query.sqlite_open",
                sql,
                format!("open sqlite connection: {error}"),
            );
        }
    };
    let values = sqlite_params(params);
    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            return policy_db_error_json(
                "agentdesk.db.query.sqlite_prepare",
                sql,
                format!("prepare sqlite query: {error}"),
            );
        }
    };
    let column_names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect();
    let mut rows = match stmt.query(sqlite_test::params_from_iter(values.iter())) {
        Ok(rows) => rows,
        Err(error) => {
            return policy_db_error_json(
                "agentdesk.db.query.sqlite_fetch",
                sql,
                format!("query sqlite rows: {error}"),
            );
        }
    };
    let mut result = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let mut object = serde_json::Map::new();
                for (idx, name) in column_names.iter().enumerate() {
                    let value = row
                        .get_ref(idx)
                        .map(sqlite_value_ref_to_json)
                        .unwrap_or(serde_json::Value::Null);
                    object.insert(name.clone(), value);
                }
                result.push(serde_json::Value::Object(object));
            }
            Ok(None) => break,
            Err(error) => {
                return policy_db_error_json(
                    "agentdesk.db.query.sqlite_collect",
                    sql,
                    format!("collect sqlite row: {error}"),
                );
            }
        }
    }

    let elapsed = started.elapsed();
    if elapsed >= POLICY_DB_WARN_THRESHOLD {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis(),
            row_count = result.len(),
            sql = %compact_sql(sql),
            "policy db query slow"
        );
    }

    serde_json::to_string(&result).unwrap_or_else(|error| {
        policy_db_error_json(
            "agentdesk.db.query.serialize_sqlite",
            sql,
            format!("serialize query result: {error}"),
        )
    })
}

fn db_execute_raw(
    _legacy_db: Option<&Db>,
    pg_pool: Option<PgPool>,
    sql: &str,
    params_json: &str,
) -> String {
    let started = std::time::Instant::now();
    emit_raw_db_audit("agentdesk.db.execute", sql);
    if let Some(violation) = detect_core_table_write(sql) {
        return serde_json::json!({ "error": violation.error_message() }).to_string();
    }

    let params: Vec<serde_json::Value> =
        match parse_params_json(params_json, "agentdesk.db.execute.parse_params", sql) {
            Ok(params) => params,
            Err(error_json) => return error_json,
        };

    let Some(pg_pool) = pg_pool else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        if let Some(db) = _legacy_db {
            return db_execute_raw_sqlite(db, sql, &params, started);
        }
        return policy_db_error_json(
            "agentdesk.db.execute.pg_backend",
            sql,
            "postgres backend is required for db.execute".to_string(),
        );
    };

    db_execute_raw_pg(&pg_pool, sql, &params, started)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn db_execute_raw_sqlite(
    db: &Db,
    sql: &str,
    params: &[serde_json::Value],
    started: std::time::Instant,
) -> String {
    let conn = match db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return policy_db_error_json(
                "agentdesk.db.execute.sqlite_open",
                sql,
                format!("open sqlite connection: {error}"),
            );
        }
    };
    let values = sqlite_params(params);
    match conn.execute(sql, sqlite_test::params_from_iter(values.iter())) {
        Ok(rows_affected) => {
            let elapsed = started.elapsed();
            if elapsed >= POLICY_DB_WARN_THRESHOLD {
                tracing::warn!(
                    elapsed_ms = elapsed.as_millis(),
                    rows_affected,
                    sql = %compact_sql(sql),
                    "policy db execute slow"
                );
            }
            serde_json::json!({ "rows_affected": rows_affected }).to_string()
        }
        Err(error) => policy_db_error_json(
            "agentdesk.db.execute.sqlite_execute",
            sql,
            format!("execute sqlite policy SQL: {error}"),
        ),
    }
}

pub(crate) fn execute_policy_sql(
    _db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    sql: &str,
    params: &[serde_json::Value],
) -> Result<u64, String> {
    emit_raw_db_audit("intent.execute_sql", sql);
    if let Some(violation) = detect_core_table_write(sql) {
        tracing::warn!("{}", violation.warning_message("ExecuteSQL intent", sql));
        return Err(violation.error_message().to_string());
    }

    let Some(pg_pool) = pg_pool else {
        return Err("postgres backend is required for ExecuteSQL intent".to_string());
    };

    let prepared_sql = prepare_policy_sql_for_pg(sql, params)?;
    let pool = pg_pool.clone();
    run_async_bridge_pg(&pool, move |pool| async move {
        bind_policy_sql_params(sqlx::query(&prepared_sql.sql), prepared_sql.params)
            .execute(&pool)
            .await
            .map(|result| result.rows_affected())
            .map_err(|error| format!("execute postgres policy SQL: {error}"))
    })
}

fn db_execute_raw_pg(
    pg_pool: &PgPool,
    sql: &str,
    params: &[serde_json::Value],
    started: std::time::Instant,
) -> String {
    let prepared_sql = match prepare_policy_sql_for_pg(sql, params) {
        Ok(prepared) => prepared,
        Err(error) => {
            return policy_db_bad_request_json("agentdesk.db.execute.translate_pg", sql, error);
        }
    };

    let pool = pg_pool.clone();
    let execute_sql = prepared_sql.sql.clone();
    let bind_params = prepared_sql.params.clone();
    let changes = match run_async_bridge_pg(&pool, move |pool| async move {
        bind_policy_sql_params(sqlx::query(&execute_sql), bind_params)
            .execute(&pool)
            .await
            .map(|result| result.rows_affected())
            .map_err(|error| format!("execute: {error}"))
    }) {
        Ok(changes) => changes,
        Err(error) => {
            return policy_db_error_json("agentdesk.db.execute.execute_pg", sql, error);
        }
    };

    let elapsed = started.elapsed();
    if elapsed >= POLICY_DB_WARN_THRESHOLD {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis(),
            changes,
            sql = %compact_sql(sql),
            translated_sql = %compact_sql(&prepared_sql.sql),
            "policy db execute slow"
        );
    }

    serde_json::json!({ "changes": changes }).to_string()
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn sqlite_params(params: &[serde_json::Value]) -> Vec<sqlite_test::types::Value> {
    params.iter().map(sqlite_param).collect()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn sqlite_param(value: &serde_json::Value) -> sqlite_test::types::Value {
    match value {
        serde_json::Value::Null => sqlite_test::types::Value::Null,
        serde_json::Value::Bool(value) => sqlite_test::types::Value::Integer(i64::from(*value)),
        serde_json::Value::Number(value) => {
            if let Some(int_value) = value.as_i64() {
                sqlite_test::types::Value::Integer(int_value)
            } else if let Some(float_value) = value.as_f64() {
                sqlite_test::types::Value::Real(float_value)
            } else {
                sqlite_test::types::Value::Text(value.to_string())
            }
        }
        serde_json::Value::String(value) => sqlite_test::types::Value::Text(value.clone()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            sqlite_test::types::Value::Text(value.to_string())
        }
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn sqlite_value_ref_to_json(value: sqlite_test::types::ValueRef<'_>) -> serde_json::Value {
    match value {
        sqlite_test::types::ValueRef::Null => serde_json::Value::Null,
        sqlite_test::types::ValueRef::Integer(value) => serde_json::json!(value),
        sqlite_test::types::ValueRef::Real(value) => serde_json::json!(value),
        sqlite_test::types::ValueRef::Text(value) => {
            serde_json::Value::String(String::from_utf8_lossy(value).to_string())
        }
        sqlite_test::types::ValueRef::Blob(value) => {
            serde_json::Value::String(format!("{value:?}"))
        }
    }
}

fn policy_db_error_json(operation: &str, sql: &str, message: impl Into<String>) -> String {
    AppError::internal(message.into())
        .with_code(ErrorCode::Database)
        .with_operation(operation)
        .with_context("sql", compact_sql(sql))
        .into_policy_json_string()
}

fn policy_db_bad_request_json(operation: &str, sql: &str, message: impl Into<String>) -> String {
    AppError::bad_request(message.into())
        .with_code(ErrorCode::Policy)
        .with_operation(operation)
        .with_context("sql", compact_sql(sql))
        .into_policy_json_string()
}

#[derive(Debug, Clone, PartialEq)]
struct PreparedPolicySql {
    sql: String,
    params: Vec<serde_json::Value>,
}

fn prepare_policy_sql_for_pg(
    sql: &str,
    params: &[serde_json::Value],
) -> Result<PreparedPolicySql, String> {
    let translated = translate_insert_with_conflict(sql)?;
    let translated = translate_sqlite_rowid(&translated);
    translate_policy_sql_placeholders(&translated, params)
}

fn translate_insert_with_conflict(sql: &str) -> Result<String, String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if let Some(translated) = rewrite_insert_conflict(trimmed, ConflictMode::Replace)? {
        return Ok(translated);
    }
    if let Some(translated) = rewrite_insert_conflict(trimmed, ConflictMode::Ignore)? {
        return Ok(translated);
    }
    Ok(trimmed.to_string())
}

#[derive(Clone, Copy)]
enum ConflictMode {
    Replace,
    Ignore,
}

fn rewrite_insert_conflict(sql: &str, mode: ConflictMode) -> Result<Option<String>, String> {
    let prefix = match mode {
        ConflictMode::Replace => "INSERT OR REPLACE INTO",
        ConflictMode::Ignore => "INSERT OR IGNORE INTO",
    };

    let Some(mut rest) = strip_prefix_ci(sql, prefix) else {
        return Ok(None);
    };
    rest = rest.trim_start();

    let table_end = rest
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace() || *ch == '(')
        .map(|(idx, _)| idx)
        .unwrap_or(rest.len());
    let table_name = rest[..table_end].trim();
    if table_name.is_empty() {
        return Err(format!("{prefix} requires a table name"));
    }

    rest = rest[table_end..].trim_start();
    if !rest.starts_with('(') {
        return Err(format!("{prefix} requires an explicit column list"));
    }
    let columns_end = find_matching_paren(rest, 0)
        .ok_or_else(|| format!("{prefix} has an unmatched column list"))?;
    let columns_raw = &rest[1..columns_end];

    rest = rest[columns_end + 1..].trim_start();
    let Some(values_rest) = strip_prefix_ci(rest, "VALUES") else {
        return Err(format!("{prefix} only supports VALUES clauses"));
    };
    let values_rest = values_rest.trim_start();
    if !values_rest.starts_with('(') {
        return Err(format!("{prefix} requires a VALUES tuple"));
    }
    let values_end = find_matching_paren(values_rest, 0)
        .ok_or_else(|| format!("{prefix} has an unmatched VALUES tuple"))?;
    let values_raw = &values_rest[1..values_end];
    if !values_rest[values_end + 1..].trim().is_empty() {
        return Err(format!("{prefix} only supports a single VALUES tuple"));
    }

    let columns = split_identifier_list(columns_raw);
    if columns.is_empty() {
        return Err(format!("{prefix} requires at least one column"));
    }
    let conflict_target = columns[0].clone();

    let normalized_columns = columns.join(", ");
    let normalized_insert =
        format!("INSERT INTO {table_name} ({normalized_columns}) VALUES ({values_raw})");

    let translated = match mode {
        ConflictMode::Ignore => format!("{normalized_insert} ON CONFLICT DO NOTHING"),
        ConflictMode::Replace => {
            let assignments: Vec<String> = columns
                .iter()
                .skip(1)
                .map(|column| format!("{column} = EXCLUDED.{column}"))
                .collect();
            if assignments.is_empty() {
                format!("{normalized_insert} ON CONFLICT ({conflict_target}) DO NOTHING")
            } else {
                format!(
                    "{normalized_insert} ON CONFLICT ({conflict_target}) DO UPDATE SET {}",
                    assignments.join(", ")
                )
            }
        }
    };

    Ok(Some(translated))
}

fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let prefix_len = prefix.len();
    if value.len() < prefix_len {
        return None;
    }
    if value[..prefix_len].eq_ignore_ascii_case(prefix) {
        Some(&value[prefix_len..])
    } else {
        None
    }
}

fn translate_sqlite_rowid(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut result = String::with_capacity(sql.len());
    let mut idx = 0usize;
    let mut in_single_quote = false;

    while idx < chars.len() {
        let ch = chars[idx];
        if ch == '\'' {
            result.push(ch);
            if in_single_quote {
                if idx + 1 < chars.len() && chars[idx + 1] == '\'' {
                    result.push('\'');
                    idx += 2;
                    continue;
                }
                in_single_quote = false;
            } else {
                in_single_quote = true;
            }
            idx += 1;
            continue;
        }

        if !in_single_quote && (ch.is_ascii_alphabetic() || ch == '_') {
            let start = idx;
            idx += 1;
            while idx < chars.len()
                && (chars[idx].is_ascii_alphanumeric() || chars[idx] == '_' || chars[idx] == '.')
            {
                idx += 1;
            }

            let token: String = chars[start..idx].iter().collect();
            if token.eq_ignore_ascii_case("rowid") {
                result.push_str("ctid");
                continue;
            }

            let lower = token.to_ascii_lowercase();
            if let Some(prefix) = lower.strip_suffix(".rowid") {
                result.push_str(&token[..prefix.len()]);
                result.push_str(".ctid");
                continue;
            }

            result.push_str(&token);
            continue;
        }

        result.push(ch);
        idx += 1;
    }

    result
}

fn split_identifier_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|value| value.trim().trim_matches('"').to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn find_matching_paren(value: &str, start: usize) -> Option<usize> {
    let bytes = value.as_bytes();
    if bytes.get(start).copied() != Some(b'(') {
        return None;
    }

    let mut depth = 0usize;
    let mut in_single_quote = false;
    let mut idx = start;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if in_single_quote {
            if byte == b'\'' {
                if bytes.get(idx + 1).copied() == Some(b'\'') {
                    idx += 2;
                    continue;
                }
                in_single_quote = false;
            }
            idx += 1;
            continue;
        }

        match byte {
            b'\'' => in_single_quote = true,
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
        idx += 1;
    }

    None
}

fn translate_policy_sql_placeholders(
    sql: &str,
    params: &[serde_json::Value],
) -> Result<PreparedPolicySql, String> {
    let mut result = String::with_capacity(sql.len() + params.len() * 3);
    let bytes = sql.as_bytes();
    let mut idx = 0usize;
    let mut next_unnumbered_param = 0usize;
    let mut bind_param_by_source = HashMap::<usize, usize>::new();
    let mut bind_params = Vec::<serde_json::Value>::new();

    #[derive(Debug)]
    enum ScanState {
        Normal,
        SingleQuoted,
        DoubleQuoted,
        LineComment,
        BlockComment { depth: usize },
        DollarQuoted { delimiter: String },
    }

    let mut state = ScanState::Normal;

    while idx < bytes.len() {
        match &mut state {
            ScanState::Normal => {
                if bytes[idx] == b'\'' {
                    result.push('\'');
                    state = ScanState::SingleQuoted;
                    idx += 1;
                    continue;
                }
                if bytes[idx] == b'"' {
                    result.push('"');
                    state = ScanState::DoubleQuoted;
                    idx += 1;
                    continue;
                }
                if sql[idx..].starts_with("--") {
                    result.push_str("--");
                    state = ScanState::LineComment;
                    idx += 2;
                    continue;
                }
                if sql[idx..].starts_with("/*") {
                    result.push_str("/*");
                    state = ScanState::BlockComment { depth: 1 };
                    idx += 2;
                    continue;
                }
                if let Some(delimiter) = dollar_quote_delimiter(sql, idx) {
                    result.push_str(&delimiter);
                    idx += delimiter.len();
                    state = ScanState::DollarQuoted { delimiter };
                    continue;
                }
                if bytes[idx] == b'?' {
                    let mut digit_idx = idx + 1;
                    while digit_idx < bytes.len() && bytes[digit_idx].is_ascii_digit() {
                        digit_idx += 1;
                    }
                    let digits = &sql[idx + 1..digit_idx];

                    let (param_index, placeholder_label) = if digits.is_empty() {
                        let current = next_unnumbered_param;
                        next_unnumbered_param += 1;
                        (current, format!("?{}", current + 1))
                    } else {
                        let parsed = digits
                            .parse::<usize>()
                            .map_err(|_| format!("invalid numbered placeholder '?{digits}'"))?;
                        let param_index = parsed
                            .checked_sub(1)
                            .ok_or_else(|| "placeholder index must start at 1".to_string())?;
                        (param_index, format!("?{digits}"))
                    };

                    let bind_position = match bind_param_by_source.get(&param_index).copied() {
                        Some(position) => position,
                        None => {
                            let param = params.get(param_index).ok_or_else(|| {
                                format!(
                                    "placeholder {placeholder_label} does not have a matching parameter"
                                )
                            })?;
                            bind_params.push(param.clone());
                            let position = bind_params.len();
                            bind_param_by_source.insert(param_index, position);
                            position
                        }
                    };
                    result.push('$');
                    result.push_str(&bind_position.to_string());
                    idx = digit_idx;
                    continue;
                }
            }
            ScanState::SingleQuoted => {
                if bytes[idx] == b'\'' {
                    result.push('\'');
                    if idx + 1 < bytes.len() && bytes[idx + 1] == b'\'' {
                        result.push('\'');
                        idx += 2;
                    } else {
                        state = ScanState::Normal;
                        idx += 1;
                    }
                    continue;
                }
            }
            ScanState::DoubleQuoted => {
                if bytes[idx] == b'"' {
                    result.push('"');
                    if idx + 1 < bytes.len() && bytes[idx + 1] == b'"' {
                        result.push('"');
                        idx += 2;
                    } else {
                        state = ScanState::Normal;
                        idx += 1;
                    }
                    continue;
                }
            }
            ScanState::LineComment => {
                if bytes[idx] == b'\n' {
                    result.push('\n');
                    state = ScanState::Normal;
                    idx += 1;
                    continue;
                }
            }
            ScanState::BlockComment { depth } => {
                if sql[idx..].starts_with("/*") {
                    result.push_str("/*");
                    *depth += 1;
                    idx += 2;
                    continue;
                }
                if sql[idx..].starts_with("*/") {
                    result.push_str("*/");
                    *depth = depth.saturating_sub(1);
                    if *depth == 0 {
                        state = ScanState::Normal;
                    }
                    idx += 2;
                    continue;
                }
            }
            ScanState::DollarQuoted { delimiter } => {
                if sql[idx..].starts_with(delimiter.as_str()) {
                    result.push_str(delimiter);
                    idx += delimiter.len();
                    state = ScanState::Normal;
                    continue;
                }
            }
        };

        let ch = sql[idx..]
            .chars()
            .next()
            .expect("idx is always within SQL bounds");
        result.push(ch);
        idx += ch.len_utf8();
    }

    Ok(PreparedPolicySql {
        sql: result,
        params: bind_params,
    })
}

fn dollar_quote_delimiter(sql: &str, idx: usize) -> Option<String> {
    let bytes = sql.as_bytes();
    if bytes.get(idx).copied() != Some(b'$') {
        return None;
    }

    let mut end = idx + 1;
    while end < bytes.len() && is_dollar_quote_tag_byte(bytes[end]) {
        end += 1;
    }
    if bytes.get(end).copied() != Some(b'$') {
        return None;
    }

    Some(sql[idx..=end].to_string())
}

fn is_dollar_quote_tag_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[derive(Debug, Clone)]
struct PolicySqlParam(serde_json::Value);

impl PolicySqlParam {
    fn wire_text(&self) -> Option<String> {
        match &self.0 {
            serde_json::Value::Null => None,
            serde_json::Value::Bool(value) => Some(value.to_string()),
            serde_json::Value::Number(value) => Some(value.to_string()),
            serde_json::Value::String(value) => Some(value.clone()),
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => Some(self.0.to_string()),
        }
    }
}

impl Type<Postgres> for PolicySqlParam {
    fn type_info() -> PgTypeInfo {
        <String as Type<Postgres>>::type_info()
    }
}

impl Encode<'_, Postgres> for PolicySqlParam {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        let Some(value) = self.wire_text() else {
            return Ok(IsNull::Yes);
        };
        buf.extend(value.as_bytes());
        Ok(IsNull::No)
    }

    fn size_hint(&self) -> usize {
        self.wire_text().map(|value| value.len()).unwrap_or(0)
    }
}

fn bind_policy_sql_params<'q>(
    mut query: sqlx::query::Query<'q, Postgres, PgArguments>,
    params: Vec<serde_json::Value>,
) -> sqlx::query::Query<'q, Postgres, PgArguments> {
    // Bind each policy param with the Postgres type that matches its JSON
    // shape. The previous implementation routed everything through
    // `PolicySqlParam` which advertises `text` only — that broke
    // `xp + ?` style updates with `operator does not exist: bigint + text`
    // and silently aborted the kanban OnDispatchCompleted hook, which in
    // turn skipped the in_progress→review transition for auto-queue cards
    // (run 24837914 Phase 4 entries observed in production).
    for param in params {
        query = match param {
            serde_json::Value::Null => query.bind(Option::<i64>::None),
            serde_json::Value::Bool(b) => query.bind(b),
            serde_json::Value::Number(ref n) => {
                if let Some(i) = n.as_i64() {
                    query.bind(i)
                } else if let Some(u) = n.as_u64() {
                    // PostgreSQL has no UINT8 — clamp to i64 range. Values
                    // larger than i64::MAX are exceptionally rare for
                    // policy params; encoding as text preserves the value.
                    if u <= i64::MAX as u64 {
                        query.bind(u as i64)
                    } else {
                        query.bind(PolicySqlParam(param))
                    }
                } else if let Some(f) = n.as_f64() {
                    query.bind(f)
                } else {
                    query.bind(PolicySqlParam(param))
                }
            }
            serde_json::Value::String(s) => query.bind(s),
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                query.bind(sqlx::types::Json(param))
            }
        };
    }
    query
}

fn pg_row_to_json(
    row: &sqlx::postgres::PgRow,
    json_column_mode: JsonColumnMode,
) -> Result<serde_json::Value, String> {
    let mut map = serde_json::Map::new();

    for (index, column) in row.columns().iter().enumerate() {
        let column_name = column.name().to_string();
        let type_name = column.type_info().name().to_ascii_uppercase();
        let value = match type_name.as_str() {
            "BOOL" => option_to_json(
                row.try_get::<Option<bool>, _>(index)
                    .map_err(|error| format!("decode bool column {column_name}: {error}"))?,
            ),
            "INT2" | "INT4" => option_to_json(
                row.try_get::<Option<i32>, _>(index)
                    .map_err(|error| format!("decode int column {column_name}: {error}"))?,
            ),
            "INT8" => option_to_json(
                row.try_get::<Option<i64>, _>(index)
                    .map_err(|error| format!("decode bigint column {column_name}: {error}"))?,
            ),
            "FLOAT4" => option_to_json(
                row.try_get::<Option<f32>, _>(index)
                    .map_err(|error| format!("decode float column {column_name}: {error}"))?,
            ),
            "FLOAT8" | "NUMERIC" => option_to_json(
                row.try_get::<Option<f64>, _>(index)
                    .map_err(|error| format!("decode numeric column {column_name}: {error}"))?,
            ),
            "JSON" | "JSONB" => match row
                .try_get::<Option<serde_json::Value>, _>(index)
                .map_err(|error| format!("decode json column {column_name}: {error}"))?
            {
                Some(value) => match json_column_mode {
                    JsonColumnMode::LegacyString => serde_json::Value::String(value.to_string()),
                    JsonColumnMode::Typed => value,
                },
                None => serde_json::Value::Null,
            },
            "TIMESTAMPTZ" => match row
                .try_get::<Option<DateTime<Utc>>, _>(index)
                .map_err(|error| format!("decode timestamptz column {column_name}: {error}"))?
            {
                Some(value) => {
                    serde_json::Value::String(value.format("%Y-%m-%d %H:%M:%S").to_string())
                }
                None => serde_json::Value::Null,
            },
            "TIMESTAMP" => match row
                .try_get::<Option<NaiveDateTime>, _>(index)
                .map_err(|error| format!("decode timestamp column {column_name}: {error}"))?
            {
                Some(value) => {
                    serde_json::Value::String(value.format("%Y-%m-%d %H:%M:%S").to_string())
                }
                None => serde_json::Value::Null,
            },
            "DATE" => match row
                .try_get::<Option<NaiveDate>, _>(index)
                .map_err(|error| format!("decode date column {column_name}: {error}"))?
            {
                Some(value) => serde_json::Value::String(value.to_string()),
                None => serde_json::Value::Null,
            },
            "TIME" => match row
                .try_get::<Option<NaiveTime>, _>(index)
                .map_err(|error| format!("decode time column {column_name}: {error}"))?
            {
                Some(value) => serde_json::Value::String(value.format("%H:%M:%S").to_string()),
                None => serde_json::Value::Null,
            },
            _ => match row.try_get::<Option<String>, _>(index).map_err(|error| {
                format!("unsupported postgres column {column_name} type {type_name}: {error}")
            })? {
                Some(value) => serde_json::Value::String(value),
                None => serde_json::Value::Null,
            },
        };
        map.insert(column_name, value);
    }

    Ok(serde_json::Value::Object(map))
}

fn option_to_json<T>(value: Option<T>) -> serde_json::Value
where
    T: serde::Serialize,
{
    match value {
        Some(value) => serde_json::json!(value),
        None => serde_json::Value::Null,
    }
}

/// Emit a structured audit log whenever the legacy raw-DB escape hatch
/// (`agentdesk.db.query` / `agentdesk.db.execute`) fires from a policy.
///
/// Part of #1007 — gives ops a paper trail for residual raw SQL usage
/// while the typed-facade migration proceeds slice by slice. The payload
/// includes:
///   * `origin` — the JS entrypoint name (query vs execute)
///   * `sql_category` — SELECT/INSERT/UPDATE/DELETE/etc., derived from
///     the first SQL keyword
///   * `policy_name`, `capability`, `source_event` — parsed from the
///     inline escape-hatch marker comment
///     `/* legacy-raw-db: policy=… capability=… source_event=… */`
///     when callers annotate the callsite (optional)
fn emit_raw_db_audit(origin: &str, sql: &str) {
    let category = sql_category(sql);
    let marker = parse_raw_db_marker(sql);
    tracing::info!(
        target: "policy.raw_db_audit",
        origin = origin,
        sql_category = category,
        policy_name = marker.policy.as_deref().unwrap_or("<unspecified>"),
        capability = marker.capability.as_deref().unwrap_or("<unspecified>"),
        source_event = marker.source_event.as_deref().unwrap_or("<unspecified>"),
        sql = %compact_sql(sql),
        "policy raw-db escape hatch invoked"
    );
}

fn sql_category(sql: &str) -> &'static str {
    let trimmed = sql.trim_start();
    // Skip leading comments and whitespace
    let mut cursor = trimmed;
    loop {
        let next = cursor.trim_start();
        if let Some(rest) = next.strip_prefix("--") {
            cursor = match rest.find('\n') {
                Some(i) => &rest[i + 1..],
                None => "",
            };
            continue;
        }
        if let Some(rest) = next.strip_prefix("/*") {
            cursor = match rest.find("*/") {
                Some(i) => &rest[i + 2..],
                None => "",
            };
            continue;
        }
        cursor = next;
        break;
    }
    let first = cursor
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match first.as_str() {
        "SELECT" | "WITH" => "select",
        "INSERT" => "insert",
        "UPDATE" => "update",
        "DELETE" => "delete",
        "REPLACE" => "replace",
        "CREATE" => "ddl_create",
        "DROP" => "ddl_drop",
        "ALTER" => "ddl_alter",
        "PRAGMA" => "pragma",
        _ => "other",
    }
}

#[derive(Default)]
struct RawDbMarker {
    policy: Option<String>,
    capability: Option<String>,
    source_event: Option<String>,
}

fn parse_raw_db_marker(sql: &str) -> RawDbMarker {
    let mut marker = RawDbMarker::default();
    // Look for `/* legacy-raw-db: key=value key=value ... */`
    let Some(start) = sql.find("legacy-raw-db:") else {
        return marker;
    };
    let rest = &sql[start + "legacy-raw-db:".len()..];
    let end = rest.find("*/").unwrap_or(rest.len());
    let body = rest[..end].trim();
    for pair in body.split_whitespace() {
        if let Some((k, v)) = pair.split_once('=') {
            let v = v.trim_matches(|c: char| c == '"' || c == '\'');
            match k.trim() {
                "policy" | "policy_name" => marker.policy = Some(v.to_string()),
                "capability" => marker.capability = Some(v.to_string()),
                "source_event" | "event" => marker.source_event = Some(v.to_string()),
                _ => {}
            }
        }
    }
    marker
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

fn run_async_bridge_pg<F, T>(
    pool: &PgPool,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_pg_result(pool, future_factory, |error| error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::Row;

    struct TestDatabase {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestDatabase {
        async fn create() -> Self {
            let admin_url = admin_database_url();
            let database_name = format!("agentdesk_db_ops_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "db ops pg tests",
            )
            .await
            .expect("create postgres test db");

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "db ops pg tests",
            )
            .await
            .expect("migrate postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "db ops pg tests",
            )
            .await
            .expect("drop postgres test db");
        }
    }

    fn base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", base_database_url(), admin_db)
    }

    #[test]
    fn prepare_policy_sql_for_pg_rewrites_insert_or_replace() {
        let sql = "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?, ?, datetime('now', '+' || ? || ' seconds'))";
        let prepared = prepare_policy_sql_for_pg(sql, &[json!("k"), json!("v"), json!(600)])
            .expect("render insert or replace");

        assert_eq!(
            prepared.sql,
            "INSERT INTO kv_meta (key, value, expires_at) VALUES ($1, $2, datetime('now', '+' || $3 || ' seconds')) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, expires_at = EXCLUDED.expires_at"
        );
        assert_eq!(prepared.params, vec![json!("k"), json!("v"), json!(600)]);
    }

    #[test]
    fn prepare_policy_sql_for_pg_rewrites_insert_or_ignore() {
        let sql = "INSERT OR IGNORE INTO kanban_cards (id, title) VALUES (?1, ?2)";
        let prepared = prepare_policy_sql_for_pg(sql, &[json!("card-1"), json!("Title")])
            .expect("render insert or ignore");

        assert_eq!(
            prepared.sql,
            "INSERT INTO kanban_cards (id, title) VALUES ($1, $2) ON CONFLICT DO NOTHING"
        );
        assert_eq!(prepared.params, vec![json!("card-1"), json!("Title")]);
    }

    #[test]
    fn prepare_policy_sql_for_pg_rewrites_rowid_tokens() {
        let sql = "SELECT rowid, td.rowid, 'rowid' AS literal FROM task_dispatches td ORDER BY td.rowid DESC, rowid DESC";
        let prepared = prepare_policy_sql_for_pg(sql, &[]).expect("render rowid");

        assert!(
            prepared
                .sql
                .contains("SELECT ctid, td.ctid, 'rowid' AS literal")
        );
        assert!(prepared.sql.contains("ORDER BY td.ctid DESC, ctid DESC"));
        assert!(prepared.params.is_empty());
    }

    #[test]
    fn prepare_policy_sql_for_pg_leaves_question_marks_inside_strings() {
        let sql = "SELECT '?' AS marker, ?1 AS value";
        let prepared = prepare_policy_sql_for_pg(sql, &[json!("ok")]).expect("interpolate");
        assert_eq!(prepared.sql, "SELECT '?' AS marker, $1 AS value");
        assert_eq!(prepared.params, vec![json!("ok")]);
    }

    #[test]
    fn prepare_policy_sql_for_pg_leaves_question_marks_inside_comments() {
        let sql = "SELECT ?1 AS value -- ?2 is a comment\n/* ?3 is also a comment */";
        let prepared = prepare_policy_sql_for_pg(sql, &[json!("ok")]).expect("interpolate");
        assert_eq!(
            prepared.sql,
            "SELECT $1 AS value -- ?2 is a comment\n/* ?3 is also a comment */"
        );
        assert_eq!(prepared.params, vec![json!("ok")]);
    }

    #[test]
    fn prepare_policy_sql_for_pg_reuses_numbered_placeholders() {
        let sql = "SELECT ?2 AS second, ?1 AS first, ?2 AS second_again";
        let prepared =
            prepare_policy_sql_for_pg(sql, &[json!("one"), json!("two")]).expect("interpolate");
        assert_eq!(
            prepared.sql,
            "SELECT $1 AS second, $2 AS first, $1 AS second_again"
        );
        assert_eq!(prepared.params, vec![json!("two"), json!("one")]);
    }

    #[test]
    fn prepare_policy_sql_for_pg_errors_when_parameter_is_missing() {
        let sql = "SELECT ?1, ?2";
        let error = prepare_policy_sql_for_pg(sql, &[json!(1)]).expect_err("missing param");
        assert!(error.contains("?2"));
    }

    #[test]
    fn policy_sql_param_wire_text_preserves_json_scalar_and_container_values() {
        assert_eq!(
            PolicySqlParam(json!(true)).wire_text().as_deref(),
            Some("true")
        );
        assert_eq!(PolicySqlParam(json!(42)).wire_text().as_deref(), Some("42"));
        assert_eq!(PolicySqlParam(json!(null)).wire_text().as_deref(), None);
        assert_eq!(
            PolicySqlParam(json!({"k": "v"})).wire_text().as_deref(),
            Some(r#"{"k":"v"}"#)
        );
        assert_eq!(
            PolicySqlParam(json!(["a", "b"])).wire_text().as_deref(),
            Some(r#"["a","b"]"#)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_db_query_json_returns_typed_json_columns_and_query_keeps_strings() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        let pool_for_js = pool.clone();
        let result: String = ctx.with(|ctx| {
            let ad = rquickjs::Object::new(ctx.clone()).unwrap();
            ctx.globals().set("agentdesk", ad).unwrap();
            register_db_ops(&ctx, None, Some(pool_for_js)).unwrap();
            ctx.eval(
                r#"
                (function() {
                    var sql =
                        "SELECT " +
                        "jsonb_build_object('nested', jsonb_build_object('count', 2), 'items', jsonb_build_array('a', 'b')) AS payload, " +
                        "json_build_array(1, 2) AS numbers, " +
                        "NULL::jsonb AS missing";
                    var legacy = agentdesk.db.query(sql)[0];
                    var typed = agentdesk.db.queryJson(sql)[0];
                    return JSON.stringify({
                        legacy_payload_type: typeof legacy.payload,
                        legacy_payload_decoded: JSON.parse(legacy.payload),
                        legacy_numbers_type: typeof legacy.numbers,
                        typed_payload_is_object: typed.payload && typeof typed.payload === "object" && !Array.isArray(typed.payload),
                        typed_payload: typed.payload,
                        typed_numbers_is_array: Array.isArray(typed.numbers),
                        typed_missing_is_null: typed.missing === null
                    });
                })()
                "#,
            )
            .unwrap()
        });

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["legacy_payload_type"], "string");
        assert_eq!(parsed["legacy_payload_decoded"]["nested"]["count"], 2);
        assert_eq!(parsed["legacy_payload_decoded"]["items"], json!(["a", "b"]));
        assert_eq!(parsed["legacy_numbers_type"], "string");
        assert_eq!(parsed["typed_payload_is_object"], true);
        assert_eq!(parsed["typed_payload"]["nested"]["count"], 2);
        assert_eq!(parsed["typed_payload"]["items"], json!(["a", "b"]));
        assert_eq!(parsed["typed_numbers_is_array"], true);
        assert_eq!(parsed["typed_missing_is_null"], true);

        pool.close().await;
        test_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_db_pg_exec_and_query_support_sqlite_compat_functions() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        let execute = db_execute_raw_pg(
            &pool,
            "INSERT OR REPLACE INTO kv_meta (key, value, expires_at)
             VALUES (?1, json_extract(?2, '$.payload.value'), datetime('now', '+' || ?3 || ' seconds'))",
            &[json!("pg-db-compat"), json!({"payload": {"value": "hello-pg"}}), json!(600)],
            std::time::Instant::now(),
        );
        let execute_json: serde_json::Value =
            serde_json::from_str(&execute).expect("parse execute json");
        assert_eq!(execute_json["changes"], 1);

        let rows = db_query_raw_pg(
            &pool,
            "SELECT key,
                    value,
                    expires_at > datetime('now') AS still_valid,
                    json_extract(?2, '$.meta.answer') AS extracted_answer
             FROM kv_meta
             WHERE key = ?1",
            &[json!("pg-db-compat"), json!({"meta": {"answer": 42}})],
            std::time::Instant::now(),
        );
        let rows_json: serde_json::Value = serde_json::from_str(&rows).expect("parse query json");
        let result_rows = rows_json.as_array().expect("query rows array");
        assert_eq!(result_rows.len(), 1);
        assert_eq!(result_rows[0]["key"], "pg-db-compat");
        assert_eq!(result_rows[0]["value"], "hello-pg");
        assert_eq!(result_rows[0]["still_valid"], true);
        assert_eq!(result_rows[0]["extracted_answer"], "42");

        let placeholder_rows = db_query_raw_pg(
            &pool,
            "SELECT ?1::text AS marker -- ?2 remains inside a line comment
                    /* ?3 remains inside a block comment */",
            &[json!("literal-ok")],
            std::time::Instant::now(),
        );
        let placeholder_json: serde_json::Value =
            serde_json::from_str(&placeholder_rows).expect("parse placeholder query json");
        assert_eq!(placeholder_json[0]["marker"], "literal-ok");

        let typed_rows = db_query_raw_pg(
            &pool,
            "SELECT
                    json_extract(?1::jsonb, '$.kind') AS object_kind,
                    jsonb_array_length(?2::jsonb) AS array_len,
                    ?3::boolean AS bool_value,
                    ?4::bigint AS numeric_value,
                    ?5::double precision AS float_value,
                    ?6::text IS NULL AS null_value",
            &[
                json!({"kind": "object"}),
                json!(["a", "b"]),
                json!(true),
                json!(42),
                json!(3.5),
                serde_json::Value::Null,
            ],
            std::time::Instant::now(),
        );
        let typed_json: serde_json::Value =
            serde_json::from_str(&typed_rows).expect("parse typed query json");
        let typed_rows = typed_json.as_array().unwrap_or_else(|| {
            panic!("typed query returned non-array response: {typed_json}");
        });
        assert_eq!(typed_rows[0]["object_kind"], "object");
        assert_eq!(typed_rows[0]["array_len"], 2);
        assert_eq!(typed_rows[0]["bool_value"], true);
        assert_eq!(typed_rows[0]["numeric_value"], 42);
        assert_eq!(typed_rows[0]["float_value"], 3.5);
        assert_eq!(typed_rows[0]["null_value"], true);

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ('card-json-param', 'JSON Param', 'review', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed json param card");
        let metadata_update = db_execute_raw_pg(
            &pool,
            "UPDATE kanban_cards SET metadata = ? WHERE id = ?",
            &[
                json!({"loop_guard": {"review_churn": {"enter_count": 1}}}),
                json!("card-json-param"),
            ],
            std::time::Instant::now(),
        );
        let metadata_update_json: serde_json::Value =
            serde_json::from_str(&metadata_update).expect("parse metadata update json");
        assert_eq!(metadata_update_json["changes"], 1);
        let stored_metadata: serde_json::Value =
            sqlx::query_scalar("SELECT metadata FROM kanban_cards WHERE id = 'card-json-param'")
                .fetch_one(&pool)
                .await
                .expect("fetch json param metadata");
        assert_eq!(
            stored_metadata["loop_guard"]["review_churn"]["enter_count"],
            1
        );

        let expires_at: chrono::DateTime<chrono::Utc> = sqlx::query(
            "SELECT expires_at
             FROM kv_meta
             WHERE key = $1",
        )
        .bind("pg-db-compat")
        .fetch_one(&pool)
        .await
        .expect("fetch kv_meta row")
        .try_get("expires_at")
        .expect("decode expires_at");
        assert!(expires_at > chrono::Utc::now());

        pool.close().await;
        test_db.drop().await;
    }
}
