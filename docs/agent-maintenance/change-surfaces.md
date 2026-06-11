# Change Surfaces

> Source: [`docs/agent-maintenance/index.md`](index.md). For every "where do I
> add this?" question, consult this page first. The giant-file list is the
> auto-generated inventory in
> [`docs/generated/module-inventory.md`](../generated/module-inventory.md) and
> the owner/deadline registry in
> [`docs/generated/giant-file-registry.md`](../generated/giant-file-registry.md);
> the rows below project the operational meaning of each entry.
>
> Last refreshed: 2026-06-11 (against #3306 idle-relay registry/dedupe-mirror drift self-heal — new non-giant `src/services/discord/idle_relay_drift.rs` rate-limits the per-session drift WARN and one-shot self-heals the authoritative registry from a DURABLE source — the settings binding or the `sessions.channel_id` durable column via the new `load_session_channel_id_pg` — guarded by live-pane + instance-id + mirror-agreement so #3018's single-authority / never-route-from-mirror invariant holds; `tui_prompt_relay.rs` net 0-line at the 5429 freeze, `mod.rs` +1 to the 4987 freeze, `db/dispatched_sessions.rs` +48; on top of #3303 deferred-claim anchor reconcile — the `tui_direct_abort_marker` module is decomposed into a directory module of `mod.rs` + `store.rs` + `deferred_claim.rs`, all non-giant, and a successful watcher-owned deferred claim now records an own-identity `DeferredClaim` marker so the anchor's `⏳` converges to `✅` on the own commit or a bounded sweep `⚠`; tmux_watcher / tui_prompt_relay / health recovery / placeholder_sweeper are 0-line changes; on top of #3216 resume-protection residual gaps on top of #3207 — session_runtime now adds a SAFE legacy NULL-channel_id fallback to the channel-scoped cwd resolves via resolve_cwd_for_session_key honored only for a single legacy row, plus a live-tmux recovery-cwd reconcile in auto_restore_session_force via reconcile_recovery_cwd and correct_session_cwd_to_tmux that adopts the live pane cwd over a divergent DB cwd and self-heals the row; #3207 comprehensive channel_id scoping of session-cwd DB resolves — auto_restore_session_force restart restore via restore_session_cwd_from_db and watcher recovery via load_restored_session_cwd require channel_id = $2; on top of #3105 dead/orphaned TUI-session mirror eviction made flake-resistant and run off the Tokio executor).

## Read This First

- "giant-file" = `>= 1000` **production** lines per
  `scripts/generate_inventory_docs.py` (lines inside `#[cfg(test)] mod` blocks
  are excluded; see the `Prod` column in `module-inventory.md`). New logic added
  to a giant file inherits the file's review surface — every reviewer must
  re-read the entire module — so adding to it without an extraction plan is
  rejected. A module whose production surface falls below the threshold is no
  longer frozen and must be removed from the lists below.
- `do_not_edit_without_migration_plan` columns below mean: even though the
  file builds and runs, the scheduled migration owner will roll back ad-hoc
  additions. If you must change behaviour there, scope it to a single bugfix
  AND link the migration issue in the PR description.
- `active_callsite_coverage` only applies to surfaces with a parallel canonical
  path already implemented (e.g. Discord outbound v3). For pre-migration giant
  files (no canonical replacement yet), the column is `n/a`.

## Surface Map (by feature)

### `discord_outbound`

- canonical_modules: `src/services/discord/outbound/{message,policy,result,decision,delivery,transport}.rs`
  (#1006 v3 domain types, pure planner, delivery implementation, and shared
  transport/dedup primitives).
- legacy_modules: none; `src/services/discord/outbound/legacy.rs` was removed
  in #2535.
- do_not_edit_without_migration_plan:
  - `src/services/discord/formatting.rs::send_long_message_raw` (line 1971,
    ordered-chunk continuation contract not yet modelled in v3).
  - `src/services/message_outbox.rs` is the PG-backed message outbox
    enqueue/claim/accounting surface (now below the giant-file threshold once
    `#[cfg(test)] mod` blocks are excluded; bugfix only until split).
- active_callsite_coverage: see
  [`discord-outbound-migration.md`](discord-outbound-migration.md) (table is
  the authoritative coverage record).
- invariants: every new `send` or `edit` from production code goes through
  `outbound::delivery::deliver_outbound` — never `channel_id.say`,
  `channel_id.send_message`, or raw `http.send_message` from a route or
  worker. Interaction-token responses (`ctx.say`, `ComponentInteraction`) are
  the only allowed exception per #1175.
- allowed_changes: `new_feature` only on the v3 `outbound/` submodules;
  `extraction` from `formatting.rs` requires a contract update for ordered
  chunk metadata.
- tests: per-module unit tests in
  `outbound/{message,policy,decision,result}.rs`.
- related_issues: #1006, #1175, #1280.

### `policy_engine`

- canonical_modules: `src/engine/mod.rs` (driver) plus `src/engine/ops/*.rs`
  (per-domain op handlers). `src/pipeline.rs` (1314 lines, giant-file)
  composes the policy pipeline.
- legacy_modules: none — there is no parallel engine. The whole surface is
  pre-migration giant-file territory.
- do_not_edit_without_migration_plan:
  - `src/engine/mod.rs` (1265 lines, giant-file).
  - `src/engine/ops/db_ops.rs` (1111 lines, giant-file).
  - `src/engine/loader.rs` (1332 lines, giant-file) — engine loader / QuickJS
    validator surface; split before adding non-bugfix behavior.
  - `src/pipeline.rs` (1314 lines, giant-file).
- non-giant migration-sensitive note: `src/engine/intent.rs` is below the
  giant-file threshold but remains a migration-sensitive intent surface; keep
  changes scoped to the typed-facade contract.
- active_callsite_coverage: n/a (no canonical replacement yet).
- invariants: typed-facade contract from
  [`docs/policy-typed-facade.md`](../policy-typed-facade.md); engine never
  mutates DB rows directly outside `engine::ops::db_ops`.
- allowed_changes: `bugfix` only. Any non-bugfix needs an extraction issue
  filed first (no current owner — file under "policy-engine refactor" follow-up
  per #1279).
- tests: see `engine/` per-module `#[cfg(test)] mod tests` and
  `src/high_risk_recovery.rs`.
- related_issues: #735 (`docs/policy-tick-bottleneck-735.md`).

### `dispatch`

- canonical_modules: `src/dispatch/{mod,dispatch_context,dispatch_create,dispatch_status}.rs`.
- legacy_modules: none.
- do_not_edit_without_migration_plan (giant-file, awaiting split issue):
  - `src/dispatch/dispatch_context.rs` (2805 lines).
  - `src/dispatch/dispatch_create.rs` (1381 lines).
  - `src/dispatch/dispatch_status.rs` (1493 lines).
  - `src/services/dispatches/outbox_route.rs` (1089 lines; route extraction
    orchestration surface from #1722, split before adding non-bugfix behavior).
  - `src/services/dispatches/discord_delivery/orchestration.rs` (1490 lines;
    delivery orchestration surface extracted from the route layer in #1760,
    split before adding non-bugfix behavior).
- active_callsite_coverage: n/a.
- invariants: dispatch creation is the only writer for `dispatched_sessions`;
  status transitions go through `dispatch_status`.
- allowed_changes: `bugfix` only.
- tests: `src/integration_tests/dispatch_flow/*` and per-module unit tests.
- related_issues: #1784, #1785, #1808-#1814.

### `tmux_watcher`

- canonical_modules: `src/services/discord/watchers/lifecycle.rs` (watcher
  stop/reattach/claim/restore lifecycle, including the #1222 single-owner
  claim path and #1283 cancel-induced reattach contract),
  `src/services/discord/tmux.rs` (watcher loop and remaining tmux relay
  parsing), `src/services/discord/inflight.rs` (state file contract).
- legacy_modules: none — relay routes are being consolidated, not replaced.
- do_not_edit_without_migration_plan (giant-file):
  - `src/services/discord/watchers/lifecycle.rs` (2330 lines — canonical
    lifecycle extraction surface from #1435; split further before adding new
    lifecycle behavior; #3016 phase-5b2 dropped the `mailbox_finalize_owed`
    construction from the watcher-spawn handle).
  - `src/services/discord/tmux.rs` (2011 lines after #2558 dead-code sweep;
    +4 from #3167: the monitor-auto-turn start passes `ActiveTurnKind::Background`
    so a queued user message can supersede the low-priority monitor/loop turn;
    failover guard; #3087 `session_panel_instance_key`/`write_spawn_nonce`
    re-exports; #3107 `RestoredWatcherTurn.injected_prompt_message_id`;
    #3016 option A `normal_completion` finalize-decouple param;
    #3017 monitor-auto-turn finalizer routing + ledger-generation +
    relay-watermark reset re-exports; +10 from #3041 P1-1 making
    `advance_watcher_confirmed_end` `pub(in crate::services::discord)` +
    doc-comment — the watcher commits the delivery lease and advances this
    monotonic-CAS offset INLINE (synchronously) on a `Delivered` outcome; the
    finalizer actor's `CommitDelivery`/`ReleaseDelivery` handlers are DORMANT
    (retained for a later phase, not the live watcher path after the R2 revert);
    still giant-file territory).
  - `src/services/discord/tmux_watcher.rs` (8122 production lines after
    #3038 tmux_watcher S1 moved top-level decision clusters A/B/C/E/F/I/J/K
    into `tmux_watcher/` child modules: `liveness.rs` (301),
    `panel_decisions.rs` (372), `prompt_observe.rs` (109),
    `turn_identity.rs` (327), `completion_gate.rs` (275), and
    `commit_decisions.rs` (140). #3016 phase-5b2 removed the `mailbox_finalize_owed`
    swap reads, the watcher-fn flag params, and the `LegacyFlagGated`
    decision variant; #1520 watcher loop extraction + #2427 D/A
    explicit-cleanup wires + #3055 watcher session-panel lifecycle
    refresh + #3087 session-instance-key panel reset + #3095 durable
    provider-selector fallback to the in-memory cache on resume turns
    + #3099 task-notification anchor `⏳` cleanup for `user_msg_id == 0`
    external-input turns (+9 from the #3099 re-review pinned-injected-message-id
    cleanup target) + #3107 self-heal: throttled live-pane-busy probe gates the
    inflight-missing suppressions, re-acquires a watcher-owned inflight, and
    preserves the panel under an active turn; +16 from #3077 routing the
    TUI-direct status-panel publish + orphan-cleanup writes through the typed
    `inflight::bind_status_panel` / `clear_status_panel_if_current` ownership ops;
    +124 from #3077 codex P1 honoring the `bind_status_panel` return at the
    TUI-direct publish site (delete the just-sent panel + disown the handle when
    the bind did not record it, instead of leaking a duplicate);
    +50 from #3104 terminal/idle reconciliation pass that strips a lingering
    `계속 처리 중` streaming footer off the committed-but-unrelayed placeholder;
    +46 from #3016 option A codex R2 `pinned_finalize_user_msg_id` pure helper
    that binds the watcher normal-completion finalize id to the OUTPUT RANGE
    (`turn_start_offset.unwrap_or(last_offset) < current_offset`, mirroring the
    yield guard at `tmux.rs:2110-2111`) so a follow-up turn started after this
    range is not released by stale output;
    +143 from #3017 the no-inflight relay-dedup gate (reads `committed_relay_offset`
    + generation-aware watermark resets, suppresses a wake/idle terminal already
    committed by another relay actor) and the monitor-auto-turn synthetic-id /
    ledger-generation threading through `finish_monitor_auto_turn_if_claimed`;
    +185 from #3041 P1-1 wiring the WATCHER terminal delivery through the live
    `DeliveryLeaseCell`: a per-spawn `instance_id`, the B3 acquire-deadline
    constant + rationale, the pre-send `try_acquire` on the turn-pinned identity,
    the B2 single-holder skip arm (a replacement watcher must not re-emit a held
    range), and committing the lease + advancing `advance_watcher_confirmed_end`
    for the watcher terminal path INLINE (synchronously; the actor
    `commit_delivery`/`release_delivery` round-trip was reverted in R2 — those
    actor handlers are DORMANT, retained for a later phase); plus the R2 Issue-1
    heartbeat: a `DeliveryLeaseHeartbeat` background task `renew()`s the lease
    every 5s while the send future is in flight (deadline cut to 15s for fast
    dead-holder recovery), stopped before the inline commit so a long multi-chunk
    send is never reclaimed mid-flight;
    +164 from #3041 P1-3 Part b: REPLACING the 10s blind terminal re-send with the
    §3.2 committed-offset reconciliation — `watcher_terminal_resend_action`
    (skip-already-committed / send-suffix / send-full against
    `committed_relay_offset`), a dedicated `SkipAlreadyCommitted` relay arm that
    treats an already-committed range as a completed delegated delivery (no
    duplicate, no placeholder double-handling), and the suffix-trim wiring; the ACK
    polling itself is preserved;
    +75 from #3041 P1-3 Part a (frame-carried B1 commit fence): the RESULT-bearing
    `StreamFrame` now carries `terminal_consumed_end` + the pinned turn identity
    (`watcher_terminal_commit_fence`; deferred forward at both read sites so the
    terminal frame is detected post-`process_watcher_lines` and rides the commit
    data), the sink advances `confirmed_end_offset` identity-gated on its CONFIRMED
    POST (`advance_offset_for_confirmed_delegated_terminal`), and the RACY
    inflight-persist Part a (`session_bound_delegated_terminal_end`) is REMOVED;
    +21 from #3041 P1-3 codex review (PR #3150) fixes: issue-1 multi-turn-chunk
    split (`split_decoded_chunk_at_terminal_boundary` +
    `forward_terminal_chunk_with_trailing_to_supervisor_relay` — the TERMINAL frame
    carries ONLY the just-completed turn's bytes, a trailing later-turn tail rides a
    separate non-terminal frame so it is never black-holed), and the
    `WatcherTerminalResendAction::SendFull` slow-sink-in-flight deferral doc (#3151);
    +R4 (PR #3150) codex P1-3 R4: STRICT frame `turn_start_offset` identity gate
    (no weak `is_none_or` None fallback) with the producer guarantee that
    `watcher_terminal_commit_fence` only emits a fence when the turn's
    `turn_start_offset` is known (else a non-terminal frame + watcher SendFull),
    plus the issue-1 ACK-correlation close: a fence-less frame reports
    `FrameAccepted` (never a terminal outcome) so turn B's tail post can never mask
    turn A's terminal-ACK (multi-RESULT-per-chunk per-turn fence deferred to #3151,
    no black-hole);
    +R6 (PR #3150) codex P1-3 R6: TURN-SCOPE the carried session-bound
    `ack_target` (`SessionBoundRelayAckTarget` now stamps the terminal frame's
    pinned `turn_start_offset`; `carry_session_bound_ack_for_turn` resets a stored
    ack to `None` on a turn boundary instead of the legacy "store only when Some").
    A single chunk holding `result(A)+result(B)` where B completes inside the split
    tail (its frame sequence discarded) no longer lets B inherit A's stale ack: B's
    pass sees a different pinned turn identity → ack reset to `None` → B reconciles
    against `committed_relay_offset` (None → MissingTarget → §3.2 SendFull/Skip),
    NEVER black-holed even when A reported Delivered;
    +59 from R7 (PR #3150) codex P1-3 R7: TURN-BOUNDARY ack reset at the split.
    R6's `carry_session_bound_ack_for_turn` STILL black-holes a later turn when
    `turn_identity_for_panel` is NOT refreshed (B's inflight not yet established when
    B's leftover bytes are processed → the pinned offset is STILL A's → the carry
    helper KEEPS A's ack). `SupervisorRelayForward` now carries a `trailing_turn_follows`
    signal set by `forward_terminal_chunk_with_trailing_to_supervisor_relay` whenever it
    splits a result-bearing chunk with a non-empty trailing tail (a later turn follows).
    A pass-scoped `split_trailing_turn_follows` latch ORs that over both forward sites;
    AFTER this turn waits on (consumes) its own terminal ACK — right after the relay
    flight-recorder log — the watcher resets `all_data_session_bound_relay_ack` to `None`.
    So a later turn ALWAYS starts with no inherited ack → MissingTarget → §3.2 reconcile
    (SendFull/Skip) → never black-holed, independent of whether the pinned identity
    refreshed. A's own delivery still resolves on A's ack (the reset is post-wait);
    split loop helpers further before adding behavior;
    +24 from #3041 P1-4 codex: the watcher post-delivery external-input lease clear
    now snapshots the lease GENERATION before the awaited relay
    (`external_input_lease_generation_before_relay`) and clears via
    `clear_external_input_relay_lease_if_generation_matches` instead of the
    unconditional by-key `clear_external_input_relay_lease`, closing the
    stale-snapshot clobber where a turn-2 same-key lease recorded during turn-1's
    in-flight send was wrongly removed by turn-1's success clear);
    +3 from #3041 P1-4 codex R3: the snapshot now reads the lease ONCE
    (`external_input_relay_lease(...)` under a SINGLE STATE lock) and derives BOTH
    the presence bool and the generation from that one atomic read, closing the
    present/generation TOCTOU where two separate accessor calls re-locked STATE and
    a concurrently-started turn could slip a newer same-key lease into the gap);
    +60 from #3041 P1-5 (FINAL phase): unify the terminal delivery outcome into the
    cross-actor 3-way `DeliveryOutcome { Delivered, NotDelivered, Unknown }`. The
    watcher's `SessionBoundRelayAckOutcome::TerminalSkipped` is renamed `NotDelivered`
    (folds ring `DeliveryOutcome::NotDelivered`), a new `RingUnknown` arm folds the
    explicit ring `Unknown`, and the failure/unconfirmed arms
    (`RingUnknown`/`Dropped`/`SinkError`/`TimedOut`/`MissingTarget`) collapse to
    `DeliveryOutcome::Unknown` via the new pure `session_bound_ack_delivery_outcome`
    fold. `watcher_should_direct_send_after_session_bound_ack` now decides on that
    3-way (`!= Delivered`) instead of the implicit `ack_outcome != Delivered` bit.
    §3.2 SAFETY INVARIANT: BOTH `NotDelivered` AND every `Unknown`-class arm still
    flow through `watcher_terminal_resend_action` (committed-offset reconciliation:
    `committed >= end` → SkipAlreadyCommitted, else SendFull) — NO blind-skip for
    NotDelivered, NO blind 10s re-send for Unknown; the should-direct-send bool stays
    the precondition gate, the send paths stay masked by `!SkipAlreadyCommitted`. New
    tests `unknown_outcome_triggers_committed_offset_reconciliation_not_blind_resend`
    and `not_delivered_outcome_keeps_no_resend_when_foreign_owner_committed`);
    +18 from #3041 P1-5 (codex P1 follow-up) routing the ownerless `TimedOut`
    through §3.2: the pre-existing #3042 band-aid (`if !relay_owner_present &&
    TimedOut { return false }` early-return in
    `watcher_should_direct_send_after_session_bound_ack`) blanket-suppressed an
    ownerless TimedOut BEFORE it could reach `watcher_terminal_resend_action`
    (committed-offset reconciliation) — neither reconciling nor resending → a
    potential black-hole when committed < end. P1-3 Part (a)
    (`advance_offset_for_confirmed_delegated_terminal`) made the committed offset
    authoritative on a CONFIRMED post (the same fence that arms the ACK target →
    TimedOut), so the band-aid is obsolete: the early-return is removed and an
    ownerless TimedOut now flows through the SAME §3.2 path as every other
    non-Delivered outcome (committed >= end → SkipAlreadyCommitted → the #3042 3×
    duplicate is prevented PRINCIPALLY; committed < end → SendFull → black-hole
    closed). The §3.2 universality invariant now covers EVERY non-Delivered arm with
    no exception. New/updated tests `ownerless_timed_out_intends_resend_via_gate`
    (renamed from `ownerless_timeout_suppresses_watcher_direct_fallback`),
    `ownerless_timed_out_reconciles_skip_when_committed_reaches_end` (#3042
    regression guard), `ownerless_timed_out_reconciles_full_when_not_committed`.
    +93 from #3142 (EPIC, follow-up to #3141): TURN-ALIASING SAFETY for the
    remaining committed-output consumers that the #3141 finalize/clear/reaction
    gate did not cover. A new pure sibling helper
    `committed_anchor_cleanup_is_stale_for_newer_turn` (the id==0-INCLUSIVE variant
    of #3141's `committed_completion_is_stale_for_newer_turn`, mirroring the same
    `turn_start_offset.unwrap_or(last_offset) >= current_offset` yield-guard offset
    test but requiring anchor-relevance — `user_msg_id != 0 ||
    injected_prompt_message_id.is_some() || external_input`) gates the two
    anchor-cleanup branches (the `should_complete_tui_direct_anchor_lifecycle`
    first branch — also the `lifecycle_stage_paused`-with-inflight path — and the
    `injected_prompt_message_id` task-notification branch) so an id==0
    external-input/injected newer turn's anchor is never `✅`'d mid-flight. The
    id!=0 `completion_is_stale_for_newer_turn` now ALSO gates dispatch finalization
    (the `else if let Some(did) = resolved_did.as_deref().filter(|_| !stale)` arm
    falls through to the no-finalize `else => true` so a newer dispatch is not
    completed with the older `full_response`) and the TUI history push (the newer
    turn's `user_text` is never cross-paired with the older response). The
    status-panel completion identity is offset-pinned via `pinned_finalize_user_msg_id`
    (None for a newer pre-relay snapshot) so the panel binding agrees by
    construction with the reaction/transcript/analytics gate. Matrix tests
    `committed_anchor_cleanup_stale_for_newer_turn_matrix` (+ per-consumer
    `dispatch_finalization_skips_when_stale`, `history_append_skips_when_stale`,
    `status_panel_id_none_when_pre_relay_snapshot_is_newer`,
    `anchor_cleanup_skips_when_stale_id0`, `paused_first_branch_anchor_gate`).
    +54 from the #3142 codex re-review (residual status-panel aliasing gap): the
    completion IDENTITY was offset-pinned but the status-panel ADOPT site
    (`should_adopt_inflight_terminal_message_ids` → `status_message_id`) and the
    EDIT/finalize site (`complete_watcher_status_panel_v2` + the external-input
    orphan-store reconciliation) still acted on a stale NEWER pre-relay snapshot,
    so the older committed range could pull a newer turn's panel id and EDIT it.
    Both sites now gate on
    `!committed_anchor_cleanup_is_stale_for_newer_turn(inflight_before_relay, None,
    session, current_offset)` (the id==0-INCLUSIVE anchor variant catches a newer
    id==0 external-input/injected panel owner the id!=0 sibling would miss; the
    OFFSET test — not `pinned == 0` — keeps an in-range id==0 watcher-direct turn
    NON-suppressed). turn_bridge/mod.rs:2009 (BRIDGE path / #3016 core hotfile) is
    explicitly OUT OF SCOPE and deferred to a follow-up. New test
    `status_panel_adopt_and_edit_gate_is_turn_aliasing_safe` covers stale-newer
    (incl. id==0 external/injected) NOT adopted/edited, in-range id==0
    watcher-direct STILL adopts+edits (over-suppression guard), and in-range id!=0
    unchanged.
    +22 from #3169 P1: `mark_watcher_terminal_delivery_committed` now lets a
    self-paced loop turn (`user_msg_id == 0`) set `terminal_delivery_committed` on a
    fully-anchored completion (the original `user_msg_id != 0` requirement skipped
    every loop turn, so the #3126 stall-watchdog guard had no architectural
    finished-delivery signal → death #1 false-positive force-clean). NOT a blanket
    relaxation: a loop turn is admitted only when its frame-carried
    `turn_start_offset` is known AND matches the loaded inflight (loop turns are
    disambiguated by `started_at` + `turn_start_offset` per #3041 P1-3, since the
    1-second `started_at` can collide across two consecutive self-triggered turns)
    so a late completion can never commit the WRONG newer loop turn; the
    `user_msg_id != 0` path is byte-for-byte unchanged. New test
    `watcher_terminal_delivery_commit_marks_loop_turn_with_zero_user_msg_id`.
    +273 from #3016 S3 (the A2 / phase-5 enabler): REWRITE the watcher fresh-idle
    finalize decision to consult the S1 STRUCTURAL completion signal
    (`TurnFinalizer::completion_signal_state` → `CompletionSignal{Done,PausedLive,
    Unknown}`) instead of the in-memory `mailbox_finalize_owed` flag as the sole
    finalize signal. A new pure helper `watcher_fresh_idle_finalize_decision`
    (→ `FreshIdleFinalizeDecision{DeferPausedLive,AbortFollowupTookOver,SkipStale,
    Finalize,LegacyFlagGated}`) fuses the signal with the #3197 A2 wrong-turn-race
    defenses (pinned pre-cleanup `pinned_finalize_user_msg_id` + stale-skip via
    `committed_completion_is_stale_for_newer_turn` + the pause/epoch guard, all
    evaluated BEFORE the destructive clear because this branch `continue`s before
    the canonical guard at tmux.rs runs). Done (structural terminator proven, even
    when the response is EMPTY) → finalize via
    `finish_restored_watcher_active_turn(.., normal_completion=true, ..)` with the
    pinned current-turn id; PausedLive (no terminator — paused at selector /
    permission prompt / subagent running / long silent tool) → DEFER, never
    finalize; Unknown (non-JSONL runtime: LegacyTmuxWrapper/ProcessBackend/
    ClaudeEAdapter) → KEEP the legacy `mailbox_finalize_owed` flag path VERBATIM.
    The DEFER decision now keys on the STRUCTURAL TERMINATOR (not on response
    emptiness), which is the fix for the contradiction that killed the first A2
    attempt (deferring `delegated && empty` made the empty-but-done completion's
    finalize unreachable). The flag is NOT deleted (stage 5); it stays read for the
    Unknown arm and is still `swap(false)`'d to keep its revoke lifecycle. New tests
    `fresh_idle_paused_live_defers_via_completion_signal`,
    `fresh_idle_done_finalizes_and_unknown_falls_through_to_legacy`,
    `fresh_idle_done_wrong_turn_race_does_not_finalize_followup`, and the
    end-to-end `fresh_idle_empty_terminated_completion_finalizes_via_completion_signal_flag_false`
    (drives the REAL completion signal over a real JSONL transcript + the REAL
    finalizer actor, NOT a re-implementation).
    +36 from #3016 S3 gate-fix iteration (3 adversarial-gate concerns): (1) the
    `Done` decision now reads the STRICTER turn-END-only terminator
    (`jsonl_turn_end_terminator_idle`, accepts ONLY Codex `turn.completed` /
    Claude `result`+`system{turn_duration|stop_hook_summary}`) so a completed
    Codex `agent_message` / Claude mid-turn message cannot over-finalize a LIVE
    turn; (2) the Done-arm destructive `clear_inflight_state` is now gated by
    `committed_completion_is_stale_for_newer_turn` with BOTH the pinned snapshot
    AND a LATE on-disk re-read (closing the TOCTOU where a follow-up turn saved
    inflight during the cleanup awaits), mirroring the canonical clear at
    tmux.rs; (3) the ignored `turn_start_offset` param was REMOVED from
    `completion_signal_state` (range-independence is documented: the turn-END
    scan is offset-independent and turn-correctness comes from the pinned-id +
    stale-skip). New test `fresh_idle_clear_gate_skips_when_late_reread_is_newer_turn`.
    +25 from #3016 S3 FINAL fix (residual TOCTOU on the Done-arm clear): the
    gate-fix iteration above still split the clear across TWO locks (late re-read +
    `committed_completion_is_stale_for_newer_turn` check under one lock, then the
    UNCONDITIONAL `clear_inflight_state` under another) — on a multi-threaded tokio
    runtime a follow-up turn could save a NEW inflight between the re-read and the
    clear, so the unconditional delete wiped it (check-then-act not atomic). The
    Done arm now performs the on-disk clear with the EXISTING atomic
    compare-and-clear helper `clear_inflight_state_if_matches_identity`
    (inflight.rs: read+validate+unlink under a SINGLE sidecar lock), keyed on the
    PINNED turn's `InflightTurnIdentity::from_state` (the same snapshot the decision
    helper derived `user_msg_id` from). It deletes ONLY if the on-disk identity is
    STILL the pinned turn; a follow-up's different identity → `UserMsgMismatch`
    no-op → its inflight survives. The window is closed atomically (no separate
    re-read). The finalize-skip stays a SEPARATE pinned-snapshot decision in
    `watcher_fresh_idle_finalize_decision`; only the destructive CLEAR moved to the
    atomic helper. The CANONICAL normal-completion clear in the
    `tmux_watcher.rs` root loop still uses the weaker non-atomic re-read+clear
    pattern — left UNCHANGED (out of
    scope; the S3 arm is now strictly safer). Test
    `fresh_idle_clear_gate_skips_when_late_reread_is_newer_turn` rewritten to drive
    the REAL atomic helper against REAL on-disk inflight: a follow-up's inflight on
    disk → atomic clear is a no-op (follow-up preserved); the pinned turn on disk →
    atomic clear removes it (happy path).
    -24 from #3016 phase-5b1: REPLACE the watcher fresh-idle `Unknown` (non-JSONL
    runtime) `mailbox_finalize_owed`-flag CONSUMER with a flag-independent decision.
    +47 from #3016 phase-5b1 codex HIGH fix: re-key the `Unknown` routing on response
    EMPTINESS (NOT the flag, NOT unconditionally), restoring the OLD (pre-5b1) defer
    behaviour flag-independently. `watcher_fresh_idle_finalize_decision` now routes a
    NON-empty `Unknown` to the `Finalize` arm (prompt, flag-independent — the intended
    5b1 improvement: the fresh-idle gate already PROVES pane idle via
    `watcher_session_ready_for_input` — the SAME `pane_ready_fallback_allowed &&
    tmux_session_ready_for_input` predicate the 5a far-backstop uses for `Unknown`),
    but an EMPTY `Unknown` → new `DeferEmptyUnknown` (preserve inflight). Rationale:
    non-JSONL runtimes (Gemini / OpenCode / Qwen / LegacyTmuxWrapper) have NO
    structured `PausedLive` signal, so a turn awaiting a selector / permission /
    interactive prompt can look pane-idle with empty output; finalizing it here would
    kill the turn mid-work. Deferring on emptiness reconstructs the OLD
    `delegated_finalize_owed && empty → defer` gate without the flag (`owed` was
    ~always true for a delegated `Unknown`), and the 5a 1800s far-backstop remains its
    finalizer. The defer gate in the `tmux_watcher.rs` root loop likewise defers `PausedLive` and
    EMPTY `Unknown`; `Done` (JSONL terminator) finalizes even when empty; the
    paused/epoch abort + stale-for-newer-turn skip race guards are kept exactly on
    both finalize arms. The now-unreachable `LegacyFlagGated` exec arm is a defensive
    preserve-inflight no-op (the `mailbox_finalize_owed` field/producers are removed in
    phase-5b2). Tests `fresh_idle_done_finalizes_and_unknown_routes_by_emptiness`
    (Done finalizes even when empty; non-empty `Unknown` finalizes promptly,
    flag-independent; EMPTY `Unknown` DEFERS — the codex HIGH regression case that the
    prior 5b1 build finalized prematurely) +
    `fresh_idle_unknown_keeps_wrong_turn_race_guards`.
    #3262 (-16): the watcher-completed status-panel context-usage backfill block
    is extracted into `adk_session::backfill_completed_panel_usage_and_maybe_inject_compact`
    (a single call now replaces the inline `set_context_panel_usage` block). The
    helper additionally fires the claude-only AgentDesk-side `/compact` injection
    when live usage crosses `context_compact_percent_claude` (Claude Code ignores
    `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`); see `src/services/claude_compact_trigger.rs`.
    +19 from #3296: the aborted-anchor reconcile chokepoint — on a body-visible
    normal commit (`terminal_output_committed &&
    tui_direct_anchor_terminal_body_visible && !lifecycle_stage_paused`, sited
    BEFORE `clear_inflight_state` since codex r2) the watcher durably records a
    `tui_direct_abort_marker::record_commit_tombstone` (committed turn
    identity — written FIRST, so any reconciler that observes "no live row"
    already sees the commit evidence) and then calls
    `tui_direct_abort_marker::drain_on_terminal_commit` with the COMMITTED
    turn's identity (codex r1: positive correlation — only the turn identity
    the marker pinned may cover: the foreign prior inflight for ABORT markers,
    the worker's OWN synthetic turn for #3303 `DeferredClaim` markers), so an
    anchor whose synthetic turn-start ABORTed (input already
    provider-submitted, `⏳` kept) or whose deferred claim succeeded but never
    saw its own commit pass flips `⏳ → ✅` when the pinned turn commits; the
    marker logic itself lives in the non-giant `tui_direct_abort_marker/`
    directory module (#3303 decomposition: `mod.rs` reconcilers + Abort
    disposition, `store.rs` durable marker/tombstone I/O, `deferred_claim.rs`
    own-identity pin + uncapped-while-pinned disposition; sweeper TTL/hard-cap
    `⚠` fallback + commit-tombstone 대조 semantics unchanged); the #3296
    additions are offset in-file by compressing the #3016-S3 finalize/TOCTOU
    and #1670/#1708 decoupling comment blocks. #3038 tmux_watcher S1 then lowered
    the ratchet baseline to 8122 by moving pure decision clusters into the
    capped `tmux_watcher/` child modules.
  - `src/services/discord/tui_prompt_relay.rs` (5429 production lines; #3296
    codex r1+r2: the ABORT cleanup hook pins the foreign prior inflight's
    identity — the live row at the record instant, or the worker's LAST-VIEW
    identity when that row just vanished — and persists the marker via
    `record_for_abort`, whose commit-tombstone 대조 (never bare row-absence:
    codex r2 removed the unfounded pre-covered promotion) decides whether it
    starts covered; #3016 phase-5b2
    removed the dead `publish_tui_direct_watcher_finalize_debt` producer;
    SSH-direct TUI
    prompt notification plus Codex rollout response relay surface, bugfix only
    outside an extraction plan; +4 from #3167: the self-paced TUI loop relay
    starts its synthetic turn with `ActiveTurnKind::Background` so a queued user
    message can supersede it; +54 from #3189: the `/loop` control note carries
    its directive body via `extract_loop_body` (operator wants the recurring loop
    content visible; only the #3153 double-post is deduped, never the content) —
    the `<command-args>` block (closing tag REQUIRED via `split_once`, so an
    unterminated wrapper falls back instead of spilling the appended skill body)
    or raw-echo args, never the trailing skill markdown;
    every OTHER machine command (`/compact`, `Compacted …` stdout) stays
    kind-only; +185 from #3178: the machine slash-command
    control trigger (`/loop`/`/compact`/`<command-*>`) is a FULL active turn —
    it claims the mailbox active turn (so a message injected mid-`/loop` queues
    cleanly via `mailbox_try_start_turn`) and gets an anchor
    (`format_slash_command_control_note` + `slash_command_control_kind`) + ⏳ +
    synthetic inflight + ✅. The #3153 near-simultaneous
    duplicate half (raw echo + expanded wrapper of the same injection) is dropped
    by the 2s dedupe gate (`slash_command_control_turn_is_first_sighting`) BEFORE
    any external-input lease is recorded, so it can never overwrite the first
    turn's lease; the kind is the real command name so distinct commands are not
    collapsed; +64 from #3176: identity-pinned idle-tail
    drain-wait (`inflight_is_current_turn_synthetic` + `current_turn_anchor_id`
    threading) closes a relay self-deadlock; +12 from #3041 P1-4 codex: the lease record
    helpers now RETURN the recorded lease (with its per-record `generation`
    nonce) and callers adopt it back so a later exact-match/by-generation clear
    targets the precise stored identity (no clobber of a newer lease); +4 from
    #3082 queued-only answer-flush gate
    (`is_queued_notice = false` for the TUI idle-response placeholder); +139
    from #3099/#3100 injected-prompt classifier + neutral system-continuation
    note; +140 from the #3099/#3100 codex re-review: P1 bridge-tail output
    delivery for system-continuation, anchored continuation classifier, and the
    P2 pinned injected-message-id cleanup helper + regression tests; +50 from the
    #3100 codex P2 fix: strip a leading SSH-direct injection wrapper line before
    the continuation `starts_with` check so a wrapped/round-tripped banner is not
    mis-classified as a human turn, plus wrapped/quoted-mid-body regression tests;
    +32 from #3105 self-heal of the authoritative tmux-session→channel registry
    for live thread-suffixed TUI sessions whose watcher slot was evicted
    (rehydrate loop re-registers the settings-derived owner channel + a bounded
    incident, never routes from the dedupe mirror); +84 from #3105 codex-P1
    sub-case B: rehydrate now tombstone-evicts the stale dedupe mirror for
    dead/orphaned sessions (pane gone + no live watcher) so the idle relay loop
    stops re-emitting the per-poll drift/skip WARN; +74 from #3105 codex-P2: the
    dead/orphaned verdict is now flake-resistant — `has_live_pane` is sampled
    across multiple probes (any live read aborts eviction) and a session is only
    evicted once the hard `tmux has-session` check confirms it is truly gone, so a
    transient pane-probe flake can never tombstone a LIVE session's mirror; +26
    from #3105 codex-P2 followup: the blocking rehydrate pass (sync `tmux`
    subprocess probes + the multi-sample `std::thread::sleep`) is now dispatched
    via `tokio::task::spawn_blocking` so it never stalls a Tokio executor worker);
    +37 from #3075 codex P1 #2: the `<task-notification>` edit-repeat early-return
    now clears exactly the external-input turn lease it recorded
    (`clear_observed_external_turn_lease_if_current`) before returning, so a
    dangling non-`Unassigned` lease can no longer make session-bound delivery skip
    a legitimate bridge-tail delivery, plus exact-match preserve-newer regression
    tests; net -1 from #3075: the `<task-notification>` TaskNotificationEvent class now
    renders a structured, deduped card (the `terminal injected input` raw block
    is replaced for that class only) — the card render/parse/dedupe-store logic
    lives in the new `tui_task_card.rs` module, and the shared
    `strip_terminal_controls` + ASCII `truncate_chars` helpers were consolidated
    there too, so this file's surface shrank by one line overall; the new
    `tui_task_card.rs` module (627 prod LoC, below the giant threshold) hosts the
    card render/parse/JSON-aggregate/dedupe-store logic; +48 from #3075 codex P1
    #1: a `CardSlot::Pending` variant + `TaskCardOutcome` enum so a repeat that
    races ahead of `record_card_message` drops as a no-op instead of building
    `MessageId::new(0)` (panic), plus the pre-record-repeat regression test);
    +21 from #3075 codex P2: the TaskNotificationEvent post-failure path now
    releases the reserved card placeholder via `forget_reserved_card`
    (exact-match: only while `message_id == 0`, never evicting a concurrently
    recorded real id) so a transient Discord post failure no longer leaves a
    stuck `Pending` slot suppressing that task-id for up to 1h; the next
    same-task notification reserves fresh and reposts (plus failed-post-reposts /
    preserve-recorded-id / missing-id regression tests); +15 from #3146 Part 1:
    `claim_tui_direct_synthetic_turn` now clears the stale `📦 … idle N분`
    idle-recap card once a TUI-driven turn is owned for the channel, reusing the
    shared `idle_recap::spawn_clear_idle_recap_for_channel` helper (keyed on
    `channel_id`, mirrors the Discord-intake clear);
    +96 from #3041 P1-4 codex: a `TuiDirectObservedLeaseEarlyReturnGuard` (arm right
    after `record_observed_external_turn_lease`, disarm on the success path before
    the bridge-tail ownership block) closes the early-return leak where a FAILURE
    abort (health registry None, notify `resolve_bot_http` Err/503, task-card repeat,
    anchor POST failure) left the recorded (possibly BridgeAdapter-owned) lease set
    for the full TTL, blocking the legitimate watcher/sink delivery. The guard clears
    BY the recorded generation (`clear_external_input_relay_lease_if_generation_matches`)
    so a newer same-key lease recorded during the await is never clobbered; success
    persistence is preserved by disarming before the bridge legitimately retains the
    turn (plus failure-clear / disarm-persist / no-clobber regression tests);
    +90 from #3183: the idle-response-tail start offset is now clamped to
    `>= shared.committed_relay_offset(channel_id)` (the watcher's confirmed
    delivery watermark, #3017) at the single `spawn_claude_idle_response_tail_once`
    choke point, so when the tmux watcher already relayed a turn's terminal
    response the idle tail starts past it and cannot re-relay the same byte range
    (the double-relay duplicate). When the watcher stopped / never covered the turn
    the watermark is 0 (or lags), so the clamp is a no-op and the tail still relays
    from the prompt-timestamp offset — the #3176 outage fallback is preserved
    (plus clamp-up / outage-noop unit tests);
    +104 from #3154 codex P1/P2: the deferred synthetic turn-start path now (P1-3)
    routes the relay-owner handoff through two shared pure decisions —
    `observer_should_spawn_bridge_tail` (the observer stands down whenever the
    start was deferred OR a watcher already owns the lease) and
    `claim_should_adopt_relay_owner` (a successful claim that flips the owner
    re-records the lease as the watcher owner) — so the deferred worker is the
    SINGLE relayer: not zero (no relay GAP) and not two (no duplicate relay). Both
    the inline and the deferred (`pending_start_claim_fn`) claim paths call the
    same adoption decision, and the post-anchor bridge-tail block consults
    `observer_should_spawn_bridge_tail` instead of an inline guard (plus the
    no-GAP / observer-skip / failed-claim-no-adopt regression tests); +74 from
    #3154 codex P1 (BridgeAdapter-GAP): the P1-3 fix only closed the TmuxWatcher
    owner path — a deferred claim that RESOLVED to a BridgeAdapter owner had the
    observer stand down (deferred) AND no worker bridge tail AND background idle
    relay suppressed by the inflight, so `relayer_count == 0` (the synthetic
    turn's output was lost). The observer cannot know the resolved owner pre-claim
    (the claim runs later in the worker), so the worker (`pending_start_claim_fn`)
    now mirrors the inline path: after its claim resolves, the new owner-kind-aware
    `deferred_claim_requires_bridge_tail_relayer` (true iff the resolved owner is
    the BridgeAdapter) makes the worker spawn EXACTLY ONE bridge tail for the
    BridgeAdapter case and stand down for the watcher case — `maybe_spawn_claude_idle_response_tail`
    self-gates on `bridge_adapter_owns_external_turn`, so a watcher/stale lease can
    never double-relay. New parallel no-GAP tests assert `relayer_count == 1` for
    BOTH resolved owners (RED before this fix: BridgeAdapter count == 0 == GAP;
    over-eager spawn would push the watcher path to count == 2 == DUPLICATE);
    +71 from #3154 codex P1 (timestamp-anchor output loss): the worker-spawned
    BridgeAdapter tail used to synthesize `observed_at = Utc::now()` AFTER the
    deferred-claim wait, so `maybe_spawn_claude_idle_response_tail`'s timestamp
    scan skipped every transcript byte written during the wait window (the bytes
    of THIS synthetic turn). The claim now carries its post-drain EOF
    `turn_start_offset` (the `relay_last_offset()` it already seeded into the
    inflight) on `TuiDirectSyntheticTurnClaim`, and the worker passes it as the
    new `explicit_start_offset` arg; the shared `resolve_idle_tail_start_offset`
    choke point anchors DIRECTLY to that offset (bypassing the `Utc::now()` scan)
    for the deferred path while the inline path keeps the timestamp scan
    (`None`). The committed-offset clamp still dedupes against watcher delivery,
    so the relayed window is exactly `[turn_start_offset, EOF)` — no byte skip,
    no prior-turn re-relay (RED→GREEN test: stale-high fallback skips the turn
    under the old scan, explicit anchor relays it whole); +294 from #3256: the
    Claude external-input idle path now STREAMS prose THROUGH a single bridge
    turn instead of pre-collecting the whole response and posting one batched
    `[Text{full}, Done]` at turn end. `run_claude_idle_response_tail` spawns the
    transcript reader on a blocking thread, buffers leading frames until the
    first content frame (empty-turn → original no-card path preserved), then
    `stream_tui_idle_response_through_bridge` sends EXACTLY ONE intake
    placeholder + ONE `spawn_turn_bridge` and `forward_idle_stream_into_bridge`
    forwards each reader `StreamMessage` (Text/ToolUse/OutputOffset/Done) into
    the same bridge `tx` LIVE so a LONG continuous turn relays prose
    progressively within that one card; leading TUI chrome is stripped from the
    FIRST Text frame and a fallback `Done` is appended only if the reader closed
    without one (bridge finalizes EXACTLY ONCE, "first wins"). The committed
    runtime-binding offset still advances on successful delivery and the
    start-offset clamp to `committed_relay_offset` is untouched (no double-send).
    The Codex idle path (`relay_tui_idle_response_through_bridge`) is unchanged;
    +54 from #3282: the deferred pending-start worker's terminal backstop ABORT
    (`backstop_abort_foreign_inflight_live`) now runs the injected
    `pending_start_abort_cleanup_fn` — resolved from the SAME
    `shared.serenity_http_or_token_fallback()` provider/command-bot identity that
    added the anchor `⏳` (#3164 add≡remove invariant) — removing the stranded
    `⏳` and marking `⚠` on the anchor message (precedent: the watcher's
    auth-expired `⏳ → ⚠` swap), because no claim ever drives the normal
    `⏳ → ✅` completion for an ABORTed synthetic start (the `⚠` swap itself is
    superseded by #3296: the hook now KEEPS the `⏳` and records the durable
    aborted-anchor marker — see the entry head); -75 from #3282
    follow-up: verbose multi-line comment blocks compressed (no semantic
    change) to offset the +54 growth and keep prod LoC under the frozen
    `giant_file_ratchet` baseline (5438, #3028); #3305: a LOCAL-completing
    pass-through slash command (`/effort` `/compact` `/cost` `/context`, the
    `commands::is_local_only_slash_command_kind` allow-list) renders in the TUI
    but starts NO model turn, so its `<command-*>` transcript echo must NOT mint a
    synthetic external turn. `relay_observed_prompt` hoists the
    `slash_command_control_kind`, and when it is local-only posts ONLY the
    kind-only `format_slash_command_control_note` and RETURNS before `disarm()` —
    the armed `observed_lease_early_return_guard` clears the recorded lease
    (generation-exact, no anchor/⏳/synthetic inflight), and the Claude idle
    transcript loop `continue`s past such a prompt (no external-owner lease / no
    synthetic-claim wait / no response tail) — so the next injection is not
    FOREIGN-ABORTed and the #3302 sweeper sees no fake row. `/loop` (a model-turn
    command) is OFF the allow-list and keeps the full #3178 lifecycle (fail-safe
    allow-list; an anti-drift test pins the list to the `ClaudeSlashPassthrough`
    variant set). Comment dedup in the same root offsets the new lines, lowering
    the frozen baseline 5438 -> 5429 (-9, locks in the win).
  - `src/services/discord/idle_recap.rs` (1319 prod lines; idle-recap card
    compose/post/clear surface, registered giant-file (#3036) — bugfix only
    outside an extraction plan. Crossed 1000 prod LoC with #3146 Part 1: the
    shared capture-at-claim + CAS clear helpers, the `channel_has_active_turn`
    mailbox/inflight probe, and the `post_recheck_action` seam that skips/undoes
    a recap post when a turn raced the compose window. Split compose vs
    lifecycle/clear submodules before adding behavior).
  - `src/services/codex_tmux_wrapper.rs` (1289 lines; Codex tmux wrapper JSON
    event parser and relay bridge for native Codex session events — bugfix only
    outside an extraction plan; +65 from #3275: capture per-call
    `token_count.info.last_token_usage` and re-emit it as a Claude-compatible
    nested `usage` on the success result frame so watcher-owned codex turns
    persist token telemetry — never the session-cumulative
    `info.total_token_usage`).
  - `src/services/tui_prompt_dedupe.rs` (1356 lines; shared TUI prompt
    fingerprinting/dedupe state for hook and rollout relay paths, bugfix only
    outside an extraction plan; +88 from #3041 P1-4 codex: a per-record
    `generation: u64` nonce on `ExternalInputRelayLease` (process-global
    `AtomicU64`, stamped in `record_external_input_turn_lease` which now returns
    the recorded lease) plus the `clear_external_input_relay_lease_if_generation_matches`
    no-clobber primitive — two value-identical `Unassigned` leases for the same
    key get DISTINCT generations so a slow old delivery's RAII guard never clears
    a newer lease; +9 from the #3099 re-review crate-visible
    `reset_state_for_tests` helper; +26 from #3105 codex-P1 sub-case B
    `evict_dead_tmux_mirror` tombstone helper that drops both the runtime and
    channel mirror for a dead/orphaned session and then allows re-registration;
    -20 from #3041 P1-4 codex R3: REMOVED the now-unused
    `external_input_relay_lease_generation` read-only accessor (its only caller was the
    watcher, which now snapshots the lease ONCE via the single-lock
    `external_input_relay_lease` and derives both presence + generation from that one
    atomic read, closing the present/generation TOCTOU) plus its dedicated accessor unit
    test; the watcher-snapshot no-clobber regression test is retained, rewritten to take
    its G1/G2 snapshots from `external_input_relay_lease(...).map(|l| l.generation)`;
    +62 from #3304: slash-command canonical prompt keys for `<command-*>` XML vs
    `/command args` dedupe, plus focused loop skill-expansion regressions).
  - `src/services/discord/recovery_engine.rs` (4090 lines; #3016 phase-5b2
    dropped the `mailbox_finalize_owed` construction from the three recovery
    watcher-spawn handles; +9 from #3166
    fetching real context thresholds for the recovered-turn status panel; +36 from #3099
    task-notification anchor `⏳` cleanup for `user_msg_id == 0` recovery; +4
    from the #3099 re-review pinned-injected-message-id cleanup target; +55 from
    #3078 PR-2 routing recovery completion through `StatusPanelController` behind
    a shadow parity check (the controller adopts the recovered panel id and its
    chosen completion id is asserted equal to the legacy
    `recovery_status_panel_message_id_for_completion` result; the legacy path
    still executes the Discord IO, so behaviour is unchanged); +4 from #3017
    routing the recovery terminal through the single-authority finalizer
    (`submit_terminal` + `FinalizeContext::monitor`) instead of inline
    `mailbox_finish_turn`; +60 from #3248 gap-1 adding
    `reseed_watcher_owned_finalizer_ledger` + two guarded reattach call-sites that
    re-register the watcher-owned turn in the post-restart finalizer ledger so a
    mid-turn deploy's live pane auto-reconciles without a new user turn; net ±0
    from #3293 promoting the recovery terminal relay from `bool` to the 3-state
    `RecoveryRelayOutcome` — the five notice branches route through
    `recovery_paths/restart.rs::dispose_recovery_relay_outcome` (permanent
    Discord 404/403/410 force-clear + 3-restart transient budget); the pure
    decision matrix lives in `recovery_paths/shared.rs`).
  - `src/services/discord/health.rs` (402 prod lines after the #3038 Phase A
    directory decomposition; module root keeps the `HealthRegistry` core +
    re-export surface, and the former monolith body lives in six flat
    `health/` submodules, all sub-1000 prod LoC: `runtime_resolve.rs` (321),
    `headless_turn.rs` (297), `send_target.rs` (150), `send_gate.rs` (374),
    `manual_delivery.rs` (580), `send_api.rs` (259). Previously 2240 after
    the #3034 dead-code sweep, 2292 after #3038 send-to-agent dispatch
    extraction to `outbound/send_to_agent.rs`, #1879 snapshot/mailbox
    extraction, and #3082 answer-flush-barrier field).
  - `src/services/discord/health/recovery.rs` (2641 lines; health recovery
    extraction surface, split further before adding non-bugfix behavior; +70
    from #3126 stall-watchdog completed-idle false-positive guard tests; +88
    from #3169 stall-watchdog jsonl-mtime liveness guard + tests, closing the
    Death #1 force-clean false-positive on loop mid-write sessions; -4 from
    #3293 routing the mailbox probe through the non-creating
    `health/mailbox.rs::peeked_provider_mailbox_state` so repair probes stop
    minting permanent registry entries for non-existent channels).
  - `src/services/discord/router/message_handler/intake_turn.rs` (3771 lines;
    Discord message intake turn orchestration split from the router message
    handler; bugfix only outside a further extraction plan; +9 from #3082
    queued-only answer-flush gate (`is_queued_notice` on the two
    `send_intake_placeholder` call sites: `true` for the race-lost queued card,
    `false` for the active-turn placeholder); +57 from #3182 normal-dequeue
    queue-pending reaction cleanup (`queue_pending_reactions_to_clear` helper +
    `remove_reaction_raw` at the `started==true` promotion point, removing the
    stranded `📬`/`➕` so a processed message no longer shows `📬`+`✅`); +1 from
    #3038 S1 mechanical `.queued_placeholders` -> `.queued.queued_placeholders`
    re-wire after lifting cluster C into `QueuedPlaceholderState`).
  - `src/services/discord/router/message_handler/headless_turn.rs` (1316 lines;
    headless Discord turn launch/terminal-response path split from the router
    message handler; bugfix only outside a further extraction plan).
  - `src/services/discord/meeting_orchestrator.rs` (3222 lines after #3034
    dead-code sweep removed `is_meeting_channel`).
  - `src/services/discord/turn_bridge/tmux_runtime.rs` (1545 lines; provider
    stop-token/tmux binding runtime + PID-exit observation helper (#2426),
    split before adding non-bugfix behavior. #3169: added the
    claude-anonymous-teardown SIGINT suppression guard (death #3)).
  - `src/services/discord/turn_bridge/mod.rs` (6778 prod lines; the BRIDGE
    spawn/turn-lifecycle surface — `spawn_turn_bridge` and the per-channel
    turn loop. Registered giant-file (#3038 decompose target — see
    `giant-file-registry.md`, owner `discord-relay`, deadline 2026-08-31).
    It surfaced as a giant only after #3028 fixed the prod/test splitter: an
    unterminated char-literal scan on a Rust lifetime (`&'a self`) inside the
    first inline `#[cfg(test)] mod` block over-extended the block to EOF, so
    most of the production code was mislabeled test and the file reported only
    626 prod LoC. #3038 turn_bridge S1 moved six helper-zone clusters out of
    the root while leaving the `spawn_turn_bridge` async body byte-identical:
    `status_panel.rs` (437 prod; tests split to `status_panel_tests.rs`),
    `voice_completion.rs` (385 prod; tests split to
    `voice_completion_tests.rs`), `headless_delivery.rs` (415 prod),
    `memory_lifecycle.rs` background-memory observation, `turn_analytics.rs`
    analytics persistence, and `terminal_delivery.rs` confirmed-end advance.
    Hotfile — bugfix only outside the #3038 decompose plan).
  - `src/services/discord/turn_bridge/completion_guard.rs` (1834 lines).
  - `src/services/discord/turn_bridge/tmux_runtime.rs` (1545 lines).
  - `src/services/discord/turn_bridge/terminal_delivery.rs` (604 prod lines;
    no longer a giant-file after the #3028 splitter fix corrected its inline
    `#[cfg(test)] mod` accounting (previously miscounted as 1341 prod). Its
    giant-file-registry [[entry]] was removed. #3038 turn_bridge S1 moved
    `advance_tmux_relay_confirmed_end` here; split the remaining lease wiring
    vs delivery helpers before adding behavior).
  - `src/services/discord/turn_finalizer.rs` (1526 prod lines; single-authority
    turn-finalize state machine — ledger/actor-loop/reconciler. Crossed the
    giant-file threshold when #3041 P1-0 added the dormant `DeliveryLeaseCell`
    finalizer messages/handlers on top of #3143's `FinalizeContext::monitor()` +
    monitor turn-key/ledger-generation logic; tracked decompose target — see
    `giant-file-registry.md` (owner `discord-finalizer`, deadline 2026-08-31,
    issue #3016). Bugfix only outside a finalizer-decomposition plan).
  - `src/services/discord/formatting.rs` (2802 lines; net +0 from #3034 scoped
    dead-code allows on the `MonitorHandoffReason::InlineTimeout` /
    `MonitorHandoffStatus::Failed` reserved variants — the two added `#[allow]`
    lines were offset by collapsing the adjacent reason comments back to inline
    form, so the file stays at its frozen baseline; +46 from #3082
    answer-flush-barrier guards (+11 around the plain multi-chunk send loops;
    +24 from the #3082 codex follow-up that also guards the edit/replace path
    `replace_long_message_raw_with_outcome` and bumps `note_progress` after each
    delivered chunk for the progress-aware flush wait; +11 from the codex P1-2
    residual that bumps `note_progress` after the FIRST edited chunk too, on the
    multi-chunk path only, so the queued-card waiter's inactivity grace cannot
    expire between the first edit and the first continuation); +37 from #3104
    `finalize_stale_streaming_footer` / `text_ends_with_streaming_footer` shared
    terminal-idle reconciliation helpers + their unit tests).
  - `src/services/discord/prompt_builder/` (directory, refactored).
  - `src/services/discord/runtime_bootstrap.rs` (524 production lines after
    #3038 run_bot S0/S3; characterization tests pin the startup-doctor barrier,
    restored settings filters, queued-placeholder filtering/deletion, and
    gateway intents, then the low-risk clusters moved verbatim into
    `runtime_bootstrap/`: `restored_state.rs` (124), `queued_placeholders.rs`
    (193), `startup_doctor.rs` (156), `orphan_recovery.rs` (337),
    `session_gc.rs` (102 prod / 69 test), `framework_setup.rs` (287),
    `spawns.rs` (218), `recovery_flush.rs` (356), `voice.rs` (140),
    `gateway_lease.rs` (187), `shutdown.rs` (203), and `intake.rs` (63).
    The namespace is capped at 700 prod lines per child module in
    `audit_maintainability_config.toml`; the root is no longer a prod giant and
    was removed from `giant_file_registry.toml`, while S4
    (`run_bot_build_shared_data`) stays gated on the SharedData slice).
  - `src/services/discord/session_runtime.rs` (1753 lines).
  - `src/services/discord/voice_barge_in.rs` (4657 lines; net +0 from #3034
    scoped dead-code allows on the test-only runtime API
    (`disabled` / `runtime_config_snapshot` / `apply_voice_command` /
    `reset_after_playback_start` / `clear_playback`) — the five added `#[allow]`
    lines were offset by collapsing their reason comments to inline form and
    rewrapping adjacent doc comments, so the file stays at its frozen baseline;
    voice STT/TTS,
    lobby routing, progress mirroring, and barge-in orchestration surface;
    tracked decompose target — see `giant-file-registry.md` (owner
    `voice-runtime`, deadline 2026-08-31, #3036)).
  - `src/voice/receiver.rs` (1052 lines; voice receive pipeline, utterance
    segmentation, artifact cleanup, and retention policy surface; split before
    adding non-bugfix behavior).
  - `src/voice/announce_meta.rs` (1001 lines; voice announcement durability /
    handoff metadata surface; crossed the giant threshold when #3034 restored
    per-item dead_code reasoning on the runtime-gated durable helpers; tracked
    decompose target — see `giant-file-registry.md` (owner `voice-runtime`,
    deadline 2026-08-31, #3036)).
  - `src/db/automation_candidates.rs` (1003 lines; pipeline-v2 automation
    candidate iteration repository surface (#2064); crossed the giant threshold
    when #3034 restored per-item dead_code reasoning on the still-unwired
    iteration-loop helpers; tracked decompose target — see
    `giant-file-registry.md` (owner `automation-pipeline`, deadline
    2026-08-31, #3036)).
  - `src/services/discord/commands/config.rs` (1054 lines).
  - `src/services/discord/{commands/text_commands.rs, commands/diagnostics.rs,
    discord_config_audit.rs, router/intake_gate.rs, inflight.rs}`
    (all 1000+ production lines).
- active_callsite_coverage: n/a.
- invariants: watcher single-owner per #1222; placeholder lifecycle invariants
  per #1112; `/api/inflight/rebind` is the only path that synthesises an
  inflight state file (`src/services/discord/inflight.rs:107`,
  `:415`, `:952`). Cancel-induced death must trigger immediate re-attach
  (#1283 contract, see `src/services/discord/watchers/lifecycle.rs`).
- allowed_changes: `bugfix` only on `tmux.rs` and the giant Discord modules.
  `extraction` requires a follow-up issue.
- 2026-05-18 refresh: #2431/#2475/#2477/#2478 touched `tmux.rs`,
  `tmux_watcher.rs`, watcher lifecycle, and the new TUI prompt relay/dedupe
  helpers for SSH-direct prompt relay. The migration-sensitive invariant remains
  one owner per `(tmux_session, output_path)` and separate runtime-vs-relay
  offsets for Codex rollout wrappers.
- 2026-05-18 refresh: #2558 removed dead watcher/placeholder cleanup parameters
  and retained a warning log for pause/epoch placeholder delete failures; no
  new watcher ownership path was introduced.
- tests: `src/high_risk_recovery.rs` cancel/recovery suites.
- related_issues: #964, #1112, #1138, #1222, #1223, #1283.

### `dashboard_routes`

- canonical_modules: `src/server/routes/*.rs` (per-domain route module).
  `src/server/routes/auto_queue.rs` is now a small HTTP-only facade;
  its query/command/view/FSM behavior lives under
  `src/services/auto_queue/{query,command,view,fsm,phase_gate}.rs` plus
  smaller route-delegation slices.
  `src/services/auto_queue/activate_command.rs` (1351 lines, post-#1444
  idempotency-guard expansion + #3038 phase-helper decomposition) is the
  canonical activate/dispatch-next command surface; it is intentionally above
  the giant-file threshold and tracked here. The `activate_with_deps_pg`
  orchestrator was decomposed into named phase helpers (resolve-run-id,
  acquire-lock, promote, empty-run completion, capacity, group planning,
  finalize) under #3038 — the added doc-commented scaffolding nets a small
  file-LoC increase while shrinking the god-function from ~1158 to ~559 lines.
  Further growth requires a split issue.
  `src/services/auto_queue/cancel_run.rs` (1032 lines) is the canonical
  auto-queue cancellation and run-stop command surface; split before adding
  non-bugfix behavior.
- legacy_modules: none, but several routes still call `legacy_db()` against
  the SQLite compat handle (see `known-legacy.md`).
- do_not_edit_without_migration_plan (giant-file routes):
  - `src/server/routes/kanban.rs` (2676 lines after #3037 backflow batch
    relocated the `require_explicit_bearer_token` /
    `resolve_requesting_agent_id_with_pg` auth/identity helpers to
    `crate::services::kanban`).
  - `src/server/routes/docs.rs` (5884 lines; +4 from #3293 documenting
    the stale-mailbox repair `purge` body param + registry-purge response
    fields).
  - `src/server/routes/escalation.rs` (1376 lines).
  - `src/server/routes/meetings.rs` (1675 lines).
  - `src/server/routes/review_verdict/decision_route.rs` was decomposed in
    #3038 slice 1 into a 139-line module root (orchestrator + shared types)
    plus eight sub-1000-line directory modules under
    `src/server/routes/review_verdict/decision_route/`
    (`repo_card`/`repo_dispatch`/`worktree_stale`/`adapters`/`pending`/
    `accept`/`dispute`/`dismiss_finalize`). Conservatively still bugfix-only
    until the PG-backed characterization harness (slice 3) lands; re-inflation
    is ratcheted in `scripts/audit_maintainability_giant_baseline.toml`.
  - `src/server/routes/{agents,agents_crud,agents_setup,v1,resume}.rs` (all
    1000+ production lines). (`dispatches/thread_reuse.rs` dropped below the
    giant threshold in #3037 after its Postgres/Discord-API thread-map helpers
    were relocated to `services/dispatches/discord_delivery/thread_reuse.rs`.)
- active_callsite_coverage: legacy_db helper coverage tracked separately —
  see `known-legacy.md` row `legacy_db_helper`.
- invariants:
  - `/api/inflight/rebind` is the only synthetic inflight writer
    (`src/server/routes/health_api.rs:684`).
  - Dashboard routes never write to canonical config files; they read DB
    state and emit events.
- allowed_changes: `bugfix` only on giant routes; `new_feature` only when
  added to a sub-1000-line module or after splitting. Auto-queue domain logic
  changes must go under `src/services/auto_queue/*`; the route facade should
  remain extraction/delegation only. New routes must register in the route
  inventory generator.
- tests: `src/server/routes/routes_tests.rs`, plus per-route module tests.
- related_issues: split issues TBD (file under follow-up).

### `cli_runtime`

- canonical_modules: `src/cli/*.rs`.
- legacy_modules: none.
- do_not_edit_without_migration_plan (giant-file):
  - `src/cli/migrate.rs` is the retired postgres-cutover facade (now below the
    giant-file threshold; bugfix only).
  - `src/cli/doctor/orchestrator.rs` (4376 lines).
  - `src/cli/migrate/apply.rs` (3230 lines).
  - `src/cli/migrate/{plan.rs (1513), source.rs (1612)}`.
  - `src/cli/{init.rs (1445), client.rs (2955), direct.rs (1781),
    dcserver.rs (1560)}`.
  - `src/cli/provider_cli/mod.rs` (1039 lines).
- active_callsite_coverage: n/a.
- invariants: LaunchAgent plist and runtime layout are generated only — see
  the matrix in `docs/source-of-truth.md`.
- allowed_changes: `bugfix` only; PG-cutover retention plan is owned by
  #1239.

### `runtime_core`

- canonical_modules: `src/config.rs`, `src/runtime_layout/mod.rs`,
  `src/server/mod.rs`, `src/kanban/state_machine.rs`, `src/receipt.rs`,
  `src/github/sync.rs`, `src/reconcile.rs` (periodic stale-inflight + orphan
  sweep), `src/high_risk_recovery.rs` (PG recovery harness for delivery
  outbox/notify), `src/server/task_dispatch_claims.rs` (cluster-aware
  task-dispatch claim coordination), `src/server/cluster.rs`
  (cluster role/leader-failover coordination), and `src/server/worker_registry.rs`
  (supervised-worker registry / leader-only lifecycle).
- legacy_modules: none — these are shared runtime coordination surfaces.
- do_not_edit_without_migration_plan (giant-file):
  - `src/config.rs` (2272 lines).
  - `src/server/mod.rs` (2410 lines).
  - `src/receipt.rs` (1842 lines).
  - `src/github/sync.rs` (1488 lines).
  - `src/reconcile.rs` (1821 lines; periodic reconcile loop covering stale
    inflights, orphan uploads, dispatched-session drift, and queue-review
    drift — split before adding non-bugfix behavior).
- active_callsite_coverage: n/a.
- invariants: config precedence, runtime path generation, kanban state, receipt
  persistence, and GitHub sync must keep their existing owner-specific
  contracts; split work needs a dedicated extraction issue before new feature
  logic lands here.
- allowed_changes: `bugfix` only; `src/kanban/` extraction is scoped by
  `docs/agent-maintenance/kanban-extraction-plan.md`; new feature logic must
  land in smaller owner-specific modules or a scoped extraction branch.
- tests: owner-specific tests, with `src/kanban/state_machine.rs` inline tests moving per
  `docs/agent-maintenance/kanban-extraction-plan.md`.
- related_issues: #1786, #1787, #1818-#1825 for `src/kanban/`; other runtime
  giant-file split issues TBD.

### `db_layer`

- canonical_modules: `src/db/{mod,postgres,schema}.rs` and per-domain modules.
- legacy_modules: SQLite path through `libsql_rusqlite` (see `known-legacy.md`).
- do_not_edit_without_migration_plan (giant-file):
  - `src/db/auto_queue/tests.rs` is the migrated auto-queue test harness; it is a
    dedicated `*_tests.rs` file (excluded from the production giant-file count),
    so add coverage freely but keep it split-friendly.
  - `src/db/auto_queue/entries.rs` (1508 lines; awaiting follow-up split per
    auto-queue decompose epic #1782).
  - `src/db/auto_queue/phase_gates.rs` (1639 lines after #1980 durable
    reconciliation, production LoC; PG-backed tests for `current_batch_phase_pg`
    + `reconcile_phase_gate_for_terminal_dispatch_on_pg_tx` live in a
    `#[cfg(test)] mod`. Split the test module out into a sibling
    `phase_gates_tests.rs` before adding new feature logic).
  - `src/db/dispatches/mod.rs` (1028 lines; dispatch slot/thread binding and
    outbox-adjacent PG helpers, pushed over the giant-file threshold by
    #2778/#2783 slot-isolation recovery. Split slot allocation helpers before
    adding new feature logic).
  - `src/db/kanban_cards/` (1932 total lines; kanban card persistence and
    GitHub sync lookup surface).
  - `src/db/postgres.rs` (1018 lines).
  - `src/db/dispatched_sessions.rs` (1610 lines; dispatched session
    persistence helpers. #3306: +48 for the narrow `load_session_channel_id_pg`
    durable-truth accessor the idle-relay drift self-heal reads).
  - `src/db/session_transcripts.rs` is a retained PG-cleanup surface (now below
    the giant-file threshold; bugfix only).
  - `src/db/prompt_manifests/` (directory, refactored).
  - `src/db/intake_outbox.rs` is the intake-node-routing claim/transition/sweep
    surface; its production LoC is now below the giant-file threshold once the
    `#[cfg(test)] mod` PG coverage is excluded (bugfix only).
- active_callsite_coverage: PG-only cleanup tracked per #1237/#1238/#1239 —
  see `known-legacy.md`.
- invariants: production reads/writes go through `pg_pool_ref()`; `legacy_db()`
  remains for unmigrated callsites only.
- allowed_changes: `bugfix` on existing path; `new_feature` MUST use PG.
- tests: `src/integration_tests/postgres_only/*`.
- related_issues: #843 epic, #1237, #1238, #1239.

### `services_misc_giants`

The remaining giant-file modules under `src/services/` not covered above.
Line counts are *production* LoC (the `Prod` column in `module-inventory.md`,
which excludes `#[cfg(test)] mod` blocks); the freshness gate keeps them in sync.

- `src/services/auto_queue.rs` (1546) and
  `src/services/auto_queue/activate_command.rs` (1351); auto-queue route
  behavior is split across `src/services/auto_queue/*` slices, with
  `activate_command.rs` now giant-file territory.
  `src/services/auto_queue/cancel_run.rs` (1032) is also giant-file territory;
  split before further non-bugfix growth.
- `src/services/onboarding/mod.rs` (2936),
  `src/services/dispatched_sessions.rs` (1328), and
  `src/services/settings.rs` (1007) — service-layer route support surfaces
  split out of the large dashboard route modules. (`src/services/onboarding.rs`
  and `src/services/api_friction.rs` have been removed/decomposed.)
- `src/services/dispatches/outbox_route.rs` (1089) — dispatch outbox route
  support extracted from the route layer; split before adding non-bugfix
  behavior.
- `src/services/claude.rs` (3991), `src/services/gemini.rs` (1358),
  `src/services/qwen.rs` (2196), `src/services/codex.rs` (3001),
  `src/services/opencode.rs` (1881), `src/services/provider.rs` (1796) —
  provider adapters. (#3034 removed dead non-cancel `execute_command_simple*`
  twins from the claude/codex/gemini adapters and a superseded
  `select_counterpart_from` from provider. #3263 added the Codex max-of-cache
  context-window fallback (now cache-first for both `Some(model)` and the
  provider-default `None` path — constant only on absent/unusable cache),
  documented per-provider context-window intent, and a
  `codex_context_window_from_cache` unit-test module. #3281 truthified the
  Claude TUI producer-exit `lines=` count via `ReadHarvestStats` and added
  `claude_tui_zero_harvest_*` observability events for delivered turns that
  forwarded nothing. #3038 S1: +101 from extracting the warm-followup stranded
  prompt-draft recovery block into `recover_claude_tui_stranded_prompt_draft`
  with the `#[must_use] ClaudeTuiDraftRecoveryOutcome` carrier — behaviour-
  preserving, the +101 is the new outcome enum, the two verbatim-extraction doc
  contracts, and the call-site `match` dispatch (the recovery body itself is a
  move). claude.rs is not in the giant-file ratchet baseline; this is a
  freshness-annotation sync, not a baseline raise. Slice S3 relocates the
  hosting cluster to a directory and drives claude.rs back under the giant
  threshold.)
- `src/services/codex_tui/rollout_tail.rs` (1663) — Codex TUI rollout tail
  parsing and resume identity surface; split before adding non-bugfix behavior
  beyond the #2169 session identity fix.
- `src/services/codex_tui/input.rs` (1350) — Codex TUI input readiness
  detector and prompt delivery surface (#2399 hardened the post-turn
  handoff deadline). Treat as giant-file territory; split before adding
  non-bugfix behavior beyond the readiness/cancel contract.
- `src/services/claude_tui/input.rs` (1294) — Claude TUI input readiness
  detector, prompt delivery, and cancellation/offset handoff surface. Treat as
  giant-file territory; split before adding non-bugfix behavior beyond the
  readiness/cancel contract.
- `src/services/memory/memento.rs` (1893).
- `src/services/dispatched_sessions.rs` (1328) — dispatched session domain
  service. This is the post-#1515 SRP extraction target for route/database
  callsites, but the module itself is now giant-file territory; split focused
  helpers before adding non-bugfix behavior. (+5 from #3169 exposing the idle-
  kill `latest_runtime_activity_unix_nanos` jsonl-mtime probe to the stall-
  watchdog liveness guard.)
- `src/services/settings.rs` (1007) — settings domain service extracted from
  the route layer in #1519. Keep follow-up changes bugfix-only unless the file
  is split further.
- `src/services/routines/{store.rs (2844), migrated.rs (1286),
  discord_log.rs (1344), agent_executor.rs (1044)}` — durable routine storage,
  migrated launchd validation, Discord notification plumbing, and agent
  execution are the canonical scheduled JS routine surfaces. Split focused
  helper modules before growing these files again.
- `src/services/platform/binary_resolver.rs` (1221).
- `src/services/discord/mod.rs` (4987; +34 from #3019 added the
  single-authority `increment_global_active` helper + doc mirroring the
  existing decrement helper — offset by removing 6 inline raw `fetch_add`
  blocks across the relay turn-start sites that now route through it; +12 from
  #3082 answer-flush-barrier field/init/doc; +81 from #3105 the authoritative
  `TmuxWatcherRegistry` gained a `restored_owner_by_tmux_session` map plus
  `restore_owner_channel_for_tmux_session`/`clear_restored_owner_for_tmux_session`
  so a live thread-suffixed TUI session with no live watcher slot can be
  re-registered authoritatively instead of dropped forever; -201 from #3038 S1
  lifting cluster C — the three queued-placeholder fields + their nine inherent
  methods — into `shared_state::QueuedPlaceholderState`, leaving a single
  `queued: QueuedPlaceholderState` group field on `SharedData` and re-exporting
  the type for surface freeze; ±0 from #3293 — the +13 closed-retry rewires
  (`mailbox_peek` + `*_with_closed_retry` routing for recovery kickoff and
  intervention enqueue) are offset by queue-exit comment dedup in the same
  root, no baseline raise),
  `src/services/discord_config_audit.rs` (1273).
- `src/services/turn_orchestrator.rs` (3089; +3 from #3293 declaring the
  `registry_purge` child module — the non-creating `peek` lookup and the
  operator-gated `remove_idle_entry` purge live in
  `turn_orchestrator/registry_purge.rs`, outside the frozen module root).

Decomposed below the giant-file threshold (no longer frozen; bugfix-scoped but
normal test growth is allowed): `src/services/analytics.rs`,
`src/services/provider_hosting.rs`, `src/services/claude_tui/hook_bundle.rs`,
`src/services/observability/mod.rs`, `src/services/pipeline_override.rs`,
`src/services/routines/loader.rs`, `src/services/platform/shell.rs`,
`src/services/platform/tmux.rs`, `src/services/mcp_config.rs`,
`src/services/process.rs`, `src/services/discord/tmux_lifecycle.rs`,
`src/services/qwen_tmux_wrapper.rs`, `src/services/discord/session_relay_sink.rs`,
`src/services/tui_turn_state.rs`, `src/services/session_backend.rs`,
`src/voice/turn_link.rs`.

Same rule: `bugfix` only without a split issue.

## Shared API Helpers

For new HTTP route logic that paginates over Postgres-backed lists, prefer the
shared helper `crate::utils::api::clamp_api_limit` (in `src/utils/api.rs`) over
inline `limit.clamp(1, 2000)` calls. The helper applies the standard API-limit
shape (default 50, clamped to 1..=2000) and is the single canonical site for
that bound — `scripts/audit_maintainability/checks/limit_clamp_duplication.py`
flags any new inline `clamp(1, 2000)` outside the helper definition (#1698).
For non-standard bounds, extend `clamp_limit(limit, default, max)` rather than
reintroducing bespoke clamp expressions.

## Updating This Page

- Re-run `python3 scripts/generate_inventory_docs.py` and reconcile the
  giant-file list against the `Prod` column in `module-inventory.md`. Each
  `(N lines)` token on this page must equal the measured production LoC;
  `scripts/check_agent_maintenance_docs.py` fails CI when it drifts, when a
  frozen entry's production surface grows (decomposition regression), or when a
  frozen entry has fallen below the threshold (ghost — remove it).
- When a giant file is split, move its canonical_module entry to the new
  module path, remove it from `do_not_edit_without_migration_plan`, and drop it
  from `scripts/giant_file_registry.toml`.
- When a new module crosses the `1000`-production-line threshold, register it in
  `scripts/giant_file_registry.toml` with an owner, deadline, and decompose
  issue (deadline-less registration is rejected by the generator) and add it to
  its feature block in the same PR — do not let the inventory generator be the
  only signal.
