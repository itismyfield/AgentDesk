//! One precedence for the key table: a persisted kv value (a `runtime-config` field or
//! an individual `kv_meta` row), then the YAML snapshot the caller holds, then the default.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use crate::config::{Config, RuntimeSettingsConfig};
use crate::services::dispatch_gate::{DEFAULT_DANGER_PCT, DEFAULT_STALE_SEC};

/// The `kv_meta` row holding the persisted `runtime-config` blob.
const RUNTIME_CONFIG_ROW: &str = "runtime-config";

/// The layer that supplied a resolved setting value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    KvOverride,
    Yaml,
    Default,
}

/// A setting value and the layer it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved<T> {
    pub value: T,
    pub source: SettingSource,
}

/// Which persisted values outrank YAML.
#[derive(Clone, Copy, Debug)]
enum KvAuthority {
    AnyValue,
    /// A seeded default that was never set explicitly defers to the live YAML snapshot.
    ExplicitOrNonDefault,
}

/// A runtime-config key: its field in the persisted blob, its YAML field and its default.
pub struct RuntimeSetting<T: 'static> {
    kv_key: &'static str,
    default: T,
    authority: KvAuthority,
    parse_kv: fn(&Value) -> Option<T>,
    yaml: fn(&RuntimeSettingsConfig) -> Option<T>,
}

pub const DISPATCH_GATE_ENABLED: RuntimeSetting<bool> = RuntimeSetting {
    kv_key: "dispatchRateLimitGateEnabled",
    default: true,
    authority: KvAuthority::ExplicitOrNonDefault,
    parse_kv: Value::as_bool,
    yaml: |runtime| runtime.dispatch_rate_limit_gate_enabled,
};

pub const DISPATCH_GATE_DANGER_PCT: RuntimeSetting<u64> = RuntimeSetting {
    kv_key: "dispatchRateLimitGateDangerPct",
    default: DEFAULT_DANGER_PCT,
    authority: KvAuthority::ExplicitOrNonDefault,
    parse_kv: Value::as_u64,
    yaml: |runtime| runtime.dispatch_rate_limit_gate_danger_pct.map(u64::from),
};

pub const RATE_LIMIT_STALE_SEC: RuntimeSetting<i64> = RuntimeSetting {
    kv_key: "rateLimitStaleSec",
    default: DEFAULT_STALE_SEC,
    authority: KvAuthority::ExplicitOrNonDefault,
    parse_kv: Value::as_i64,
    yaml: |runtime| runtime.rate_limit_stale_sec.map(|value| value as i64),
};

pub const MAX_ENTRY_RETRIES: RuntimeSetting<u64> = RuntimeSetting {
    kv_key: "maxEntryRetries",
    default: 3,
    authority: KvAuthority::AnyValue,
    parse_kv: Value::as_u64,
    yaml: |runtime| runtime.max_entry_retries,
};

/// An individual `kv_meta` row key: values below `floor` are raised to it.
pub struct RowSetting {
    kv_key: &'static str,
    default: u64,
    floor: u64,
    /// The key whose resolved value stands in when this key has no kv or YAML value.
    inherits: Option<&'static RowSetting>,
    yaml: fn(&RuntimeSettingsConfig) -> Option<u64>,
}

pub const REQUESTED_TIMEOUT_MIN: RowSetting = RowSetting {
    kv_key: "requested_timeout_min",
    default: 45,
    floor: 1,
    inherits: None,
    yaml: |runtime| runtime.requested_timeout_min,
};

pub const IN_PROGRESS_STALE_MIN: RowSetting = RowSetting {
    kv_key: "in_progress_stale_min",
    default: 120,
    floor: 1,
    inherits: None,
    yaml: |runtime| runtime.in_progress_stale_min,
};

pub const CONTEXT_COMPACT_PERCENT: RowSetting = RowSetting {
    kv_key: "context_compact_percent",
    default: 60,
    floor: 0,
    inherits: None,
    yaml: |runtime| runtime.context_compact_percent,
};

pub const CONTEXT_COMPACT_PERCENT_CODEX: RowSetting = RowSetting {
    kv_key: "context_compact_percent_codex",
    default: 100,
    floor: 0,
    inherits: None,
    yaml: |runtime| runtime.context_compact_percent_codex,
};

pub const CONTEXT_COMPACT_PERCENT_CLAUDE: RowSetting = RowSetting {
    kv_key: "context_compact_percent_claude",
    default: 60,
    floor: 0,
    inherits: Some(&CONTEXT_COMPACT_PERCENT),
    yaml: |runtime| runtime.context_compact_percent_claude,
};

pub const CONTEXT_COMPACT_LOWER_BOUND_TOKENS: RowSetting = RowSetting {
    kv_key: "context_compact_lower_bound_tokens",
    default: crate::services::claude_compact_context::DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS,
    floor: 0,
    inherits: None,
    yaml: |runtime| runtime.context_compact_lower_bound_tokens,
};

pub const ROW_SETTINGS: [&RowSetting; 6] = [
    &REQUESTED_TIMEOUT_MIN,
    &IN_PROGRESS_STALE_MIN,
    &CONTEXT_COMPACT_PERCENT,
    &CONTEXT_COMPACT_PERCENT_CODEX,
    &CONTEXT_COMPACT_PERCENT_CLAUDE,
    &CONTEXT_COMPACT_LOWER_BOUND_TOKENS,
];

impl RowSetting {
    pub fn kv_key(&self) -> &'static str {
        self.kv_key
    }

    /// The `kv_meta` rows [`resolve_row`] reads for this key.
    pub fn row_keys(&self) -> Vec<&'static str> {
        std::iter::successors(Some(self), |key| key.inherits)
            .map(|key| key.kv_key)
            .collect()
    }
}

impl<T: Copy + PartialEq> RuntimeSetting<T> {
    /// The persisted override in a parsed `runtime-config` blob; a wrongly typed value is no override.
    pub fn kv_override(&self, runtime_config: Option<&Value>) -> Option<T> {
        let blob = runtime_config?;
        let value = runtime_config_field(blob, self.kv_key).and_then(self.parse_kv)?;
        let authoritative = match self.authority {
            KvAuthority::AnyValue => true,
            KvAuthority::ExplicitOrNonDefault => {
                value != self.default
                    || blob.as_object().is_some_and(|object| {
                        super::explicit_runtime_config_keys(object).contains(self.kv_key)
                    })
            }
        };
        authoritative.then_some(value)
    }
}

/// A field of a parsed `runtime-config` blob; a non-object blob has none.
pub fn runtime_config_field<'a>(blob: &'a Value, key: &str) -> Option<&'a Value> {
    blob.as_object()?.get(key)
}

/// A retry limit as a positive `i64`: zero becomes one and larger values saturate.
pub fn clamp_retry_limit(value: u64) -> i64 {
    value.max(1).min(i64::MAX as u64) as i64
}

/// The auto-queue entry retry limit: [`MAX_ENTRY_RETRIES`] resolved, then clamped.
pub fn max_entry_retries(runtime_config: Option<&Value>, yaml: Option<&Config>) -> i64 {
    clamp_retry_limit(resolve(&MAX_ENTRY_RETRIES, runtime_config, yaml).value)
}

/// Resolve `key` from a parsed `runtime-config` blob and one YAML snapshot.
pub fn resolve<T: Copy + PartialEq>(
    key: &RuntimeSetting<T>,
    runtime_config: Option<&Value>,
    yaml: Option<&Config>,
) -> Resolved<T> {
    resolve_with_kv(key, key.kv_override(runtime_config), yaml)
}

/// Resolve `key` when the kv layer was already read with [`RuntimeSetting::kv_override`].
pub fn resolve_with_kv<T: Copy>(
    key: &RuntimeSetting<T>,
    kv: Option<T>,
    yaml: Option<&Config>,
) -> Resolved<T> {
    layered(
        kv,
        yaml.and_then(|config| (key.yaml)(&config.runtime)),
        key.default,
    )
}

/// Resolve a row key from the `kv_meta` rows read for it; an unparsable row is no override.
pub fn resolve_row(
    key: &RowSetting,
    rows: &HashMap<String, String>,
    yaml: Option<&Config>,
) -> Resolved<u64> {
    let kv = rows
        .get(key.kv_key)
        .and_then(|value| value.trim().parse::<u64>().ok());
    let yaml_value = yaml.and_then(|config| (key.yaml)(&config.runtime));
    let resolved = match key.inherits {
        Some(parent) if kv.is_none() && yaml_value.is_none() => resolve_row(parent, rows, yaml),
        _ => layered(kv, yaml_value, key.default),
    };
    Resolved {
        value: resolved.value.max(key.floor),
        source: resolved.source,
    }
}

/// The kv rows a policy `config.get(key)` resolves from; `None` outside the key table.
pub fn policy_kv_rows(key: &str) -> Option<Vec<&'static str>> {
    if let Some(row) = ROW_SETTINGS.iter().find(|row| row.kv_key == key) {
        return Some(row.row_keys());
    }
    policy_runtime_config_value(key, None, None).map(|_| vec![RUNTIME_CONFIG_ROW])
}

/// The resolved value policy `config.get(key)` returns; `None` outside the key table.
pub fn policy_value(
    key: &str,
    rows: &HashMap<String, String>,
    yaml: Option<&Config>,
) -> Option<Value> {
    if let Some(row) = ROW_SETTINGS.iter().find(|row| row.kv_key == key) {
        return Some(resolve_row(row, rows, yaml).value.into());
    }
    let runtime_config = rows
        .get(RUNTIME_CONFIG_ROW)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    policy_runtime_config_value(key, runtime_config.as_ref(), yaml)
}

fn policy_runtime_config_value(
    key: &str,
    runtime_config: Option<&Value>,
    yaml: Option<&Config>,
) -> Option<Value> {
    Some(match key {
        _ if key == MAX_ENTRY_RETRIES.kv_key => max_entry_retries(runtime_config, yaml).into(),
        _ if key == DISPATCH_GATE_ENABLED.kv_key => {
            resolve(&DISPATCH_GATE_ENABLED, runtime_config, yaml)
                .value
                .into()
        }
        _ if key == DISPATCH_GATE_DANGER_PCT.kv_key => {
            resolve(&DISPATCH_GATE_DANGER_PCT, runtime_config, yaml)
                .value
                .into()
        }
        _ if key == RATE_LIMIT_STALE_SEC.kv_key => {
            resolve(&RATE_LIMIT_STALE_SEC, runtime_config, yaml)
                .value
                .into()
        }
        _ => return None,
    })
}

/// The one precedence every key-table key follows.
fn layered<T>(kv: Option<T>, yaml: Option<T>, default: T) -> Resolved<T> {
    if let Some(value) = kv {
        return Resolved {
            value,
            source: SettingSource::KvOverride,
        };
    }
    if let Some(value) = yaml {
        return Resolved {
            value,
            source: SettingSource::Yaml,
        };
    }
    Resolved {
        value: default,
        source: SettingSource::Default,
    }
}

#[cfg(test)]
mod tests;
