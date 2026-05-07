# Auto-Queue DB Extraction Plan

> Last refreshed: 2026-05-07 (after #1803/#1804 production extraction).

Source issues: #1782 epic, #1783 mapping proposal.

The former `src/db/auto_queue/core.rs` production monolith has been split behind
the `src/db/auto_queue/mod.rs` facade. Public `crate::db::auto_queue::*` imports
remain stable while production slices now live in `queries`, `phase_gates`,
`slots`, `claim`, `runs`, `entries`, and `consultation`. The remaining large
surface is test-only: `src/db/auto_queue/tests.rs` keeps the migrated PG
invariant tests together until a later cleanup moves them beside their owning
modules.

## Scope Notes

- Do not bundle behavior changes into extraction work.
- #1797 created the facade shell; #1798-#1804 split the production code into
  sibling modules behind re-exports.
- Preserve public function names during the split. Existing service, route,
  dispatch, supervisor, and GitHub-sync call sites should not need import
  changes unless a follow-up issue intentionally tightens them.
- Keep production behavior PG-only. Do not add SQLite fallback paths.
- Test ownership is the remaining cleanup: move groups from
  `src/db/auto_queue/tests.rs` into the modules they protect once the production
  split has settled.

## Subdomain Map

| Module | Current source | Approx. prod LOC | Responsibility | Main consumers |
| --- | --- | ---: | --- | --- |
| `auto_queue::mod` | `src/db/auto_queue/mod.rs` | 20 | Facade, module wiring, stable re-exports. | All `crate::db::auto_queue::*` callers. |
| `auto_queue::queries` | `src/db/auto_queue/queries.rs` | 484 | Read-only filters, record structs, run/status/history/backlog/generate reads, card status counts, row mappers. | `src/services/auto_queue.rs`, `src/services/auto_queue/view_admin_routes.rs`. |
| `auto_queue::phase_gates` | `src/db/auto_queue/phase_gates.rs` | 358 | Batch phase eligibility, blocking gate reads, phase-gate state normalization, advisory lock, stale cleanup, save/clear persistence. | `src/services/auto_queue/activate_command.rs`, `src/engine/ops/auto_queue_ops.rs`, run finalization. |
| `auto_queue::slots` | `src/db/auto_queue/slots.rs` | 197 | Slot pool sizing, slot row creation, inactive assignment cleanup, run-wide and targeted release, active dispatch checks. | `activate_command`, `planning`, `slot_routes`, engine auto-queue ops. |
| `auto_queue::claim` | `src/db/auto_queue/claim.rs` | 688 | Group pending discovery, first pending selection, assigned group reuse, slot allocation/rebind CAS loops, group metadata sync. | `src/services/auto_queue/activate_command.rs`, `src/services/auto_queue/fsm.rs`, slot admin routes. |
| `auto_queue::runs` | `src/db/auto_queue/runs.rs` | 275 | Run pause/resume/complete, ready-to-finalize policy, review-disabled completion hook, completion notification outbox target selection. | `src/engine/ops/auto_queue_ops.rs`, `src/github/sync.rs`, entry terminal sync. |
| `auto_queue::entries` | `src/db/auto_queue/entries.rs` | 1,436 | Entry lifecycle persistence, status constants/types, transition allowlist, optimistic update/retry, terminal dispatch sync, dispatch-failure retry accounting, reactivation, transition audit, dispatch history, latest Codex session lookup. | Dispatch status, supervisor, kanban transitions, GitHub sync, auto-queue FSM/activation/planning/admin. |
| `auto_queue::consultation` | `src/db/auto_queue/consultation.rs` | 120 | Consultation metadata merge and atomic card metadata plus entry-dispatched update. | `src/services/auto_queue/fsm.rs`, `src/engine/ops/auto_queue_ops.rs`. |
| `auto_queue::test_support` / `tests` | `src/db/auto_queue/test_support.rs`, `src/db/auto_queue/tests.rs` | test-only | Shared isolated PG database lifecycle plus migrated invariant tests. | Per-module auto_queue DB tests. |

## Recommended Extraction Order

All production extraction items below are complete as of #1803/#1804. The
remaining cleanup is test placement, not production DB ownership.

1. #1797 `auto_queue db: create facade module shell and shared test support`
   - Lowest behavior risk and unblocks directory modules.
   - Acceptance focus: no SQL changes, stable public API, compile-only import
     stability, shared PG fixture available for later issues.

2. #1798 `auto_queue db: extract read query records`
   - Mostly read-only SQL and DTO mapping. This is the lowest-risk production
     split after the facade.
   - Keeps dashboard/status consumers stable while reducing the middle of the
     giant file.

3. #1799 `auto_queue db: extract phase gate persistence`
   - Self-contained write domain with strong tests for idempotency, stale-row
     rollback, and advisory-lock ordering.
   - Extract before run lifecycle because `maybe_finalize_run_if_ready_pg`
     must check blocking phase gates.

4. #1800 `auto_queue db: extract slot lifecycle persistence`
   - Separates low-level slot row operations from higher-level claim policy.
   - Provides the dependency boundary that the claim module should call instead
     of duplicating slot SQL.

5. #1801 `auto_queue db: extract group claim and allocation helpers`
   - Keeps the concurrency-sensitive CAS loop together with pending-group
     selection and slot-index binding.
   - Should run the single-slot concurrency, same-run rebind, cross-run reclaim,
     and bounded retry tests.

6. #1802 `auto_queue db: extract run lifecycle and completion notifications`
   - Moves run state writers after phase/slot dependencies exist.
   - Must preserve the invariant that `done` entry transitions do not finalize
     review-enabled runs or release slots until the policy hook says so.

7. #1803 `auto_queue db: extract entry status lifecycle and dispatch history`
   - Largest and most coupled slice, so delay until supporting modules are
     already isolated.
   - Depends on `runs` for finalization and on shared status/types for an
     acyclic module graph.

8. #1804 `auto_queue db: extract consultation dispatch persistence`
   - Small composition layer that should move last because it calls the entry
     transaction helper.
   - Verifies atomic card metadata update plus entry transition behavior.

## Test Migration Map

| Test area | Move with |
| --- | --- |
| Resume-session context parsing | `entries` or shared status/types if the helper stays shared. |
| Phase gate idempotency, rollback, concurrent clear, stale filtering, clear | `phase_gates`. |
| Completed dispatch finalizer, review-disabled completion, paused blocking gate, user-cancelled non-finalization | Split between `entries` terminal sync and `runs`; keep assertions close to the public helper under test. |
| Entry transitions, stale retry, restore allow/block rules, dispatch history, latest Codex session, done reactivation | `entries`. |
| Slot allocation concurrency, same-run rebind, cross-run reclaim, bounded CAS retry, slot release, active dispatch check | `slots` and `claim` according to the helper under test. |
| Consultation dispatch metadata and validation | `consultation`. |

## Dependency Rules

- `entries` may call `runs::maybe_finalize_run_if_ready_pg`; `runs` should not
  call `entries`.
- `claim` may call `slots` for low-level slot persistence; `slots` should not
  call `claim`.
- `runs` may call `phase_gates` to ask whether completion is blocked.
- `consultation` may call `entries::update_entry_status_on_pg_tx`.
- `queries` should remain read-only and should not depend on write modules.
- `mod.rs` should re-export the current public API until downstream call sites
  are deliberately narrowed in later cleanup.
