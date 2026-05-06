# Agent Maintenance Index

> Purpose: when an agent is asked to change AgentDesk, this directory tells it
> *where* the canonical surface is, *what* is mid-migration, and *what is
> intentionally legacy and must not absorb new logic*. It complements
> [`docs/source-of-truth.md`](../source-of-truth.md) (which answers "which
> file do I edit?" for runtime/config) by answering "which Rust module do I
> edit?" for code.
>
> Created per #1279. Update cadence: each PR that finishes a migration milestone
> or introduces a new mid-migration surface must update the relevant page in
> the same PR. The full table re-audit is run quarterly against the generated
> inventories below.

## Pages

- [`auto-queue-db-extraction-plan.md`](auto-queue-db-extraction-plan.md) —
  subdomain map and ordered extraction issues for splitting the giant
  `src/db/auto_queue.rs` persistence module (#1783).
- [`change-surfaces.md`](change-surfaces.md) — change-allowed surfaces, with
  canonical modules, giant-file flags, and `do_not_edit_without_migration_plan`
  list. Use this before adding logic to any new file.
- [`dispatch-mod-extraction-plan.md`](dispatch-mod-extraction-plan.md) —
  subdomain map and ordered extraction issues for shrinking
  `src/dispatch/mod.rs` into a facade (#1785).
- [`discord-outbound-migration.md`](discord-outbound-migration.md) — five
  production callsite families for Discord outbound (#1006). Each row marks
  `migrated | legacy | unknown`. New sends MUST use the v3 outbound; legacy
  bugfixes are permitted only on rows still flagged legacy.
- [`known-legacy.md`](known-legacy.md) — code paths that intentionally remain
  legacy, with the cleanup-owner issue number. Touch them only inside the
  scope of the listed issue or for narrow bugfix.
- [`multinode-transition.md`](multinode-transition.md) — transition map for
  moving AgentDesk from one dcserver node to leader/worker execution, including
  single-node assumptions, side-effect ownership, invariants, and #876-#884
  test gates.
- [`opencode-usability-spec.md`](opencode-usability-spec.md) — implementation
  contract for raising OpenCode's Discord-facing usability toward Claude/Codex
  parity, including prompt/output safety, SSE text handling, and MCP sync.
- [`post-stabilization-backlog.md`](post-stabilization-backlog.md) — Phase 3
  maintainability checklist deferred until relay recovery and CookingHeart
  two-node readiness are stable. Use this page to decide when P2 cleanup may be
  promoted into a runtime lane.

## Generated Companions (read-only)

- [`docs/generated/module-inventory.md`](../generated/module-inventory.md) —
  every Rust module with line count and giant-file flag. Source of truth for
  the giant-file list in `change-surfaces.md`.
- [`docs/generated/route-inventory.md`](../generated/route-inventory.md) — HTTP
  routes and the file/line where each is registered.
- [`docs/generated/worker-inventory.md`](../generated/worker-inventory.md) —
  background workers spawned at startup.
- [`docs/generated/db-file-duplication-audit.md`](../generated/db-file-duplication-audit.md)
  and [`docs/generated/policy-db-inventory.md`](../generated/policy-db-inventory.md)
  — cross-file duplication and policy DB usage.

Regenerate with:

```
python3 scripts/generate_inventory_docs.py
```

## Freshness Gate

Run `python3 scripts/check_agent_maintenance_docs.py` before landing changes
to migration-sensitive surfaces. The gate requires each guarded page to carry
the preferred ``Last refreshed: <date> (against `main` @ `<sha>`)`` header
shape and verifies the referenced commit is an ancestor of `HEAD`. When a
refresh is intentionally anchored to review context instead of a commit, use
`Last refreshed: <date> (against #<issue> <reason>)` or
`Last refreshed: <date> (manual: <reason>)`; those forms still get date
freshness checks but skip commit ancestry validation. The gate warns when
copied line counts in `change-surfaces.md` drift from
`docs/generated/module-inventory.md`, and requires the matching maintenance
page to be touched when guarded code globs change. That touch rule is a
presence check only; reviewers still confirm the content was refreshed. CI runs
this gate in `--warning-only` mode during the initial rollout for #1432; remove
that flag when the quiet-week promotion is approved.

## Maintainability Hard Gates

CI runs `python3 scripts/audit_maintainability.py --check` and blocks on these
four checks:

- `new_direct_discord_send_outside_allowed_exclusion`
  (`direct_discord_sends`) — no direct Discord send/edit call outside the
  outbound dispatcher baseline.
- `new_runtime_sqlite_or_legacy_db_reference` (`legacy_sqlite_refs`) — no new
  runtime SQLite or legacy DB reference outside the compat/migration paths.
- `new_source_of_truth_alias_write` (`source_of_truth_alias_writes`) — no new
  write to an alias path from `docs/source-of-truth.md`.
- `new_giant_file_without_change_surface_doc` (`giant_files`) — no production
  Rust file at or above 1000 lines unless `change-surfaces.md` names it.

`route_srp_violations`, `manual_json_row_mapping`, and
`limit_clamp_duplication` intentionally remain warning-only.

For new tuple-style JSON route handlers, prefer the light helpers in
`crate::utils::api` for common simple responses and API query bounds:
`bad_request`, `internal_error`, `not_found`, and `clamp_api_limit`. Keep using
`AppError` for routes that already return `AppResult` or need structured
`code/context` fields.

Allowlist format lives in `scripts/audit_allowlist.toml`. Entries are
repo-relative POSIX paths or `path:line` findings under the matching check key.
New exclusions for hard-gated checks must include a nearby comment that
references an open extraction or cleanup issue; do not add a blanket path
unless the check cannot produce stable line-level findings. For giant files,
prefer updating `change-surfaces.md` because that page is the source of truth
for allowed migration-sensitive surfaces.

## Schema (Common to All Pages)

Each row in `change-surfaces.md` and `known-legacy.md`, and each entry in
`discord-outbound-migration.md`, follows the schema below (per static-analysis
report §8). Any field that is not yet known is written as `unknown` rather
than omitted.

- `feature` — short slug (e.g. `discord_outbound`, `policy_engine`).
- `canonical_modules` — Rust module path(s) where new logic for this feature
  belongs.
- `legacy_modules` — modules that previously owned the feature and are being
  drained.
- `do_not_edit_without_migration_plan` — modules that compile and run today
  but are scheduled for replacement; new logic added here will be rolled back.
- `active_callsite_coverage` — for in-flight migrations, the current
  per-callsite status (`migrated | legacy | unknown`).
- `invariants` — properties that must hold across canonical and legacy paths
  during migration.
- `allowed_changes` — `bugfix`, `new_feature`, `extraction`, scoped per row.
- `tests` — the test file(s) that guard the invariants.
- `related_issues` — GitHub issue numbers that own the migration or cleanup.

## Cross-Reference

`docs/source-of-truth.md` lists this directory in its `Deprecated References`
section so that operators looking for the old "which file do I edit?" map find
the code-side companion. When this directory's contract or layout changes,
update the source-of-truth note in the same PR.

## Out-of-Scope (Follow-up Issues)

Per #1279, four additional pages are deferred to follow-up issues to keep this
landing reviewable:

- `pg-only-cleanup.md` — once #1237/#1238/#1239 land.
- `tmux-watcher-lifecycle.md` — depends on #1138 / #964 / #1222.
- `dashboard-read-models.md` — once dashboard read-model split is scoped.
- `policy-engine-guardrails.md` — once policy engine refactor is scoped.

Open a child issue under #1279 to add any of these.
