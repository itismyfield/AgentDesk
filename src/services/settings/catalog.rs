use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{
    RUNTIME_CONFIG_EXPLICIT_KEYS_META, config_entry_baseline_source, config_entry_default,
    config_entry_effective_value, config_entry_override_active, config_entry_restart_behavior,
    explicit_runtime_config_keys, is_read_only_config_key, runtime_config_defaults_map,
    runtime_config_put::RUNTIME_KEYS,
};

pub(super) struct ConfigKey {
    pub key: &'static str,
    category: &'static str,
    label_ko: &'static str,
    label_en: &'static str,
    description_ko: &'static str,
    pub default_value: Option<&'static str>,
}

pub(super) const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey {
        key: "deadlock_manager_channel_id",
        category: "pipeline",
        label_ko: "데드락 매니저 채널 ID",
        label_en: "Deadlock Manager Channel ID",
        description_ko: "데드락 매니저 알림을 보낼 Discord 채널입니다.",
        default_value: None,
    },
    ConfigKey {
        key: "agent_quality_monitoring_channel_id",
        category: "quality",
        label_ko: "GitHub 동기화 알림 채널 ID",
        label_en: "GitHub Sync Alert Channel ID",
        description_ko: "GitHub 동기화 중 종료된 카드와 열린 이슈의 불일치를 알릴 Discord 채널입니다.",
        default_value: None,
    },
    ConfigKey {
        key: "kanban_relay_alert_threshold",
        category: "quality",
        label_ko: "릴레이 누락 신호 경보 임계",
        label_en: "Relay Signal Alert Threshold",
        description_ko: "시간당 릴레이 누락 경보를 보내는 기준 횟수입니다. 비어 있으면 신호별 기본값을 씁니다.",
        default_value: None,
    },
    ConfigKey {
        key: "review_enabled",
        category: "review",
        label_ko: "리뷰 활성화",
        label_en: "Review Enabled",
        description_ko: "자동 리뷰를 실행할지 정합니다.",
        default_value: None,
    },
    ConfigKey {
        key: "max_review_rounds",
        category: "review",
        label_ko: "최대 리뷰 라운드",
        label_en: "Max Review Rounds",
        description_ko: "리뷰를 반복할 수 있는 최대 횟수입니다.",
        default_value: Some("3"),
    },
    ConfigKey {
        key: "server_port",
        category: "system",
        label_ko: "서버 포트",
        label_en: "Server Port",
        description_ko: "서버가 요청을 받는 포트입니다.",
        default_value: None,
    },
    ConfigKey {
        key: "requested_timeout_min",
        category: "timeout",
        label_ko: "요청됨 타임아웃 (분)",
        label_en: "Requested Timeout (min)",
        description_ko: "요청됨 상태의 작업을 타임아웃으로 판정할 때까지 기다리는 시간입니다.",
        default_value: Some("45"),
    },
    ConfigKey {
        key: "in_progress_stale_min",
        category: "timeout",
        label_ko: "진행 중 정체 판정 (분)",
        label_en: "In-Progress Stale (min)",
        description_ko: "진행 중인 작업을 정체로 판정할 때까지 기다리는 시간입니다.",
        default_value: Some("120"),
    },
    ConfigKey {
        key: "context_compact_percent",
        category: "context",
        label_ko: "컨텍스트 compact 임계값 (%)",
        label_en: "Context Compact Threshold (%)",
        description_ko: "컨텍스트 압축을 시작하는 사용률입니다. 제공자별 값이 있으면 그 값이 우선합니다.",
        default_value: Some("60"),
    },
    ConfigKey {
        key: "context_compact_percent_codex",
        category: "context",
        label_ko: "Codex 컨텍스트 compact 임계값 (%)",
        label_en: "Codex Context Compact Threshold (%)",
        description_ko: "Codex 컨텍스트 압축을 시작하는 사용률입니다.",
        default_value: None,
    },
    ConfigKey {
        key: "context_compact_percent_claude",
        category: "context",
        label_ko: "Claude 컨텍스트 compact 임계값 (%)",
        label_en: "Claude Context Compact Threshold (%)",
        description_ko: "Claude 컨텍스트 압축을 시작하는 사용률입니다.",
        default_value: None,
    },
    ConfigKey {
        key: "context_compact_lower_bound_tokens",
        category: "context",
        label_ko: "컨텍스트 compact 최소 토큰",
        label_en: "Context Compact Lower Bound (tokens)",
        description_ko: "사용률과 함께 적용하는 컨텍스트 압축의 최소 토큰 수입니다.",
        default_value: Some("300000"),
    },
];

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
    pub description_ko: &'static str,
    #[serde(rename = "default")]
    pub default_value: Option<String>,
    pub baseline: Option<String>,
    pub baseline_source: Option<String>,
    pub override_active: bool,
    pub editable: bool,
    pub restart_behavior: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeConfigResponse {
    pub current: Map<String, Value>,
    pub defaults: Map<String, Value>,
    pub explicit_keys: Vec<String>,
    pub key_metadata: BTreeMap<&'static str, RuntimeConfigKeyMetadata>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeConfigKeyMetadata {
    pub label_ko: &'static str,
}

pub(super) fn config_entries(
    config: &crate::config::Config,
    pg_values: &HashMap<String, String>,
) -> SettingsConfigEntriesResponse {
    let mut entries = Vec::new();
    for item in CONFIG_KEYS {
        let key = item.key;
        let stored_value: Option<String> = if is_read_only_config_key(key) {
            None
        } else {
            pg_values.get(key).cloned()
        };
        let baseline = config_entry_default(config, key, item.default_value);
        let effective = config_entry_effective_value(key, stored_value, baseline.clone());
        let editable = !is_read_only_config_key(key);
        entries.push(SettingsConfigEntry {
            key: key.to_string(),
            value: effective.clone(),
            category: item.category.to_string(),
            label_ko: item.label_ko.to_string(),
            label_en: item.label_en.to_string(),
            description_ko: item.description_ko,
            default_value: baseline.clone(),
            baseline: baseline.clone(),
            baseline_source: config_entry_baseline_source(config, key, item.default_value)
                .map(str::to_string),
            override_active: config_entry_override_active(
                editable,
                effective.as_deref(),
                baseline.as_deref(),
            ),
            editable,
            restart_behavior: config_entry_restart_behavior(config, key, item.default_value)
                .to_string(),
        });
    }

    SettingsConfigEntriesResponse { entries }
}

pub(super) fn runtime_config(
    config: &crate::config::Config,
    saved_raw: Option<&str>,
) -> RuntimeConfigResponse {
    let saved = saved_raw
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_else(|| json!({}));
    let defaults = runtime_config_defaults_map(config);

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

    RuntimeConfigResponse {
        current,
        defaults,
        explicit_keys,
        key_metadata: RUNTIME_KEYS
            .iter()
            .map(|&(key, label_ko)| (key, RuntimeConfigKeyMetadata { label_ko }))
            .collect(),
    }
}
