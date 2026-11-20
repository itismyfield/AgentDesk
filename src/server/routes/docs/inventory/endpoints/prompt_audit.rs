use serde_json::json;

use super::super::{EndpointDoc, ep, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    let params = [
        ("id", path_param("Registered agent ID.")),
        (
            "provider",
            query_param(
                "string",
                false,
                "Bound provider: claude or codex. Defaults to the agent's primary provider.",
            ),
        ),
    ];
    vec![
        ep("GET", "/api/agents/{id}/prompt-audit", "agents",
            "Inspect launch instructions. origin is current_session, latest_saved or canon_reassembled. Timestamps and logical file paths can be null. Section source is canon_file, memento, runtime or conditional. chars counts Unicode characters in the displayed text. Each credential match contributes to masked_count. The JSON body uses Cache-Control: no-store.")
            .with_params(params.clone())
            .with_example(json!(null), json!({
                "agent_id":"example-agent","provider":"claude","session_id":"example-session",
                "origin":"current_session","session_started_at":"2026-11-20T01:00:00Z",
                "canon_modified_at":"2026-11-19T22:00:00Z","masked_count":1,
                "sections":[{"name":"role_prompt","source":"canon_file","path":"agents/role.prompt.md",
                    "chars":29,"text":"Follow the role guidance.\n***","masked_count":1}]
            })),
        ep("GET", "/api/agents/{id}/anchors", "agents",
            "Read the anchors injected into the selected provider session, with the most recent saved copy available when no current session is bound. source is current_session, latest_saved or unavailable. memento.status is connected, disconnected, unknown or unconfigured and checked_at is nullable. Metadata missing from legacy snapshots is null. overlap_candidates points to zero-based anchor indexes or logical canon paths. The JSON body uses Cache-Control: no-store.")
            .with_params(params)
            .with_example(json!(null), json!({
                "agent_id":"example-agent","provider":"claude","session_id":"example-session",
                "session_started_at":"2026-11-20T01:00:00Z","source":"latest_saved",
                "memento":{"status":"disconnected","checked_at":"2026-11-20T01:05:00Z"},
                "anchor_count":1,"anchors":[{"id":"frag-example","type":"procedure","scope":"project",
                    "chars":21,"limit":300,"text":"Reuse canon guidance.","masked_count":0,
                    "overlap_candidates":[{"source":"canon_file","path":"agents/_shared.prompt.md"}],
                    "receiving_agents":["example-agent"]}]
            })),
    ]
}
