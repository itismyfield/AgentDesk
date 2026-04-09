use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Map, Value, json};

use super::AppState;

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
];

/// GET /api/settings
pub async fn get_settings(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let value: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'settings'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());

    let parsed: serde_json::Value = serde_json::from_str(&value).unwrap_or(json!({}));

    (StatusCode::OK, Json(parsed))
}

fn prune_retired_settings_keys(mut body: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = body.as_object_mut() {
        for key in RETIRED_SETTINGS_JSON_KEYS {
            obj.remove(*key);
        }
    }
    body
}

/// PUT /api/settings
/// Replaces the stored `kv_meta['settings']` JSON object; callers must send a merged payload
/// if they want to preserve hidden keys. Retired legacy settings keys are stripped server-side.
pub async fn put_settings(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let normalized = prune_retired_settings_keys(body);
    let value_str = serde_json::to_string(&normalized).unwrap_or_else(|_| "{}".to_string());

    if let Err(e) = conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('settings', ?1)",
        [&value_str],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    (StatusCode::OK, Json(json!({"ok": true})))
}

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
        "review_enabled",
        "review",
        "리뷰 활성화",
        "Review Enabled",
        None,
    ),
    (
        "counter_model_review_enabled",
        "review",
        "카운터모델 리뷰 활성화",
        "Counter-Model Review",
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
    (
        "narrate_progress",
        "system",
        "진행 상황 내레이션",
        "Narrate Progress",
        Some("true"),
    ),
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
    "reviewReminderMin",
    "rateLimitWarningPct",
    "rateLimitDangerPct",
    "githubRepoCacheSec",
    "rateLimitStaleSec",
];

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
        "review_enabled" => stringified_bool(config.review.enabled),
        "counter_model_review_enabled" => stringified_bool(config.review.counter_model_enabled),
        "max_review_rounds" => stringified_number(config.review.max_rounds),
        "pm_decision_gate_enabled" => stringified_bool(config.kanban.pm_decision_gate_enabled),
        "merge_automation_enabled" => stringified_bool(config.automation.enabled),
        "merge_strategy" => config.automation.strategy.clone(),
        "merge_allowed_authors" => config.automation.allowed_authors.clone(),
        "requested_timeout_min" => stringified_number(config.runtime.requested_timeout_min),
        "in_progress_stale_min" => stringified_number(config.runtime.in_progress_stale_min),
        "context_compact_percent" => stringified_number(config.runtime.context_compact_percent),
        "context_compact_percent_codex" => {
            stringified_number(config.runtime.context_compact_percent_codex)
        }
        "context_compact_percent_claude" => {
            stringified_number(config.runtime.context_compact_percent_claude)
        }
        "narrate_progress" => stringified_bool(config.runtime.narrate_progress),
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

/// Default runtime config values.
/// Only polling intervals and Rust-only settings live here.
/// Kanban/timeout/review/context settings are in CONFIG_KEYS (individual kv_meta keys).
fn runtime_config_defaults(config: &crate::config::Config) -> serde_json::Value {
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

fn write_runtime_config(conn: &rusqlite::Connection, values: &Map<String, Value>) {
    let value_str =
        serde_json::to_string(&Value::Object(values.clone())).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('runtime-config', ?1)",
        [&value_str],
    )
    .ok();

    for key in RUNTIME_CONFIG_KEYS {
        conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
            .ok();
    }
    for (key, value) in values {
        if let Some(text) = runtime_scalar_to_string(value) {
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, text],
            )
            .ok();
        }
    }
}

fn seed_runtime_config_defaults(conn: &rusqlite::Connection, config: &crate::config::Config) {
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
        write_runtime_config(conn, &current);
    } else if let Some(saved) = saved_obj {
        write_runtime_config(conn, &saved);
    }
}

/// GET /api/settings/config
pub async fn get_config_entries(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let mut entries = Vec::new();
    for (key, category, label_ko, label_en, default_val) in CONFIG_KEYS {
        let value: Option<String> = conn
            .query_row("SELECT value FROM kv_meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .ok();
        entries.push(json!({
            "key": key, "value": value, "category": category,
            "label_ko": label_ko, "label_en": label_en,
            "default": config_entry_default(state.config.as_ref(), key, *default_val),
        }));
    }
    // Only return whitelisted CONFIG_KEYS — unknown kv_meta keys are not exposed.
    (StatusCode::OK, Json(json!({"entries": entries})))
}

/// PATCH /api/settings/config
pub async fn patch_config_entries(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let entries = match body.as_object() {
        Some(obj) => obj,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "expected JSON object"})),
            );
        }
    };
    let allowed: std::collections::HashSet<&str> =
        CONFIG_KEYS.iter().map(|(k, _, _, _, _)| *k).collect();
    let mut updated = 0;
    let mut rejected = Vec::new();
    for (key, value) in entries {
        if !allowed.contains(key.as_str()) {
            rejected.push(key.clone());
            continue;
        }
        let v = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, v],
        )
        .ok();
        updated += 1;
    }
    if !rejected.is_empty() {
        tracing::warn!(
            "patch_config_entries: rejected unknown keys: {:?}",
            rejected
        );
    }
    (
        StatusCode::OK,
        Json(json!({"ok": true, "updated": updated, "rejected": rejected})),
    )
}

/// Seed default values for CONFIG_KEYS into kv_meta on startup.
/// YAML values are treated as startup baseline and overwrite runtime overrides on reboot.
/// When `runtime.reset_overrides_on_restart` is enabled, the entire managed surface resets
/// back to YAML-or-hardcoded defaults.
pub fn seed_config_defaults(conn: &rusqlite::Connection, config: &crate::config::Config) {
    for (key, _, _, _, default_val) in CONFIG_KEYS {
        if *key == "server_port" {
            continue;
        }

        let yaml_value = yaml_section_value(config, key);
        let baseline = config_entry_default(config, key, *default_val);

        if config.runtime.reset_overrides_on_restart {
            match baseline {
                Some(val) => {
                    conn.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                        rusqlite::params![key, val],
                    )
                    .ok();
                }
                None => {
                    conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
                        .ok();
                }
            }
            continue;
        }

        match (yaml_value, baseline) {
            (Some(val), _) => {
                conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    rusqlite::params![key, val],
                )
                .ok();
            }
            (None, Some(val)) => {
                conn.execute(
                    "INSERT OR IGNORE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    rusqlite::params![key, val],
                )
                .ok();
            }
            (None, None) => {}
        }
    }

    seed_runtime_config_defaults(conn, config);

    for key in RETIRED_CONFIG_KEYS {
        conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
            .ok();
    }
}

/// GET /api/settings/runtime-config
pub async fn get_runtime_config(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let value: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());

    let saved: serde_json::Value = serde_json::from_str(&value).unwrap_or(json!({}));
    let defaults = runtime_config_defaults(state.config.as_ref());

    let mut current = defaults.as_object().cloned().unwrap_or_default();
    if let Some(saved_obj) = saved.as_object() {
        for (k, v) in saved_obj {
            current.insert(k.clone(), v.clone());
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "current": current,
            "defaults": defaults,
        })),
    )
}

/// PUT /api/settings/runtime-config
pub async fn put_runtime_config(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    if let Some(obj) = body.as_object() {
        write_runtime_config(&conn, obj);
    } else {
        let value_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('runtime-config', ?1)",
            [&value_str],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    (StatusCode::OK, Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::engine::PolicyEngine;
    use crate::server::routes::AppState;
    use std::path::PathBuf;

    fn test_db() -> db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::schema::migrate(&conn).unwrap();
        db::wrap_conn(conn)
    }

    fn test_engine(db: &db::Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    #[tokio::test]
    async fn get_config_entries_includes_merge_automation_and_omits_retired_keys() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, Json(body)) = get_config_entries(State(state)).await;
        assert_eq!(status, StatusCode::OK);

        let entries = body["entries"].as_array().expect("entries array");
        let keys: std::collections::HashSet<&str> = entries
            .iter()
            .filter_map(|entry| entry["key"].as_str())
            .collect();

        assert!(keys.contains("context_compact_percent_codex"));
        assert!(keys.contains("context_compact_percent_claude"));
        assert!(keys.contains("narrate_progress"));
        assert!(keys.contains("merge_automation_enabled"));
        assert!(keys.contains("merge_strategy"));
        assert!(keys.contains("merge_allowed_authors"));
        assert!(!keys.contains("max_chain_depth"));
        assert!(!keys.contains("context_clear_percent"));
        assert!(!keys.contains("context_clear_idle_minutes"));
    }

    #[tokio::test]
    async fn patch_config_entries_accepts_merge_automation_and_provider_specific_keys() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (patch_status, Json(patch_body)) = patch_config_entries(
            State(state.clone()),
            Json(json!({
                "merge_automation_enabled": true,
                "merge_strategy": "rebase",
                "merge_allowed_authors": "itismyfield,octocat",
                "context_compact_percent_codex": "85",
                "context_compact_percent_claude": "75",
                "narrate_progress": false,
            })),
        )
        .await;
        assert_eq!(patch_status, StatusCode::OK);
        assert_eq!(patch_body["updated"], json!(6));
        assert_eq!(patch_body["rejected"], json!([]));

        let (get_status, Json(get_body)) = get_config_entries(State(state)).await;
        assert_eq!(get_status, StatusCode::OK);

        let entries = get_body["entries"].as_array().expect("entries array");
        let values: std::collections::HashMap<&str, Option<&str>> = entries
            .iter()
            .filter_map(|entry| Some((entry["key"].as_str()?, entry["value"].as_str())))
            .collect();

        assert_eq!(
            values.get("context_compact_percent_codex"),
            Some(&Some("85"))
        );
        assert_eq!(
            values.get("context_compact_percent_claude"),
            Some(&Some("75"))
        );
        assert_eq!(values.get("narrate_progress"), Some(&Some("false")));
        assert_eq!(values.get("merge_automation_enabled"), Some(&Some("true")));
        assert_eq!(values.get("merge_strategy"), Some(&Some("rebase")));
        assert_eq!(
            values.get("merge_allowed_authors"),
            Some(&Some("itismyfield,octocat"))
        );
    }

    #[test]
    fn seed_config_defaults_inserts_narrate_progress_true() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::schema::migrate(&conn).unwrap();

        seed_config_defaults(&conn, &crate::config::Config::default());

        let value: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'narrate_progress'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "true");
    }

    #[test]
    fn seed_config_defaults_removes_retired_config_keys() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::schema::migrate(&conn).unwrap();

        conn.execute(
            "INSERT INTO kv_meta (key, value) VALUES ('max_chain_depth', '5')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kv_meta (key, value) VALUES ('context_clear_percent', '85')",
            [],
        )
        .unwrap();

        seed_config_defaults(&conn, &crate::config::Config::default());

        let retired_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key IN ('max_chain_depth', 'context_clear_percent')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired_count, 0);
    }

    #[test]
    fn seed_config_defaults_prefers_yaml_values_and_preserves_other_runtime_overrides() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::schema::migrate(&conn).unwrap();

        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('merge_strategy', 'merge')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('requested_timeout_min', '15')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('runtime-config', '{\"dispatchPollSec\":10,\"maxRetries\":7}')",
            [],
        )
        .unwrap();

        let mut config = crate::config::Config::default();
        config.automation.strategy = Some("rebase".to_string());
        config.runtime.requested_timeout_min = Some(55);
        config.runtime.dispatch_poll_sec = Some(45);

        seed_config_defaults(&conn, &config);

        let merge_strategy: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'merge_strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(merge_strategy, "rebase");

        let timeout_min: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'requested_timeout_min'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(timeout_min, "55");

        let runtime_config: Value = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(|| json!({}));
        assert_eq!(runtime_config["dispatchPollSec"], json!(45));
        assert_eq!(runtime_config["maxRetries"], json!(7));
    }

    #[test]
    fn seed_config_defaults_can_reset_runtime_overrides_on_restart() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::schema::migrate(&conn).unwrap();

        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('merge_allowed_authors', 'legacy-user')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('runtime-config', '{\"dispatchPollSec\":10,\"maxRetries\":7}')",
            [],
        )
        .unwrap();

        let mut config = crate::config::Config::default();
        config.runtime.reset_overrides_on_restart = true;
        config.automation.enabled = Some(true);

        seed_config_defaults(&conn, &config);

        let merge_allowed_authors_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key = 'merge_allowed_authors'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(merge_allowed_authors_count, 0);

        let merge_automation_enabled: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'merge_automation_enabled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(merge_automation_enabled, "true");

        let runtime_config: Value = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'runtime-config'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(|| json!({}));
        assert_eq!(runtime_config["dispatchPollSec"], json!(30));
        assert_eq!(runtime_config["maxRetries"], json!(3));
    }

    #[tokio::test]
    async fn get_runtime_config_uses_yaml_baseline_from_app_state() {
        let db = test_db();
        let mut config = crate::config::Config::default();
        config.runtime.dispatch_poll_sec = Some(45);
        config.runtime.max_retries = Some(5);
        let state = AppState::test_state_with_config(db.clone(), test_engine(&db), config);

        let (status, Json(body)) = get_runtime_config(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["current"]["dispatchPollSec"], json!(45));
        assert_eq!(body["defaults"]["dispatchPollSec"], json!(45));
        assert_eq!(body["current"]["maxRetries"], json!(5));
        assert_eq!(body["defaults"]["maxRetries"], json!(5));
    }

    #[tokio::test]
    async fn put_runtime_config_mirrors_scalar_keys_for_runtime_consumers() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, _) = put_runtime_config(
            State(state),
            Json(json!({
                "dispatchPollSec": 15,
                "maxRetries": 7,
                "rateLimitStaleSec": 900
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let conn = db.lock().unwrap();
        let stale_sec: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'rateLimitStaleSec'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_sec, "900");
    }

    #[tokio::test]
    async fn put_settings_is_full_replace_and_strips_retired_company_keys() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (first_status, _) = put_settings(
            State(state.clone()),
            Json(json!({
                "companyName": "AgentDesk",
                "roomThemes": {"dev": {"accent": "#fff"}},
                "autoUpdateEnabled": true,
            })),
        )
        .await;
        assert_eq!(first_status, StatusCode::OK);

        let (_, Json(first_body)) = get_settings(State(state.clone())).await;
        assert_eq!(first_body["companyName"], json!("AgentDesk"));
        assert!(first_body.get("autoUpdateEnabled").is_none());
        assert_eq!(first_body["roomThemes"]["dev"]["accent"], json!("#fff"));

        let (second_status, _) = put_settings(
            State(state.clone()),
            Json(json!({
                "theme": "light",
            })),
        )
        .await;
        assert_eq!(second_status, StatusCode::OK);

        let (_, Json(second_body)) = get_settings(State(state)).await;
        assert_eq!(second_body, json!({"theme": "light"}));
    }
}
