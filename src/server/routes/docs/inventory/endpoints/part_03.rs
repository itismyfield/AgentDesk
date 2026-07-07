use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "POST",
            "/api/agents/{id}/turn/start",
            "agents",
            "Start a headless agent turn from the agent primary mailbox/session. Returns conflict when another turn is already active for that agent mailbox.",
        )
        .with_params([
            ("id", path_param("Agent id")),
            (
                "prompt",
                body_param("string", true, "Instruction to execute in the headless turn"),
            ),
            (
                "metadata",
                body_param(
                    "object",
                    false,
                    "Optional trigger metadata injected into the turn context",
                ),
            ),
            (
                "source",
                body_param(
                    "string",
                    false,
                    "Optional trigger source label (for example system or pipeline)",
                ),
            ),
            (
                "dm_user_id",
                body_param(
                    "string",
                    false,
                    "Optional Discord user id. When set, the turn is bound to that user's DM channel with the agent's primary bot.",
                ),
            ),
        ])
        .with_example(
            json!({
                "path": {"id": "family-counsel"},
                "body": {
                    "prompt": "오부장 probe slot 도달. memento recall 후 gap 탐지하고 DM 전송",
                    "metadata": {
                        "trigger_source": "launchd:family-profile-probe",
                        "target_key": "obujang"
                    },
                    "source": "system"
                }
            }),
            json!({
                "ok": true,
                "turn_id": "discord:1473922824350601297:9100000000000000000",
                "status": "started"
            }),
        )
        .with_error_example(
            409,
            json!({"path": {"id": "family-counsel"}, "body": {"prompt": "do it"}}),
            json!({"error": "turn already active for this agent mailbox", "active_turn_id": "discord:1473922824350601297:9000000000000000000"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/family-counsel/turn/start -H 'Content-Type: application/json' -d '{\"prompt\":\"hello\",\"source\":\"system\",\"dm_user_id\":\"343742347365974026\"}'"),
        ep(
            "POST",
            "/api/agents/{id}/turn/stop",
            "agents",
            "Stop the active turn for agent",
        )
        .with_params([("id", path_param("Agent id"))])
        .with_example(
            json!({"path": {"id": "family-counsel"}}),
            json!({"ok": true, "turn_id": "discord:1473922824350601297:9100000000000000000", "status": "stopped"}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "family-counsel"}}),
            json!({"error": "no active turn for agent"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/family-counsel/turn/stop"),
        ep(
            "GET",
            "/api/agents/{id}/transcripts",
            "agents",
            "List recent completed turn transcripts for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/timeline",
            "agents",
            "Agent activity timeline",
        ),
        ep(
            "GET",
            "/api/agents/{id}/quality",
            "agents",
            "Per-agent quality summary (#1102): current + trend_7d + trend_30d from agent_quality_daily with event-based mini-rollup fallback",
        )
        .with_params([
            ("id", path_param("Agent id")),
            (
                "days",
                query_param("integer", false, "Lookback window in days for the daily trend")
                    .with_default(30),
            ),
            (
                "limit",
                query_param("integer", false, "Max daily rows to return").with_default(60),
            ),
        ])
        .with_example(
            json!({"path": {"id": "project-agentdesk"}, "query": {"days": 30}}),
            json!({"agent_id": "project-agentdesk", "current": {"turn_success_rate": 0.92, "sample_size": 50}, "trend_7d": {"turn_success_rate": 0.9}, "trend_30d": {"turn_success_rate": 0.88}}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "ghost"}}),
            json!({"error": "agent not found: ghost"}),
        )
        .with_curl("curl 'http://localhost:8787/api/agents/project-agentdesk/quality?days=30'"),
        ep(
            "GET",
            "/api/agents/quality/ranking",
            "agents",
            "Cross-agent quality ranking (#1102): sorts by the requested metric×window with sample_size >= min_sample_size",
        )
        .with_params([
            (
                "limit",
                query_param("integer", false, "Max agents to return").with_default(50),
            ),
            (
                "metric",
                ParamDoc {
                    location: "query",
                    kind: "string",
                    required: false,
                    description: "Ranking metric",
                    enum_values: None,
                    default: None,
                }
                .with_enum(&["turn_success_rate", "review_pass_rate"])
                .with_default("turn_success_rate"),
            ),
            (
                "window",
                ParamDoc {
                    location: "query",
                    kind: "string",
                    required: false,
                    description: "Rolling window",
                    enum_values: None,
                    default: None,
                }
                .with_enum(&["7d", "30d"])
                .with_default("7d"),
            ),
            (
                "min_sample_size",
                query_param("integer", false, "Exclude agents with window sample_size below this threshold")
                    .with_default(5),
            ),
        ])
        .with_example(
            json!({"query": {"metric": "turn_success_rate", "window": "7d", "limit": 10}}),
            json!({"ranking": [{"agent_id": "project-agentdesk", "turn_success_rate": 0.94, "sample_size": 20}]}),
        )
        .with_error_example(
            400,
            json!({"query": {"metric": "unknown", "window": "7d"}}),
            json!({"error": "metric must be one of turn_success_rate|review_pass_rate"}),
        )
        .with_curl("curl 'http://localhost:8787/api/agents/quality/ranking?metric=turn_success_rate&window=7d&limit=10'"),
        ep("GET", "/api/sessions", "sessions", "List sessions"),
        ep("GET", "/api/policies", "policies", "List policies"),
        ep(
            "GET",
            "/api/auth/session",
            "auth",
            "Get current auth session",
        ),
        ep("GET", "/api/kanban-cards", "kanban", "List kanban cards")
            .with_params([
                (
                    "status",
                    query_param("string", false, "Filter cards by pipeline status"),
                ),
                (
                    "repo_id",
                    query_param("string", false, "Filter cards by repository id"),
                ),
                (
                    "assigned_agent_id",
                    query_param("string", false, "Filter cards by assigned agent id"),
                ),
            ])
            .with_example(
                json!({"query": {"status": "ready"}}),
                json!({"cards": [{"id": "card-1", "title": "Fix docs", "status": "ready", "priority": "high"}]}),
            )
            .with_error_example(
                400,
                json!({"query": {"status": "invalid_status"}}),
                json!({"error": "unknown pipeline status: invalid_status"}),
            )
            .with_curl("curl 'http://localhost:8787/api/kanban-cards?status=ready'"),
        ep("POST", "/api/kanban-cards", "kanban", "Create kanban card")
            .with_params([
                ("title", body_param("string", true, "Card title")),
                (
                    "repo_id",
                    body_param("string", false, "Repository id or full name"),
                ),
                (
                    "priority",
                    body_param("string", false, "Priority label")
                        .with_default("medium"),
                ),
                (
                    "github_issue_url",
                    body_param("string", false, "Linked GitHub issue URL"),
                ),
            ])
            .with_example(
                json!({"body": {"title": "Test Card", "priority": "high"}}),
                json!({"card": {"id": "uuid-card-1", "title": "Test Card", "status": "backlog", "priority": "high"}}),
            )
            .with_error_example(
                400,
                json!({"body": {"priority": "high"}}),
                json!({"error": "title is required"}),
            )
            .with_curl("curl -X POST http://localhost:8787/api/kanban-cards -H 'Content-Type: application/json' -d '{\"title\":\"Test Card\",\"priority\":\"high\"}'"),
        ep(
            "POST",
            "/api/automation-candidates",
            "automation-candidates",
            "Create or upsert an automation candidate card. Cards enter the iteration loop when pipeline_stage_id='automation-candidate' and metadata.program contains repo_dir, allowed_write_paths, metric_name, and metric_target. pipeline_stage_id alone is the discriminator — no extra boolean flags required.",
        )
        .with_params([
            ("title", body_param("string", true, "Candidate card title")),
            ("repo_id", body_param("string", false, "Repository id or full name")),
            ("assigned_agent_id", body_param("string", false, "Agent that should run the loop")),
            ("source", body_param("string", false, "Origin such as user, routine_recommender, memento_digest, or api_friction").with_default("user")),
            ("dedupe_key", body_param("string", false, "Stable candidate identity for idempotent upsert")),
            ("start_ready", body_param("boolean", false, "When true, mark the candidate ready for the automation candidate executor immediately").with_default(false)),
            ("program.repo_dir", body_param("string", true, "Absolute repository path used to create isolated worktrees")),
            ("program.allowed_write_paths", body_param("array<string>", true, "Non-empty clean relative path allowlist")),
            ("program.metric_name", body_param("string", true, "Metric name measured by the iteration")),
            ("program.metric_target", body_param("number", true, "Target metric value")),
            ("program.metric_direction", body_param("string", false, "lower_is_better or higher_is_better").with_default("lower_is_better")),
            ("program.final_gate", body_param("string", false, "manual_review or auto_apply_after_green").with_default("manual_review")),
            ("program.iteration_budget", body_param("integer", false, "Maximum iteration count, clamped to 1..10").with_default(3)),
        ])
        .with_example(
            json!({"body": {
                "title": "Reduce repeated Discord routing failures",
                "source": "routine_recommender",
                "dedupe_key": "api-friction:discord-routing-timeout",
                "start_ready": false,
                "program": {
                    "repo_dir": "/Users/kunkun/kunkunGames/agentdesk",
                    "allowed_write_paths": ["src/services/discord", "src/server/routes/discord.rs"],
                    "metric_name": "routing_failure_rate",
                    "metric_target": 0.0,
                    "metric_direction": "lower_is_better",
                    "final_gate": "manual_review",
                    "iteration_budget": 3
                }
            }}),
            json!({
                "card_id": "uuid-card-1",
                "created": true,
                "status": "backlog",
                "pipeline_stage_id": "automation-candidate",
                "discriminator": {
                    "pipeline_stage_id": "automation-candidate",
                    "required_program_fields": ["repo_dir", "allowed_write_paths", "metric_name", "metric_target"]
                }
            }),
        )
        .with_error_example(
            400,
            json!({"body": {"title": "x", "program": {"allowed_write_paths": ["../src"]}}}),
            json!({"error": "program.repo_dir is required", "code": "MISSING_PROGRAM_CONTRACT"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/automation-candidates -H 'Content-Type: application/json' -d '{\"title\":\"Reduce repeated routing failures\",\"dedupe_key\":\"api-friction:routing\",\"program\":{\"repo_dir\":\"/Users/kunkun/kunkunGames/agentdesk\",\"allowed_write_paths\":[\"src/services/discord\"],\"metric_name\":\"routing_failure_rate\",\"metric_target\":0,\"metric_direction\":\"lower_is_better\"}}'"),
        ep(
            "GET",
            "/api/kanban-cards/stalled",
            "kanban",
            "List stalled cards",
        ),
        ep(
            "POST",
            "/api/kanban-cards/assign-issue",
            "kanban",
            "Create or update a card from a GitHub issue. Assignment is guaranteed once the request succeeds; transition to a dispatchable state is best-effort and must be checked in response.transition.",
        )
        .with_params([
            (
                "github_repo",
                body_param("string", true, "Repository full name"),
            ),
            (
                "github_issue_number",
                body_param("number", true, "GitHub issue number"),
            ),
            (
                "github_issue_url",
                body_param("string", false, "Linked GitHub issue URL"),
            ),
            ("title", body_param("string", true, "Card title")),
            (
                "description",
                body_param("string", false, "Card description override"),
            ),
            (
                "assignee_agent_id",
                body_param("string", true, "Agent that should own the card"),
            ),
        ])
        .with_example(
            json!({"body": {"github_repo": "itismyfield/AgentDesk", "github_issue_number": 426, "title": "Improve docs", "assignee_agent_id": "project-agentdesk"}}),
            json!({
                "card": {"id": "card-426", "status": "requested", "github_issue_number": 426, "assigned_agent_id": "project-agentdesk"},
                "deduplicated": false,
                "assignment": {"ok": true, "agent_id": "project-agentdesk"},
                "transition": {"attempted": true, "ok": true, "from": "backlog", "to": "requested", "target": "requested", "target_status": "requested", "error": null, "next_action": "none_required"}
            }),
        )
        .with_partial_success_example(
            json!({"body": {"github_repo": "itismyfield/AgentDesk", "github_issue_number": 427, "title": "Already done issue", "assignee_agent_id": "project-agentdesk"}}),
            json!({
                "card": {"id": "card-427", "status": "done", "github_issue_number": 427, "assigned_agent_id": "project-agentdesk"},
                "deduplicated": true,
                "assignment": {"ok": true, "agent_id": "project-agentdesk"},
                "transition": {
                    "attempted": true,
                    "ok": false,
                    "from": "done",
                    "to": "done",
                    "target": "requested",
                    "target_status": "requested",
                    "steps": ["requested"],
                    "completed_steps": [],
                    "failed_step": "requested",
                    "error": "transition done -> requested is not allowed",
                    "next_action": "inspect_transition_error"
                }
            }),
        )
        .with_error_example(
            404,
            json!({"body": {"github_repo": "unknown/repo", "github_issue_number": 1, "title": "x", "assignee_agent_id": "ghost"}}),
            json!({"error": "assignee agent not found: ghost"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/assign-issue -H 'Content-Type: application/json' -d '{\"github_repo\":\"itismyfield/AgentDesk\",\"github_issue_number\":426,\"title\":\"Improve docs\",\"assignee_agent_id\":\"project-agentdesk\"}'"),
        ep("GET", "/api/kanban-cards/{id}", "kanban", "Get card by ID")
            .with_params([("id", path_param("Kanban card ID"))])
            .with_example(
                json!({"path": {"id": "card-1"}}),
                json!({"card": {"id": "card-1", "title": "Fix docs", "status": "ready"}}),
            )
            .with_error_example(
                404,
                json!({"path": {"id": "ghost-card"}}),
                json!({"error": "card not found: ghost-card"}),
            )
            .with_curl("curl http://localhost:8787/api/kanban-cards/card-1"),
        ep(
            "PATCH",
            "/api/kanban-cards/{id}",
            "kanban",
            "Update card fields, or perform one manual status edit. Status edits cannot be combined with metadata or other field updates. Status edits are limited to backlog -> ready and any -> backlog; use /transition for administrative force transitions and /rereview for review reruns.",
        )
            .with_params([
                ("id", path_param("Kanban card ID")),
                ("title", body_param("string", false, "Updated title")),
                (
                    "status",
                    body_param(
                        "string",
                        false,
                        "Limited manual status edit: backlog -> ready or any -> backlog only; do not combine with other fields",
                    ),
                ),
                (
                    "priority",
                    body_param("string", false, "Priority label"),
                ),
                (
                    "assigned_agent_id",
                    body_param("string", false, "Assigned agent id"),
                ),
                (
                    "assignee_agent_id",
                    body_param("string", false, "Alias for assigned_agent_id"),
                ),
                (
                    "repo_id",
                    body_param("string", false, "Repository id or full name"),
                ),
                (
                    "github_issue_url",
                    body_param("string", false, "Linked GitHub issue URL"),
                ),
                (
                    "metadata",
                    body_param("object", false, "Metadata object"),
                ),
                (
                    "description",
                    body_param("string", false, "Card description"),
                ),
                (
                    "metadata_json",
                    body_param("string", false, "Raw metadata JSON string"),
                ),
                (
                    "review_status",
                    body_param("string", false, "Review status override"),
                ),
                (
                    "review_notes",
                    body_param("string", false, "Review notes"),
                ),
            ])
            .with_example(
                json!({"path": {"id": "card-1"}, "body": {"priority": "high"}}),
                json!({"card": {"id": "card-1", "status": "backlog", "priority": "high"}}),
            )
            .with_error_example(
                400,
                json!({"path": {"id": "card-1"}, "body": {"status": "done"}}),
                json!({"error": "PATCH /api/kanban-cards/{id} only allows manual status transitions backlog -> ready and any -> backlog (requested: review -> done). Use POST /api/kanban-cards/{id}/transition for administrative force transitions, or POST /api/kanban-cards/{id}/rereview for review reruns."}),
            )
            .with_error_example(
                400,
                json!({"path": {"id": "card-1"}, "body": {"status": "ready", "metadata_json": "{\"x\":true}"}}),
                json!({"error": "PATCH /api/kanban-cards/{id} cannot combine status changes with metadata or other field updates. Send metadata/field updates in one request, then send a status-only PATCH request, or use POST /api/kanban-cards/{id}/transition for administrative force transitions."}),
            )
            .with_curl("curl -X PATCH http://localhost:8787/api/kanban-cards/card-1 -H 'Content-Type: application/json' -d '{\"priority\":\"high\"}'"),
        ep(
            "DELETE",
            "/api/kanban-cards/{id}",
            "kanban",
            "Delete card",
        )
        .with_params([("id", path_param("Kanban card ID"))])
        .with_example(
            json!({"path": {"id": "card-1"}}),
            json!({"ok": true}),
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/assign",
            "kanban",
            "Assign card to agent. Assignment is guaranteed once the request succeeds; transition to a dispatchable state is best-effort and failures are reported in response.transition.",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            ("agent_id", body_param("string", true, "Agent ID")),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"agent_id": "ch-td"}}),
            json!({
                "card": {"id": "card-1", "assigned_agent_id": "ch-td", "status": "requested"},
                "assignment": {"ok": true, "agent_id": "ch-td"},
                "transition": {"attempted": true, "ok": true, "from": "backlog", "to": "requested", "target": "requested", "target_status": "requested", "error": null, "next_action": "none_required"}
            }),
        )
        .with_partial_success_example(
            json!({"path": {"id": "card-2"}, "body": {"agent_id": "ch-td"}}),
            json!({
                "card": {"id": "card-2", "assigned_agent_id": "ch-td", "status": "done"},
                "assignment": {"ok": true, "agent_id": "ch-td"},
                "transition": {
                    "attempted": true,
                    "ok": false,
                    "from": "done",
                    "to": "done",
                    "target": "requested",
                    "target_status": "requested",
                    "steps": ["requested"],
                    "completed_steps": [],
                    "failed_step": "requested",
                    "error": "transition done -> requested is not allowed",
                    "next_action": "inspect_transition_error"
                }
            }),
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/rereview",
            "kanban",
            "Force a card back through review and create or reuse a fresh review dispatch. Use this instead of PATCH status=review for review reruns; requires explicit Bearer auth.",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "reason",
                body_param("string", false, "Why the rereview is needed"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"reason": "repeated finding"}}),
            json!({"card": {"id": "card-1", "status": "review"}, "rereviewed": true, "review_dispatch_id": "dispatch-review-1", "reason": "repeated finding"}),
        ),
        ep(
            "POST",
            "/api/kanban-cards/batch-rereview",
            "kanban",
            "Batch rereview by GitHub issue number",
        )
        .with_params([
            (
                "issues",
                body_param("number[]", true, "GitHub issue numbers to rereview"),
            ),
            (
                "reason",
                body_param("string", false, "Shared rereview reason"),
            ),
        ])
        .with_example(
            json!({"body": {"issues": [423, 426], "reason": "counter-model retry"}}),
            json!({"results": [{"issue": 423, "ok": true, "dispatch_id": "dispatch-review-423"}, {"issue": 426, "ok": false, "error": "card not found for issue #426"}]}),
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/reopen",
            "kanban",
            "Reopen a terminal card: move a closed/done card back to an active pipeline state (ready). Distinct from /retry (same failed step), /redispatch (new dispatch id), and /resume (continue checkpoint); reopen re-admits a terminal card into the board.",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "review_status",
                body_param("string", false, "Optional review status to set after reopen"),
            ),
            (
                "dispatch_type",
                body_param("string", false, "Reserved dispatch type override"),
            ),
            ("reason", body_param("string", false, "Audit reason")),
            (
                "reset_full",
                body_param(
                    "boolean",
                    false,
                    "Clear recovery/review state and cancel stale work",
                )
                .with_default(false),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"reason": "manual reopen", "reset_full": true}}),
            json!({"card": {"id": "card-1", "status": "ready"}, "reopened": true, "reset_full": true, "cancelled_dispatches": 1, "skipped_auto_queue_entries": 1, "from": "done", "to": "ready", "reason": "manual reopen"}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "does-not-exist"}, "body": {}}),
            json!({"error": "card not found: does-not-exist"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/reopen -H 'Content-Type: application/json' -d '{\"reason\":\"manual reopen\",\"reset_full\":true}'"),
        ep(
            "POST",
            "/api/kanban-cards/{id}/transition",
            "kanban",
            "Transition a single card with administrative force-transition semantics. Canonical runtime path is /transition; both the old /api/kanban-cards/{id}/force-transition path and the bulk /api/kanban-cards/batch-transition endpoint are fully removed (no alias, no redirect) — those paths now return 404/405. Migrate per-card to this endpoint. Auth requirements: (1) Authorization: Bearer <token> when config.server.auth_token is set; (2) X-Channel-Id: <kanban_manager_channel_id> when config.kanban.manager_channel_id is set — missing or mismatched X-Channel-Id returns 401 'force-transition requires PMD channel authorization'. Discover the expected channel id via `agentdesk config get kanban.manager_channel_id` or the /api/agents endpoint. Single-call complete: do NOT chain /redispatch, /retry, or /queue/generate after it (#1442). Inspect cancelled_dispatch_ids, created_dispatch_id, and next_action_hint in the response. See /api/docs/card-lifecycle-ops for the full decision tree (#1443).",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "status",
                body_param(
                    "string",
                    true,
                    "Target pipeline status for the administrative force transition",
                ),
            ),
            (
                "cancel_dispatches",
                body_param(
                    "boolean",
                    false,
                    "When cleanup applies, cancel active dispatches and skip affected auto-queue entries",
                )
                .with_default(true),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"status": "ready", "cancel_dispatches": true}}),
            json!({
                "card": {"id": "card-1", "status": "ready"},
                "forced": true,
                "from": "in_progress",
                "to": "ready",
                "cancelled_dispatches": 1,
                "cancelled_dispatch_ids": ["dispatch-abc"],
                "created_dispatch_id": null,
                "next_action_hint": "call /api/queue/generate to dispatch newly-ready card",
                "skipped_auto_queue_entries": 1
            }),
        )
        .with_error_example(
            400,
            json!({"path": {"id": "card-1"}, "body": {}}),
            json!({"error": "status is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/transition -H 'Content-Type: application/json' -d '{\"status\":\"ready\",\"cancel_dispatches\":true}'"),
        ep("POST", "/api/kanban-cards/{id}/retry", "kanban", "Retry card: re-execute the same failed step with the same intent and context (optionally swapping assignee). Distinct from /redispatch (creates a NEW dispatch id), /resume (continues a checkpointed turn), and /reopen (re-admits a closed card). Single-call complete: do NOT chain /transition or /queue/generate after it (#1442). See /api/docs/card-lifecycle-ops for the full decision tree (#1443).")
            .with_params([
                ("id", path_param("Kanban card ID")),
                (
                    "assignee_agent_id",
                    body_param("string", false, "Override assignee before retry"),
                ),
                (
                    "request_now",
                    body_param("boolean", false, "Legacy compatibility flag"),
                ),
            ])
            .with_example(
                json!({"path": {"id": "card-1"}, "body": {"assignee_agent_id": "agent-review"}}),
                json!({
                    "card": {"id": "card-1", "assigned_agent_id": "agent-review", "latest_dispatch_id": "dispatch-retry-1"},
                    "new_dispatch_id": "dispatch-retry-1",
                    "cancelled_dispatch_id": "dispatch-old-1",
                    "next_action": "none_required"
                }),
            )
            .with_error_example(
                409,
                json!({"path": {"id": "card-1"}, "body": {}}),
                json!({"error": "card has no failed dispatch to retry"}),
            )
            .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/retry -H 'Content-Type: application/json' -d '{\"assignee_agent_id\":\"agent-review\"}'")
    ]
}
