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
                    "Optional dependency references rendered into `## 의존성`; auto-queue generate holds the card until the issues named there (#N, owner/repo#N or an issue URL) are done",
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
            "Synchronously fetch GitHub issues for one registered repo, triage newly discovered issues into kanban cards, reconcile closed/mainline-linked issues into card state, make linked campaign nodes follow their issues, and update github_repos.last_synced_at. `campaign_nodes_failed` is true when the campaign step could not load campaigns or read issues from GitHub; `deferred_nodes` only waited for a fresher read. Request body: none. Requirements: the repo must already exist in github_repos (register with POST /api/github/repos or configure it), Postgres must be available, and the server host must have authenticated gh CLI access to the target repo. Auth: include Authorization: Bearer <token> when config.server.auth_token is set; local unauthenticated use is allowed only when server auth is disabled.",
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
                "inconsistencies": 0,
                "campaign_nodes": {"changed_nodes": 1, "deferred_nodes": 0, "errors": 0, "fetch_errors": 0},
                "campaign_nodes_failed": false
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
            json!({"body": {"name": "Platform"}}),
            json!({"department": {"id": "dept-platform", "name": "Platform"}}),
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
            json!({"path": {"id": "dept-platform"}, "body": {"name": "Runtime"}}),
            json!({"department": {"id": "dept-platform", "name": "Runtime"}}),
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
                "searchObservability": {
                    "feedback_counts_by_trigger_type": {"voluntary": 5, "automatic": 2}
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
                "max_review_rounds": 5
            }),
            json!({"ok": true, "updated": 1, "rejected": []}),
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
                    "rateLimitStaleSec": 300,
                    "maxRetries": 7,
                    "maxEntryRetries": 4
                },
                "defaults": {
                    "rateLimitStaleSec": 600,
                    "maxRetries": 3,
                    "maxEntryRetries": 3
                },
                "explicit_keys": ["maxEntryRetries", "maxRetries", "rateLimitStaleSec"]
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
            "Update runtime-config: metadata-less PUT preserves omitted explicit overrides and promotes submitted known keys; reserved metadata is an exact replacement",
        )
        .with_example(
            json!({
                "rateLimitStaleSec": 300,
                "maxRetries": 7,
                "maxEntryRetries": 4
            }),
            json!({"ok": true}),
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
