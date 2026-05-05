use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::Row;

use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

const RETIRED_SETTINGS_JSON_KEYS: &[&str] = &[
    "autoUpdateEnabled",
    "autoUpdateNoticePending",
    "oauthAutoSwap",
    "officeWorkflowPack",
    "providerModelConfig",
    "messengerChannels",
    "officePackProfiles",
    "officePackHydratedPacks",
];

const RETIRED_CONFIG_KEYS: &[&str] = &[
    "max_chain_depth",
    "context_clear_percent",
    "context_clear_idle_minutes",
    "counter_model_review_enabled",
    "narrate_progress",
];

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

/// Known individual `kv_meta` config keys surfaced to the dashboard and policy helpers.
/// (key, category, label_ko, label_en, default_value)
/// default_value is seeded into kv_meta on startup if absent.
const CONFIG_KEYS: &[(&str, &str, &str, &str, Option<&str>)] = &[
    (
        "kanban_manager_channel_id",
        "pipeline",
        "칸반매니저 채널 ID",
        "Kanban Manager Channel ID",
        None,
    ),
    (
        "deadlock_manager_channel_id",
        "pipeline",
        "데드락 매니저 채널 ID",
        "Deadlock Manager Channel ID",
        None,
    ),
    (
        "kanban_human_alert_channel_id",
        "pipeline",
        "사람 알림 채널 ID",
        "Human Alert Channel ID",
        None,
    ),
    (
        "agent_quality_monitoring_channel_id",
        "quality",
        "에이전트 품질 알림 채널 ID",
        "Agent Quality Alert Channel ID",
        None,
    ),
    (
        "review_enabled",
        "review",
        "리뷰 활성화",
        "Review Enabled",
        None,
    ),
    (
        "max_review_rounds",
        "review",
        "최대 리뷰 라운드",
        "Max Review Rounds",
        Some("3"),
    ),
    (
        "pm_decision_gate_enabled",
        "pipeline",
        "PM 판단 게이트",
        "PM Decision Gate",
        None,
    ),
    (
        "merge_automation_enabled",
        "automation",
        "자동 머지 활성화",
        "Merge Automation Enabled",
        Some("false"),
    ),
    (
        "merge_strategy",
        "automation",
        "자동 머지 전략",
        "Merge Strategy",
        Some("squash"),
    ),
    (
        "merge_strategy_mode",
        "automation",
        "자동 머지 경로",
        "Merge Strategy Mode",
        Some("direct-first"),
    ),
    (
        "merge_allowed_authors",
        "automation",
        "자동 머지 허용 작성자",
        "Merge Allowed Authors",
        None,
    ),
    ("server_port", "system", "서버 포트", "Server Port", None),
    (
        "requested_timeout_min",
        "timeout",
        "요청됨 타임아웃 (분)",
        "Requested Timeout (min)",
        Some("45"),
    ),
    (
        "in_progress_stale_min",
        "timeout",
        "진행 중 정체 판정 (분)",
        "In-Progress Stale (min)",
        Some("120"),
    ),
    (
        "long_turn_alert_interval_min",
        "timeout",
        "장시간 턴 알림 주기 (분)",
        "Long-Turn Alert Interval (min)",
        Some("30"),
    ),
    (
        "context_compact_percent",
        "context",
        "컨텍스트 compact 임계값 (%)",
        "Context Compact Threshold (%)",
        Some("60"),
    ),
    (
        "context_compact_percent_codex",
        "context",
        "Codex 컨텍스트 compact 임계값 (%)",
        "Codex Context Compact Threshold (%)",
        None,
    ),
    (
        "context_compact_percent_claude",
        "context",
        "Claude 컨텍스트 compact 임계값 (%)",
        "Claude Context Compact Threshold (%)",
        None,
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KvSeedAction {
    Put { key: String, value: String },
    PutIfAbsent { key: String, value: String },
    Delete { key: String },
}

#[derive(Clone)]
pub struct SettingsService {
    pg_pool: Option<sqlx::PgPool>,
    config: Arc<crate::config::Config>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct SettingsDocument(pub Value);

#[derive(Debug, Clone, Serialize)]
pub struct SettingsOkResponse {
    pub ok: bool,
}

impl SettingsOkResponse {
    pub fn ok() -> Self {
        Self { ok: true }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsConfigEntriesResponse {
    pub entries: Vec<SettingsConfigEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsConfigEntry {
    pub key: String,
    pub value: Option<String>,
    pub category: String,
    pub label_ko: String,
    pub label_en: String,
    #[serde(rename = "default")]
    pub default_value: Option<String>,
    pub baseline: Option<String>,
    pub baseline_source: Option<String>,
    pub override_active: bool,
    pub editable: bool,
    pub restart_behavior: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsConfigPatchResponse {
    pub ok: bool,
    pub updated: usize,
    pub rejected: Vec<String>,
}

impl SettingsConfigPatchResponse {
    pub fn new(updated: usize, rejected: Vec<String>) -> Self {
        Self {
            ok: true,
            updated,
            rejected,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeConfigResponse {
    pub current: Map<String, Value>,
    pub defaults: Map<String, Value>,
}

impl SettingsService {
    pub fn new(pg_pool: Option<sqlx::PgPool>, config: Arc<crate::config::Config>) -> Self {
        Self { pg_pool, config }
    }

    fn pg_pool(&self, operation: &'static str) -> ServiceResult<&sqlx::PgPool> {
        self.pg_pool
            .as_ref()
            .ok_or_else(|| pg_unavailable_error(operation))
    }

    pub async fn get_settings(&self) -> ServiceResult<SettingsDocument> {
        let pool = self.pg_pool("get_settings.pg_pool")?;
        let value = sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
            .bind("settings")
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("get_settings.load_pg")
            })?
            .unwrap_or_else(|| "{}".to_string());

        Ok(SettingsDocument(
            serde_json::from_str(&value).unwrap_or_else(|_| json!({})),
        ))
    }

    pub async fn put_settings(&self, body: Value) -> ServiceResult<SettingsOkResponse> {
        let pool = self.pg_pool("put_settings.pg_pool")?;
        let normalized = prune_retired_settings_keys(body);
        let value_str = serde_json::to_string(&normalized).unwrap_or_else(|_| "{}".to_string());

        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind("settings")
        .bind(&value_str)
        .execute(pool)
        .await
        .map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("put_settings.upsert_pg")
        })?;

        Ok(SettingsOkResponse::ok())
    }

    pub async fn get_config_entries(&self) -> ServiceResult<SettingsConfigEntriesResponse> {
        let pool = self.pg_pool("get_config_entries.pg_pool")?;
        let pg_values = load_pg_kv_values(pool).await?;

        let mut entries = Vec::new();
        for (key, category, label_ko, label_en, default_val) in CONFIG_KEYS {
            let stored_value: Option<String> = if is_read_only_config_key(key) {
                None
            } else {
                pg_values.get(*key).cloned()
            };
            let baseline = config_entry_default(self.config.as_ref(), key, *default_val);
            let effective = config_entry_effective_value(key, stored_value, baseline.clone());
            let editable = !is_read_only_config_key(key);
            entries.push(SettingsConfigEntry {
                key: (*key).to_string(),
                value: effective.clone(),
                category: (*category).to_string(),
                label_ko: (*label_ko).to_string(),
                label_en: (*label_en).to_string(),
                default_value: baseline.clone(),
                baseline: baseline.clone(),
                baseline_source: config_entry_baseline_source(
                    self.config.as_ref(),
                    key,
                    *default_val,
                )
                .map(str::to_string),
                override_active: config_entry_override_active(
                    editable,
                    effective.as_deref(),
                    baseline.as_deref(),
                ),
                editable,
                restart_behavior: config_entry_restart_behavior(
                    self.config.as_ref(),
                    key,
                    *default_val,
                )
                .to_string(),
            });
        }

        Ok(SettingsConfigEntriesResponse { entries })
    }

    pub async fn patch_config_entries(
        &self,
        body: Value,
    ) -> ServiceResult<SettingsConfigPatchResponse> {
        let entries = body.as_object().ok_or_else(|| {
            ServiceError::bad_request("expected JSON object")
                .with_code(ErrorCode::Settings)
                .with_operation("patch_config_entries.validate_body")
        })?;
        let allowed: HashSet<&str> = CONFIG_KEYS.iter().map(|(k, _, _, _, _)| *k).collect();
        let pool = self.pg_pool("patch_config_entries.pg_pool")?;
        let mut updated = 0;
        let mut rejected = Vec::new();

        for (key, value) in entries {
            if !allowed.contains(key.as_str()) || is_read_only_config_key(key) {
                rejected.push(key.clone());
                continue;
            }
            let v = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(key)
            .bind(v)
            .execute(pool)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("patch_config_entries.upsert_pg")
                    .with_context("key", key)
            })?;
            updated += 1;
        }

        if !rejected.is_empty() {
            tracing::warn!(
                "patch_config_entries: rejected unknown keys: {:?}",
                rejected
            );
        }

        Ok(SettingsConfigPatchResponse::new(updated, rejected))
    }

    pub async fn get_runtime_config(&self) -> ServiceResult<RuntimeConfigResponse> {
        let pool = self.pg_pool("get_runtime_config.pg_pool")?;
        let saved_raw =
            sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
                .bind("runtime-config")
                .fetch_optional(pool)
                .await
                .map_err(|error| {
                    ServiceError::internal(format!("{error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("get_runtime_config.load_pg")
                })?;
        let saved = saved_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_else(|| json!({}));
        let defaults = runtime_config_defaults_map(self.config.as_ref());

        let mut current = defaults.clone();
        if let Some(saved_obj) = saved.as_object() {
            for (key, value) in saved_obj {
                current.insert(key.clone(), value.clone());
            }
        }

        Ok(RuntimeConfigResponse { current, defaults })
    }

    pub async fn put_runtime_config(&self, body: Value) -> ServiceResult<SettingsOkResponse> {
        let pool = self.pg_pool("put_runtime_config.pg_pool")?;
        let value_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        if let Some(values) = body.as_object() {
            write_runtime_config_pg_async(pool, &value_str, values).await?;
        } else {
            upsert_runtime_config_value_pg_async(pool, &value_str).await?;
        }
        Ok(SettingsOkResponse::ok())
    }
}

fn pg_unavailable_error(operation: &'static str) -> ServiceError {
    ServiceError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Database,
        "postgres pool not configured",
    )
    .with_operation(operation)
}

fn prune_retired_settings_keys(mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
        for key in RETIRED_SETTINGS_JSON_KEYS {
            obj.remove(*key);
        }
    }
    body
}

fn stringified_bool(value: Option<bool>) -> Option<String> {
    value.map(|flag| flag.to_string())
}

fn stringified_number<T: ToString>(value: Option<T>) -> Option<String> {
    value.map(|number| number.to_string())
}

fn yaml_section_value(config: &crate::config::Config, key: &str) -> Option<String> {
    match key {
        "kanban_manager_channel_id" => config.kanban.manager_channel_id.clone(),
        "deadlock_manager_channel_id" => config.kanban.deadlock_manager_channel_id.clone(),
        "kanban_human_alert_channel_id" => config.kanban.human_alert_channel_id.clone(),
        "agent_quality_monitoring_channel_id" => None,
        "review_enabled" => stringified_bool(config.review.enabled),
        "max_review_rounds" => stringified_number(config.review.max_rounds),
        "pm_decision_gate_enabled" => stringified_bool(config.kanban.pm_decision_gate_enabled),
        "merge_automation_enabled" => stringified_bool(config.automation.enabled),
        "merge_strategy" => config.automation.strategy.clone(),
        "merge_strategy_mode" => config.automation.strategy_mode.clone(),
        "merge_allowed_authors" => config.automation.allowed_authors.clone(),
        "requested_timeout_min" => stringified_number(config.runtime.requested_timeout_min),
        "in_progress_stale_min" => stringified_number(config.runtime.in_progress_stale_min),
        "long_turn_alert_interval_min" => {
            stringified_number(config.runtime.long_turn_alert_interval_min)
        }
        "context_compact_percent" => stringified_number(config.runtime.context_compact_percent),
        "context_compact_percent_codex" => {
            stringified_number(config.runtime.context_compact_percent_codex)
        }
        "context_compact_percent_claude" => {
            stringified_number(config.runtime.context_compact_percent_claude)
        }
        _ => None,
    }
}

fn config_entry_default(
    config: &crate::config::Config,
    key: &str,
    hardcoded_default: Option<&str>,
) -> Option<String> {
    match key {
        "server_port" => Some(config.server.port.to_string()),
        _ => yaml_section_value(config, key).or_else(|| hardcoded_default.map(str::to_string)),
    }
}

fn is_read_only_config_key(key: &str) -> bool {
    matches!(key, "server_port")
}

fn config_entry_baseline_source(
    config: &crate::config::Config,
    key: &str,
    hardcoded_default: Option<&str>,
) -> Option<&'static str> {
    match key {
        "server_port" => Some("config"),
        _ if yaml_section_value(config, key).is_some() => Some("yaml"),
        _ if hardcoded_default.is_some() => Some("hardcoded"),
        _ => None,
    }
}

fn config_entry_effective_value(
    key: &str,
    stored_value: Option<String>,
    baseline: Option<String>,
) -> Option<String> {
    if is_read_only_config_key(key) {
        return baseline;
    }
    stored_value.or(baseline)
}

fn config_entry_override_active(
    editable: bool,
    effective: Option<&str>,
    baseline: Option<&str>,
) -> bool {
    if !editable {
        return false;
    }

    match (effective, baseline) {
        (Some(current), Some(default_value)) => current != default_value,
        (Some(_), None) => true,
        _ => false,
    }
}

fn config_entry_restart_behavior(
    config: &crate::config::Config,
    key: &str,
    hardcoded_default: Option<&str>,
) -> &'static str {
    if is_read_only_config_key(key) {
        return "config-only";
    }

    let baseline = config_entry_default(config, key, hardcoded_default);
    if config.runtime.reset_overrides_on_restart {
        return if baseline.is_some() {
            "reset-to-baseline"
        } else {
            "clear-on-restart"
        };
    }

    if yaml_section_value(config, key).is_some() {
        "reseed-from-yaml"
    } else {
        "persist-live-override"
    }
}

pub(crate) fn config_default_seed_actions(config: &crate::config::Config) -> Vec<KvSeedAction> {
    let mut actions = Vec::new();

    for (key, _, _, _, default_val) in CONFIG_KEYS {
        if *key == "server_port" {
            continue;
        }

        let yaml_value = yaml_section_value(config, key);
        let baseline = config_entry_default(config, key, *default_val);

        if config.runtime.reset_overrides_on_restart {
            match baseline {
                Some(value) => actions.push(KvSeedAction::Put {
                    key: (*key).to_string(),
                    value,
                }),
                None => actions.push(KvSeedAction::Delete {
                    key: (*key).to_string(),
                }),
            }
            continue;
        }

        match (yaml_value, baseline) {
            (Some(value), _) => actions.push(KvSeedAction::Put {
                key: (*key).to_string(),
                value,
            }),
            (None, Some(value)) => actions.push(KvSeedAction::PutIfAbsent {
                key: (*key).to_string(),
                value,
            }),
            (None, None) => {}
        }
    }

    actions.push(KvSeedAction::PutIfAbsent {
        key: "workspace_root".to_string(),
        value: env!("CARGO_MANIFEST_DIR").to_string(),
    });

    for key in RETIRED_CONFIG_KEYS {
        actions.push(KvSeedAction::Delete {
            key: (*key).to_string(),
        });
    }

    actions
}

/// Seed default values for CONFIG_KEYS into kv_meta on startup.
///
/// SQLite-backed; retained as a `cfg(test)`-only helper. Production runtime seeds
/// kv_meta defaults via `crate::db::postgres::apply_kv_seed_actions` (PG-only since #1306).
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn seed_config_defaults(conn: &sqlite_test::Connection, config: &crate::config::Config) {
    apply_kv_seed_actions(conn, &config_default_seed_actions(config));
    seed_runtime_config_defaults(conn, config);
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn apply_kv_seed_actions(conn: &sqlite_test::Connection, actions: &[KvSeedAction]) {
    for action in actions {
        match action {
            KvSeedAction::Put { key, value } => {
                conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    sqlite_test::params![key, value],
                )
                .ok();
            }
            KvSeedAction::PutIfAbsent { key, value } => {
                conn.execute(
                    "INSERT OR IGNORE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    sqlite_test::params![key, value],
                )
                .ok();
            }
            KvSeedAction::Delete { key } => {
                conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
                    .ok();
            }
        }
    }
}

async fn load_pg_kv_values(pool: &sqlx::PgPool) -> ServiceResult<HashMap<String, String>> {
    let rows = sqlx::query("SELECT key, value FROM kv_meta")
        .fetch_all(pool)
        .await
        .map_err(|error| {
            ServiceError::internal(format!("load postgres kv_meta: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("settings.load_pg_kv_values")
        })?;
    let mut values = HashMap::with_capacity(rows.len());
    for row in rows {
        values.insert(
            row.get::<String, _>("key"),
            row.get::<Option<String>, _>("value").unwrap_or_default(),
        );
    }
    Ok(values)
}

async fn upsert_runtime_config_value_pg_async(
    pool: &sqlx::PgPool,
    value_str: &str,
) -> ServiceResult<()> {
    sqlx::query(
        "INSERT INTO kv_meta (key, value, expires_at)
         VALUES ($1, $2, NULL)
         ON CONFLICT (key) DO UPDATE
             SET value = EXCLUDED.value,
                 expires_at = EXCLUDED.expires_at",
    )
    .bind("runtime-config")
    .bind(value_str)
    .execute(pool)
    .await
    .map_err(|error| {
        ServiceError::internal(format!("{error}"))
            .with_code(ErrorCode::Database)
            .with_operation("put_runtime_config.upsert_runtime_config_pg")
    })?;
    Ok(())
}

async fn write_runtime_config_pg_async(
    pool: &sqlx::PgPool,
    value_str: &str,
    values: &Map<String, Value>,
) -> ServiceResult<()> {
    let mut tx = pool.begin().await.map_err(|error| {
        ServiceError::internal(format!("begin runtime-config tx: {error}"))
            .with_code(ErrorCode::Database)
            .with_operation("put_runtime_config.begin_pg_tx")
    })?;

    sqlx::query(
        "INSERT INTO kv_meta (key, value, expires_at)
         VALUES ($1, $2, NULL)
         ON CONFLICT (key) DO UPDATE
             SET value = EXCLUDED.value,
                 expires_at = EXCLUDED.expires_at",
    )
    .bind("runtime-config")
    .bind(value_str)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        ServiceError::internal(format!("{error}"))
            .with_code(ErrorCode::Database)
            .with_operation("put_runtime_config.upsert_runtime_config_pg")
    })?;

    for key in RUNTIME_CONFIG_KEYS {
        sqlx::query("DELETE FROM kv_meta WHERE key = $1")
            .bind(*key)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("put_runtime_config.delete_scalar_pg")
                    .with_context("key", key)
            })?;
    }

    for (key, value) in values {
        let Some(text) = runtime_scalar_to_string(value) else {
            continue;
        };
        sqlx::query(
            "INSERT INTO kv_meta (key, value, expires_at)
             VALUES ($1, $2, NULL)
             ON CONFLICT (key) DO UPDATE
                 SET value = EXCLUDED.value,
                     expires_at = EXCLUDED.expires_at",
        )
        .bind(key)
        .bind(&text)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            ServiceError::internal(format!("{error}"))
                .with_code(ErrorCode::Database)
                .with_operation("put_runtime_config.upsert_scalar_pg")
                .with_context("key", key)
        })?;
    }

    tx.commit().await.map_err(|error| {
        ServiceError::internal(format!("{error}"))
            .with_code(ErrorCode::Database)
            .with_operation("put_runtime_config.commit_pg_tx")
    })?;

    Ok(())
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

fn runtime_scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn seed_runtime_config_defaults(
    conn: &sqlite_test::Connection,
    config: &crate::config::Config,
) {
    let saved_obj = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config' LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned());
    let target = seeded_runtime_config_map(saved_obj, config);
    write_runtime_config_sqlite(conn, &target);
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn write_runtime_config_sqlite(conn: &sqlite_test::Connection, values: &Map<String, Value>) {
    let value_str =
        serde_json::to_string(&Value::Object(values.clone())).unwrap_or_else(|_| "{}".to_string());

    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        sqlite_test::params!["runtime-config", value_str],
    )
    .ok();

    for key in RUNTIME_CONFIG_KEYS {
        conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
            .ok();
    }

    for (key, value) in values {
        let Some(text) = runtime_scalar_to_string(value) else {
            continue;
        };
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            sqlite_test::params![key, text],
        )
        .ok();
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_response_dtos_serialize_existing_contract_fields() {
        let response = SettingsConfigEntriesResponse {
            entries: vec![SettingsConfigEntry {
                key: "merge_strategy".to_string(),
                value: Some("rebase".to_string()),
                category: "automation".to_string(),
                label_ko: "자동 머지 전략".to_string(),
                label_en: "Merge Strategy".to_string(),
                default_value: Some("squash".to_string()),
                baseline: Some("squash".to_string()),
                baseline_source: Some("hardcoded".to_string()),
                override_active: true,
                editable: true,
                restart_behavior: "persist-live-override".to_string(),
            }],
        };

        let value = serde_json::to_value(response).expect("serialize settings config response");
        assert_eq!(value["entries"][0]["key"], json!("merge_strategy"));
        assert_eq!(value["entries"][0]["default"], json!("squash"));
        assert_eq!(
            value["entries"][0]["restart_behavior"],
            json!("persist-live-override")
        );
    }

    #[test]
    fn settings_write_response_serializes_ok_contract() {
        let value =
            serde_json::to_value(SettingsOkResponse::ok()).expect("serialize settings ok response");
        assert_eq!(value, json!({"ok": true}));
    }
}
