use serde_json::{Map, Value};

use super::{
    RUNTIME_CONFIG_EXPLICIT_KEYS_META, explicit_runtime_config_keys, is_runtime_config_key,
};

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
