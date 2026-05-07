# Change Surfaces

> Source: [`docs/agent-maintenance/index.md`](index.md). For every "where do I
> add this?" question, consult this page first. The giant-file list is the
> auto-generated inventory in
> [`docs/generated/module-inventory.md`](../generated/module-inventory.md);
> the rows below project the operational meaning of each entry.
>
> Last refreshed: 2026-05-05 (against #1742 deployment-blocking migration checksum hotfix and current maintainability audit baseline).

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

- canonical_modules: `src/services/discord/outbound/{message,policy,result,decision,delivery}.rs`
  (#1006 v3 domain types, pure planner, and delivery implementation).
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
  (per-domain op handlers). `src/pipeline.rs` (2125 lines, giant-file)
  composes the policy pipeline.
- legacy_modules: none — there is no parallel engine. The whole surface is
  pre-migration giant-file territory.
- do_not_edit_without_migration_plan:
  - `src/engine/mod.rs` (2590 lines, giant-file).
  - `src/engine/ops/review_automation_ops.rs` (2140 lines, giant-file).
  - `src/engine/transition.rs` (1309 lines, giant-file).
  - `src/engine/ops/kanban_ops.rs` (1116 lines, giant-file).
  - `src/engine/ops/db_ops.rs` (1652 lines, giant-file).
  - `src/engine/intent.rs` (873 lines, retained migration-sensitive surface).
  - `src/pipeline.rs` (2125 lines, giant-file).
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
  - `src/dispatch/mod.rs` (5109 lines).
  - `src/dispatch/dispatch_context.rs` (3987 lines).
  - `src/dispatch/dispatch_create.rs` (2877 lines).
  - `src/dispatch/dispatch_status.rs` (1921 lines).
  - `src/services/dispatches/outbox_route.rs` (1074 lines; route extraction
    orchestration surface from #1722, split before adding non-bugfix behavior).
  - `src/services/dispatches/discord_delivery/orchestration.rs` (1670 lines;
    delivery orchestration surface extracted from the route layer in #1760,
    split before adding non-bugfix behavior).
- active_callsite_coverage: n/a.
- invariants: dispatch creation is the only writer for `dispatched_sessions`;
  status transitions go through `dispatch_status`.
- allowed_changes: `bugfix` only.
- tests: `src/integration_tests/dispatch_flow/*` and per-module unit tests.
- related_issues: #1784, #1785, #1808-#1814.

### `tmux_watcher`

- canonical_modules: `src/services/discord/watchers/lifecycle.rs` (watcher
  stop/reattach/claim/restore lifecycle, including the #1222 single-owner
  claim path and #1283 cancel-induced reattach contract),
  `src/services/discord/tmux.rs` (watcher loop and remaining tmux relay
  parsing), `src/services/discord/inflight.rs` (state file contract).
- legacy_modules: none — relay routes are being consolidated, not replaced.
- do_not_edit_without_migration_plan (giant-file):
  - `src/services/discord/watchers/lifecycle.rs` (1942 lines — canonical
    lifecycle extraction surface from #1435; split further before adding new
    lifecycle behavior).
  - `src/services/discord/tmux.rs` (6208 lines after #1435 lifecycle
    extraction; still giant-file territory).
  - `src/services/discord/tmux_watcher.rs` (3214 lines after #1520 watcher
    loop extraction; split loop helpers further before adding behavior).
  - `src/services/discord/recovery_engine.rs` (4842 lines).
  - `src/services/discord/health.rs` (4880 lines after #1879 snapshot/mailbox
    extraction).
  - `src/services/discord/health/recovery.rs` (1433 lines; health recovery
    extraction surface, split further before adding non-bugfix behavior).
  - `src/services/discord/placeholder_controller.rs` (1237 lines).
  - `src/services/discord/router/message_handler.rs` (7013 lines).
  - `src/services/discord/meeting_orchestrator.rs` (3779 lines).
  - `src/services/discord/turn_bridge/mod.rs` (4267 lines).
  - `src/services/discord/turn_bridge/tmux_runtime.rs` (1016 lines; provider
    stop-token/tmux binding runtime, split before adding non-bugfix behavior).
  - `src/services/discord/turn_bridge/completion_guard.rs` (2096 lines).
  - `src/services/discord/turn_bridge/tmux_runtime.rs` (1016 lines).
  - `src/services/discord/formatting.rs` (3247 lines).
  - `src/services/discord/settings.rs` (2445 lines).
  - `src/services/discord/prompt_builder.rs` (2114 lines).
  - `src/services/discord/runtime_bootstrap.rs` (2844 lines).
  - `src/services/discord/session_runtime.rs` (1887 lines).
  - `src/services/discord/commands/config.rs` (1877 lines).
  - `src/services/discord/commands/inspect.rs` (1058 lines, post-#1701
    context-view manifest binding pushed it past the giant-file threshold).
  - `src/services/discord/{commands/text_commands.rs, commands/diagnostics.rs,
    discord_config_audit.rs, router/intake_gate.rs, model_catalog.rs,
    qwen_tmux_wrapper.rs, agentdesk_config.rs, inflight.rs}` (all 1000+ lines).
- active_callsite_coverage: n/a.
- invariants: watcher single-owner per #1222; placeholder lifecycle invariants
  per #1112; `/api/inflight/rebind` is the only path that synthesises an
  inflight state file (`src/services/discord/inflight.rs:107`,
  `:415`, `:952`). Cancel-induced death must trigger immediate re-attach
  (#1283 contract, see `src/services/discord/watchers/lifecycle.rs`).
- allowed_changes: `bugfix` only on `tmux.rs` and the giant Discord modules.
  `extraction` requires a follow-up issue.
- tests: `src/integration_tests/tests/*` cancel/recovery suites.
- related_issues: #964, #1112, #1138, #1222, #1223, #1283.

### `dashboard_routes`

- canonical_modules: `src/server/routes/*.rs` (per-domain route module).
  `src/server/routes/auto_queue.rs` (151 lines) is now an HTTP-only facade;
  its query/command/view/FSM behavior lives under
  `src/services/auto_queue/{query,command,view,fsm,phase_gate}.rs` plus
  smaller route-delegation slices.
  `src/services/auto_queue/activate_command.rs` (1010 lines, post-#1444
  idempotency-guard expansion) is the canonical activate/dispatch-next
  command surface; it is intentionally above the giant-file threshold and
  tracked here. Further growth requires a split issue.
- legacy_modules: none, but several routes still call `legacy_db()` against
  the SQLite compat handle (see `known-legacy.md`).
- do_not_edit_without_migration_plan (giant-file routes):
  - `src/server/routes/dispatches/discord_delivery.rs` (5564 lines).
  - `src/server/routes/kanban.rs` (4426 lines).
  - `src/server/routes/dispatched_sessions.rs` (4002 lines).
  - `src/server/routes/onboarding.rs` (5271 lines).
  - `src/server/routes/docs.rs` (4632 lines).
  - `src/server/routes/dispatches/outbox.rs` (3294 lines).
  - `src/server/routes/escalation.rs` (2110 lines).
  - `src/server/routes/meetings.rs` (2158 lines).
  - `src/server/routes/review_verdict/decision_route.rs` (1865 lines).
  - `src/server/routes/{agents,agents_crud,agents_setup,analytics,v1,
    settings,resume,pipeline,reviews,dispatches/thread_reuse}.rs` (all 1000+
    lines).
- active_callsite_coverage: legacy_db helper coverage tracked separately —
  see `known-legacy.md` row `legacy_db_helper`.
- invariants:
  - `/api/inflight/rebind` is the only synthetic inflight writer
    (`src/server/routes/health_api.rs:684`).
  - Dashboard routes never write to canonical config files; they read DB
    state and emit events.
- allowed_changes: `bugfix` only on giant routes; `new_feature` only when
  added to a sub-1000-line module or after splitting. Auto-queue domain logic
  changes must go under `src/services/auto_queue/*`; the route facade should
  remain extraction/delegation only. New routes must register in the route
  inventory generator.
- tests: `src/server/routes/routes_tests.rs`, plus per-route module tests.
- related_issues: split issues TBD (file under follow-up).

### `cli_runtime`

- canonical_modules: `src/cli/*.rs`.
- legacy_modules: none.
- do_not_edit_without_migration_plan (giant-file):
  - `src/cli/migrate.rs` (348 lines, retired postgres-cutover facade).
  - `src/cli/doctor/orchestrator.rs` (4324 lines).
  - `src/cli/migrate/apply.rs` (3142 lines).
  - `src/cli/migrate/{plan.rs (1513), source.rs (1612)}`.
  - `src/cli/{init.rs (1600), client.rs (1583), direct.rs (1535),
    dcserver.rs (1496)}`.
  - `src/cli/provider_cli/mod.rs` (1701 lines).
- active_callsite_coverage: n/a.
- invariants: LaunchAgent plist and runtime layout are generated only — see
  the matrix in `docs/source-of-truth.md`.
- allowed_changes: `bugfix` only; PG-cutover retention plan is owned by
  #1239.

### `runtime_core`

- canonical_modules: `src/config.rs`, `src/runtime_layout/mod.rs`,
  `src/server/mod.rs`, `src/kanban/state_machine.rs`, `src/receipt.rs`, and
  `src/github/sync.rs`.
- legacy_modules: none — these are shared runtime coordination surfaces.
- do_not_edit_without_migration_plan (giant-file):
  - `src/config.rs` (2601 lines).
  - `src/runtime_layout/mod.rs` (1425 lines).
  - `src/server/mod.rs` (3370 lines).
  - `src/kanban/state_machine.rs` (3550 lines).
  - `src/receipt.rs` (2133 lines).
  - `src/github/sync.rs` (1059 lines).
- active_callsite_coverage: n/a.
- invariants: config precedence, runtime path generation, kanban state, receipt
  persistence, and GitHub sync must keep their existing owner-specific
  contracts; split work needs a dedicated extraction issue before new feature
  logic lands here.
- allowed_changes: `bugfix` only; `src/kanban/` extraction is scoped by
  `docs/agent-maintenance/kanban-extraction-plan.md`; new feature logic must
  land in smaller owner-specific modules or a scoped extraction branch.
- tests: owner-specific tests, with `src/kanban/state_machine.rs` inline tests moving per
  `docs/agent-maintenance/kanban-extraction-plan.md`.
- related_issues: #1786, #1787, #1818-#1825 for `src/kanban/`; other runtime
  giant-file split issues TBD.

### `db_layer`

- canonical_modules: `src/db/{mod,postgres,schema}.rs` and per-domain modules.
- legacy_modules: SQLite path through `libsql_rusqlite` (see `known-legacy.md`).
- do_not_edit_without_migration_plan (giant-file):
  - `src/db/auto_queue.rs` (5691 lines).
  - `src/db/schema.rs` (3194 lines).
  - `src/db/kanban_cards.rs` (1903 lines; kanban card persistence and GitHub
    sync lookup surface).
  - `src/db/postgres.rs` (1536 lines).
  - `src/db/dispatched_sessions.rs` (1200 lines; dispatched session
    persistence helpers).
  - `src/db/session_transcripts.rs` (877 lines, retained PG-cleanup surface).
  - `src/db/agents.rs` (1125 lines).
  - `src/db/prompt_manifests.rs` (1219 lines, post-#1699 retention policy +
    write-time byte cap pushed it past the giant-file threshold).
- active_callsite_coverage: PG-only cleanup tracked per #1237/#1238/#1239 —
  see `known-legacy.md`.
- invariants: production reads/writes go through `pg_pool_ref()`; `legacy_db()`
  remains for unmigrated callsites only.
- allowed_changes: `bugfix` on existing path; `new_feature` MUST use PG.
- tests: `src/integration_tests/postgres_only/*`.
- related_issues: #843 epic, #1237, #1238, #1239.

### `services_misc_giants`

The remaining giant-file modules under `src/services/` not covered above:

- `src/services/api_friction.rs` (1709) — extraction scoped by
  `docs/agent-maintenance/api-friction-extraction-plan.md` (#1788, #1789,
  #1830-#1837); bugfix only outside that plan.
- `src/services/analytics.rs` (1049) — dashboard analytics query surface;
  split before adding non-bugfix behavior.
- `src/services/auto_queue.rs` (1047) and
  `src/services/auto_queue/activate_command.rs` (1012); auto-queue route
  behavior is split across `src/services/auto_queue/*` slices, with
  `activate_command.rs` now giant-file territory.
- `src/services/onboarding.rs` (5279), `src/services/dispatched_sessions.rs`
  (2954), and `src/services/settings.rs` (1015) — service-layer route support
  surfaces split out of the large dashboard route modules.
- `src/services/dispatches/outbox_route.rs` (1074) — dispatch outbox route
  support extracted from the route layer; split before adding non-bugfix
  behavior.
- `src/services/claude.rs` (2477), `src/services/gemini.rs` (2565),
  `src/services/qwen.rs` (2466), `src/services/codex.rs` (1665),
  `src/services/opencode.rs` (2133), `src/services/provider.rs` (2177) —
  provider adapters.
- `src/services/memory/memento.rs` (2479).
- `src/services/observability/mod.rs` (3647).
- `src/services/dispatched_sessions.rs` (2954) — dispatched session domain
  service. This is the post-#1515 SRP extraction target for route/database
  callsites, but the module itself is now giant-file territory; split focused
  helpers before adding non-bugfix behavior.
- `src/services/onboarding.rs` (5279) — onboarding domain service extracted
  from the route layer in #1518. Treat as giant-file territory; future behavior
  should move into focused onboarding helper modules before adding breadth.
- `src/services/settings.rs` (1015) — settings domain service extracted from
  the route layer in #1519. Keep follow-up changes bugfix-only unless the file
  is split further.
- `src/services/routines/loader.rs` (1753),
  `src/services/routines/store.rs` (2755), and
  `src/services/routines/discord_log.rs` (1056); routine loader/store and
  Discord logging are the canonical scheduled JS routine surfaces. Further
  feature work should split focused helper modules before growing these files
  again.
- `src/services/platform/shell.rs` (1507) — split owned by #1281
  (GitClient extraction).
- `src/services/platform/binary_resolver.rs` (1377).
- `src/services/mcp_config.rs` (1072).
- `src/services/routines/{store.rs (2755), loader.rs (2013),
  discord_log.rs (1056)}` — durable routine storage, script loading, and
  Discord notification plumbing. Split before broadening behavior outside the
  current routine API/runtime contract.
- `src/services/discord/mod.rs` (5519),
  `src/services/discord_config_audit.rs` (1310), and
  `src/services/qwen_tmux_wrapper.rs` (1194).
- `src/services/turn_orchestrator.rs` (2070).
- `src/services/session_backend.rs` (1053).

Same rule: `bugfix` only without a split issue.

## Shared API Helpers

For new HTTP route logic that paginates over Postgres-backed lists, prefer the
shared helper `crate::utils::api::clamp_api_limit` (in `src/utils/api.rs`) over
inline `limit.clamp(1, 100)` calls. The helper applies the standard API-limit
shape (default 50, clamped to 1..=100) and is the single canonical site for
that bound — `scripts/audit_maintainability/checks/limit_clamp_duplication.py`
flags any new inline `clamp(1, 100)` outside the helper definition (#1698).
For non-standard bounds, extend `clamp_limit(limit, default, max)` rather than
reintroducing bespoke clamp expressions.

## Updating This Page

- Re-run `python3 scripts/generate_inventory_docs.py` and reconcile the
  giant-file list against the table above.
- When a giant file is split, move its canonical_module entry to the new
  module path and remove it from `do_not_edit_without_migration_plan`.
- When a new module crosses the 1000-line threshold, add it to its feature
  block in the same PR — do not let the inventory generator be the only
  signal.
