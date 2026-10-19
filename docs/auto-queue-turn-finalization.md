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
| `auto_queue_runs.status = completed` | `finalize_run_if_ready_on_pg_tx` (derived), operator `end` force-completion, destructive reset | auto-queue dashboard/API status, later phase dispatch gating | derived from entry and phase-gate readiness |
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
| Auto-queue run completion | `finalize_run_if_ready_on_pg_tx` | Pure derivation from entries and phase gates; see "Run Completion Writer". |
| Discord mailbox cleanup | mailbox finish/stop helpers | Cleans runtime state after canonical completion/cancel has recorded durable state. |
| Watchdog | detector/reconciler | Emits suspicion/timeout and invokes cancel/recovery; never marks work complete directly. |

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
- Preserve `user_cancelled` as resumable and non-run-finalizing.
- Keep `finalize_run_if_ready_on_pg_tx` as the only derived run completion
  writer (`maybe_finalize_run_if_ready_pg` is its boolean view).
- Rollback point: disable the new entry update and leave existing policy/GitHub
  cleanup fallbacks in place.

### #1640 - Watchdog As Detector/Reconciler

- Remove direct completion semantics from watchdog paths.
- Make timeout handling invoke the same cancel/recovery finalizer used by
  explicit operator cancel and restart recovery.
- Keep watchdog observability and prealert behavior intact.
- Rollback point: restore the current watchdog stop path while retaining the
  canonical normal-completion finalizer.

## Run Completion Writer

Derived completion has exactly one writer, `finalize_run_if_ready_on_pg_tx`
in `src/db/auto_queue/runs.rs`. Every caller (terminal-entry writers, GitHub
sync, phase-gate reconciliation, the policy `completeRun` facade, the activate
tail and the empty-run order/activate paths) reaches it directly or through
`finalize_run_if_ready_pg`, which owns a transaction and takes the blocking run
token first. The writer returns `Completed | Blocked(reason) | AlreadyTerminal`
and never reports `Completed` for a zero-row update.

Inside one transaction, after the `aq_run:<run_id>` token, it refuses when any
of these hold, in this order: a pending/failed phase gate, an open
`phase_gate_grace_until` window, a `user_cancelled` entry, or a
`pending`/`dispatched` entry. Only then does it flip the run to `completed`,
release the run's slots and queue the completion notification. The
notification is queued only on the transition, so repeated calls are
idempotent (`AlreadyTerminal`) and cannot duplicate it. A run that never held
an entry completes silently.

Force-completion is a separate operator command: `POST /api/queue/runs/{id}/end`
(`end_run_with_pg` -> `terminalize_selected_runs_with_pg`, audit source
`auto_queue_end`). It is the only completion path allowed to delete phase gates
and skip live entries; the generic `PATCH /api/queue/runs/{id}` rejects
`status: completed`. The destructive reset paths in
`services/auto_queue/command.rs` remain separate admin owners. The
`run_completion_contract_tests` module pins the set of files that may contain
a `completed` status write.

## Verification Plan

- Unit tests for the canonical finalizer input contract and idempotent repeated
  completion.
- Postgres tests covering dispatch completed, entry terminal, phase-gated run
  completion, and `user_cancelled` non-finalization.
- Route or integration tests for watcher completion, bridge fallback completion,
  watchdog timeout cancellation, and GitHub/card terminal cleanup fallback.
- Observability assertions that completion events are emitted once per terminal
  transition and include the same correlation id across dispatch, entry, and run
  layers.
