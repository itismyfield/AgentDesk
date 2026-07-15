use serde_json::{Map, Value};

use super::{
    RUNTIME_CONFIG_EXPLICIT_KEYS_META, explicit_runtime_config_keys, is_runtime_config_key,
};

/// Normalize a full-replace PUT while keeping explicit-override authority server-owned.
/// Supplied metadata is exact (including empty); otherwise known body keys are explicit.
pub(super) fn with_explicit_runtime_config_keys(
    mut values: Map<String, Value>,
) -> Map<String, Value> {
    let explicit_keys = match values.get(RUNTIME_CONFIG_EXPLICIT_KEYS_META) {
        Some(_) => explicit_runtime_config_keys(&values),
        None => values
            .keys()
            .filter(|key| is_runtime_config_key(key))
            .cloned()
            .collect(),
    };
    let mut keys = explicit_keys.into_iter().collect::<Vec<_>>();
    keys.sort();
    values.insert(
        RUNTIME_CONFIG_EXPLICIT_KEYS_META.to_string(),
        Value::Array(keys.into_iter().map(Value::String).collect()),
    );
    values
}
