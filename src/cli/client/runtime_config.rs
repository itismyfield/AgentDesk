use serde_json::Value;

/// Convert either raw values or the runtime-config GET envelope into a PUT body.
/// GET metadata is copied to the reserved field so CLI GET-to-SET keeps authority exact.
pub(crate) fn payload(value: Value) -> Result<Value, String> {
    let explicit_keys = value.get("explicit_keys").cloned();
    let mut normalized = match value.get("current") {
        Some(current) if current.is_object() => current.clone(),
        Some(_) => return Err("runtime config `current` must be a JSON object".to_string()),
        None => value,
    };
    if let (Some(keys), Some(current)) = (explicit_keys, normalized.as_object_mut()) {
        current.insert(
            crate::services::settings::RUNTIME_CONFIG_EXPLICIT_KEYS_META.to_string(),
            keys,
        );
    }
    normalized
        .is_object()
        .then_some(normalized)
        .ok_or_else(|| "runtime config must be a JSON object".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_get_payload_preserves_explicit_keys_for_cli_set() {
        let normalized = payload(json!({
            "current": {"maxEntryRetries": 7, "maxRetries": 3},
            "defaults": {"maxEntryRetries": 3, "maxRetries": 3},
            "explicit_keys": ["maxEntryRetries"]
        }))
        .expect("normalize runtime-config GET response");

        assert_eq!(
            normalized,
            json!({
                "maxEntryRetries": 7,
                "maxRetries": 3,
                "__runtimeConfigExplicitKeys": ["maxEntryRetries"]
            })
        );
    }

    #[test]
    fn plain_cli_set_stays_metadata_less_for_server_authority() {
        let normalized =
            payload(json!({"maxEntryRetries": 7})).expect("normalize plain CLI config set");

        assert_eq!(normalized, json!({"maxEntryRetries": 7}));
    }
}
