use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::db::Db;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

const RUNTIME_CONFIG_KEYS: &[&str] = &[
    "dispatchPollSec",
    "agentSyncSec",
    "githubIssueSyncSec",
    "claudeRateLimitPollSec",
    "codexRateLimitPollSec",
    "issueTriagePollSec",
    "ceoWarnDepth",
    "maxRetries",
    "maxEntryRetries",
    "staleDispatchedGraceMin",
    "staleDispatchedTerminalStatuses",
    "staleDispatchedRecoverNullDispatch",
    "staleDispatchedRecoverMissingDispatch",
    "reviewReminderMin",
    "rateLimitWarningPct",
    "rateLimitDangerPct",
    "githubRepoCacheSec",
    "rateLimitStaleSec",
];

#[derive(Clone)]
pub struct SettingsService {
    db: Db,
    pg_pool: Option<sqlx::PgPool>,
    config: Arc<crate::config::Config>,
}

impl SettingsService {
    pub fn new(db: Db, pg_pool: Option<sqlx::PgPool>, config: Arc<crate::config::Config>) -> Self {
        Self {
            db,
            pg_pool,
            config,
        }
    }

    pub fn get_runtime_config(&self) -> ServiceResult<Value> {
        let saved = match crate::services::discord::internal_api::get_kv_value("runtime-config") {
            Ok(Some(value)) => serde_json::from_str(&value).unwrap_or_else(|_| json!({})),
            Ok(None) => json!({}),
            Err(error) if direct_api_context_unavailable(&error) => {
                if let Some(pg_pool) = self.pg_pool.as_ref() {
                    load_runtime_config_pg(pg_pool)?
                } else {
                    let conn = self.db.lock().map_err(|e| {
                        ServiceError::internal(format!("{e}"))
                            .with_code(ErrorCode::Database)
                            .with_operation("get_runtime_config.lock")
                    })?;
                    let value: String = conn
                        .query_row(
                            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap_or_else(|_| "{}".to_string());
                    serde_json::from_str(&value).unwrap_or_else(|_| json!({}))
                }
            }
            Err(error) => {
                return Err(ServiceError::internal(error)
                    .with_code(ErrorCode::Database)
                    .with_operation("get_runtime_config.load_runtime_config"));
            }
        };
        let defaults = runtime_config_defaults(self.config.as_ref());

        let mut current = defaults.as_object().cloned().unwrap_or_default();
        if let Some(saved_obj) = saved.as_object() {
            for (key, value) in saved_obj {
                current.insert(key.clone(), value.clone());
            }
        }

        Ok(json!({
            "current": current,
            "defaults": defaults,
        }))
    }

    pub fn put_runtime_config(&self, body: Value) -> ServiceResult<()> {
        if let Some(obj) = body.as_object() {
            match write_runtime_config_internal_api(obj) {
                Ok(()) => return Ok(()),
                Err(error) if !direct_api_context_unavailable(&error) => {
                    return Err(ServiceError::internal(error)
                        .with_code(ErrorCode::Database)
                        .with_operation("put_runtime_config.pg_sync_runtime_config"));
                }
                Err(_) => {
                    if let Some(pg_pool) = self.pg_pool.as_ref() {
                        write_runtime_config_pg(pg_pool, obj)?;
                    } else {
                        let conn = self.db.lock().map_err(|e| {
                            ServiceError::internal(format!("{e}"))
                                .with_code(ErrorCode::Database)
                                .with_operation("put_runtime_config.lock")
                        })?;
                        write_runtime_config(&conn, obj)?;
                    }
                    return Ok(());
                }
            }
        }

        let value_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        match crate::services::discord::internal_api::set_kv_value("runtime-config", &value_str) {
            Ok(()) => return Ok(()),
            Err(error) if !direct_api_context_unavailable(&error) => {
                return Err(ServiceError::internal(error)
                    .with_code(ErrorCode::Database)
                    .with_operation("put_runtime_config.upsert_runtime_config"));
            }
            Err(_) => {}
        }

        if let Some(pg_pool) = self.pg_pool.as_ref() {
            upsert_runtime_config_value_pg(pg_pool, &value_str)?;
            return Ok(());
        }

        let conn = self.db.lock().map_err(|e| {
            ServiceError::internal(format!("{e}"))
                .with_code(ErrorCode::Database)
                .with_operation("put_runtime_config.lock")
        })?;
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('runtime-config', ?1)",
            [&value_str],
        )
        .map_err(|e| {
            ServiceError::internal(format!("{e}"))
                .with_code(ErrorCode::Database)
                .with_operation("put_runtime_config.upsert_runtime_config")
        })?;

        Ok(())
    }
}

fn direct_api_context_unavailable(error: &str) -> bool {
    error.contains("direct runtime API context is unavailable")
}

fn load_runtime_config_pg(pg_pool: &sqlx::PgPool) -> ServiceResult<Value> {
    let value = crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |bridge_pool| async move {
            sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
                .bind("runtime-config")
                .fetch_optional(&bridge_pool)
                .await
                .map(|value| value.unwrap_or_else(|| "{}".to_string()))
                .map_err(|error| format!("load pg runtime-config: {error}"))
        },
        |error| error,
    )
    .map_err(|error| {
        ServiceError::internal(error)
            .with_code(ErrorCode::Database)
            .with_operation("get_runtime_config.load_runtime_config_pg")
    })?;
    Ok(serde_json::from_str(&value).unwrap_or_else(|_| json!({})))
}

fn insert_runtime_number(map: &mut Map<String, Value>, key: &str, value: Option<u64>) {
    if let Some(number) = value {
        map.insert(key.to_string(), json!(number));
    }
}

fn insert_runtime_bool(map: &mut Map<String, Value>, key: &str, value: Option<bool>) {
    if let Some(flag) = value {
        map.insert(key.to_string(), json!(flag));
    }
}

fn insert_runtime_string(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(text) = value.map(str::trim).filter(|text| !text.is_empty()) {
        map.insert(key.to_string(), json!(text));
    }
}

fn runtime_config_yaml_overrides(config: &crate::config::Config) -> Map<String, Value> {
    let mut overrides = Map::new();
    insert_runtime_number(
        &mut overrides,
        "dispatchPollSec",
        config.runtime.dispatch_poll_sec,
    );
    insert_runtime_number(
        &mut overrides,
        "agentSyncSec",
        config.runtime.agent_sync_sec,
    );
    insert_runtime_number(
        &mut overrides,
        "githubIssueSyncSec",
        config.runtime.github_issue_sync_sec,
    );
    insert_runtime_number(
        &mut overrides,
        "claudeRateLimitPollSec",
        config.runtime.claude_rate_limit_poll_sec,
    );
    insert_runtime_number(
        &mut overrides,
        "codexRateLimitPollSec",
        config.runtime.codex_rate_limit_poll_sec,
    );
    insert_runtime_number(
        &mut overrides,
        "issueTriagePollSec",
        config.runtime.issue_triage_poll_sec,
    );
    insert_runtime_number(
        &mut overrides,
        "ceoWarnDepth",
        config.runtime.ceo_warn_depth,
    );
    insert_runtime_number(&mut overrides, "maxRetries", config.runtime.max_retries);
    insert_runtime_number(
        &mut overrides,
        "maxEntryRetries",
        config.runtime.max_entry_retries,
    );
    insert_runtime_number(
        &mut overrides,
        "staleDispatchedGraceMin",
        config.runtime.stale_dispatched_grace_min,
    );
    insert_runtime_string(
        &mut overrides,
        "staleDispatchedTerminalStatuses",
        config.runtime.stale_dispatched_terminal_statuses.as_deref(),
    );
    insert_runtime_bool(
        &mut overrides,
        "staleDispatchedRecoverNullDispatch",
        config.runtime.stale_dispatched_recover_null_dispatch,
    );
    insert_runtime_bool(
        &mut overrides,
        "staleDispatchedRecoverMissingDispatch",
        config.runtime.stale_dispatched_recover_missing_dispatch,
    );
    insert_runtime_number(
        &mut overrides,
        "reviewReminderMin",
        config.runtime.review_reminder_min,
    );
    insert_runtime_number(
        &mut overrides,
        "rateLimitWarningPct",
        config.runtime.rate_limit_warning_pct,
    );
    insert_runtime_number(
        &mut overrides,
        "rateLimitDangerPct",
        config.runtime.rate_limit_danger_pct,
    );
    insert_runtime_number(
        &mut overrides,
        "githubRepoCacheSec",
        config.runtime.github_repo_cache_sec,
    );
    insert_runtime_number(
        &mut overrides,
        "rateLimitStaleSec",
        config.runtime.rate_limit_stale_sec,
    );
    overrides
}

fn runtime_config_defaults_map(config: &crate::config::Config) -> Map<String, Value> {
    let mut defaults = json!({
        "dispatchPollSec": 30,
        "agentSyncSec": 300,
        "githubIssueSyncSec": 900,
        "claudeRateLimitPollSec": 120,
        "codexRateLimitPollSec": 120,
        "issueTriagePollSec": 300,
        "ceoWarnDepth": 3,
        "maxRetries": 3,
        "maxEntryRetries": 3,
        "staleDispatchedGraceMin": 2,
        "staleDispatchedTerminalStatuses": "cancelled,failed",
        "staleDispatchedRecoverNullDispatch": true,
        "staleDispatchedRecoverMissingDispatch": true,
        "reviewReminderMin": 30,
        "rateLimitWarningPct": 80,
        "rateLimitDangerPct": 95,
        "githubRepoCacheSec": 300,
        "rateLimitStaleSec": 600,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    for (key, value) in runtime_config_yaml_overrides(config) {
        defaults.insert(key, value);
    }
    defaults
}

pub fn runtime_config_defaults(config: &crate::config::Config) -> Value {
    Value::Object(runtime_config_defaults_map(config))
}

pub fn runtime_config_u64(
    conn: &libsql_rusqlite::Connection,
    config: &crate::config::Config,
    key: &str,
) -> Option<u64> {
    let saved = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    if let Some(value) = saved
        .as_ref()
        .and_then(|value| value.get(key))
        .and_then(Value::as_u64)
    {
        return Some(value);
    }

    runtime_config_defaults_map(config)
        .get(key)
        .and_then(Value::as_u64)
}

fn runtime_scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn write_runtime_config_internal_api(values: &Map<String, Value>) -> Result<(), String> {
    let value_str =
        serde_json::to_string(&Value::Object(values.clone())).unwrap_or_else(|_| "{}".to_string());
    crate::services::discord::internal_api::set_kv_value("runtime-config", &value_str)?;
    for key in RUNTIME_CONFIG_KEYS {
        crate::services::discord::internal_api::delete_kv_value(key)?;
    }
    for (key, value) in values {
        if let Some(text) = runtime_scalar_to_string(value) {
            crate::services::discord::internal_api::set_kv_value(key, &text)?;
        }
    }
    Ok(())
}

fn upsert_runtime_config_value_pg(pg_pool: &sqlx::PgPool, value_str: &str) -> ServiceResult<()> {
    let value_str = value_str.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |bridge_pool| async move {
            sqlx::query(
                "INSERT INTO kv_meta (key, value, expires_at)
                 VALUES ($1, $2, NULL)
                 ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value,
                         expires_at = EXCLUDED.expires_at",
            )
            .bind("runtime-config")
            .bind(&value_str)
            .execute(&bridge_pool)
            .await
            .map_err(|error| format!("upsert pg runtime-config: {error}"))?;
            Ok(())
        },
        |error| error,
    )
    .map_err(|error| {
        ServiceError::internal(error)
            .with_code(ErrorCode::Database)
            .with_operation("put_runtime_config.upsert_runtime_config_pg")
    })
}

fn write_runtime_config_pg(
    pg_pool: &sqlx::PgPool,
    values: &Map<String, Value>,
) -> ServiceResult<()> {
    let value_str =
        serde_json::to_string(&Value::Object(values.clone())).unwrap_or_else(|_| "{}".to_string());
    let scalar_values = values
        .iter()
        .filter_map(|(key, value)| runtime_scalar_to_string(value).map(|text| (key.clone(), text)))
        .collect::<Vec<_>>();

    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |bridge_pool| async move {
            let mut tx = bridge_pool
                .begin()
                .await
                .map_err(|error| format!("begin runtime-config pg tx: {error}"))?;

            sqlx::query(
                "INSERT INTO kv_meta (key, value, expires_at)
                 VALUES ($1, $2, NULL)
                 ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value,
                         expires_at = EXCLUDED.expires_at",
            )
            .bind("runtime-config")
            .bind(&value_str)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("upsert pg runtime-config: {error}"))?;

            for key in RUNTIME_CONFIG_KEYS {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(*key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| format!("delete pg runtime-config scalar {key}: {error}"))?;
            }

            for (key, text) in scalar_values {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value, expires_at)
                     VALUES ($1, $2, NULL)
                     ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                )
                .bind(&key)
                .bind(&text)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("upsert pg runtime-config scalar {key}: {error}"))?;
            }

            tx.commit()
                .await
                .map_err(|error| format!("commit runtime-config pg tx: {error}"))?;
            Ok(())
        },
        |error| error,
    )
    .map_err(|error| {
        ServiceError::internal(error)
            .with_code(ErrorCode::Database)
            .with_operation("put_runtime_config.write_runtime_config_pg")
    })
}

fn write_runtime_config(
    conn: &libsql_rusqlite::Connection,
    values: &Map<String, Value>,
) -> ServiceResult<()> {
    let value_str =
        serde_json::to_string(&Value::Object(values.clone())).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('runtime-config', ?1)",
        [&value_str],
    )
    .map_err(|error| {
        ServiceError::internal(format!("{error}"))
            .with_code(ErrorCode::Database)
            .with_operation("write_runtime_config.upsert_runtime_config")
    })?;

    for key in RUNTIME_CONFIG_KEYS {
        conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("write_runtime_config.delete_legacy_key")
                    .with_context("key", key)
            })?;
    }
    for (key, value) in values {
        if let Some(text) = runtime_scalar_to_string(value) {
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![key, text],
            )
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("write_runtime_config.upsert_scalar")
                    .with_context("key", key)
            })?;
        }
    }

    Ok(())
}

pub fn seed_runtime_config_defaults(
    conn: &libsql_rusqlite::Connection,
    config: &crate::config::Config,
) {
    let saved_obj = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned());
    let target = seeded_runtime_config_map(saved_obj, config);

    if let Err(error) = write_runtime_config(conn, &target) {
        tracing::warn!("[settings] failed to seed runtime config defaults: {error}");
    }
}

pub async fn seed_runtime_config_defaults_pg(
    pool: &sqlx::PgPool,
    config: &crate::config::Config,
) -> Result<(), String> {
    let saved_obj = sqlx::query_scalar::<_, String>(
        "SELECT value
         FROM kv_meta
         WHERE key = $1
         LIMIT 1",
    )
    .bind("runtime-config")
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load pg runtime-config seed state: {error}"))?
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .and_then(|value| value.as_object().cloned());
    let target = seeded_runtime_config_map(saved_obj, config);
    write_runtime_config_pg(pool, &target).map_err(|error| error.to_string())
}

fn seeded_runtime_config_map(
    saved_obj: Option<Map<String, Value>>,
    config: &crate::config::Config,
) -> Map<String, Value> {
    let defaults = runtime_config_defaults_map(config);
    let yaml_overrides = runtime_config_yaml_overrides(config);

    let mut current = if config.runtime.reset_overrides_on_restart {
        defaults.clone()
    } else {
        saved_obj.clone().unwrap_or_else(|| defaults.clone())
    };

    if !config.runtime.reset_overrides_on_restart {
        for (key, value) in yaml_overrides {
            current.insert(key, value);
        }
    }

    if config.runtime.reset_overrides_on_restart || saved_obj.is_none() || current != defaults {
        current
    } else {
        saved_obj.unwrap_or(current)
    }
}
