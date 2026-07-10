use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "POST",
            "/api/queue/dispatch-next",
            "auto-queue",
            "Dispatch the next pending auto-queue entries. See /api/docs/card-lifecycle-ops for the full decision tree on when to call /generate vs /dispatch-next (#1443).",
        )
        .with_params([
            (
                "run_id",
                body_param("string", false, "Specific auto-queue run to activate"),
            ),
            (
                "repo",
                body_param("string", false, "Restrict activation to matching repo"),
            ),
            (
                "agent_id",
                body_param("string", false, "Restrict activation to matching agent"),
            ),
            (
                "thread_group",
                body_param("number", false, "Limit activation to a single thread group"),
            ),
            (
                "unified_thread",
                body_param(
                    "boolean",
                    false,
                    "Accepted for compatibility but ignored; slot pooling stays enabled",
                )
                .with_default(false),
            ),
            (
                "active_only",
                body_param(
                    "boolean",
                    false,
                    "Internal recovery mode; do not promote generated/pending runs",
                )
                .with_default(false),
            ),
        ])
        .with_example(
            json!({"body": {"repo": "test-repo", "unified_thread": false}}),
            json!({"dispatched": [{"id": "entry-1", "card_id": "card-423", "dispatch_id": "dispatch-1", "status": "dispatched"}], "count": 1, "active_groups": 1, "pending_groups": 1}),
        )
        .with_error_example(
            404,
            json!({"body": {"run_id": "run-ghost"}}),
            json!({"error": "auto-queue run not found: run-ghost"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/dispatch-next -H 'Content-Type: application/json' -d '{\"repo\":\"test-repo\"}'"),
        ep(
            "GET",
            "/api/queue/status",
            "auto-queue",
            "Get latest auto-queue run state. diagnostics.entry_dispatch_delivery_mismatches surfaces split-brain delivery where an entry is dispatched but the linked dispatch/session is not live; it includes run_id, entry_id, dispatch_id, card_id, github_issue_number, thread_group, slot_index, dispatch_status, entry_status, age_ms, and recovery endpoints. diagnostics.run_timeout_overruns reports active runs beyond timeout_minutes. When auto_queue_slot_single_active_entry is violated, diagnostics.slot_invariant_violations identifies run_id, agent_id, slot_index, conflicting entry_ids, related dispatch_ids, and recovery endpoints. Recommended recovery: reset stale entries to pending, release slot bindings with /api/queue/slots/{agent_id}/{slot_index}/reset-thread or /api/queue/slots/{agent_id}/{slot_index}/rebind, then dispatch again.",
        )
        .with_params([
            (
                "repo",
                query_param(
                    "string",
                    false,
                    "Restrict status view to a repository id; use agent_id for agent filters",
                ),
            ),
            (
                "agent_id",
                query_param("string", false, "Restrict status view to an agent"),
            ),
        ])
        .with_example(
            json!({"query": {"repo": "test-repo"}}),
            json!({"run": {"id": "run-1", "status": "active", "review_mode": "enabled"}, "entries": [{"id": "entry-1", "status": "pending", "github_issue_number": 423}], "agents": {"agent-1": {"pending": 1, "dispatched": 0, "done": 0, "skipped": 0}}, "thread_groups": {"0": {"status": "pending", "pending": 1, "dispatched": 0, "done": 0, "skipped": 0, "entries": [{"id": "entry-1", "card_id": "card-423", "status": "pending", "github_issue_number": 423, "batch_phase": 0}]}}}),
        )
        .with_error_example(
            404,
            json!({"query": {"repo": "no-such-repo"}}),
            json!({"error": "no auto-queue run for repo: no-such-repo"}),
        )
        .with_curl("curl 'http://localhost:8787/api/queue/status?repo=test-repo'"),
        ep(
            "GET",
            "/api/queue/history",
            "auto-queue",
            "List recent auto-queue runs with outcome metrics",
        )
        .with_params([
            (
                "repo",
                query_param("string", false, "Restrict history view to a repo"),
            ),
            (
                "agent_id",
                query_param("string", false, "Restrict history view to an agent"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum number of recent runs to return")
                    .with_default(8),
            ),
        ])
        .with_example(
            json!({"query": {"repo": "test-repo", "limit": 5}}),
            json!({
                "summary": {
                    "total_runs": 2,
                    "completed_runs": 1,
                    "success_rate": 0.375,
                    "failure_rate": 0.625
                },
                "runs": [{
                    "id": "run-2",
                    "repo": "test-repo",
                    "agent_id": "agent-1",
                    "status": "completed",
                    "timeout_minutes": 120,
                    "timeout_exceeded": false,
                    "timeout_overrun_ms": 0,
                    "created_at": 1712600000000_i64,
                    "completed_at": 1712600300000_i64,
                    "duration_ms": 300000_i64,
                    "entry_count": 4,
                    "done_count": 3,
                    "skipped_count": 1,
                    "pending_count": 0,
                    "dispatched_count": 0,
                    "success_rate": 0.75,
                    "failure_rate": 0.25
                }]
            }),
        ),
        ep(
            "PATCH",
            "/api/queue/entries/{id}",
            "auto-queue",
            "Update one auto-queue entry or reconcile a failed terminal entry",
        )
        .with_params([
            ("id", path_param("Auto-queue entry ID")),
            (
                "thread_group",
                body_param("number", false, "Move the entry to another thread group"),
            ),
            (
                "batch_phase",
                body_param("number", false, "Move the entry to another batch phase"),
            ),
            (
                "priority_rank",
                body_param("number", false, "Set the entry's rank within its group"),
            ),
            (
                "status",
                body_param(
                    "string",
                    false,
                    "Manual status update: pending, skipped, or done only when reconciling a failed entry whose card is done and completed_commit evidence exists",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "entry-1"}, "body": {"thread_group": 1, "batch_phase": 2, "priority_rank": 0}}),
            json!({"ok": true, "entry": {"id": "entry-1", "thread_group": 1, "batch_phase": 2, "priority_rank": 0, "status": "pending"}}),
        )
        .with_example(
            json!({"path": {"id": "entry-failed"}, "body": {"status": "done"}}),
            json!({"ok": true, "entry": {"id": "entry-failed", "status": "done", "card_id": "card-1794", "github_issue_number": 1794}}),
        ),
        ep(
            "PATCH",
            "/api/queue/entries/{id}/skip",
            "auto-queue",
            "Skip a pending auto-queue entry",
        )
        .with_params([("id", path_param("Auto-queue entry ID"))])
        .with_example(
            json!({"path": {"id": "entry-1"}}),
            json!({"ok": true}),
        ),
        ep(
            "PATCH",
            "/api/queue/runs/{id}",
            "auto-queue",
            "Update auto-queue run metadata",
        )
        .with_params([
            ("id", path_param("Auto-queue run ID")),
            (
                "status",
                body_param("string", false, "New run status"),
            ),
            (
                "max_concurrent_threads",
                body_param("number", false, "Set the run's concurrency limit"),
            ),
            (
                "unified_thread",
                body_param(
                    "boolean",
                    false,
                    "Accepted for compatibility but ignored",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "run-1"}, "body": {"status": "completed", "max_concurrent_threads": 4, "unified_thread": true}}),
            json!({"ok": true, "ignored": ["unified_thread"]}),
        ),
        ep(
            "PATCH",
            "/api/queue/reorder",
            "auto-queue",
            "Reorder pending auto-queue entries",
        )
        .with_params([
            (
                "ordered_ids",
                body_param("string[]", true, "Ordered entry ids in desired priority order"),
            ),
            (
                "agent_id",
                body_param("string", false, "Optional agent scope for reordering"),
            ),
        ])
        .with_example(
            json!({"body": {"ordered_ids": ["entry-2", "entry-1"], "agent_id": "agent-1"}}),
            json!({"ok": true}),
        )
        .with_error_example(
            400,
            json!({"body": {"agent_id": "agent-1"}}),
            json!({"error": "ordered_ids is required"}),
        )
        .with_curl("curl -X PATCH http://localhost:8787/api/queue/reorder -H 'Content-Type: application/json' -d '{\"ordered_ids\":[\"entry-2\",\"entry-1\"],\"agent_id\":\"agent-1\"}'"),
        ep(
            "POST",
            "/api/queue/slots/{agent_id}/{slot_index}/rebind",
            "auto-queue",
            "Rebind one auto-queue slot to a run/thread group after the active dispatch has been completed, cancelled, or skipped.",
        )
        .with_params([
            ("agent_id", path_param("Agent ID")),
            (
                "slot_index",
                ParamDoc {
                    location: "path",
                    kind: "number",
                    required: true,
                    description: "Slot pool index",
                    enum_values: None,
                    default: None,
                },
            ),
            ("run_id", body_param("string", true, "Active or paused auto-queue run ID")),
            (
                "thread_group",
                body_param("number", true, "Thread group that should own this slot"),
            ),
        ])
        .with_example(
            json!({"path": {"agent_id": "agent-1", "slot_index": 0}, "body": {"run_id": "run-1", "thread_group": 0}}),
            json!({"ok": true, "agent_id": "agent-1", "slot_index": 0, "run_id": "run-1", "thread_group": 0, "rebound": true, "updated_entries": 1}),
        )
        .with_error_example(
            409,
            json!({"path": {"agent_id": "agent-1", "slot_index": 0}, "body": {"run_id": "run-1", "thread_group": 0}}),
            json!({"error": "slot 0 for agent-1 has an active dispatch; reset or complete it before rebind"}),
        ),
        ep(
            "POST",
            "/api/queue/slots/{agent_id}/{slot_index}/reset-thread",
            "auto-queue",
            "Reset a slot-thread binding for an agent",
        )
        .with_params([
            ("agent_id", path_param("Agent ID")),
            (
                "slot_index",
                ParamDoc {
                    location: "path",
                    kind: "number",
                    required: true,
                    description: "Slot pool index",
                    enum_values: None,
                    default: None,
                },
            ),
        ])
        .with_example(
            json!({"path": {"agent_id": "agent-1", "slot_index": 0}}),
            json!({"ok": true, "agent_id": "agent-1", "slot_index": 0, "archived_threads": 1, "cleared_sessions": 1, "cleared_bindings": 1}),
        ),
        ep(
            "POST",
            "/api/queue/reset",
            "auto-queue",
            "Reset one agent queue and clear its queue entries",
        )
        .with_params([("agent_id", body_param("string", true, "Agent ID for the queue reset"))])
        .with_example(
            json!({"body": {"agent_id": "agent-1"}}),
            json!({"ok": true, "deleted_entries": 4, "completed_runs": 1, "protected_active_runs": 0}),
        ),
        ep(
            "POST",
            "/api/queue/reset-global",
            "auto-queue",
            "Reset all queues with an explicit confirmation token",
        )
        .with_params([(
            "confirmation_token",
            body_param(
                "string",
                true,
                "Confirmation token required for global reset",
            ),
        )])
        .with_example(
            json!({"body": {"confirmation_token": "confirm-global-reset"}}),
            json!({"ok": true, "deleted_entries": 4, "completed_runs": 1, "protected_active_runs": 0}),
        ),
        ep(
            "POST",
            "/api/queue/pause",
            "auto-queue",
            "Soft-pause active runs",
        )
        .with_params([(
            "force",
            body_param(
                "boolean",
                false,
                "Cancel live dispatches and release tmux slots before pausing",
            )
            .with_default(false),
        )])
        .with_example(
            json!({}),
            json!({
                "ok": true,
                "paused_runs": 1,
                "cancelled_dispatches": 0,
                "released_slots": 0,
                "cleared_slot_sessions": 0
            }),
        )
        .with_error_example(
            409,
            json!({}),
            json!({"error": "no active runs to pause"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/pause -H 'Content-Type: application/json' -d '{}'"),
        ep(
            "POST",
            "/api/queue/resume",
            "auto-queue",
            "Resume paused runs and dispatch next entries. Runs blocked by pending/failed phase-gate rows remain paused and return blocked_runs with message='No resumable runs'.",
        )
        .with_example(json!({}), json!({"ok": true, "resumed_runs": 1, "blocked_runs": 0, "dispatched": 1}))
        .with_error_example(
            200,
            json!({}),
            json!({"ok": true, "resumed_runs": 0, "blocked_runs": 1, "message": "No resumable runs"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/resume"),
        ep(
            "POST",
            "/api/queue/runs/{id}/phase-gates/repair",
            "auto-queue",
            "Operator repair endpoint. Re-evaluate terminal phase-gate dispatch results for a paused run, including gates already marked failed. When server.auth_token or kanban.manager_channel_id is configured, the matching Bearer token/channel header is required; unconfigured local installs allow the repair path so operators are not blocked during incident recovery. Accepts Idempotency-Key for replay-safe double clicks. Use this before /api/queue/resume when blocked_runs indicates a pending/failed phase gate and the dispatch result has been repaired or persisted late.",
        )
        .with_params([
            ("id", path_param("Auto-queue run ID")),
            (
                "phase",
                body_param("number", false, "Restrict repair to one batch phase"),
            ),
            (
                "dispatch_id",
                body_param(
                    "string",
                    false,
                    "Restrict repair to one terminal phase-gate dispatch",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "run-1"}, "body": {"phase": 1}}),
            json!({
                "ok": true,
                "run_id": "run-1",
                "phase_filter": 1,
                "dispatch_id_filter": null,
                "candidate_dispatches": 1,
                "cleared_gates": 1,
                "failed_gates": 0,
                "orphan_gates_skipped": 0,
                "blocking_gates_remaining": 0,
                "run_status": "active",
                "outcomes": [{"dispatch_id": "dispatch-gate-1", "phase": 1, "outcome": "cleared", "run_resumed": true, "run_finalized": false}]
            }),
        )
        .with_error_example(
            401,
            json!({"path": {"id": "run-1"}}),
            json!({"error": "phase-gate repair requires explicit Bearer token"}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "run-ghost"}}),
            json!({"error": "auto-queue run not found: run-ghost"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/runs/run-1/phase-gates/repair -H 'Authorization: Bearer <token>' -H 'Idempotency-Key: <uuid>' -H 'Content-Type: application/json' -d '{\"phase\":1}'"),
        ep(
            "POST",
            "/api/queue/runs/{id}/restore",
            "auto-queue",
            "Restore a cancelled or restoring run by re-evaluating skipped entries. Paused runs are rejected; use /api/queue/resume unless the run is cancelled/restoring.",
        )
        .with_params([("id", path_param("Auto-queue run ID"))])
        .with_example(
            json!({"path": {"id": "run-1"}}),
            json!({
                "ok": true,
                "run_id": "run-1",
                "run_status": "active",
                "restored_pending": 1,
                "restored_done": 0,
                "restored_dispatched": 0,
                "rebound_slots": 0,
                "created_dispatches": 0,
                "unbound_dispatches": 0
            }),
        )
        .with_error_example(
            400,
            json!({"path": {"id": "run-1"}}),
            json!({"error": "only cancelled or restoring runs can be restored (status=paused)", "run_id": "run-1", "status": "paused"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/runs/run-1/restore"),
        ep(
            "POST",
            "/api/queue/cancel",
            "auto-queue",
            "Cancel active or paused runs and skip pending entries",
        )
        .with_example(
            json!({}),
            json!({"ok": true, "cancelled_entries": 3, "cancelled_runs": 1}),
        )
        .with_error_example(
            409,
            json!({}),
            json!({"error": "no cancellable runs"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/cancel -H 'Content-Type: application/json' -d '{}'"),
        ep(
            "POST",
            "/api/queue/runs/{id}/order",
            "auto-queue",
            "Submit ordered cards for a pending run",
        )
        .with_params([
            ("id", path_param("Auto-queue run ID")),
            (
                "order",
                body_param(
                    "number[]|string[]",
                    true,
                    "Ordered GitHub issue numbers or card ids",
                ),
            ),
            (
                "rationale",
                body_param("string", false, "Ordering rationale for this run"),
            ),
            (
                "reasoning",
                body_param("string", false, "Alias for rationale"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "run-1"}, "body": {"order": [423, 405], "rationale": "dependency-first"}}),
            json!({"ok": true, "created": 2, "run_id": "run-1", "message": "Queue active. Call POST /api/queue/dispatch-next to start dispatching."}),
        ),
        ep(
            "GET",
            "/api/queue/phase-gates/catalog",
            "auto-queue",
            "User-facing phase-gate kind catalog (#2125). Dashboard and agents share a single vocabulary for `phase_gate_kind` values passed to /api/queue/generate entries. Each kind exposes id, label (ko/en), description, and the underlying internal checks it implies. `default_kind` is applied when an entry omits phase_gate_kind.",
        )
        .with_example(
            json!({}),
            json!({
                "kinds": [
                    {
                        "id": "pr-confirm",
                        "label": {"ko": "PR 확인", "en": "PR Verify"},
                        "description": "PR 머지 및 이슈 종료 확인 후 다음 페이즈 진행",
                        "checks": ["merge_verified", "issue_closed"],
                    },
                    {
                        "id": "deploy-gate",
                        "label": {"ko": "배포 게이트", "en": "Deploy Gate"},
                        "description": "스테이지 빌드/배포 통과 후 다음 페이즈 진행",
                        "checks": ["build_passed", "deploy_verified"],
                    },
                ],
                "default_kind": "pr-confirm",
            }),
        )
        .with_curl("curl http://localhost:8787/api/queue/phase-gates/catalog"),
        ep(
            "POST",
            "/api/queue/request-generate",
            "auto-queue",
            "Dashboard-facing: send a standardized self-contained instruction to the agent's Discord channel asking it to call /api/queue/generate for the given issues (#2126). Backend owns both the instruction text and channel routing so the dashboard stays decoupled from prompt evolution. Returns 202 with request_id, target, channel_id, dispatched_at, and instruction_preview.",
        )
        .with_params([
            (
                "repo",
                body_param("string", true, "GitHub repository (owner/name)"),
            ),
            (
                "agent_id",
                body_param("string", true, "Target agent role_id (Discord channel target is `agent:<id>`)"),
            ),
            (
                "issue_numbers",
                body_param(
                    "number[]",
                    true,
                    "GitHub issue numbers the agent should consider for the queue",
                ),
            ),
            (
                "allowed_gate_kinds",
                body_param(
                    "string[]",
                    false,
                    "Restrict phase_gate_kind choices to this subset of GET /api/queue/phase-gates/catalog ids",
                ),
            ),
            (
                "force",
                body_param(
                    "boolean",
                    false,
                    "Reserved for future force-cancel semantics; accepted but currently has no effect and is not echoed in the response",
                ),
            ),
        ])
        .with_example(
            json!({"body": {"repo": "itismyfield/AgentDesk", "agent_id": "project-agentdesk", "issue_numbers": [2120, 2121, 2122], "allowed_gate_kinds": ["pr-confirm", "deploy-gate"]}}),
            json!({
                "request_id": "req-uuid",
                "target": "agent:project-agentdesk",
                "channel_id": "1490141485167808532",
                "dispatched_at": "2026-05-14T12:30:00+00:00",
                "instruction_preview": "[자동큐 생성 의뢰] (request_id: req-uuid)…"
            }),
        )
        .with_error_example(
            400,
            json!({"body": {"repo": "x", "agent_id": "a", "issue_numbers": [1], "allowed_gate_kinds": ["ship-it"]}}),
            json!({"error": "unknown phase_gate_kind 'ship-it' (see GET /api/queue/phase-gates/catalog)"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/request-generate -H 'Content-Type: application/json' -d '{\"repo\":\"itismyfield/AgentDesk\",\"agent_id\":\"project-agentdesk\",\"issue_numbers\":[2120]}'"),
        ep(
            "GET",
            "/api/channels/{id}/queue",
            "queue",
            "List queue entries for a Discord channel",
        )
        .with_example(
            json!({"path": {"id": "1473922824350601297"}}),
            json!({"channel_id": "1473922824350601297", "dispatches": [{"dispatch_id": "dispatch-1", "status": "pending", "title": "Implement feature"}]}),
        ),
        ep(
            "GET",
            "/api/channels/{id}/watcher-state",
            "monitoring",
            "Read-only snapshot of the tmux-watcher lifecycle state for a channel. \
             Core fields (#964): provider, attached, tmux_session, last_relay_offset, \
             inflight_state_present, last_relay_ts_ms, last_capture_offset, unread_bytes, \
             desynced (orphan/cross-owner/stale capture divergence, 30s threshold), \
             reconnect_count, has_pending_queue. \
             #1133 enriched diagnostics (omitted when source is absent): \
             inflight_started_at, inflight_updated_at, inflight_user_msg_id, \
             inflight_current_msg_id, watcher_owner_channel_id, tmux_session_alive \
             (PID check via `tmux has-session`), mailbox_active_user_msg_id. \
             #4408 phase-2 (I1, omitted when unknown): bound_output_path and \
             bound_session_id expose the transcript path / provider session the \
             relay tail is actually bound to (inflight `output_path`/`session_id` \
             first, else the in-memory tmux runtime binding's relay path), so an \
             out-of-band watchdog can compare the server's asserted selector \
             against its own growth-aware transcript pick. Returns \
             404 when no watcher / inflight / mailbox engagement exists for the channel.",
        )
        .with_params([("id", path_param("Discord channel ID (numeric)"))])
        .with_example(
            json!({"path": {"id": "523456789012345678"}}),
            json!({
                "provider": "codex",
                "attached": true,
                "tmux_session": "agentdesk-codex-channel-523456789012345678",
                "watcher_owner_channel_id": 523456789012345678_u64,
                "last_relay_offset": 2048,
                "inflight_state_present": true,
                "last_relay_ts_ms": 1_761_369_600_000_i64,
                "last_capture_offset": 4096,
                "unread_bytes": 2048,
                "desynced": false,
                "reconnect_count": 1,
                "inflight_started_at": "2026-04-25 03:00:00",
                "inflight_updated_at": "2026-04-25 03:00:42",
                "inflight_user_msg_id": 9001,
                "inflight_current_msg_id": 9002,
                "tmux_session_alive": true,
                "has_pending_queue": false,
                "mailbox_active_user_msg_id": 9001,
                "bound_output_path": "/tmp/rollout.jsonl",
                "bound_session_id": "session-1",
            }),
        ),
        ep(
            "GET",
            "/api/dispatches/pending",
            "queue",
            "List pending dispatches",
        )
        .with_example(
            json!({}),
            json!({"dispatches": [{"id": "dispatch-1", "kanban_card_id": "card-1", "to_agent_id": "project-agentdesk", "status": "pending"}], "count": 1}),
        ),
        ep(
            "POST",
            "/api/dispatches/{id}/cancel",
            "dispatches",
            "Cancel a pending or dispatched dispatch, reset linked auto-queue bookkeeping, cancel any matching active turn through the shared turn cancel finalizer, and remove the dispatch notify guard. Terminal dispatches return 409 Conflict.",
        )
        .with_params([("id", path_param("Dispatch ID"))])
        .with_example(
            json!({"path": {"id": "dispatch-1"}}),
            json!({
                "ok": true,
                "dispatch_id": "dispatch-1",
                "active_turn_cancelled": true,
                "turn_status": "cancelled",
                "turn_completed_at": "2026-05-03T01:23:45Z"
            }),
        )
        .with_error_example(
            409,
            json!({"path": {"id": "dispatch-1"}}),
            json!({"error": "dispatch already in terminal state: completed", "code": "dispatch"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/dispatches/dispatch-1/cancel"),
        ep(
            "POST",
            "/api/dispatches/cancel-all",
            "queue",
            "Cancel all queued dispatches",
        )
        .with_example(
            json!({"body": {"kanban_card_id": "card-1", "agent_id": "project-agentdesk"}}),
            json!({"ok": true, "cancelled": 2, "filters": {"kanban_card_id": "card-1", "agent_id": "project-agentdesk"}}),
        )
    ]
}
