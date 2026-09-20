use serde_json::json;

use super::super::{EndpointDoc, body_param, ep, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "GET",
            "/api/campaigns",
            "campaigns",
            "List durable campaign DAG checkpoints from canonical PostgreSQL, newest updated first.",
        )
        .with_params([
            ("limit", query_param("integer", false, "Page size 1..500; default 100.")),
            ("offset", query_param("integer", false, "Zero-based offset; default 0.")),
        ])
        .with_example(json!({}), json!({"campaigns": [], "limit": 100, "offset": 0}))
        .with_curl("curl -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" \"$ADK_URL/api/campaigns\""),
        ep(
            "POST",
            "/api/campaigns",
            "campaigns",
            "Create a durable campaign at revision 1. Validates unique node IDs, existing dependencies and acyclic DAG; duplicate campaign IDs return 409.",
        )
        .with_params([
            ("id", body_param("string", false, "Stable client ID, otherwise a generated UUID.")),
            ("title", body_param("string", true, "Nonempty title, at most 512 bytes.")),
            ("description", body_param("string", false, "Durable objective and context.")),
            ("status", body_param("string", true, "planned|active|paused|completed|cancelled")),
            ("round", body_param("integer", true, "Positive campaign round.")),
            ("nodes", body_param("array", false, "Full DAG: id/title/status/stage/round, dependencies, assignee/session_id/provider, issue_url/pr_url/head_sha, details/acceptance/findings/evidence/evidence_records/next_action/blocker. See docs/campaign-ledger.md.")),
        ])
        .with_example(
            json!({"body": {"id": "release-a", "title": "Release A", "status": "planned", "round": 1, "nodes": []}}),
            json!({"campaign": {"id": "release-a", "title": "Release A", "description": "", "status": "planned", "round": 1, "revision": 1, "nodes": [], "created_at": "2026-09-20T00:00:00Z", "updated_at": "2026-09-20T00:00:00Z"}}),
        )
        .with_error_example(409, json!({"body": {"id": "existing", "title": "Existing", "status": "planned", "round": 1}}), json!({"error": "campaign revision conflict; reload before retrying", "code": "conflict", "context": {}}))
        .with_curl("curl -X POST -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" -H 'Content-Type: application/json' \"$ADK_URL/api/campaigns\" -d '{\"id\":\"release-a\",\"title\":\"Release A\",\"status\":\"planned\",\"round\":1,\"nodes\":[]}'"),
        ep(
            "GET",
            "/api/campaigns/{id}",
            "campaigns",
            "Read the canonical checkpoint before resuming after clear, compaction, quota interruption, or provider/session replacement. Recorded running state is not proof of a live process.",
        )
        .with_params([("id", path_param("Campaign ID."))])
        .with_example(json!({"path": {"id": "release-a"}}), json!({"campaign": {"id": "release-a", "title": "Release A", "description": "", "status": "planned", "round": 1, "revision": 1, "nodes": [], "created_at": "2026-09-20T00:00:00Z", "updated_at": "2026-09-20T00:00:00Z"}}))
        .with_curl("curl -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" \"$ADK_URL/api/campaigns/release-a\""),
        ep(
            "PUT",
            "/api/campaigns/{id}",
            "campaigns",
            "Atomically replace the complete campaign DAG and append revision history. Requires the revision just read; stale writers receive 409 and must reload/reconcile. Omitted nodes are removed from the current DAG, retained in history.",
        )
        .with_params([
            ("id", path_param("Campaign ID.")),
            ("expected_revision", body_param("integer", true, "Revision observed by the caller.")),
            ("title", body_param("string", true, "Campaign title.")),
            ("description", body_param("string", false, "Durable objective and context.")),
            ("status", body_param("string", true, "planned|active|paused|completed|cancelled")),
            ("round", body_param("integer", true, "Positive campaign round.")),
            ("nodes", body_param("array", false, "Complete replacement DAG, including durable evidence and next actions.")),
        ])
        .with_example(json!({"path": {"id": "release-a"}, "body": {"expected_revision": 1, "title": "Release A", "status": "paused", "round": 1, "nodes": []}}), json!({"campaign": {"id": "release-a", "title": "Release A", "description": "", "status": "paused", "round": 1, "revision": 2, "nodes": [], "created_at": "2026-09-20T00:00:00Z", "updated_at": "2026-09-20T00:01:00Z"}}))
        .with_error_example(409, json!({"body": {"expected_revision": 1, "title": "Release A", "status": "paused", "round": 1}}), json!({"error": "campaign revision conflict; reload before retrying", "code": "conflict", "context": {}}))
        .with_curl("curl -X PUT -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" -H 'Content-Type: application/json' \"$ADK_URL/api/campaigns/release-a\" -d '{\"expected_revision\":1,\"title\":\"Release A\",\"status\":\"paused\",\"round\":1,\"nodes\":[]}'"),
        ep(
            "GET",
            "/api/campaigns/{id}/history",
            "campaigns",
            "Read the latest 50 durable campaign revision snapshots, newest revision first.",
        )
        .with_params([("id", path_param("Campaign ID."))])
        .with_example(json!({"path": {"id": "release-a"}}), json!({"revisions": [{"id": "release-a", "title": "Release A", "description": "", "status": "planned", "round": 1, "revision": 1, "nodes": [], "created_at": "2026-09-20T00:00:00Z", "updated_at": "2026-09-20T00:00:00Z"}]}))
        .with_curl("curl -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" \"$ADK_URL/api/campaigns/release-a/history\""),
    ]
}
