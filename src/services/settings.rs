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
    "reviewReminderMin",
    "rateLimitWarningPct",
    "rateLimitDangerPct",
    "githubRepoCacheSec",
    "rateLimitStaleSec",
];

#[derive(Clone)]
pub struct SettingsService {
    db: Db,
    config: Arc<crate::config::Config>,
}

impl SettingsService {
    pub fn new(db: Db, config: Arc<crate::config::Config>) -> Self {
        Self { db, config }
    }

    pub fn get_runtime_config(&self) -> ServiceResult<Value> {
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

        let saved: Value = serde_json::from_str(&value).unwrap_or_else(|_| json!({}));
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
        let conn = self.db.lock().map_err(|e| {
            ServiceError::internal(format!("{e}"))
                .with_code(ErrorCode::Database)
                .with_operation("put_runtime_config.lock")
        })?;

        if let Some(obj) = body.as_object() {
            write_runtime_config(&conn, obj)?;
            return Ok(());
        }

        let value_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
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

fn insert_runtime_number(map: &mut Map<String, Value>, key: &str, value: Option<u64>) {
    if let Some(number) = value {
        map.insert(key.to_string(), json!(number));
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

fn runtime_scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn write_runtime_config(
    conn: &rusqlite::Connection,
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
                rusqlite::params![key, text],
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

pub fn seed_runtime_config_defaults(conn: &rusqlite::Connection, config: &crate::config::Config) {
    let defaults = runtime_config_defaults_map(config);
    let yaml_overrides = runtime_config_yaml_overrides(config);
    let saved_obj = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned());

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
        if let Err(error) = write_runtime_config(conn, &current) {
            tracing::warn!("[settings] failed to seed runtime config defaults: {error}");
        }
    } else if let Some(saved) = saved_obj {
        if let Err(error) = write_runtime_config(conn, &saved) {
            tracing::warn!("[settings] failed to preserve runtime config defaults: {error}");
        }
    }
}
