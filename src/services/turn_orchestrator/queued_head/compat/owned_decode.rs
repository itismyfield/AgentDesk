use super::super::v2_types::{SnapshotV2, marker_control::MarkerStage};
use serde_json::Value;

pub(crate) fn decode(value: Value) -> Result<SnapshotV2, String> {
    let version = value["version"].as_u64();
    if version == Some(2) {
        if value.get("a_control").is_some() {
            return Err("marker control debt requires queue version 3".into());
        }
    } else if version == Some(3) {
        const FIELDS: &[&str] = &[
            "version",
            "items",
            "head_injection",
            "retained_inputs",
            "terminal_identities",
            "namespace",
            "admission_receipts",
            "admission_sequence",
            "admission_binds",
            "owed_drop_receipts",
            "a_control",
        ];
        let object = value.as_object().ok_or("queue V3 must be an object")?;
        if let Some(field) = object
            .keys()
            .find(|field| !FIELDS.contains(&field.as_str()))
        {
            return Err(format!("unknown queue V3 field: {field}"));
        }
    } else {
        return Err("unsupported owned queue version".into());
    }
    let raw_controls = value.get("a_control").cloned();
    let snapshot: SnapshotV2 = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if snapshot.version == 3 {
        let known_controls =
            serde_json::to_value(&snapshot.a_control).map_err(|e| e.to_string())?;
        if raw_controls
            .as_ref()
            .is_some_and(|raw| unknown_keys(raw, &known_controls))
        {
            return Err("unknown queue V3 marker control field".into());
        }
        if snapshot
            .namespace
            .as_ref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err("queue V3 requires a namespace".into());
        }
        for control in &snapshot.a_control {
            if control.cleanup_id.trim().is_empty()
                || control.epoch.trim().is_empty()
                || control.identity.sources.is_empty()
                || control.paths.is_empty()
                || control.paths.iter().any(|path| !path.path.is_absolute())
                || matches!(&control.stage, MarkerStage::Outbox { event_id } if event_id.trim().is_empty())
            {
                return Err("incomplete queue V3 marker control record".into());
            }
        }
    }
    Ok(snapshot)
}

fn unknown_keys(raw: &Value, known: &Value) -> bool {
    match (raw, known) {
        (Value::Object(raw), Value::Object(known)) => raw.iter().any(|(key, value)| {
            known
                .get(key)
                .is_none_or(|field| unknown_keys(value, field))
        }),
        (Value::Array(raw), Value::Array(known)) => raw
            .iter()
            .zip(known)
            .any(|(raw, known)| unknown_keys(raw, known)),
        _ => false,
    }
}
