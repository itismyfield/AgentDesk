use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "GET",
            "/api/doctor/startup/latest",
            "health",
            "Local/protected latest startup doctor artifact envelope for agent rescue and diagnosis.",
        )
        .with_example(
            json!({}),
            json!({
                "ok": true,
                "available": true,
                "artifact_path": "/Users/kunkun/.adk/release/runtime/doctor/startup/123-456.json",
                "detail_source": "startup_doctor_artifact",
                "followup_context": "restart_followup",
                "summary": {"passed": 21, "warned": 3, "failed": 1, "total": 25},
                "artifact": {"schema_version": 1, "boot_id": "123-456", "checks": []}
            }),
        )
        .with_error_example(
            200,
            json!({}),
            json!({"ok": true, "available": false, "artifact_path": null, "reason": "startup_doctor_artifact_missing", "artifact": null}),
        )
        .with_curl("curl http://localhost:8787/api/doctor/startup/latest"),
        ep(
            "POST",
            "/api/doctor/stale-mailbox/repair",
            "health",
            "Local/protected stale mailbox repair endpoint used by doctor follow-up workflows.",
        )
        .with_params([
            ("channel_id", body_param("integer", true, "Discord channel snowflake")),
            (
                "provider",
                body_param("string", false, "Optional provider filter such as claude or codex"),
            ),
            (
                "expected_has_cancel_token",
                body_param("boolean", false, "Optional guard for the observed mailbox token state"),
            ),
            (
                "purge",
                body_param("boolean", false, "Default false. After a fully applied repair, also unlink the channel's idle in-memory mailbox registry entry (no disk/DB mutation; refused while live work evidence exists)"),
            ),
        ])
        .with_example(
            json!({"body": {"channel_id": "1486017489027469493", "provider": "claude", "expected_has_cancel_token": true, "purge": true}}),
            json!({"ok": true, "applied": true, "registry_entry_removed": true, "registry_purge_skipped_reason": null}),
        )
        .with_error_example(
            403,
            json!({"body": {"channel_id": "1486017489027469493"}}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/doctor/stale-mailbox/repair -H 'Content-Type: application/json' -d '{\"channel_id\":1486017489027469493}'"),
        ep(
            "POST",
            "/api/channels/{id}/relay-recovery",
            "health",
            "Local/protected relay recovery dry-run endpoint with bounded apply for safe local auto-heal actions, including stale proof cleanup and detached watcher reattach.",
        )
        .with_params([
            ("id", path_param("Discord channel snowflake")),
            (
                "provider",
                body_param("string", false, "Optional provider filter such as codex"),
            ),
            (
                "apply",
                body_param("boolean", false, "Default false. When true, only eligible bounded local cleanup or watcher reattach may run"),
            ),
        ])
        .with_example(
            json!({"body": {"provider": "codex", "apply": false}}),
            json!({
                "ok": true,
                "mode": "dry_run",
                "applied": false,
                "skipped": false,
                "decision": {
                    "relay_stall_state": "orphan_pending_token",
                    "action": "clear_orphan_pending_token",
                    "reason": "mailbox holds a cancel token without bridge, watcher, or live tmux evidence",
                    "evidence": {"mailbox_has_cancel_token": true, "bridge_inflight_present": false, "watcher_attached": false},
                    "affected": {"channel_id": "1486017489027469493", "provider": "codex"},
                    "auto_heal": {"eligible": true, "bounded": true, "max_attempts_per_window": 1, "window_secs": 600}
                }
            }),
        )
        .with_error_example(
            403,
            json!({"body": {"provider": "codex"}}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/channels/1486017489027469493/relay-recovery -H 'Content-Type: application/json' -d '{\"provider\":\"codex\",\"apply\":false}'"),
        ep(
            "POST",
            "/api/discord/send",
            "discord",
            "Send a Discord channel message",
        )
        .with_params([
            (
                "target",
                body_param("string", false, "Target channel:<id>|channel:<name>|agent:<roleId>"),
            ),
            (
                "content",
                body_param("string", false, "Message body (markdown supported)"),
            ),
            (
                "channel_id",
                body_param("string", false, "Alias for target=channel:<id>"),
            ),
            ("message", body_param("string", false, "Alias for content")),
            (
                "source",
                body_param("string", false, "Source label allowed for the caller class: CLI uses agentdesk-cli/operator, dashboard uses dashboard or an agent role_id, and internal labels such as system/headless_turn require a loopback internal caller"),
            ),
            (
                "bot",
                body_param("string", false, "Delivery bot: announce (default) or notify"),
            ),
            (
                "X-AgentDesk-Source",
                header_param(
                    "string",
                    false,
                    "Caller class attestation: cli, dashboard, or loopback/internal",
                ),
            ),
        ])
        .with_example(
            json!({"body": {"target": "channel:1473922824350601297", "content": "hello", "source": "operator", "bot": "notify"}}),
            json!({"ok": true, "message_id": "1500000000000000000"}),
        )
        .with_error_example(
            400,
            json!({"body": {"target": "channel:1473922824350601297"}}),
            json!({"error": "content is required", "ok": false}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/discord/send -H 'Content-Type: application/json' -H 'X-AgentDesk-Source: cli' -d '{\"target\":\"channel:1473922824350601297\",\"content\":\"hello\",\"source\":\"operator\",\"bot\":\"notify\"}'"),
        ep(
            "POST",
            "/api/discord/bot-tokens/reload",
            "discord",
            "Reload announce/notify Discord utility bot tokens",
        )
        .with_example(
            json!({}),
            json!({
                "ok": true,
                "status": "reloaded",
                "report": {
                    "announce": {
                        "bot": "announce",
                        "credential": "credential/announce_bot_token",
                        "status": "reloaded",
                        "reloaded": true,
                        "previous_client_kept": false,
                        "user_id_cache_invalidated": true
                    },
                    "notify": {
                        "bot": "notify",
                        "credential": "credential/notify_bot_token",
                        "status": "reloaded",
                        "reloaded": true,
                        "previous_client_kept": false,
                        "user_id_cache_invalidated": true
                    },
                    "runtime_root_available": true,
                    "any_reloaded": true,
                    "utility_bot_user_ids_invalidated": true,
                    "scopes": {
                        "utility_rest_clients": {
                            "scope": "utility_rest_clients",
                            "status": "reload_supported",
                            "live_reload_supported": true,
                            "restart_required": false,
                            "token_source": "credential/announce_bot_token and credential/notify_bot_token",
                            "detail": "POST /api/discord/bot-tokens/reload rebuilds announce/notify HealthRegistry REST clients in place."
                        },
                        "provider_runtime_cached_token": {
                            "scope": "provider_runtime_cached_token",
                            "status": "restart_required",
                            "live_reload_supported": false,
                            "restart_required": true,
                            "token_source": "discord.bots.<name>.token or credential/<name>_bot_token selected at provider runtime startup",
                            "detail": "SharedData.cached_bot_token is a OnceCell per provider runtime, so rotated provider REST fallback credentials are not adopted until dcserver restarts."
                        },
                        "provider_gateway_session": {
                            "scope": "provider_gateway_session",
                            "status": "restart_required",
                            "live_reload_supported": false,
                            "restart_required": true,
                            "token_source": "discord.bots.<name>.token or credential/<name>_bot_token selected at provider runtime startup",
                            "detail": "Discord gateway sessions are created by provider runtimes at startup; reconnecting them with a rotated token requires a dcserver restart."
                        }
                    },
                    "provider_cached_bot_token_scope": "announce/notify HealthRegistry clients are reloaded; provider runtime SharedData.cached_bot_token is restart-only"
                }
            }),
        )
        .with_error_example(
            403,
            json!({}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl -X POST http://127.0.0.1:8787/api/discord/bot-tokens/reload"),
        ep(
            "POST",
            "/api/discord/send-to-agent",
            "discord",
            "Send a Discord message by agent role_id",
        )
        .with_params([
            ("role_id", body_param("string", true, "Target agent role_id")),
            ("message", body_param("string", true, "Discord message content")),
            (
                "mode",
                body_param(
                    "string",
                    false,
                    "Delivery bot: announce (default) or notify",
                )
                .with_enum(&["announce", "notify"]),
            ),
        ])
        .with_example(
            json!({"body": {"role_id": "project-agentdesk", "message": "deploy done", "mode": "announce"}}),
            json!({"ok": true, "channel_id": "1473922824350601297", "message_id": "1500000000000000001"}),
        )
        .with_error_example(
            404,
            json!({"body": {"role_id": "ghost-agent", "message": "hi"}}),
            json!({"error": "agent not found: ghost-agent"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/discord/send-to-agent -H 'Content-Type: application/json' -d '{\"role_id\":\"project-agentdesk\",\"message\":\"deploy done\"}'"),
        ep(
            "POST",
            "/api/discord/send-dm",
            "discord",
            "Send a Discord direct message",
        )
        .with_params([
            ("user_id", body_param("string", true, "Target Discord user snowflake")),
            ("message", body_param("string", true, "DM body")),
        ])
        .with_example(
            json!({"body": {"user_id": "100000000000000000", "message": "heads-up"}}),
            json!({"ok": true, "message_id": "1500000000000000002"}),
        )
        .with_error_example(
            400,
            json!({"body": {"message": "heads-up"}}),
            json!({"error": "user_id is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/discord/send-dm -H 'Content-Type: application/json' -d '{\"user_id\":\"100000000000000000\",\"message\":\"heads-up\"}'"),
        ep("GET", "/api/agents", "agents", "List all agents")
            .with_example(
                json!({}),
                json!({"agents": [{"id": "project-agentdesk", "name": "AgentDesk", "discord_channel_id": "1473922824350601297"}]}),
            )
            .with_error_example(
                500,
                json!({}),
                json!({"error": "internal: failed to load agents"}),
            )
            .with_curl("curl http://localhost:8787/api/agents"),
        ep("POST", "/api/agents", "agents", "Create an agent")
            .with_params([
                ("id", body_param("string", true, "New agent id (role_id)")),
                ("name", body_param("string", true, "Display name")),
                ("discord_channel_id", body_param("string", false, "Primary Discord channel")),
            ])
            .with_example(
                json!({"body": {"id": "pm-planner", "name": "PM Planner", "discord_channel_id": "1473922824350601297"}}),
                json!({"agent": {"id": "pm-planner", "name": "PM Planner", "discord_channel_id": "1473922824350601297"}}),
            )
            .with_error_example(
                409,
                json!({"body": {"id": "project-agentdesk", "name": "dup"}}),
                json!({"error": "agent id already exists: project-agentdesk"}),
            )
            .with_curl("curl -X POST http://localhost:8787/api/agents -H 'Content-Type: application/json' -d '{\"id\":\"pm-planner\",\"name\":\"PM Planner\"}'"),
        ep("GET", "/api/agents/{id}", "agents", "Get agent by ID")
            .with_params([("id", path_param("Agent id"))])
            .with_example(
                json!({"path": {"id": "project-agentdesk"}}),
                json!({"agent": {"id": "project-agentdesk", "name": "AgentDesk", "discord_channel_id": "1473922824350601297"}}),
            )
            .with_error_example(
                404,
                json!({"path": {"id": "ghost"}}),
                json!({"error": "agent not found: ghost"}),
            )
            .with_curl("curl http://localhost:8787/api/agents/project-agentdesk"),
        ep("PATCH", "/api/agents/{id}", "agents", "Update agent metadata and prompt content")
            .with_params([
                ("id", path_param("Agent id")),
                ("name", body_param("string", false, "Display name")),
                ("department_id", body_param("string", false, "Department id")),
                ("prompt_content", body_param("string", false, "Full prompt markdown content to rewrite")),
                ("auto_commit", body_param("boolean", false, "Commit prompt rewrite with git when available").with_default(false)),
            ])
            .with_example(
                json!({"path": {"id": "project-agentdesk"}, "body": {"name": "AgentDesk", "prompt_content": "# role\n...", "auto_commit": false}}),
                json!({"agent": {"id": "project-agentdesk"}, "prompt": {"changed": true}}),
            )
            .with_error_example(
                404,
                json!({"path": {"id": "ghost"}, "body": {"name": "x"}}),
                json!({"error": "agent not found: ghost"}),
            )
            .with_curl("curl -X PATCH http://localhost:8787/api/agents/project-agentdesk -H 'Content-Type: application/json' -d '{\"name\":\"AgentDesk\"}'"),
        ep("DELETE", "/api/agents/{id}", "agents", "Delete agent").with_example(
            json!({"path": {"id": "project-agentdesk"}}),
            json!({"ok": true}),
        ),
        ep(
            "POST",
            "/api/agents/{id}/duplicate",
            "agents",
            "Duplicate an agent by reusing /api/agents/setup with the source prompt as template.",
        )
        .with_params([
            ("id", path_param("Source agent id")),
            ("new_agent_id", body_param("string", true, "New role id")),
            ("channel_id", body_param("string", true, "Existing Discord channel snowflake")),
            ("provider", body_param("string", false, "Provider override")),
            ("dry_run", body_param("boolean", false, "Preview setup mutations only").with_default(false)),
        ])
        .with_example(
            json!({"path": {"id": "project-agentdesk"}, "body": {"new_agent_id": "project-agentdesk-copy", "channel_id": "1473922824350601297", "provider": "codex", "dry_run": true}}),
            json!({"ok": true, "duplicate": true, "source_agent_id": "project-agentdesk", "new_agent_id": "project-agentdesk-copy", "setup": {"dry_run": true}}),
        ),
        ep(
            "GET",
            "/api/onboarding/status",
            "onboarding",
            "Get onboarding status",
        ),
        ep(
            "GET",
            "/api/onboarding/draft",
            "onboarding",
            "Get onboarding resume draft",
        ),
        ep(
            "PUT",
            "/api/onboarding/draft",
            "onboarding",
            "Persist onboarding resume draft",
        )
        .with_example(
            json!({"body": {"version": 1, "step": 2, "selected_guild": "1490141479707086938", "agents": []}}),
            json!({"ok": true, "available": true, "secret_policy": "redacted"}),
        ),
        ep(
            "DELETE",
            "/api/onboarding/draft",
            "onboarding",
            "Clear onboarding resume draft",
        )
        .with_example(
            json!({}),
            json!({"ok": true, "available": false, "secret_policy": "redacted"}),
        ),
        ep(
            "POST",
            "/api/onboarding/validate-token",
            "onboarding",
            "Validate onboarding token",
        )
        .with_example(
            json!({"body": {"token": "discord-bot-token"}}),
            json!({"valid": true, "bot_id": "123456789012345678", "bot_name": "agentdesk-bot", "avatar": null}),
        ),
        ep(
            "GET",
            "/api/onboarding/channels",
            "onboarding",
            "List onboarding candidate channels",
        ),
        ep(
            "POST",
            "/api/onboarding/channels",
            "onboarding",
            "Persist onboarding channel selection",
        )
        .with_example(
            json!({"body": {"token": "discord-bot-token"}}),
            json!({"guilds": [{"id": "1490141479707086938", "name": "AgentDesk"}], "channels": [{"id": "1473922824350601297", "name": "agentdesk"}]}),
        ),
        ep(
            "POST",
            "/api/onboarding/complete",
            "onboarding",
            "Complete onboarding",
        )
        .with_example(
            json!({"body": {"token": "discord-bot-token", "guild_id": "1490141479707086938", "provider": "codex", "channels": [{"channel_id": "1473922824350601297", "channel_name": "agentdesk", "role_id": "project-agentdesk"}]}}),
            json!({"ok": true, "provider": "codex", "rerun_policy": "safe"}),
        ),
        ep(
            "POST",
            "/api/onboarding/check-provider",
            "onboarding",
            "Validate provider installation and credentials",
        )
        .with_example(
            json!({"body": {"provider": "codex"}}),
            json!({"installed": true, "logged_in": true, "version": "codex 1.0.0", "path": "/usr/local/bin/codex"}),
        ),
        ep(
            "POST",
            "/api/onboarding/generate-prompt",
            "onboarding",
            "Generate onboarding prompt",
        )
        .with_example(
            json!({"body": {"name": "Docs Agent", "description": "Maintains API docs", "provider": "codex"}}),
            json!({"prompt": "You maintain concise API documentation."}),
        ),
        ep(
            "POST",
            "/api/agents/setup",
            "agents",
            "Atomically create an agent config binding, prompt file, workspace seed, DB row, and optional skill workspace mapping. Supports dry_run planning and rollback on partial failure.",
        )
        .with_params([
            ("agent_id", body_param("string", true, "New agent id")),
            (
                "channel_id",
                body_param("string", true, "Existing Discord channel snowflake"),
            ),
            (
                "provider",
                body_param("string", true, "Provider for the agent channel")
                    .with_enum(&["claude", "codex", "gemini", "opencode", "qwen"]),
            ),
            (
                "prompt_template_path",
                body_param(
                    "string",
                    true,
                    "Prompt template path, usually config/agents/_shared.prompt.md",
                ),
            ),
            (
                "skills",
                body_param("array", false, "Managed skill ids to map to the new workspace"),
            ),
            (
                "dry_run",
                body_param("boolean", false, "Validate and return planned mutations only")
                    .with_default(false),
            ),
        ])
        .with_example(
            json!({
                "body": {
                    "agent_id": "project-agentdesk",
                    "channel_id": "1473922824350601297",
                    "provider": "codex",
                    "prompt_template_path": "config/agents/_shared.prompt.md",
                    "skills": ["memory-read"],
                    "dry_run": true
                }
            }),
            json!({
                "ok": true,
                "dry_run": true,
                "created": [],
                "rolled_back": [],
                "errors": [],
            }),
        )
        .with_error_example(
            400,
            json!({"body": {"agent_id": "x", "channel_id": "1473922824350601297", "provider": "unknown", "prompt_template_path": "config/agents/_shared.prompt.md"}}),
            json!({"error": "provider must be one of claude|codex|gemini|opencode|qwen"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/setup -H 'Content-Type: application/json' -d '{\"agent_id\":\"project-agentdesk\",\"channel_id\":\"1473922824350601297\",\"provider\":\"codex\",\"prompt_template_path\":\"config/agents/_shared.prompt.md\",\"dry_run\":true}'"),
        ep(
            "GET",
            "/api/agents/{id}/offices",
            "agents",
            "List offices for agent",
        ),
        ep(
            "POST",
            "/api/agents/{id}/signal",
            "agents",
            "Send runtime signal to agent",
        )
        .with_example(
            json!({"path": {"id": "project-agentdesk"}, "body": {"signal": "blocked", "reason": "waiting on review"}}),
            json!({"ok": true, "card_id": "card-1", "signal": "blocked"}),
        ),
        ep(
            "POST",
            "/api/agents/{id}/message",
            "agents",
            "Send a trigger-capable agent handoff via the announce bot",
        )
        .with_params([
            ("id", path_param("Target agent id")),
            ("from_agent_id", body_param("string", true, "Source agent id")),
            ("message", body_param("string", true, "Message body")),
            (
                "channel_kind",
                body_param("string", false, "Target binding: cc (default) or cdx")
                    .with_enum(&["cc", "cdx"])
                    .with_default(json!("cc")),
            ),
            (
                "prefix",
                body_param("boolean", false, "Add the handoff prefix").with_default(true),
            ),
            (
                "expect_reply",
                body_param(
                    "boolean",
                    false,
                    "Reply-expectation contract appended to the body: true → 회신 필수, false → 회신 불필요, omitted → no contract",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "adk-dashboard"}, "body": {"from_agent_id": "project-agentdesk", "message": "hello", "channel_kind": "cc", "prefix": true, "expect_reply": true}}),
            json!({"to_agent_id": "adk-dashboard", "channel_id": "1473922824350601297", "channel_kind": "cc", "message_id": "1500000000000000002", "bot": "announce", "prefixed": true}),
        )
        .with_error_example(
            422,
            json!({"path": {"id": "ghost-agent"}, "body": {"from_agent_id": "project-agentdesk", "message": "hello", "channel_kind": "cc"}}),
            json!({"error": "channel_kind unset", "to_agent_id": "ghost-agent", "channel_kind": "cc", "available_kinds": []}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/adk-dashboard/message -H 'Content-Type: application/json' -d '{\"from_agent_id\":\"project-agentdesk\",\"message\":\"hello\",\"channel_kind\":\"cc\",\"prefix\":true,\"expect_reply\":true}'"),
        ep(
            "POST",
            "/api/agents/{id}/handoff",
            "agents",
            "#3556 — agent-to-agent turn-trigger handoff. Reserves a headless turn on the target's cc/cdx mailbox instead of posting an announce message, so the receiving agent is authoritatively woken. Returns 409 when a turn is already active for that mailbox (a semantic the announce-only /message path does not have). Use /message for human-readable notifications and /handoff for execution intent.",
        )
        .with_params([
            ("id", path_param("Target agent id")),
            ("from_agent_id", body_param("string", true, "Source agent id")),
            ("prompt", body_param("string", true, "Instruction to execute in the handoff turn")),
            (
                "channel_kind",
                body_param("string", false, "Target mailbox: cc (default) or cdx")
                    .with_enum(&["cc", "cdx"])
                    .with_default(json!("cc")),
            ),
            (
                "prefix",
                body_param("boolean", false, "Add the from->to handoff prefix to the prompt").with_default(true),
            ),
            (
                "expect_reply",
                body_param(
                    "boolean",
                    false,
                    "Reply-expectation contract appended to the prompt: true → 회신 필수, false → 회신 불필요, omitted → no contract",
                ),
            ),
            (
                "source",
                body_param("string", false, "Optional trigger source label"),
            ),
            (
                "metadata",
                body_param("object", false, "Optional trigger metadata injected into the turn context"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "adk-dashboard"}, "body": {"from_agent_id": "project-agentdesk", "prompt": "리뷰 코멘트 반영해서 PR 업데이트해줘", "channel_kind": "cc", "expect_reply": true, "source": "agent-relay"}}),
            json!({"ok": true, "to_agent_id": "adk-dashboard", "channel_id": "1473922824350601297", "channel_kind": "cc", "turn_id": "discord:1473922824350601297:9100000000000000000", "status": "started"}),
        )
        .with_error_example(
            409,
            json!({"path": {"id": "adk-dashboard"}, "body": {"from_agent_id": "project-agentdesk", "prompt": "do it"}}),
            json!({"error": "turn already active for this agent mailbox", "status": "conflict"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/adk-dashboard/handoff -H 'Content-Type: application/json' -d '{\"from_agent_id\":\"project-agentdesk\",\"prompt\":\"리뷰 반영해줘\",\"channel_kind\":\"cc\",\"expect_reply\":true}'"),
        ep(
            "GET",
            "/api/agents/{id}/cron",
            "agents",
            "List cron jobs for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/skills",
            "agents",
            "List skills for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/dispatched-sessions",
            "agents",
            "List dispatched sessions for agent. Rows are de-duplicated by \
             (channel_id, agent_id) so the same agent never appears twice for \
             the same Discord channel even when stale provider snapshots \
             linger. Each row carries Discord deeplink fields the dashboard \
             can drop straight into an anchor `href`: `channel_id`, \
             `deeplink_url` (web — https://discord.com/channels/{guild}/{channel}), \
             plus thread aliases `thread_id` and `thread_deeplink_url` \
             (Discord app — discord://discord.com/channels/{guild}/{channel}). \
             Legacy fields `thread_channel_id`, `channel_web_url`, \
             `channel_deeplink_url` are preserved for backwards compatibility \
             with existing dashboard code paths.",
        )
        .with_params([("id", path_param("Agent id"))])
        .with_example(
            json!({"path": {"id": "project-agentdesk"}}),
            json!({
                "sessions": [
                    {
                        "id": 42,
                        "session_key": "mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011",
                        "agent_id": "project-agentdesk",
                        "provider": "codex",
                        "status": "working",
                        "active_dispatch_id": "dispatch-1",
                        "model": null,
                        "tokens": 0,
                        "cwd": null,
                        "last_heartbeat": "2026-04-27T12:34:56+00:00",
                        "thread_channel_id": "1485506232256168011",
                        "channel_id": "1485506232256168011",
                        "thread_id": "1485506232256168011",
                        "guild_id": "1490141479707086938",
                        "channel_web_url": "https://discord.com/channels/1490141479707086938/1485506232256168011",
                        "channel_deeplink_url": "discord://discord.com/channels/1490141479707086938/1485506232256168011",
                        "deeplink_url": "https://discord.com/channels/1490141479707086938/1485506232256168011",
                        "thread_deeplink_url": "discord://discord.com/channels/1490141479707086938/1485506232256168011",
                        "kanban_card_id": null
                    }
                ]
            }),
        ),
        ep(
            "GET",
            "/api/agents/{id}/turn",
            "agents",
            "Get active turn status and recent output",
        )
    ]
}
