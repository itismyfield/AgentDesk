use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "POST",
            "/api/kanban-cards/{id}/redispatch",
            "kanban",
            "Redispatch: cancel the current live dispatch and create a brand-new dispatch entry with a new dispatch_id for the same card intent. Distinct from /retry (re-executes the SAME step with the same params), /resume (continues a checkpoint), and /reopen (re-admits a closed card). Single-call complete: do NOT chain /transition or /queue/generate after it (#1442). See /api/docs/card-lifecycle-ops for the full decision tree (#1443).",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "reason",
                body_param("string", false, "Optional redispatch rationale"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"reason": "stale thread"}}),
            json!({
                "card": {"id": "card-1", "latest_dispatch_id": "dispatch-redispatch-1", "status": "requested"},
                "new_dispatch_id": "dispatch-redispatch-1",
                "cancelled_dispatch_id": "dispatch-old-1",
                "next_action": "none_required"
            }),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "card-unknown"}, "body": {"reason": "stale thread"}}),
            json!({"error": "card not found: card-unknown"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/redispatch -H 'Content-Type: application/json' -d '{\"reason\":\"stale thread\"}'"),
        ep(
            "POST",
            "/api/kanban-cards/{id}/resume",
            "kanban",
            "Resume: continue a stuck/paused card from its current checkpointed state by inspecting review/dispatch state and issuing the minimal next action. Distinct from /retry (re-run same failed step), /redispatch (new dispatch id), and /reopen (re-admit closed card).",
        )
        .with_params([
            (
                "id",
                path_param("Card ID or GitHub issue number for the most recent matching card"),
            ),
            (
                "force",
                body_param("boolean", false, "Bypass guards for manual-intervention review/in-progress states")
                    .with_default(false),
            ),
            (
                "reason",
                body_param("string", false, "Audit reason for the resume"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-resume"}, "body": {"reason": "manual resume"}}),
            json!({"card": {"id": "card-resume", "status": "in_progress"}, "action": {"type": "new_implementation_dispatch", "dispatch_id": "dispatch-resume-1"}}),
        )
        .with_error_example(
            409,
            json!({"path": {"id": "card-in-review"}, "body": {}}),
            json!({"error": "resume blocked: card is in manual-intervention review; retry with force=true"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-resume/resume -H 'Content-Type: application/json' -d '{\"reason\":\"manual resume\"}'"),
        ep(
            "PATCH",
            "/api/kanban-cards/{id}/defer-dod",
            "kanban",
            "Update deferred DoD items",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "items",
                body_param("string[]", false, "Replace the full deferred DoD item list"),
            ),
            (
                "verify",
                body_param("string[]", false, "Mark DoD items as verified"),
            ),
            (
                "unverify",
                body_param("string[]", false, "Remove DoD items from verified set"),
            ),
            (
                "remove",
                body_param("string[]", false, "Remove items from items and verified"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"items": ["ship tests"], "verify": ["ship tests"]}}),
            json!({"card": {"id": "card-1", "deferred_dod": {"items": ["ship tests"], "verified": ["ship tests"]}}}),
        ),
        ep(
            "GET",
            "/api/kanban-cards/{id}/reviews",
            "kanban",
            "List reviews for card // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/review-state",
            "kanban",
            "Get canonical review-state record for card // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/audit-log",
            "kanban",
            "Get audit log for card // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/comments",
            "kanban",
            "Get GitHub comments for linked card issue // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "POST",
            "/api/automation-candidates/{card_id}/prepare-worktree",
            "automation-candidates",
            "Prepare or reuse an isolated git worktree for one automation-candidate iteration.",
        )
        .with_params([
            ("card_id", path_param("Automation candidate card ID")),
            ("iteration", body_param("integer", true, "Iteration number, starting at 1")),
        ])
        .with_example(
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 1}}),
            json!({"path": "/tmp/agentdesk-worktrees/card-1-1", "branch": "automation/card-1/iter-1", "commit": "abc123", "created": true}),
        )
        .with_error_example(
            400,
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 0}}),
            json!({"error": "iteration must be >= 1"}),
        )
        .with_error_example(
            409,
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 3}}),
            json!({"error": "iteration out of sequence: expected 2, got 3", "code": "ITERATION_OUT_OF_SEQUENCE", "expected": 2, "actual": 3}),
        )
        .with_error_example(
            409,
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 2}}),
            json!({"error": "automation candidate is not executable in status 'review'", "code": "INACTIVE_AUTOMATION_CANDIDATE", "status": "review"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/automation-candidates/card-1/prepare-worktree -H 'Content-Type: application/json' -d '{\"iteration\":1}'"),
        ep(
            "POST",
            "/api/automation-candidates/{card_id}/iteration-result",
            "automation-candidates",
            "Submit one iteration result. Rust computes keep/discard deterministically from the card program contract.",
        )
        .with_params([
            ("card_id", path_param("Automation candidate card ID")),
            ("iteration", body_param("integer", true, "Iteration number")),
            ("branch", body_param("string", true, "Automation branch name")),
            ("metric_before", body_param("number", false, "Metric value before changes")),
            ("metric_after", body_param("number", false, "Metric value after changes")),
            ("allowed_write_paths_used", body_param("array<string>", true, "Non-empty changed-path report; all paths must be within program.allowed_write_paths")),
        ])
        .with_example(
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 1, "branch": "automation/card-1/iter-1", "metric_before": 0.4, "metric_after": 0.2, "status": "keep", "allowed_write_paths_used": ["src/services/discord/router.rs"]}}),
            json!({"verdict": "keep", "action": "keep_continue", "child_card_id": null}),
        )
        .with_error_example(
            400,
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 1, "branch": "automation/card-1/iter-1", "status": "keep"}}),
            json!({"error": "allowed_write_paths_used is required and must be non-empty", "code": "MISSING_CHANGED_PATHS_REPORT"}),
        )
        .with_error_example(
            403,
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 1, "branch": "automation/card-1/iter-1", "allowed_write_paths_used": ["migrations/secret.sql"]}}),
            json!({"error": "path 'migrations/secret.sql' is not in allowed_write_paths", "code": "ALLOWED_PATHS_VIOLATION", "path": "migrations/secret.sql"}),
        )
        .with_error_example(
            409,
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 3, "branch": "automation/card-1/iter-3", "allowed_write_paths_used": ["src/services/discord/router.rs"]}}),
            json!({"error": "iteration out of sequence: expected 2, got 3", "code": "ITERATION_OUT_OF_SEQUENCE", "expected": 2, "actual": 3}),
        )
        .with_error_example(
            409,
            json!({"path": {"card_id": "card-1"}, "body": {"iteration": 2, "branch": "automation/card-1/iter-2", "allowed_write_paths_used": ["src/services/discord/router.rs"]}}),
            json!({"error": "automation candidate is not executable in status 'review'", "code": "INACTIVE_AUTOMATION_CANDIDATE", "status": "review"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/automation-candidates/card-1/iteration-result -H 'Content-Type: application/json' -d '{\"iteration\":1,\"branch\":\"automation/card-1/iter-1\",\"metric_before\":0.4,\"metric_after\":0.2,\"status\":\"keep\",\"allowed_write_paths_used\":[\"src/services/discord/router.rs\"]}'"),
        ep(
            "GET",
            "/api/automation-candidates/{card_id}/iterations",
            "automation-candidates",
            "List all iteration records for a card in chronological order.",
        )
        .with_params([("card_id", path_param("Automation candidate card ID"))])
        .with_example(
            json!({}),
            json!({
                "iterations": [
                    { "iteration": 1, "status": "keep", "metric_before": 0.4, "metric_after": 0.2, "description": "Reduced routing failure rate", "branch": "automation/card-1/iter-1" }
                ]
            }),
        )
        .with_curl("curl http://localhost:8787/api/automation-candidates/card-1/iterations"),
        ep(
            "GET",
            "/api/automation-candidates/{card_id}/automation-inventory",
            "automation-candidates",
            "Return per-card iteration history wrapped with card_id, in the shape consumed by ctx.automationInventory[cardId] in the automation executor routine.",
        )
        .with_params([("card_id", path_param("Automation candidate card ID"))])
        .with_example(
            json!({}),
            json!({
                "card_id": "card-1",
                "iterations": [
                    { "iteration": 1, "status": "keep", "metric_before": 0.4, "metric_after": 0.2, "description": "Reduced routing failure rate", "branch": "automation/card-1/iter-1" }
                ]
            }),
        )
        .with_curl("curl http://localhost:8787/api/automation-candidates/card-1/automation-inventory"),
        ep(
            "POST",
            "/api/automation-candidates/{card_id}/approve",
            "automation-candidates",
            "Approve the final candidate. The side-effect simulator may downgrade auto_apply_after_green to manual_review.",
        )
        .with_params([("card_id", path_param("Automation candidate card ID"))])
        .with_example(
            json!({"path": {"card_id": "card-1"}}),
            json!({"status": "approved", "card_id": "card-1", "final_gate": "auto_apply_after_green", "effective_final_gate": "manual_review", "next_action": "await_manual_merge", "side_effect_simulation": {"safe_for_auto_apply": false}}),
        )
        .with_error_example(
            404,
            json!({"path": {"card_id": "ghost"}}),
            json!({"error": "automation candidate card not found"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/automation-candidates/card-1/approve"),
        ep(
            "GET",
            "/api/kanban-repos",
            "kanban-repos",
            "List kanban repos // TODO: example",
        ),
        ep(
            "POST",
            "/api/kanban-repos",
            "kanban-repos",
            "Create kanban repo // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/kanban-repos/{owner}/{repo}",
            "kanban-repos",
            "Update kanban repo // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/kanban-repos/{owner}/{repo}",
            "kanban-repos",
            "Delete kanban repo // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/kanban-reviews/{id}/decisions",
            "reviews",
            "Update review decisions // TODO: example",
        ),
        ep(
            "POST",
            "/api/kanban-reviews/{id}/trigger-rework",
            "reviews",
            "Trigger rework for review // TODO: example",
        ),
        ep(
            "POST",
            "/api/reviews/recovery",
            "reviews",
            "Recover a review dispatch target commit/worktree after stale cwd or incorrect metadata.",
        )
        .with_params([
            ("dispatch_id", body_param("string", false, "Review dispatch ID")),
            (
                "card_id",
                body_param("string", false, "Kanban card ID; resolves the latest active or failed review dispatch when dispatch_id is omitted"),
            ),
            (
                "target_commit",
                body_param("string", false, "Reviewed commit SHA to pin; must reference or belong to the card issue"),
            ),
            (
                "worktree_path",
                body_param("string", false, "Worktree path whose HEAD must match target_commit; used to infer target_commit when omitted"),
            ),
            ("reason", body_param("string", false, "Operator reason recorded in audit payload")),
        ])
        .with_example(
            json!({"body": {"dispatch_id": "review-1874-r1", "target_commit": "abc1234", "worktree_path": "/Users/me/.adk/release/workspaces/agentdesk-issue-1874", "reason": "stale cwd correction"}}),
            json!({"ok": true, "dispatch_id": "review-1874-r1", "card_id": "card-1874", "from_status": "failed", "to_status": "pending", "target": {"reviewed_commit": "abc1234", "worktree_path": "/Users/me/.adk/release/workspaces/agentdesk-issue-1874", "branch": "fix/1874-review-recovery-endpoint"}, "cleared_failure_markers": 2}),
        )
        .with_error_example(
            422,
            json!({"body": {"dispatch_id": "review-1874-r1", "target_commit": "def9999"}}),
            json!({"error": "target_commit def9999 does not reference or belong to card card-1874"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/reviews/recovery -H 'Content-Type: application/json' -d '{\"dispatch_id\":\"review-1874-r1\",\"target_commit\":\"abc1234\",\"worktree_path\":\"/Users/me/.adk/release/workspaces/agentdesk-issue-1874\",\"reason\":\"stale cwd correction\"}'"),
        ep("GET", "/api/dispatches", "dispatches", "List dispatches")
            .with_params([
                (
                    "status",
                    query_param("string", false, "Filter by dispatch status"),
                ),
                (
                    "kanban_card_id",
                    query_param("string", false, "Filter by kanban card id"),
                ),
            ])
            .with_example(
                json!({"query": {"status": "pending"}}),
                json!({"dispatches": [{"id": "dispatch-1", "kanban_card_id": "card-1", "to_agent_id": "agent-1", "status": "pending", "title": "Implement feature"}]}),
            )
            .with_error_example(
                400,
                json!({"query": {"status": "invalid"}}),
                json!({"error": "unknown dispatch status: invalid"}),
            )
            .with_curl("curl 'http://localhost:8787/api/dispatches?status=pending'"),
        ep(
            "POST",
            "/api/dispatches",
            "dispatches",
            "Create dispatch (supports optional skip_outbox for bookkeeping-only dispatches)",
        )
        .with_params([
            (
                "kanban_card_id",
                body_param("string", true, "Card to dispatch"),
            ),
            (
                "to_agent_id",
                body_param("string", true, "Target agent ID"),
            ),
            (
                "dispatch_type",
                body_param("string", false, "Dispatch type such as review or implementation"),
            ),
            ("title", body_param("string", true, "Dispatch title")),
            (
                "context",
                body_param("object", false, "Structured context payload"),
            ),
            (
                "skip_outbox",
                body_param(
                    "boolean",
                    false,
                    "Suppress notify outbox persistence for bookkeeping-only dispatches",
                )
                .with_default(false),
            ),
        ])
        .with_example(
            json!({"body": {"kanban_card_id": "card-1", "to_agent_id": "ch-td", "title": "Do it", "skip_outbox": true}}),
            json!({"dispatch": {"id": "dispatch-1", "kanban_card_id": "card-1", "to_agent_id": "ch-td", "status": "pending", "title": "Do it"}}),
        )
        .with_error_example(
            404,
            json!({"body": {"kanban_card_id": "ghost", "to_agent_id": "ghost", "title": "x"}}),
            json!({"error": "agent not found: ghost"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/dispatches -H 'Content-Type: application/json' -d '{\"kanban_card_id\":\"card-1\",\"to_agent_id\":\"ch-td\",\"title\":\"Do it\"}'"),
        ep(
            "GET",
            "/api/dispatches/{id}",
            "dispatches",
            "Get dispatch by ID",
        )
        .with_params([("id", path_param("Dispatch ID"))])
        .with_example(
            json!({"path": {"id": "dispatch-1"}}),
            json!({"dispatch": {"id": "dispatch-1", "status": "pending", "kanban_card_id": "card-1"}}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "dispatch-ghost"}}),
            json!({"error": "dispatch not found: dispatch-ghost"}),
        )
        .with_curl("curl http://localhost:8787/api/dispatches/dispatch-1"),
        ep(
            "GET",
            "/api/dispatches/{id}/events",
            "dispatches",
            "List read-only dispatch delivery events recorded in the typed delivery table for one dispatch. Returns events newest-first and never reads kv_meta.",
        )
        .with_params([("id", path_param("Dispatch ID"))])
        .with_example(
            json!({"path": {"id": "dispatch-1"}}),
            json!({
                "dispatch_id": "dispatch-1",
                "events": [{
                    "id": 1,
                    "dispatch_id": "dispatch-1",
                    "correlation_id": "dispatch:dispatch-1",
                    "semantic_event_id": "dispatch:dispatch-1:notify",
                    "operation": "send",
                    "target_kind": "channel",
                    "target_channel_id": "1500000000000000000",
                    "target_thread_id": null,
                    "status": "sent",
                    "attempt": 1,
                    "message_id": "1500000000000000001",
                    "messages_json": [{"channel_id": "1500000000000000000", "message_id": "1500000000000000001"}],
                    "fallback_kind": null,
                    "error": null,
                    "result_json": {"status": "success"},
                    "reserved_until": null,
                    "created_at": "2026-05-06T08:00:00Z",
                    "updated_at": "2026-05-06T08:00:01Z"
                }]
            }),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "dispatch-ghost"}}),
            json!({"error": "dispatch not found"}),
        )
        .with_curl("curl http://localhost:8787/api/dispatches/dispatch-1/events"),
        ep(
            "GET",
            "/api/dispatches/delivery-events/reconcile-stats",
            "dispatches",
            "Read typed dispatch_delivery_events versus kv_meta delivery guard reconciliation stats. Compares dispatch_reserving and dispatch_notified guard keys with latest typed reserved/sent rows, returns mismatch samples, and exposes the cumulative agentdesk_dispatch_delivery_event_mismatch_total metric by kind.",
        )
        .with_example(
            json!({}),
            json!({
                "stats": {
                    "kv_reserving_checked": 1,
                    "kv_notified_checked": 2,
                    "typed_events_checked": 3,
                    "mismatch_count": 1,
                    "missing_typed": 0,
                    "notified_status_mismatch": 1,
                    "missing_kv_meta": 0
                },
                "mismatches": [{
                    "dispatch_id": "dispatch-1",
                    "kind": "notified_status_mismatch",
                    "expected_status": "sent",
                    "actual_status": "reserved"
                }],
                "metrics": [{
                    "name": "agentdesk_dispatch_delivery_event_mismatch_total",
                    "kind": "notified_status_mismatch",
                    "value": 1
                }]
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres pool unavailable"}),
        )
        .with_curl("curl http://localhost:8787/api/dispatches/delivery-events/reconcile-stats"),
        ep(
            "PATCH",
            "/api/dispatches/{id}",
            "dispatches",
            "Update dispatch lifecycle state or result. Allowed status values are pending, dispatched, completed, cancelled, and failed. status=completed uses the dispatch completion finalizer and is allowed only from pending or dispatched; already-terminal dispatches return 409 instead of a silent no-op. Review-type dispatches (`dispatch_type=review`) require an explicit `verdict` or `decision` string inside `result` at completion or the request fails 400 — the canonical path for review completion is POST /api/reviews/verdict. The response `result_summary` is derived in priority from these result/context keys: `summary`, `work_summary`, `result_summary`, `task_summary`, `completion_summary`, `message`, `final_message`, `decision`, `comment`, `verdict`, `reason`, `completion_source`, `work_outcome`, `noop_reason`, `pm_decision`, `notes`, `content`. allowed_from is a status precondition; when the dispatch exists but its current status is outside this set, the request returns 409. Non-completed status changes and result-only updates refresh updated_at. Completed responses include result_summary and completed_at; legacy completed rows without completed_at mirror updated_at in the response.",
        )
        .with_params([
            ("id", path_param("Dispatch ID")),
            (
                "status",
                body_param("string", false, "New dispatch status").with_enum(&[
                    "pending",
                    "dispatched",
                    "completed",
                    "cancelled",
                    "failed",
                ]),
            ),
            (
                "result",
                body_param(
                    "object",
                    false,
                    "Structured dispatch result payload; response derives result_summary from result/context",
                ),
            ),
            (
                "allowed_from",
                body_param(
                    "array<string>",
                    false,
                    "Optional status precondition; when the dispatch exists but its current status is outside this set, the request returns 409 and does not mutate the dispatch",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "dispatch-1"}, "body": {"status": "completed", "result": {"summary": "done"}}}),
            json!({"dispatch": {"id": "dispatch-1", "status": "completed", "result": {"summary": "done"}, "result_summary": "done", "updated_at": "2026-05-03 01:23:45+00", "completed_at": "2026-05-03 01:23:45+00"}}),
        )
        .with_error_example(
            409,
            json!({"path": {"id": "dispatch-1"}, "body": {"status": "completed"}}),
            json!({"error": "dispatch dispatch-1 is in status 'completed' and cannot be completed; completion is allowed only from pending or dispatched", "dispatch_id": "dispatch-1"}),
        )
        .with_curl("curl -X PATCH http://localhost:8787/api/dispatches/dispatch-1 -H 'Content-Type: application/json' -d '{\"status\":\"completed\"}'"),
        ep(
            "POST",
            "/api/internal/link-dispatch-thread",
            "internal",
            "Link dispatch to an existing Discord thread // TODO: example",
        ),
        ep(
            "GET",
            "/api/internal/card-thread",
            "internal",
            "Resolve thread metadata for a card // TODO: example",
        ),
        ep(
            "GET",
            "/api/internal/pending-dispatch-for-thread",
            "internal",
            "Find pending dispatch bound to a thread // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/stages",
            "pipeline",
            "List pipeline stages // TODO: example",
        ),
        ep(
            "PUT",
            "/api/pipeline/stages",
            "pipeline",
            "Replace all pipeline stages // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/pipeline/stages",
            "pipeline",
            "Delete pipeline stages // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/cards/{card_id}",
            "pipeline",
            "Get card pipeline state",
        )
        .with_params([("card_id", path_param("Kanban card ID"))])
        .with_example(
            json!({"path": {"card_id": "card-1"}}),
            json!({"card_id": "card-1", "status": "ready", "stage": "implementation", "review_status": null}),
        )
        .with_error_example(
            404,
            json!({"path": {"card_id": "ghost-card"}}),
            json!({"error": "card not found: ghost-card"}),
        )
        .with_curl("curl http://localhost:8787/api/pipeline/cards/card-1"),
        ep(
            "GET",
            "/api/pipeline/cards/{card_id}/history",
            "pipeline",
            "Get card transition history // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/cards/{card_id}/transcripts",
            "pipeline",
            "List completed turn transcripts linked to card dispatches // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/config/default",
            "pipeline",
            "Get default pipeline config // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/config/effective",
            "pipeline",
            "Get effective merged pipeline config // TODO: example",
        )
        .with_params([
            (
                "repo",
                query_param("string", false, "Repository full name for config resolution"),
            ),
            (
                "agent_id",
                query_param("string", false, "Agent id for config resolution"),
            ),
        ]),
        ep(
            "GET",
            "/api/pipeline/config/repo/{owner}/{repo}",
            "pipeline",
            "Get repo pipeline override // TODO: example",
        ),
        ep(
            "PUT",
            "/api/pipeline/config/repo/{owner}/{repo}",
            "pipeline",
            "Set repo pipeline override // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/config/agent/{agent_id}",
            "pipeline",
            "Get agent pipeline override // TODO: example",
        ),
        ep(
            "PUT",
            "/api/pipeline/config/agent/{agent_id}",
            "pipeline",
            "Set agent pipeline override // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/config/graph",
            "pipeline",
            "Get pipeline graph // TODO: example",
        )
        .with_params([
            (
                "repo",
                query_param("string", false, "Repository full name for config resolution"),
            ),
            (
                "agent_id",
                query_param("string", false, "Agent id for config resolution"),
            ),
        ]),
        ep("GET", "/api/github/repos", "github", "List GitHub repos // TODO: example")
    ]
}
