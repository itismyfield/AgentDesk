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
| `watcher_one_per_channel` | A Discord channel may have at most one live tmux watcher handle. Replacement must cancel the stale handle before installing the new one. | `src/services/discord/watchers/lifecycle/claims.rs` (`try_claim_watcher_for_host`, `claim_watcher_for_host`) | `debug_assert!` and invariant counter in the same two functions. |
| `inflight_tmux_one_to_one` | One tmux session name must not be owned by multiple inflight state files, and one channel's inflight file must not drift to a different tmux session mid-turn. | `src/services/discord/inflight/store.rs` (`validate_inflight_state_for_save_with_delivery_rewind_reason`) | Soft invariant counter in the same function and duplicate-owner detection in `src/services/discord/inflight/removal.rs` (`load_inflight_probe_with_scope`). |
| `response_sent_offset_monotonic` | `response_sent_offset` only advances within a turn. It must not move backwards when the turn bridge or restored watcher persists delivery progress. | `src/services/discord/turn_bridge/turn_analytics.rs` (`assert_response_sent_offset_progress`), `src/services/discord/inflight/store.rs` (`validate_inflight_state_for_save_with_delivery_rewind_reason`) | `debug_assert!` and invariant counter in both functions. |
| `response_sent_offset_in_bounds` | `response_sent_offset` must stay on a UTF-8 boundary within `full_response`. | `src/services/discord/turn_bridge/turn_analytics.rs` (`assert_response_sent_offset_progress`), `src/services/discord/tmux.rs` (`persist_watcher_stream_progress`), `src/services/discord/inflight/store.rs` (`validate_inflight_state_for_save_with_delivery_rewind_reason`) | `debug_assert!` and invariant counter in all three functions. |
| `tmux_confirmed_end_monotonic` | The tmux relay `confirmed_end_offset` watermark only advances and must reach the committed tmux output end after a direct delivery or bridge handoff. This is the tmux-output counterpart to `response_sent_offset`; the two are different units and must not be compared directly. | `src/services/discord/turn_bridge/terminal_delivery.rs` (`advance_tmux_relay_confirmed_end`), `src/services/discord/tmux.rs` (`advance_watcher_confirmed_end_inner`) | Invariant counter in both functions; `debug_assert!` in `advance_watcher_confirmed_end_inner`. |
| `mailbox_active_turn_matches_dispatch` | While a foreground Discord turn is active, the channel mailbox owns exactly one active turn token. Turn finalization must remove that token before queue follow-up dispatch starts. | `src/services/turn_orchestrator.rs` (`try_start_turn_kinded_result`, `finish_turn`), `src/services/discord/turn_bridge/post_loop_finalize.rs` (`run_post_loop_finalize`) | Soft invariant counter in `run_post_loop_finalize`. |
| `turn_id_unique_within_session` | A persisted turn id is `discord:{channel_id}:{user_msg_id}`. Discord message ids are unique within the channel/session scope, and zero ids are reserved for synthetic rebind state that must not create real turn rows. | `src/services/discord/turn_bridge/turn_analytics.rs` (`discord_turn_id`), `src/services/discord/router/turn_start.rs` (`discord_turn_id`), `src/services/discord/inflight.rs` (`turn_id_for_state`) | `debug_assert!` and invariant counter in `turn_analytics.rs` (`discord_turn_id`). |
| `auto_queue_slot_single_active_entry` | Within a single auto_queue_run, each `(agent_id, slot_index)` pair owns at most one entry in the `dispatched` state at any moment. A second dispatched entry on the same slot indicates either a stuck prior dispatch or a slot allocation that admitted a new card while an active dispatch still owned the slot. | `src/services/auto_queue.rs:assemble_status_response`, `src/db/auto_queue/claim.rs:allocate_slot_for_group_agent_pg`, `src/db/auto_queue/claim.rs:active_dispatch_slot_guard_sql`, `src/db/auto_queue/slots.rs:slot_has_active_dispatch_pg` | Soft invariant counter at `src/services/auto_queue.rs:assemble_status_response`; allocation SQL and post-claim checks reject slots with active entries or dispatches (no panic — this tripping means an auto_queue tick bug, not corruption). |
| `dispatch_outbox_retry_count_in_bounds` | `dispatch_outbox.retry_count` is a `bigint` on Postgres and is read as `i64` in Rust (`MAX_RETRY_COUNT = 4`). It must be non-negative and must not exceed `MAX_RETRY_COUNT + 1` (a transient +1 can exist right before the row flips to `failed`). Overflowing into i32 territory was the original bug that motivated widening the column; the assertion guards against either a schema regression or an accounting bug that advances retry_count past the failure threshold without flipping status. | `src/services/dispatches/outbox_queue.rs` (`MAX_RETRY_COUNT`, `process_outbox_batch_with_pg`) | Soft invariant counter in `src/services/dispatches/outbox_queue.rs` (`check_dispatch_outbox_retry_count_in_bounds`). |

## Lifecycle Invariants

| Invariant | Rule | Authoritative code | Runtime guard |
| --- | --- | --- | --- |
| `recovery_phase_valid` | Recovery phase values are restricted to `pending`, `watcher_reattach`, `inflight_restore`, and `done`; transition helpers must canonicalize persisted values through that enum. | `src/services/discord/recovery_engine.rs` (`RecoveryPhase`, `RecoveryPhase::from_str`) | Existing unit tests cover phase parsing and transition helpers. Recovery fires remain observable through `emit_recovery_fired` (`src/services/observability/emit.rs`), called from `src/services/discord/recovery_engine/restore_inflight.rs`. |
| `recovery_mailbox_reregister_idempotent` | Restart recovery may re-register an active mailbox turn from inflight state, but repeated attempts must not create parallel active turns. | `src/services/discord/recovery_engine/runtime.rs` (`reregister_active_turn_from_inflight`, `reregister_active_turn_from_inflight_inner`) | Covered by the mailbox single-token invariant and `reregister_active_turn_from_inflight` tests. |
| `dispatch_completion_single_authority` | All dispatch completion paths route through `finalize_dispatch_with_backends` / `complete_dispatch_inner_with_backends` so evidence validation, DB status transition, hooks, and follow-ups share one lifecycle. | `src/dispatch/dispatch_status.rs` (`finalize_dispatch_with_backends`, `complete_dispatch_inner_with_backends`) | Existing dispatch result observability is emitted by the shared status transition path; this change does not rewrite the dispatch state machine. |
| `dispatch_outbox_single_delivery_worker` | Discord side effects for dispatch outbox rows originate from the outbox worker; other paths enqueue durable outbox rows and return. | `src/services/dispatches/outbox_queue.rs` (`dispatch_outbox_loop`, `process_outbox_batch_with_pg`, `RealOutboxNotifier`) | Existing outbox retry/backoff tests cover the lifecycle. No new runtime panic is introduced here. |

## Per-Invariant Notes

### `auto_queue_slot_single_active_entry`

Each auto_queue_run assigns cards to agents through a `(thread_group, slot_index)`
pool. The tick-based runtime allocates a slot via
`allocate_slot_for_group_agent_pg`; allocator guards reject slots with an active
dispatch, a post-claim probe closes allocation races, and terminal transitions
release slot ownership in the same transaction. If two entries under the same
`(agent_id, slot_index)` appear as `dispatched` concurrently, those guards or
the transactional release have failed. That produces confused operator UI (the
status view shows two "active" entries in one slot) and desyncs the agent-side
tmux session binding. The check runs inside `assemble_status_response` — the same
path the HTTP status endpoint renders — so any operator query triggers the
observation without extra DB round trips.

### `dispatch_outbox_retry_count_in_bounds`

`dispatch_outbox.retry_count` was widened from `i32` to `bigint` to prevent a
prior overflow bug where retries accumulated beyond `i32::MAX` on a pinned
stuck row. The batch loop in `process_outbox_batch_with_pg` increments the
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

- Emit invariant violations through `record_invariant_check` in `src/services/observability/emit.rs`.
- Store the invariant key in `observability_events.status` with payload fields
  `invariant`, `code_location`, `message`, and `details`.
- Query counts through `query_invariants_pg` at
  `src/services/analytics/queue_metrics.rs`.
- Expose the API via `GET /api/analytics/invariants` in
  `src/server/routes/analytics.rs` (`invariants`).

Release builds must not gain new panic paths from invariant checks. Use
`debug_assert!` only beside checks that are expected to be impossible in normal
execution; use `record_invariant_check` alone for lifecycle races or stale
runtime files that can temporarily exist during restart/recovery.
