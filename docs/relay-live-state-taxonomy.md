# Relay Live-State Taxonomy and Ownership Gates

This document classifies the current Discord relay live-state surfaces by
persistence and ownership. It is a current-state map, not approval of a future
multinode handoff design. In particular, it does not give a host-local file
cluster authority merely because that file survives a process restart.

The relay invariants remain normative in
[`relay-state-contract.md`](relay-state-contract.md). The multinode rules remain
normative in
[`agent-maintenance/multinode-transition.md`](agent-maintenance/multinode-transition.md).

## Four categories

| Category | Authority and recovery contract | Current relay state in this category | Code anchors |
| --- | --- | --- | --- |
| **Authoritative durable (PostgreSQL)** | Cluster-visible state whose database claim, lease, uniqueness, or CAS decides whether work may proceed. It survives host loss and may be consumed on another node according to its own fencing contract. | PostgreSQL `message_outbox` rows and task-notification card/response state are authoritative for their respective delivery obligations. They do **not** make the node-local turn, transcript cursor, or inflight file cluster-authoritative. | `src/services/message_outbox.rs:enqueue_outbox_pg_returning_id`; `src/services/discord/task_notification_delivery/store/card_claim.rs:claim_card_pg`; `src/services/discord/task_notification_delivery/store/response_fence.rs:claim_response_delivery` |
| **Host-local durable (lease-bound)** | Files survive a process restart on one host, but are valid only with the owning host's provider session, transcript generation, gateway/runtime ownership, and local file-lock or delivery-lease fences. They must never be read as cross-node authority. | `discord_pending_queue/<provider>/<token>/<channel>.json` is recovery-critical queue state and is hydrated into the local mailbox after restart. `discord_inflight/<provider>/<channel>.json` is the local turn/recovery projection. `discord_delivery_records/<provider>/<channel>.json` stores the generation- and EOF-bounded delivery frontier. None currently carries a cluster `node_id + lease_epoch` ownership stamp. | `src/services/turn_orchestrator/pending_queue_persistence.rs:pending_queue_file_path`; `src/services/turn_orchestrator/pending_queue_persistence.rs:load_pending_queues`; `src/services/discord/inflight/store.rs:inflight_state_path`; `src/services/discord/inflight/model.rs:InflightTurnState`; `src/services/discord/outbound/delivery_record.rs:DeliveryRecord`; `src/services/discord/outbound/delivery_record.rs:current_generation_durable_frontier_at` |
| **In-memory projection** | Reconstructible, process-local coordination state. It may accelerate or serialize a live owner, but process loss discards it and another node must not infer ownership from its absence. Durable evidence must be consulted where an invariant requires it. | `GLOBAL_CHANNEL_MAILBOXES` and `ChannelMailboxRegistry` expose actor handles; `TmuxRelayCoord` carries the in-memory confirmed-end watermark and shared terminal `DeliveryLeaseCell`; `PanelCacheInvalidations` carries epoch-guarded forced-rerender requests. | `src/services/turn_orchestrator.rs:GLOBAL_CHANNEL_MAILBOXES`; `src/services/turn_orchestrator.rs:ChannelMailboxRegistry`; `src/services/discord/mod.rs:TmuxRelayCoord`; `src/services/discord/mod.rs:DeliveryLeaseCell`; `src/services/discord/placeholder_live_events/panel_cache_invalidation.rs:PanelCacheInvalidations` |
| **Transient** | Data exists only while a frame, parse, transport, or awaited operation is in flight. It is neither a recovery record nor an ownership claim. Loss is recovered only by replay from an already-authoritative source or is an accepted limitation. | `StreamFrame` is the queued provider-output envelope. `SessionRelayParser` owns partial `buffer`/`full_response` only until it hands a completed `SessionRelayDelivery` to transport. Local delivery guards and HTTP futures are likewise ephemeral. | `src/services/cluster/stream_relay.rs:StreamFrame`; `src/services/discord/session_relay_sink/turn_parser.rs:SessionRelayParser`; `src/services/discord/session_relay_sink.rs:SessionRelayDelivery`; `src/services/discord/session_relay_sink.rs:SinkDeliveryLeaseGuard` |

### Required reading of the table

- `pending_queue` and `inflight` are **host-local durable**, not PostgreSQL
  authority. Restart hydration does not imply cross-node adoption.
- The mailbox registry is an **in-memory projection** even when its queue contents
  have a host-local durable mirror.
- A relay range or parser buffer is **transient** until a confirmed delivery
  advances the appropriate durable or in-memory committed frontier.
- PostgreSQL authority is scoped to the row's domain. For example, a claimed
  task-notification card does not confer ownership of the provider tmux session.

## Compatibility with current relay invariants

| Contract | Taxonomy consequence | Status |
| --- | --- | --- |
| I9 — every session-bound terminal POST holds the shared delivery lease (#4277, PR #4847) | `DeliveryLeaseCell` is an in-memory single-winner coordination projection. Winning it authorizes only the matching local `(channel, turn, byte range)` terminal attempt; it does not create cross-node ownership or promote `inflight` to cluster authority. | **Landed.** |
| I10 — idle cursor confirmed-commit-only (#4536, PR #4852) | Idle/catch-up ranges stay transient and retryable while deferred or merely enqueued. The local cursor may consume only an intentional classified drop or a range covered by a generation-scoped confirmed commit; durable frontier persistence precedes the in-memory watermark. | **Proposed.** PR #4852 is open, so this row is not a landed invariant and must be updated when that PR resolves. |
| Durable-frontier reanchor CAS (#4841, PR #4843) | The host-local delivery frontier may be lowered after same-path compaction only while holding its record lock and while wrapper generation plus the expected observed frontier still match. A concurrent commit wins and makes the reanchor a no-op. | **Landed.** Anchor: `src/services/discord/outbound/delivery_record.rs:reanchor_current_generation_frontier`. |
| Panel cache invalidation epoch (#4340, PR #4830) | Forced panel rerender state is an in-memory projection keyed by `(channel, message)` and epoch. A successful edit clears only the epoch it observed, so a newer invalidation cannot be erased by an older completion. Durable panel identity/generation remains in the host-local inflight row. | **Landed.** Anchors: `src/services/discord/placeholder_live_events/panel_cache_invalidation.rs:PanelCacheInvalidations::clear_if_epoch`; `src/services/discord/inflight/model.rs:InflightTurnState::status_panel_generation`. |

These contracts are deliberately composable: the terminal lease prevents two
local posters, the confirmed-commit rule prevents enqueue-time cursor loss, the
frontier CAS prevents compaction repair from overwriting a concurrent commit,
and the panel epoch prevents a stale UI completion from clearing newer rerender
work. None is a substitute for a node-ownership lease.

## #4414 owner-decision gates

Issue #4414 proposes demoting owned turn state to a transcript-derived view. Its
stated lifecycle is design, then user/owner approval, then staged implementation.
The following choices therefore remain explicit **decision gates**; this document
does not decide them.

| Gate requiring user/owner decision | Decision required before #4414 implementation crosses the gate | Why #4521 must not pre-decide it |
| --- | --- | --- |
| **G1. Persistent authority set and delivery-ledger schema** | Approve whether the target's only persistent authorities are the session registry and delivery ledger, and define the ledger identity, offset/generation coordinates, Discord message ownership, leases, and pipe-mode representation. | Calling today's `inflight` or delivery sidecar authoritative would prejudge the target schema; removing it without a ledger design would remove current recovery fences. |
| **G2. `inflight` residual role** | Choose whether `discord_inflight` is deleted, retained temporarily as a non-authoritative compatibility/recovery cache, or retained under the fallback architecture (“owned turn with reduced state surface”). Set the migration and rollback boundary. | `InflightTurnState` currently carries live Discord handles, transcript coordinates, finalizer identity, and recovery markers. Its post-#4414 residue determines whether any ownership stamp is useful. |
| **G3. Transcript-unanchored event attribution** | Approve how harness task notifications, synthetic turns, monitor turns, and other events without a stable transcript anchor join the derived turn view and delivery ledger. | #4414 lists this as an open question; choosing an identity here changes deduplication and completion semantics. |
| **G4. Queue ownership** | Approve whether the visible per-channel mailbox/pending queue remains AgentDesk authority or input arbitration is delegated to the TUI, including the accepted loss of queue visibility and restart behavior. | `pending_queue` is recovery-critical host-local state today. Moving or deleting it is a product/UX trade-off, not a documentation inference. |
| **G5. Pipe compatibility boundary** | Approve the common downstream contract under which pipe is a one-turn stream, including process-exit termination and which delivery-ledger fields are mandatory when no tmux transcript exists. | The hard pipe-mode constraint is part of #4414; a transcript-only implementation would violate it. |
| **G6. Pilot and fallback thresholds** | Before the pilot, approve measurable diff-size, regression-rate, relay miss/duplicate, and rollback thresholds for choosing derived-view migration versus the fallback “owned turn with reduced state surface.” | #4414 explicitly requires quantified criteria; selecting thresholds is an owner acceptance decision. |
| **G7. Cross-node loss and handoff objective** | After G1–G6 establish the residual state, decide whether active-host death may continue to lose in-flight relay work or whether cross-node adoption becomes a required SLO. | A cross-node `instance_id + epoch` stamp and PostgreSQL ownership reconciliation are useful only if residual authoritative work must transfer. Implementing them now risks a throwaway lease protocol. |

### Dependency rule for node ownership

- Until #4414 reaches the relevant owner-approved design gate, node ownership
  stamping and reconciliation are **P3/deferred** under the multinode track
  (#876–#884). No current sidecar may be promoted to cluster authority by
  documentation alone.
- After #4414 defines the residual durable state, re-audit only that residue. If
  cross-node adoption is approved, design `instance_id + epoch` fencing and
  PostgreSQL ownership comparison around the residual authority, not around the
  pre-migration `InflightTurnState` blob.
- Until both decisions land, fail closed: the owning node may recover its own
  files; another node must not adopt them.
