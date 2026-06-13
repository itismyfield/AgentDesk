# #3089 — Unify Discord agent-output delivery behind one controller

Status: **Design** (pre-implementation, hardened by one adversarial review round). Issue: #3089. Parent EPIC: #3016 (TurnFinalizer single-authority, closed). Folds in: #3235 (idle-tail dedup), #3078 (status-panel lifecycle), #3088 (TUI external-input streaming parity), #3082 (queued-notice chunk split), #3416 (same-turn offset-monotonic backward write).

This document is the synthesis of three independently-authored proposals (Gateway / Finalizer-actor / durable-datamodel), reconciled adversarially, then hardened against a design-review pass (3 High + 4 Medium findings, all folded in). It is the plan of record for #3089; each phase below becomes its own PR.

---

## 1. Problem and scope

Agent **turn output** (live streaming, terminal/final delivery, recovery/standby/session-bound delivery, headless terminal delivery, external-input/idle-tail delivery) is decided independently by **seven** surfaces. Each surface re-derives "how to send", so ordering, fallback, dedup, duplicate-prevention, and status-panel ownership are not reasonable about as a unit. Control-plane replies (`/help`, `/queue`, `/status`) are explicitly **out of scope** unless they participate in an agent turn.

### 1.1 The seven surfaces and the direct operations (file:line verified on `origin/main` 7f354d8f9)

| Surface | frozen? | direct send/replace sites | uses `DeliveryLeaseCell`? |
|---|---|---|---|
| `turn_bridge/mod.rs` (+ terminal_delivery.rs, headless_delivery.rs, status_panel.rs, single_message_footer.rs) | #3016 | send-ordered terminal, replace-with-outcome ×3 (cancel / prompt-too-long / normal), streaming rollover, headless enqueue+direct fallback | **yes** (Bridge, `terminal_delivery.rs:517/563`) |
| `tmux_watcher.rs` | #3016 | ordered terminal fallback, replace-with-outcome, send-with-reference, streaming rollover, placeholder delete/orphan | **yes** (Watcher, `tmux_watcher.rs:5903/6972`) |
| `session_relay_sink.rs` | #3036 | send-long-rollback (long), replace-with-outcome (short), send-with-reference (new) | **yes** (Sink, `session_relay_sink.rs:332/766`) |
| `standby_relay.rs` | — | send-long-rollback (long), replace-with-outcome (short), send (new) | no |
| `recovery_engine.rs` | — | replace-long-raw (anchored), send-long-raw (TUI-direct), panel completion | no |
| `placeholder_sweeper.rs` | — | edit (abandoned badge), delete (orphan panel) | no |
| `tui_prompt_relay.rs` (external-input / idle-tail tail) | #3016 hotfile | `replace_long_message_raw_with_outcome` for external-input delivery (`tui_prompt_relay.rs:282`); idle-tail read-clamp only (`:2253`) — **not in the lease** (#3235) | no (#3235) |

Turn-output `send_long_message_raw*` + `replace_long_message_raw*` compress to **~15 direct call sites** (send 8 + replace 6 + the `tui_prompt_relay` external-input replace). `edit`/`delete` are scattered behind surface-local wrappers (`delete_nonterminal_placeholder`, `delete_terminal_placeholder`, `edit_channel_message`).

> Review fix (M7): `tui_prompt_relay` was previously omitted. It is a real owner — #3088 (external-input parity) does **not** become mechanical after A6 unless this surface is routed through the controller. It is now an explicit A-phase owner (A6b), not an afterthought.

### 1.2 The four fragmentation axes

1. **`len > 2000` duplicated four ways** under four names with extra per-surface gate bools: `terminal_delivery_should_send_new_chunks` (`can_chain_locally && len>2000`), `watcher_should_send_ordered_new_chunks_for_terminal_fallback` (`full_body && len>2000`), `session_bound_should_send_new_chunks_for_placeholder` (`len>2000`), `standby_should_send_new_chunks_for_placeholder` (`len>2000`). `DISCORD_MSG_LIMIT = 2000` (`mod.rs:217`).
2. **edit-fail fallback placeholder-delete is asymmetric**: sink/standby **never delete** the original on fallback (#2757, `session_relay_sink.rs:906`, `standby_relay.rs:663`); watcher deletes **conditionally** (`watcher_fallback_edit_failure_can_delete_original_placeholder`). Flattening naively re-opens #2757 (streamed-body loss).
3. **Five failure-signal representations**: `LeaseOutcome{Delivered,NotDelivered,Unknown}` (`mod.rs:1671`); `RelaySinkOutcome{TerminalDelivered,TerminalNotDelivered,TerminalUnknown}` (`stream_relay.rs:361`, ≅ LeaseOutcome); `relay_ok: bool`+`retry_from_offset` (watcher); `RelaySinkError::Transient` (`stream_relay.rs:388`); recovery probe-classified outcome.
4. **`status_message_id` has no single owner**: created `single_message_footer.rs:91` / `tmux_watcher.rs:3437`, re-bound `mod.rs:4156`, normalized `status_panel.rs`, read by headless cleanup, reclaimed/cleared by sweeper. 10+ files.

### 1.3 Live motivating defect (#3416)

On the deployed binary, `save_inflight_state_in_root`'s **observe-only** monotonic guard (#3154, `inflight.rs:1003-1048`, gated by same `user_msg_id`+`turn_start_offset`) fires when a TUI-direct synthetic inflight creation + a turn_start watcher-at-offset-0 replacement writes `last_offset`/`response_sent_offset` **backward within the same turn**. Observe-only means the backward write persists. 29 occurrences over 3 days, across binaries (incl. #3358-deployed). This is precisely the *multiple writers updating one turn's offset without a single authority* problem; #3089's single offset authority (Phase B) closes the class structurally and lets the guard be promoted from observe to enforce.

---

## 2. Reusable primitives (do not reinvent)

| Primitive | file:line | What it gives | Durability |
|---|---|---|---|
| `RelayOwnerKind` | `inflight.rs:312` (field `:252`) | durable "who owns" (None/Watcher/StandbyRelay/SessionBoundRelay/Unknown) | **durable** (inflight JSON) |
| `DeliveryLeaseCell` | `mod.rs:1756` | single-winner CAS acquire/commit/release, 3-way outcome, B6 advance gate. **In-memory state machine** `Unleased → Leased{holder,turn,deadline_ms,start,end} → Committed{outcome}` (`mod.rs:1691`). Inline by Watcher/Bridge/Sink. Standby/recovery/sweeper/idle-tail do NOT lease. | process-local (non-durable) |
| `TurnFinalizer` | `turn_finalizer.rs` | `submit_terminal(key,event)→ack`, ledger Pending→Finalizing→Finalized exactly-once. Dormant `FinalizeMsg::{Acquire,Commit,Release}Delivery` (`:417`), `commit_delivery` awaits actor (`:589`) — reverted to inline by #3143. | actor in-memory; finalize side-effects durable |
| `PlaceholderController` | `placeholder_controller.rs` | `TurnGateway` trait + `ensure_active`/`transition`/`detach` + `classify_edit_error` | n/a |
| `outbound/` layer | `outbound/{delivery,decision,policy}.rs` | `decide_policy` → `LengthPolicyDecision{Inline,Split}`, `deliver_outbound`. **No namespace cap** (`audit_maintainability_config.toml:6` has no `outbound/**`). | n/a |
| durable markers | `inflight.rs:145/151/196` | `response_sent_offset`, `terminal_delivery_committed`, `last_watcher_relayed_offset` (durable **START**, written `tmux_watcher/commit_decisions.rs:75`) | **durable** |

Two load-bearing correctness facts:

- **`confirmed_end_offset` is deliberately non-durable** (`mod.rs:1574`, reset to 0 on restart). The terminal END is frame-carried on the RESULT StreamFrame; the racy inflight "Part(a)" persist was removed (`tmux_watcher.rs:5142`, `:5991-5995`). Re-persisting END must not re-introduce that race.
- **#3143**: routing delivery commit through the actor's `Terminal` mailbox left `confirmed_end_offset` stale across the await while the sink deduped on `committed_relay_offset` reads → same range re-relayed → duplicate (`tmux_watcher.rs:6936`). Fix kept commit+advance **inline**.

---

## 3. The three proposals, compared

| | Gateway | Finalizer-actor | durable-datamodel |
|---|---|---|---|
| Core move | thin `outbound/` facade over `PlaceholderController` + `decide_policy` | activate a delivery ledger phase in `TurnFinalizer` | durable per-turn lease in inflight JSON |
| Unifies surfaces (AC1-5) | **yes, fast** | partial | later (state first) |
| AC6 durable lease across restart | no | no (actor in-memory) | **yes (only one)** |
| Folds #3235 idle-tail | no | needs lease adoption | **yes** |
| Strengthens delivery↔finalize exactly-once | no | **yes** | no |
| Top risk | leaves state/race unsolved | **#3143 structurally re-openable** | migration / mixed-binary / I/O |
| Disposition | **adopt as skeleton** | **reject ledger-routed commit; adopt its invariant** | **adopt the durable lease** |

The Finalizer proposal's own conclusion is that routing commit through the actor re-creates #3143, so the actor can only be an *after-the-fact mirror*. A durable lease is a strictly better restart-surviving authority than an in-memory mirror, so the durable-datamodel layer subsumes the actor's only safe role. We therefore keep the existing inline `submit_terminal` finalize path and do **not** wake the dormant ledger-routed commit.

---

## 4. Decision — layered synthesis

Adopt **Gateway facade (surface unification) + durable delivery-lease state machine (state authority)**. Reject Finalizer ledger-routed commit; promote its invariant to a tested rule.

### 4.1 Invariants

- **I1 — commit+advance is owned by the controller, inline, before any post-send await.** Review fix (H2): an `async deliver_turn_output(...).await -> DeliveryOutcome` that hands the outcome back for the caller to commit is **insufficient** — the watcher already has post-send awaits before advance (`tmux_watcher.rs:6673/6885`, advance `:6972`), so a caller-side commit can land after an await and re-open #3143. Therefore the controller performs `lease.commit() + confirmed_end advance` **internally, synchronously, immediately after confirmed transport success and before it does any cleanup/status/await work**, and returns an already-committed outcome. Enforced by a blocking fake-gateway test asserting the offset is advanced before any post-send await can run.
- **I2 — ambiguous never advances.** `Unknown`/`Transient` must not advance the committed offset (`mod.rs:1677`).
- **I3 — durable lease is additive and conservative.** Missing/unknown record → fall back to the conservative side (offset NOT advanced; treat as not-yet-delivered), never "assume delivered".
- **I4 — controller decides, durable writes go through existing flock CAS helpers** (`bind_status_panel`, `clear_status_panel_if_current`, `save_inflight_state_if_matches_identity`) — no new non-atomic load→mutate→save race.

### 4.2 Controller API (in `outbound/turn_output_controller.rs`, no cap)

```rust
// Commit+advance happen INSIDE this fn (I1); the returned outcome is already committed.
pub(in crate::services::discord) async fn deliver_turn_output<G: TurnGateway + ?Sized>(
    gateway: &G,
    ctx: TurnOutputCtx<'_>,
) -> DeliveryOutcome;

struct TurnOutputCtx<'a> {
    turn: turn_finalizer::TurnKey,
    owner: RelayOwnerKind,
    holder: LeaseHolder,                  // Watcher{id}/Sink/Bridge
    lease: &'a DeliveryLeaseCell,         // borrowed; controller drives acquire→send→commit→release internally
    channel_id: ChannelId,
    placeholder: PlaceholderState<'a>,    // None | Active{message_id, key}
    body: &'a str,
    send_range: (u64, u64),
    plan: OutputPlan,                     // from outbound::decide_policy (Inline→Replace, Split→SendNewChunks)
    edit_fail_policy: EditFailPlaceholderPolicy, // explicit, no default (M-#2757)
}

enum OutputPlan { Replace { lifecycle: PlaceholderLifecycle }, SendNewChunks { chunk_count: usize }, NoOp }
//                        ^ distinguishes the 3 replace variants (cancel/prompt-too-long/normal), recon risk #5
enum DeliveryOutcome { Delivered { committed_to: u64 }, Transient { retry_from_offset: u64 }, Unknown, Skipped }
enum EditFailPlaceholderPolicy { PreserveAlways, DeleteIfProvenStale }
```

### 4.3 Durable delivery-lease state (in a new `delivery_record.rs`)

Review fix (H1): a record that persists only `committed_range` cannot tell "Leased-but-not-Committed" (restart mid-POST) from "never leased", so it does NOT satisfy AC6. Review r2 (H): a single state machine that folds `Committed` into the lease also breaks, because release clears the lease back to `Unleased` (`mod.rs:1952`) — persisting that would erase the restart hydration data — and not every commit is a *delivered* commit (`Unknown`/`NotDelivered` must not advance, I2). So split into **two independent durable fields**: a transient lease and a release-surviving delivered frontier.

```rust
// delivery_record.rs holds the types and, by default (sidecar option §4.4), the
// authoritative `DeliveryRecord { delivery_lease, delivered_frontier }` lives in
// its OWN store outside the inflight `*.json` scan path — NOT in InflightTurnState.
// The two fields below land inline in inflight.rs ONLY under the rollout-gate
// fallback, each paired with an offsetting prod-LoC deletion in the same PR (M5).
#[serde(default)] pub delivery_lease:    Option<DurableLease>,     // live/in-flight; CLEARED on release
#[serde(default)] pub delivered_frontier: Option<DeliveredCommit>, // SURVIVES release; only Delivered writes it

struct DurableLease {
    holder_id: LeaseHolderId, attempt_id: u64, range: (u64,u64),
    deadline_epoch_ms: u64,        // ABSOLUTE wall-clock (review r2 H) — process-monotonic lease_now_ms()
                                   //   (mod.rs:1721) is meaningless after restart
    holder_generation: i64,        // distinguishes a pre-restart holder from a live one
}
struct DeliveredCommit {           // the durable mirror of confirmed_end_offset
    range: (u64,u64), generation_mtime_ns: i64, attempts: u32, panel_msg_id: Option<u64>,
}
```

- **Only a `Delivered` outcome writes/advances `delivered_frontier`** (I2 preserved). `Unknown`/`NotDelivered` clear `delivery_lease` **without** touching the frontier — so an ambiguous commit never advances the durable offset.
- **`release` clears `delivery_lease` only**; `delivered_frontier` persists. On restart, `delivered_frontier` hydrates `confirmed_end_offset` (no 0-reset); a leftover `delivery_lease` is the in-flight state to reconcile.
- **Restart lease reconciliation** (review r2 H): local-instance leases (`holder_generation` ≠ current) are **reclaimed immediately** on restart (their process-monotonic deadline is dead); only remote/cluster holders are judged by `deadline_epoch_ms` (wall-clock) + heartbeat.
- **Write-frequency rule**: persist on lease *transition* (acquire/commit/release), not on every mid-stream advance — mid-stream stays in the memory atomic; keeps frame-carried-END intact and bounds I/O.
- **"terminal commit" timing** (review M4): `delivered_frontier` is written only **after a confirmed Discord POST and the identity-gated inline advance** (I1) — never the pre-sink Part(a) write that was removed (`tmux_watcher.rs:5142`).
- Absorbs `relay_owner_kind` (dual-write during migration), `status_message_id` (→ `delivered_frontier.panel_msg_id`, single authority), `terminal_delivery_committed` (→ presence of `delivered_frontier`), `recovery_relay_attempts` (→ attempts).

### 4.4 Mixed-binary safety (review fix H3 — this is a hard blocker for Phase B)

Old binaries deserialize into their older `InflightTurnState` and reserialize with `serde_json::to_string_pretty` (`inflight.rs:1181`, `:2214`), **dropping unknown fields**. Standby rewrites the same `{channel_id}.json` on its heartbeat (`standby_relay.rs:434`). So a `#[serde(default)]` field added on the new primary is silently erased by an old standby, and a B2 reader that has flipped to record-authority then sees a missing record and falls back to legacy markers (which lack durable END) → duplicate or lost range. `#[serde(default)]` + tolerant deserialize is **not** sufficient.

Resolution (pick one, decided at B-start):
- **(a) sidecar delivery record — default.** A separate store old binaries never read **or scan**. Critical (review r2 H): it must live **outside the inflight provider directory's `*.json` scan path** — old binaries enumerate every `*.json` there and *delete malformed* entries (`inflight.rs:2577`), so a sibling `{channel_id}.delivery.json` would be reaped. Use a dedicated `delivery_records/` subtree **or** a non-`.json` extension the old scan ignores. With the sidecar, the inline `inflight.rs` `delivery_lease`/`delivered_frontier` fields are NOT added (no inflight LoC pressure, no old-writer erasure).
- **(b) rollout gate** that guarantees no old writer is live before B2 flips authority (cluster-version fence). Only this option uses the inline `inflight.rs` nested fields; it needs fleet-wide version coordination, so it is the fallback.

Either way, B2 must not flip read-authority until the chosen isolation holds.

---

## 5. Phased plan (each phase its own PR; ratchet stays green)

`giant_file_ratchet` blocks only LoC **increase**; verbatim `git mv` out of a frozen owner lowers its baseline. Precedent: #3379 (`31cdd73f2`, R100 git mv + re-export shim). Ratchet facts (review M5): test LoC is excluded **only for whole `#[cfg(test)] mod` blocks**, not arbitrary `#[cfg(test)]` items (`audit_maintainability/checks/giant_files.py`, `generate_inventory_docs.py:189`); `inflight.rs` sits **exactly at baseline 2523** (`audit_maintainability_giant_baseline.toml:176`) so any prod field needs an offsetting deletion **in the same PR**.

### Phase A — Gateway / surface unification (AC 1–5)
- **A0 characterization** — pin all ~15 send/replace sites: ordering, edit-fail preserve vs delete (#2757 both arms), the five failure signals, streaming rollover. Tests go in **whole `#[cfg(test)] mod tests` blocks** (M5) → prod-LoC-neutral → ratchet green. Must pass before any cutover.
- **A1 controller skeleton** — new `outbound/turn_output_controller.rs`: `deliver_turn_output` (commit+advance internal, I1) + `OutputPlan::from_length_decision` + `DeliveryOutcome`/`EditFailPlaceholderPolicy`. Wires `PlaceholderController.transition` + `outbound::deliver_outbound`. Blocking fake-gateway test for I1 (advance-before-post-send-await). No owner connected (pure add, no cap).
- **A2 cut over `session_relay_sink` first** (review M6) — the lease-using surface (`session_relay_sink.rs:766`), behind a flag, to validate lease/outcome semantics end-to-end (not standby, which doesn't lease). Frozen #3036, so cutover shrinks it → baseline lowers → green.
- **A3 standby_relay** — transport-only validation (no lease); not frozen, lowest LoC risk.
- **A4 watcher (#3016), A5 turn_bridge (#3038), A6a recovery_engine** — one PR per owner, verbatim `git mv` of the send body into the controller + a call site; each owner shrinks; `relay-e2e` per PR. `EditFailPlaceholderPolicy` injected explicitly per owner (no default) so watcher's conditional-delete never reaches sink/standby (#2757 fence).
- **A6b `tui_prompt_relay` external-input tail** (review M7) — route external-input delivery (`tui_prompt_relay.rs:282`) through the controller. This is what actually makes #3088 mechanical; without it #3088 stays open.
- After Phase A: one entry point, one chunker (`formatting::split_message`), one outcome enum, one explicit fallback policy. AC 1–5 met. Lease durability untouched.

### Phase B — durable delivery-lease + #3235 + #3416
- **B0 mixed-binary isolation** — implement §4.4 (sidecar file or rollout gate) FIRST. Nothing in B persists authority until this holds.
- **B1 add the two fields, shadow-write** — `DurableLease`/`DeliveredCommit` in `delivery_record.rs`. With sidecar (default) the record lives in its own store (no `inflight.rs` LoC change); with the rollout-gate fallback the two nested fields land in `inflight.rs` **paired with an offsetting prod-LoC deletion in the same PR** (M5) or an inflight split. Existing read-authority stays legacy markers; record shadow-written. Shadow assert checks **`delivered_frontier.range.end` against in-memory `confirmed_end_offset`** right after the same commit/advance, paired with `generation_mtime_ns` (review M4 — END is the risky datum, not START).
- **B2 authority flip + idle-tail join** — flip read-authority to `delivered_frontier` (only after B0 holds); bring `tui_prompt_relay` idle-tail (`:2253`) into the lease (acquire/commit, replacing read-clamp). Closes **#3235** and removes the uncoordinated-writer class behind **#3416**.
- **B3 demote memory lease to a cache + enforce the guard** — on restart, hydrate `confirmed_end_offset` from `delivered_frontier` and reconcile any leftover `delivery_lease` (local-instance → reclaim, remote → deadline_epoch_ms/heartbeat); promote the #3154 observe-only monotonic guard (`inflight.rs:1025`) to **enforce** now that a single authority exists (closes #3416). Deprecate legacy duplicate markers behind a version bump with a one-release compat window. **AC6 met here.**

### Phase C — optional
Fold delivery into the finalizer ledger only if A+B leave a measured exactly-once gap. Honor I1 (advance inline; actor as after-the-fact mirror). Default expectation: not needed.

---

## 6. Acceptance criteria mapping

1. one controller API used by all surfaces → A1–A6b.
2. raw `send/replace/edit/delete` removed from owners or wrapped only in controller → A2–A6b (the ~15 sites collapse into the controller).
3. **durable per-turn delivery owner/lease across restart/recovery** → **B1–B3** via the full lease state machine (H1), gated by mixed-binary isolation (H3).
4. identical multi-chunk ordering → A1 (`LengthPolicyDecision` + single `split_message`), pinned by A0.
5. fallback/delete/orphan tested once at the controller layer → A1 controller-layer test of both `EditFailPlaceholderPolicy` arms.
6. command replies stay on simple helpers → explicit out-of-scope guard test.

---

## 7. Regression risks and mitigations

1. **#2757 flattening** → `EditFailPlaceholderPolicy` injected explicitly per owner, no default; A0 pins both arms; cutover blocked until A0 green.
2. **#3143 duplicate re-open** → I1 promoted to a tested invariant; commit+advance owned inside the controller before any post-send await (H2); blocking fake-gateway test; never wake dormant ledger-routed commit.
3. **Leased-but-not-Committed lost on restart** (H1 / r2-H) → persist `delivery_lease` and `delivered_frontier` as **separate** fields; release clears only the lease; only `Delivered` writes the frontier; reclaim local-instance leases on restart (process-monotonic deadline dead), judge remote holders by `deadline_epoch_ms`.
4. **Mixed-binary authority split** (H3 / r2-H) → B0 sidecar **outside the `*.json` scan path** (`inflight.rs:2577` reaps malformed siblings) or a rollout gate, before any authority flip; conservative I3 fallback (missing record ≠ delivered).
5. **#1270 rotation-vs-respawn watermark** → write `generation_mtime_ns` in the same atomic transition as `range`; characterization test for both #1270 scenarios across restart.
6. **ratchet** (M5) → A0 tests in whole `#[cfg(test)] mod` blocks; B1 pairs the inflight field with an offsetting deletion or an inflight split.
7. **recon count drift** → replace is 6 not 4; `OutputPlan::Replace` carries `PlaceholderLifecycle` to keep the cancel/prompt-too-long/normal variants distinct.

---

## 8. Sequencing with sibling issues

- **#3235** closes in B2 (idle-tail joins the lease).
- **#3416** closes in B2/B3 (single offset authority + observe→enforce guard).
- **#3078** (status-panel single controller) largely satisfied by the `panel_msg_id` single authority in B1 + existing CAS helpers; track residual there.
- **#3088** (TUI external-input parity) closes in **A6b** (explicit owner; NOT automatic after A6a).
- **#3082** (queued notice splitting an answer) covered by the single chunker in A1.

Recommended order: **A0 → A1 → A2 (sink, lease proof) → A3 (standby) → A4 (watcher) → A5 (turn_bridge) → A6a (recovery) → A6b (tui_prompt_relay, closes #3088) → B0 (mixed-binary isolation) → B1 → B2 (#3235, #3416) → B3 (AC6, enforce guard) → reassess C / #3078 residual**.
