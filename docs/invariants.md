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

## Lifecycle Invariants

| Invariant | Rule | Authoritative code | Runtime guard |
| --- | --- | --- | --- |
| `recovery_phase_valid` | Recovery phase values are restricted to `pending`, `watcher_reattach`, `inflight_restore`, and `done`; transition helpers must canonicalize persisted values through that enum. | `src/services/discord/recovery_engine.rs:30`, `src/services/discord/recovery_engine.rs:186`, `src/services/discord/recovery_engine.rs:214`, `src/services/discord/recovery_engine.rs:225` | Existing unit tests cover phase parsing and transition helpers. Recovery fires remain observable through `emit_recovery_fired` at `src/services/discord/recovery_engine.rs:2359`. |
| `recovery_mailbox_reregister_idempotent` | Restart recovery may re-register an active mailbox turn from inflight state, but repeated attempts must not create parallel active turns. | `src/services/discord/recovery_engine.rs:409`, `src/services/discord/recovery_engine.rs:419` | Covered by the mailbox single-token invariant and `reregister_active_turn_from_inflight` tests. |
| `dispatch_completion_single_authority` | All dispatch completion paths route through `finalize_dispatch` / `complete_dispatch_inner_with_backends` so evidence validation, DB status transition, hooks, and follow-ups share one lifecycle. | `src/dispatch/dispatch_status.rs:1077`, `src/dispatch/dispatch_status.rs:1342`, `src/dispatch/dispatch_status.rs:1345` | Existing dispatch result observability is emitted by the shared status transition path; this change does not rewrite the dispatch state machine. |
| `dispatch_outbox_single_delivery_worker` | Discord side effects for dispatch outbox rows originate from the outbox worker; other paths enqueue durable outbox rows and return. | `src/server/routes/dispatches/outbox.rs:334`, `src/server/routes/dispatches/outbox.rs:1662`, `src/server/routes/dispatches/outbox.rs:1697` | Existing outbox retry/backoff tests cover the lifecycle. No new runtime panic is introduced here. |

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
