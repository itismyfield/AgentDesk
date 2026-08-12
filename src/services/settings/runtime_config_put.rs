use serde_json::{Map, Value};

use super::{
    RUNTIME_CONFIG_EXPLICIT_KEYS_META, explicit_runtime_config_keys, is_runtime_config_key,
};
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

pub(super) fn validate_runtime_config_values(values: &Map<String, Value>) -> ServiceResult<()> {
    let compact_window = values
        .get("claudeAutoCompactWindowTokens")
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                ServiceError::bad_request("claudeAutoCompactWindowTokens must be an integer")
                    .with_code(ErrorCode::Settings)
                    .with_operation("put_runtime_config.validate")
            })
        })
        .transpose()?;
    crate::config::validate_claude_auto_compact_window_tokens(compact_window).map_err(|error| {
        ServiceError::bad_request(error.to_string())
            .with_code(ErrorCode::Settings)
            .with_operation("put_runtime_config.validate")
    })?;

    if let Some(value) = values.get("paneEnvInjections") {
        let entries = value.as_array().ok_or_else(|| {
            ServiceError::bad_request("paneEnvInjections must be an array of strings")
                .with_code(ErrorCode::Settings)
                .with_operation("put_runtime_config.validate")
        })?;
        let entries = entries
            .iter()
            .map(|entry| {
                entry.as_str().map(str::to_string).ok_or_else(|| {
                    ServiceError::bad_request("paneEnvInjections must be an array of strings")
                        .with_code(ErrorCode::Settings)
                        .with_operation("put_runtime_config.validate")
                })
            })
            .collect::<ServiceResult<Vec<_>>>()?;
        crate::config::validate_pane_env_injections(&entries).map_err(|error| {
            ServiceError::bad_request(error.to_string())
                .with_code(ErrorCode::Settings)
                .with_operation("put_runtime_config.validate")
        })?;
    }
    Ok(())
}

/// Normalize a PUT while keeping explicit-override authority server-owned.
/// Supplied metadata is exact (including empty); metadata-less updates retain omitted
/// explicit overrides while promoting known submitted keys to explicit overrides.
pub(super) fn with_explicit_runtime_config_keys(
    mut values: Map<String, Value>,
    saved_values: Option<&Map<String, Value>>,
) -> Map<String, Value> {
    let explicit_keys = match values.get(RUNTIME_CONFIG_EXPLICIT_KEYS_META) {
        Some(_) => explicit_runtime_config_keys(&values),
        None => {
            let mut explicit_keys = saved_values
                .map(explicit_runtime_config_keys)
                .unwrap_or_default();
            for key in explicit_keys.iter() {
                if values.contains_key(key) {
                    continue;
                }
                if let Some(value) = saved_values.and_then(|saved| saved.get(key)) {
                    values.insert(key.clone(), value.clone());
                }
            }
            explicit_keys.extend(
                values
                    .keys()
                    .filter(|key| is_runtime_config_key(key))
                    .cloned(),
            );
            explicit_keys
        }
    };
    let mut keys = explicit_keys.into_iter().collect::<Vec<_>>();
    keys.sort();
    values.insert(
        RUNTIME_CONFIG_EXPLICIT_KEYS_META.to_string(),
        Value::Array(keys.into_iter().map(Value::String).collect()),
    );
    values
}
