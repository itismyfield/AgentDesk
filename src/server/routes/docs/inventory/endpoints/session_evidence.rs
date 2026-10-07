use super::super::{EndpointDoc, ep, path_param};
use serde_json::json;

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![ep("GET", "/api/agents/{id}/session-evidence", "agents",
        "Observe the stored session identity in one SELECT snapshot; no cleanup, tmux probe or event. Missing raw provider identity is null; ambiguous bindings return 409.")
        .with_params([("id", path_param("Agent ID or numeric Discord channel ID"))])
        .with_example(json!(null), json!({"agent_id":"adk-claude-tui-e2e","channel_id":"1509350490461180105","provider":"claude","session_key":"claude/hash/host:session","raw_provider_session_id":null}))]
}
