# Relay State Contract

This document captures the invariants that the Discord relay path
(`watcher` ↔ `turn_bridge` ↔ `inflight` state) currently relies on. It exists
because the `single-relay-owner` work tracked under #1222 needs an explicit,
testable contract before relay ownership can be migrated from `turn_bridge`
into `watcher`.

Each invariant lists:
- the **definition site** (where the value is owned),
- the **consumer sites** (who reads it),
- the **producer sites** (who mutates it),
- the **violation surface** (what a regression looks like in production),
- the **invariant key** used by `crate::services::observability::record_invariant_check`.

If you change relay ownership, every invariant below must continue to hold.
A regression here is a user-visible relay miss / duplicate, so keep the
checks loud (debug_assert + observability record) instead of silent.

---

## I1. `response_sent_offset` is bounded and monotonic

- Definition: `InflightTurnState::response_sent_offset`
  (`src/services/discord/inflight.rs:50`).
- Producer: both `turn_bridge`
  (`src/services/discord/turn_bridge/mod.rs:1505-1511`, terminal save sites)
  and `watcher` mutate this through `save_inflight_state`.
- Validation: `validate_inflight_state_for_save`
  (`src/services/discord/inflight.rs:318-368`).
- Invariant keys:
  - `response_sent_offset_in_bounds` — must stay within `full_response.len()`
    and land on a UTF-8 char boundary.
  - `response_sent_offset_monotonic` — must not move backwards relative to
    the previously persisted state for the same channel.
- Violation surface: a backwards move re-emits prior assistant text and
  causes Discord duplicates; an out-of-bounds value panics in debug builds
  and silently drops a relay slice in release builds.

## I2. `current_msg_id` rollover has a single source of truth

- Definition: `InflightTurnState::current_msg_id`.
- Producer:
  - `turn_bridge` rolls over at `src/services/discord/turn_bridge/mod.rs:1505-1511`.
  - `watcher` rolls over at `src/services/discord/tmux.rs:4513-4526`.
- Invariant: when both owners observe the same `inflight` snapshot, they
  must agree on which `MessageId` is the active streaming placeholder.
  After a rollover one of the owners writes the new id back into
  `inflight`; the other must consume it before issuing a follow-up edit.
- Violation surface: two parallel streaming placeholders for one turn,
  visible as ghost duplicates in Discord.
- Invariant key: `current_msg_id_single_source` (NEW — recorded by the
  test added in this slice; production hooks will adopt the key as the
  ownership migration progresses).

## I3. `last_watcher_relayed_offset` is idempotent across watcher replacement

- Definition: `InflightTurnState::last_watcher_relayed_offset`
  (`src/services/discord/inflight.rs:81`).
- Consumer: watcher startup at
  `src/services/discord/tmux.rs:3759-3766` initialises its in-memory
  `last_relayed_offset` from this value so a replacement watcher does not
  re-emit content the previous watcher already sent.
- Invariant: a watcher that restarts at offset `O` must not relay any
  bytes whose start offset is `< O`. Equivalently, replaying the same
  output buffer twice must result in zero new Discord messages.
- Violation surface: replacement watcher (post restart, post replace,
  post crash) re-emits the previous turn's tail to Discord.
- Invariant key: `watcher_relay_idempotent`.

## I4. `confirmed-end` watermark has a single owner per turn end

- Definition: `tmux_relay_confirmed_end` watermark, written by both:
  - `turn_bridge::advance_tmux_relay_confirmed_end`
    (`src/services/discord/turn_bridge/mod.rs:302-335`, called at
    `1871-1876 / 1906-1911 / 2233-2238`),
  - `watcher` self-confirm at `src/services/discord/tmux.rs:5618-5629`.
- Invariant: at the end of a single turn there is exactly one writer of
  the confirmed-end watermark for that turn. The `single-relay-owner`
  migration's first observable contract change is that this writer is
  always the watcher.
- Violation surface: stuck "in progress" UI, dispatch never marked
  complete, duplicate completion side-effects (memento double-capture).
- Invariant key: `confirmed_end_single_writer`.

## I5. duplicate-suppression protocol (`turn_delivered` / `resume_offset` / `pause_epoch`)

- Definition: shared atomic flags created in
  `src/services/discord/mod.rs:457-472`.
- Producer (current): `turn_bridge` writes
  `turn_delivered` / `resume_offset` / `pause_epoch` at
  `src/services/discord/turn_bridge/mod.rs:2261-2295`.
- Consumer: `watcher` checks at
  `src/services/discord/tmux.rs:3792-3808` (resume guard) and the late
  guard at `4987-5023`.
- Invariant: this is **not** lifecycle metadata — it is an active
  duplicate-suppression handshake. After a `pause` window closes the
  watcher must not relay any byte in `[resume_offset_seen,
  resume_offset_now)` because the bridge already delivered it
  in-band.
- Violation surface: every previous duplicate-relay regression
  (#1044 A→C, #1137, #1199 follow-ups, #1216) was a hole in this
  protocol.
- Invariant key: `pause_resume_handshake`.

When the migration removes `turn_bridge` as a relay producer, this
protocol must be replaced (not merely deleted): the watcher itself
gains the responsibility of refusing to relay bytes that were already
delivered through any other authorised path. Any sub-issue under
#1222 that touches relay must reaffirm this invariant in its test
plan.

---

## How to add a new invariant

1. Document it here with the same structure (definition, producer,
   consumer, surface, key).
2. Wire `record_invariant_check(condition, InvariantViolation { ... })`
   at every producer site so violations are observable in
   `observability_event` rows (counters + recent records).
3. Add a regression test that intentionally violates the invariant to
   prove the check fires.
4. Reference this document from the relevant sub-issue under #1222.
