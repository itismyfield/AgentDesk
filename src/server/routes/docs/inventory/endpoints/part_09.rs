use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "GET",
            "/api/v1/stream",
            "v1",
            "Versioned Server-Sent Events stream with optional last-event-id replay.",
        ),
        ep(
            "GET",
            "/api/v1/activity",
            "v1",
            "Versioned activity feed with limit and cursor pagination.",
        )
        .with_params([
            ("limit", query_param("integer", false, "Maximum activity items")),
            ("before", query_param("string", false, "Cursor returned by a prior page")),
        ]),
        ep(
            "GET",
            "/api/v1/achievements",
            "v1",
            "Versioned achievement bundle, optionally scoped to an agent.",
        )
        .with_params([(
            "agentId",
            query_param("string", false, "Optional agent id filter"),
        )]),
        ep(
            "GET",
            "/api/v1/settings",
            "v1",
            "Versioned settings list compatible with the dashboard settings surface.",
        ),
        ep(
            "PATCH",
            "/api/v1/settings/{key}",
            "v1",
            "Patch one versioned settings value using the dashboard-compatible settings contract.",
        )
        .with_params([
            ("key", path_param("Settings key")),
            ("value", body_param("any", true, "New settings value")),
        ]),
        // provider-cli safe migration
        ep(
            "GET",
            "/api/provider-cli",
            "provider-cli",
            "List current channel snapshots and migration states for all providers (codex, claude, gemini, qwen).",
        )
        .with_example(
            json!(null),
            json!({
                "providers": [{"provider": "codex", "current": null, "candidate": null}],
                "migrations": [],
                "generated_at": "2026-01-01T00:00:00Z"
            }),
        ),
        ep(
            "PATCH",
            "/api/provider-cli/{provider}",
            "provider-cli",
            "Apply an operator action to a provider migration state. Actions: confirm_promote, rollback, rollback_to_previous.",
        )
        .with_params([
            (
                "provider",
                path_param("Provider id: codex, claude, gemini, or qwen"),
            ),
            ("action", body_param("string", true, "confirm_promote | rollback | rollback_to_previous")),
            ("evidence", body_param("string", false, "Optional operator note recorded in migration history")),
        ])
        .with_example(
            json!({"body": {"action": "confirm_promote", "evidence": "operator approved via Discord"}}),
            json!({"provider": "codex", "action": "confirm_promote", "state": "ProviderAgentsMigrated", "updated_at": "2026-01-01T00:00:00Z"}),
        )
    ]
}
