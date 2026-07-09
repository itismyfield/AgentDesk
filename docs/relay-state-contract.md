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

### Reference format

Code anchors below are **symbol-path references**, not `file:line` (which
decomposition silently breaks — #4268). Each machine-checkable anchor is an
inline `sym:` span, e.g. (this fenced example is illustrative and is NOT itself
checked, so it cannot pad the anchor set):

```
sym:<module>::<path>::<Symbol>
```

Paths are written from `src/services/discord/`. The gate is enforced in two
halves, each by the tool that can actually prove its half:

- **Existence is proven by the compiler.** Every `sym:` anchor here has a
  matching real reference in a `#[cfg(test)] mod relay_state_contract_refs`
  block (in `inflight/store.rs`, `turn_bridge/terminal_delivery.rs`,
  `tmux_watcher/liveness.rs`, and `router/message_handler/watchdog.rs` — split
  by module visibility). A reference is a `use <path> as _;` (functions/items),
  a `let _ = <Type>::<assoc_fn>;` (associated functions), or a
  `let _ = |x: &<Type>| { let _ = &x.<field>; };` (fields — `use` cannot name a
  field). Each fails to **compile** if its symbol is renamed, moved, or removed,
  and `cargo check --workspace --all-targets` (a required CI gate) compiles those
  blocks. So a rename trips CI regardless of raw strings, macros, or cfg — the
  compiler is the source of truth for existence. The block's cfg gate and every
  attribute inside it are checked against byte-exact whitelists (no cfg parser):
  the gate must be `#[cfg(test)]` or `#[cfg(all(test, unix))]`, and the only
  attribute allowed inside the block is `#[test]`. `unix` is allowed because the
  only required PR Rust compile is `check_fast` (matrix `os: [ubuntu-latest]`,
  where `cfg(unix)` is true), so that required job compiles the block; the
  windows lane is advisory/non-required and skipped for relay-only changes, so a
  windows/non-ubuntu gate would run in no required job. The `pause_epoch`
  producer is `#[cfg(unix)]`, so its anchor block is `#[cfg(all(test, unix))]`.
  Anything else fails loudly — a feature/non-test or non-ubuntu block gate, a
  malformed cfg, or an item-level `#[cfg(feature = "never")]` on a reference that
  would drop it from the compile while the block survives.
- **Doc↔code agreement is proven by `scripts/check_contract_symbol_refs.py`**,
  which does only a cheap exact set comparison: the distinct `sym:` anchors here
  must equal the distinct anchors the checker **parses out of the reference
  expressions themselves** (resolving `super::` / `crate::services::discord::` to
  the paths above). It parses no Rust definitions and reads no comments, so there
  is nothing for a text bypass to exploit. It runs in
  `scripts/ci-script-checks.sh` and as an unconditional `ci-pr.yml` step (a
  relax-safe branch must not skip a contract gate).

When you move contract code, update the `sym:` anchor here **and** its
compiler-checked reference together.

**How the two halves stay on together in CI.** The set-comparison half is an
unconditional `ci-pr.yml` step. The compile-existence half is the `check_fast`
job, which by itself inherits the `ci_relax_safe` skip; so a `relay_contract`
path filter (the four anchor files, this doc, and the gate script) force-runs
`check_fast` whenever the doc↔code binding surface changes, even on a relax
branch. Editing an anchor or this doc therefore always runs both halves.

**There is no more "mislabeled comment" gap.** Earlier revisions carried a
`// sym:` label next to each reference and the checker counted the label, so a
comment could name a symbol the code did not actually reference (and a label
could outlive a deleted reference). The anchor name is now derived from the
reference the compiler checks, so no comment is trusted and none exists: comment
out or `use super::*;`-replace a reference and its anchor disappears, tripping
the set comparison.

---

## I1. `response_sent_offset` is bounded and monotonic

- Definition: `InflightTurnState::response_sent_offset`
  (`sym:inflight::model::InflightTurnState::response_sent_offset`).
- Producer: both `turn_bridge` and `watcher` mutate this through the durable
  save writer `save_inflight_state`
  (`sym:inflight::save_store::save_inflight_state`) at their terminal save
  sites.
- Validation: `validate_inflight_state_for_save`
  (`sym:inflight::store::validate_inflight_state_for_save`).
- Invariant keys:
  - `response_sent_offset_in_bounds` — must stay within `full_response.len()`
    and land on a UTF-8 char boundary.
  - `response_sent_offset_monotonic` — must not move backwards relative to
    the previously persisted state for the same channel.
- Violation surface: a backwards move re-emits prior assistant text and
  causes Discord duplicates; an out-of-bounds value panics in debug builds
  and silently drops a relay slice in release builds.

## I2. `current_msg_id` rollover has a single source of truth

- Definition: `InflightTurnState::current_msg_id`
  (`sym:inflight::model::InflightTurnState::current_msg_id`).
- Producer:
  - `turn_bridge` rolls the placeholder over in the `spawn_turn_bridge`
    streaming task (`sym:turn_bridge::spawn_turn_bridge`); `spawn_turn_bridge`
    itself resolves/pins the initial `current_msg_id`, and the streaming child
    fns it drives write each subsequent rollover.
  - `watcher` pins/rolls its `current_msg_id` in
    `reacquire_watcher_inflight_for_active_stream`
    (`sym:tmux_watcher::liveness::reacquire_watcher_inflight_for_active_stream`).
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
  (`sym:inflight::model::InflightTurnState::last_watcher_relayed_offset`).
- Consumer: watcher startup in `tmux_output_watcher_with_restore`
  (`sym:tmux_watcher::tmux_output_watcher_with_restore`) initialises its
  in-memory `last_relayed_offset` from this value so a replacement watcher does
  not re-emit content the previous watcher already sent.
- Invariant: a watcher that restarts at offset `O` must not relay any
  bytes whose start offset is `< O`. Equivalently, replaying the same
  output buffer twice must result in zero new Discord messages.
- Violation surface: replacement watcher (post restart, post replace,
  post crash) re-emits the previous turn's tail to Discord.
- Invariant key: `watcher_relay_idempotent`.

## I4. `confirmed-end` watermark has a single owner per turn end

- Definition: `tmux_relay_confirmed_end` watermark, written by both:
  - `turn_bridge` via `advance_tmux_relay_confirmed_end`
    (`sym:turn_bridge::terminal_delivery::advance_tmux_relay_confirmed_end`),
    called from `deliver_short_replace_via_controller`
    (`sym:turn_bridge::terminal_controller_cutover::deliver_short_replace_via_controller`),
    `deliver_long_chunks_via_controller`
    (`sym:turn_bridge::terminal_controller_cutover::deliver_long_chunks_via_controller`),
    and the lease-commit path `BridgeDeliveryLease::commit_and_advance`
    (`sym:turn_bridge::terminal_delivery::BridgeDeliveryLease::commit_and_advance`);
  - `watcher` self-confirm via `advance_watcher_confirmed_end`
    (`sym:tmux::advance_watcher_confirmed_end`).
- Invariant: at the end of a single turn there is exactly one writer of
  the confirmed-end watermark for that turn. The `single-relay-owner`
  migration's first observable contract change is that this writer is
  always the watcher.
- Violation surface: stuck "in progress" UI, dispatch never marked
  complete, duplicate completion side-effects (memento double-capture).
- Invariant key: `confirmed_end_single_writer`.

## I5. duplicate-suppression protocol (`turn_delivered` / `resume_offset` / `pause_epoch`)

- Definition: shared atomic flags held on the `TmuxWatcherHandle`
  (`sym:TmuxWatcherHandle::turn_delivered`, `sym:TmuxWatcherHandle::resume_offset`,
  `sym:TmuxWatcherHandle::pause_epoch`).
- Producers (per field — these three flags do **not** share one writer; the
  earlier "single producer `run_terminal_outcome_delivery`" claim was wrong and
  is corrected here per #4268 r3):
  - `turn_delivered` is set true by two producers: the bridge in-band terminal
    delivery path `run_terminal_outcome_delivery`
    (`sym:turn_bridge::terminal_outcome_delivery::run_terminal_outcome_delivery`)
    and the watcher terminal-commit epilogue `run_terminal_commit_epilogue`
    (`sym:tmux_watcher::terminal_commit_epilogue::run_terminal_commit_epilogue`).
    (It is additionally *cleared* to false on the handoff/reset and auto-heal
    paths, and by the watcher after it consumes the flag; those resets are not
    producers of the delivered signal.)
  - `resume_offset` is written (as the "already delivered in-band up to here"
    marker) by the completion postlude `run_completion_postlude`
    (`sym:turn_bridge::completion_postlude::run_completion_postlude`) and the
    runtime-handoff loop `handle_runtime_handoff_loop_message`
    (`sym:turn_bridge::runtime_handoff_loop::handle_runtime_handoff_loop_message`).
    These are the primary terminal/handoff producers; the busy-turn handoff,
    finalize-epilogue, and auto-heal paths also seed it, and the watcher clears
    it on consume.
  - `pause_epoch` has exactly one production writer, and it is **not** in
    `turn_bridge`: the watchdog increments it when it opens a pause window, in
    `attach_paused_turn_watcher_inner`
    (`sym:router::message_handler::watchdog::attach_paused_turn_watcher_inner`).
- Consumer: `watcher` checks these in `poll_watcher_output_or_continue`
  (`sym:tmux_watcher::loop_poll_prologue::poll_watcher_output_or_continue`),
  which owns both the resume guard and the late guard.
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

## I6. `last_offset` watermark is owner-gated and monotonic per turn (#3017)

- Definition: `InflightTurnState::last_offset`
  (`sym:inflight::model::InflightTurnState::last_offset`).
- Producers (the three writers #3017 unifies):
  - `turn_bridge` sets in-memory `inflight_state.last_offset = …` then calls
    the durable writer `save_inflight_state`
    (`sym:inflight::save_store::save_inflight_state`).
  - `watcher` via the same `save_inflight_state` durable writer.
  - standby JSONL relay (from `standby_relay`) via
    `refresh_inflight_last_offset_if_matches_identity`
    (`sym:inflight::clear_store::refresh_inflight_last_offset_if_matches_identity`
    → `sym:inflight::clear_store::refresh_inflight_last_offset_if_matches_identity_in_root`).
- Validation:
  - ENFORCING in the standby/refresh path
    (`refresh_inflight_last_offset_if_matches_identity_in_root`): the
    write is skipped (returns `false`, on-disk state unchanged) when
    (a) the caller is not the live relay owner
    (`effective_relay_owner_kind()`,
    `sym:inflight::model::InflightTurnState::effective_relay_owner_kind`), or
    (b) `last_offset` would move backwards for the SAME turn identity.
  - OBSERVE-ONLY on the bridge/watcher save path
    (`validate_inflight_state_for_save`,
    `sym:inflight::store::validate_inflight_state_for_save`): a backward
    `last_offset` for
    the same turn identity records the violation + `debug_assert` but
    does not drop the write, so a legit fresh-turn reset can still
    persist.
- Invariant: for a given (provider, channel, turn identity) the persisted
  `last_offset` is MONOTONIC non-decreasing AND is advanced only by the
  current relay owner; a non-owner (standby/idle) follows the
  authoritative offset read-only. A NEW turn (different `user_msg_id` /
  `turn_start_offset`) legitimately resetting the watermark is EXEMPT —
  the identity guards distinguish this from a backward clobber.
- Violation surface: a non-owner or backward write clobbers the
  watermark → stale transcript tail re-emitted (#2843) or relay bound to
  the wrong session / frozen binding offset (#2789).
- Invariant keys:
  - `last_offset_monotonic` — must not move backwards for the same turn
    identity.
  - `last_offset_owner_gated` — only the current relay owner may advance
    it; standby yields to a live Watcher.

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
