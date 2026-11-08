use serde_json::json;

use super::super::{EndpointDoc, body_param, ep, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "GET",
            "/api/campaigns",
            "campaigns",
            "List durable campaign DAG checkpoints from canonical PostgreSQL, newest updated first. `live` maps campaign id -> node id -> what the kanban card behind the node's GitHub issue_url is doing now (card_id, card_status, newest dispatch_type/dispatch_status, session_status/session_seen_at of the session holding that dispatch, running, newest auto-queue queue_status); `running` is true only while that session is mid-turn with a fresh heartbeat. It is read per request and never stored in the ledger. `issue_drift` maps campaign id -> completed nodes whose linked issue github-sync saw open (reason open_issue or changed_after_auto_close); it is reported, never acted on. Answers carry a strong `ETag` and `Cache-Control: no-cache`; a request whose If-None-Match lists that tag gets 304 Not Modified with an empty body.",
        )
        .with_params([
            ("limit", query_param("integer", false, "Page size 1..500; default 100.")),
            ("offset", query_param("integer", false, "Zero-based offset; default 0.")),
            ("view", query_param("string", false, "full (default) or summary; any other value is 400. summary drops each node's details, evidence, evidence_records and findings and marks the campaign nodes_partial: true; read them, and any document to write back, from GET /api/campaigns/{id}.")),
        ])
        .with_example(json!({}), json!({"campaigns": [], "live": {}, "issue_drift": {}, "limit": 100, "offset": 0}))
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
            ("nodes", body_param("array", false, "Full DAG: id/title/status/stage/group/round, dependencies, assignee/session_id/provider, issue_url/pr_url/head_sha, details/acceptance/findings/evidence/evidence_records/next_action/blocker, optional summary/benefit/post_merge_hold (work left after merge; while set a closed issue does not complete the node). Optional group is trimmed; blank/omitted/null stays unclassified, independent of stage/status. See docs/campaign-ledger.md.")),
            ("auto_queue", body_param("boolean", false, "Hand ready nodes to auto-queue automatically while the campaign is active; default false. The response then carries `handoff`.")),
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
            "Read the canonical checkpoint before resuming after clear, compaction, quota interruption, or provider/session replacement. Recorded running state is not proof of a live process; check `live` (node id -> card, dispatch, session and queue status of the node's issue card), where `running` means a session is mid-turn on it now. `issue_drift` lists completed nodes whose linked issue is open. The response carries a strong `ETag` (hash of the whole body, so live/issue_drift changes count) and `Cache-Control: no-cache`; a request whose If-None-Match lists that tag gets 304 Not Modified with an empty body.",
        )
        .with_params([("id", path_param("Campaign ID."))])
        .with_example(json!({"path": {"id": "release-a"}}), json!({"campaign": {"id": "release-a", "title": "Release A", "description": "", "status": "planned", "round": 1, "revision": 1, "nodes": [], "created_at": "2026-09-20T00:00:00Z", "updated_at": "2026-09-20T00:00:00Z"}, "live": {}, "issue_drift": []}))
        .with_curl("curl -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" \"$ADK_URL/api/campaigns/release-a\""),
        ep(
            "PUT",
            "/api/campaigns/{id}",
            "campaigns",
            "Atomically replace the complete campaign DAG and append revision history. Requires the revision just read; stale writers receive 409 and must reload/reconcile. Omitted nodes are removed from the current DAG, retained in history. Build the body from GET /api/campaigns/{id}, not a view=summary list: a summary node written back clears its details, evidence, evidence_records and findings, and a document marked nodes_partial: true is refused with 400.",
        )
        .with_params([
            ("id", path_param("Campaign ID.")),
            ("expected_revision", body_param("integer", true, "Revision observed by the caller.")),
            ("title", body_param("string", true, "Campaign title.")),
            ("description", body_param("string", false, "Durable objective and context.")),
            ("status", body_param("string", true, "planned|active|paused|completed|cancelled")),
            ("round", body_param("integer", true, "Positive campaign round.")),
            ("nodes", body_param("array", false, "Complete replacement DAG, including durable evidence, next actions and optional group labels. Group changes share revision CAS/history; blank/omitted/null means unclassified.")),
            ("auto_queue", body_param("boolean", false, "Turn automatic handoff on or off; omitted keeps the stored value. While on and active, pending nodes whose dependencies are done (completed/skipped, or pending/running with a terminal card) join their card agent's live auto-queue run or a new `campaign` run, after every save and every terminal card. The response carries `handoff` {queued, waiting with reasons} or `handoff_error`; see docs/campaign-ledger.md.")),
        ])
        .with_example(json!({"path": {"id": "release-a"}, "body": {"expected_revision": 1, "title": "Release A", "status": "paused", "round": 1, "nodes": []}}), json!({"campaign": {"id": "release-a", "title": "Release A", "description": "", "status": "paused", "round": 1, "revision": 2, "nodes": [], "created_at": "2026-09-20T00:00:00Z", "updated_at": "2026-09-20T00:01:00Z"}}))
        .with_error_example(409, json!({"body": {"expected_revision": 1, "title": "Release A", "status": "paused", "round": 1}}), json!({"error": "campaign revision conflict; reload before retrying", "code": "conflict", "context": {}}))
        .with_curl("curl -X PUT -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" -H 'Content-Type: application/json' \"$ADK_URL/api/campaigns/release-a\" -d '{\"expected_revision\":1,\"title\":\"Release A\",\"status\":\"paused\",\"round\":1,\"nodes\":[]}'"),
        ep(
            "PATCH",
            "/api/campaigns/{id}/nodes/{node_id}",
            "campaigns",
            "Update one node against the server's full stored campaign. Omitted fields and other nodes stay unchanged; arrays replace the whole field. Explicit null clears optional scalars to null, arrays to [], and details to an empty string; required title/status/stage/round cannot be cleared (400). Shares PUT's full DAG validation, revision CAS, history transaction and active auto_queue handoff/handoff_error response. Stale revision returns 409; missing campaign or node returns 404; invalid dependencies, cycles, unknown/read-only fields or invalid node values return 400. Missing or malformed expected_revision returns 422. Reload/reconcile on 409.",
        )
        .with_params([
            ("id", path_param("Campaign ID.")),
            ("node_id", path_param("Existing node ID; cannot be changed in the body.")),
            ("expected_revision", body_param("integer", true, "Observed campaign revision; positive and incrementable. Must equal the revision of the server's initial full read; otherwise 409 before merging or saving.")),
            ("title", body_param("string", false, "Nonempty node title, at most 512 bytes.")),
            ("status", body_param("string", false, "pending|running|blocked|completed|failed|skipped")),
            ("stage", body_param("string", false, "Nonempty workflow stage, at most 128 bytes.")),
            ("round", body_param("integer", false, "Positive node round.")),
            ("group", body_param("string|null", false, "Organizational label; trimmed, blank/null clears it.")),
            ("assignee", body_param("string|null", false, "Node assignee.")),
            ("session_id", body_param("string|null", false, "Historical session reference.")),
            ("provider", body_param("string|null", false, "Provider label.")),
            ("dependencies", body_param("array|null", false, "Replacement node ID array; must exist and keep the full DAG acyclic.")),
            ("issue_url", body_param("string|null", false, "Linked issue; changing it clears server-owned issue sync metadata as in PUT.")),
            ("pr_url", body_param("string|null", false, "Linked pull request.")),
            ("head_sha", body_param("string|null", false, "Recorded commit.")),
            ("evidence", body_param("array|null", false, "Replacement evidence string array.")),
            ("next_action", body_param("string|null", false, "Next action.")),
            ("blocker", body_param("string|null", false, "Current blocker.")),
            ("summary", body_param("string|null", false, "One-line gist.")),
            ("benefit", body_param("string|null", false, "Expected effect.")),
            ("post_merge_hold", body_param("string|null", false, "Remaining post-merge work; null clears it.")),
            ("details", body_param("string|null", false, "Durable notes; null clears to an empty string.")),
            ("acceptance", body_param("array|null", false, "Replacement acceptance string array.")),
            ("findings", body_param("array|null", false, "Replacement findings string array.")),
            ("evidence_records", body_param("array|null", false, "Replacement structured evidence array; see docs/campaign-ledger.md.")),
        ])
        .with_example(json!({"path": {"id": "release-a", "node_id": "review"}, "body": {"expected_revision": 1, "status": "running"}}), json!({"campaign": {"id": "release-a", "revision": 2, "nodes": [{"id": "review", "status": "running"}]}}))
        .with_error_example(400, json!({"body": {"expected_revision": 1, "dependencies": ["absent"]}}), json!({"error": "node review has missing dependency absent", "code": "validation", "context": {}}))
        .with_error_example(404, json!({"path": {"id": "release-a", "node_id": "absent"}}), json!({"error": "campaign node not found", "code": "not_found", "context": {}}))
        .with_error_example(409, json!({"body": {"expected_revision": 1, "status": "running"}}), json!({"error": "campaign revision conflict; reload before retrying", "code": "conflict", "context": {}}))
        .with_curl("curl -X PATCH -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" -H 'Content-Type: application/json' \"$ADK_URL/api/campaigns/release-a/nodes/review\" -d '{\"expected_revision\":1,\"status\":\"running\"}'"),
        ep(
            "GET",
            "/api/campaigns/{id}/history",
            "campaigns",
            "Read the retained campaign revision snapshots, newest revision first. Each write prunes all but the newest 10, so older revisions are not recoverable from this endpoint.",
        )
        .with_params([("id", path_param("Campaign ID."))])
        .with_example(json!({"path": {"id": "release-a"}}), json!({"revisions": [{"id": "release-a", "title": "Release A", "description": "", "status": "planned", "round": 1, "revision": 1, "nodes": [], "created_at": "2026-09-20T00:00:00Z", "updated_at": "2026-09-20T00:00:00Z"}]}))
        .with_curl("curl -H \"Authorization: Bearer $ADK_AUTH_TOKEN\" \"$ADK_URL/api/campaigns/release-a/history\""),
    ]
}
