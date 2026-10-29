//! One precedence for runtime-config keys: a persisted `kv_meta['runtime-config']`
//! override, then the YAML snapshot the caller holds, then the compiled default.

use serde::Serialize;
use serde_json::Value;

use crate::config::{Config, RuntimeSettingsConfig};
use crate::services::dispatch_gate::{DEFAULT_DANGER_PCT, DEFAULT_STALE_SEC};

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
    if let Some(value) = kv {
        return Resolved {
            value,
            source: SettingSource::KvOverride,
        };
    }
    if let Some(value) = yaml.and_then(|config| (key.yaml)(&config.runtime)) {
        return Resolved {
            value,
            source: SettingSource::Yaml,
        };
    }
    Resolved {
        value: key.default,
        source: SettingSource::Default,
    }
}

#[cfg(test)]
mod tests;
