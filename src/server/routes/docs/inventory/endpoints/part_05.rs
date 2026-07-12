use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "POST",
            "/api/github/issues/create",
            "github",
            "Create a GitHub issue with server-enforced issue markdown format. Successful creation returns HTTP 201 Created (not 200) with the issue payload. block_on records positive GitHub issue-number dependencies for auto-queue via the rendered `## 의존성` section and kanban metadata. dry_run returns 200 OK with rendered_body, no side effects, and capabilities that show auto_dispatch is not supported by this public contract.",
        )
        .with_params([
            (
                "repo",
                body_param(
                    "string",
                    true,
                    "Repository alias (`ADK`, `CH`) or `owner/repo`",
                ),
            ),
            ("title", body_param("string", true, "Issue title")),
            (
                "background",
                body_param("string", true, "Required `## 배경` body text"),
            ),
            (
                "content",
                body_param("array[string]", true, "Required bullet items for `## 내용`"),
            ),
            (
                "dod",
                body_param(
                    "array[string]",
                    true,
                    "Required DoD checklist items (1-10 entries, emitted as `- [ ]`)",
                ),
            ),
            (
                "agent_id",
                body_param(
                    "string",
                    false,
                    "Optional agent id converted into the `agent:<id>` GitHub label; dry_run also warns when the agent is unknown but still previews the label",
                ),
            ),
            (
                "dependencies",
                body_param(
                    "array[number|string|object]",
                    false,
                    "Optional dependency references rendered into `## 의존성`",
                ),
            ),
            (
                "risks",
                body_param(
                    "array[string]",
                    false,
                    "Optional risk bullets rendered into `## 리스크`",
                ),
            ),
            (
                "hints",
                body_param(
                    "array[string]",
                    false,
                    "Optional kickoff hints rendered into `## 착수 힌트` with a warning banner",
                ),
            ),
            (
                "block_on",
                body_param(
                    "array[number]",
                    false,
                    "Optional positive GitHub issue numbers rendered into `## 의존성` and stored as kanban metadata `depends_on` for auto-queue dependency gating",
                ),
            ),
            (
                "dry_run",
                body_param(
                    "boolean",
                    false,
                    "Preview validation, rendered markdown, and labels without creating GitHub issue, kanban card, or announcement; does not check gh CLI availability",
                )
                .with_default(false),
            ),
        ])
        .with_example(
            json!({
                "repo": "ADK",
                "title": "create-issue 스킬을 ADK API로 승격",
                "background": "AgentDesk 내부에서 issue 포맷을 서버 API로 직접 생성해야 한다.",
                "content": [
                    "POST /api/github/issues/create 엔드포인트를 추가한다.",
                    "서버에서 issue 마크다운 포맷을 강제한다."
                ],
                "dod": [
                    "성공 시 GitHub issue URL을 반환한다",
                    "DoD는 서버에서 - [ ] 체크리스트로 변환된다"
                ],
                "agent_id": "adk-backend"
            }),
            json!({
                "issue": {
                    "number": 819,
                    "url": "https://github.com/itismyfield/AgentDesk/issues/819",
                    "repo": "itismyfield/AgentDesk"
                },
                "applied_labels": ["agent:adk-backend"],
                "issue_format_version": 1,
                "pmd_format_version": 1
            }),
        )
        .with_dry_run_example(
            json!({
                "repo": "ADK",
                "title": "Preview issue",
                "background": "Check the generated markdown before creating anything.",
                "content": ["render issue body"],
                "dod": ["response includes rendered_body"],
                "agent_id": "project-agentdesk",
                "block_on": [3718],
                "dry_run": true
            }),
            json!({
                "dry_run": true,
                "issue": {
                    "number": null,
                    "url": null,
                    "repo": "itismyfield/AgentDesk"
                },
                "kanban_card_id": null,
                "kanban_card_sync_error": null,
                "announcement_channel_id": null,
                "announcement_message_id": null,
                "announcement_sync_error": null,
                "applied_labels": ["agent:project-agentdesk"],
                "rendered_body": "## 배경\nCheck the generated markdown before creating anything.\n\n## 내용\n- render issue body\n\n## 의존성\n- #3718\n\n## DoD\n- [ ] response includes rendered_body",
                "validation_warnings": [],
                "capabilities": {
                    "auto_dispatch": false,
                    "block_on": true,
                    "unsupported_features": ["auto_dispatch"]
                },
                "block_on": [3718],
                "unsupported_features": [],
                "issue_format_version": 1,
                "pmd_format_version": 1
            }),
        )
        .with_error_example(
            422,
            json!({"body": {"repo": "ADK", "title": "no DoD", "background": "bg", "content": ["item"], "dod": []}}),
            json!({"error": "dod must contain at least one item"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/github/issues/create -H 'Content-Type: application/json' -d '{\"repo\":\"ADK\",\"title\":\"Example\",\"background\":\"bg\",\"content\":[\"do thing\"],\"dod\":[\"it works\"]}'"),
        ep("POST", "/api/github/repos", "github", "Register GitHub repo").with_example(
            json!({"body": {"id": "itismyfield/AgentDesk"}}),
            json!({"repo": {"id": "itismyfield/AgentDesk", "display_name": "AgentDesk", "sync_enabled": true, "last_synced_at": null}}),
        ),
        ep(
            "POST",
            "/api/github/repos/{owner}/{repo}/sync",
            "github",
            "Synchronously fetch GitHub issues for one registered repo, triage newly discovered issues into kanban cards, reconcile closed/mainline-linked issues into card state, and update github_repos.last_synced_at. Request body: none. Requirements: the repo must already exist in github_repos (register with POST /api/github/repos or configure it), Postgres must be available, and the server host must have authenticated gh CLI access to the target repo. Auth: include Authorization: Bearer <token> when config.server.auth_token is set; local unauthenticated use is allowed only when server auth is disabled.",
        )
        .with_params([
            (
                "owner",
                path_param("GitHub owner or organization path segment, for example `itismyfield`"),
            ),
            (
                "repo",
                path_param("GitHub repository name path segment, for example `AgentDesk`"),
            ),
            (
                "Authorization",
                header_param(
                    "string",
                    false,
                    "Bearer token required when config.server.auth_token is set",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"owner": "itismyfield", "repo": "AgentDesk"}}),
            json!({
                "synced": true,
                "repo": "itismyfield/AgentDesk",
                "issues_fetched": 12,
                "cards_created": 2,
                "cards_closed": 1,
                "inconsistencies": 0
            }),
        )
        .with_error_example(
            404,
            json!({"path": {"owner": "itismyfield", "repo": "UnknownRepo"}}),
            json!({"error": "repo 'itismyfield/UnknownRepo' not registered"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/github/repos/itismyfield/AgentDesk/sync -H 'Authorization: Bearer <token>'"),
        ep(
            "GET",
            "/api/github-repos",
            "github-dashboard",
            "List GitHub repos for dashboard",
        ),
        ep(
            "GET",
            "/api/github-issues",
            "github-dashboard",
            "List GitHub issues for dashboard",
        )
        .with_example(
            json!({"query": {"repo": "itismyfield/AgentDesk", "state": "open", "limit": 20}}),
            json!({"repo": "itismyfield/AgentDesk", "issues": [{"number": 4227, "title": "Docs inventory sweep", "state": "OPEN"}]}),
        ),
        ep(
            "PATCH",
            "/api/github-issues/{owner}/{repo}/{number}/close",
            "github-dashboard",
            "Close GitHub issue from dashboard",
        )
        .with_example(
            json!({"path": {"owner": "itismyfield", "repo": "AgentDesk", "number": 4227}}),
            json!({"ok": true, "repo": "itismyfield/AgentDesk", "number": 4227}),
        ),
        ep(
            "GET",
            "/api/github-closed-today",
            "github-dashboard",
            "List issues closed today",
        ),
        ep("GET", "/api/offices", "offices", "List offices"),
        ep("POST", "/api/offices", "offices", "Create office").with_example(
            json!({"body": {"name": "Engineering", "layout": "kanban"}}),
            json!({"office": {"id": "office-1", "name": "Engineering", "layout": "kanban"}}),
        ),
        ep(
            "PATCH",
            "/api/offices/reorder",
            "offices",
            "Reorder offices",
        )
        .with_example(
            json!({"body": [{"id": "office-1", "sort_order": 1}]}),
            json!({"ok": true, "updated": 1}),
        ),
        ep("PATCH", "/api/offices/{id}", "offices", "Update office").with_example(
            json!({"path": {"id": "office-1"}, "body": {"name": "Platform", "layout": "matrix"}}),
            json!({"office": {"id": "office-1", "name": "Platform", "layout": "matrix"}}),
        ),
        ep("DELETE", "/api/offices/{id}", "offices", "Delete office").with_example(
            json!({"path": {"id": "office-1"}}),
            json!({"ok": true}),
        ),
        ep(
            "POST",
            "/api/offices/{id}/agents",
            "offices",
            "Add agent to office",
        )
        .with_example(
            json!({"path": {"id": "office-1"}, "body": {"agent_id": "project-agentdesk", "department_id": "dept-platform"}}),
            json!({"ok": true}),
        ),
        ep(
            "POST",
            "/api/offices/{id}/agents/batch",
            "offices",
            "Batch add agents to office",
        )
        .with_example(
            json!({"path": {"id": "office-1"}, "body": {"agent_ids": ["project-agentdesk", "adk-dashboard"]}}),
            json!({"ok": true}),
        ),
        ep(
            "DELETE",
            "/api/offices/{id}/agents/{agentId}",
            "offices",
            "Remove agent from office",
        )
        .with_example(
            json!({"path": {"id": "office-1", "agentId": "project-agentdesk"}}),
            json!({"ok": true}),
        ),
        ep(
            "PATCH",
            "/api/offices/{id}/agents/{agentId}",
            "offices",
            "Update office agent",
        )
        .with_example(
            json!({"path": {"id": "office-1", "agentId": "project-agentdesk"}, "body": {"department_id": "dept-platform"}}),
            json!({"ok": true}),
        ),
        ep(
            "GET",
            "/api/departments",
            "departments",
            "List departments",
        ),
        ep(
            "POST",
            "/api/departments",
            "departments",
            "Create department",
        )
        .with_example(
            json!({"body": {"name": "Platform", "office_id": "office-1"}}),
            json!({"department": {"id": "dept-platform", "name": "Platform", "office_id": "office-1"}}),
        ),
        ep(
            "PATCH",
            "/api/departments/reorder",
            "departments",
            "Reorder departments",
        )
        .with_example(
            json!({"body": {"order": [{"id": "dept-platform", "sort_order": 1}]}}),
            json!({"ok": true, "updated": 1}),
        ),
        ep(
            "PATCH",
            "/api/departments/{id}",
            "departments",
            "Update department",
        )
        .with_example(
            json!({"path": {"id": "dept-platform"}, "body": {"name": "Runtime", "office_id": "office-1"}}),
            json!({"department": {"id": "dept-platform", "name": "Runtime", "office_id": "office-1"}}),
        ),
        ep(
            "DELETE",
            "/api/departments/{id}",
            "departments",
            "Delete department",
        )
        .with_example(
            json!({"path": {"id": "dept-platform"}}),
            json!({"ok": true}),
        ),
        ep("GET", "/api/stats", "stats", "Get system stats"),
        ep(
            "GET",
            "/api/stats/memento",
            "stats",
            "Get hourly Memento logical call counts and dedup hit rates",
        )
        .with_params([(
            "hours",
            query_param("integer", false, "Trailing window size in hours (1-168)")
                .with_default(24),
        )])
        .with_example(
            json!({"query": {"hours": 24}}),
            json!({
                "window_hours": 24,
                "summary": {"logical_calls": 12, "dedup_hits": 3},
                "recall_context": {
                    "full_turns": 8,
                    "full_bytes": 4096,
                    "full_average_bytes": 512,
                    "identity_only_turns": 4,
                    "identity_only_bytes": 384,
                    "identity_only_average_bytes": 96,
                    "identity_only_empty_turns": 1,
                    "skipped_turns": 2
                }
            }),
        ),
        ep(
            "GET",
            "/api/settings",
            "settings",
            "Get the canonical company settings JSON stored in `kv_meta['settings']`",
        )
        .with_example(
            json!({}),
            json!({
                "companyName": "AgentDesk",
                "language": "ko",
                "theme": "midnight"
            }),
        ),
        ep(
            "PUT",
            "/api/settings",
            "settings",
            "Full-replace company settings JSON. Callers must send a merged payload if hidden keys should survive.",
        )
        .with_example(
            json!({
                "companyName": "AgentDesk",
                "language": "ko",
                "theme": "midnight"
            }),
            json!({"ok": true}),
        ),
        ep(
            "GET",
            "/api/settings/config",
            "settings",
            "Get editable policy/config keys with effective value, baseline, and restart-behavior metadata",
        )
        .with_example(
            json!({}),
            json!({
                "entries": [
                    {
                        "key": "merge_strategy",
                        "value": "merge",
                        "default": "rebase",
                        "baseline": "rebase",
                        "baseline_source": "yaml",
                        "override_active": true,
                        "editable": true,
                        "restart_behavior": "reseed-from-yaml",
                        "category": "automation",
                        "label_ko": "자동 머지 전략",
                        "label_en": "Merge Strategy"
                    },
                    {
                        "key": "merge_strategy_mode",
                        "value": "pr-always",
                        "default": "direct-first",
                        "baseline": "direct-first",
                        "baseline_source": "hardcoded",
                        "override_active": true,
                        "editable": true,
                        "restart_behavior": "persist-live-override",
                        "category": "automation",
                        "label_ko": "자동 머지 경로",
                        "label_en": "Merge Strategy Mode"
                    },
                    {
                        "key": "server_port",
                        "value": "8791",
                        "default": "8791",
                        "baseline": "8791",
                        "baseline_source": "config",
                        "override_active": false,
                        "editable": false,
                        "restart_behavior": "config-only",
                        "category": "system",
                        "label_ko": "서버 포트",
                        "label_en": "Server Port"
                    }
                ]
            }),
        ),
        ep(
            "PATCH",
            "/api/settings/config",
            "settings",
            "Patch live overrides for editable whitelisted config keys. YAML-backed keys are re-seeded on restart.",
        )
        .with_example(
            json!({
                "merge_strategy": "merge",
                "merge_strategy_mode": "pr-always",
                "max_review_rounds": 5
            }),
            json!({"ok": true, "updated": 2, "rejected": []}),
        ),
        ep(
            "GET",
            "/api/settings/runtime-config",
            "settings",
            "Get runtime tuning as `current` merged over YAML-or-hardcoded `defaults`",
        )
        .with_example(
            json!({}),
            json!({
                "current": {
                    "dispatchPollSec": 15,
                    "maxRetries": 7,
                    "maxEntryRetries": 4
                },
                "defaults": {
                    "dispatchPollSec": 30,
                    "maxRetries": 3,
                    "maxEntryRetries": 3
                }
            }),
        ),
        ep(
            "GET",
            "/api/settings/operator-connectors",
            "settings",
            "Get optional operator connector status and setup actions. Missing optional connectors do not block the core runtime.",
        )
        .with_example(
            json!({}),
            json!({
                "connectors": [
                    {
                        "id": "obsidian_skill_root",
                        "name": "Obsidian skill root",
                        "state": "missing_config",
                        "optional": true,
                        "env_var": "AGENTDESK_OBSIDIAN_SKILL_ROOT",
                        "source": "/Users/user/ObsidianVault/RemoteVault/99_Skills",
                        "reason": "missing_config",
                        "detail": "state=missing_config source=/Users/user/ObsidianVault/RemoteVault/99_Skills reason=missing_config",
                        "setup_actions": [
                            "Set AGENTDESK_OBSIDIAN_SKILL_ROOT to an existing skill directory containing at least one <skill>/SKILL.md, or run scripts/operator-init-portable.py --with-obsidian-stubs before syncing real skills."
                        ],
                        "capabilities": ["obsidian_skill_root"]
                    }
                ],
                "summary": {
                    "ready": 0,
                    "skipped": 0,
                    "missing_config": 1,
                    "missing_path": 0,
                    "missing_provider": 0,
                    "invalid_config": 0,
                    "invalid": 0,
                    "total": 1,
                    "core_runtime_blocking": false
                }
            }),
        ),
        ep(
            "PUT",
            "/api/settings/runtime-config",
            "settings",
            "Replace the stored runtime-config override object",
        )
        .with_example(
            json!({
                "dispatchPollSec": 15,
                "maxRetries": 7,
                "maxEntryRetries": 4
            }),
            json!({"ok": true}),
        ),
        ep(
            "GET",
            "/api/settings/escalation",
            "settings",
            "Get escalation routing defaults plus the current override-applied value",
        )
        .with_example(
            json!({}),
            json!({
                "current": {
                    "mode": "scheduled",
                    "owner_user_id": 343742347365974026u64,
                    "pm_channel_id": "kanban-manager",
                    "schedule": {
                        "pm_hours": "09:00-18:00",
                        "timezone": "Asia/Seoul"
                    }
                },
                "defaults": {
                    "mode": "pm",
                    "owner_user_id": 343742347365974026u64,
                    "pm_channel_id": "kanban-manager",
                    "schedule": {
                        "pm_hours": "00:00-08:00",
                        "timezone": "Asia/Seoul"
                    }
                }
            }),
        ),
        ep(
            "PUT",
            "/api/settings/escalation",
            "settings",
            "Replace the escalation override. Sending the default body clears the stored override.",
        )
        .with_example(
            json!({
                "mode": "scheduled",
                "owner_user_id": 343742347365974026u64,
                "pm_channel_id": "kanban-manager",
                "schedule": {
                    "pm_hours": "09:00-18:00",
                    "timezone": "Asia/Seoul"
                }
            }),
            json!({
                "ok": true,
                "current": {
                    "mode": "scheduled",
                    "owner_user_id": 343742347365974026u64,
                    "pm_channel_id": "kanban-manager",
                    "schedule": {
                        "pm_hours": "09:00-18:00",
                        "timezone": "Asia/Seoul"
                    }
                },
                "defaults": {
                    "mode": "pm",
                    "owner_user_id": 343742347365974026u64,
                    "pm_channel_id": "kanban-manager",
                    "schedule": {
                        "pm_hours": "00:00-08:00",
                        "timezone": "Asia/Seoul"
                    }
                }
            }),
        ),
        ep(
            "GET",
            "/api/voice/config",
            "settings",
            "Get dashboard-editable voice-lobby config from agentdesk.yaml with version for optimistic locking.",
        )
        .with_example(
            json!({}),
            json!({
                "global": {
                    "lobby_channel_id": "1503294653313712169",
                    "active_agent_ttl_seconds": 180,
                    "default_sensitivity_mode": "normal"
                },
                "agents": [{
                    "id": "project-agentdesk",
                    "name": "AgentDesk",
                    "name_ko": "에이전트데스크",
                    "voice_enabled": true,
                    "wake_word": "에이전트",
                    "aliases": ["ADK", "에이전트데스크"],
                    "sensitivity_mode": "normal"
                }],
                "version": "sha256",
                "source_path": "/Users/example/.adk/release/agentdesk.yaml"
            }),
        ),
        ep(
            "PUT",
            "/api/voice/config",
            "settings",
            "Replace dashboard-editable voice-lobby config. Alias collisions return 409 with conflicting agent names.",
        )
        .with_example(
            json!({
                "version": "sha256",
                "actor": "dashboard",
                "global": {
                    "lobby_channel_id": "1503294653313712169",
                    "active_agent_ttl_seconds": 180,
                    "default_sensitivity_mode": "normal"
                },
                "agents": [{
                    "id": "project-agentdesk",
                    "name": "AgentDesk",
                    "name_ko": "에이전트데스크",
                    "voice_enabled": true,
                    "wake_word": "에이전트",
                    "aliases": ["ADK"],
                    "sensitivity_mode": "normal"
                }]
            }),
            json!({
                "global": {
                    "lobby_channel_id": "1503294653313712169",
                    "active_agent_ttl_seconds": 180,
                    "default_sensitivity_mode": "normal"
                },
                "agents": [],
                "version": "next-sha256",
                "source_path": "/Users/example/.adk/release/agentdesk.yaml"
            }),
        )
        .with_error_example(
            409,
            json!({
                "version": "stale",
                "global": {
                    "lobby_channel_id": "1503294653313712169",
                    "active_agent_ttl_seconds": 180,
                    "default_sensitivity_mode": "normal"
                },
                "agents": []
            }),
            json!({
                "error": "alias_conflict",
                "message": "voice alias collision",
                "conflict": {
                    "normalized": "adk",
                    "first_agent_name": "AgentDesk",
                    "second_agent_name": "Another Agent"
                }
            }),
        ),
        ep(
            "GET",
            "/api/dispatched-sessions",
            "dispatched-sessions",
            "List dispatched sessions with recovery identifiers for active dispatch, tmux session, thread channel, and linked auto-queue entry/run/slot when available.",
        )
        .with_example(
            json!({"query": {"all": false}}),
            json!({"sessions": [{
                "session_key": "mac-mini:AgentDesk-codex-adk-cdx",
                "status": "turn_active",
                "active_dispatch_id": "dispatch-1",
                "tmux_session": "AgentDesk-codex-adk-cdx",
                "resolved_thread_channel_id": "1501205715878936748",
                "auto_queue_entry_id": "entry-1",
                "auto_queue_run_id": "run-1",
                "auto_queue_slot_index": 0,
                "recovery_identifiers": {
                    "session_key": "mac-mini:AgentDesk-codex-adk-cdx",
                    "tmux_session": "AgentDesk-codex-adk-cdx",
                    "active_dispatch_id": "dispatch-1",
                    "thread_channel_id": "1501205715878936748",
                    "auto_queue_entry_id": "entry-1",
                    "auto_queue_run_id": "run-1",
                    "auto_queue_slot_index": 0,
                    "auto_queue_thread_group": 0
                }
            }]}),
        ),
        ep(
            "DELETE",
            "/api/dispatched-sessions/cleanup",
            "dispatched-sessions",
            "Delete stale dispatched sessions",
        )
        .with_example(
            json!({}),
            json!({"ok": true, "deleted": 2}),
        )
    ]
}
