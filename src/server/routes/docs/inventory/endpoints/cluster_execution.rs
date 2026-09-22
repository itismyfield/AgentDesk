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
        ep("GET", "/api/agents/{id}/execution-requirements", "agents",
            "Read central hard execution requirements. Unlike the default node, these constraints also apply to existing session owners.")
            .with_params([("id", path_param("Agent ID."))])
            .with_example(json!({"path":{"id":"codex"}}), json!({"execution_requirements":{"os":["windows"]}})),
        ep("PUT", "/api/agents/{id}/execution-requirements", "agents",
            "Replace hard requirements with os, arch, nodes, tools, repositories and backends lists; an empty object clears requirements. Unknown fields and invalid identifiers are rejected. Full runtime only.")
            .with_params([("id", path_param("Agent ID."))])
            .with_example(json!({"path":{"id":"codex"},"body":{"os":["windows"],"repositories":["kunkunGames/AgentDesk"]}}), json!({"execution_requirements":{"os":["windows"],"repositories":["kunkunGames/AgentDesk"]}})),
        ep("GET", "/api/internal/node-probe", "cluster",
            "Authenticated node identity and session-forwarding protocol probe. Used to verify that the configured peer endpoint belongs to the advertised node; does not grant access to worker admin routes."),
    ]
}
