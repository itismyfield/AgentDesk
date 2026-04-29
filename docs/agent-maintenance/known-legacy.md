# Known Legacy

> Code paths that are intentionally legacy on `main`. Each entry has a cleanup
> owner issue. Touch these paths only inside the scope of the listed issue or
> for a single narrow bugfix; do not extend them with new logic.
>
> Last refreshed: 2026-04-29 (against `main` @ `1d165cd3844e94015ab30cda8e4b1bba717f934d`).

## Schema Reminder

Every entry uses the common §8 schema: `feature`, `canonical_modules`,
`legacy_modules`, `do_not_edit_without_migration_plan`,
`active_callsite_coverage`, `invariants`, `allowed_changes`, `tests`,
`related_issues`.

## `libsql_rusqlite` Cargo Dependency

- feature: `db_layer / sqlite_dependency`
- canonical_modules: `src/db/postgres.rs` (PG pool), `src/db/mod.rs` (PG-only
  helpers)
- legacy_modules:
  - `Cargo.toml::libsql_rusqlite = { package = "libsql-rusqlite", version =
    "0.33", features = ["bundled"] }`
  - `src/db/schema.rs` SQLite migration code (giant-file, 3195 lines)
  - `src/db/mod.rs` SQLite init/`DbPool` types
  - All `use libsql_rusqlite::*` imports in production code (1000+ hits per
    issue baseline)
- do_not_edit_without_migration_plan: do not add any new
  `use libsql_rusqlite::*` import. Removing existing ones is welcome inside
  #1239 scope.
- active_callsite_coverage: blocked behind #1237 (remove runtime
  `AppState.db`) and #1238 (PG-only fallback removal). #1239 deletes the
  Cargo dependency last.
- invariants: PG cutover policy from
  [`docs/postgres-cutover.md`](../postgres-cutover.md) and
  [`docs/generated/pg-audit-checklist.md`](../generated/pg-audit-checklist.md).
- allowed_changes: `bugfix` only inside the SQLite paths; PG-only `new_feature`
  must use `pg_pool_ref()`.
- tests: `cargo test --bin agentdesk high_risk_recovery::` (post-#1239 DoD).
- related_issues: parent epic #843, ordered chain #1237 → #1238 → #1239.

## Legacy Outbound (`outbound/legacy.rs`)

- feature: `discord_outbound / legacy_helpers`
- canonical_modules: `src/services/discord/outbound/{message,policy,result,decision}.rs`
  (v3 domain types and pure planner)
- legacy_modules: `src/services/discord/outbound/legacy.rs` —
  `deliver_outbound`, `OutboundDeduper`, all `Discord*` types and policy
  enums. Also `src/services/discord/formatting.rs::send_long_message_raw`
  (line 1971), kept direct because the ordered-chunk continuation contract
  is not yet modelled in v3.
- do_not_edit_without_migration_plan: do not extend `legacy.rs` types. New
  send/edit features land on the v3 modules. Bugfix is permitted only when
  the corresponding callsite row in
  [`discord-outbound-migration.md`](discord-outbound-migration.md) is `legacy`
  or `unknown`.
- active_callsite_coverage: see the table in
  [`discord-outbound-migration.md`](discord-outbound-migration.md).
- invariants: `new_send_must_use_v3`,
  `legacy_bugfix_only_when_table_legacy` (full text in the migration page).
- allowed_changes: `bugfix` (per table); `extraction` to a v3 module is
  encouraged.
- tests: `src/integration_tests/discord_flow/scenarios.rs`,
  `src/integration_tests/agents_setup_e2e.rs`.
- related_issues: #1006, #1175, #1280.

## `legacy_db()` helper

- feature: `db_layer / legacy_compat_handle`
- canonical_modules: PG pool via `state.pg_pool_ref()` and per-domain PG
  query modules.
- legacy_modules: per-route shim functions, all named `fn legacy_db(state:
  &AppState) -> &crate::db::Db`:
  - `src/server/routes/onboarding.rs:17`
  - `src/server/routes/kanban.rs:16`
  - `src/server/routes/review_verdict/verdict_route.rs:8`
  - `src/server/routes/review_verdict/decision_route.rs:10`
  - Plus the engine-side definition `src/engine/mod.rs:436`:
    `pub(crate) fn legacy_db(&self) -> Option<&Db>`.
  - Server-side reads at `src/server/mod.rs:158`, `:365`, `:376`.
- do_not_edit_without_migration_plan: do not add a new `legacy_db(&AppState)`
  shim to a new route. New routes use `state.pg_pool_ref()`.
- active_callsite_coverage: tracked by #1238. Each removed shim is a
  step-down toward `legacy_db()` returning `None` everywhere, after which
  the engine-side helper itself is deleted in #1239.
- invariants: kanban handlers per the comment at
  `src/server/routes/kanban.rs:18` already tolerate `legacy_db().is_none()`;
  this is the target shape for every other route.
- allowed_changes: `bugfix` only; `extraction` to PG-only is the cleanup
  itself.
- tests: PG-only test suite under `src/integration_tests/postgres_only/*`.
- related_issues: #843, #1237, #1238, #1239.

## SQLite Fallback Branches in Routes

- feature: `db_layer / runtime_sqlite_fallback`
- canonical_modules: PG-only handlers under `src/server/routes/*.rs`.
- legacy_modules: any `match state.legacy_db() { Some(db) => …, None => …
  }` branch in a route. Concentrated in `onboarding.rs`, `kanban.rs`, the
  review-verdict routes, and the auto_queue route family.
- do_not_edit_without_migration_plan: do not add a new SQLite arm. Existing
  arms are removed in #1238 in batches.
- active_callsite_coverage: baseline grep is owned by #1239 ("작업 시작 전
  `rg -c 'legacy_db|rusqlite|libsql_rusqlite' src | sort -nr | head -20`"
  per the issue's "정적분석 보강" section).
- invariants: a route handler must produce the same response shape on the PG
  path as it did on the SQLite path during the migration window.
- allowed_changes: `bugfix` only; `extraction` is the cleanup itself.
- tests: PG-only suite plus per-route tests under `src/server/routes/*`.
- related_issues: #1238 (primary), #1237 (prereq), #1239 (final dependency
  removal).

## `/api/inflight/rebind`

- feature: `tmux_watcher / orphan_recovery`
- canonical_modules:
  - Route: `src/server/routes/health_api.rs:684`.
  - Body parsing and handler: `src/services/discord/health.rs:2449`,
    `:2509`.
  - Inflight state writer: `src/services/discord/inflight.rs:415`,
    constants and gates at `:107`, `:952`, `:964`.
  - Tmux-side adoption notes: `src/services/discord/tmux.rs:5059`,
    `:5456`, `:5458`.
- legacy_modules: none — this route is the canonical orphan-recovery entry
  point per #896.
- do_not_edit_without_migration_plan: it is NOT legacy. It is listed here
  because agents have repeatedly tried to add other inflight writers; this
  page makes the contract explicit.
- active_callsite_coverage: n/a.
- invariants: `/api/inflight/rebind` is the ONLY synthetic writer of an
  inflight state file. Other producers must run through the regular tmux
  watcher lifecycle.
- allowed_changes: `bugfix`, `extraction` (provided the single-writer
  invariant is preserved); no `new_feature` that introduces a parallel
  synthetic writer.
- tests: integration tests under
  `src/integration_tests/tests/high_risk_recovery.rs` and the inflight
  cancel/recovery suites.
- related_issues: #896 (origin), #1283 (cancel-induced death immediate
  re-attach), #1138 (lifecycle restructure).

## `cli/migrate/postgres_cutover.rs` Retention

- feature: `cli_runtime / pg_cutover_archive`
- canonical_modules: PG-only runtime; one-shot cutover code.
- legacy_modules: `src/cli/migrate/postgres_cutover.rs` (7669 lines, largest
  CLI module; retained for restoration / reference).
- do_not_edit_without_migration_plan: no new logic. The 추가 DoD on #1239
  states this file must be feature-gated or moved to a separate module
  before final SQLite-dependency removal.
- active_callsite_coverage: n/a.
- invariants: not on the production runtime path. CLI invocation only.
- allowed_changes: `bugfix` only inside #1239 scope.
- related_issues: #843, #1239.

## Update Cadence

When an entry's cleanup-owner issue closes, remove the entry from this page
in the same PR that closes the issue. New legacy paths discovered by
inventory generators (`scripts/generate_inventory_docs.py`) or audits should
be added with the cleanup owner pre-filed; do not park unowned legacy here.
