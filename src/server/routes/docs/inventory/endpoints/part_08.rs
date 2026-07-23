use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "POST",
            "/api/turns/{channel_id}/cancel",
            "queue",
            "Cancel the active turn in a channel. Default (force=false) requests the preserve path: drain the channel mailbox while leaving the live tmux session and tool subprocesses (cargo, claude CLI, …) alone — the usual meaning of 'queue 정리'. NOTE: this is best-effort, not a hard guarantee. The underlying C-c → SIGKILL → child cleanup can still take the tmux session down as a side effect (e.g. the Claude TUI wrapper exits when `claude` exits). The authoritative side-effect signal is `tmux_killed=true` in the response; treat that flag as the source of truth regardless of `lifecycle_path` (canonical, runtime-fallback, and direct-fallback paths can all report `tmux_killed=true` on the preserve route — `lifecycle_path` only describes which cleanup route ran, not whether tmux survived). force=true tears the tmux session down and SIGKILLs the entire child PID tree; the turn will not complete gracefully. Reserve force=true for explicit recovery (#1196). Always inspect `tmux_killed`, `lifecycle_path`, `queue_preserved`, and `inflight_cleared` to learn what actually happened. `dispatch_cancelled` reports the *attempted* dispatch id (the one this cancel-turn correlated to); the underlying postgres dispatch cancel is best-effort and logs warnings on failure rather than aborting the response, so do not treat a non-null `dispatch_cancelled` as proof the dispatch row reached a terminal state — verify via `/api/dispatches/{id}` if certainty is required. See src/server/routes/queue_api.rs::cancel_turn and src/services/queue.rs::QueueService::cancel_turn.",
        )
        .with_params([
            ("channel_id", path_param("Discord channel ID hosting the live turn")),
            (
                "force",
                query_param(
                    "boolean",
                    false,
                    "false (default): drain mailbox and request the preserve path (live tmux session + tool subprocesses kept alive on a best-effort basis; `tmux_killed=true` in the response indicates the session died as a side effect). true: SIGKILL the tmux session and child PID tree; in-flight cargo/claude subprocesses are terminated.",
                )
                .with_default(false),
            ),
        ])
        .with_example(
            json!({"path": {"channel_id": "1473922824350601297"}}),
            json!({
                "ok": true,
                "channel_id": "1473922824350601297",
                "agent_id": "agent-cc",
                "requested_provider": "claude",
                "exact_channel_match": true,
                "session_key": "claude:1473922824350601297:agentdesk-claude-channel-1473922824350601297",
                "tmux_session": "agentdesk-claude-channel-1473922824350601297",
                "tmux_killed": false,
                "lifecycle_path": "canonical",
                "queued_remaining": 0,
                "queued_before": 0,
                "queue_preserved": true,
                "queue_disk_present_before": false,
                "queue_disk_present_after": false,
                "inflight_cleared": false,
                "dispatch_cancelled": null,
                "turn_status": "cancelled",
                "turn_completed_at": "2026-05-17T03:00:00+00:00"
            }),
        )
        .with_error_example(
            404,
            json!({"path": {"channel_id": "1473922824350601297"}}),
            json!({
                "error": "no active turn found for this channel",
                "code": "queue",
                "context": {"channel_id": "1473922824350601297"}
            }),
        )
        .with_curl("curl -X POST 'http://localhost:8787/api/turns/1473922824350601297/cancel?force=false'"),
        ep(
            "POST",
            "/api/turns/{channel_id}/extend-timeout",
            "queue",
            "Extend live turn timeout",
        )
        .with_example(
            json!({"path": {"channel_id": "1473922824350601297"}, "body": {"extend_secs": 1800}}),
            json!({"ok": true, "channel_id": "1473922824350601297", "requested_extend_secs": 1800, "applied_extend_secs": 1800, "remaining_minutes": 30}),
        ),
        ep(
            "POST",
            "/api/channels/{channel_id}/monitoring",
            "monitoring",
            "Create or update a channel monitoring status entry",
        )
        .with_params([
            ("channel_id", path_param("Discord channel ID")),
            ("key", body_param("string", true, "Stable monitoring entry key")),
            (
                "description",
                body_param("string", true, "Human-readable status description"),
            ),
        ])
        .with_example(
            json!({"path": {"channel_id": "1473922824350601297"}, "body": {"key": "relay", "description": "Relay queue is healthy"}}),
            json!({"status": "ok", "active_count": 1}),
        ),
        ep(
            "GET",
            "/api/channels/{channel_id}/monitoring",
            "monitoring",
            "List channel monitoring status entries",
        )
        .with_params([("channel_id", path_param("Discord channel ID"))])
        .with_example(
            json!({"path": {"channel_id": "1473922824350601297"}}),
            json!({"status": "ok", "active_count": 1, "entries": [{"key": "relay", "description": "Relay queue is healthy"}]}),
        ),
        ep(
            "DELETE",
            "/api/channels/{channel_id}/monitoring/{key}",
            "monitoring",
            "Remove a channel monitoring status entry",
        )
        .with_params([
            ("channel_id", path_param("Discord channel ID")),
            ("key", path_param("Monitoring entry key")),
        ])
        .with_example(
            json!({"path": {"channel_id": "1473922824350601297", "key": "relay"}}),
            json!({"status": "ok", "active_count": 0}),
        ),
        ep("GET", "/api/analytics", "analytics", "Observability counters and structured events")
            .with_params([
                (
                    "provider",
                    query_param("string", false, "Filter by provider id (claude/codex/gemini/opencode/qwen)"),
                ),
                (
                    "channelId",
                    query_param("string", false, "Filter by Discord channel id"),
                ),
                (
                    "eventType",
                    query_param("string", false, "Filter by event type"),
                ),
                (
                    "limit",
                    query_param("integer", false, "Maximum recent events to return").with_default(100),
                ),
            ])
            .with_example(
                json!({"query": {"provider": "codex", "channelId": "1473922824350601297", "eventType": "turn_started", "limit": 25}}),
                json!({"counters": [{"provider": "codex", "channel_id": "1473922824350601297", "turns_total": 12}], "recent_events": [{"event_type": "turn_started", "provider": "codex"}]}),
            ),
        ep(
            "GET",
            "/api/analytics/invariants",
            "analytics",
            "Runtime invariant violation counts and recent events",
        )
        .with_params([
            (
                "provider",
                query_param("string", false, "Filter by provider id (claude/codex/gemini/opencode/qwen)"),
            ),
            (
                "channelId",
                query_param("string", false, "Filter by Discord channel id"),
            ),
            (
                "invariant",
                query_param("string", false, "Filter by invariant key"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum recent violations to return").with_default(50),
            ),
        ])
        .with_example(
            json!({"query": {"limit": 10}}),
            json!({"counts": {"dispatch_without_card": 0}, "recent_violations": []}),
        )
        .with_error_example(
            400,
            json!({"query": {"limit": -1}}),
            json!({"error": "limit must be non-negative"}),
        )
        .with_curl("curl 'http://localhost:8787/api/analytics/invariants?limit=10'"),
        ep(
            "GET",
            "/api/analytics/observability",
            "analytics",
            "Foundation-layer atomic counters per channel×provider + in-memory structured event ring (#1070)",
        )
        .with_params([(
            "recentLimit",
            query_param("integer", false, "Maximum recent events to return (<=1000)")
                .with_default(100),
        )])
        .with_example(
            json!({"query": {"recentLimit": 50}}),
            json!({"counters": [{"channel_id": "1473922824350601297", "provider": "codex", "turns_total": 320, "errors_total": 3}], "recent_events": []}),
        )
        .with_error_example(
            400,
            json!({"query": {"recentLimit": 9999}}),
            json!({"error": "recentLimit must be <= 1000"}),
        )
        .with_curl("curl 'http://localhost:8787/api/analytics/observability?recentLimit=50'"),
        ep(
            "GET",
            "/api/quality/events",
            "analytics",
            "Agent quality raw event stream",
        )
        .with_params([
            (
                "agent_id",
                query_param("string", false, "Filter by agent id"),
            ),
            (
                "days",
                query_param("integer", false, "Lookback window in days").with_default(7),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum recent events to return").with_default(200),
            ),
        ])
        .with_example(
            json!({"query": {"agent_id": "project-agentdesk", "days": 7, "limit": 50}}),
            json!({"events": [{"agent_id": "project-agentdesk", "event_type": "turn_completed", "quality_score": 0.95}]}),
        ),
        ep("GET", "/api/streaks", "analytics", "Agent activity streaks"),
        ep("GET", "/api/achievements", "analytics", "Agent achievements"),
        ep(
            "GET",
            "/api/activity-heatmap",
            "analytics",
            "Activity heatmap by hour",
        ),
        ep("GET", "/api/audit-logs", "analytics", "Audit logs"),
        ep(
            "GET",
            "/api/machine-status",
            "analytics",
            "Machine online status",
        ),
        ep(
            "GET",
            "/api/rate-limits",
            "analytics",
            "Cached rate limits per provider",
        ),
        ep("GET", "/api/receipt", "analytics", "Latest usage receipt snapshot"),
        ep(
            "GET",
            "/api/token-analytics",
            "analytics",
            "Token dashboard analytics with daily trend, heatmap, and usage breakdowns",
        )
        .with_params([(
            "period",
            ParamDoc {
                location: "query",
                kind: "string",
                required: false,
                description: "Analytics window",
                enum_values: None,
                default: None,
            }
            .with_enum(&["7d", "30d", "90d"])
            .with_default("30d"),
        )])
        .with_example(
            json!({"query": {"period": "30d", "fresh": "1"}}),
            json!({"period": "30d", "summary": {"total_tokens": 12000, "total_cost": 1.23}, "daily": [], "heatmap": []}),
        ),
        ep(
            "GET",
            "/api/home/kpi-trends",
            "analytics",
            "Home KPI sparkline data — tokens, cost, in-progress dispatch counts, and rate-limit utilization in a single payload (#1242). Each series exposes `label`, `unit`, and a `values` array sized to the requested `days` window so a sparkline component can render any tile with the same axis. The rate-limit section returns one entry per provider with `current_pct`, `unsupported`, `stale`, `reason`, and a flat `values` sparkline; providers without telemetry come back with `unsupported: true` and an empty `values` array so the dashboard can render a placeholder.",
        )
        .with_params([(
            "days",
            query_param("integer", false, "Lookback window in days (default 14, clamped to [1, 30])"),
        )])
        .with_example(
            json!({"query": {"days": 14}}),
            json!({
                "days": 14,
                "generated_at": "2026-04-27T00:00:00Z",
                "dates": ["2026-04-14", "2026-04-15", "..."],
                "tokens":      {"label": "Today's tokens", "unit": "tokens",     "values": [0, 0]},
                "cost":        {"label": "API cost",       "unit": "usd",        "values": [0.0, 0.0]},
                "in_progress": {"label": "In progress",    "unit": "dispatches", "values": [0, 0]},
                "rate_limit": {
                    "label": "Rate limit",
                    "unit": "percent",
                    "providers": [
                        {"provider": "claude", "current_pct": 25.0, "unsupported": false, "stale": false, "reason": null, "values": [25.0, 25.0]},
                        {"provider": "qwen",   "current_pct": null, "unsupported": true,  "stale": false, "reason": "no telemetry yet", "values": []}
                    ]
                }
            }),
        ),
        ep(
            "GET",
            "/api/skills-trend",
            "analytics",
            "Skill usage trend by day",
        )
        .with_example(
            json!({"query": {"days": 30}}),
            json!({"days": 30, "trend": [{"date": "2026-07-08", "skill_name": "memory-read", "calls": 4}]}),
        ),
        ep(
            "GET",
            "/api/help",
            "docs",
            "Agent-friendly API inventory with categories, params, and examples",
        )
        .with_example(
            json!({}),
            json!({"categories": [{"name": "queue", "count": 15}], "endpoints": [{"method": "POST", "path": "/api/queue/generate", "category": "queue", "subcategory": "auto-queue"}]}),
        ),
        ep(
            "GET",
            "/api/docs",
            "docs",
            "List the eight #1063 top-level documentation groups, or return the flat endpoint list with format=flat",
        )
        .with_params([(
            "format",
            query_param("string", false, "Use format=flat for the full endpoint array"),
        )])
        .with_example(
            json!({}),
            json!({"groups": [{"name": "runtime", "description": "Turns, sessions, dispatches, message log, and server lifecycle surfaces.", "categories": ["dispatches", "sessions"]}]}),
        )
        .with_error_example(
            400,
            json!({"query": {"format": "xml"}}),
            json!({"error": "unsupported format: xml; use format=flat for the flat endpoint list"}),
        )
        .with_curl("curl http://localhost:8787/api/docs"),
        ep(
            "GET",
            "/api/docs/{group}",
            "docs",
            "List the fine-grained categories inside one of the eight #1063 groups (runtime/kanban/agents/integrations/automation/config/observability/internal). Falls back to legacy flat-category output with X-Deprecated header when a category name is supplied instead.",
        )
        .with_params([(
            "group",
            path_param("Group name such as runtime, kanban, automation, or integrations"),
        )])
        .with_example(
            json!({"path": {"group": "kanban"}}),
            json!({"group": "kanban", "categories": [{"name": "kanban", "endpoint_count": 24}, {"name": "reviews", "endpoint_count": 8}]}),
        ),
        ep(
            "GET",
            "/api/docs/{group}/{category}",
            "docs",
            "Get detailed endpoints for one fine-grained category nested inside a group (e.g. kanban/reviews, automation/auto-queue).",
        )
        .with_params([
            (
                "group",
                path_param("Top-level group name such as kanban or automation"),
            ),
            (
                "category",
                path_param("Category under the group such as reviews or auto-queue"),
            ),
        ])
        .with_example(
            json!({"path": {"group": "kanban", "category": "reviews"}}),
            json!({"group": "kanban", "category": "reviews", "count": 8, "endpoints": [{"method": "POST", "path": "/api/reviews/verdict"}]}),
        ),
        ep(
            "POST",
            "/api/reviews/verdict",
            "reviews",
            "Submit counter-model review verdict for a review dispatch",
        )
        .with_params([
            ("dispatch_id", body_param("string", true, "Review dispatch ID")),
            ("overall", body_param("string", true, "pass | improve | reject | rework | approved")),
            ("notes", body_param("string", false, "Reviewer notes")),
            ("feedback", body_param("string", false, "Reviewer feedback")),
            ("commit", body_param("string", false, "Reviewed commit SHA")),
            ("provider", body_param("string", false, "Verdict submitter provider")),
        ])
        .with_example(
            json!({"body": {"dispatch_id": "review-1", "overall": "pass", "notes": "LGTM"}}),
            json!({"ok": true, "dispatch_id": "review-1", "overall": "pass"}),
        )
        .with_error_example(
            400,
            json!({"body": {"dispatch_id": "review-1"}}),
            json!({"error": "overall must be one of: pass, improve, reject, rework, approved"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/reviews/verdict -H 'Content-Type: application/json' -d '{\"dispatch_id\":\"review-1\",\"overall\":\"pass\",\"notes\":\"LGTM\"}'"),
        ep(
            "POST",
            "/api/reviews/decision",
            "reviews",
            "Submit review-decision action. For accept, optional commit_sha takes precedence over worktree inference for skip_rework detection.",
        )
        .with_params([
            ("card_id", body_param("string", true, "Kanban card ID")),
            (
                "decision",
                body_param("string", true, "accept | dispute | dismiss"),
            ),
            ("comment", body_param("string", false, "Optional decision comment")),
            (
                "commit_sha",
                body_param(
                    "string",
                    false,
                    "Current implementation commit SHA. On accept, explicit commit_sha is compared to the last review reviewed_commit before falling back to worktree inference.",
                ),
            ),
            (
                "dispatch_id",
                body_param(
                    "string",
                    false,
                    "Pending review-decision dispatch ID for stale/replay protection",
                ),
            ),
        ])
        .with_example(
            json!({"body": {"card_id": "card-1977", "decision": "accept", "commit_sha": "dbadcb1234567890"}}),
            json!({"ok": true, "card_id": "card-1977", "decision": "accept", "rework_dispatch_created": false, "direct_review_created": true, "review_auto_approved": false, "skip_rework": true}),
        )
        .with_error_example(
            400,
            json!({"body": {"card_id": "card-1977", "decision": "accept", "commit_sha": "not-a-sha"}}),
            json!({"error": "commit_sha must be a 7-64 character hex git commit SHA", "field": "commit_sha"}),
        ),
        ep(
            "POST",
            "/api/reviews/tuning/aggregate",
            "reviews",
            "Aggregate review-tuning outcomes",
        ),
        ep(
            "POST",
            "/api/pm-decision",
            "pm",
            "Apply a PM decision to a force-only card",
        )
        .with_params([
            ("card_id", body_param("string", true, "Kanban card ID")),
            (
                "decision",
                body_param("string", true, "PM decision")
                    .with_enum(&["resume", "rework", "dismiss", "requeue"]),
            ),
            (
                "comment",
                body_param("string", false, "Optional PM comment"),
            ),
        ])
        .with_example(
            json!({"body": {"card_id": "card-1", "decision": "requeue", "comment": "needs reprioritization"}}),
            json!({"ok": true, "card_id": "card-1", "decision": "requeue", "message": "Card moved back to ready for reprioritization"}),
        ),
        // #1066 /api/memory dual-mode
        ep(
            "POST",
            "/api/memory/recall",
            "memory",
            "Recall memory fragments by keyword/text from PostgreSQL local_memory. Auto-detects runtime-active memento, but memento recall is not implemented on this HTTP route; when runtime-active memento is selected the endpoint returns 501 instead of falling back to local rows. If memento is unavailable, or ADK_FORCE_LOCAL_MEMORY=1 is set, the route uses local fallback access.",
        )
        .with_params([
            (
                "keywords",
                body_param("array", false, "List of keywords matched via LIKE in local mode"),
            ),
            (
                "text",
                body_param("string", false, "Free-form text appended to keyword filters"),
            ),
            (
                "workspace",
                body_param("string", false, "Optional workspace scope filter"),
            ),
            (
                "limit",
                body_param("integer", false, "Max fragments returned (default 20, max 200)"),
            ),
        ])
        .with_example(
            json!({"body": {"keywords": ["postgres"], "workspace": "ops", "limit": 5}}),
            json!({
                "fragments": [{"id": "mem-abc", "content": "PostgreSQL cutover done", "topic": "pg-cutover"}],
                "source": "local",
                "detected_backend": "local"
            }),
        )
        .with_example(
            json!({"body": {"keywords": ["postgres"], "workspace": "ops"}}),
            json!({
                "ok": false,
                "error": "memento recall bridge is not implemented on /api/memory/recall",
                "code": "memento_recall_unsupported",
                "operation": "recall",
                "source": "memento",
                "detected_backend": "memento",
                "local_fallback_available": true,
                "local_fallback_hint": "set ADK_FORCE_LOCAL_MEMORY=1 to query/delete only PostgreSQL local_memory fallback rows"
            }),
        ),
        ep(
            "POST",
            "/api/memory/remember",
            "memory",
            "Persist a memory fragment. Auto-selects memento or local backend.",
        )
        .with_params([
            ("content", body_param("string", true, "Fragment content")),
            ("topic", body_param("string", true, "Topic label for grouping")),
            (
                "type",
                body_param(
                    "string",
                    true,
                    "Fragment type: fact/decision/error/preference/procedure/relation/episode",
                ),
            ),
            (
                "importance",
                body_param("number", false, "Importance score 0.0–1.0"),
            ),
            (
                "workspace",
                body_param("string", false, "Optional workspace scope"),
            ),
            (
                "keywords",
                body_param("array", false, "Optional keyword array"),
            ),
        ])
        .with_example(
            json!({"body": {"content": "Agent #1066 landed", "topic": "release", "type": "decision"}}),
            json!({"id": "mem-abc", "source": "local"}),
        ),
        ep(
            "POST",
            "/api/memory/forget",
            "memory",
            "Remove a PostgreSQL local_memory fragment by id. Auto-detects runtime-active memento, but memento forget is not implemented on this HTTP route; when runtime-active memento is selected the endpoint returns 501 instead of deleting local fallback rows. If memento is unavailable, or ADK_FORCE_LOCAL_MEMORY=1 is set, the route uses local fallback access.",
        )
        .with_params([("id", body_param("string", true, "Fragment id returned by remember"))])
        .with_example(
            json!({"body": {"id": "mem-abc"}}),
            json!({"ok": true, "source": "local"}),
        )
        .with_example(
            json!({"body": {"id": "memento:release"}}),
            json!({
                "ok": false,
                "error": "memento forget bridge is not implemented on /api/memory/forget",
                "code": "memento_forget_unsupported",
                "operation": "forget",
                "source": "memento",
                "detected_backend": "memento",
                "local_fallback_available": true,
                "local_fallback_hint": "set ADK_FORCE_LOCAL_MEMORY=1 to query/delete only PostgreSQL local_memory fallback rows"
            }),
        ),
        // #3719 mounted-route coverage: compact docs for routes that were
        // mounted but absent from the curated /api/docs endpoint list.
        ep(
            "GET",
            "/api/agents/diag/{identifier}",
            "agents",
            "Agent diagnostic snapshot by id or role identifier.",
        )
        .with_params([(
            "identifier",
            path_param("Agent id, role id, or configured agent identifier"),
        )]),
        ep(
            "GET",
            "/api/analytics/policy-hooks",
            "analytics",
            "Policy hook timeout and execution counters.",
        ),
        ep(
            "POST",
            "/api/hook/reset-status",
            "ops",
            "Reset agents currently marked working back to idle and report rows updated.",
        ),
        ep(
            "POST",
            "/api/hook/skill-usage",
            "skills",
            "Record one skill usage event with optional agent, role, and session context.",
        )
        .with_params([
            ("skill_id", body_param("string", true, "Skill identifier to record")),
            ("agent_id", body_param("string", false, "Optional agent id")),
            (
                "role_id",
                body_param(
                    "string",
                    false,
                    "Optional role id used to resolve an agent when agent_id is absent",
                ),
            ),
            (
                "session_key",
                body_param("string", false, "Optional session key associated with the usage event"),
            ),
        ]),
        ep(
            "DELETE",
            "/api/hook/session/{sessionKey}",
            "sessions",
            "Mark the matching session disconnected by session key.",
        )
        .with_params([(
            "sessionKey",
            path_param("Session key to mark disconnected"),
        )]),
        ep(
            "POST",
            "/api/internal/escalation/emit",
            "internal",
            "Emit a manual-decision escalation for a card through the configured user-thread or PM Discord route.",
        )
        .with_params([
            ("card_id", body_param("string", true, "Kanban card id to escalate")),
            (
                "reasons",
                body_param("array<string>", true, "Non-empty escalation reasons"),
            ),
        ]),
        ep(
            "GET",
            "/api/github/pr-summary",
            "github",
            "Fetch a GitHub PR summary from the AgentDesk cache, with gh CLI fallback on cache miss.",
        )
        .with_params([
            ("repo", query_param("string", true, "Repository in owner/name form")),
            ("pr", query_param("integer", true, "Pull request number")),
            (
                "force_refresh",
                query_param("boolean", false, "Bypass cached value and refetch"),
            ),
            (
                "expected_head_sha",
                query_param("string", false, "Use cache only when the head SHA matches"),
            ),
        ]),
        ep(
            "POST",
            "/api/github/pr-summary/invalidate",
            "github",
            "Invalidate one cached GitHub PR summary entry.",
        )
        .with_params([
            ("repo", body_param("string", true, "Repository in owner/name form")),
            ("pr", body_param("integer", true, "Pull request number")),
        ]),
        ep(
            "GET",
            "/api/maintenance/jobs",
            "ops",
            "List maintenance job status records from PostgreSQL.",
        ),
        ep(
            "GET",
            "/api/queue/phase-gates/violations",
            "auto-queue",
            "Report pending or active auto-queue entries whose batch phase is ahead of the run phase pointer.",
        ),
        ep(
            "POST",
            "/api/queue/runs/{id}/entries",
            "auto-queue",
            "Append a GitHub issue entry to an existing auto-queue run.",
        )
        .with_params([
            ("id", path_param("Auto-queue run id")),
            ("issue_number", body_param("integer", true, "GitHub issue number")),
            ("thread_group", body_param("integer", false, "Optional thread-group override")),
            ("batch_phase", body_param("integer", false, "Optional batch phase override")),
        ]),
        ep(
            "GET",
            "/api/round-table-meetings/channels",
            "meetings",
            "List Discord channels available to round-table meeting workflows.",
        ),
        ep(
            "POST",
            "/api/inflight/rebind",
            "dispatches",
            "Recover an orphaned live tmux session by rebinding inflight state and respawning its watcher.",
        ),
        ep(
            "POST",
            "/api/sessions/{session_key}/idle-recap",
            "sessions",
            "Trigger an idle-recap card for a resumable dispatched session.",
        )
        .with_params([(
            "session_key",
            path_param("Dispatched session key to recap"),
        )]),
        ep(
            "POST",
            "/api/sessions/{session_key}/reconcile-stale-turn",
            "sessions",
            "Reset a stale busy session to idle only when it has no active dispatch and an expired heartbeat.",
        )
        .with_params([(
            "session_key",
            path_param("Dispatched session key to reconcile"),
        )]),
        ep(
            "GET",
            "/api/v1/overview",
            "v1",
            "Versioned dashboard overview combining health, agents, kanban, dispatch, token, and sparkline summaries.",
        ),
        ep(
            "GET",
            "/api/v1/agents",
            "v1",
            "Versioned dashboard agent list, optionally filtered by officeId.",
        )
        .with_params([(
            "officeId",
            query_param("string", false, "Optional office id filter"),
        )]),
        ep(
            "GET",
            "/api/v1/tokens",
            "v1",
            "Versioned token usage summary for range or period query windows.",
        )
        .with_params([
            ("range", query_param("string", false, "Usage window such as 7d or 30d")),
            ("period", query_param("string", false, "Legacy alias for range")),
        ]),
        ep(
            "GET",
            "/api/v1/kanban",
            "v1",
            "Versioned dashboard kanban summary.",
        ),
        ep(
            "GET",
            "/api/v1/ops/health",
            "v1",
            "Versioned operational health payload with bottleneck annotations.",
        )
    ]
}
