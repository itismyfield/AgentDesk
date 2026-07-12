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
        ),
        // claude-accounts (cswap) usage panel + global auth switch (#4089)
        ep(
            "GET",
            "/api/claude-accounts",
            "claude-accounts",
            "List Claude accounts with 5h/7d usage from cswap --list --json (60s server cache). Reports installed=false with an install hint when cswap is absent.",
        )
        .with_example(
            json!(null),
            json!({
                "installed": true,
                "hostname": "mac-mini",
                "fetched_at": "2026-01-01T00:00:00Z",
                "accounts": [{"email": "user@example.com", "active": true, "usage": {"fiveHour": {"pct": 12, "resetsAt": "2026-01-01T01:00:00Z"}, "sevenDay": {"pct": 40, "resetsAt": "2026-01-04T00:00:00Z"}}}]
            }),
        ),
        ep(
            "POST",
            "/api/claude-accounts/switch",
            "claude-accounts",
            "Switch the machine-global Claude auth via cswap --switch-to (single-flight, 20s timeout). Schedules a best-effort leader rate-limit refresh; the switch applies on the receiving node only.",
        )
        .with_params([
            ("account", body_param("string", true, "Account number or email as shown by the list endpoint")),
        ])
        .with_example(
            json!({"body": {"account": "user@example.com"}}),
            json!({"switched": true, "from": "old@example.com", "to": "user@example.com", "reason": null, "hostname": "mac-mini", "rate_limit_refresh": {"scheduled": true}}),
        ),
        ep(
            "GET",
            "/api/scheduled-messages",
            "messages",
            "List scheduled-message reservations with status/kind/agent/channel filters and cursor pagination.",
        )
        .with_params([
            ("status", query_param("string", false, "Filter by scheduled, firing, sent, failed, canceled, or expired")),
            ("deliveryKind", query_param("string", false, "Filter by push or agent")),
            ("agentId", query_param("string", false, "Filter by delivering agent")),
            ("targetChannelId", query_param("string", false, "Filter by target Discord channel id")),
            ("dueBefore", query_param("string", false, "RFC3339 upper bound on scheduledAt")),
            ("dueAfter", query_param("string", false, "RFC3339 lower bound on scheduledAt")),
            ("before", query_param("string", false, "Cursor: createdAt returned by a prior page")),
            ("limit", query_param("integer", false, "Page size (default 50, max 200)")),
        ])
        .with_example(
            json!({"query": {"status": "scheduled"}}),
            json!({"scheduledMessages": [{"id": "smsg_1", "content": "standup agenda", "deliveryKind": "push", "targetChannelId": "123", "scheduledAt": "2026-07-09T09:00:00+00:00", "status": "scheduled"}], "nextCursor": "2026-07-08T01:00:00+00:00"}),
        ),
        ep(
            "POST",
            "/api/scheduled-messages",
            "messages",
            "Create a scheduled-message reservation delivered at scheduledAt via direct push or a delivering agent.",
        )
        .with_params([
            ("content", body_param("string", true, "Message body to deliver")),
            ("scheduledAt", body_param("string", true, "RFC3339 fire time; past values require a schedule and are advanced to the next occurrence")),
            ("targetChannelId", body_param("string", false, "Discord channel id (required for push; agent falls back to its primary channel)")),
            ("deliveryKind", body_param("string", false, "push (default) or agent")),
            ("agentId", body_param("string", false, "Delivering agent id (required for agent kind)")),
            ("agentInstruction", body_param("string", false, "Extra instruction injected into the agent turn")),
            ("onAgentFailure", body_param("string", false, "fail (default) or push_raw to demote to a direct push")),
            ("schedule", body_param("string", false, "Recurrence: '@every <duration>' or 5-field cron (routine grammar)")),
            ("timezone", body_param("string", false, "Cron timezone (default Asia/Seoul)")),
            ("expiresAt", body_param("string", false, "RFC3339 end of a recurring reservation")),
            ("bot", body_param("string", false, "Delivery bot (default notify; announce intentionally wakes a receiving agent)")),
            ("title", body_param("string", false, "Display title")),
            ("dedupeKey", body_param("string", false, "Idempotency key; unique among live reservations")),
        ])
        .with_example(
            json!({"body": {"content": "standup agenda", "targetChannelId": "123", "scheduledAt": "2026-07-09T09:00:00+09:00"}}),
            json!({"scheduledMessage": {"id": "smsg_1", "status": "scheduled", "deliveryKind": "push", "scheduledAt": "2026-07-09T00:00:00+00:00"}}),
        ),
        ep(
            "GET",
            "/api/scheduled-messages/{id}",
            "messages",
            "Fetch one scheduled-message reservation with its five most recent deliveries.",
        )
        .with_params([("id", path_param("Scheduled message id"))]),
        ep(
            "PATCH",
            "/api/scheduled-messages/{id}",
            "messages",
            "Edit a reservation that is still in scheduled status; null clears nullable fields.",
        )
        .with_params([("id", path_param("Scheduled message id"))]),
        ep(
            "DELETE",
            "/api/scheduled-messages/{id}",
            "messages",
            "Cancel a scheduled or firing reservation; in-flight deliveries already handed to the outbox cannot be recalled.",
        )
        .with_params([("id", path_param("Scheduled message id"))]),
        ep(
            "POST",
            "/api/scheduled-messages/{id}/trigger-now",
            "messages",
            "Fire a scheduled reservation immediately in a one-off slot; a recurring reservation keeps its original scheduledAt.",
        )
        .with_params([("id", path_param("Scheduled message id"))])
        .with_example(
            json!({}),
            json!({"delivery": {"id": "smdel_1", "status": "running"}}),
        ),
        ep(
            "GET",
            "/api/scheduled-messages/{id}/deliveries",
            "messages",
            "Fire history for one reservation, enriched with the final message_outbox status of each handoff.",
        )
        .with_params([
            ("id", path_param("Scheduled message id")),
            ("limit", query_param("integer", false, "Page size (default 20, max 100)")),
            ("before", query_param("string", false, "Cursor: createdAt returned by a prior page")),
        ]),
    ]
}
