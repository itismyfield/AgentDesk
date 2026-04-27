# Change Surfaces

> Source: [`docs/agent-maintenance/index.md`](index.md). For every "where do I
> add this?" question, consult this page first. The giant-file list is the
> auto-generated inventory in
> [`docs/generated/module-inventory.md`](../generated/module-inventory.md);
> the rows below project the operational meaning of each entry.

## Read This First

- "giant-file" = `>= 1000` lines per `scripts/generate_inventory_docs.py`. New
  logic added to a giant file inherits the file's review surface — every
  reviewer must re-read the entire module — so adding to it without an
  extraction plan is rejected.
- `do_not_edit_without_migration_plan` columns below mean: even though the
  file builds and runs, the scheduled migration owner will roll back ad-hoc
  additions. If you must change behaviour there, scope it to a single bugfix
  AND link the migration issue in the PR description.
- `active_callsite_coverage` only applies to surfaces with a parallel canonical
  path already implemented (e.g. Discord outbound v3). For pre-migration giant
  files (no canonical replacement yet), the column is `n/a`.

## Surface Map (by feature)

### `discord_outbound`

- canonical_modules: `src/services/discord/outbound/{message,policy,result,decision}.rs`
  (#1006 v3 domain types and pure planner).
- legacy_modules: `src/services/discord/outbound/legacy.rs` (`deliver_outbound`,
  `OutboundDeduper`, `DiscordOutbound*` types).
- do_not_edit_without_migration_plan:
  `src/services/discord/formatting.rs::send_long_message_raw` (line 1971,
  ordered-chunk continuation contract not yet modelled in v3).
- active_callsite_coverage: see
  [`discord-outbound-migration.md`](discord-outbound-migration.md) (table is
  the authoritative coverage record).
- invariants: every new `send` or `edit` from production code goes through
  `outbound::deliver_outbound` (or the v3 successor) — never `channel_id.say`,
  `channel_id.send_message`, or raw `http.send_message` from a route or
  worker. Interaction-token responses (`ctx.say`, `ComponentInteraction`) are
  the only allowed exception per #1175.
- allowed_changes: `bugfix` on `legacy.rs` only when the migration table marks
  the calling row as `legacy`; `new_feature` only on the v3 `outbound/`
  submodules; `extraction` from `formatting.rs` requires a contract update
  for ordered chunk metadata.
- tests: `src/integration_tests/discord_flow/scenarios.rs`,
  `src/integration_tests/agents_setup_e2e.rs`, plus per-module unit tests in
  `outbound/{message,policy,decision,result}.rs`.
- related_issues: #1006, #1175, #1280.

### `policy_engine`

- canonical_modules: `src/engine/mod.rs` (driver) plus `src/engine/ops/*.rs`
  (per-domain op handlers). `src/pipeline.rs` (2144 lines, giant-file)
  composes the policy pipeline.
- legacy_modules: none — there is no parallel engine. The whole surface is
  pre-migration giant-file territory.
- do_not_edit_without_migration_plan:
  - `src/engine/mod.rs` (2363 lines, giant-file).
  - `src/engine/ops/review_automation_ops.rs` (2722 lines, giant-file).
  - `src/engine/transition.rs` (1757 lines, giant-file).
  - `src/engine/ops/kanban_ops.rs` (1484 lines, giant-file).
  - `src/engine/ops/db_ops.rs` (1200 lines, giant-file).
  - `src/engine/intent.rs` (1018 lines, giant-file).
  - `src/pipeline.rs` (2144 lines, giant-file).
- active_callsite_coverage: n/a (no canonical replacement yet).
- invariants: typed-facade contract from
  [`docs/policy-typed-facade.md`](../policy-typed-facade.md); engine never
  mutates DB rows directly outside `engine::ops::db_ops`.
- allowed_changes: `bugfix` only. Any non-bugfix needs an extraction issue
  filed first (no current owner — file under "policy-engine refactor" follow-up
  per #1279).
- tests: see `engine/` per-module `#[cfg(test)] mod tests` and
  `src/integration_tests/tests/high_risk_recovery.rs`.
- related_issues: #735 (`docs/policy-tick-bottleneck-735.md`).

### `dispatch`

- canonical_modules: `src/dispatch/{mod,dispatch_context,dispatch_create,dispatch_status}.rs`.
- legacy_modules: none.
- do_not_edit_without_migration_plan (giant-file, awaiting split issue):
  - `src/dispatch/mod.rs` (4899 lines).
  - `src/dispatch/dispatch_context.rs` (3802 lines).
  - `src/dispatch/dispatch_create.rs` (2517 lines).
  - `src/dispatch/dispatch_status.rs` (1877 lines).
- active_callsite_coverage: n/a.
- invariants: dispatch creation is the only writer for `dispatched_sessions`;
  status transitions go through `dispatch_status`.
- allowed_changes: `bugfix` only.
- tests: `src/integration_tests/dispatch_flow/*` and per-module unit tests.
- related_issues: track under #1281-style follow-up if a split is scoped.

### `tmux_watcher`

- canonical_modules: `src/services/discord/tmux.rs` (single owner per #1222
  single-relay-owner contract), `src/services/discord/inflight.rs` (state file
  contract).
- legacy_modules: none — relay routes are being consolidated, not replaced.
- do_not_edit_without_migration_plan (giant-file):
  - `src/services/discord/tmux.rs` (11537 lines — largest in the repo).
  - `src/services/discord/recovery_engine.rs` (4669 lines).
  - `src/services/discord/health.rs` (4093 lines).
  - `src/services/discord/router/message_handler.rs` (6094 lines).
  - `src/services/discord/meeting_orchestrator.rs` (3777 lines).
  - `src/services/discord/turn_bridge/mod.rs` (2878 lines).
  - `src/services/discord/turn_bridge/completion_guard.rs` (2092 lines).
  - `src/services/discord/formatting.rs` (2736 lines).
  - `src/services/discord/settings.rs` (2394 lines).
  - `src/services/discord/prompt_builder.rs` (1963 lines).
  - `src/services/discord/runtime_bootstrap.rs` (1962 lines).
  - `src/services/discord/session_runtime.rs` (1880 lines).
  - `src/services/discord/commands/config.rs` (1809 lines).
  - `src/services/discord/{commands/text_commands.rs, commands/diagnostics.rs,
    discord_config_audit.rs, router/intake_gate.rs, model_catalog.rs,
    qwen_tmux_wrapper.rs, agentdesk_config.rs, inflight.rs}` (all 1000+ lines).
- active_callsite_coverage: n/a.
- invariants: watcher single-owner per #1222; placeholder lifecycle invariants
  per #1112; `/api/inflight/rebind` is the only path that synthesises an
  inflight state file (`src/services/discord/inflight.rs:107`,
  `:415`, `:952`). Cancel-induced death must trigger immediate re-attach
  (#1283 contract, see `src/services/discord/tmux.rs`).
- allowed_changes: `bugfix` only on `tmux.rs` and the giant Discord modules.
  `extraction` requires a follow-up issue.
- tests: `src/integration_tests/tests/*` cancel/recovery suites.
- related_issues: #964, #1112, #1138, #1222, #1223, #1283.

### `dashboard_routes`

- canonical_modules: `src/server/routes/*.rs` (per-domain route module).
- legacy_modules: none, but several routes still call `legacy_db()` against
  the SQLite compat handle (see `known-legacy.md`).
- do_not_edit_without_migration_plan (giant-file routes):
  - `src/server/routes/auto_queue.rs` (10249 lines).
  - `src/server/routes/dispatches/discord_delivery.rs` (6101 lines).
  - `src/server/routes/kanban.rs` (4797 lines).
  - `src/server/routes/dispatched_sessions.rs` (4498 lines).
  - `src/server/routes/onboarding.rs` (4482 lines).
  - `src/server/routes/docs.rs` (3629 lines).
  - `src/server/routes/dispatches/outbox.rs` (2882 lines).
  - `src/server/routes/escalation.rs` (2205 lines).
  - `src/server/routes/meetings.rs` (2024 lines).
  - `src/server/routes/review_verdict/decision_route.rs` (2003 lines).
  - `src/server/routes/{agents,agents_crud,agents_setup,analytics,v1,
    settings,resume,pipeline,dispatches/thread_reuse}.rs` (all 1000+ lines).
- active_callsite_coverage: legacy_db helper coverage tracked separately —
  see `known-legacy.md` row `legacy_db_helper`.
- invariants:
  - `/api/inflight/rebind` is the only synthetic inflight writer
    (`src/server/routes/health_api.rs:684`).
  - Dashboard routes never write to canonical config files; they read DB
    state and emit events.
- allowed_changes: `bugfix` only on giant routes; `new_feature` only when
  added to a sub-1000-line module or after splitting. New routes must register
  in the route inventory generator.
- tests: `src/server/routes/routes_tests.rs`, plus per-route module tests.
- related_issues: split issues TBD (file under follow-up).

### `cli_runtime`

- canonical_modules: `src/cli/*.rs`.
- legacy_modules: none.
- do_not_edit_without_migration_plan (giant-file):
  - `src/cli/migrate/postgres_cutover.rs` (7669 lines, retention candidate
    after #1239).
  - `src/cli/doctor/orchestrator.rs` (4362 lines).
  - `src/cli/migrate/apply.rs` (3144 lines).
  - `src/cli/migrate/{plan.rs (1513), source.rs (1612)}`.
  - `src/cli/{init.rs (1597), client.rs (1496), direct.rs (1536),
    dcserver.rs (1541)}`.
- active_callsite_coverage: n/a.
- invariants: LaunchAgent plist and runtime layout are generated only — see
  the matrix in `docs/source-of-truth.md`.
- allowed_changes: `bugfix` only; PG-cutover retention plan is owned by
  #1239.

### `db_layer`

- canonical_modules: `src/db/{mod,postgres,schema}.rs` and per-domain modules.
- legacy_modules: SQLite path through `libsql_rusqlite` (see `known-legacy.md`).
- do_not_edit_without_migration_plan (giant-file):
  - `src/db/auto_queue.rs` (5940 lines).
  - `src/db/schema.rs` (3195 lines).
  - `src/db/postgres.rs` (1421 lines).
  - `src/db/session_transcripts.rs` (1245 lines).
  - `src/db/agents.rs` (1039 lines).
- active_callsite_coverage: PG-only cleanup tracked per #1237/#1238/#1239 —
  see `known-legacy.md`.
- invariants: production reads/writes go through `pg_pool_ref()`; `legacy_db()`
  remains for unmigrated callsites only.
- allowed_changes: `bugfix` on existing path; `new_feature` MUST use PG.
- tests: `src/integration_tests/postgres_only/*`.
- related_issues: #843 epic, #1237, #1238, #1239.

### `services_misc_giants`

The remaining giant-file modules under `src/services/` not covered above:

- `src/services/api_friction.rs` (1808).
- `src/services/auto_queue.rs` (1050) and `auto_queue/cancel_run.rs` (1782).
- `src/services/claude.rs` (2477), `gemini.rs` (2546), `qwen.rs` (2446),
  `codex.rs` (1679), `provider.rs` (2100) — provider adapters.
- `src/services/memory/memento.rs` (2479).
- `src/services/observability/mod.rs` (3647).
- `src/services/platform/shell.rs` (1507) — split owned by #1281
  (GitClient extraction).
- `src/services/turn_orchestrator.rs` (2070).
- `src/services/session_backend.rs` (1053).

Same rule: `bugfix` only without a split issue.

## Updating This Page

- Re-run `python3 scripts/generate_inventory_docs.py` and reconcile the
  giant-file list against the table above.
- When a giant file is split, move its canonical_module entry to the new
  module path and remove it from `do_not_edit_without_migration_plan`.
- When a new module crosses the 1000-line threshold, add it to its feature
  block in the same PR — do not let the inventory generator be the only
  signal.
