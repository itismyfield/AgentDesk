use serde_json::json;

use super::super::{EndpointDoc, body_param, ep, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "POST",
            "/api/message-outbox/monitor-alerts",
            "health",
            "Durably enqueue one auto-queue monitor alert or recovery by its persisted action ID; replaying the ID is idempotent.",
        )
        .with_params([
            ("target", body_param("string", true, "Discord channel target in channel:<positive id> form.")),
            ("content", body_param("string", true, "Non-empty notification text, at most 2000 bytes.")),
            ("action_id", body_param("string", true, "Persisted 32-character lowercase hexadecimal monitor action ID.")),
            ("action", body_param("string", true, "Either alert or recovery.")),
        ])
        .with_example(
            json!({"body": {"target": "channel:1479671298497183835", "content": "[auto-queue monitor] STUCK: #4448", "action_id": "0123456789abcdef0123456789abcdef", "action": "alert"}}),
            json!({"ok": true, "enqueued": true, "action_id": "0123456789abcdef0123456789abcdef"}),
        )
        .with_error_example(
            400,
            json!({"body": {"target": "channel:123", "content": "alert", "action_id": "bad", "action": "alert"}}),
            json!({"ok": false, "error": "action_id must be 32 lowercase hexadecimal characters"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/message-outbox/monitor-alerts -H 'Content-Type: application/json' -d '{\"target\":\"channel:1479671298497183835\",\"content\":\"[auto-queue monitor] STUCK: #4448\",\"action_id\":\"0123456789abcdef0123456789abcdef\",\"action\":\"alert\"}'"),
        ep(
            "GET",
            "/api/message-outbox/failed",
            "health",
            "Inspect only the requested message_outbox IDs with bounded content/error previews and semantic sibling state.",
        )
        .with_params([(
            "ids",
            query_param(
                "string",
                true,
                "Comma-separated exact message_outbox IDs; 1 to 50 positive unique IDs. No all/mass mode.",
            ),
        )])
        .with_example(
            json!({"query": {"ids": "13651,13652,13653"}}),
            json!({
                "ok": true,
                "count": 1,
                "missing_ids": [],
                "rows": [{
                    "id": 13651,
                    "status": "failed",
                    "target": "channel:1479671301387059200",
                    "bot": "notify",
                    "source": "catch_up_too_old",
                    "reason_code": "catch_up_too_old",
                    "session_key": "catch_up_too_old:1479671301387059200:1524946021082337372",
                    "retry_count": 5,
                    "error_snippet": "source not allowed for this caller",
                    "dedupe_key": "message_outbox:v1:…",
                    "content_snippet": "…",
                    "content_hash": "blake3-hex",
                    "semantic_siblings": []
                }]
            }),
        )
        .with_error_example(
            400,
            json!({"query": {"ids": "all"}}),
            json!({"ok": false, "error": "ids must be a comma-separated list of positive integers"}),
        )
        .with_curl("curl 'http://localhost:8787/api/message-outbox/failed?ids=13651,13652,13653'"),
        ep(
            "POST",
            "/api/message-outbox/failed/redrive",
            "health",
            "Dry-run by default or idempotently redrive only requested failed message_outbox rows after source and semantic-sibling preflight.",
        )
        .with_params([
            ("ids", body_param("array<integer>", true, "1 to 50 exact positive unique message_outbox IDs; no all/mass mode.")),
            ("idempotency_key", body_param("string", true, "Stable operator key, at most 128 bytes; replaying it never mutates again.")),
            ("reason", body_param("string", true, "Operator audit reason, at most 500 bytes.")),
            ("dry_run", body_param("boolean", false, "Defaults true; false applies only would_redrive outcomes.")),
        ])
        .with_example(
            json!({"body": {"ids": [13651, 13652, 13653], "idempotency_key": "issue-4424-catchup-notices-v1", "reason": "recover verified P0 incident rows", "dry_run": false}}),
            json!({"ok": true, "dry_run": false, "idempotency_key": "issue-4424-catchup-notices-v1", "results": [{"id": 13651, "outcome": "redriven"}]}),
        )
        .with_dry_run_example(
            json!({"body": {"ids": [13651, 13652, 13653], "idempotency_key": "issue-4424-catchup-notices-v1", "reason": "recover verified P0 incident rows"}}),
            json!({"ok": true, "dry_run": true, "results": [{"id": 13651, "outcome": "would_redrive"}]}),
        )
        .with_error_example(
            409,
            json!({"body": {"ids": [13651], "idempotency_key": "issue-4424-v1", "reason": "operator recovery", "dry_run": false}}),
            json!({"ok": false, "code": "source_not_allowed", "id": 13651, "error": "message_outbox row 13651 source `unknown` is not registered for LoopbackInternal"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/message-outbox/failed/redrive -H 'Content-Type: application/json' -d '{\"ids\":[13651,13652,13653],\"idempotency_key\":\"issue-4424-catchup-notices-v1\",\"reason\":\"recover verified P0 incident rows\"}'"),
    ]
}
