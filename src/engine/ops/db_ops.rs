use crate::db::Db;
use crate::engine::sql_guard::detect_core_table_write;
use crate::error::{AppError, ErrorCode};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::{Column, PgPool, Row, TypeInfo};
use std::future::Future;
use std::time::Duration;

// ── DB ops ───────────────────────────────────────────────────────
//
// We use a JSON-string bridge: Rust receives (sql, params_json_string)
// and returns a json_string. A thin JS wrapper does JSON.parse/stringify.

const POLICY_DB_WARN_THRESHOLD: Duration = Duration::from_millis(100);

pub(super) fn register_db_ops<'js>(
    ctx: &Ctx<'js>,
    db: Db,
    pg_pool: Option<PgPool>,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let db_obj = Object::new(ctx.clone())?;

    // Internal: __db_query_raw(sql, params_json) → json_string
    let db_q = db.clone();
    let pg_q = pg_pool.clone();
    let query_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(move |sql: String, params_json: String| -> String {
            db_query_raw(&db_q, pg_q.clone(), &sql, &params_json)
        }),
    )?;
    db_obj.set("__query_raw", query_raw)?;

    // Internal: __db_execute_raw(sql, params_json) → json_string
    let db_e = db.clone();
    let pg_e = pg_pool.clone();
    let execute_raw = Function::new(
        ctx.clone(),
        rquickjs::function::MutFn::from(move |sql: String, params_json: String| -> String {
            db_execute_raw(&db_e, pg_e.clone(), &sql, &params_json)
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

fn db_query_raw(db: &Db, pg_pool: Option<PgPool>, sql: &str, params_json: &str) -> String {
    let started = std::time::Instant::now();
    let params: Vec<serde_json::Value> =
        match parse_params_json(params_json, "agentdesk.db.query.parse_params", sql) {
            Ok(params) => params,
            Err(error_json) => return error_json,
        };

    if let Some(pg_pool) = pg_pool {
        return db_query_raw_pg(&pg_pool, sql, &params, started);
    }

    let bind: Vec<libsql_rusqlite::types::Value> = params.iter().map(json_to_sqlite).collect();

    // Use a separate read-only connection to avoid blocking the write Mutex.
    // This prevents deadlock when onTick (holding engine lock) queries DB
    // while request handlers hold the write lock.
    let conn = match db.read_conn() {
        Ok(c) => c,
        Err(e) => {
            return policy_db_error_json(
                "agentdesk.db.query.read_conn",
                sql,
                format!("db read: {e}"),
            );
        }
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            return policy_db_error_json(
                "agentdesk.db.query.prepare",
                sql,
                format!("prepare: {e}"),
            );
        }
    };

    let col_count = stmt.column_count();
    let col_names: Vec<std::string::String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();

    let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> = bind
        .iter()
        .map(|v| v as &dyn libsql_rusqlite::types::ToSql)
        .collect();

    let rows = match stmt.query_map(params_ref.as_slice(), |row| {
        let mut map = serde_json::Map::new();
        for (i, col_name) in col_names.iter().enumerate() {
            let val: libsql_rusqlite::types::Value = row.get(i)?;
            let jv = sqlite_to_json(&val);
            map.insert(col_name.clone(), jv);
        }
        Ok(serde_json::Value::Object(map))
    }) {
        Ok(r) => r,
        Err(e) => {
            return policy_db_error_json(
                "agentdesk.db.query.query_map",
                sql,
                format!("query: {e}"),
            );
        }
    };

    let result: Vec<serde_json::Value> = match rows.collect::<Result<Vec<_>, _>>() {
        Ok(result) => result,
        Err(error) => {
            return policy_db_error_json(
                "agentdesk.db.query.collect_rows",
                sql,
                format!("row decode: {error}"),
            );
        }
    };
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
            "agentdesk.db.query.serialize",
            sql,
            format!("serialize query result: {error}"),
        )
    })
}

fn db_query_raw_pg(
    pg_pool: &PgPool,
    sql: &str,
    params: &[serde_json::Value],
    started: std::time::Instant,
) -> String {
    let prepared_sql = match prepare_policy_sql_for_pg(sql, params) {
        Ok(sql) => sql,
        Err(error) => {
            return policy_db_bad_request_json("agentdesk.db.query.translate_pg", sql, error);
        }
    };

    let pool = pg_pool.clone();
    let query_sql = prepared_sql.clone();
    let rows = match run_async_bridge_pg(&pool, move |pool| async move {
        sqlx::query(&query_sql)
            .fetch_all(&pool)
            .await
            .map_err(|error| format!("query: {error}"))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            return policy_db_error_json("agentdesk.db.query.fetch_all_pg", sql, error);
        }
    };

    let mut result = Vec::with_capacity(rows.len());
    for row in &rows {
        match pg_row_to_json(row) {
            Ok(value) => result.push(value),
            Err(error) => {
                return policy_db_error_json("agentdesk.db.query.collect_rows_pg", sql, error);
            }
        }
    }

    let elapsed = started.elapsed();
    if elapsed >= POLICY_DB_WARN_THRESHOLD {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis(),
            row_count = result.len(),
            sql = %compact_sql(sql),
            translated_sql = %compact_sql(&prepared_sql),
            "policy db query slow"
        );
    }

    serde_json::to_string(&result).unwrap_or_else(|error| {
        policy_db_error_json(
            "agentdesk.db.query.serialize_pg",
            sql,
            format!("serialize query result: {error}"),
        )
    })
}

fn db_execute_raw(db: &Db, pg_pool: Option<PgPool>, sql: &str, params_json: &str) -> String {
    let started = std::time::Instant::now();
    if let Some(violation) = detect_core_table_write(sql) {
        return serde_json::json!({ "error": violation.error_message() }).to_string();
    }

    let params: Vec<serde_json::Value> =
        match parse_params_json(params_json, "agentdesk.db.execute.parse_params", sql) {
            Ok(params) => params,
            Err(error_json) => return error_json,
        };

    if let Some(pg_pool) = pg_pool {
        return db_execute_raw_pg(&pg_pool, sql, &params, started);
    }

    let bind: Vec<libsql_rusqlite::types::Value> = params.iter().map(json_to_sqlite).collect();

    // Use a separate read-write connection to avoid holding the main
    // Rust Mutex that request handlers need. SQLite WAL serializes
    // concurrent writers via busy_timeout (5s).
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return policy_db_error_json(
                "agentdesk.db.execute.separate_conn",
                sql,
                format!("db conn: {e}"),
            );
        }
    };

    let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> = bind
        .iter()
        .map(|v| v as &dyn libsql_rusqlite::types::ToSql)
        .collect();

    let changes = match conn.execute(sql, params_ref.as_slice()) {
        Ok(n) => n,
        Err(e) => {
            return policy_db_error_json(
                "agentdesk.db.execute.execute",
                sql,
                format!("execute: {e}"),
            );
        }
    };
    let elapsed = started.elapsed();
    if elapsed >= POLICY_DB_WARN_THRESHOLD {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis(),
            changes,
            sql = %compact_sql(sql),
            "policy db execute slow"
        );
    }

    format!(r#"{{"changes":{changes}}}"#)
}

fn db_execute_raw_pg(
    pg_pool: &PgPool,
    sql: &str,
    params: &[serde_json::Value],
    started: std::time::Instant,
) -> String {
    let prepared_sql = match prepare_policy_sql_for_pg(sql, params) {
        Ok(sql) => sql,
        Err(error) => {
            return policy_db_bad_request_json("agentdesk.db.execute.translate_pg", sql, error);
        }
    };

    let pool = pg_pool.clone();
    let execute_sql = prepared_sql.clone();
    let changes = match run_async_bridge_pg(&pool, move |pool| async move {
        sqlx::query(&execute_sql)
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
            translated_sql = %compact_sql(&prepared_sql),
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

fn prepare_policy_sql_for_pg(sql: &str, params: &[serde_json::Value]) -> Result<String, String> {
    let translated = translate_insert_with_conflict(sql)?;
    let translated = translate_sqlite_rowid(&translated);
    interpolate_policy_sql_params(&translated, params)
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

fn interpolate_policy_sql_params(
    sql: &str,
    params: &[serde_json::Value],
) -> Result<String, String> {
    let mut result = String::with_capacity(sql.len() + params.len() * 8);
    let chars: Vec<char> = sql.chars().collect();
    let mut idx = 0usize;
    let mut next_unnumbered_param = 0usize;
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

        if !in_single_quote && ch == '?' {
            let mut digit_idx = idx + 1;
            let mut digits = String::new();
            while digit_idx < chars.len() && chars[digit_idx].is_ascii_digit() {
                digits.push(chars[digit_idx]);
                digit_idx += 1;
            }

            let param_index = if digits.is_empty() {
                let current = next_unnumbered_param;
                next_unnumbered_param += 1;
                current
            } else {
                digits
                    .parse::<usize>()
                    .map_err(|_| format!("invalid numbered placeholder '?{digits}'"))?
                    .checked_sub(1)
                    .ok_or_else(|| "placeholder index must start at 1".to_string())?
            };

            let param = params.get(param_index).ok_or_else(|| {
                format!(
                    "placeholder {} does not have a matching parameter",
                    if digits.is_empty() {
                        format!("?{}", next_unnumbered_param)
                    } else {
                        format!("?{digits}")
                    }
                )
            })?;
            result.push_str(&json_to_pg_literal(param));
            idx = digit_idx;
            continue;
        }

        result.push(ch);
        idx += 1;
    }

    Ok(result)
}

fn json_to_pg_literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(value) => {
            if *value {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => format!("'{}'", escape_pg_string(value)),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            format!("'{}'", escape_pg_string(&value.to_string()))
        }
    }
}

fn escape_pg_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn pg_row_to_json(row: &sqlx::postgres::PgRow) -> Result<serde_json::Value, String> {
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
                Some(value) => serde_json::Value::String(value.to_string()),
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

fn compact_sql(sql: &str) -> String {
    const MAX_SQL_CONTEXT_LEN: usize = 120;

    let compact = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= MAX_SQL_CONTEXT_LEN {
        compact
    } else {
        format!("{}...", &compact[..MAX_SQL_CONTEXT_LEN])
    }
}

fn run_async_bridge<F, T>(future: F) -> Result<T, String>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    crate::utils::async_bridge::block_on_result(future, |error| error)
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

fn json_to_sqlite(val: &serde_json::Value) -> libsql_rusqlite::types::Value {
    match val {
        serde_json::Value::Null => libsql_rusqlite::types::Value::Null,
        serde_json::Value::Bool(b) => {
            libsql_rusqlite::types::Value::Integer(if *b { 1 } else { 0 })
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                libsql_rusqlite::types::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                libsql_rusqlite::types::Value::Real(f)
            } else {
                libsql_rusqlite::types::Value::Null
            }
        }
        serde_json::Value::String(s) => libsql_rusqlite::types::Value::Text(s.clone()),
        _ => libsql_rusqlite::types::Value::Text(val.to_string()),
    }
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
            let admin_pool = sqlx::PgPool::connect(&admin_url)
                .await
                .expect("connect postgres admin db");
            sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
                .expect("create postgres test db");
            admin_pool.close().await;

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            let pool = sqlx::PgPool::connect(&self.database_url)
                .await
                .expect("connect postgres test db");
            crate::db::postgres::migrate(&pool)
                .await
                .expect("migrate postgres test db");
            pool
        }

        async fn drop(self) {
            let admin_pool = sqlx::PgPool::connect(&self.admin_url)
                .await
                .expect("reconnect postgres admin db");
            sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await
            .expect("terminate postgres test db sessions");
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await
            .expect("drop postgres test db");
            admin_pool.close().await;
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
        let rendered = prepare_policy_sql_for_pg(sql, &[json!("k"), json!("v"), json!(600)])
            .expect("render insert or replace");

        assert!(rendered.starts_with("INSERT INTO kv_meta (key, value, expires_at) VALUES ('k', 'v', datetime('now', '+' || 600 || ' seconds'))"));
        assert!(rendered.contains("ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, expires_at = EXCLUDED.expires_at"));
    }

    #[test]
    fn prepare_policy_sql_for_pg_rewrites_insert_or_ignore() {
        let sql = "INSERT OR IGNORE INTO kanban_cards (id, title) VALUES (?1, ?2)";
        let rendered = prepare_policy_sql_for_pg(sql, &[json!("card-1"), json!("Title")])
            .expect("render insert or ignore");

        assert_eq!(
            rendered,
            "INSERT INTO kanban_cards (id, title) VALUES ('card-1', 'Title') ON CONFLICT DO NOTHING"
        );
    }

    #[test]
    fn prepare_policy_sql_for_pg_rewrites_rowid_tokens() {
        let sql = "SELECT rowid, td.rowid, 'rowid' AS literal FROM task_dispatches td ORDER BY td.rowid DESC, rowid DESC";
        let rendered = prepare_policy_sql_for_pg(sql, &[]).expect("render rowid");

        assert!(rendered.contains("SELECT ctid, td.ctid, 'rowid' AS literal"));
        assert!(rendered.contains("ORDER BY td.ctid DESC, ctid DESC"));
    }

    #[test]
    fn interpolate_policy_sql_params_leaves_question_marks_inside_strings() {
        let sql = "SELECT '?' AS marker, ?1 AS value";
        let rendered = interpolate_policy_sql_params(sql, &[json!("ok")]).expect("interpolate");
        assert_eq!(rendered, "SELECT '?' AS marker, 'ok' AS value");
    }

    #[test]
    fn interpolate_policy_sql_params_errors_when_parameter_is_missing() {
        let sql = "SELECT ?1, ?2";
        let error = interpolate_policy_sql_params(sql, &[json!(1)]).expect_err("missing param");
        assert!(error.contains("?2"));
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

fn sqlite_to_json(val: &libsql_rusqlite::types::Value) -> serde_json::Value {
    match val {
        libsql_rusqlite::types::Value::Null => serde_json::Value::Null,
        libsql_rusqlite::types::Value::Integer(i) => serde_json::json!(*i),
        libsql_rusqlite::types::Value::Real(f) => serde_json::json!(*f),
        libsql_rusqlite::types::Value::Text(s) => serde_json::Value::String(s.clone()),
        libsql_rusqlite::types::Value::Blob(b) => {
            let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b);
            serde_json::Value::String(encoded)
        }
    }
}
