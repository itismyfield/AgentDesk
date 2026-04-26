# AgentDesk Architecture Guide

High-signal navigation guide for contributors. The generated inventories under `docs/generated/` and the `src/` snapshot below are the authoritative structure references. Regenerate them with `python3 scripts/generate_inventory_docs.py`.

## Repository Map

- `src/` — runtime code: CLI, HTTP server, Discord bot, orchestration, policy engine, persistence.
- `policies/` — JavaScript lifecycle hooks loaded by `src/engine`.
- `dashboard/` — React/Vite UI for the web dashboard.
- `docs/generated/module-inventory.md` — generated Rust module inventory.
- `docs/generated/route-inventory.md` — generated HTTP/WebSocket route inventory.
- `docs/generated/worker-inventory.md` — generated supervised worker inventory.

Worktree builds expect `sccache` on `PATH` via `.cargo/config.toml`; install it with `brew install sccache`, and override the documented `SCCACHE_CACHE_SIZE=10G` default only when a host needs a different local cache cap.

## Generated `src/` Tree

This block is generated from the filesystem and is checked in CI for drift.

<!-- BEGIN GENERATED: SRC TREE -->
```text
src/
├── cli/
│   ├── doctor/
│   │   ├── contract.rs
│   │   ├── health.rs
│   │   ├── mailbox.rs
│   │   ├── orchestrator.rs
│   │   └── startup.rs
│   ├── migrate/
│   │   ├── apply.rs
│   │   ├── plan.rs
│   │   ├── postgres_cutover.rs
│   │   ├── source.rs
│   │   └── tests.rs
│   ├── args.rs
│   ├── client.rs
│   ├── dcserver.rs
│   ├── direct.rs
│   ├── discord.rs
│   ├── doctor.rs
│   ├── init.rs
│   ├── migrate.rs
│   ├── mod.rs
│   ├── monitoring.rs
│   ├── run.rs
│   └── utils.rs
├── compat/
│   ├── deprecated_alias.rs
│   ├── legacy_tmp_paths.rs
│   └── mod.rs
├── db/
│   ├── agents.rs
│   ├── auto_queue.rs
│   ├── cancel_tombstones.rs
│   ├── kanban.rs
│   ├── memento_feedback_stats.rs
│   ├── mod.rs
│   ├── postgres.rs
│   ├── schema.rs
│   ├── session_agent_resolution.rs
│   ├── session_transcripts.rs
│   ├── table_metadata.rs
│   └── turns.rs
├── dispatch/
│   ├── dispatch_channel.rs
│   ├── dispatch_context.rs
│   ├── dispatch_create.rs
│   ├── dispatch_status.rs
│   └── mod.rs
├── engine/
│   ├── ops/
│   │   ├── agent_ops.rs
│   │   ├── auto_queue_ops.rs
│   │   ├── cards_ops.rs
│   │   ├── ci_recovery_ops.rs
│   │   ├── config_ops.rs
│   │   ├── db_ops.rs
│   │   ├── dispatch_ops.rs
│   │   ├── dm_reply_ops.rs
│   │   ├── exec_ops.rs
│   │   ├── http_ops.rs
│   │   ├── kanban_ops.rs
│   │   ├── kv_ops.rs
│   │   ├── log_ops.rs
│   │   ├── message_ops.rs
│   │   ├── pipeline_ops.rs
│   │   ├── quality_ops.rs
│   │   ├── queue_ops.rs
│   │   ├── review_automation_ops.rs
│   │   ├── review_ops.rs
│   │   ├── runtime_ops.rs
│   │   └── tests.rs
│   ├── hooks.rs
│   ├── intent.rs
│   ├── loader.rs
│   ├── mod.rs
│   ├── ops.rs
│   ├── sql_guard.rs
│   ├── transition.rs
│   └── transition_executor_pg.rs
├── github/
│   ├── dod.rs
│   ├── mod.rs
│   ├── sync.rs
│   └── triage.rs
├── integration_tests/
│   ├── discord_flow/
│   │   ├── harness.rs
│   │   ├── mock_discord.rs
│   │   ├── mod.rs
│   │   └── scenarios.rs
│   ├── tests/
│   │   └── high_risk_recovery.rs
│   └── agents_setup_e2e.rs
├── runtime_layout/
│   ├── config_merge.rs
│   ├── legacy_migration.rs
│   ├── mod.rs
│   ├── paths.rs
│   └── skill_sync.rs
├── server/
│   ├── routes/
│   │   ├── dispatches/
│   │   │   ├── crud.rs
│   │   │   ├── discord_delivery.rs
│   │   │   ├── mod.rs
│   │   │   ├── outbox.rs
│   │   │   ├── tests.rs
│   │   │   └── thread_reuse.rs
│   │   ├── domains/
│   │   │   ├── access.rs
│   │   │   ├── admin.rs
│   │   │   ├── agents.rs
│   │   │   ├── integrations.rs
│   │   │   ├── kanban.rs
│   │   │   ├── mod.rs
│   │   │   ├── onboarding.rs
│   │   │   ├── ops.rs
│   │   │   └── reviews.rs
│   │   ├── review_verdict/
│   │   │   ├── decision_route.rs
│   │   │   ├── mod.rs
│   │   │   ├── review_state_repo.rs
│   │   │   ├── tests.rs
│   │   │   ├── tuning_aggregate.rs
│   │   │   └── verdict_route.rs
│   │   ├── agents.rs
│   │   ├── agents_crud.rs
│   │   ├── agents_setup.rs
│   │   ├── analytics.rs
│   │   ├── auth.rs
│   │   ├── auto_queue.rs
│   │   ├── cron_api.rs
│   │   ├── departments.rs
│   │   ├── discord.rs
│   │   ├── dispatched_sessions.rs
│   │   ├── dm_reply.rs
│   │   ├── docs.rs
│   │   ├── escalation.rs
│   │   ├── github.rs
│   │   ├── github_dashboard.rs
│   │   ├── health_api.rs
│   │   ├── home_metrics.rs
│   │   ├── hooks.rs
│   │   ├── kanban.rs
│   │   ├── kanban_repos.rs
│   │   ├── maintenance.rs
│   │   ├── meetings.rs
│   │   ├── memory_api.rs
│   │   ├── messages.rs
│   │   ├── mod.rs
│   │   ├── monitoring.rs
│   │   ├── offices.rs
│   │   ├── onboarding.rs
│   │   ├── pipeline.rs
│   │   ├── queue_api.rs
│   │   ├── receipt.rs
│   │   ├── resume.rs
│   │   ├── reviews.rs
│   │   ├── routes_tests.rs
│   │   ├── session_activity.rs
│   │   ├── settings.rs
│   │   ├── skill_usage_analytics.rs
│   │   ├── skills_api.rs
│   │   ├── stats.rs
│   │   ├── termination_events.rs
│   │   └── v1.rs
│   ├── cron_catalog.rs
│   ├── maintenance.rs
│   ├── mod.rs
│   ├── state.rs
│   ├── worker_registry.rs
│   └── ws.rs
├── services/
│   ├── agent_quality/
│   │   ├── mod.rs
│   │   └── regression_alerts.rs
│   ├── auto_queue/
│   │   ├── cancel_run.rs
│   │   └── runtime.rs
│   ├── discord/
│   │   ├── commands/
│   │   │   ├── command_policy.rs
│   │   │   ├── config.rs
│   │   │   ├── control.rs
│   │   │   ├── diagnostics.rs
│   │   │   ├── fast_mode.rs
│   │   │   ├── help.rs
│   │   │   ├── meeting_cmd.rs
│   │   │   ├── mod.rs
│   │   │   ├── model_picker.rs
│   │   │   ├── model_ui.rs
│   │   │   ├── receipt.rs
│   │   │   ├── restart.rs
│   │   │   ├── session.rs
│   │   │   ├── skill.rs
│   │   │   └── text_commands.rs
│   │   ├── outbound/
│   │   │   ├── decision.rs
│   │   │   ├── legacy.rs
│   │   │   ├── message.rs
│   │   │   ├── mod.rs
│   │   │   ├── policy.rs
│   │   │   └── result.rs
│   │   ├── recovery_paths/
│   │   │   ├── mod.rs
│   │   │   └── shared.rs
│   │   ├── router/
│   │   │   ├── control_intent.rs
│   │   │   ├── intake_gate.rs
│   │   │   ├── message_handler.rs
│   │   │   ├── mod.rs
│   │   │   ├── tests.rs
│   │   │   └── thread_binding.rs
│   │   ├── settings/
│   │   │   ├── content.rs
│   │   │   ├── memory.rs
│   │   │   ├── read.rs
│   │   │   ├── validation.rs
│   │   │   └── write.rs
│   │   ├── turn_bridge/
│   │   │   ├── completion_guard.rs
│   │   │   ├── context_window.rs
│   │   │   ├── memory_lifecycle.rs
│   │   │   ├── mod.rs
│   │   │   ├── recall_feedback.rs
│   │   │   ├── recovery_text.rs
│   │   │   ├── retry_state.rs
│   │   │   ├── skill_usage.rs
│   │   │   ├── stale_resume.rs
│   │   │   ├── tests.rs
│   │   │   └── tmux_runtime.rs
│   │   ├── adk_session.rs
│   │   ├── agentdesk_config.rs
│   │   ├── discord_io.rs
│   │   ├── formatting.rs
│   │   ├── gateway.rs
│   │   ├── handoff.rs
│   │   ├── health.rs
│   │   ├── idle_detector.rs
│   │   ├── inflight.rs
│   │   ├── internal_api.rs
│   │   ├── mcp_credential_watcher.rs
│   │   ├── meeting_artifact_store.rs
│   │   ├── meeting_orchestrator.rs
│   │   ├── meeting_state_machine.rs
│   │   ├── metrics.rs
│   │   ├── mod.rs
│   │   ├── model_catalog.rs
│   │   ├── model_picker_interaction.rs
│   │   ├── monitoring_status.rs
│   │   ├── org_schema.rs
│   │   ├── org_writer.rs
│   │   ├── placeholder_cleanup.rs
│   │   ├── placeholder_controller.rs
│   │   ├── placeholder_sweeper.rs
│   │   ├── prompt_builder.rs
│   │   ├── queue_io.rs
│   │   ├── queued_placeholders_store.rs
│   │   ├── recovery_engine.rs
│   │   ├── response_sanitizer.rs
│   │   ├── restart_ctrl.rs
│   │   ├── restart_mode.rs
│   │   ├── restart_report.rs
│   │   ├── role_map.rs
│   │   ├── runtime_bootstrap.rs
│   │   ├── runtime_store.rs
│   │   ├── session_identity.rs
│   │   ├── session_runtime.rs
│   │   ├── settings.rs
│   │   ├── shared_memory.rs
│   │   ├── tmux.rs
│   │   ├── tmux_error_detect.rs
│   │   ├── tmux_lifecycle.rs
│   │   ├── tmux_overload_retry.rs
│   │   ├── tmux_reaper.rs
│   │   └── tmux_restart_handoff.rs
│   ├── maintenance/
│   │   ├── jobs/
│   │   │   ├── db_retention.rs
│   │   │   ├── hang_dump_cleanup.rs
│   │   │   ├── memento_consolidation.rs
│   │   │   ├── mod.rs
│   │   │   ├── target_sweep.rs
│   │   │   └── worktree_orphan_sweep.rs
│   │   └── mod.rs
│   ├── memory/
│   │   ├── local.rs
│   │   ├── memento.rs
│   │   ├── memento_throttle.rs
│   │   ├── mod.rs
│   │   └── runtime_state.rs
│   ├── observability/
│   │   ├── events.rs
│   │   ├── metrics.rs
│   │   ├── mod.rs
│   │   └── watcher_latency.rs
│   ├── platform/
│   │   ├── binary_resolver.rs
│   │   ├── dump_tool.rs
│   │   ├── mod.rs
│   │   ├── shell.rs
│   │   └── tmux.rs
│   ├── provider_cli/
│   │   ├── context.rs
│   │   ├── io.rs
│   │   ├── mod.rs
│   │   ├── paths.rs
│   │   ├── registry.rs
│   │   └── snapshot.rs
│   ├── slo/
│   │   └── mod.rs
│   ├── agent_protocol.rs
│   ├── api_friction.rs
│   ├── auto_queue.rs
│   ├── claude.rs
│   ├── codex.rs
│   ├── codex_tmux_wrapper.rs
│   ├── discord_config_audit.rs
│   ├── discord_dm_reply_store.rs
│   ├── disk_monitor.rs
│   ├── dispatches.rs
│   ├── dispatches_followup.rs
│   ├── gemini.rs
│   ├── kanban.rs
│   ├── mcp_config.rs
│   ├── message_outbox.rs
│   ├── mod.rs
│   ├── opencode.rs
│   ├── process.rs
│   ├── provider.rs
│   ├── provider_exec.rs
│   ├── provider_runtime.rs
│   ├── queue.rs
│   ├── qwen.rs
│   ├── qwen_tmux_wrapper.rs
│   ├── remote_stub.rs
│   ├── retrospectives.rs
│   ├── service_error.rs
│   ├── session_backend.rs
│   ├── settings.rs
│   ├── shell_guard.rs
│   ├── termination_audit.rs
│   ├── tmux_common.rs
│   ├── tmux_diagnostics.rs
│   ├── tmux_wrapper.rs
│   ├── tool_output_guard.rs
│   ├── turn_lifecycle.rs
│   └── turn_orchestrator.rs
├── supervisor/
│   └── mod.rs
├── ui/
│   ├── ai_screen.rs
│   └── mod.rs
├── utils/
│   ├── async_bridge.rs
│   ├── format.rs
│   ├── mod.rs
│   └── wip_detect.rs
├── bootstrap.rs
├── config.rs
├── credential.rs
├── error.rs
├── integration_tests.rs
├── kanban.rs
├── launch.rs
├── logging.rs
├── main.rs
├── manual_intervention.rs
├── pipeline.rs
├── receipt.rs
├── reconcile.rs
└── runtime.rs
```
<!-- END GENERATED: SRC TREE -->

## High-Signal Module Map

### Top-Level Rust Modules

This table is generated from the current `src/` root and fails CI when a new top-level module or directory lacks a description.

<!-- BEGIN GENERATED: TOP LEVEL MODULE MAP -->
> Generated by `python3 scripts/generate_inventory_docs.py`. Update `TOP_LEVEL_MODULE_PURPOSES` when `src/` top-level entries change.

| Path | Purpose |
| --- | --- |
| `src/cli/` | Operator-facing CLI commands, direct API shims, migrations, and Discord send helpers. |
| `src/compat/` | Centralised home for compatibility/legacy/fallback shims (#1076). Each public item carries a `REMOVE_WHEN` comment so retirement is grep-driven. |
| `src/db/` | SQLite access layer and schema authority (`src/db/schema.rs`). |
| `src/dispatch/` | Dispatch context construction, review metadata, and worktree targeting. |
| `src/engine/` | QuickJS policy runtime, hook wiring, transition logic, and Rust-JS bridge ops. |
| `src/github/` | GitHub sync, issue triage, and Definition-of-Done mirroring. |
| `src/integration_tests/` | Scenario-specific integration test modules that supplement `src/integration_tests.rs`. |
| `src/runtime_layout/` | Managed runtime layout, memory-path migration, shared prompt sync, and skill deployment. |
| `src/server/` | Axum server boot, routes, workers, background loops, and WebSocket broadcast. |
| `src/services/` | Core runtime services: provider runners, Discord bot, queueing, memory, and platform helpers. |
| `src/supervisor/` | Runtime supervisor signals and recovery decisions for orphaned or stalled work. |
| `src/ui/` | Compatibility shims for persisted UI/session types used by the Discord runtime. |
| `src/utils/` | Shared formatting and Unicode-safe string utilities. |
| `src/bootstrap.rs` | Builds config, database, policy engine, and shared app state before launch. |
| `src/config.rs` | `agentdesk.yaml` parsing, configuration defaults, and shared test env helpers. |
| `src/credential.rs` | Reads runtime credential files such as Discord bot tokens from the AgentDesk root. |
| `src/error.rs` | Shared HTTP and policy error type with typed codes and JSON response helpers. |
| `src/integration_tests.rs` | End-to-end pipeline, dispatch, review, and recovery integration test harness. |
| `src/kanban.rs` | High-level kanban orchestration and transition entrypoints. |
| `src/launch.rs` | Starts the Tokio runtime and hands off to server boot. |
| `src/logging.rs` | Tracing span helpers that stamp dispatch, card, agent, and hook context onto logs. |
| `src/main.rs` | Binary entry point. Dispatches CLI commands or boots the server runtime. |
| `src/manual_intervention.rs` | Manual intervention parsing and helpers shared by Discord reply/requeue flows. |
| `src/pipeline.rs` | Pipeline stage loading, resolution, and transition helpers. |
| `src/receipt.rs` | Receipt parsing and workspace attribution helpers. |
| `src/reconcile.rs` | Boot-time reconciliation for persisted state and dispatch-runtime drift. |
| `src/runtime.rs` | Session runtime abstraction (`SessionRuntime`) plus the tmux-backed implementation. |
<!-- END GENERATED: TOP LEVEL MODULE MAP -->

### Discord Runtime

| Path | Purpose |
| --- | --- |
| `src/services/discord/mod.rs` | Shared bot state, boot wiring, cross-module exports. |
| `src/services/discord/router/` | Message intake, thread binding, dispatch guard, control intent parsing. |
| `src/services/discord/turn_bridge/` | Turn execution lifecycle, completion guard, retry handling, memory capture. |
| `src/services/discord/session_runtime.rs` | Session bootstrap, path/worktree resolution, per-channel session state. |
| `src/services/discord/tmux.rs` / `tmux_reaper.rs` | tmux watcher lifecycle, stale session cleanup, reaping. |
| `src/services/discord/recovery_engine.rs` | Restart-time inflight turn recovery. |
| `src/services/discord/gateway.rs` / `discord_io.rs` / `queue_io.rs` | Discord gateway bridge and outbound/inbound message plumbing. |
| `src/services/discord/commands/` | Slash command handlers for session, config, diagnostics, meetings, models, receipts, skills. |
| `src/services/discord/agentdesk_config.rs` / `config_audit.rs` | YAML/DB/legacy config source-of-truth handling and audits. |
| `src/services/discord/prompt_builder.rs` / `shared_memory.rs` / `role_map.rs` | Turn prompt assembly and org/shared memory context. |

### Provider and Execution Services

| Path | Purpose |
| --- | --- |
| `src/services/claude.rs`, `codex.rs`, `gemini.rs`, `qwen.rs` | Provider-specific session execution and stream handling. |
| `src/services/provider.rs` / `provider_exec.rs` / `provider_runtime.rs` | Provider abstraction, dispatch, runtime metadata. |
| `src/services/session_backend.rs` | Child-process session backend for non-tmux execution paths. |
| `src/services/tmux_wrapper.rs`, `codex_tmux_wrapper.rs`, `qwen_tmux_wrapper.rs` | Provider wrappers used inside tmux-managed sessions. |
| `src/services/process.rs` / `platform/` | Process-tree control, shell helpers, binary resolution, tmux/platform utilities. |
| `src/services/queue.rs` / `turn_orchestrator.rs` / `turn_lifecycle.rs` | Per-channel queueing, cancellation, active turn bookkeeping. |

### Server and API

| Path | Purpose |
| --- | --- |
| `src/server/boot.rs` / `src/server/mod.rs` | Axum boot, router assembly, background/tick startup. |
| `src/server/routes/mod.rs` | API route registration under `/api`. |
| `src/server/routes/dispatches/` | Dispatch CRUD, Discord delivery, outbox, thread reuse. |
| `src/server/routes/review_verdict/` | Review verdict and decision routes plus review-state storage helpers. |
| `src/server/ws.rs` | Top-level WebSocket endpoint and broadcast plumbing. |
| `src/server/worker_registry.rs` | Supervised worker specs; mirrored to `docs/generated/worker-inventory.md`. |

## Generated Inventories

- `docs/generated/module-inventory.md` is the fastest way to answer “which module owns this code?”
- `docs/generated/route-inventory.md` is the authoritative endpoint-to-handler map. Prefer it over manually maintained tables.
- `docs/generated/worker-inventory.md` shows every supervised worker, its start stage, restart policy, and owner.
- `python3 scripts/generate_inventory_docs.py --check` is the CI drift gate for these inventories, the generated `src/` snapshot above, and the top-level module coverage table.

## Troubleshooting: Where to Look

### Discord turn did not start

1. `src/services/discord/router/message_handler.rs` — intake, session/worktree selection, dispatch context hints.
2. `src/services/discord/turn_bridge/mod.rs` — turn spawn, stream loop, completion path.
3. Provider file: `src/services/claude.rs`, `codex.rs`, `gemini.rs`, or `qwen.rs`.

### Session died or output stopped

1. `src/services/discord/tmux.rs` — watcher, session kill, resume, orphan handling.
2. `src/services/discord/turn_bridge/tmux_runtime.rs` — active token and watcher handoff helpers.
3. `src/services/tmux_diagnostics.rs` / `src/services/process.rs` — exit diagnostics and process-tree cleanup.
4. `src/services/discord/recovery_engine.rs` — restart-time restoration.

### Worktree or cwd is wrong

1. `src/services/discord/session_runtime.rs` — session path/worktree creation.
2. `src/dispatch/mod.rs` — card-scoped worktree resolution and dispatch context injection.
3. `src/cli/client.rs` — completion payload fallback for `completed_worktree_path`.

### Kanban or review state is wrong

1. `src/kanban.rs` — high-level card orchestration.
2. `src/engine/transition.rs` — canonical state transitions.
3. `src/engine/ops/kanban_ops.rs` — review-state sync bridge and SQL-side helpers.
4. `src/server/routes/review_verdict/` — review verdict/decision HTTP surface.

### API endpoint is missing or behaving unexpectedly

1. `src/server/routes/mod.rs` — confirm registration.
2. Relevant handler file under `src/server/routes/`.
3. `docs/generated/route-inventory.md` — confirm method/path/handler mapping.
4. `src/server/ws.rs` — for the top-level `/ws` endpoint.

### Startup failed

1. `src/bootstrap.rs` — config/db/runtime assembly.
2. `src/config.rs` — config load/defaults.
3. `src/db/mod.rs` and `src/db/schema.rs` — DB open/migrations.
4. `src/launch.rs`, `src/server/boot.rs`, `src/server/mod.rs` — runtime and HTTP boot.

## Policy and Runtime Flow

- Policy definitions live in `policies/*.js`.
- Hook contracts live in `src/engine/hooks.rs`.
- Policy loading and execution live in `src/engine/loader.rs` and `src/engine/mod.rs`.
- Rust bridge functions live in `src/engine/ops.rs` plus `src/engine/ops/*.rs`.
- Tick orchestration lives in `src/server/tick.rs` and the server boot path.

## Session Execution Paths

### tmux-backed providers

1. Message intake: `src/services/discord/router/message_handler.rs`
2. Turn spawn: `src/services/discord/turn_bridge/mod.rs`
3. Provider execution: provider module + `*_tmux_wrapper.rs`
4. Watch/recovery: `src/services/discord/tmux.rs` and `src/services/discord/recovery_engine.rs`

### Child-process backend

1. Message intake: `src/services/discord/router/message_handler.rs`
2. Session spawn: `src/services/session_backend.rs`
3. Cleanup: `src/services/process.rs`

### Session runtime state (issue #892)

Tmux-backed sessions keep four kinds of runtime state alongside each pane:
the provider jsonl stream (`.jsonl`), the input FIFO (`.input`), the launch
script (`.sh`), and an owner marker (`.owner`). These files live in a
**persistent** per-runtime directory rather than `/tmp/` so that a dcserver
restart does not render a still-alive tmux pane "unusable":

- **Persistent path:** `runtime_root()/runtime/sessions/` (mode `0o700`),
  resolved via `tmux_common::session_temp_path(session_name, ext)`. The
  directory is created at dcserver startup in `src/cli/dcserver.rs` and
  lazily re-created inside `agentdesk_temp_dir()` for early callers.
- **Legacy `/tmp/` fallback:** wrappers spawned before this migration
  still hold open fds on `/tmp/agentdesk-*` files. Readers go through
  `tmux_common::resolve_session_temp_path` which prefers the new path
  and falls back to the legacy `/tmp` location so `session_usable`
  checks (`claude::execute_streaming_local_tmux`,
  `codex::execute_streaming_local_tmux`,
  `qwen::execute_streaming_local_tmux`) keep re-attaching to live panes.
  Legacy files are **never** swept at startup — pre-migration wrappers
  may still be writing into them.
- **Size cap policy:** 20 MB rolling head-truncate. The watcher in
  `src/services/discord/tmux.rs::tmux_output_watcher` periodically
  (~every 60 loop ticks) calls `truncate_jsonl_head_safe(path, 20 MB, 15 MB)`
  which rewrites the file keeping only the last ~15 MB worth of complete
  lines. Any partial leading line is dropped so downstream stream-json
  parsers never observe half-records.
- **Cleanup triggers:**
  - Recreate path inside each provider module calls
    `cleanup_session_temp_files(session_name)` before building a fresh
    session; it hits both persistent and legacy locations.
  - `turn_lifecycle::stop_turn_with_policy` calls
    `cleanup_session_temp_files` after a successful forced kill
    (force_kill_turn, `/clear`, etc).
  - `discord::tmux_reaper::reap_orphan_tmux_files` uses the same helper.
  - Startup orphan sweep: `discord::tmux::sweep_orphan_session_files`
    removes files in the persistent sessions dir whose stem has no
    matching live tmux session and whose oldest mtime is older than
    10 minutes. Deliberately skips `/tmp/` to avoid stomping on
    pre-migration wrappers.
- **Key helpers:** `src/services/tmux_common.rs`
  (`session_temp_path`, `legacy_tmp_session_path`,
  `resolve_session_temp_path`, `cleanup_session_temp_files`,
  `truncate_jsonl_head_safe`, `ensure_sessions_dir_on_startup`,
  `persistent_sessions_dir`).

## Data Model Anchors

- `src/db/schema.rs` is the authoritative schema.
- Most operational state hangs off `agents`, `kanban_cards`, `task_dispatches`, `sessions`, `auto_queue_runs`, `auto_queue_entries`, `github_repos`, and `kv_meta`.
- If a doc or handler disagrees with the schema, trust `src/db/schema.rs` and update the doc.
