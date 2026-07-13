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
    "dispatchRateLimitGateEnabled",
    "dispatchRateLimitGateDangerPct",
];

pub(crate) fn is_runtime_config_key(key: &str) -> bool {
    RUNTIME_CONFIG_KEYS.contains(&key)
}

pub(crate) const RUNTIME_CONFIG_EXPLICIT_KEYS_META: &str = "__runtimeConfigExplicitKeys";

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
        // #3561 — operator override for the per-hour relay-loss signal alert
        // threshold. Empty/absent ⇒ conservative per-signal code defaults.
        "kanban_relay_alert_threshold",
        "quality",
        "릴레이 누락 신호 경보 임계",
        "Relay Signal Alert Threshold",
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
    pub explicit_keys: Vec<String>,
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
                if key == RUNTIME_CONFIG_EXPLICIT_KEYS_META {
                    continue;
                }
                current.insert(key.clone(), value.clone());
            }
        }
        let mut explicit_keys = saved
            .as_object()
            .map(explicit_runtime_config_keys)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        explicit_keys.sort();

        Ok(RuntimeConfigResponse {
            current,
            defaults,
            explicit_keys,
        })
    }

    pub async fn put_runtime_config(&self, body: Value) -> ServiceResult<SettingsOkResponse> {
        let pool = self.pg_pool("put_runtime_config.pg_pool")?;
        if let Some(values) = body.as_object() {
            let marked = with_explicit_runtime_config_keys(values.clone());
            let value_str = serde_json::to_string(&Value::Object(marked.clone()))
                .unwrap_or_else(|_| "{}".to_string());
            write_runtime_config_pg_async(pool, &value_str).await?;
        } else {
            let value_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
            write_runtime_config_pg_async(pool, &value_str).await?;
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
        "kanban_relay_alert_threshold" => stringified_number(config.kanban.relay_alert_threshold),
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeConfigKvWrite {
    UpsertBlob(String),
    DeleteScalar(&'static str),
}

fn runtime_config_write_plan(value: String) -> Vec<RuntimeConfigKvWrite> {
    std::iter::once(RuntimeConfigKvWrite::UpsertBlob(value))
        .chain(
            RUNTIME_CONFIG_KEYS
                .iter()
                .copied()
                .map(RuntimeConfigKvWrite::DeleteScalar),
        )
        .collect()
}

async fn write_runtime_config_pg_async(pool: &sqlx::PgPool, value_str: &str) -> ServiceResult<()> {
    let mut tx = pool.begin().await.map_err(|error| {
        ServiceError::internal(format!("begin runtime-config tx: {error}"))
            .with_code(ErrorCode::Database)
            .with_operation("put_runtime_config.begin_pg_tx")
    })?;

    for write in runtime_config_write_plan(value_str.to_string()) {
        match write {
            RuntimeConfigKvWrite::UpsertBlob(value) => {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value, expires_at)
                     VALUES ($1, $2, NULL)
                     ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                )
                .bind("runtime-config")
                .bind(value)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    ServiceError::internal(format!("{error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("put_runtime_config.upsert_runtime_config_pg")
                })?;
            }
            RuntimeConfigKvWrite::DeleteScalar(key) => {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| {
                        ServiceError::internal(format!("{error}"))
                            .with_code(ErrorCode::Database)
                            .with_operation("put_runtime_config.delete_scalar_pg")
                            .with_context("key", key)
                    })?;
            }
        }
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
    insert_runtime_bool(
        &mut overrides,
        "dispatchRateLimitGateEnabled",
        config.runtime.dispatch_rate_limit_gate_enabled,
    );
    insert_runtime_number(
        &mut overrides,
        "dispatchRateLimitGateDangerPct",
        config
            .runtime
            .dispatch_rate_limit_gate_danger_pct
            .map(u64::from),
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
        "dispatchRateLimitGateEnabled": true,
        "dispatchRateLimitGateDangerPct": 100,
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

pub(crate) fn explicit_runtime_config_keys(values: &Map<String, Value>) -> HashSet<String> {
    values
        .get(RUNTIME_CONFIG_EXPLICIT_KEYS_META)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|key| RUNTIME_CONFIG_KEYS.contains(key))
        .map(ToOwned::to_owned)
        .collect()
}

fn with_explicit_runtime_config_keys(mut values: Map<String, Value>) -> Map<String, Value> {
    let explicit_keys = explicit_runtime_config_keys(&values);
    let mut keys = explicit_keys.into_iter().collect::<Vec<_>>();
    keys.sort();
    values.insert(
        RUNTIME_CONFIG_EXPLICIT_KEYS_META.to_string(),
        Value::Array(keys.into_iter().map(Value::String).collect()),
    );
    values
}

fn write_runtime_config_pg(
    pg_pool: &sqlx::PgPool,
    values: &Map<String, Value>,
) -> ServiceResult<()> {
    let value_str =
        serde_json::to_string(&Value::Object(values.clone())).unwrap_or_else(|_| "{}".to_string());
    let write_plan = runtime_config_write_plan(value_str);

    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |bridge_pool| async move {
            let mut tx = bridge_pool
                .begin()
                .await
                .map_err(|error| format!("begin runtime-config pg tx: {error}"))?;

            for write in write_plan {
                match write {
                    RuntimeConfigKvWrite::UpsertBlob(value) => {
                        sqlx::query(
                            "INSERT INTO kv_meta (key, value, expires_at)
                             VALUES ($1, $2, NULL)
                             ON CONFLICT (key) DO UPDATE
                                 SET value = EXCLUDED.value,
                                     expires_at = EXCLUDED.expires_at",
                        )
                        .bind("runtime-config")
                        .bind(value)
                        .execute(&mut *tx)
                        .await
                        .map_err(|error| format!("upsert pg runtime-config: {error}"))?;
                    }
                    RuntimeConfigKvWrite::DeleteScalar(key) => {
                        sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                            .bind(key)
                            .execute(&mut *tx)
                            .await
                            .map_err(|error| {
                                format!("delete pg runtime-config scalar {key}: {error}")
                            })?;
                    }
                }
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
    let has_yaml_overrides = !yaml_overrides.is_empty();
    let explicit_keys = saved_obj
        .as_ref()
        .map(explicit_runtime_config_keys)
        .unwrap_or_default();

    let has_explicit_meta = saved_obj
        .as_ref()
        .is_some_and(|obj| obj.contains_key(RUNTIME_CONFIG_EXPLICIT_KEYS_META));
    let mut current = if config.runtime.reset_overrides_on_restart {
        defaults.clone()
    } else if has_explicit_meta {
        let mut seeded = defaults.clone();
        if let Some(saved_obj) = saved_obj.as_ref() {
            for key in &explicit_keys {
                if let Some(value) = saved_obj.get(key) {
                    seeded.insert(key.clone(), value.clone());
                }
            }
        }
        seeded
    } else {
        saved_obj.clone().unwrap_or_else(|| defaults.clone())
    };
    current.remove(RUNTIME_CONFIG_EXPLICIT_KEYS_META);

    if !config.runtime.reset_overrides_on_restart {
        for (key, value) in yaml_overrides {
            if explicit_keys.contains(&key) {
                continue;
            }
            current.insert(key, value);
        }
    }
    if !config.runtime.reset_overrides_on_restart && !explicit_keys.is_empty() {
        let mut keys = explicit_keys.into_iter().collect::<Vec<_>>();
        keys.sort();
        current.insert(
            RUNTIME_CONFIG_EXPLICIT_KEYS_META.to_string(),
            Value::Array(keys.into_iter().map(Value::String).collect()),
        );
    }

    if config.runtime.reset_overrides_on_restart
        || saved_obj.is_none()
        || has_yaml_overrides
        || current != defaults
    {
        current
    } else {
        saved_obj.unwrap_or(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TestDatabase {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestDatabase {
        async fn create() -> Self {
            let admin_url = crate::dispatch::test_support::postgres_admin_database_url();
            let database_name = format!("agentdesk_settings_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!(
                "{}/{}",
                crate::dispatch::test_support::postgres_base_database_url(),
                database_name
            );
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "settings runtime-config pg tests",
            )
            .await
            .expect("create settings postgres test db");
            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect(&self) -> sqlx::PgPool {
            let pool = crate::db::postgres::connect_test_pool(
                &self.database_url,
                "settings runtime-config pg tests",
            )
            .await
            .expect("connect settings postgres test db");
            sqlx::query(
                "CREATE TABLE kv_meta (
                    key TEXT PRIMARY KEY,
                    value TEXT,
                    expires_at TIMESTAMPTZ
                )",
            )
            .execute(&pool)
            .await
            .expect("create settings kv_meta table");
            pool
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "settings runtime-config pg tests",
            )
            .await
            .expect("drop settings postgres test db");
        }
    }

    async fn upsert_test_kv(pool: &sqlx::PgPool, key: &str, value: &str) {
        sqlx::query(
            "INSERT INTO kv_meta (key, value, expires_at)
             VALUES ($1, $2, NULL)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("upsert test kv_meta {key}: {error}"));
    }

    async fn seed_runtime_scalar_mirrors(pool: &sqlx::PgPool) {
        for key in RUNTIME_CONFIG_KEYS {
            upsert_test_kv(pool, key, &format!("legacy-{key}")).await;
        }
    }

    async fn assert_runtime_blob_and_no_mirrors(pool: &sqlx::PgPool, expected: &Value) {
        let blob = sqlx::query_scalar::<_, String>(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
        )
        .fetch_one(pool)
        .await
        .expect("load authoritative runtime-config blob");
        assert_eq!(
            serde_json::from_str::<Value>(&blob).expect("parse authoritative runtime-config blob"),
            *expected
        );

        for key in RUNTIME_CONFIG_KEYS {
            let scalar =
                sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
                    .bind(key)
                    .fetch_optional(pool)
                    .await
                    .unwrap_or_else(|error| panic!("load runtime scalar {key}: {error}"));
            assert_eq!(
                scalar, None,
                "legacy runtime scalar {key} must stay deleted"
            );
        }
    }

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

    #[test]
    fn seeded_runtime_config_applies_yaml_overrides_over_seeded_defaults() {
        let saved_obj = runtime_config_defaults_map(&crate::config::Config::default());
        let mut config = crate::config::Config::default();
        config.runtime.dispatch_rate_limit_gate_enabled = Some(false);
        config.runtime.dispatch_rate_limit_gate_danger_pct = Some(95);
        config.runtime.rate_limit_stale_sec = Some(900);

        let seeded = seeded_runtime_config_map(Some(saved_obj), &config);

        assert_eq!(
            seeded.get("dispatchRateLimitGateEnabled"),
            Some(&json!(false))
        );
        assert_eq!(
            seeded.get("dispatchRateLimitGateDangerPct"),
            Some(&json!(95))
        );
        assert_eq!(seeded.get("rateLimitStaleSec"), Some(&json!(900)));
    }

    #[test]
    fn seeded_runtime_config_preserves_explicit_api_overrides_over_yaml() {
        let mut saved_obj = runtime_config_defaults_map(&crate::config::Config::default());
        saved_obj.insert("dispatchRateLimitGateEnabled".to_string(), json!(true));
        saved_obj.insert("dispatchRateLimitGateDangerPct".to_string(), json!(100));
        saved_obj.insert("rateLimitStaleSec".to_string(), json!(600));
        saved_obj.insert(
            RUNTIME_CONFIG_EXPLICIT_KEYS_META.to_string(),
            json!([
                "dispatchRateLimitGateEnabled",
                "dispatchRateLimitGateDangerPct",
                "rateLimitStaleSec"
            ]),
        );
        let mut config = crate::config::Config::default();
        config.runtime.dispatch_rate_limit_gate_enabled = Some(false);
        config.runtime.dispatch_rate_limit_gate_danger_pct = Some(95);
        config.runtime.rate_limit_stale_sec = Some(900);

        let seeded = seeded_runtime_config_map(Some(saved_obj), &config);

        assert_eq!(
            seeded.get("dispatchRateLimitGateEnabled"),
            Some(&json!(true))
        );
        assert_eq!(
            seeded.get("dispatchRateLimitGateDangerPct"),
            Some(&json!(100))
        );
        assert_eq!(seeded.get("rateLimitStaleSec"), Some(&json!(600)));
        assert!(
            seeded
                .get(RUNTIME_CONFIG_EXPLICIT_KEYS_META)
                .and_then(Value::as_array)
                .is_some_and(|keys| keys.len() == 3)
        );
    }

    #[test]
    fn seeded_runtime_config_rebases_non_explicit_saved_values() {
        let mut saved_obj = runtime_config_defaults_map(&crate::config::Config::default());
        saved_obj.insert("dispatchRateLimitGateEnabled".to_string(), json!(true));
        saved_obj.insert("dispatchRateLimitGateDangerPct".to_string(), json!(100));
        saved_obj.insert("rateLimitStaleSec".to_string(), json!(600));
        saved_obj.insert(
            RUNTIME_CONFIG_EXPLICIT_KEYS_META.to_string(),
            json!(["rateLimitStaleSec"]),
        );
        let mut config = crate::config::Config::default();
        config.runtime.dispatch_rate_limit_gate_enabled = Some(false);
        config.runtime.dispatch_rate_limit_gate_danger_pct = Some(95);
        config.runtime.rate_limit_stale_sec = Some(900);

        let seeded = seeded_runtime_config_map(Some(saved_obj), &config);

        assert_eq!(
            seeded.get("dispatchRateLimitGateEnabled"),
            Some(&json!(false)),
            "non-explicit full-dashboard save values must not freeze YAML/default gate settings"
        );
        assert_eq!(
            seeded.get("dispatchRateLimitGateDangerPct"),
            Some(&json!(95)),
            "non-explicit gate threshold follows YAML/default baseline"
        );
        assert_eq!(
            seeded.get("rateLimitStaleSec"),
            Some(&json!(600)),
            "explicit API override still wins over YAML"
        );
    }

    #[test]
    fn explicit_runtime_config_keys_respects_payload_metadata() {
        let mut values = runtime_config_defaults_map(&crate::config::Config::default());
        values.insert(
            RUNTIME_CONFIG_EXPLICIT_KEYS_META.to_string(),
            json!(["rateLimitStaleSec"]),
        );

        let keys = explicit_runtime_config_keys(&values);

        assert_eq!(keys.len(), 1);
        assert!(keys.contains("rateLimitStaleSec"));
        assert!(!keys.contains("dispatchRateLimitGateEnabled"));
    }

    #[test]
    fn explicit_runtime_config_keys_without_metadata_is_empty() {
        let values = runtime_config_defaults_map(&crate::config::Config::default());

        let keys = explicit_runtime_config_keys(&values);

        assert!(
            keys.is_empty(),
            "full saved runtime-config values are not explicit unless the metadata says so"
        );
    }

    #[test]
    fn runtime_config_key_lookup_exposes_only_known_value_keys() {
        assert!(is_runtime_config_key("maxEntryRetries"));
        assert!(is_runtime_config_key("staleDispatchedRecoverNullDispatch"));
        assert!(!is_runtime_config_key("runtime-config"));
        assert!(!is_runtime_config_key(RUNTIME_CONFIG_EXPLICIT_KEYS_META));
        assert!(!is_runtime_config_key("privateBlobField"));
    }

    #[test]
    fn runtime_config_write_plan_has_one_blob_authority_and_cleanup_only() {
        let plan = runtime_config_write_plan(r#"{"maxEntryRetries":7}"#.to_string());

        assert_eq!(
            plan.first(),
            Some(&RuntimeConfigKvWrite::UpsertBlob(
                r#"{"maxEntryRetries":7}"#.to_string()
            ))
        );
        assert_eq!(plan.len(), RUNTIME_CONFIG_KEYS.len() + 1);
        assert!(
            plan[1..]
                .iter()
                .all(|write| matches!(write, RuntimeConfigKvWrite::DeleteScalar(_)))
        );
        for key in RUNTIME_CONFIG_KEYS {
            assert!(plan.contains(&RuntimeConfigKvWrite::DeleteScalar(*key)));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_runtime_config_object_and_non_object_keep_only_blob_authority() {
        let database = TestDatabase::create().await;
        let pool = database.connect().await;
        upsert_test_kv(&pool, "runtime-config", r#"{"old":true}"#).await;
        seed_runtime_scalar_mirrors(&pool).await;
        let service = SettingsService::new(
            Some(pool.clone()),
            Arc::new(crate::config::Config::default()),
        );

        service
            .put_runtime_config(json!({
                "maxEntryRetries": 7,
                "staleDispatchedRecoverNullDispatch": false,
                "staleDispatchedTerminalStatuses": ["failed", "expired"]
            }))
            .await
            .expect("write object runtime-config through SettingsService");
        assert_runtime_blob_and_no_mirrors(
            &pool,
            &json!({
                "maxEntryRetries": 7,
                "staleDispatchedRecoverNullDispatch": false,
                "staleDispatchedTerminalStatuses": ["failed", "expired"],
                "__runtimeConfigExplicitKeys": []
            }),
        )
        .await;

        seed_runtime_scalar_mirrors(&pool).await;
        service
            .put_runtime_config(json!(["non-object", 7, false]))
            .await
            .expect("write non-object runtime-config through SettingsService");
        assert_runtime_blob_and_no_mirrors(&pool, &json!(["non-object", 7, false])).await;

        drop(service);
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_runtime_config_writer_keeps_blob_authority_without_mirror_reinsertion() {
        let database = TestDatabase::create().await;
        let pool = database.connect().await;
        upsert_test_kv(&pool, "runtime-config", r#"{"maxEntryRetries":99}"#).await;
        seed_runtime_scalar_mirrors(&pool).await;

        seed_runtime_config_defaults_pg(&pool, &crate::config::Config::default())
            .await
            .expect("seed runtime-config through startup sync writer");

        assert_runtime_blob_and_no_mirrors(&pool, &json!({"maxEntryRetries": 99})).await;

        pool.close().await;
        database.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_config_delete_failure_rolls_back_blob_and_every_mirror() {
        let database = TestDatabase::create().await;
        let pool = database.connect().await;
        let old_blob = r#"{"maxEntryRetries":4,"marker":"old"}"#;
        upsert_test_kv(&pool, "runtime-config", old_blob).await;
        seed_runtime_scalar_mirrors(&pool).await;
        sqlx::query(
            r#"
            CREATE FUNCTION reject_runtime_scalar_delete() RETURNS trigger AS $$
            BEGIN
                IF OLD.key = 'maxEntryRetries' THEN
                    RAISE EXCEPTION 'injected runtime scalar delete failure';
                END IF;
                RETURN OLD;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .execute(&pool)
        .await
        .expect("install runtime scalar delete fault function");
        sqlx::query(
            "CREATE TRIGGER reject_runtime_scalar_delete_trigger
             BEFORE DELETE ON kv_meta
             FOR EACH ROW EXECUTE FUNCTION reject_runtime_scalar_delete()",
        )
        .execute(&pool)
        .await
        .expect("install runtime scalar delete fault trigger");
        let service = SettingsService::new(
            Some(pool.clone()),
            Arc::new(crate::config::Config::default()),
        );

        service
            .put_runtime_config(json!({"maxEntryRetries": 12}))
            .await
            .expect_err("injected delete failure must abort runtime-config write");

        let blob = sqlx::query_scalar::<_, String>(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
        )
        .fetch_one(&pool)
        .await
        .expect("load blob after rollback");
        assert_eq!(
            blob, old_blob,
            "blob upsert must roll back with mirror deletes"
        );
        for key in RUNTIME_CONFIG_KEYS {
            let scalar =
                sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
                    .bind(key)
                    .fetch_one(&pool)
                    .await
                    .unwrap_or_else(|load_error| {
                        panic!("load runtime scalar {key} after rollback: {load_error}")
                    });
            assert_eq!(
                scalar,
                format!("legacy-{key}"),
                "runtime scalar {key} must survive transaction rollback"
            );
        }

        drop(service);
        pool.close().await;
        database.drop().await;
    }
}
