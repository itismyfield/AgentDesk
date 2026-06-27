# Known Legacy

> Code paths that are intentionally legacy on `main`. Each entry has a cleanup
> owner issue. Touch these paths only inside the scope of the listed issue or
> for a single narrow bugfix; do not extend them with new logic.
>
> Last refreshed: 2026-05-18 (against #2535 legacy outbound bridge removal).
> Baseline note: legacy SQLite adapter crates are absent from the default
> `cargo tree`.

## Schema Reminder

Every entry uses the common §8 schema: `feature`, `canonical_modules`,
`legacy_modules`, `do_not_edit_without_migration_plan`,
`active_callsite_coverage`, `invariants`, `allowed_changes`, `tests`,
`related_issues`.

## Retired `legacy_db()` Helper

- feature: retired `db_layer` compatibility handle history.
- canonical_modules: PG pool via `state.pg_pool_ref()` and per-domain PG
  query modules.
- legacy_modules: removed per-route shim functions historically named
  `legacy_db`:
  - `src/server/routes/onboarding.rs:17`
  - `src/server/routes/kanban.rs:16`
  - `src/server/routes/review_verdict/verdict_route.rs:8`
  - `src/services/review_decision/repo_card.rs:38` (the
    `review_state_db` stub — always `None` since #1384; relocated from the
    former `decision_route.rs` monolith in the #3038 slice-1 split and S1
    service relocation)
  - Plus the engine-side definition `src/engine/mod.rs:436`:
    `pub(crate) fn legacy_db(&self) -> Option<&Db>`.
  - Server-side reads at `src/server/mod.rs:158`, `:365`, `:376`.
- do_not_edit_without_migration_plan: do not add a new retired DB shim. New
  routes use `state.pg_pool_ref()`.
- active_callsite_coverage: tracked historically by #1238/#1239 and completed
  by the SQLite sunset cleanup.
- invariants: route handlers preserve the response shape captured during the
  migration window while using PostgreSQL as the only live backend.
- allowed_changes: historical note updates only.
- tests: PG-only test suite under `src/integration_tests/postgres_only/*`.
- related_issues: #843, #1237, #1238, #1239.

## Retired SQLite Fallback Branches in Routes

- feature: retired `db_layer` SQLite fallback history.
- canonical_modules: PG-only handlers under `src/server/routes/*.rs`.
- legacy_modules: removed `state.legacy_db()` route arms from the onboarding,
  kanban, review-verdict, and auto_queue migration era.
- do_not_edit_without_migration_plan: do not add a new SQLite arm; route work
  must stay PostgreSQL-owned.
- active_callsite_coverage: baseline grep was owned by #1239 static analysis;
  #1438 removed the default SQLite dependency surface and #3035 completed the
  sunset.
- invariants: a route handler must preserve the response shape captured during
  migration while using the PG path as the only live control-plane backend.
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
- tests: PG-only high-risk recovery suite under
  `src/high_risk_recovery.rs` and the inflight
  cancel/recovery suites.
- related_issues: #896 (origin), #1283 (cancel-induced death immediate
  re-attach), #1138 (lifecycle restructure).

## Update Cadence

When an entry's cleanup-owner issue closes, remove the entry from this page
in the same PR that closes the issue. New legacy paths discovered by
inventory generators (`scripts/generate_inventory_docs.py`) or audits should
be added with the cleanup owner pre-filed; do not park unowned legacy here.
