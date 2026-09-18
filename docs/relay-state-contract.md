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

The persistence and node-ownership status of these values is classified in
[`relay-live-state-taxonomy.md`](relay-live-state-taxonomy.md). In particular,
a durable sidecar is host-local unless a PostgreSQL-backed ownership contract
explicitly says otherwise.

## Canonical owner, dependency, and acceptance contract (Task #32)

This is the release acceptance contract, not a claim that the target authority
already exists. The **required target** assigns every accepted Discord source
message ID exactly one immutable terminal intake disposition: `Steered` or
`DurablyQueued`. `DurablyQueued` survives completion, worker replacement,
shutdown, and restart until eventual drain; drain does not add or rewrite the
intake disposition. Current production cannot claim this guarantee because it
has no canonical disposition authority.

### Current production versus required target

| Surface | Current production authority | Required target / gap | Identity and linearization | Forbidden fallback / acceptance |
|---|---|---|---|---|
| Discord intake | Channel mailbox plus host-local `discord_pending_queue/<provider>/<token_hash>/<channel_id>.json`; no PostgreSQL disposition authority | Canonical durable admission store writes exactly one `Steered` or `DurablyQueued` disposition and preserves queued work through replacement/shutdown/restart to eventual drain | Intake starts with provider, channel/source message, turn/dispatch, and exact tmux incarnation when known. Transcript range/frontier is a later delivery-bound identity extension, not an intake prerequisite. One transaction/CAS is the target linearization point. | Reactions, panels, in-memory mailbox state, “busy” inference, or attempted sends are not acceptance. Concurrent replay yields one disposition; every durable queue row drains once. |
| Terminal body | Single relay owner and shared delivery lease; host-local delivery record/frontier participates in dedup | Replacement-safe durable ownership and dedup authority over the full delivery identity | Full delivery identity is `(provider, channel_id, turn_id, dispatch_id, exact tmux session, generation, spawn nonce, transcript range/frontier)`. Target linearization is confirmed Discord transport plus identity-gated durable frontier commit. | No markerless POST, restored-body/fingerprint guess, legacy in-memory dedup, or fresh POST merely because edit failed. Owner/restart races yield one body and one monotonic frontier. |
| Terminal ACK | Process-local bounded `RelayMetrics.terminal_outcomes` `VecDeque`; exact-sequence lookup exists but is neither durable nor replacement-safe | Durable exact-sequence typed outcome keyed by full delivery identity; currently unimplemented | Current typed outcomes are `Delivered`, `FreshDelivered`, `NotDelivered`, and `Unknown`-class provenance. Target sequence: pin identity/range and lease → enqueue sequence `N` → transport and commit attempt → durably record outcome `N` → watcher reads exactly `N`. | Never use `>= N`, another turn's ACK, timeout-as-success, blind skip, or blind resend. Every non-confirmed outcome reconciles the committed frontier. |
| Status panel | Host-local `discord_status_panel_singletons` file stores only current message ID plus generation | `status_panel_transition_v2` journal/CAS model is dormant substrate with **zero production callers**; production transition authority remains unimplemented | Target transition is `Prepared → BindAuthorized → Bound → RetireAuthorized → Retired`, with typed failed/quarantined terminals and provider/channel/turn/candidate/generation/spawn-nonce identity. | Discord success and durable commit are separate. A success→commit crash may not delete the successful panel or create an unjournaled replacement; faults converge to at most one journal-owned panel. |
| Terminal abort | Watcher calls platform kill by session name; termination audit and exit-reason writes are best-effort/fire-and-forget, with no exact-target revalidation or observed-exit authority | Required sequence, not current behavior: durable typed audit → exact session/pane/PID/generation/spawn-nonce revalidation → kill → bounded confirmation that that exact target exited | Closed typed provenance and full delivery/target identity are mandatory. Timeout, probe failure, or identity change is typed unconfirmed. | No plaintext-substring classification, session-name-only target, fabricated finalizer pin, or unconfirmed kill reported as success. The canonical main orchestration session is never an automatic target. |

Missing or legacy identity fields are explicit typed states, never wildcards. The
Discord-success↔durable-commit crash window remains `Unknown` until a stable
nonce/message probe and frontier/journal reconciliation proves success or grants
one retry. No production caller means no completion credit.

### Verified dependency DAG and evidence boundary

```text
#4890 → #4911 → #4891 → #4860 → #4889
#4909 ─────────→ #4891
#4895 → #4896
#4874 (independent)
```

`#4934` (dormant panel reducer), `#4933` (dormant typed completion codec), and
`#4918` (deploy-gate containment) are substrate/containment only. `#4898` is
closed-but-incomplete because trusted typed deployment evidence is absent. The
discarded research commits `7a733441`, `53705ac3`, and `5cf5e7b` are forbidden
as completion evidence. Closed issues, ancestry, unit tests, and abstractions
without production callers cannot close a DAG edge.

### Measurable completion gates

1. **Disposition:** concurrent replay gives every accepted source ID exactly one
   disposition; replacement, shutdown, and restart lose none; queued obligations
   reach zero and drain once.
2. **Actual Discord faults:** in a dedicated Discord channel run at least 20
   accepted messages per injected 429/5xx, ACK delay, edit 404, post-accept
   connection loss, and process death in the success→commit window. Preserve all
   IDs, drain within five minutes after recovery, and observe zero duplicate
   terminal bodies or unjournaled panels.
3. **Ownership/ACK:** fault bridge, watcher, replacement, shutdown, and restart
   on both sides of each linearization point; prove one emitter per range,
   monotonic frontier, exact typed ACK, and reconciliation of every `Unknown`.
4. **Panel/abort:** fault every panel transition and wrong-target abort race;
   retain at most one current panel, require exact retire authority, produce zero
   plaintext/session-name-only kills, and count success only after exact-target
   exit confirmation.
5. **Evidence:** each predecessor needs a production caller, focused regression,
   and linked release evidence; exclude the substrate PRs, `#4898`, and all three
   discarded commits.

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
  `tmux_watcher/liveness.rs`, `router/message_handler/watchdog.rs`,
  `mailbox_finish.rs`, and `session_relay_sink.rs` — split by module visibility). A reference is a `use <path> as _;` (functions/items),
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
explicit `ci-pr.yml` step. The compile-existence half is the required
`check_fast` job; a `relay_contract` path filter (the four anchor files, this
doc, and the gate script) force-runs it for doc-only binding changes as well as
Rust changes. No branch-name escape hatch may skip either half.

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
    (It is additionally *cleared* to false on the handoff/reset paths and by
    the watcher after it consumes the flag; those resets are not producers of
    the delivered signal. The auto-heal redrive used to clear it too — #5943
    removed that, because the redrive re-reads a turn rather than starting one;
    see I16.)
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

## I7. session-bound parser hands off completed turn state before delivery

- Definition: the session-local parser owns `buffer`, `full_response`, tool state,
  and task-notification context only until it recognizes the current turn's
  terminal record (`sym:session_relay_sink::turn_parser::SessionRelayParser::ingest_frame`).
- Producer: `SessionRelayParser::ingest_frame` moves the completed response and
  context into `SessionRelayDelivery`, then resets all turn-local fields before
  returning the delivery to the asynchronous Discord transport.
- Consumer: the next `StreamFrame` for the same tmux session may be ingested as
  soon as the current delivery has been handed off; it must start from empty
  turn-local state while preserving any unprocessed next-turn bytes in `buffer`.
- Invariant: completed prose belongs exclusively to the emitted delivery. It may
  not remain parser-owned until POST/edit success, failure, or ACK resolution.
- Violation surface: a delayed or desynchronized transport lets the next turn
  append to the completed `full_response`, re-publishing the previous turn as a
  byte-identical prefix or merging several turns into one Discord message (#4365).
- Invariant key: `session_parser_turn_handoff` (enforced by parser ownership and
  regression tests; an observability producer can be added if this boundary
  becomes fallible).

## I8. mailbox turn cleanup is guarded by durable episode identity (#4595)

- Definition:
  - every modern `CancelToken` carries one immutable durable nonce
    (`sym:provider::CancelToken::turn_nonce`), and the mailbox actor copies it
    into `ChannelMailboxSnapshot::active_turn_nonce`
    (`sym:turn_orchestrator::ChannelMailboxSnapshot::active_turn_nonce`) when it
    serializes a turn admission;
  - the same value is persisted in `InflightTurnState::turn_nonce`
    (`sym:inflight::model::InflightTurnState::turn_nonce`).
- Producers: Discord intake, headless intake, TUI-direct synthetic admission,
  monitor auto-turn admission, and restart restoration bind the actor-owned
  nonce to the durable row. Restart restoration preserves an explicit legacy
  `None` instead of minting an unrelated modern episode.
- Destructive consumer: stale durable-repair paths call
  `mailbox_finish_turn_if_matches_episode_started_before`
  (`sym:mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before`),
  whose single mailbox-actor handler compares `(user_msg_id, turn_nonce)` plus
  the monotonic start cutoff before taking the active token.
- Legacy policy: `None` is an exact legacy identity, never a wildcard. A legacy
  row can finish only a legacy actor claim; it cannot finish a modern claim and
  a modern row cannot finish a legacy claim.
- Invariant: cleanup for stale episode A must be a destructive no-op when episode
  B owns the same message ID under a different nonce, including when B claimed
  before the sweep cutoff and its durable save is delayed. B's token, owner,
  message ID, nonce, soft queue, and `global_active` remain unchanged.
- Violation surface: a stale placeholder sweep or health repair cancels a live
  same-message-ID successor, drops queued work, decrements `global_active`, and
  can kill the successor's tmux session.
- Invariant key: `mailbox_episode_identity_exact` (enforced by the actor predicate
  and mutation-proven regression tests rather than an observe-only hook).

## I9. every session-bound terminal POST holds the shared delivery lease (#4277)

- Definition: terminal deliveries parsed by
  `SessionRelayParser::ingest_frame`
  (`sym:session_relay_sink::turn_parser::SessionRelayParser::ingest_frame`) carry
  either their strict turn fence, an idle/catch-up ordered JSONL range, or the
  legacy no-range shape.
- Producer: `SessionBoundDiscordRelaySink::deliver_response`
  (`sym:session_relay_sink::SessionBoundDiscordRelaySink::deliver_response`)
  derives one lease coordinate before any Discord transport. Strict fenced
  frames use `[turn_start_offset, terminal_consumed_end)`, ordered idle/catch-up
  frames use their carried range, and unresolvable legacy frames still contend
  on the degenerate key and zero-width coordinate.
- Consumer: the sink and watcher share the channel's `DeliveryLeaseCell`; a lost
  acquire is a deterministic not-delivered result and must never fall through to
  a markerless POST.
- Invariant: there is no session-bound terminal POST/edit path without first
  winning the shared delivery lease. The controller short-replace path owns its
  own acquire; all other terminal paths use `SinkDeliveryLeaseGuard`.
- Violation surface: an inflight-less idle-tail frame POSTs without a lease while
  the watcher independently wins the fallback-keyed lease for the same bytes,
  producing a duplicate Discord response.
- Invariant key: `session_terminal_post_lease_required` (enforced structurally by
  the mandatory acquire and production-entry regression tests).
- Consumer rule: a confirmed fresh-message POST returned by the controller is
  preserved as a typed exact-sequence terminal resolution through
  `sym:cluster::stream_relay::RelaySinkOutcome::terminal_fresh_delivered` and the
  watcher transport-confirmation predicate.
  `committed_to=None` and `persistence_recorded=false` describe missing frontier
  authority or retry metadata; neither erases confirmed transport or authorizes
  the watcher to acquire the released lease and POST the same body again.
- Violation surface: folding confirmed fresh transport into `RelaySinkError` loses
  its provenance; §3.2 then sees `committed < end`, reacquires the shared lease,
  and sends a duplicate full response.

## I10. idle JSONL cursors consume only classified drops or confirmed commits (#4536)

- Definition: the idle backstop range decision
  (`sym:session_relay_sink::idle_jsonl::idle_jsonl_suppressed_range_action`)
  separates intentional classification drops from temporary inflight/grace
  deferral.
- Producer: confirmed ranged transport commits through
  `SessionBoundDiscordRelaySink::advance_idle_range_after_confirmed_post`
  (`sym:session_relay_sink::SessionBoundDiscordRelaySink::advance_idle_range_after_confirmed_post`),
  which first persists the generation-scoped frontier via
  `commit_ordered_jsonl_range`
  (`sym:outbound::delivery_record::commit_ordered_jsonl_range`) and then advances
  the in-memory watermark.
- Consumer: the idle loop advances its local cursor only when the range is an
  intentional drop or current-generation committed coverage reaches its end;
  enqueue acceptance alone leaves the pending range retryable.
- Invariant: active-turn, post-inflight-grace, and new-session-grace suppression
  never consumes an uncommitted byte. A confirmed ranged POST must match the
  queued wrapper generation and remain EOF-bounded before either authority is
  advanced.
- Violation surface: enqueue followed by transport/commit failure permanently
  skips a wake/background answer, or a delayed range commits against a replaced
  transcript and suppresses unrelated output.
- Invariant key: `idle_cursor_confirmed_commit_only` (enforced structurally and by
  cursor/commit/generation regression tests).

## I11. edit failure does not authorize a fresh fallback POST (#4508)

- Definition: the formatting layer's edit-only replace primitive
  (`sym:formatting::replace_long_message_raw_deferred`) returns a typed edit
  failure without issuing a fallback POST.
- Producer: before awaiting the edit, the range owner captures the expected
  watcher output path and nonzero generation. After edit failure it re-reads a
  stable path+generation+EOF+durable-frontier snapshot with
  `range_committed_after_edit_failure`
  (`sym:outbound::delivery_record::range_committed_after_edit_failure`) while the
  same `DeliveryLease` is still held. The delivery-record lock serializes
  conforming frontier writes, and post-read path/generation/file-identity checks
  reject wrapper rotation or transcript replacement during the snapshot.
- Consumer: controller and legacy watcher short-replace paths suppress fallback
  delivery only when the pre-edit identity still matches and fresh,
  generation-matching, EOF-bounded coverage reaches the range end. Missing
  markers, path or generation changes, unstable file metadata, unknown EOF, and
  frontier past EOF remain fail-open for delivery and retain the existing
  one-shot fallback.
- Invariant: an edit failure is not fresh-send authority. Confirmed committed
  coverage yields `AlreadyCommittedAfterEditFailure` with zero fallback POSTs and
  no transport-success commit; watcher reconciliation uses the delivered-anchor-
  aware guarded cleanup and clears placeholder/orphan tracking only after a
  committed stale-placeholder delete. The delivered anchor itself and any case
  without positive delivered-elsewhere proof remain tracked and preserved.
  Otherwise, only a confirmed fallback POST may run the existing commit/advance
  path.
- Violation surface: a restart leaves a stale placeholder target, another owner
  commits the JSONL range while the edit is awaited, Discord returns Unknown
  Message, and the stale owner duplicates the already delivered response via a
  fresh POST.
- Invariant key: `edit_failure_fallback_requires_fresh_frontier` (enforced by
  controller mutation-sensitive post-count tests, delivery-record generation/EOF
  tests, and controller/legacy owner wiring tests).

## I12. Bounded no-progress redrive (#4906)

- Definition: a redrive cannot move below the committed frontier or enqueue the
  same still-pending frontier twice.
- Producer: the relay auto-healer computes the requested frontier as the maximum
  of the watcher snapshot and committed frontier, then rejects already-covered
  or identical pending requests before mutating watcher state.
- Consumer: six no-progress actions enter a one-hour degraded backoff and then
  re-arm the same stalled episode; reaching the cap never abandons recovery
  permanently.
- Violation surface: a frozen frontier is enqueued on every health pass, or a
  capped stalled episode is never reconsidered after its bounded backoff.
- Tracing events: `redrive_frontier_no_progress` and
  `redrive_no_progress_capped`. This recovery path currently emits tracing logs
  rather than `record_invariant_check` observability rows.

## I16. A redrive re-reads a turn; it does not retire one (#5943)

Numbered I16, not I13: `docs/design/4987-relay-reachability.md` §8.2 reserves
I13/I14/I15 for the reachability obligations. §-1.8 marks §8.2 "대체 → §-1.5",
but that supersedes the section's *content* — §-1.5 says "I13 재작성", and §8.4's
gate plan still reads "계약 문서 게이트 (S3에서 I13–I15 도입 시)". The numbers are
still spoken for, so this one steps past them rather than colliding.

- Definition: the undelivered-backlog redrive must not disarm the
  `turn_delivered` marker, and a resume that moves a watcher BACKWARD must not
  fold that marker into the watcher's sticky `terminal_delivery_observed` latch.
- Producer: `health::relay_auto_heal`'s backlog nudge enqueues a resume point and
  nothing else. It is admitted only for an UNDELIVERED backlog of the turn
  already in flight (`should_redrive_undelivered_backlog`), so it re-reads a turn
  rather than starting one, and the marker is not its to clear. The two real
  producers of `turn_delivered == true` are the ones I5 enumerates — the bridge's
  `run_terminal_outcome_delivery` and the watcher's own
  `run_terminal_commit_epilogue` — and the redrive is neither.
- Consumer: `tmux_watcher::watcher_resume` resolves the queued point. The
  duplicate-relay floor keeps its pre-#5943 rule — pinned AT the resume point
  when the bridge delivered the turn, dropped otherwise — which is what keeps
  `pre_emit_guard`'s `data_start_offset < last_relayed_offset` branch unreachable
  from this path. That branch is not a trim of the already-relayed prefix: it
  suppresses the whole batch, deletes the placeholder and discards the pending
  buffer, so a floor carried ABOVE the resume point would destroy the backlog
  instead of re-relaying it. The latch takes the marker only from a FORWARD
  resume; a backward one re-opens the current turn, so its marker belongs to an
  earlier turn. The watcher still clears the marker once it has consumed the
  resume point — that clear is what keeps the relay-suppression consumer below
  from holding for the rest of the watcher's life.
- Violation surface: FOUR consumers read the live marker for themselves, so
  clearing it at the producer makes all of them wrong at once —
  `pre_emit_guard` (-> `tmux::should_suppress_relay_before_emit`) stops
  suppressing a relay the bridge already delivered, the streaming status tick
  reads it for the same suppression question, the five watcher-observed tmux
  death sites fold it into `terminal_delivery_observed` and so report a delivered
  turn as undelivered, and this resume path folds it too. In the other direction,
  latching a stale marker retires
  `tmux_death_should_attempt_restart_handoff` — the only user-facing signal for
  an abnormal mid-turn pane crash — for the whole remaining life of that watcher,
  because `terminal_delivery_observed` is initialised once per dispatch and never
  reset.
- Scope, stated because it is easy to over-read: this invariant is about the
  redrive DISARMING those guards. Whether the redrive's rewind itself caused the
  duplicate relays observed on 2026-09-15 is NOT established — the 2026-09-16
  issue correction withdrew that reading, leaving only time correlation — and
  nothing here depends on it.
- Tracing events: none of its own. The redrive's own refusals are I12's
  (`redrive_frontier_no_progress`) and I19's.
- Invariant key: `redrive_may_not_disarm_the_delivery_marker` (enforced by
  `relay_auto_heal::tests::redrive_does_not_clear_the_bridge_delivery_marker_5943`,
  the `tmux_watcher::watcher_resume::tests` resume-contract set, and the
  end-to-end `loop_poll_prologue` resume-consumption test; like I12 this path
  emits tracing logs rather than `record_invariant_check` rows).

## I17. A terminal frame has a delivery owner or a record (#5941)

Numbered I17 for the same reason I16 is not I13: `docs/design/4987-relay-reachability.md`
§8.2 still reserves I13/I14/I15 for the reachability obligations, and #5943 took I16.

- Definition — SCOPED to the #5175 denial seam, not to the relay as a whole. A
  terminal frame that REACHES the producer below carrying an undelivered body
  must end with a delivery owner (the session-bound sink, or an authorized
  soft-terminal watcher) or with a durable `relay_dead_letter` row preserving
  that body — not with neither. Frames that never reach it are outside the
  invariant: the seam runs only where the watcher REQUESTED a direct fallback and
  was DENIED authority, so a frame nobody tried to relay, a frame relayed under
  authority, and any loss on a path that does not run `terminal_relay_plan` are
  all unobserved here and none of them can violate it.
- Producer: `tmux_watcher::orphan_terminal_frame::observe_orphan_terminal_frame`,
  called from `terminal_relay_plan` at the #5175 denial seam, where the sink has
  already declined delivery and soft-terminal authority has been denied. The
  record is admitted by `OrphanTerminalFrameFacts::record_required`: a denial, a
  requested but unauthorized watcher fallback, no session-bound terminal
  ownership, no #4081/#4714 duplicate refusal — read RAW, since the routed flag
  ANDs in the authorization this seam has already denied — a NON-EMPTY unsent
  body (18 of the 33 denials in the 2026-09-16 incident carried none), a
  non-zero consumed JSONL range, and
  — the SINK side of the question, which the watcher's own refusal is no
  evidence about — neither a landed-but-unproven POST (`RingUnknown`, the sink's
  `SentButUncommitted`) nor a range at or below the resend-dedup committed
  floor. Without those last two a body already on screen is filed as ownerless.
- Consumer: the #3561 hourly operator monitor. `RELAY_SIGNAL_DEFINITIONS` gains
  two threshold-1 rows — `relay_terminal_authority_denied` (the loss itself,
  whose counter had a producer since #5175 and no consumer) and
  `terminal_frame_without_owner_or_record` (the loss that left no record
  either). The DLQ row has no redelivery consumer yet; recovery is
  operator-driven.
- Violation surface: the record is fire-and-forget by construction
  (`relay_dead_letter::record_detached_reporting` never blocks the watcher loop),
  so the invariant is decided by the WRITE, not by the presence of a pool —
  reported synchronously when no PG pool is configured, and from the detached
  task when the INSERT fails. It fires rather than staying silent because #5941
  lost three assistant answers while every health surface read `healthy`.
- Strength of the guarantee — do not read it as more than it says. The write is
  AT-LEAST-ONCE and NOT idempotent: a process death between the spawn and the
  INSERT loses the row with every surface still reporting intact, and the write
  carries no dedup key, so re-observing the SAME frame files another row. Read
  that as the ordinary case, not the crash case: the watcher polls, and a frame
  that stays terminal and unowned is re-observed on every tick until the offset
  moves, so one loss routinely yields several rows; a restart merely adds to the
  same pile (both pinned by the double-observe test). `D` below is a lower bound
  that may also contain duplicates.
- Boundary: this invariant does NOT claim the body was delivered, and it does
  not advance the delivery frontier — only that the loss is attributable and the
  content recoverable. The upstream fix (the bridge conceding relay authority
  without finalizing, leaving a stale inflight row that denied authority for
  1h45m) is #5944's.
- Audit arithmetic: over a deploy window, `N+` = `#5175` WARN lines with
  `full_response_len > 0`, `N0` = those with `full_response_len == 0`, and `D` =
  `relay_dead_letter` rows with `kind = 'terminal_no_delivery_owner'`, and
  `N+ + N0` is every WARN line. The relation is `D <= N+`, NOT `D == N+`: the
  sink-side conjuncts withhold a row for every WARN whose body the sink had in
  fact delivered, and those WARNs carry a body. `N+ - D` is that residual plus
  the duplicate/lost rows above — a number to explain, not a violation. `D == 0`
  proves nothing either; it is also what a silently broken writer looks like.
- Invariant key: `terminal_frame_has_a_delivery_owner_or_a_record` (enforced by
  `record_invariant_check` in the producer above, and by the
  `terminal_relay_plan_tests` #5941 set, whose deliberate-violation test drives a
  record-required frame through the producer with no pool configured).

## I18. A rewind resend is identified by source bytes, never by sequence (#5948)

Numbered I18 because I13/I14/I15 stay reserved for the reachability obligations
(see I16's note), I16 is taken by #5943, and I17 by #5941.

- Definition: when a watcher rewind re-sends JSONL bytes the session-bound sink
  parser already folded into the turn it is still accumulating, the parser folds
  only the part of the payload it has not seen, and it decides that from the
  frame's absolute source byte range — `StreamFrame::source_span` — not from
  `StreamFrame::sequence` and not from payload equality.
- Why not `sequence`: the relay mints a frame's sequence at the SEND, fresh, per
  frame (`stream_relay::try_send_frame_inner`). A rewind resend is a new send, so
  it carries a strictly LARGER sequence than the original. `sequence` describes
  relay order, never source identity; a receiver-side `sequence <= last_sequence`
  test can therefore never fire on the case this invariant is about.
- Why not payload equality: a turn that prints the same sentence twice is
  byte-identical to a one-line replay. Suppressing on content would delete real
  output, which is the #5941-class silent loss this contract exists to prevent.
  Byte offsets separate the two exactly, because genuinely repeated prose always
  occupies a strictly LATER source range than the prose it repeats.
- Producer: `tmux_watcher::turn_stream_collector` names the absolute range of the
  bytes it forwards on both the initial and the streaming path, and
  `supervisor_relay` carries it to the frame. A result+next-turn chunk is split at
  the terminal boundary, so the span splits with it — the terminal frame owns
  `[start, boundary)` and the tail frame owns `[boundary, end)`. A producer that
  cannot name a range sends `None`. The range is read off the collector's
  EXISTING buffer bookkeeping (`all_data_start_offset` +
  `advance_buffer_start_offset`) — the same coordinate it already hands
  `process_watcher_lines_for_turn` for pre-turn skipping and terminal-evidence
  offsets — so this invariant inherits exactly that coordinate's accuracy and
  introduces no new offset authority. One guard keeps that coordinate truthful
  across a rewind (#5979): the rewind sites move `current_offset` and empty
  `all_data` without reaching `Utf8ChunkDecoder`, so the read that refills an
  EMPTY buffer (`Utf8ChunkDecoder::decode_source_for_buffer`) drops a buffered
  split-scalar tail the read does not continue, instead of gluing it onto the
  replay and re-anchoring the buffer at the abandoned read's offset. Reads into a
  non-empty buffer keep the decoder's mixed-carry contract.
- Consumer: `session_relay_sink::turn_parser::SessionRelayParser::fold_frame_payload`
  keeps one `turn_source_end` watermark. `None` span means fold the whole payload —
  the sink refuses to guess, so an un-instrumented producer degrades to today's
  behaviour rather than to loss. A span is honoured ONLY when it names exactly as
  many bytes as its payload carries; that equality is what makes the overlap a
  byte PREFIX of the payload, and a span that disagrees folds whole rather than
  slice a body on a coordinate the sink cannot trust.
- Scope, stated because it is easy to over-read: the watermark is TURN-scoped.
  `reset_turn` clears it, so suppression only ever applies inside one UNDELIVERED
  turn — which is the actual rewind damage, a single delivery whose prose is
  doubled. A resend that arrives after the turn was handed off reproduces the same
  body rather than a doubled one, and that resend IS the retry the watcher's
  terminal-delivery rewind exists to drive ("must retry the SAME range next
  loop"); suppressing it would convert a failed POST into permanent silent loss.
  Cross-delivery duplicates remain the send point's job. A generation change also
  clears the watermark, because a rotated transcript restarts the offsets.
- Advisory, not a commit coordinate: `source_span` never participates in the
  commit decision. `relay_range` alone still steers
  `advance_after_confirmed_post` onto the idle-range path, which is precisely why
  this is a new field instead of a reuse of `relay_range` — reusing it would have
  silently rerouted streaming frames onto the idle commit.
- The native-Codex terminal path clears the span: `ingest_verified_native_terminal`
  SYNTHESISES its payload rather than reading it from the transcript, so the
  incoming frame's range does not describe those bytes. Seeding the watermark from
  a synthetic body would make the next genuine frame in that range look like a
  replay and drop a real answer.
- Violation surface: fold the same range twice and one delivery carries the same
  prose twice; suppress across the turn handoff and a failed POST becomes silent
  loss.
- Tracing events: `record_relay_resend_suppressed` emits a WARN plus the
  `relay_resend_suppressed` relay root-cause counter, so every suppression lands
  in the restart-safe `observability_events` stream and in the hourly #3561
  operator alert table (`RELAY_SIGNAL_DEFINITIONS`). A suppression is never
  silent.
- Invariant key: `rewind_resend_identified_by_source_bytes` (enforced by the
  `session_relay_sink::turn_parser::resend_dedupe_tests` set; like I12/I16 this path emits
  tracing + counters rather than `record_invariant_check` rows).

## I19. A redrive resumes on a witness, not on a value (#5943)

I16 stopped the redrive from disarming the duplicate-relay guard on its way
past. I19 governs the resume point itself.

- Definition: the backlog redrive may not enqueue a resume point of zero while
  the CURRENT TURN's in-flight row says bytes of it have already been relayed,
  and it may not enqueue zero for a turn whose row records where the turn began.
  Every other resume point, in either direction, is enqueued unchanged.
- Producer: `health::relay_auto_heal::redrive_resume_point`, whose only caller is
  `nudge_watcher_handle_for_backlog`. The offset requested is unchanged from I12
  (`last_relay_offset.max(committed_offset)`); I19 decides what a ZERO means.
- Direction is not consulted, and that is the load-bearing half: a resume point
  behind the read head is the ordinary healthy shape (channel 1479671298497183835,
  2026-09-16 12:42..13:53: eight such redrives). Refusing rewinds as a class
  would convert this invariant into the loss it prevents.
- What is refused: a zero that a durable delivery contradicts. Both I12 readings
  reach zero through a coord that never advanced or was never restored, so the
  VALUE cannot separate "nothing was delivered" from "not restored yet". At
  2026-09-16T14:13:37Z..14:21:32Z five consecutive redrives ran with
  `last_relay_offset=0` and `unread_bytes=24_553_403` — the entire transcript —
  after a dcserver restart dropped the in-memory coordinate.
- Witness: `DurableFrontierObservation::durable_delivery_witness`, carried onto
  the snapshot as `WatcherStateSnapshot::durable_frontier`: the one term the
  restart does not take with it (both I12 readings live in the emptied
  `SharedData::tmux_relay_coords`). `RowPresent` alone is not the predicate —
  `live_generation_mtime_ns` is read off the coordinate ENTRY, so after that
  restart `observe` can only answer `GenerationUnresolved`; both count. Three
  shapes give NO witness: `GenerationMismatch` (a different incarnation, whose
  re-created wrapper legitimately zeroes), a `relayed_start` of zero (the very
  value the guard distrusts), and a row with no relayed offset (`RowUnrelayed`,
  `RowAbsent`).
- Floor (r3): an unwitnessed zero is NOT approved as zero. The row's
  `last_watcher_relayed_offset` is `None` from the turn's birth until its first
  relay, so every fresh turn arrives unwitnessed and zero would re-read every
  earlier turn in the file. The same row's `turn_start_offset` (set at every
  claim; what `recovery_watcher_start_offset` resumes from on reattach) is the
  floor, under that function's rule: a capture shorter than the floor means the
  file was re-created and zero is its start. Only a rowless turn (`RowAbsent`)
  keeps zero: nothing durable says where it began, and a re-post is recoverable
  where a skipped head is not.
- I12's two readings are one reading spelled twice — `redrive_grace` admits a
  redrive only while `snapshot.last_relay_offset == token.committed_offset`, so
  their `max()` is a no-op on every production path into this guard; it is kept
  for the handle-level entry point tests reach with the two apart.
- Not refused, deliberately: a resume point BEHIND the durable witness (the sink
  confirm lag over an already-persisted batch — precisely what recovery is for),
  and an unset `watcher_owner_channel_id` (the rest of the codebase already reads
  that as "the polled channel": `nudge_existing_watcher_for_backlog`,
  `RelayHealthSnapshot::channel_binding`, `idle_recap`).
- On refusal the redrive is counted as a no-progress attempt (r3): the same
  backoff as a nudge, then `redrive_no_progress_capped` after
  `REDRIVE_MAX_NO_PROGRESS_ATTEMPTS`, instead of re-running every poll. It never
  escalates to `ReattachWatcher`: the refusal evidences an unrestored in-memory
  frontier, not a dead watcher, and a reattach would cancel a watcher that is
  still reading to resume it from the same durable row
  (`last_offset.max(turn_start_offset)`, zero only on truncation).
  `nudge_existing_watcher_for_backlog` returns `RedriveNudge` rather than `bool`
  so the `apply` arm can tell a refusal from a nudge that did not apply.
- Violation surface: a re-post of an already-relayed prefix with a longer body
  (the transcript grows between passes). The 2026-09-15 REST scan of adk-cc
  measured 145 bot bodies, 28 re-posts, 22 partial — so this invariant is
  verified by counting re-posts, not byte-identical duplicates.
- What I19 does NOT give you: a duplicate-relay defence past the resume.
  `watcher_resume_outcome` pins the floor AT the requested offset, so
  `pre_emit_guard` cannot fire on the resumed batch — deliberately, since that
  branch discards the pending buffer and would turn a re-post into total loss of
  `[floor, EOF)`. The defences here are the refusal, the floor, and I12's
  no-progress gate.
- Tracing events: `redrive_unrestored_frontier`, in I12's
  `redrive_frontier_no_progress` shape so both refusal reasons count off one
  `event` field; `redrive_no_progress_capped` once per capped refusal episode.
- Invariant key: `redrive_resumes_only_onto_a_restored_frontier` (enforced by the
  `relay_auto_heal::tests` `_5943` set; like I12 and I16 this path emits tracing
  logs rather than `record_invariant_check` rows).

## I20. A live turn is proven by progress, not by the presence or age of its bookkeeping (#5996)

Numbered I20 for the reason I16's note gives: I13/I14/I15 stay reserved for the
reachability obligations, I16 and I19 are #5943's, I17 #5941's, I18 #5948's.

- Definition: a consumer deciding whether a turn is still working may not read it
  from the EXISTENCE of a bookkeeping record — the mailbox active-turn anchor, an
  `intervention_queue` entry, an inflight row, a dispatch reservation — nor from
  that record's AGE. Progress evidence is a term a finished turn cannot produce
  and an unfinished one can: a durable completion witness, or a MEASURED count of
  bytes still unrelayed. Existence and age are admissible only as a bounded
  fallback where the witness is structurally unreadable, never as the authority.
- Authority vs telemetry. AUTHORITATIVE: `InflightTurnState::terminal_delivery_committed`
  (as `stale_synthetic_mailbox_owner_reclaim_reason` reads it),
  `relay_recovery::unread_tail_is_proven_drained` over `SessionEnrichment`'s
  `unread_bytes`, `relay_recovery::idle_tmux_repair_has_unrelayed_tail_answer`'s
  terminal `result` past `last_offset`, and receipt coverage (`ReceiptIndex::covers`
  under `reachability::composite::sweep_coverage`). TELEMETRY, never authority:
  `RelayHealthSnapshot::last_relay_offset`, `mailbox_turn_age_secs`,
  `ChannelMailboxState::turn_started_at`, `queue_depth`, and the bare
  `inflight_state_present` / `mailbox_has_cancel_token` pair. The precedent is
  `health::watcher_respawn::force_clean_respawn_offset_floor`: it discards the
  snapshot offset into `_unfenced_snapshot_frontier`, floors on
  `tmux::committed_frontier_for_current_generation`, and where no fence exists it
  disables the floor rather than falling back to the value. That precedent bounds the
  first list too: `unread_tail_is_proven_drained` is authoritative over a MEASURED
  zero only. Its `Some(0)` is `capture.saturating_sub(last_relay_offset)` in
  `health::session_enrichment::load` over that same unfenced frontier, so it is
  equally the answer when the frontier runs AHEAD of the capture offset — a rotated
  or truncated transcript, or the #4986 split where the row's `output_path` and the
  watcher's file differ — and `relay_state_matches_inflight` compares tmux session
  names only when row and binding BOTH carry one, so another session's frontier can
  surface as `Some(0)` when either side is unnamed. The predicate's own doc comment
  states both. A SATURATED zero and an UNATTRIBUTED zero therefore carry the grade of
  `None` — UNMEASURED, not measured-empty. Separating the SATURATED zero is required and
  possible everywhere: `last_capture_offset` and `last_relay_offset` are both `pub` on
  `WatcherStateSnapshot`. Separating the UNATTRIBUTED zero needs `SessionEnrichment`,
  `pub(super)` to `discord::health`, so outside that module this term is UNMEASURED and a
  consumer there has NOT measured the tail; the row precondition below narrows that case,
  it does not close it, and the term may not decide a retirement alone.
- THE DISCRIMINATOR between (a) state that lingers too long (this issue) and (b)
  state retired too early (#5951 (b), #5775, #5755) is a MEASURED tail, never a
  clock — and it is already implemented, in the idle-tmux branch of the
  `stale-mailbox/repair` route: the row consents
  (`inflight_state_allows_idle_tmux_repair_for_channel`), no terminal answer sits
  past the watermark (`!channel_has_unrelayed_idle_tmux_tail_answer`), and the tail
  is proven drained (`unread_tail_is_proven_drained`). All three hold → (a): the
  record outlived its work, retiring it loses nothing. Any one fails → (b). Those are
  not three defenses on a ROWLESS channel. "The row consents" reads
  `if snapshot.inflight_state_present { .. } else { true }`, so absence passes it, and
  `channel_has_unrelayed_idle_tmux_tail_answer` is `load_inflight_state(..).is_some_and(..)`
  whose note reads "Absent row → no tail answer to lose → false", so `!unrelayed_tail`
  passes too. Rowless — the #5996 shape — the test collapses to the tail term alone,
  and the qualification above is why that term is not unconditional. What keeps this
  route safe is not the conjunction but two row-required preconditions outside it: the
  tail term is `None` when there is no `output_path` to measure, and
  `health::recovery::clear_idle_tmux_stale_turn` returns early when
  `load_idle_tmux_stale_turn_inflight_clear_candidate` finds no row. A consumer
  replicating this test where those are absent must require the row's EXISTENCE as an
  explicit precondition — "the row consents" is true only where a row exists to consent.
- Unmeasured resolves to (b), and that asymmetry is the whole guard. `unread_bytes`
  is three-valued and its `None` is UNMEASURED, not measured-empty, so
  `unread_tail_is_proven_drained(None)` is false; the sibling doc says why the two
  compose only in conjunction ("two blind witnesses do not compose into a proof")
  — `idle_tmux_repair_has_unrelayed_tail_answer` is blind under the same conditions,
  false for an absent path or a failed extract. The cost argument is written at
  `recovery_known_ids::live_pending_dispatch_message_ids`: a false recover costs a
  duplicate, a false suppression costs a message. A wrongly-preserved (a) wedge is
  cleared by the next poll that measures; a wrongly-retired (b) turn is gone.
  `classify_reachability` takes the same rule from the other side — every fault arm
  that can preempt it runs before the timer, "a thing that went WRONG must not be
  retired by a clock".
- Honest gap; L1 must not paper over it. For the EXACT #5996 shape the
  discriminator does not resolve today. `classify_reachability` short-circuits to
  `Unknown(RowlessActiveTurn)` BEFORE it builds the receipt index and runs
  `sweep_coverage`, so a rowless active turn can never obtain the delivery coverage
  proving its answer landed — what the incident measured (`inflight_state_present
  false`, `rowless_active_turn`, the answer delivered four minutes earlier). The
  tail term answers there only through
  `RelayHealthSnapshot::idle_witness_tail_is_not_waiting`'s `!bridge_inflight_present`
  arm, a structural `None`, not a measurement; the route's three-conjunct test collapses
  to that same term here too, so the gap is not the anchor axis alone. I20 does NOT
  authorize releasing that anchor on today's operands; that shape becomes decidable only
  with a receipt read ordered ahead of the short-circuit — follow-up, not this contract.
- Relation to I19 and I17. I19 is witness-vs-value on one field, a zero resume offset a
  restart can fabricate; I20 is witness-vs-existence-and-age across four retirement
  decisions. I19 admits a floor value when unwitnessed; I20 no fallback for the age term
  except an unreadable witness. I17 makes a loss ATTRIBUTABLE; I20 a retirement EARNED.
- Consumer — `turn_orchestrator::release_active_turn_anchor`. Its three callers —
  `finalize_turn_state`, the `ChannelMailboxMsg::Clear` arm, and the force-`PurgeQueue`
  arm's `clear_cancelled_active_anchor` — do not re-derive the release, but TWO
  evidence-driven paths reach them. `synthetic_start::stale_reclaim` reads
  `terminal_delivery_committed` and finalizes a `Cancel` through the identity-guarded
  finish, which lands in `finalize_turn_state`; it is
  DEMAND-DRIVEN — only where a new TUI-direct synthetic start finds the mailbox held —
  and OWNER-SCOPED (`classify_reclaimable_mailbox_owner`). The second,
  `relay_auto_heal::run_orphan_token_auto_heal_pass`, is PERIODIC but NOT evidence-driven:
  `health::recovery::run_stall_watchdog_pass` drives it over every mailbox snapshot into
  the `Clear` arm behind `eligible_orphan_pending_token`, whose age-free
  `..._without_admission_grace` form runs only for the `StallWatchdog` source, not the
  `ProbeAutoHeal` one this sweep uses. Both forms are a ledger PRESENCE
  (`mailbox_has_cancel_token`) over absences, no witness among them, and both turn on
  `tmux_alive == Some(false) || !is_agentdesk_tmux_session(..)`: an UNMEASURED producer
  (`tmux_alive` `None`) passes that gate wherever the session is not named `AgentDesk-*`
  — a PRE-EXISTING candidate violation of this invariant, not the gap filled — and where
  it IS so named that same term refuses the repair (`protected_agentdesk_tmux_session`).
  So what is missing is evidence, not periodicity, and the population is wider than the
  sweep's reach: `stale_thread_proof` also preempts the classifier, after which
  `eligible_stale_thread_proof` refuses that channel too (it requires
  `!mailbox_has_cancel_token`), so the anchor outlives a lost release event in both
  shapes, not only where a producer, watcher, or bridge row survives. Which path carries
  the repair is L1's, provided the deciding term is a witness or a measured tail — that
  sweep's gate is neither, so periodizing it unchanged is the retirement this invariant
  forbids. Any release not driven by a turn-end event owes progress evidence, and neither
  the anchor's presence nor its `turn_started_at` age is that.
- Consumer — the `stale-mailbox/repair` route's `queue_not_empty` gate. A
  `queue_depth > 0` is not "live queue evidence": it is equally the signature of a
  queue that cannot drain — the state the gate is asked to repair — so the
  `skipped_reason` names the opposite of what the field measures. Depth is
  inadmissible as a liveness term; the authority is the three-conjunct test the same
  route already holds in its idle-tmux branch — carried with the row precondition named
  above, because that branch is reached today only after `queue_depth > 0` has returned
  CONFLICT, and retiring the depth gate admits queued channels to it.
- Consumer — `catch_up` phase 2's `existing_ids` membership test.
  `recovery_known_message_ids` unions three sets and only one tests liveness:
  `live_pending_dispatch_message_ids` reads an orphaned reservation as NOT live "so
  a leaked marker can never suppress recovery of a genuinely unanswered message".
  The queued-ids arm applies no such test — presence in `intervention_queue` counts
  a message recovered whether or not it was ever dispatched. The destructive half is
  the `advance_phase2_checkpoint` call, not the skip: a skip is retried next scan, an
  advance forecloses it. Queue membership is not evidence of dispatch, and the
  checkpoint may advance past a message only on evidence of dispatch or answer.
- Consumer — `synthetic_start::stale_reclaim`'s age gate, right on one arm only.
  `stale_synthetic_mailbox_owner_reclaim_reason` reclaims `OwnerInflightFinalized` —
  the row's `terminal_delivery_committed` bit — with NO age gate, and
  `requires_positive_owner_age` confines `STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS`
  to `OwnerInflightAbsent` / `OwnerInflightReplaced`, the two reasons with no row bit
  to read. Its note calls that clock defense-in-depth over a positive proof, true only on
  the REAL-USER arm, where `classify_reclaimable_mailbox_owner` demands the ledger's
  `finished` bit through `is_readopted_mailbox_owner`. A SYNTHETIC owner leaves that
  classifier before any ledger read, and the reason function answers `OwnerInflightAbsent`
  on a `None` row before inspecting anything, so a synthetic-owned ROWLESS mailbox — the
  #5996 shape — is retired on absence plus age alone; only demand bounds that today, and
  periodizing this arm unchanged is the retirement I20 forbids. The age is never the
  authority, and may not be extended to a reason whose witness IS readable.
- What I20 does NOT give you. It does not authorize retiring state on the ABSENCE of
  progress evidence — absence is the unmeasured case, which this invariant sends to
  (b); a consumer reading "no witness" as "retire it" builds the very (b) loss the
  discriminator prevents. It adds no coordinate: every authoritative term above
  already exists and I20 only reassigns which may DECIDE — with one known exception:
  `release_active_turn_anchor` takes only `&mut ChannelMailboxState`, carrying neither
  channel nor provider, so L1 must pass both into that decision point before it can
  record a violation there — `channel_id` is already a `finalize_turn_state` parameter
  and the provider rides its `Option<&QueuePersistenceContext>` WHERE THAT IS `Some` (a
  `None` call site carries no provider), two arguments rather than a new thread of
  identity. Duplicate relays after a retirement stay I18's and I19's.
- It also puts nothing in conflict with the pinned "normal", and no lane may weaken
  that to land a repair. `relay_recovery::tests::unpaired_active_token_is_observe_only`
  pairs `mailbox_turn_age_secs: Some(601)` with a fixture whose `unread_bytes` is
  `None` — UNMEASURED, so `ObserveOnly` is the verdict I20 requires and the test is
  under-specified rather than wrong. A repair acting on that stall state belongs in a
  NEW fixture naming a measured tail, and `scripts/deploy-release.sh` classifying
  `unpaired_active_token` as `obs=` not `marker=` stays correct for the same reason.
- Violation surface: retire on presence or age and a live turn loses its answer (the
  #5951 (b) shape); trust presence or age as liveness and the queue wedges behind a
  turn that finished — 12 minutes on channel 1490141479707086938 on 2026-09-18, with
  `effective_state` already reading `idle`.
- Observability, not optional here: I17 records its own failure on this point, #5175
  leaving a counter with a producer and no consumer so the loss passed silently. Each
  consuming lane wires `record_invariant_check(condition, InvariantViolation {
  invariant: "live_turn_proven_by_progress_not_presence", .. })` where the retirement
  decision is TAKEN, in the row form
  `tmux_watcher::orphan_terminal_frame::observe_orphan_terminal_frame` uses, with
  `details` naming WHICH term decided — witness, measured tail, an unreadable witness, or
  age fallback. An age fallback must be countable apart from a witness, or its growth
  is invisible and the clock silently becomes the authority again. Violations reach an
  operator through the #3561 hourly table only once a lane ADDS the row:
  `RELAY_SIGNAL_DEFINITIONS` matches `event_type = "invariant_violation"` against an
  explicit `statuses` list (`relay_signal_alert`'s `status = ANY($2)`) with no wildcard
  entry, so a key absent from that list counts zero forever — the #5175 shape this
  bullet opened by naming. Nothing backstops that table: `record_invariant_check` emits
  NOTHING while the condition HOLDS, so silence cannot be told from unwired, and the
  `guard_fires` counter a violation bumps is keyed by channel and provider only
  (`record_guard_fire`) — one bucket for all invariants, unable to name which fired.
  One lane adds the threshold-1 entry carrying
  `live_turn_proven_by_progress_not_presence` in `statuses`, as I17 and I18 each did.
- Invariant key: `live_turn_proven_by_progress_not_presence`. This document lands the
  contract only and enforces nothing by itself: steps 2 and 3 below — the
  `record_invariant_check` wiring and a deliberate-violation test per consumer —
  belong to the four consuming lanes, each citing this section; the rowless
  receipt-coverage follow-up is a fifth lane citing the honest-gap bullet. #5996's DoD
  clause — an unpaired active token with no progress evidence must not block the queue
  — needs the anchor and route lanes together and is closed by neither alone (#5946).

## How to add a new invariant

1. Document it here with the same structure (definition, producer,
   consumer, surface, key).
2. Wire `record_invariant_check(condition, InvariantViolation { ... })`
   at every producer site so violations are observable in
   `observability_event` rows (counters + recent records).
3. Add a regression test that intentionally violates the invariant to
   prove the check fires.
4. Reference this document from the relevant sub-issue under #1222.
