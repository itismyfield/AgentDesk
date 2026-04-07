use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;

use super::AppState;

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

/// PUT /api/settings
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

    let value_str = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());

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

/// Known config keys with metadata for the settings UI.
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
        "max_chain_depth",
        "dispatch",
        "최대 체인 깊이",
        "Max Chain Depth",
        Some("5"),
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
        "context_clear_percent",
        "context",
        "컨텍스트 clear 임계값 (%)",
        "Context Clear Threshold (%)",
        Some("40"),
    ),
    (
        "context_clear_idle_minutes",
        "context",
        "컨텍스트 clear 유휴 시간 (분)",
        "Context Clear Idle Time (min)",
        Some("60"),
    ),
];

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
            "default": default_val,
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

/// Default runtime config values.
/// Only polling intervals and Rust-only settings live here.
/// Kanban/timeout/review/context settings are in CONFIG_KEYS (individual kv_meta keys).
fn runtime_config_defaults() -> serde_json::Value {
    json!({
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
}

/// Seed default values for CONFIG_KEYS into kv_meta on startup.
/// Only inserts if the key doesn't already exist (INSERT OR IGNORE).
pub fn seed_config_defaults(conn: &rusqlite::Connection) {
    for (key, _, _, _, default_val) in CONFIG_KEYS {
        if let Some(val) = default_val {
            conn.execute(
                "INSERT OR IGNORE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, val],
            )
            .ok();
        }
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
    let defaults = runtime_config_defaults();

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
    async fn get_config_entries_includes_provider_specific_compact_percent_keys() {
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
    }

    #[tokio::test]
    async fn patch_config_entries_accepts_provider_specific_compact_percent_keys() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (patch_status, Json(patch_body)) = patch_config_entries(
            State(state.clone()),
            Json(json!({
                "context_compact_percent_codex": "85",
                "context_compact_percent_claude": "75",
            })),
        )
        .await;
        assert_eq!(patch_status, StatusCode::OK);
        assert_eq!(patch_body["updated"], json!(2));
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
    }
}
