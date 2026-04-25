# Runtime Invariants

This document records runtime invariants that must hold across the AgentDesk
Discord runtime. Violations should surface through `debug_assert!` in dev/test
when the condition is expected to be certain, or through `tracing::error!` plus
the observability invariant counter when a transient runtime race is possible.

Invariant violations are emitted as `observability_events.event_type =
invariant_violation`, counted through the existing observability guard counter
path, and exposed at `GET /api/analytics/invariants`.

## Core Invariants

| Invariant | Rule | Authoritative code | Runtime guard |
| --- | --- | --- | --- |
| `watcher_one_per_channel` | A Discord channel may have at most one live tmux watcher handle. Replacement must cancel the stale handle before installing the new one. | `src/services/discord/tmux.rs:1986`, `src/services/discord/tmux.rs:2023` | `debug_assert!` and invariant counter at `src/services/discord/tmux.rs:2004`, `src/services/discord/tmux.rs:2081`. |
| `inflight_tmux_one_to_one` | One tmux session name must not be owned by multiple inflight state files, and one channel's inflight file must not drift to a different tmux session mid-turn. | `src/services/discord/inflight.rs:254`, `src/services/discord/inflight.rs:575` | Soft invariant counter at `src/services/discord/inflight.rs:312` and duplicate-owner detection at `src/services/discord/inflight.rs:635`. |
| `response_sent_offset_monotonic` | `response_sent_offset` only advances within a turn. It must not move backwards when the turn bridge or restored watcher persists delivery progress. | `src/services/discord/turn_bridge/mod.rs:244`, `src/services/discord/tmux.rs:2152`, `src/services/discord/inflight.rs:254` | `debug_assert!` and invariant counter at `src/services/discord/turn_bridge/mod.rs:263`, `src/services/discord/tmux.rs:2181`, `src/services/discord/inflight.rs:292`. |
| `response_sent_offset_in_bounds` | `response_sent_offset` must stay on a UTF-8 boundary within `full_response`. | `src/services/discord/turn_bridge/mod.rs:244`, `src/services/discord/tmux.rs:2152`, `src/services/discord/inflight.rs:254` | `debug_assert!` and invariant counter at `src/services/discord/turn_bridge/mod.rs:285`, `src/services/discord/tmux.rs:2200`, `src/services/discord/inflight.rs:267`. |
| `tmux_confirmed_end_monotonic` | The tmux relay `confirmed_end_offset` watermark only advances and must reach the committed tmux output end after a direct delivery or bridge handoff. This is the tmux-output counterpart to `response_sent_offset`; the two are different units and must not be compared directly. | `src/services/discord/turn_bridge/mod.rs:299`, `src/services/discord/tmux.rs:3870` | `debug_assert!` and invariant counter at `src/services/discord/turn_bridge/mod.rs:337`, `src/services/discord/tmux.rs:3870`. |
| `mailbox_active_turn_matches_dispatch` | While a foreground Discord turn is active, the channel mailbox owns exactly one active turn token. Turn finalization must remove that token before queue follow-up dispatch starts. | `src/services/turn_orchestrator.rs:675`, `src/services/turn_orchestrator.rs:1102`, `src/services/discord/mod.rs:1025`, `src/services/discord/turn_bridge/mod.rs:1541` | Soft invariant counter at `src/services/discord/turn_bridge/mod.rs:1549`. |
| `turn_id_unique_within_session` | A persisted turn id is `discord:{channel_id}:{user_msg_id}`. Discord message ids are unique within the channel/session scope, and zero ids are reserved for synthetic rebind state that must not create real turn rows. | `src/services/discord/turn_bridge/mod.rs:213`, `src/services/discord/recovery_engine.rs:409` | `debug_assert!` and invariant counter at `src/services/discord/turn_bridge/mod.rs:228`. |
| `auto_queue_slot_single_active_entry` | Within a single auto_queue_run, each `(agent_id, slot_index)` pair owns at most one entry in the `dispatched` state at any moment. A second dispatched entry on the same slot indicates either a stuck prior dispatch or a tick that failed to release the slot before dispatching the next card. | `src/services/auto_queue.rs:assemble_status_response`, `src/db/auto_queue.rs:allocate_slot_for_group_agent`, `src/db/auto_queue.rs:release_slot_for_group_agent` | Soft invariant counter at `src/services/auto_queue.rs:assemble_status_response` (no panic — this tripping means an auto_queue tick bug, not corruption). |
| `dispatch_outbox_retry_count_in_bounds` | `dispatch_outbox.retry_count` is a `bigint` on Postgres and is read as `i64` in Rust (`MAX_RETRY_COUNT = 4`). It must be non-negative and must not exceed `MAX_RETRY_COUNT + 1` (a transient +1 can exist right before the row flips to `failed`). Overflowing into i32 territory was the original bug that motivated widening the column; the assertion guards against either a schema regression or an accounting bug that advances retry_count past the failure threshold without flipping status. | `src/server/routes/dispatches/outbox.rs:150` (`MAX_RETRY_COUNT`), `src/server/routes/dispatches/outbox.rs:383` (tick loop) | Soft invariant counter at `src/server/routes/dispatches/outbox.rs:383`. |

## Lifecycle Invariants

| Invariant | Rule | Authoritative code | Runtime guard |
| --- | --- | --- | --- |
| `recovery_phase_valid` | Recovery phase values are restricted to `pending`, `watcher_reattach`, `inflight_restore`, and `done`; transition helpers must canonicalize persisted values through that enum. | `src/services/discord/recovery_engine.rs:30`, `src/services/discord/recovery_engine.rs:186`, `src/services/discord/recovery_engine.rs:214`, `src/services/discord/recovery_engine.rs:225` | Existing unit tests cover phase parsing and transition helpers. Recovery fires remain observable through `emit_recovery_fired` at `src/services/discord/recovery_engine.rs:2359`. |
| `recovery_mailbox_reregister_idempotent` | Restart recovery may re-register an active mailbox turn from inflight state, but repeated attempts must not create parallel active turns. | `src/services/discord/recovery_engine.rs:409`, `src/services/discord/recovery_engine.rs:419` | Covered by the mailbox single-token invariant and `reregister_active_turn_from_inflight` tests. |
| `dispatch_completion_single_authority` | All dispatch completion paths route through `finalize_dispatch` / `complete_dispatch_inner_with_backends` so evidence validation, DB status transition, hooks, and follow-ups share one lifecycle. | `src/dispatch/dispatch_status.rs:1077`, `src/dispatch/dispatch_status.rs:1342`, `src/dispatch/dispatch_status.rs:1345` | Existing dispatch result observability is emitted by the shared status transition path; this change does not rewrite the dispatch state machine. |
| `dispatch_outbox_single_delivery_worker` | Discord side effects for dispatch outbox rows originate from the outbox worker; other paths enqueue durable outbox rows and return. | `src/server/routes/dispatches/outbox.rs:334`, `src/server/routes/dispatches/outbox.rs:1662`, `src/server/routes/dispatches/outbox.rs:1697` | Existing outbox retry/backoff tests cover the lifecycle. No new runtime panic is introduced here. |

## Per-Invariant Notes

### `auto_queue_slot_single_active_entry`

Each auto_queue_run assigns cards to agents through a `(thread_group, slot_index)`
pool. The tick-based runtime allocates a slot via
`allocate_slot_for_group_agent`, flips the picked entry to `dispatched`, then
releases the slot in `release_slot_for_group_agent` once the entry reaches a
terminal status. If two entries under the same `(agent_id, slot_index)` appear
as `dispatched` concurrently, a tick has either failed to release the prior
slot, or it has picked a second entry before the prior dispatched entry
reached a terminal status. Both cases produce confused operator UI (the status
view shows two "active" entries in one slot) and desync the agent-side tmux
session binding. The check runs inside `assemble_status_response` — the same
path the HTTP status endpoint renders — so any operator query triggers the
observation without extra DB round trips.

### `dispatch_outbox_retry_count_in_bounds`

`dispatch_outbox.retry_count` was widened from `i32` to `bigint` to prevent a
prior overflow bug where retries accumulated beyond `i32::MAX` on a pinned
stuck row. The tick loop in `process_pending_dispatch_outbox` increments the
count on each notifier failure and flips the row to `failed` once the count
exceeds `MAX_RETRY_COUNT` (4). The assertion fires if a row leaks into the
pending scan with `retry_count < 0` (schema regression / signed-unsigned
confusion) or `retry_count > MAX_RETRY_COUNT + 1` (accounting bug that
bypassed the status transition to `failed`). The transient `+1` slack is
intentional: `new_count = retry_count + 1` is computed before the comparison
and before the status flip, so a correctly-behaving worker can read a row at
exactly `MAX_RETRY_COUNT + 1` once, and the observation should only fire when
a row is picked up again at that count.

## Observability Contract

- Emit invariant violations through `record_invariant_check` at `src/services/observability.rs:461`.
- Store the invariant key in `observability_events.status` with payload fields
  `invariant`, `code_location`, `message`, and `details`.
- Query counts and recent events through `query_invariant_analytics` at
  `src/services/observability.rs:884`.
- Expose the API via `GET /api/analytics/invariants` at
  `src/server/routes/analytics.rs:125`.

Release builds must not gain new panic paths from invariant checks. Use
`debug_assert!` only beside checks that are expected to be impossible in normal
execution; use `record_invariant_check` alone for lifecycle races or stale
runtime files that can temporarily exist during restart/recovery.
