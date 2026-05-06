# Dispatch Mod Extraction Plan

> Last refreshed: 2026-05-06 (against #1785 extraction-planning pass).

Source issue: #1785
Epic: #1784

`src/dispatch/mod.rs` is currently 5,109 lines: about 955 lines of
production facade/helper code followed by about 4,154 lines of inline tests.
The dispatch directory already has extracted sibling modules, but `mod.rs`
still owns cancellation, row-summary projection, unified-thread guards, and the
legacy catch-all test suite.

## Scope Notes

- Do not start behavior extraction in #1785. This document plus the linked
  follow-up issues are the deliverable.
- Keep the public `crate::dispatch::*` API stable until a later cleanup issue
  deliberately narrows call sites.
- Preserve the existing split between runtime PG paths and SQLite test/backfill
  helpers. Do not add a SQLite runtime fallback.
- Keep caller-owned PG transaction semantics intact for cancellation helpers.
- `src/dispatch/mod.rs` should end as a small facade/re-export module, not as a
  new home for behavior.
- No worker-loop or rate-limit/backoff implementation lives in `mod.rs` today;
  those should not be invented during this split. Worktree/CWD production logic
  already lives in `dispatch_context.rs`; `mod.rs` currently holds many of its
  legacy tests.

## Existing Dispatch Subdomains

| Module | Current LOC | Responsibility |
| --- | ---: | --- |
| `dispatch_channel.rs` | 43 | Provider suffix routing and dispatch destination provider overrides. |
| `dispatch_context.rs` | 3,987 | Session strategy, target repo/worktree resolution, review target trust, merge-base context, and review quality guidance. |
| `dispatch_create.rs` | 2,877 | Dispatch creation, dedupe, active-dispatch reuse, attached intents, stale review-decision cancellation, and PG/SQLite create APIs. |
| `dispatch_status.rs` | 1,921 | Dispatch lifecycle/status transitions, completion/finalization, phase-gate verdict injection, status events, status reactions, and notify outbox glue. |

## Remaining `mod.rs` Subdomain Map

| Proposed module | Current source ranges | Approx. prod LOC | Approx. test LOC | Responsibility | Main consumers |
| --- | --- | ---: | ---: | --- | --- |
| `dispatch::mod` facade | `src/dispatch/mod.rs:1`, `src/dispatch/mod.rs:10`, `src/dispatch/mod.rs:15` | 70 retained | 0 | Module declarations and stable re-exports for the current public API. | All `crate::dispatch::*` callers. |
| `dispatch::types` | `src/dispatch/mod.rs:64` | 20 | 0 | `DispatchCreateOptions` and any future small shared DTOs. | `dispatch_create.rs`, routes, integration tests. |
| `dispatch::cancel` | `src/dispatch/mod.rs:71`, `src/dispatch/mod.rs:104`, `src/dispatch/mod.rs:249`, `src/dispatch/mod.rs:436`, `src/dispatch/mod.rs:572` | 560 | 600 | User/system cancel classification, SQLite and PG cancel/reset paths, caller-owned transaction variant, session active-dispatch cleanup, dispatch events, cancel outbox rows, auto-queue reset/user-cancelled handling, cancelled thread-map cleanup, and terminal-card cancel-without-requeue. | `dispatch_create.rs`, `kanban.rs`, queue services, auto-queue command service, review verdict routes, DB schema cleanup. |
| `dispatch::summary` | `src/dispatch/mod.rs:638`, `src/dispatch/mod.rs:680`, `src/dispatch/mod.rs:724`, `src/dispatch/mod.rs:830`, `src/dispatch/mod.rs:863` | 240 | 80 | Parse result/context JSON, normalize human summaries, map known dispatch codes, and derive noop/rework/orphan/cancel/verdict summaries. | `dispatch_create.rs`, dispatch query paths, `services/dispatches`, kanban card listing. |
| `dispatch::query` | `src/dispatch/mod.rs:876`; PG mirror currently starts at `src/dispatch/dispatch_create.rs:437` | 130 | 40 | SQLite and PG dispatch row projection, parsed JSON fields, retry count, and `result_summary` injection. | `dispatch_status.rs`, `dispatch_create.rs`, dispatch routes. |
| `dispatch::channel` unified-thread helpers | `src/dispatch/mod.rs:924`; fold into existing `src/dispatch/dispatch_channel.rs` | 30 moved | existing provider tests | Active unified-thread channel checks, thread-channel ID parsing, channel-name guard, and kill-signal drain. | `services/provider.rs`, Discord delivery/runtime guards. |
| `dispatch::tests::support` | `src/dispatch/mod.rs:956` | 0 | 260 | Shared unit-test DB, repo, git, env override, outbox counter, event loader, and assistant-response seed helpers. | All dispatch unit test modules. |
| Owner test modules for create/status/cancel | `src/dispatch/mod.rs:1214`, `src/dispatch/mod.rs:1454`, `src/dispatch/mod.rs:1912`, `src/dispatch/mod.rs:4961` | 0 | 1,450 | Creation, completion evidence, origin-main baseline, cancellation, user-cancelled entries, sidecar phase gates, session strategy defaults, outbox/status event behavior, finalization, dedupe, terminal-card behavior. | `dispatch_create`, `dispatch_status`, `dispatch_cancel`, `dispatch_channel`. |
| Owner test modules for context/review | `src/dispatch/mod.rs:2964` | 0 | 1,875 | Card repo mapping, explicit worktree context, stale/deleted worktree refresh, review target trust, merge-base context, issue-commit membership, dirty repo fallback rejection, external target repos, noop latest work dispatches, and review checklist injection. | `dispatch_context`. |

## Recommended Extraction Order

1. #1808 `dispatch/mod: extract shared types and test support`
   - Lowest behavior risk and unlocks later test movement.
   - Acceptance focus: stable `DispatchCreateOptions` path, no SQL changes,
     shared fixtures available to later modules.

2. #1809 `dispatch/mod: extract result summary normalization`
   - Pure JSON/text logic with narrow tests.
   - Move before row projection because query mapping depends on summary
     generation.

3. #1810 `dispatch/mod: extract dispatch row projection`
   - Pairs SQLite `query_dispatch_row` with the PG mirror currently in
     `dispatch_create.rs`.
   - Keeps `dispatch_create` and `dispatch_status` from depending on `mod.rs`
     helper bodies.

4. #1811 `dispatch/mod: fold unified-thread guards into channel module`
   - Smallest remaining runtime slice.
   - Reuses the existing channel module instead of creating another tiny file.

5. #1812 `dispatch/mod: extract cancellation lifecycle`
   - Highest-risk production move because it touches status transitions,
     dispatch events, outbox rows, auto-queue status, sessions, and thread-map
     cleanup.
   - Do after the pure/query/channel slices so the final high-risk module is
     isolated and easier to review.

6. #1813 `dispatch/mod: relocate create status and dedup tests`
   - Moves the non-context inline test suites to the modules that own their
     behavior.
   - This should remove most remaining test-only bulk from `mod.rs` after
     cancellation/summary/query tests have moved with their modules.

7. #1814 `dispatch/mod: relocate worktree and review context tests`
   - Largest test-only move. Keep last so shared fixtures and owner test files
     are already established.
   - Prefer `src/dispatch/tests/context.rs`-style files over making
     `dispatch_context.rs` larger unless the owner issue chooses otherwise.

## Dependency Rules

- `dispatch_query` may depend on `dispatch_summary`; `dispatch_summary` should
  stay DB-free.
- `dispatch_create` and `dispatch_status` should depend on `dispatch_query`,
  not on helper bodies left in `mod.rs`.
- `dispatch_cancel` may call status/outbox helpers and auto-queue persistence,
  but dispatch creation should only call its stable public functions.
- `dispatch_channel` should stay independent of cancellation and creation.
- Test support may depend on the facade API, but production modules must not
  depend on test-only modules.
- `mod.rs` should re-export current public API during the split and avoid new
  behavior after these issues land.
