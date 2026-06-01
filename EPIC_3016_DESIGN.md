# EPIC #3016 — Single-Authority TurnFinalizer Design

> Auto-generated design synthesis (read-only analysis). Base: 58ffe24c1 (#3012).

## EPIC #3016 — Single-Authority TurnFinalizer: Implementation Design

### 0. Problem restated (grounded in current code)

Today finalization is a distributed handshake. `mailbox_finish_turn` (mod.rs:3301) is called directly from ~17 sites; the bridge↔watcher handoff uses a per-handle `mailbox_finalize_owed: Arc<AtomicBool>` (mod.rs ~989, set Release at turn_bridge/mod.rs:3634, consumed AcqRel at tmux_watcher.rs:3674 & 5960, revoked via `compare_exchange` at turn_bridge/mod.rs:6455). The `global_active` counter is decremented in ≥10 places with mixed orderings, then post-hoc clamped by `normalize_global_active_counter` (health/snapshot.rs). Offsets (`response_sent_offset`, `last_watcher_relayed_offset`) and the session→channel map have multiple writers. The races: (a) bridge revoke vs watcher swap (compare_exchange CAS handoff), (b) `lifecycle_stage_paused` gate-timeout where the watcher SKIPS the whole `if terminal_output_committed && !lifecycle_stage_paused` block (tmux_watcher.rs ~5908) so nothing finalizes until the 1800s placeholder sweeper — the "next pass reconciles" deferral that never fires if the pane stays busy.

The fix: introduce **one actor, `TurnFinalizer`**, that OWNS the four side-effects of finalize as an atomic unit — (1) mailbox cancel_token release via `mailbox_finish_turn`, (2) inflight clear, (3) active-counter decrement, (4) offset/session-map authority commit. Every other actor STOPS calling `mailbox_finish_turn`/`global_active.fetch_sub`/`clear_inflight_state` directly and instead SUBMITS a terminal event. The finalizer has exactly one reconciler covering all terminal paths, with time heuristics demoted to a last-resort backstop, not a primary path.

This is landed incrementally: the finalizer is introduced as the single sink first, the existing call-sites are rewired to submit to it one at a time (each rewire is behaviour-preserving because the finalizer reproduces today's `mailbox_finish_turn` + counter + inflight sequence), and only after all sites route through it do we delete the `mailbox_finalize_owed` CAS protocol.

---

### 1. New module: `src/services/discord/turn_finalizer.rs`

A per-runtime actor, held as `Arc<TurnFinalizer>` on `SharedData` (next to `global_active`, mod.rs:1534). One owning task drains an `mpsc` of terminal events; all public methods are cheap submit-or-await wrappers. The actor is the ONLY code that touches `mailbox_finish_turn`, `global_active`, `clear_inflight_state`, and the offset/session authorities.

```rust
pub(super) struct TurnFinalizer {
    tx: mpsc::UnboundedSender<FinalizeMsg>,
    // per-channel terminal-state ledger, owned by the actor task only
}

/// One key per logical turn. Channel-scoped because the mailbox/cancel_token
/// are channel-scoped today; generation guards stale paused-survivors.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct TurnKey {
    channel_id: ChannelId,
    user_msg_id: u64,      // 0 == "unknown identity" (recovery/orphan paths)
    generation: u64,       // dcserver restart generation (#3011/#3019)
}

/// Every actor submits ONE of these. The finalizer decides exactly-once.
pub(super) enum TerminalEvent {
    Complete   { offsets: OffsetCommit, relay_owner: RelayOwnerKind },
    Cancel     { reason: CancelReason },            // /!stop, reaction, watchdog
    ForceCancel{ reason: ForceReason },             // health endpoint, restart drain
    GateTimeout{ pane_quiescent: Option<bool> },    // #2293/#2780 TUI gate
    RelayMiss,                                       // no output owner, empty response
    PaneDeath,                                       // #3014 tmux EOF / no live pane
    DirectInput,                                     // external tmux input adoption
    FastTerminal,                                    // E-18 provider-native
    HeartbeatStale,                                  // B-wire sweeper (60s)
    PlaceholderAbandon,                              // C-wire sweeper (1800s) — backstop only
}

pub(super) struct OffsetCommit {     // #3017 — committed atomically with finalize
    response_sent_offset: usize,         // bridge-authored
    last_watcher_relayed_offset: Option<u64>,
    last_watcher_relayed_generation_mtime_ns: Option<i64>,
}

enum FinalizeMsg {
    Start { key: TurnKey, session_binding: SessionBinding },   // #3018 register
    Terminal { key: TurnKey, event: TerminalEvent, ack: oneshot::Sender<FinalizeOutcome> },
    OffsetProgress { key: TurnKey, partial: OffsetCommit },    // #3017 in-flight writes
    Tick,                                                       // backstop reconcile pass
}

pub(super) enum FinalizeOutcome {
    /// This submission performed the one finalize.
    Finalized { removed_token: Option<Arc<CancelToken>>, has_pending: bool, mailbox_online: bool },
    /// Another actor already finalized this turn; submitter must do nothing.
    AlreadyFinalized,
    /// Turn not terminal yet (e.g. GateTimeout with pane still busy and a relay
    /// owner alive) — recorded; reconciler will fire when the precondition clears.
    Deferred,
}
```

#### Per-turn ledger state machine (owned solely by the actor task)

```
Pending  --(first terminal event accepted)-->  Finalizing  --(finalize side-effects done)-->  Finalized
```

- The ledger stores, per `TurnKey`: current `Phase`, the latest `OffsetCommit`, the live `RelayOwnerKind`, and a `terminal_deadline: Option<Instant>` used only as backstop.
- **Exactly-once is decided in ONE place**: the actor's handling of `FinalizeMsg::Terminal`. It does a single check-and-set on `Phase`:
  - if `Pending` → transition to `Finalizing`, run `do_finalize`, transition to `Finalized`, reply `Finalized{..}` to the submitter, reply `AlreadyFinalized` to everyone who lost.
  - if already `Finalizing`/`Finalized` → reply `AlreadyFinalized`.
  - `GateTimeout` with `pane_quiescent == Some(false)` and a still-alive relay owner is the ONLY event that yields `Deferred`: it sets `terminal_deadline = now + GATE_BACKSTOP` and arms the reconciler, but does NOT finalize. All other terminal events finalize immediately.
- Because this runs inside a single-task actor, the check-and-set needs no CAS, no AcqRel handoff, no `mailbox_finalize_owed`. Ordering is by message arrival; there is no inter-thread race to reason about.

#### `do_finalize` — the single owner of the four side-effects

```rust
async fn do_finalize(&actor, key, event, latest_offsets) -> FinalizeOutcome::Finalized {
    // (A) #3017 offset authority: commit the canonical offsets to inflight BEFORE clearing,
    //     monotonic-guarded (no backward motion, response_sent_offset <= full_response.len()).
    self.offset_authority.commit(key, latest_offsets);

    // (B) inflight clear — honours restart_mode / rebind_origin preservation
    //     (GuardedClearOutcome::PlannedRestartSkipped at inflight.rs:1248) exactly as today.
    let cleared = inflight::clear_inflight_state(provider, channel_id);  // or _if_matches for sweepers

    // (C) mailbox cancel_token release — the ONE call to mailbox_finish_turn in the codebase.
    let finish = mailbox_finish_turn(shared, provider, channel_id).await;
    if let Some(tok) = &finish.removed_token {
        if !event.is_cancel() { tok.mark_completion_cleanup(); }   // provider.rs:1378
        tok.cancelled.store(true, Relaxed);                        // stops stale watchdog
    }

    // (D) #3019 active-counter: decrement ONLY here, ONLY when removed_token was present
    //     (i.e. this submission actually removed the active turn). No fetch_update guesswork,
    //     no normalize_global_active_counter band-aid.
    if finish.removed_token.is_some() {
        shared.global_active.fetch_sub(1, Relaxed);
    }

    // (E) #3018 session→channel map: release the binding for this turn.
    self.session_authority.release(key);

    // (F) post-finalize side-effects that today trail mailbox_finish_turn at the call-sites
    //     (watchdog override clear, dispatch_thread_parents retain, voice barge-in drain).
    //     These move INTO the finalizer so they cannot diverge between paths.
    record_turn_bridge_invariant(finish.removed_token.is_some(), ...);  // keep the invariant probe
    FinalizeOutcome::Finalized { removed_token: finish.removed_token, has_pending: finish.has_pending, mailbox_online: finish.mailbox_online }
}
```

Idempotence is structural: `mailbox_finish_turn` is already idempotent (returns `removed_token: None` on the second call), and the ledger gate means `do_finalize` runs at most once per `TurnKey` anyway. The counter decrement is gated on `removed_token.is_some()`, so even a hypothetical double-entry cannot underflow.

---

### 2. The ONE guaranteed reconciler (covers all terminal paths incl. gate-timeout)

There is exactly one reconciler: the actor's handling of `FinalizeMsg::Tick`, plus the immediate-finalize path inside `Terminal`. Together they guarantee every terminal path finalizes once:

- **Event-driven (primary)**: 8 of the 10 terminal events finalize synchronously on arrival (Complete, Cancel, ForceCancel, RelayMiss, PaneDeath, DirectInput, FastTerminal, and GateTimeout-with-quiescent-pane). No timer involved.
- **Deadline-armed (gate-timeout)**: `GateTimeout{pane_quiescent: Some(false)}` arms `terminal_deadline`. The `Tick` reconciler (fires every `RECONCILE_INTERVAL`, e.g. 1s) re-checks: if the pane is now quiescent OR the relay owner has died OR `now >= terminal_deadline` → finalize. `GATE_BACKSTOP` is short (e.g. 5–10s), NOT 1800s. This is the key fix: the watcher no longer SKIPS finalize and silently leaves the turn for a far-future sweeper. The pane-busy case is *recorded with a bounded deadline owned by the finalizer*, not deferred to a hypothetical next watcher pass.
- **Backstop-only heuristics**: `HeartbeatStale` (60s) and `PlaceholderAbandon` (1800s) become ordinary terminal events submitted by the sweepers. They are LAST resort — by the time they fire, the deadline-armed reconciler should already have finalized. If the ledger has no entry for the `TurnKey` (e.g. post-restart orphan with inflight on disk but no live turn), the finalizer still runs `do_finalize` driven by the inflight identity, so orphans are covered.

The `Tick` pass also sweeps `Finalizing` entries that somehow stalled (defensive: should be unreachable since `do_finalize` is synchronous within the task) and garbage-collects `Finalized` entries after a short TTL so the ledger stays bounded.

---

### 3. How #3017 / #3018 / #3019 fold in

- **#3019 (active-counter)**: `global_active.fetch_sub` exists in exactly one place — `do_finalize` step (D). Every other `fetch_sub`/`fetch_update` site is deleted and routed through a terminal submission. The matching `fetch_add` stays at intake (intake_turn.rs / health.rs open-turn), paired 1:1 with a `Start` submission so the finalizer's ledger knows the turn exists. `normalize_global_active_counter` (health/snapshot.rs) is retained for one release as a safety assertion (log if it ever clamps), then deleted once telemetry shows zero clamps. The token-Drop approach from the analysis is NOT adopted (Drop-based decrement re-introduces ordering ambiguity with the async mailbox call); explicit terminal submission is the authority.

- **#3017 (offset/watermark)**: offsets are committed only inside `do_finalize` (step A) and via `OffsetProgress` messages during the turn. Bridge keeps authoring `response_sent_offset`; watcher keeps authoring `last_watcher_relayed_offset`; both now *submit* their values to the finalizer's `offset_authority` instead of writing inflight directly. The authority enforces monotonicity and `relay_owner`-matches-field. Direct offset writes in recovery_engine.rs / tmux.rs become reads of the authority snapshot.

- **#3018 (session→channel map)**: a `SessionAuthority` (wrapping today's `tui_prompt_dedupe` maps + `tmux_watchers` fallback) is the single resolver. `Start` registers `(provider, session_id) → tmux_session_name → channel_id`; `do_finalize` releases it. `tmux_watcher.rs` session lookups (the `None`-fallback at ~2126 that causes false legacy-relay fallback) resolve through `SessionAuthority::resolve_channel`, with inflight-on-disk as the explicit last-resort tier rather than an ambiguous parallel source.

---

### 4. Incremental landing plan (safe in the churned files)

Phase 1 — Introduce `turn_finalizer.rs` + `Arc<TurnFinalizer>` on `SharedData`; implement `do_finalize` as a thin wrapper that reproduces today's exact `mailbox_finish_turn` + counter + inflight sequence. No call-sites changed yet. Ships dormant behind the actor.

Phase 2 — Route the two **bridge** finalize branches (turn_bridge/mod.rs:6378 missing-handoff, 6486 non-delegation) through `finalizer.submit(Complete/Cancel/RelayMiss)`. Delete the bridge's `global_active.fetch_sub`/`fetch_update` (6403, 6513) and the `mailbox_finalize_owed.compare_exchange` revoke (6455) — the finalizer's ledger replaces the CAS.

Phase 3 — Route the **watcher** terminal (tmux_watcher.rs:5960/3674) and the `lifecycle_stage_paused` skip (~5908). The skip becomes `submit(GateTimeout{pane_quiescent})` instead of silent skip; the deadline-armed reconciler owns the rest. Delete watcher `mailbox_finalize_owed.swap` and direct `clear_inflight_state`/`finish_restored_watcher_active_turn`.

Phase 4 — Route recovery/sweeper/intake-gate sites (health/recovery.rs, recovery_engine.rs, relay_recovery.rs, placeholder_sweeper.rs, inflight_heartbeat_sweeper.rs, intake_gate.rs, turn_start.rs, tmux.rs monitor-auto-turn) to `submit(ForceCancel/PaneDeath/HeartbeatStale/PlaceholderAbandon/...)`.

Phase 5 — Remove now-dead `mailbox_finalize_owed` field + `turn_delivered` handoff (replaced by ledger's `RelayOwnerKind` + Finalized phase suppressing re-relay), and remove `normalize_global_active_counter` once telemetry is clean.

Each phase is independently revertible; after Phase 1 the only externally observable change is which code path issues the (identical) finalize, so regressions are bisectable per phase.

## Call sites to redirect

- src/services/discord/turn_bridge/mod.rs:6378-6408 (missing-watcher-handoff branch) — replace direct mailbox_finish_turn + removed_token.cancelled.store + global_active.fetch_update with finalizer.submit(TurnKey, RelayMiss/Complete).await; keep record_turn_bridge_invariant inside finalizer
- src/services/discord/turn_bridge/mod.rs:6455 — REMOVE mailbox_finalize_owed.compare_exchange(true,false) revoke; ledger phase-gate replaces the CAS handoff
- src/services/discord/turn_bridge/mod.rs:6486-6515 (non-delegation branch) — replace mailbox_finish_turn + removed_token handling + global_active.fetch_sub(6514) with finalizer.submit(Complete/Cancel).await
- src/services/discord/turn_bridge/mod.rs:3634 — REMOVE mailbox_finalize_owed.store(true, Release); replaced by finalizer.submit(Start{relay_owner: Watcher})
- src/services/discord/turn_bridge/mod.rs:7549 (turn_delivered.store) — REMOVE; ledger Finalized-phase + RelayOwnerKind suppresses watcher re-relay
- src/services/discord/turn_bridge/mod.rs:8230,8233 (fetch_sub) — route through finalizer
- src/services/discord/tmux_watcher.rs:3674 (mailbox_finalize_owed.swap + clear_inflight_state:3677) — replace with finalizer.submit(Complete/RelayMiss).await; on AlreadyFinalized do nothing
- src/services/discord/tmux_watcher.rs:5960 (mailbox_finalize_owed.swap) + 5961 clear_inflight_state + 5997 finish_restored_watcher_active_turn — replace with finalizer.submit(Complete).await
- src/services/discord/tmux_watcher.rs:~5908 (if terminal_output_committed && !lifecycle_stage_paused skip) — replace SKIP with finalizer.submit(GateTimeout{pane_quiescent}); deadline-armed reconciler owns finalize
- src/services/discord/tmux_watcher.rs:6068 (turn_delivered.store(true,Release)) — REMOVE; superseded by ledger phase
- src/services/discord/tmux_watcher.rs:~2126 (session producer None fallback) — resolve via SessionAuthority::resolve_channel (#3018)
- src/services/discord/tmux.rs:1180 finish_monitor_auto_turn + 1185 fetch_update — finalizer.submit(Complete/FastTerminal)
- src/services/discord/tmux.rs:2350 watcher_finalized_delegated_turn + 2355 fetch_update — finalizer.submit(Complete)
- src/services/discord/health/recovery.rs:160 orphaned_active_turn_hard_stop — finalizer.submit(ForceCancel)
- src/services/discord/health/recovery.rs:595,659 (idle stale repair / kill stale tmux) — finalizer.submit(ForceCancel/PaneDeath)
- src/services/discord/health/recovery.rs:116,599 clear_inflight_state — REMOVE; finalizer owns inflight clear
- src/services/discord/recovery_engine.rs:766 recovery_abort_with_terminal_stop + 773 fetch_sub — finalizer.submit(ForceCancel)
- src/services/discord/relay_recovery.rs:571 early abort + 574 fetch_update — finalizer.submit(ForceCancel/RelayMiss)
- src/services/discord/placeholder_sweeper.rs:696 abandon + 707 global_active.fetch_sub — finalizer.submit(PlaceholderAbandon) (backstop only)
- src/services/discord/inflight_heartbeat_sweeper.rs:136,145 cancel+clear_inflight_state_if_matches — finalizer.submit(HeartbeatStale)
- src/services/discord/router/intake_gate.rs:593 stale_active_turn_proof_queue_guard / finalize_orphaned_clear — finalizer.submit(ForceCancel{orphan})
- src/services/discord/router/turn_start.rs:398,417 release after placeholder-post-failure / hosted-TUI busy — finalizer.submit(RelayMiss/GateTimeout) (preserve has_pending kickoff)
- src/services/discord/router/message_handler/intake_turn.rs:2312 fetch_add (KEEP, pair with finalizer.submit(Start)); 3213 fetch_sub — REMOVE, replaced by terminal submission
- src/services/discord/router/message_handler/intake_turn.rs:1594 abort stale dispatch guard — route through finalizer query (no direct finish)
- src/services/discord/router/message_handler/headless_turn.rs (global_active) — pair add with Start, route terminal through finalizer
- src/services/discord/commands/control.rs (mailbox_finish_turn / global_active) — finalizer.submit(ForceCancel)
- src/services/discord/stall_recovery.rs (global_active fetch_sub) — finalizer.submit(ForceCancel/HeartbeatStale)
- src/services/discord/health.rs:644,664 fetch_add (KEEP, pair with Start); health/snapshot.rs:654 normalize_global_active_counter — keep as assertion 1 release, then REMOVE
- src/services/discord/health/recovery.rs:523,748 decrement_counter — REMOVE direct decrement, route via finalizer
- src/services/discord/mod.rs:880 fetch_sub(AcqRel) inside mailbox finish wrapper — keep ONLY if invoked exclusively by finalizer; otherwise move decrement to do_finalize step D
- src/services/discord/router/message_handler/watchdog.rs (mailbox_finalize_owed/turn_delivered refs) — drop after Phase 5
- src/services/discord/watchers/lifecycle.rs (mailbox_finalize_owed/turn_delivered) — drop after Phase 5

## Terminal paths (exactly-once guarantee)

- normal-complete: bridge or watcher submits Complete{offsets,relay_owner}. First arrival flips ledger Pending→Finalizing, runs do_finalize once (offset commit + inflight clear + mailbox_finish_turn + counter--), loser gets AlreadyFinalized. No CAS, no turn_delivered handoff.
- user-cancel: cancel finalizer / watchdog / reaction submits Cancel{reason}. do_finalize runs with is_cancel()=true so removed_token.mark_completion_cleanup() is skipped but cancelled.store(true) still fires. If watcher already submitted Complete, Cancel gets AlreadyFinalized (no double finalize).
- force-cancel: health endpoint / restart drain submits ForceCancel. Ledger may have no Pending entry (operator stop of an already-finalized turn) → AlreadyFinalized; or Pending → finalize once. Generation guard prevents force-cancelling a newer turn's token.
- quiescence-gate-timeout: watcher submits GateTimeout{pane_quiescent}. If pane quiescent → finalize immediately. If pane busy AND relay owner alive → Deferred + terminal_deadline=now+GATE_BACKSTOP; the single Tick reconciler finalizes when pane quiesces, owner dies, or deadline elapses (bounded seconds, NOT 1800s). This replaces the silent watcher SKIP.
- relay-miss/suppressed: bridge submits RelayMiss when bridge_output_owner.is_none() && full_response.is_empty(). Finalizes once; emits observability event from inside finalizer.
- direct-input (TUI external adoption): adopting path submits DirectInput; finalizer registers/releases session binding (#3018) and finalizes once if a turn was active.
- fast-terminal (E-18): provider-native completion submits FastTerminal; treated as immediate-finalize terminal event, same exactly-once gate.
- force-cancel/restart (dcserver-restart #3011): restart drain submits ForceCancel per channel with the new generation; stale-generation ledger entries are GC'd, guaranteeing each drained turn finalizes once and no cross-generation token clear.
- pane-death (#3014): watcher (wire A) submits PaneDeath on tmux EOF / no live pane; finalizes once. Heartbeat sweeper (B-wire 60s) and placeholder sweeper (C-wire 1800s) submit HeartbeatStale/PlaceholderAbandon as backstops — both hit AlreadyFinalized in the normal case.
- idle-but-not-finalized: if no actor submits a terminal event (loop exited silently), the Tick reconciler detects a Pending ledger entry past its terminal_deadline (armed at Start with a generous idle ceiling) OR HeartbeatStale arrives at 60s; finalizer runs do_finalize once. This closes the orphaned-inflight leak.
- orphan-after-restart (inflight on disk, no ledger entry): sweeper/intake-gate submit drives do_finalize using the inflight identity directly (TurnKey.user_msg_id from disk); idempotent mailbox_finish_turn returns removed_token=None so counter is untouched, inflight is cleared exactly once.

## Regression test matrix

- bridge × normal-complete: bridge submits Complete, asserts FinalizeOutcome::Finalized + exactly one global_active decrement + inflight cleared (agentdesk-relay-e2e)
- watcher × normal-complete: watcher submits Complete, bridge submits Complete late → bridge receives AlreadyFinalized, finalize ran once (unit, replaces turn_bridge/tests.rs:3632 watcher_consumes_mailbox_finalize_owed)
- bridge+watcher × race: both submit Complete near-simultaneously → ledger gate yields exactly one Finalized, one AlreadyFinalized; global_active decremented once (unit, deterministic via actor message order — replaces CAS race test)
- watcher × gate-timeout pane-busy: submit GateTimeout{Some(false)} with live relay owner → Deferred; advance Tick past GATE_BACKSTOP → finalize fires once (NEW; closes the 1800s permanent-stuck gap)
- watcher × gate-timeout pane-quiesces-before-deadline: GateTimeout{Some(false)} then next Tick reports quiescent → finalize once, no double
- cancel-finalizer × user-cancel after watcher-complete: Cancel arrives after Complete → AlreadyFinalized, mark_completion_cleanup not double-applied (unit)
- recovery × force-cancel of already-finalized turn: ForceCancel on Finalized ledger → AlreadyFinalized, no counter underflow (unit; replaces restored_watcher_finish_does_not_underflow_global_active tmux.rs:6291)
- restart-drain × dcserver-restart: ForceCancel per channel with new generation, stale-gen entries GC'd → each turn finalized once, no cross-gen token clear (agentdesk-relay-e2e session recovery)
- watcher × pane-death + heartbeat-sweeper backstop: PaneDeath finalizes; HeartbeatStale(60s) later → AlreadyFinalized (unit)
- placeholder-sweeper × abandon backstop: PlaceholderAbandon(1800s) on a turn that genuinely never finalized (no actor submitted) → finalizes once; on already-finalized turn → AlreadyFinalized (unit)
- idle-but-not-finalized: Start then silent loop exit, no terminal event → Tick past idle ceiling finalizes once, inflight cleared (NEW regression for the orphaned-inflight leak)
- standby-relay × relay-miss: standby exit timeout submits RelayMiss → finalize once on leader; counter consistent (agentdesk-relay-e2e standby)
- offset-authority × monotonic: OffsetProgress with decreasing offset rejected; Complete commits max offset only (unit #3017)
- session-authority × resolve fallback: Start registers binding, resolve_channel hits registry not inflight; after release resolve falls to inflight last-resort (unit #3018)
- counter-invariant: property test — N Start submissions paired with N terminal submissions (any mix of paths) leaves global_active == 0, normalize_global_active_counter never clamps (unit #3019)
- direct-input × adoption: DirectInput on active turn finalizes once and releases session binding (agentdesk-relay-e2e tmux wrapper / external input)

## Risks & mitigations

- Actor serialization latency: routing all finalize through one mpsc task could add latency or, if the task stalls, block ALL finalization. Mitigation: keep do_finalize's only await on the already-fast mailbox actor; add a Tick watchdog that logs if the actor's queue depth or oldest-Pending age exceeds a threshold; the actor must never hold a lock across the mailbox await.
- Ordering inversion vs today's Release/AcqRel guarantees: the CAS handoff encoded a real ordering (finalize-debt visible before unpause). Replacing it with message arrival order means Start MUST be submitted before the watcher can submit Complete. Mitigation: bridge submits Start synchronously before unpausing the watcher (same site as the deleted mailbox_finalize_owed.store at 3634); add a debug assertion that Terminal for an unknown TurnKey is treated as orphan-path, not dropped.
- TurnKey identity ambiguity: recovery/orphan paths often have user_msg_id==0 or stale generation, risking a mismatched ledger entry or a missed AlreadyFinalized. Mitigation: channel_id+generation is the primary match; user_msg_id==0 falls back to channel-scoped single-active-turn assumption (matches today's mailbox semantics); generation guard rejects cross-restart matches.
- Incremental window with two authorities: during Phases 2-4 some sites submit to the finalizer while others still call mailbox_finish_turn directly, risking a double finalize. Mitigation: do_finalize relies on mailbox_finish_turn idempotence (removed_token=None on second call) and gates the counter decrement on removed_token.is_some(), so a transitional double-call is safe (counter touched once); land watcher and bridge in the same release to minimize the window.
- GATE_BACKSTOP tuning: too short re-introduces the #2293/#2780 inject-into-busy-pane corruption; too long re-creates a stuck turn. Mitigation: keep the hosted-TUI pre-submit guard as the correctness firewall (finalize releases the mailbox but the pre-submit gate still requeues if the pane is busy), and make GATE_BACKSTOP a tuned constant with telemetry on how often the deadline (vs quiescence) triggers finalize.
- normalize_global_active_counter removal: deleting the clamp before the counter is provably balanced could surface a real wrap as a user-visible stuck 'busy' state. Mitigation: keep it for one release emitting a metric whenever it would clamp; only delete after the counter-invariant property test and production telemetry show zero clamps.
- Hidden side-effects at old call-sites (voice barge-in drain, dispatch_thread_parents retain, watchdog override clear) currently trail mailbox_finish_turn inline; moving them into do_finalize risks ordering differences. Mitigation: move them verbatim into do_finalize step F and add a test asserting voice-deferred drain still runs exactly once per finalize.