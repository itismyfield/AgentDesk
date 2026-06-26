use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct CapabilityRouteDecision {
    pub(crate) instance_id: Option<String>,
    pub(crate) eligible: bool,
    pub(crate) reasons: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct CapabilityRouteCandidate {
    pub(crate) decision: CapabilityRouteDecision,
    pub(crate) score: i64,
    pub(crate) last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub(crate) fn explain_capability_match(
    node: &Value,
    required_capabilities: &Value,
) -> CapabilityRouteDecision {
    let instance_id = node
        .get("instance_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let mut reasons = Vec::new();

    let hard_required = hard_required_capabilities(required_capabilities);
    if !hard_required.is_object() {
        return CapabilityRouteDecision {
            instance_id,
            eligible: true,
            reasons,
        };
    }

    let labels = node
        .get("labels")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let labels = labels
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(required_labels) = hard_required
        .get("labels")
        .and_then(|value| value.as_array())
    {
        for label in required_labels.iter().filter_map(|value| value.as_str()) {
            if !labels.contains(label) {
                reasons.push(format!("missing label '{label}'"));
            }
        }
    }

    let capabilities = node.get("capabilities").unwrap_or(&Value::Null);
    let providers = capabilities
        .get("providers")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let providers = providers
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(required_providers) = hard_required
        .get("providers")
        .and_then(|value| value.as_array())
    {
        for provider in required_providers.iter().filter_map(|value| value.as_str()) {
            if !providers.contains(provider) {
                reasons.push(format!("missing provider '{provider}'"));
            }
        }
    }

    if let Some(required_mcp) = hard_required.get("mcp") {
        match required_mcp {
            Value::Array(names) => {
                for endpoint in names.iter().filter_map(|value| value.as_str()) {
                    if capabilities
                        .get("mcp")
                        .and_then(|mcp| mcp.get(endpoint))
                        .is_none()
                    {
                        reasons.push(format!("missing MCP endpoint '{endpoint}'"));
                    }
                }
            }
            Value::Object(map) => {
                for (endpoint, requirement) in map {
                    let actual = capabilities.get("mcp").and_then(|mcp| mcp.get(endpoint));
                    let Some(actual) = actual else {
                        reasons.push(format!("missing MCP endpoint '{endpoint}'"));
                        continue;
                    };
                    let requires_healthy = requirement
                        .get("healthy")
                        .and_then(|value| value.as_bool())
                        .or_else(|| requirement.as_bool())
                        .unwrap_or(false);
                    let actual_healthy = actual
                        .get("healthy")
                        .and_then(|value| value.as_bool())
                        .or_else(|| actual.as_bool())
                        .unwrap_or(false);
                    if requires_healthy && !actual_healthy {
                        reasons.push(format!("MCP endpoint '{endpoint}' is not healthy"));
                    }
                }
            }
            _ => {}
        }
    }

    CapabilityRouteDecision {
        instance_id,
        eligible: reasons.is_empty(),
        reasons,
    }
}

pub(crate) fn select_capability_route(
    nodes: &[Value],
    required_capabilities: &Value,
) -> Vec<CapabilityRouteCandidate> {
    let preferred = preferred_capabilities(required_capabilities);
    let mut candidates = nodes
        .iter()
        .filter(|node| node.get("status").and_then(|value| value.as_str()) == Some("online"))
        .map(|node| {
            let decision = explain_capability_match(node, required_capabilities);
            let score = if decision.eligible {
                capability_preference_score(node, preferred)
            } else {
                0
            };
            CapabilityRouteCandidate {
                decision,
                score,
                last_heartbeat_at: parse_last_heartbeat(node),
            }
        })
        .filter(|candidate| candidate.decision.eligible)
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.last_heartbeat_at.cmp(&left.last_heartbeat_at))
            .then_with(|| left.decision.instance_id.cmp(&right.decision.instance_id))
    });
    candidates
}

fn hard_required_capabilities(capabilities: &Value) -> &Value {
    capabilities
        .get("required")
        .filter(|value| value.is_object())
        .unwrap_or(capabilities)
}

fn preferred_capabilities(capabilities: &Value) -> Option<&Value> {
    capabilities
        .get("preferred")
        .filter(|value| value.is_object())
}

fn capability_preference_score(node: &Value, preferred: Option<&Value>) -> i64 {
    let Some(preferred) = preferred else {
        return 0;
    };
    let mut score = 0;

    let labels = string_set(node.get("labels"));
    if let Some(preferred_labels) = preferred.get("labels").and_then(|value| value.as_array()) {
        let preferred_count = preferred_labels.len() as i64;
        score += preferred_labels
            .iter()
            .enumerate()
            .filter_map(|(index, value)| value.as_str().map(|label| (index, label)))
            .filter(|(_, label)| labels.contains(label))
            .map(|(index, _)| preferred_count.saturating_sub(index as i64))
            .sum::<i64>();
    }

    let capabilities = node.get("capabilities").unwrap_or(&Value::Null);
    let providers = string_set(capabilities.get("providers"));
    if let Some(preferred_providers) = preferred
        .get("providers")
        .and_then(|value| value.as_array())
    {
        score += preferred_providers
            .iter()
            .filter_map(|value| value.as_str())
            .filter(|provider| providers.contains(provider))
            .count() as i64;
    }

    if let Some(preferred_mcp) = preferred.get("mcp") {
        match preferred_mcp {
            Value::Array(names) => {
                score += names
                    .iter()
                    .filter_map(|value| value.as_str())
                    .filter(|endpoint| {
                        capabilities
                            .get("mcp")
                            .and_then(|mcp| mcp.get(*endpoint))
                            .is_some()
                    })
                    .count() as i64;
            }
            Value::Object(map) => {
                for (endpoint, preference) in map {
                    let actual = capabilities.get("mcp").and_then(|mcp| mcp.get(endpoint));
                    let Some(actual) = actual else {
                        continue;
                    };
                    let prefers_healthy = preference
                        .get("healthy")
                        .and_then(|value| value.as_bool())
                        .or_else(|| preference.as_bool())
                        .unwrap_or(false);
                    let actual_healthy = actual
                        .get("healthy")
                        .and_then(|value| value.as_bool())
                        .or_else(|| actual.as_bool())
                        .unwrap_or(false);
                    if !prefers_healthy || actual_healthy {
                        score += 1;
                    }
                }
            }
            _ => {}
        }
    }

    score
}

fn string_set(value: Option<&Value>) -> std::collections::BTreeSet<&str> {
    value
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .collect()
}

fn parse_last_heartbeat(node: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    node.get("last_heartbeat_at")
        .and_then(|value| value.as_str())
        .and_then(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|value| value.with_timezone(&chrono::Utc))
        })
}
