use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::HealthStatus;
use crate::services::discord::turn_presence::supervisor::{UnknownSnapshot, unknown_snapshots};

pub(crate) fn attach_turn_presence_unknown(
    status: &mut HealthStatus,
    reasons: &mut Vec<Value>,
    body: &mut Value,
    detailed: bool,
) {
    project(unknown_snapshots(), status, reasons, body, detailed);
}

fn project(
    snapshots: Vec<UnknownSnapshot>,
    status: &mut HealthStatus,
    reasons: &mut Vec<Value>,
    body: &mut Value,
    detailed: bool,
) {
    let mut groups: BTreeMap<String, BTreeMap<u64, UnknownSnapshot>> = BTreeMap::new();
    for snapshot in snapshots {
        let key = format!(
            "turn_presence_unknown:{}:{}:{}:{}",
            snapshot.provider,
            agent_component(snapshot.agent_id.as_deref()),
            snapshot.reason,
            snapshot.hold_leaf.as_deref().unwrap_or("none")
        );
        let channels = groups.entry(key).or_default();
        if channels
            .get(&snapshot.channel_id)
            .is_none_or(|previous| snapshot.elapsed_secs > previous.elapsed_secs)
        {
            channels.insert(snapshot.channel_id, snapshot);
        }
    }
    if !groups.is_empty() {
        *status = status.worsen(HealthStatus::Degraded);
    }
    let mut details = Vec::new();
    for (key, channels) in groups {
        let oldest = channels
            .values()
            .map(|snapshot| snapshot.elapsed_secs)
            .max()
            .unwrap_or_default();
        reasons.push(json!(format!("{key}:{}:{oldest}", channels.len())));
        if detailed {
            details.extend(channels.into_values().map(detail));
        }
    }
    if detailed {
        body["turn_presence_unknown"] = Value::Array(details);
    }
}

fn agent_component(agent: Option<&str>) -> String {
    let Some(agent) = agent else {
        return "~".to_string();
    };
    let mut encoded = String::new();
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in agent.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    encoded
}

fn detail(snapshot: UnknownSnapshot) -> Value {
    json!({
        "channel_id": snapshot.channel_id,
        "provider": snapshot.provider,
        "agent_id": snapshot.agent_id,
        "reason": snapshot.reason,
        "hold_leaf": snapshot.hold_leaf,
        "unknown_since": snapshot.unknown_since,
        "elapsed_secs": snapshot.elapsed_secs,
        "observed_reason": snapshot.observed_reason,
        "observation_progress": snapshot.observation_progress,
        "notice_state": snapshot.notice_state,
        "notice_store_error": snapshot.notice_store_error,
    })
}

#[cfg(test)]
#[path = "presence_unknown_tests.rs"]
mod tests;
