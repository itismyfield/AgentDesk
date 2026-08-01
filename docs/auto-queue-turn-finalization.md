# Auto-Queue Turn Finalization Contract

Tracks issue #1586 and subissue #1637. This document inventories the current
sources of truth for "a turn is finished" before the implementation PRs make
one canonical finalizer responsible for all downstream completion effects.

## E2E Agent Mode Lane

Auto-queue sandbox preflight must default to a non-live lane. Use
`agent_mode: none` for static fixture/preflight checks and `agent_mode:
controlled` when a deterministic finalizer stub drives the transition. These
lanes must not contact a real provider and must not mutate production cards,
GitHub branches, Discord sessions, or live queue rows.

Any future live auto-queue smoke must opt in explicitly with `agent_mode:
real_live` and report `real_provider_contacted: true` plus provider/cell
identity. Required gates should fail when a scenario declares `controlled` or
`real_live` but only produces a shallower observed lane.

## Current Signal Inventory

| Signal | Current producers | Current consumers | Classification |
| --- | --- | --- | --- |
| `task_dispatches.status = completed` | `dispatch::finalize_dispatch`, `mark_dispatch_completed_pg_first`, review verdict routes, selected recovery fallbacks | policy hooks, GitHub sync, dashboard/API readers, auto-queue reconciliation | authoritative for dispatch completion, but not sufficient for run completion |
| `auto_queue_entries.status` terminal states | `update_entry_status_on_pg`, queue dispatch failure handling, skip/reorder/cancel routes | `maybe_finalize_run_if_ready_pg`, auto-queue status surfaces, phase gate logic | authoritative for entry completion and run readiness |
| `auto_queue_runs.status = completed` | `maybe_finalize_run_if_ready_pg`, terminal cleanup sweeps | auto-queue dashboard/API status, later phase dispatch gating | derived from entry and phase-gate readiness |
| `on_dispatch_completed` policy event | module-private `dispatch::complete_dispatch_inner_with_backends` after a successful status transition | policy engine hooks, review dispatch creation, card lifecycle side effects | derived side effect; must not be the primary truth |
| terminal-card cleanup (`auto_cancelled_on_terminal_card`, `js_terminal_cleanup`) | GitHub sync and policy/card terminal cleanup paths | auto-queue run finalization fallback and cleanup | fallback/reconciliation signal |
| watcher stream final hook | `tmux_watcher` after full response capture and dispatch evidence assembly | dispatch completion, follow-up queueing, Discord relay finalization | existing normal-completion path; intended future single canonical entry point |
| bridge completion guard/fallback | `turn_bridge::completion_guard` and high-risk recovery fallback paths | direct DB completion when the normal finalizer cannot run | fallback only |
| mailbox `finish_turn` sequence | Discord mailbox finish/stop/recovery helpers | inflight cleanup, watchdog deadline cleanup, active counter, deferred restart | authoritative for Discord turn cleanup, not dispatch/run completion |
| watchdog timeout | `message_handler` watchdog loops | cancel token, stop path, later recovery/reconciliation | detector/reconciler only; should not be a completion source |

## Current Drift Points

1. Dispatch completion and auto-queue entry completion are separate transitions.
   `task_dispatches.status = completed` means the worker dispatch ended, while
   `auto_queue_entries.status` determines whether the run can advance or close.
2. `on_dispatch_completed` is a side effect of dispatch status transition, but
   several paths intentionally skip or replace the hook. Hook delivery therefore
   cannot be used as the canonical completion truth.
3. Watcher and bridge paths can both reach dispatch completion code. The watcher
   path owns warm-session output, while bridge fallback paths still exist for
   recovery and cold-start edge cases.
4. Watchdog logic is currently capable of stopping a turn. It should only detect
   stale execution and hand control to the canonical cancel/reconcile path.
5. GitHub/card terminal cleanup can finalize or reconcile auto-queue runs after
   policy hook gaps. That is a fallback until run finalization is driven from
   one canonical terminal-entry path.

## Target Authority Model

| Layer | Authority after migration | Notes |
| --- | --- | --- |
| Streaming turn completion | canonical streaming-final hook | One hook receives full response, dispatch id, provider/channel, token usage, and completion source. |
| Dispatch completion | dispatch status transition helper | Called by the streaming-final hook and explicit fallback helpers only. |
| Auto-queue entry terminal state | `update_entry_status_on_pg` / terminal-entry helper | Entry status drives run readiness; no policy hook is required to close the run. |
| Auto-queue run completion | `maybe_finalize_run_if_ready_pg` | Transactional derivation from run status, blocking gates, grace, `user_cancelled`, and runnable entries; returns `Completed`, `Blocked(reason)`, or `AlreadyTerminal`. |
| Operator force completion | `force_complete_run_on_pg` | Separate command requiring operator and source; the only completion path allowed to bulk-delete valid gates, with an `audit_logs` record in the same transaction. Non-force repair may delete only orphan gates that have no matching dispatch. |
| Discord mailbox cleanup | mailbox finish/stop helpers | Cleans runtime state after canonical completion/cancel has recorded durable state. |
| Watchdog | detector/reconciler | Emits suspicion/timeout and invokes cancel/recovery; never marks work complete directly. |

## Completion Block Ownership

`RunCompletionBlockedReason` is a refusal to write `completed`, not a generic
retry promise. The enum beside the writer is the authoritative maintenance
contract: every new reason must name its resolver, failure signal, and slot
effect. This table mirrors that contract for operators.

| Reason | Resolution owner | Resolution-failure signal | Slot effect |
| --- | --- | --- | --- |
| `blocking_phase_gate` | Valid gate: terminal-dispatch reconcile, automatic. Orphan gate: operator HTTP repair, manual; no tick calls repair. | Typed readiness result; failed-gate alert and repair audit/summary where applicable. | Readiness does not release a slot. Normal gate creation pauses before creating gate dispatches, and pause releases the slot; an independently injected gate has no such guarantee. |
| `phase_gate_grace_window` | DB deadline expiry plus the bounded completion tick, automatic. App `Date.now()` writes the deadline while DB `NOW()` compares it, so finite clock skew can extend the wait. | Typed readiness result on each attempted finalization. | Readiness preserves current ownership. During the pre-gate continuation race the slot remains held until continuation clears the deadline or pauses; gate pause releases it. |
| `user_cancelled_entry` | Bounded tick changes otherwise drained, gate-free, grace-expired cancellations to `skipped`, automatic. | Typed readiness result; policy error logging if the tick transition fails. | An assigned slot remains held until the transition allows canonical completion to release it. |
| `runnable_entry` | Active-run dispatch tick, automatic. A final-phase block first resumes a paused run. `generated`/`pending` are outside that tick but are pre-activation. | Typed readiness result; warning if final-phase resume fails. | Readiness preserves current ownership. Pre-activation `generated`/`pending` runs have no assigned slot. |
| `run_status_not_completable` | No automatic resolver. A crash between `apply_restore_state_changes_pg` committing `restoring` (`fsm.rs:292-313`) and `finalize_restore_run_pg` (`fsm.rs:398-437`) requires an operator restore retry. This is pre-existing debt, not introduced by #4881. | Typed result only when readiness is explicitly called; a crash-stuck `restoring` run has no periodic sweep or alert. | The slot is deliberately retained across restore. |
| `policy_continuation_pending` | `onCardTerminal` policy continuation, automatic. | Typed terminal-entry result while continuation is pending. | Readiness retains it; continuation keeps it while dispatching more work, or releases it by completion/gate pause. |
| `run_not_found` | No resolver applies; the identifier is stale or invalid. | Typed readiness result to the caller. | No run-row slot transition is performed. |

## Migration Checklist

### #1638 - Canonical Streaming-Final Hook

- Add a small service boundary that accepts the completed stream payload and
  owns dispatch finalization plus follow-up queueing.
- Route watcher completion through that service first.
- Keep `complete_dispatch_inner_with_backends` encapsulated unless the new
  boundary proves it needs a narrower public wrapper.
- Keep bridge/recovery fallback callers on their existing helpers, but make the
  fallback result shape match the new service input.
- Rollback point: watcher can return to direct `dispatch::finalize_dispatch`
  while the new service remains unused.

### #1639 - Auto-Queue Completion From Canonical Finalizer

- After dispatch completion, update the matching auto-queue entry through the
  terminal-entry helper instead of relying on policy/card terminal side effects.
- Preserve `user_cancelled` as non-dispatchable and operator-resumable while runnable work remains; once a run is otherwise drained, the bounded tick normalizes it to `skipped` so the run and slot cannot remain pinned forever.
- Keep `maybe_finalize_run_if_ready_pg` as the only run completion writer.
- Rollback point: disable the new entry update and leave existing policy/GitHub
  cleanup fallbacks in place.

### #1640 - Watchdog As Detector/Reconciler

- Remove direct completion semantics from watchdog paths.
- Make timeout handling invoke the same cancel/recovery finalizer used by
  explicit operator cancel and restart recovery.
- Keep watchdog observability and prealert behavior intact.
- Rollback point: restore the current watchdog stop path while retaining the
  canonical normal-completion finalizer.

## Verification Plan

- Unit tests for the canonical finalizer input contract and idempotent repeated
  completion.
- Postgres tests covering dispatch completed, entry terminal, phase-gated run
  completion, immediate `user_cancelled` non-finalization, and bounded tick
  terminalization after the run drains.
- Route or integration tests for watcher completion, bridge fallback completion,
  watchdog timeout cancellation, and GitHub/card terminal cleanup fallback.
- Observability assertions that completion events are emitted once per terminal
  transition and include the same correlation id across dispatch, entry, and run
  layers.
