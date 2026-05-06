# Kanban Extraction Plan

> Last refreshed: 2026-05-06 (against #1787 extraction-planning pass).

Source issue: #1787
Epic: #1786

`src/kanban.rs` is currently 4,041 lines: about 2,400 lines of production
transition orchestration and compatibility helpers followed by about 1,640
lines of inline tests. It is a runtime coordination surface, not the primary
home for card CRUD persistence.

## Scope Notes

- Do not start behavior extraction in #1787. This document plus the linked
  follow-up issues are the deliverable.
- Keep the public `crate::kanban::*` API stable until a later cleanup issue
  deliberately narrows call sites.
- Keep runtime transition behavior PG-only. The SQLite helpers in this file are
  test/legacy compatibility paths behind `#[cfg(all(test,
  feature = "legacy-sqlite-tests"))]` and should move with their owner modules
  without becoming runtime fallbacks.
- Card CRUD, listing, card metadata, and low-level card transition persistence
  are already owned outside this file by `src/db/kanban_cards/*`,
  `src/services/kanban_cards.rs`, `src/server/dto/kanban.rs`, and
  `src/server/routes/kanban.rs`. Do not move that logic into the new
  `kanban` module tree.
- `src/kanban.rs` should end as a small facade/re-export module plus module
  wiring. New state-machine behavior should land in the extracted owner
  modules, not in the facade.

## External Kanban Owners

| Surface | Current module(s) | Responsibility |
| --- | --- | --- |
| Card persistence and metadata | `src/db/kanban_cards/{crud,listing,metadata,transitions}.rs` | Card create/update/listing, metadata mutation, and low-level card status persistence. |
| HTTP and operator workflows | `src/server/routes/kanban.rs`, `src/server/dto/kanban.rs` | Route validation, request/response DTOs, admin actions, and UI-facing card operations. |
| Engine operation bridge | `src/engine/ops/kanban_ops.rs` | Engine-dispatched kanban operations that call the stable `crate::kanban::*` facade. |
| Service helpers | `src/services/kanban_cards.rs`, `src/services/auto_queue.rs` | Service-level card helpers and auto-queue coordination triggered by card state. |

## Remaining `src/kanban.rs` Subdomain Map

| Proposed module | Current source ranges | Approx. prod LOC | Approx. test LOC | Responsibility | Main consumers |
| --- | --- | ---: | ---: | --- | --- |
| `kanban::mod` facade | `src/kanban.rs:1` | 70 retained | 0 | Module declarations, stable re-exports, and shared constants only. | All `crate::kanban::*` callers. |
| `kanban::test_support` | `src/kanban.rs:2408` through `src/kanban.rs:2739` | 0 | 330 | Isolated PG database lifecycle, env guards, script fixtures, and card/dispatch seed helpers. | Per-owner kanban unit tests. |
| `kanban::transition` | `src/kanban.rs:635` through `src/kanban.rs:1121`, `src/kanban.rs:1122` | 500 | 180 | PG transition entrypoints, caller-owned transaction variants, active-dispatch and latest-verdict gates, pipeline decision invocation, policy intent execution, and transition result DTO. | Routes, dispatch status/finalization, auto-queue services, engine kanban ops, integration tests. |
| `kanban::cleanup` | `src/kanban.rs:143` through `src/kanban.rs:162`, `src/kanban.rs:275` through `src/kanban.rs:462`, `src/kanban.rs:547` through `src/kanban.rs:713` | 580 | 260 | Escalation alert clearing, stale worktree metadata scrubbing, auto-queue live entry checks, force-transition link/revert cleanup, live dispatch cancellation, and allowlisted cleanup execution. | `kanban::transition`, auto-queue terminal sync tests, dispatch cleanup callers. |
| `kanban::terminal_cleanup` | `src/kanban.rs:163` through `src/kanban.rs:274`, `src/kanban.rs:1303` through `src/kanban.rs:1369` | 190 | 250 | Terminal managed-worktree cleanup counts, managed worktree metadata removal, terminal cleanup reason, and terminal follow-up helpers. | `kanban::transition`, dispatch cancellation, auto-queue terminal-card paths. |
| `kanban::hooks` | `src/kanban.rs:1132` through `src/kanban.rs:1213`, `src/kanban.rs:1370` through `src/kanban.rs:1839` | 550 | 420 | Dynamic hook execution, pending side-effect drain, event/state/enter/transition hook wrappers, hook pipeline resolution, and PG hook implementation. | `kanban::transition`, engine hook policy, outbox/event tests. |
| `kanban::github_sync` | `src/kanban.rs:1214` through `src/kanban.rs:1302`, `src/kanban.rs:1840` through `src/kanban.rs:1976` | 95 | 140 | GitHub sync trigger and target lookup plus legacy test-cfg audit logging helpers. | `kanban::transition`, GitHub sync worker, audit tests. |
| `kanban::review_tuning` | `src/kanban.rs:1977` through `src/kanban.rs:2400` | 420 | 160 | Review true-negative recording on pass-to-done and false-negative correction/category backfill on reopen. | `kanban::transition`, review verdict routes, tuning persistence. |
| Owner test modules | `src/kanban.rs:2741` through `src/kanban.rs:4041` | 0 | 1,300 | Transition gate tests, force override tests, terminal sync tests, hook/outbox tests, cleanup tests, GitHub sync tests, and review tuning tests moved beside owner modules. | Extracted kanban owner modules. |

## Recommended Extraction Order

1. #1818 `kanban: create facade module shell and shared test support`
   - Required first because Rust cannot keep `src/kanban.rs` and
     `src/kanban/` as the same module.
   - Acceptance focus: stable public API, no behavior changes, shared PG
     fixture available for later owner tests.

2. #1819 `kanban: extract transition core and public wrappers`
   - Moves the public entrypoints and transition result DTO behind the facade
     while the remaining helper bodies can still be called through the parent
     module during the split.
   - Acceptance focus: transition gates, caller-owned transaction semantics,
     and policy intent execution remain unchanged.

3. #1820 `kanban: extract transition cleanup policies`
   - Pulls cleanup writes out of the facade after the transition owner exists.
   - Acceptance focus: allowlisted cleanup, auto-queue live entry checks,
     dispatch cancellation, and force-transition cleanup stay transactionally
     scoped.

4. #1821 `kanban: extract terminal managed-worktree cleanup`
   - Separates terminal-card/worktree cleanup from generic transition cleanup.
   - Acceptance focus: terminal sync and managed-worktree metadata scrub tests.

5. #1822 `kanban: extract hook firing and side-effect drain`
   - Groups dynamic hooks, state/event/enter/transition wrappers, and pending
     intent drain in one owner.
   - Acceptance focus: hook ordering, outbox rows, and PG/legacy test-cfg split.

6. #1823 `kanban: extract GitHub sync and audit logging`
   - Isolates external sync target lookup and legacy audit helpers.
   - Acceptance focus: sync target selection and audit row behavior.

7. #1824 `kanban: extract review tuning outcomes`
   - Moves tuning writes after the transition and sync boundaries are clear.
   - Acceptance focus: pass-to-done true negatives and reopen false-negative
     correction remain no-ops when review context is absent.

8. #1825 `kanban: relocate inline tests by owner`
   - Final drain of the large inline test block after owner modules and shared
     fixtures exist.
   - Acceptance focus: the facade keeps only module wiring and any necessary
     public API smoke tests.

## Test Migration Map

| Test area | Move with |
| --- | --- |
| Transition authorization, allowed cleanup, force override, latest-verdict gate | `kanban::transition` plus `kanban::cleanup` for cleanup-specific assertions. |
| Terminal-card dispatch sync, managed worktree cleanup, started-at preservation | `kanban::terminal_cleanup`. |
| Dynamic hooks, event hooks, enter/state/transition hooks, pending intent drain, outbox assertions | `kanban::hooks`. |
| GitHub sync target lookup and legacy audit behavior | `kanban::github_sync`. |
| True-negative recording and false-negative correction on reopen | `kanban::review_tuning`. |
| PG fixture lifecycle, env guards, script fixtures, seed helpers | `kanban::test_support`. |

## Dependency Rules

- `kanban::transition` may call `cleanup`, `terminal_cleanup`, `hooks`,
  `github_sync`, and `review_tuning`; those modules should not call back into
  transition decision entrypoints.
- `kanban::cleanup` may depend on dispatch and auto-queue persistence helpers,
  but it should not fire hooks or GitHub sync directly.
- `kanban::terminal_cleanup` owns managed-worktree cleanup semantics and should
  not absorb unrelated transition cleanup.
- `kanban::hooks` may resolve pipelines and enqueue side effects, but it should
  not perform GitHub sync or review tuning writes.
- `kanban::github_sync` should keep target selection DB reads separate from the
  transition decision path.
- `kanban::review_tuning` should remain a post-transition outcome recorder and
  stay independent from pipeline transition policy.
- Test support may depend on the facade API, but production modules must not
  depend on test-only modules.
