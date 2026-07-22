use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "DELETE",
            "/api/dispatched-sessions/gc-threads",
            "dispatched-sessions",
            "Garbage-collect orphaned thread sessions",
        )
        .with_example(
            json!({}),
            json!({"ok": true, "gc_threads": 2}),
        ),
        ep(
            "PATCH",
            "/api/dispatched-sessions/{id}",
            "dispatched-sessions",
            "Update dispatched session",
        )
        .with_example(
            json!({"path": {"id": 42}, "body": {"status": "working", "active_dispatch_id": "dispatch-1", "model": "gpt-5", "tokens": 1024}}),
            json!({"ok": true}),
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/webhook",
            "dispatched-sessions",
            "Session webhook",
        )
        .with_example(
            json!({"body": {"session_key": "mac-mini:AgentDesk-codex-adk-cdx", "agent_id": "project-agentdesk", "status": "working", "provider": "codex", "dispatch_id": "dispatch-1"}}),
            json!({"ok": true, "session_key": "mac-mini:AgentDesk-codex-adk-cdx"}),
        ),
        ep(
            "DELETE",
            "/api/dispatched-sessions/webhook",
            "dispatched-sessions",
            "Delete session webhook state",
        )
        .with_example(
            json!({"query": {"session_key": "mac-mini:AgentDesk-codex-adk-cdx", "provider": "codex"}}),
            json!({"ok": true, "deleted": true}),
        ),
        ep(
            "GET",
            "/api/dispatched-sessions/claude-session-id",
            "dispatched-sessions",
            "Resolve Claude session id by session key",
        )
        .with_example(
            json!({"query": {"session_key": "mac-mini:AgentDesk-claude-adk-cc", "provider": "claude"}}),
            json!({"claude_session_id": "claude-session-1", "session_id": "claude-session-1", "raw_provider_session_id": "claude-session-1"}),
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/clear-stale-session-id",
            "dispatched-sessions",
            "Clear stale Claude session id",
        )
        .with_example(
            json!({"body": {"session_id": "claude-session-1"}}),
            json!({"cleared": 1}),
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/clear-session-id",
            "dispatched-sessions",
            "Clear Claude session id by session key",
        )
        .with_example(
            json!({"body": {"session_key": "mac-mini:AgentDesk-claude-adk-cc"}}),
            json!({"cleared": 1}),
        ),
        ep(
            "POST",
            "/api/sessions/{session_key}/force-kill",
            "sessions",
            "Force-kill session and optionally retry",
        )
        .with_example(
            json!({"path": {"session_key": "claude/hash123/mac-mini:AgentDesk-claude-adk-cc"}, "body": {"retry": true, "reason": "stalled turn"}}),
            json!({"ok": true, "session_key": "claude/hash123/mac-mini:AgentDesk-claude-adk-cc", "killed": true, "retry_created": true}),
        ),
        ep(
            "POST",
            "/api/sessions/{session_key}/kill-tmux",
            "sessions",
            "Kill only the tmux process for an idle session while preserving the session row and provider resume metadata.",
        )
        .with_params([
            (
                "session_key",
                path_param(
                    "Session key in legacy host:tmux_name or namespaced provider/token/host:tmux_name form",
                ),
            ),
            (
                "reason",
                body_param(
                    "string",
                    false,
                    "Human-readable reason recorded in termination audit.",
                ),
            ),
        ])
        .with_example(
            json!({
                "path": {"session_key": "claude/hash123/mac-mini:AgentDesk-claude-adk-cc"},
                "body": {"reason": "idle 6시간 초과 — 자동 정리"}
            }),
            json!({
                "ok": true,
                "tmux_killed": true,
                "tmux_was_alive": true,
                "tmux_session_name": "AgentDesk-claude-adk-cc",
                "session_row_preserved": true,
                "active_dispatch_id": null
            }),
        )
        .with_error_example(
            404,
            json!({"path": {"session_key": "provider:missing"}}),
            json!({"error": "session not found"}),
        ),
        ep(
            "POST",
            "/api/sessions/{session_key}/resume-previous",
            "sessions",
            "Rebind a channel to a previous provider session (resume its conversation). With session_id (+optional cwd) forces that session; without either, auto-selects the channel workspace's most recent prior session.",
        )
        .with_params([
            (
                "session_key",
                path_param(
                    "Session key in legacy host:tmux_name or namespaced provider/token/host:tmux_name form",
                ),
            ),
            (
                "session_id",
                body_param(
                    "string",
                    false,
                    "Target provider session id to resume. Omit to auto-select the previous session.",
                ),
            ),
            (
                "cwd",
                body_param(
                    "string",
                    false,
                    "Target worktree the resumed session lives in. Defaults to the row's current cwd when session_id is given.",
                ),
            ),
        ])
        .with_example(
            json!({
                "path": {"session_key": "claude/hash123/mac-mini:AgentDesk-claude-adk-cc"},
                "body": {
                    "session_id": "acd0ea18-a5a9-4fa6-a29c-f7034cb06273",
                    "cwd": "/Users/itismyfield/.adk/release/worktrees/claude-adk-cc-20260723-050333"
                }
            }),
            json!({
                "ok": true,
                "session_key": "claude/hash123/mac-mini:AgentDesk-claude-adk-cc",
                "target_session_id": "acd0ea18-a5a9-4fa6-a29c-f7034cb06273",
                "target_cwd": "/Users/itismyfield/.adk/release/worktrees/claude-adk-cc-20260723-050333",
                "previous_session_id": "11111111-1111-1111-1111-111111111111",
                "previous_cwd": "/Users/itismyfield/.adk/release/worktrees/claude-adk-cc-20260723-054531",
                "tmux_killed": true,
                "lifecycle_path": "canonical",
                "in_memory_rebound": true,
                "auto_selected": false
            }),
        )
        .with_error_example(
            409,
            json!({"path": {"session_key": "claude/hash123/mac-mini:AgentDesk-claude-adk-cc"}, "body": {}}),
            json!({"error": "channel has an active turn or dispatch; stop it before resuming a previous session"}),
        )
        .with_error_example(
            404,
            json!({"path": {"session_key": "claude/hash123/mac-mini:AgentDesk-claude-adk-cc"}, "body": {}}),
            json!({"error": "no previous provider session found to resume; pass an explicit session_id"}),
        ),
        ep(
            "GET",
            "/api/sessions/{id}/tmux-output",
            "sessions",
            "Capture recent tmux pane output for a session (watch-agent-turn skill promotion)",
        )
        .with_params([
            ("id", path_param("Session id (sessions.id)")),
            (
                "lines",
                query_param(
                    "integer",
                    false,
                    "Trailing tmux pane lines to capture (1..=2000)",
                )
                .with_default(80),
            ),
        ])
        .with_example(
            json!({"query": {"lines": 40}}),
            json!({
                "session_id": 42,
                "session_key": "mac-mini:remoteCC-claude-foo",
                "tmux_name": "remoteCC-claude-foo",
                "tmux_alive": true,
                "agent_id": "ch-dd",
                "provider": "claude",
                "status": "working",
                "lines_requested": 40,
                "lines_effective": 40,
                "recent_output": "...tail of tmux pane...",
                "captured_at_ms": 1_745_000_000_000_i64
            }),
        ),
        ep(
            "GET",
            "/api/session-termination-events",
            "sessions",
            "List recorded session termination events",
        )
        .with_params([
            (
                "dispatch_id",
                query_param("string", false, "Filter events by task dispatch id"),
            ),
            (
                "card_id",
                query_param("string", false, "Filter events by linked kanban card id"),
            ),
            (
                "session_key",
                query_param("string", false, "Filter events by host-qualified session key"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum events to return (capped at 500)")
                    .with_default(50),
            ),
        ])
        .with_example(
            json!({"query": {"dispatch_id": "dispatch-1", "limit": 1}}),
            json!({
                "events": [{
                    "id": 862,
                    "session_key": "mac-mini:AgentDesk-codex-adk-cdx",
                    "dispatch_id": "dispatch-1",
                    "killer_component": "tmux_watcher",
                    "reason_code": "dead_after_turn",
                    "reason_text": "watcher cleanup: dead session after turn",
                    "probe_snapshot": null,
                    "last_offset": null,
                    "tmux_alive": false,
                    "created_at": "2026-05-16T04:15:48.151Z"
                }]
            }),
        ),
        ep("GET", "/api/messages", "messages", "List messages").with_example(
            json!({"query": {"receiverId": "project-agentdesk", "receiverType": "agent", "limit": 20}}),
            json!({"messages": [{"id": 1, "sender_type": "ceo", "receiver_id": "project-agentdesk", "content": "status?", "message_type": "chat"}]}),
        ),
        ep("POST", "/api/messages", "messages", "Create message").with_example(
            json!({"body": {"sender_type": "ceo", "sender_id": "operator", "receiver_type": "agent", "receiver_id": "project-agentdesk", "content": "Please review the docs", "message_type": "chat"}}),
            json!({"id": 1, "sender_type": "ceo", "receiver_type": "agent", "receiver_id": "project-agentdesk", "content": "Please review the docs", "message_type": "chat"}),
        ),
        ep(
            "GET",
            "/api/discord/bindings",
            "discord",
            "List Discord bindings",
        ),
        ep(
            "GET",
            "/api/discord/channels/{id}/messages",
            "discord",
            "Read recent messages from a Discord channel or thread (proxy to Discord REST v10). The {id} accepts both regular channels and threads. A thread is allowed when either the thread itself or its parent channel is present in the role-map (agentdesk_config / org_schema / role_map.json). Unknown channels still return 403. See src/server/routes/discord.rs::channel_messages.",
        )
        .with_params([
            ("id", path_param("Discord channel or thread ID (snowflake)")),
            (
                "limit",
                query_param("integer", false, "Number of messages to return (1..=100)")
                    .with_default(10),
            ),
            (
                "before",
                query_param(
                    "string",
                    false,
                    "Discord snowflake — return messages before this id. Digits only.",
                ),
            ),
            (
                "after",
                query_param(
                    "string",
                    false,
                    "Discord snowflake — return messages after this id. Digits only.",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "1473922824350601297"}, "query": {"limit": 5}}),
            json!({"messages": [{"id": "1500000000000000000", "content": "hello", "author": {"id": "100", "username": "bot"}}]}),
        )
        .with_error_example(
            403,
            json!({"path": {"id": "1473922824350601297"}}),
            json!({"error": "channel not in role-map"}),
        )
        .with_curl("curl 'http://localhost:8787/api/discord/channels/1473922824350601297/messages?limit=5'"),
        ep(
            "GET",
            "/api/discord/channels/{id}",
            "discord",
            "Get channel or thread info",
        ),
        ep(
            "POST",
            "/api/dm-reply/register",
            "discord",
            "Register DM reply handler",
        )
        .with_example(
            json!({"body": {"source_agent": "family-counsel", "user_id": "123456789012345678", "channel_id": "1473922824350601297", "ttl_seconds": 3600, "context": {"topic": "followup"}}}),
            json!({"ok": true, "id": 42}),
        ),
        ep(
            "GET",
            "/api/round-table-meetings",
            "meetings",
            "List meetings",
        ),
        ep(
            "POST",
            "/api/round-table-meetings",
            "meetings",
            "Create or update meeting",
        )
        .with_example(
            json!({"body": {"id": "meeting-1", "channel_id": "1473922824350601297", "agenda": "API docs review", "status": "completed", "summary": "Decided to add examples", "participant_names": ["PM", "Reviewer"], "total_rounds": 2}}),
            json!({"ok": true, "meeting": {"id": "meeting-1", "status": "completed", "agenda": "API docs review"}}),
        ),
        ep(
            "POST",
            "/api/round-table-meetings/start",
            "meetings",
            "Start meeting",
        )
        .with_example(
            json!({"body": {"channel_id": "1473922824350601297", "agenda": "Plan docs sweep", "primary_provider": "codex", "reviewer_provider": "claude", "fixed_participants": ["project-agentdesk"]}}),
            json!({"ok": true, "message": "Meeting start scheduled"}),
        ),
        ep(
            "GET",
            "/api/round-table-meetings/{id}",
            "meetings",
            "Get meeting by ID",
        ),
        ep(
            "DELETE",
            "/api/round-table-meetings/{id}",
            "meetings",
            "Delete meeting",
        )
        .with_example(
            json!({"path": {"id": "meeting-1"}}),
            json!({"ok": true}),
        ),
        ep(
            "PATCH",
            "/api/round-table-meetings/{id}/issue-repo",
            "meetings",
            "Update meeting issue repository",
        )
        .with_example(
            json!({"path": {"id": "meeting-1"}, "body": {"repo": "itismyfield/AgentDesk"}}),
            json!({"ok": true, "meeting": {"id": "meeting-1", "issue_repo": "itismyfield/AgentDesk"}}),
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues",
            "meetings",
            "Create meeting issues",
        )
        .with_example(
            json!({"path": {"id": "meeting-1"}, "body": {"repo": "itismyfield/AgentDesk"}}),
            json!({"ok": true, "results": [{"key": "item-0", "ok": true, "issue_number": 4227}], "summary": {"total": 1, "created": 1, "failed": 0, "discarded": 0, "pending": 0}}),
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues/discard",
            "meetings",
            "Discard one meeting issue",
        )
        .with_example(
            json!({"path": {"id": "meeting-1"}, "body": {"key": "item-0"}}),
            json!({"ok": true, "summary": {"discarded": 1, "pending": 0}}),
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues/discard-all",
            "meetings",
            "Discard all meeting issues",
        )
        .with_example(
            json!({"path": {"id": "meeting-1"}}),
            json!({"ok": true, "results": [], "summary": {"discarded": 3, "pending": 0, "all_resolved": true}}),
        ),
        ep("GET", "/api/skills/catalog", "skills", "List skill catalog")
            .with_params([(
                "include_stale",
                query_param(
                    "boolean",
                    false,
                    "Include stale skill entries that no longer exist on disk",
                ),
            )])
            .with_example(
                json!({"query": {"include_stale": true}}),
                json!({"catalog": [{"name": "memory-read", "total_calls": 12, "disk_present": true}], "include_stale": true}),
            ),
        ep(
            "GET",
            "/api/skills/ranking",
            "skills",
            "Skill usage ranking",
        )
        .with_params([
            (
                "window",
                query_param("string", false, "Ranking window: 7d, 30d, 90d, all"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum number of ranking entries"),
            ),
            (
                "include_stale",
                query_param(
                    "boolean",
                    false,
                    "Include stale skill entries that no longer exist on disk",
                ),
            ),
        ])
        .with_example(
            json!({"query": {"window": "7d", "limit": 10}}),
            json!({"window": "7d", "include_stale": false, "overall": [{"skill_name": "memory-read", "calls": 12}], "byAgent": [{"agent_role_id": "project-agentdesk", "skill_name": "memory-read", "calls": 4}]}),
        ),
        ep("POST", "/api/skills/prune", "skills", "Preview or prune stale skill metadata")
            .with_params([(
                "dry_run",
                query_param(
                    "boolean",
                    false,
                    "When true, report stale skill ids without deleting skills rows",
                ),
            )])
            .with_example(
                json!({"query": {"dry_run": true}}),
                json!({"ok": true, "dry_run": true, "stale_skill_ids": ["old-skill"], "stale_count": 1, "soft_deleted_from_skills": 0, "skill_usage_policy": "preserved"}),
            ),
        ep("GET", "/api/cron-jobs", "cron", "List cron jobs"),
        ep(
            "GET",
            "/api/routines",
            "routines",
            "List durable routines with optional agent/status filters.",
        )
        .with_params([
            (
                "agent_id",
                query_param("string", false, "Filter routines attached to one agent"),
            ),
            (
                "status",
                query_param("string", false, "Filter by enabled, paused, or detached"),
            ),
        ])
        .with_example(
            json!({"query": {"status": "enabled"}}),
            json!({"routines": [{"id": "routine-1", "script_ref": "daily-summary.js", "status": "enabled", "fallback_agent_id": "claude", "max_retries": 1}], "default_timeout_secs": 1800}),
        ),
        ep(
            "GET",
            "/api/routines/metrics",
            "routines",
            "Aggregate routine status counts, run outcome/error counts, and average finished-run latency with optional agent and time-window filters.",
        )
        .with_params([
            (
                "agent_id",
                query_param("string", false, "Filter metrics to one attached agent"),
            ),
            (
                "since",
                query_param("string", false, "Optional RFC3339 lower bound for routine_runs.created_at"),
            ),
        ])
        .with_example(
            json!({"query": {"agent_id": "codex", "since": "2026-04-29T00:00:00Z"}}),
            json!({"metrics": {"routines_total": 3, "routines_enabled": 2, "routines_paused": 1, "routines_detached": 0, "runs_total": 12, "runs_running": 1, "runs_succeeded": 9, "runs_failed": 1, "runs_skipped": 0, "runs_paused": 0, "runs_interrupted": 1, "runs_error": 2, "avg_latency_ms": 1532.4}, "filters": {"agent_id": "codex", "since": "2026-04-29T00:00:00Z"}}),
        ),
        ep(
            "GET",
            "/api/routines/runs/search",
            "routines",
            "Search recent routine runs by `routine_runs.result_json` text with optional agent, status, time-window, and limit filters.",
        )
        .with_params([
            ("q", query_param("string", true, "Search text matched against routine_runs.result_json")),
            (
                "agent_id",
                query_param("string", false, "Filter matches to one attached agent"),
            ),
            (
                "status",
                query_param("string", false, "Filter by running, succeeded, failed, skipped, paused, or interrupted"),
            ),
            (
                "since",
                query_param("string", false, "Optional RFC3339 lower bound for routine_runs.created_at"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum rows to return, clamped to 1..100"),
            ),
        ])
        .with_example(
            json!({"query": {"q": "checkpoint", "agent_id": "codex", "status": "succeeded", "limit": 20}}),
            json!({"runs": [{"id": "run-1", "routine_id": "routine-1", "script_ref": "agent-checkpoint-review.js", "status": "succeeded", "result_json": {"summary": "checkpoint ok"}}], "filters": {"q": "checkpoint", "agent_id": "codex", "status": "succeeded", "since": null, "limit": 20}}),
        ),
        ep(
            "POST",
            "/api/routines",
            "routines",
            "Attach a file-backed routine row without starting an agent action.",
        )
        .with_params([
            (
                "script_ref",
                body_param(
                    "string",
                    true,
                    "Routine script path relative to routines.dir or routines.additional_dirs",
                ),
            ),
            ("name", body_param("string", false, "Human-readable routine name")),
            ("agent_id", body_param("string", false, "Optional attached agent id")),
            (
                "fallback_agent_id",
                body_param("string", false, "Optional fallback agent id used after primary retry exhaustion"),
            ),
            (
                "max_retries",
                body_param("integer", false, "Maximum primary-agent retries before fallback/failure; default 0"),
            ),
            ("execution_strategy", body_param("string", false, "fresh or persistent")),
            (
                "schedule",
                body_param("string", false, "Optional @every duration or 5-field cron such as 30 9 * * 1-5"),
            ),
            ("next_due_at", body_param("string", false, "Optional RFC3339 due time")),
            ("discord_thread_id", body_param("string", false, "Optional existing Discord thread id")),
            ("timeout_secs", body_param("integer", false, "Optional per-routine agent timeout in seconds")),
        ])
        .with_example(
            json!({"body": {"script_ref": "daily-summary.js", "name": "Daily Summary", "agent_id": "codex", "fallback_agent_id": "claude", "max_retries": 1, "execution_strategy": "fresh"}}),
            json!({"routine": {"id": "routine-1", "script_ref": "daily-summary.js", "status": "enabled"}, "discord_log": {"status": "skipped"}}),
        ),
        ep(
            "GET",
            "/api/routines/{id}",
            "routines",
            "Get one durable routine row.",
        )
        .with_params([("id", path_param("Routine id"))])
        .with_example(
            json!({"path": {"id": "routine-1"}}),
            json!({"routine": {"id": "routine-1", "script_ref": "daily-summary.js", "fallback_agent_id": "claude", "max_retries": 1}, "default_timeout_secs": 1800}),
        ),
        ep(
            "PATCH",
            "/api/routines/{id}",
            "routines",
            "Patch routine metadata, scheduling fields, or checkpoint.",
        )
        .with_params([
            ("id", path_param("Routine id")),
            ("name", body_param("string", false, "New routine name")),
            (
                "fallback_agent_id",
                body_param("string|null", false, "Fallback agent id or null to clear it"),
            ),
            (
                "max_retries",
                body_param("integer", false, "Maximum primary-agent retries before fallback/failure"),
            ),
            (
                "execution_strategy",
                body_param("string", false, "fresh or persistent"),
            ),
            (
                "schedule",
                body_param("string|null", false, "Set @every duration or 5-field cron, or pass null to clear it"),
            ),
            (
                "next_due_at",
                body_param("string|null", false, "RFC3339 due time or null to clear it"),
            ),
            (
                "checkpoint",
                body_param("object|null", false, "Replacement checkpoint JSON or null to clear it; run-produced checkpoints are capped by routines.max_checkpoint_bytes"),
            ),
            (
                "discord_thread_id",
                body_param("string|null", false, "Saved Discord thread id or null to clear it"),
            ),
            (
                "timeout_secs",
                body_param("integer|null", false, "Per-routine agent timeout in seconds or null for config default"),
            ),
        ]),
        ep(
            "DELETE",
            "/api/routines/{id}",
            "routines",
            "Hard-delete a detached routine and its routine_runs history. Returns 403 when an owned routine's caller agent scope is absent, unresolved, or different; returns 409 when the routine is not detached or has an in-flight run.",
        )
        .with_params([
            ("id", path_param("Routine id")),
            (
                "x-agent-id",
                header_param("string", false, "Self-asserted caller agent scope; required to match the owner for owned routines"),
            ),
            (
                "x-channel-id",
                header_param("string", false, "Self-asserted caller channel scope resolved to an agent id; required for owned routines when x-agent-id is absent"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "routine-1"}, "headers": {"x-agent-id": "codex"}}),
            json!({"ok": true, "routine_id": "routine-1", "run_history_deleted": 3}),
        ),
        ep(
            "GET",
            "/api/routines/{id}/runs",
            "routines",
            "List recent run history for one routine, including best-effort Discord log status and warning detail.",
        )
        .with_params([
            ("id", path_param("Routine id")),
            ("limit", query_param("integer", false, "Maximum runs to return, capped at 100")),
        ]),
        ep(
            "POST",
            "/api/routines/{id}/pause",
            "routines",
            "Pause an enabled routine, clear its next due time, and enqueue a best-effort Discord log when an attached agent has a channel.",
        )
        .with_params([("id", path_param("Routine id"))]),
        ep(
            "POST",
            "/api/routines/{id}/resume",
            "routines",
            "Resume a paused routine with an optional next due time and best-effort Discord log.",
        )
        .with_params([
            ("id", path_param("Routine id")),
            ("next_due_at", body_param("string", false, "Optional RFC3339 due time")),
        ]),
        ep(
            "POST",
            "/api/routines/{id}/detach",
            "routines",
            "Detach a non-running routine without deleting its run history; Discord log failure is returned only as discord_log.warning_code/warning.",
        )
        .with_params([("id", path_param("Routine id"))]),
        ep(
            "POST",
            "/api/routines/{id}/run-now",
            "routines",
            "Claim and execute one routine. Script actions close immediately; agent actions store turn_id and remain running until session_transcripts completion evidence is found.",
        )
        .with_params([("id", path_param("Routine id"))])
        .with_example(
            json!({"path": {"id": "routine-1"}}),
            json!({"outcome": {"run_id": "run-1", "routine_id": "routine-1", "action": "agent", "status": "running", "result_json": {"turn_id": "discord:1473922824350601297:9100000000000000000", "fresh_context_guaranteed": false}}, "discord_log": {"status": "ok"}}),
        ),
        ep(
            "POST",
            "/api/routines/{id}/session/reset",
            "routines",
            "Reset the provider session for a persistent agent-backed routine. Claude sends /clear; managed tmux providers reset the process session; providers without managed tmux clear runtime mailbox state only.",
        )
        .with_params([("id", path_param("Routine id"))])
        .with_example(
            json!({"path": {"id": "routine-1"}}),
            json!({"ok": true, "session": {"action": "reset", "provider": "codex", "provider_clear_behavior": "runtime clear plus managed process session reset for the provider tmux session", "runtime_cleared": true}, "interrupted_run_id": null}),
        ),
        ep(
            "POST",
            "/api/routines/{id}/session/kill",
            "routines",
            "Force-kill the provider session for a persistent agent-backed routine, disconnect matching session rows, and interrupt the routine's in-flight run when the session actually changes.",
        )
        .with_params([("id", path_param("Routine id"))])
        .with_example(
            json!({"path": {"id": "routine-1"}}),
            json!({"ok": true, "session": {"action": "kill", "provider": "codex", "tmux_killed": true, "lifecycle_path": "mailbox_canonical"}, "interrupted_run_id": "run-1"}),
        ),
        ep(
            "POST",
            "/api/queue/generate",
            "auto-queue",
            "Generate auto-queue entries. Single-call complete: do NOT chain /redispatch, /retry, or /transition for the same card after it (#1442). Inspect skipped_due_to_active_dispatch / skipped_due_to_dependency / skipped_due_to_filter in the response to see structured skip reasons. See /api/docs/card-lifecycle-ops for the full decision tree (#1443).",
        )
        .with_params([
            (
                "repo",
                body_param("string", false, "Filter cards by repository"),
            ),
            (
                "agent_id",
                body_param("string", false, "Filter cards by assigned agent"),
            ),
            (
                "auto_assign_agent",
                body_param(
                    "boolean",
                    false,
                    "Assign unowned explicit issue_numbers or entries to agent_id before queue generation",
                )
                .with_default(false),
            ),
            (
                "issue_numbers",
                body_param(
                    "number[]",
                    false,
                    "Explicit GitHub issue numbers to include in the run",
                ),
            ),
            (
                "entries",
                body_param(
                    "object[]",
                    false,
                    "Explicit entries with issue_number, batch_phase, and optional thread_group",
                ),
            ),
            (
                "unified_thread",
                body_param(
                    "boolean",
                    false,
                    "Accepted for compatibility but ignored; generate keeps slot pooling",
                )
                .with_default(false),
            ),
            (
                "max_concurrent_threads",
                body_param("number", false, "Upper bound for simultaneously active groups")
                    .with_default(1),
            ),
            (
                "review_mode",
                body_param(
                    "string",
                    false,
                    "Run review mode: 'enabled' keeps the normal review gate; 'disabled' skips review dispatch creation and waits for main-merge detection before moving to done",
                )
                .with_default("enabled"),
            ),
            (
                "max_concurrent_per_agent",
                body_param(
                    "number",
                    false,
                    "Legacy compatibility field; accepted but ignored",
                ),
            ),
        ])
        .with_example(
            json!({"body": {"repo": "test-repo", "issue_numbers": [423, 405, 407], "review_mode": "disabled", "unified_thread": true, "max_concurrent_threads": 2}}),
            json!({
                "run": {"id": "run-1", "status": "generated", "review_mode": "disabled", "thread_group_count": 2, "max_concurrent_threads": 2, "unified_thread": false},
                "entries": [{"id": "entry-1", "github_issue_number": 423, "thread_group": 0, "priority_rank": 0, "status": "pending"}],
                "skipped_due_to_active_dispatch": [{"issue_number": 405, "existing_dispatch_id": "dispatch-already-running"}],
                "skipped_due_to_dependency": [{"issue_number": 407, "unresolved_deps": ["#410:in_progress"]}],
                "skipped_due_to_filter": []
            }),
        )
        .with_error_example(
            400,
            json!({"body": {"repo": "test-repo", "issue_numbers": []}}),
            json!({"error": "issue_numbers or entries must be non-empty"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/generate -H 'Content-Type: application/json' -d '{\"repo\":\"test-repo\",\"issue_numbers\":[423,405]}'")
    ]
}
