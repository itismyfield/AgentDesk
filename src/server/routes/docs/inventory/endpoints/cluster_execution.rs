use super::super::{EndpointDoc, body_param, ep, path_param};
use serde_json::json;

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep("GET", "/api/agents/{id}/execution-node", "agents",
            "Read the default node for this agent's new Discord sessions. Existing session ownership and explicit channel /node selection take precedence.")
            .with_params([("id", path_param("Agent ID."))])
            .with_example(json!({"path":{"id":"codex"}}), json!({"default_node_id":"windows-worker-1","routing_enforced":true})),
        ep("PUT", "/api/agents/{id}/execution-node", "agents",
            "Set or clear a registered default node. Requires enforced intake routing for a non-null selection. Does not move existing sessions or modify hard execution requirements. Full runtime only.")
            .with_params([("id", path_param("Agent ID.")), ("default_node_id", body_param("string|null", true, "Registered instance ID; null restores the existing placement policy."))])
            .with_example(json!({"path":{"id":"codex"},"body":{"default_node_id":"windows-worker-1"}}), json!({"default_node_id":"windows-worker-1"})),
    ]
}
