# Change Surfaces

> Source: [`docs/agent-maintenance/index.md`](index.md). For every "where do I
> add this?" question, consult this page first. The giant-file list is the
> auto-generated inventory in
> [`docs/generated/module-inventory.md`](../generated/module-inventory.md) and
> the owner/deadline registry in
> [`docs/generated/giant-file-registry.md`](../generated/giant-file-registry.md);
> the rows below project the operational meaning of each entry.
>
> Last refreshed: 2026-07-11 (manual: #4055 durable task-notification card authority).
>
> PR #3456 dcserver-robustness: freeze counts re-synced after the reconcile
> row-allocation churn reduction (`src/reconcile.rs` now 1816 prod lines) and the
> OpenCode warm-server reuse/cancel recovery (`src/services/opencode.rs` now 2760 prod
> lines); no new logic added to either giant file, the line deltas are
> bugfix-only. On top of #3358 round 2 — synthetic-inflight carry-forward now
> gated on same-generation evidence: `tmux.rs` re-exports the new
> `committed_frontier_for_current_generation` reader from `tmux_session_files.rs`,
> which pairs the per-channel committed watermark with the `.generation` mtime
> wrapper-identity signal so a stale pre-restart frontier cannot clamp a
> freshly-reset synthetic forward — the content-skip guard;
> `tui_prompt_relay.rs::synthetic_start_offset_carry_forward` now takes
> `Option<u64>` where `None` means no clamp. On top of #3089 completion-footer
> slice — `tmux.rs` suppression exposure tests strip completion-only footer
> blocks so internal turns still delete cleanly; generated inventory includes
> `placeholder_live_events/completion_footer.rs`; on top of #3038 run_bot S5
> closing pass.

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

### `provider_output_guard`

- canonical_modules:
  - `src/services/provider_output_guard.rs` — pure, provider-aware,
    Markdown-aware whole-response classifier (`Clean` / `Hold` / `Blocked`).
  - `src/services/claude_tui/hook_output_guard.rs` — bounded Claude
    Stop/SubagentStop transcript-tail reader with canonical-path containment.
  - `src/services/discord/response_sanitizer.rs::sanitize_provider_response` —
    terminal Discord fail-closed boundary.
  - `src/services/discord/tmux_watcher/provider_output_guard.rs` and
    `src/services/discord/turn_bridge/stream_tick.rs::guarded_bridge_rollover_edit`
    — raw streaming-rollover boundaries.
- legacy_modules: none. Do not add substring replacement or partial redaction
  for provider harness control data; classify and hold/block the whole response.
- invariants: inspect prose outside fenced and inline Markdown code; scope
  Claude fingerprints to Claude; require compound high-confidence markers for
  terminal blocking; hold partial/standalone control markers while streaming;
  never advance a rollover offset after a held or blocked frame; never log or
  echo matched raw text, transcript paths, or provider-supplied block reasons.
- hook_boundary: transcript path/read/parse failures fail open so the provider
  is not trapped in a Stop-hook retry loop; a classified Claude completion
  returns only the static block decision/reason and enters neither prompt-ready,
  broadcast, nor hook-registry delivery. `stop_hook_active=true` bypasses the
  second inspection to bound the retry.
- tests: `cargo test invariant_4371 --lib`; raw JSONL fixture coverage crosses
  the real watcher parser and Discord formatter, while gateway seams pin both
  streaming rollover paths to safe bodies/no offset progression.
- related_issues: #4371.

### `discord_outbound`

- canonical_modules: `src/services/discord/outbound/{message,policy,result,decision,delivery,transport,send_to_agent,send_target,send_gate,send_api,manual_delivery,source_registry}.rs`
  (#1006 v3 domain types, pure planner, delivery implementation, shared
  transport/dedup primitives, and the #3038 send-to-agent/manual outbound
  dispatch surface).
- legacy_modules: none; `src/services/discord/outbound/legacy.rs` was removed
  in #2535.
- do_not_edit_without_migration_plan:
  - `src/services/discord/formatting.rs::send_long_message_raw` (line 1971,
    ordered-chunk continuation contract not yet modelled in v3).
  - `src/services/discord/outbound/delivery_record.rs` (1323 prod lines; +47 from
    #4046 S1r-1 adding the isolated `discord_fresh_send_records` path,
    Result-returning fresh-send fingerprint writer, and dedicated current-generation
    lookup so anchor-less sends cannot enter watcher suppression authority; +60
    from #4188 EOF-bound frontier guard — thread `current_transcript_eof` through
    the single durable-frontier funnel (`current_generation_durable_frontier_at`)
    + delivered-anchor / effective-committed-offset call-sites so a stale
    prior-generation frontier whose end exceeds the compacted transcript EOF is
    distrusted (fixes the delivered_frontier message-loss after `/compact`); +8
    from #4130 cfg(test) shadow_test_seam — per-thread override so default-OFF
    tests ignore developer-shell AGENTDESK_DELIVERY_RECORD_SHADOW; production
    paths untouched; durable delivery lease/frontier/owner-context sidecar, plus
    the #4081 bounded recent-content fingerprint guard; bugfix only until split
    under #3405).
  - `src/services/message_outbox.rs` is the PG-backed message outbox
    enqueue/claim/accounting surface. #4465 adds a deduplicated `held` staging
    state that workers cannot claim; callers activate it to `pending` only
    after their external authority check, or delete it when that check is stale;
    expired held rows are bounded by the shared outbox GC so a crash before the
    sidecar records the staged id cannot create permanent housekeeping residue.
    `src/services/message_outbox_recovery.rs`
    plus `message_outbox_recovery_support.rs` own exact-ID inspection and
    idempotent failed-row redrive (all below the giant-file threshold once
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

### `task_notification_card_authority`

- canonical_modules:
  - `src/services/discord/task_notification_delivery/{mod,store,gateway}.rs`
    owns semantic identity, PG lease/CAS state, stable Discord nonce, bot
    pinning, and classified create/edit/replacement delivery.
  - `src/services/discord/tui_prompt_relay/task_notification_prompt.rs` owns
    prompt observation/footer deferral; `session_relay_sink/task_notification_context.rs`
    owns card-before-answer promotion and exact reference selection.
  - `src/services/discord/gateway/outbound_messages.rs` adapts task-card
    create/edit operations to outbound v3. `gateway.rs` maps structured Discord
    errors while `outbound/transport.rs` owns the nonce-aware Serenity create
    boundary and enforces the create nonce.
- durable_state: PostgreSQL `task_notification_card_state` is cluster-shared
  authority. The process-local store is a test/non-PG fallback only.
- invariants: one logical event row and stable nonce; ambiguous creates retry
  that nonce within Discord's bounded replay window (the PG row/message id is
  the durable authority, not an indefinite Discord nonce guarantee); transient
  edits never repost; replacement requires structured Discord `404/10008`; a
  non-empty task response is sent only after card confirmation and references
  that card; watcher direct fallback consults the exact PG event/turn fence and
  stays blocked until the referenced response is confirmed and its commit-fence
  decision has run; footer eviction requires the exact terminal `tool_use_id`.
- tests: `task_notification_delivery/tests.rs`,
  `session_relay_sink/task_notification_context.rs`, and
  `placeholder_live_events/tests.rs`, plus
  `tmux_watcher/terminal_direct_fallback.rs` for the fail-closed retry gate.
  `just test` runs the non-PG task/card filters; `just test-postgres` runs the
  unique-winner and crash-window replay cases.
- related_issues: #4055, #3654, #4097.

### `policy_engine`

- canonical_modules: `src/engine/mod.rs` (driver) plus `src/engine/ops/*.rs`
  (per-domain op handlers). `src/pipeline.rs` (1383 lines, giant-file)
  composes the policy pipeline.
- legacy_modules: none — there is no parallel engine. The whole surface is
  pre-migration giant-file territory.
- do_not_edit_without_migration_plan:
  - `src/engine/mod.rs` (1278 lines, giant-file).
  - `src/engine/ops/db_ops.rs` (1212 lines, giant-file).
  - `src/engine/loader.rs` (1332 lines, giant-file) — engine loader / QuickJS
    validator surface; split before adding non-bugfix behavior.
  - `src/pipeline.rs` (1383 lines, giant-file).
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
  - `src/dispatch/dispatch_context.rs` (2817 lines).
  - `src/dispatch/dispatch_create.rs` (1334 lines).
  - `src/dispatch/dispatch_status.rs` (1445 lines).
  - `src/services/dispatches/outbox_route.rs` (1177 lines; +1 from #4055
    preserving the typed transient delivery result; +4 from #4486 typing the
    announce/notify bot identity via `UtilityBotRole::_.alias()` (mechanical,
    non-behavioral); route extraction
    orchestration surface from #1722, split before adding non-bugfix behavior).
  - `src/services/dispatches/discord_delivery/orchestration.rs` (1500 lines;
    +1 from #4055 preserving the typed transient delivery result;
    +4 from #4486 UtilityBotRole alias typing (mechanical, non-behavioral);
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
  parsing), `src/services/discord/inflight.rs` (state file facade/contract),
  `src/services/discord/inflight/removal.rs` (load-time prune + removal
  logging), `src/services/discord/inflight/clear_store/mod.rs` and
  `src/services/discord/inflight/clear_store/abandon.rs` (clear/abandon
  store-side CAS paths).
- legacy_modules: none — relay routes are being consolidated, not replaced.
- do_not_edit_without_migration_plan (giant-file):
  - `src/services/discord/watchers/lifecycle.rs` (2077 lines — canonical
    lifecycle extraction surface from #1435; split further before adding new
    lifecycle behavior; #3016 phase-5b2 dropped the `mailbox_finalize_owed`
    construction from the watcher-spawn handle; #3718 moved runtime mtime
    heartbeat timestamp selection into `watchers/lifecycle_decision.rs` and
    keeps lifecycle below its frozen ratchet; -6 from #3736 removing legacy
    remote-profile restore plumbing while remote SSH is disabled; ±0 from
    #3815 moving direct Codex TUI resume restore helpers into
    `watchers/codex_tui_restore.rs` while adding the restore branch; -207 from
    #3840 moving heartbeat/activity helpers into
    `watchers/lifecycle/activity.rs`; -32 from #3898 removing the false-positive
    "session ended … start a new session" tmux-death notice and its
    `should_send_session_ended_notice`/`session_ended_notice`/
    `TmuxDeathLifecycleDecision` plumbing; +25 from #4455 adding the explicit
    force-replace claim action used only when Codex rebind proves that a live
    same-output watcher still belongs to an earlier provider turn).
  - `src/services/discord/tmux.rs` (1621 lines; test-only #4253 wires the
    deterministic task-notification-kind disk-save/reload/restart roundtrip
    module, with no production-LoC or runtime behavior change; +11 from #4380 broadening the
    watcher-yield escape hatch (`watcher_should_yield_to_inflight_state`) to honour
    the `readopted_from_inflight` crash re-adopt marker via
    `crash_resume_guard::crash_readopt_live_relay_resume_required`, so a
    crash-recovered live turn resumes relay instead of yielding to the now-dead
    bridge (the escape-hatch body + tests; predicate lives in the non-giant
    `crash_resume_guard.rs`); -17 from #4151 removing
    `format_monitor_suppressed_body` — its only caller was the unreachable #1009
    MonitorAutoTurn suppressed-body replacement in tmux_watcher.rs (dead since
    #1708; #4144 r2 closed the None-kind Edit path); -8 from #4198 routing the restored-watcher release D-section through the shared `turn_finalizer::cleanup` helpers (`snapshot_role_override` / `clear_watchdog_and_kick_thread_parents_after_turn_release` / `remove_owned_role_override`); +32 from #4105 adding
    cross-turn restored-seed identity (`RestoredWatcherTurn.turn_identity` +
    `restored_seed_reassigned_to_different_turn`) so a long-lived watcher reused
    for a new turn drops the prior turn's stale `full_response` seed; +101 from #4106 adding
    `release_restored_watcher_active_turn_before_panel_edit` — hoists the
    identity-guarded mailbox release + `global_active` decrement + the
    finalizer's D-side channel cleanup ahead of the awaited status-panel edit so
    a same-channel follow-up racing the edit can no longer make the late
    finalizer identity-miss and permanently skip the decrement; the D-side
    role-override drop snapshots the owned value before any await and uses
    `remove_if` so a fresh counter-model follow-up inserting its own override
    during the release is not clobbered. The #4106r2 WARN-fix splits
    `finish_restored_watcher_active_turn` into a thin wrapper (pins
    `FinalizeContext::watcher()` for all legacy/recovery callers, unchanged
    signature) + a `_with_ctx` inner that takes an explicit context, so the
    post-early-release watcher site can route its now-DETERMINISTIC identity-guard
    miss through `watcher_after_pre_panel_release` and log at debug instead of
    spamming the wrong-turn WARN on every normal completion; -7 from #4048 S3 removing the
    restored-watcher direct queue-kickoff path in favor of the finalizer
    completion-event drain trigger; -12 from #4047 S2-b deleting
    the GateTimeout submit path and adding the shared bounded
    background-agent sniff wrapper; -6 from #3874 removing dead
    permanently-None `Option<&Db>` threading from tmux/outbound call paths,
    with no relay or delivery semantics change; +22 from #3871 persisting the
    streamed rollover-prefix ids through the watcher seed/restore + persist path
    so a terminal full-body fallback in a later iteration / after a restart still
    deletes them (cross-iteration durability of the dup-relay fix); +8 from #3886 hosting the
    `status_panel_timedout_reconcile` module decl + re-export so the placeholder
    sweeper can finalize a panel stuck at "진행 중" after a TimedOut completion
    gate (reconcile body lives in the new non-hot file); +14 from current inventory
    refresh after the relay split stack landed; after #2558 dead-code sweep;
    +0 from #4018 round-2 switching monitor auto-turn mailbox claims to the
    distinct `ActiveTurnKind::MonitorAutoTurn` marker so stale synthetic reclaim
    excludes live monitor relays while preserving background queue-yield behavior;
    +6 from #3818 sanitizing restored/orphan subagent-notification placeholders;
    +1 from #3384 restored-seed undelivered-body discard guard; +0 from #4455
    re-exporting the force-replace claim helper while its implementation stays
    in `watchers/lifecycle.rs`;
    +38 for suppressed-label noise, user report 2026-06-12: provider-aware
    status/footer stripping in the placeholder suppression decisions;
    -15 from #3717 footer-only placeholder target preservation plus status-strip
    helper extraction to `single_message_panel.rs`;
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
    -1 from #3038 S4 after routing the placeholder/status-panel cluster
    through `shared.ui`; +8 from #3533 ActiveBridgeTurnGuard restart-boundary
    preserve fix (no SUPPRESSED_INTERNAL_LABEL on an already-delivered duplicate);
    +1 from #3552 codex r2 (2057 -> 2058): the watcher pre-save
    `response_sent_offset_monotonic` severity emit added by #3552 codex r1 was
    REMOVED as a TOCTOU-racy duplicate — it judged WARN/ERROR against an unlocked
    `load_inflight_state` snapshot while the immediately-following
    `save_inflight_state` re-judges the row under its own lock and emits at the
    correct severity atomically with the skip/persist decision. The lock-atomic
    inflight save-path is now the single authoritative emitter of this invariant
    (`persist_watcher_stream_progress` always reaches that save), removing the
    `record_watcher_invariant_with_severity` / `persist_watcher_response_sent_offset_severity`
    helpers and the `persist_watcher_stream_progress_with_authority` test seam;
    net +1 is the replacement doc-comment. The DEBUG-only
    `debug_assert!(monotonic_offset)` tripwire stays;
    -9 from #3558 (2058 -> 2049): `persist_watcher_stream_progress` no longer
    runs an unlocked `load_inflight_state` -> mutate -> `save_inflight_state`
    (the offset-monotonic TOCTOU); it now builds a `WatcherStreamProgressPatch`
    and delegates the read-modify-write to the single-flock
    `inflight::persist_watcher_stream_progress_locked` (which preserves the
    non-owned `last_offset` from the in-lock disk reload). The duplicated
    in-bounds/monotonic local mutation block collapsed into the helper, and the
    function gained a `require_identity: Option<&InflightTurnIdentity>` param so a
    late-frame fresh row B is rejected; -576 from #3841 extracting placeholder
    suppression helpers to `tmux_placeholder_suppression/`;
    still giant-file territory).
  - `src/services/discord/tmux_watcher.rs` (5218 production lines; #4229 S4
    moved the turn stream collector (seed restore/first parse-forward/monitor
    auto-turn claim/active read-parse loop) verbatim to
    `tmux_watcher/turn_stream_collector.rs` (1158 production lines), ratcheting
    the root down after behavior-preserving decompose; #4229 S3 moved the
    throttled streaming status tick (orphan reclaim, streaming
    suppression, status-panel create/bind, rollover, re-anchor, placeholder
    edit) verbatim to `tmux_watcher/streaming_status_tick.rs`, ratcheting the
    root down after behavior-preserving decompose; #4229 S2
    moved the loop poll prologue (heartbeat/pause/rotation/initial-read/empty-poll/post-terminal suppression)
    verbatim to the non-giant `tmux_watcher/loop_poll_prologue.rs` child module,
    ratcheting the root down after behavior-preserving decompose; #4170 gated
    rollover recovery mirror update on edit success (+2); #4229 S1 moved the
    pause/epoch-discard + prompt-too-long/auth-expired/provider-overload terminal
    exits verbatim to the non-giant `tmux_watcher/terminal_abort_exits.rs` child
    module, ratcheting the root down after behavior-preserving decompose; #4049
    S4-b1 inventory sync recorded the hotfile count; #4081
    round2 moved `commit_watcher_direct_terminal_session_idle` verbatim into
    `tmux_watcher/liveness.rs` and kept only thin duplicate-guard/long-body
    wiring in the root loop; -18 from
    #3998 S1-f2 retiring the watcher terminal controller rollout flag and
    collapsing the cutover call to structural inputs only; +59 from #4019 R2
    identity-guarded watcher exits — the real stall/auth/overload exit
    release helper lives in `tmux_watcher/stall_exit.rs`, while the root carries
    only pinned-snapshot capture, three helper calls, monitor-token release before
    labelled watcher-loop breaks, and the 0-id finalize submit predicate; +33 from
    #3805 P2 PR-D review fixes — watcher re-anchor now reloads the current inflight row
    and calls the sibling watcher-ownership gate so Managed bridge-owned turns are
    never watcher-reanchored; watcher panel sends are durably pre-registered in
    the orphan store and removed only after bind/delete makes them safe; the
    `bind_status_panel` result supplies the persisted generation from the in-lock
    bump; +38 from #3805 P2 PR-D (watcher rollover re-anchor) — after a mid-turn
    answer rollover, re-anchor the two-message status panel BELOW the new tail
    answer; the giant gains only a per-interval rolled-over local + one gated
    re-anchor call after the rollover loop, all send/rebind/retire logic in the
    non-giant `tmux_watcher/two_message_panel.rs` (atomic `bind_status_panel`
    expected-old-panel + epoch-bump rebind), gated on the default-OFF
    `two_message_panel_enabled` → OFF byte-identical; +67 from #3805
    P2 PR-C (watcher two-message creation-order parity) — an answer-first
    subcondition on the existing panel-creation gate (defer the panel until the
    answer placeholder exists so it lands BELOW the answer), a per-turn
    `this_turn_status_panel_generation` local seeded from the inflight snapshot,
    an in-lock `bind_status_panel` generation bump on fresh binds, and the completion
    `generation_superseded` compute + arg; the pure gate/generation/completion
    predicates and the panel-completion tail (moved verbatim out of the 700-capped
    `single_message_footer.rs`) live in the new non-giant
    `tmux_watcher/two_message_panel.rs`, gated on the default-OFF
    `two_message_panel_enabled` → OFF byte-identical; +14 from #3805
    P1 (footer re-anchor) capturing the tail continuation chunk (id + text) in the
    terminal in-place edit arm and re-anchoring the completion footer onto it so a
    2000+ char answer no longer strands the footer in a middle chunk — the anchor
    struct, out-param plumbing, selection helper and its tests live in the
    non-giant `formatting.rs`; +10 from #3558
    (codex review follow-up) routing the two remaining session-bound-relay-success
    sites — which still did an unlocked `load_inflight_state` -> mutate ->
    `save_inflight_state` (re-writing a stale `last_offset`/`response_sent_offset`)
    — through the new single-flock `inflight::persist_watcher_relay_watermark_locked`
    helper, gated on the captured `inflight_identity_before_relay`; +2 from #3558
    threading the captured `turn_identity_for_panel.as_ref()` into both
    `persist_watcher_stream_progress` streaming call sites so the new single-flock
    RMW helper can reject a write onto a fresh row B; +1 from #3534
    gating the post-terminal-success continuation flush on
    `new_output_observed` (`current_offset > data_start_offset`) so a zero-width
    re-entry never re-relays an already-delivered carried body as a NEW message;
    was 6922 after the #3479
    SPC-shadow removal deleted the dead `StatusPanelController` watcher
    shadow-parity calls — 2x `shadow_adopt_liveness_reacquired_panel` +
    `assert_watcher_create_parity`; legacy reacquire/create/edit IO unchanged —
    and after #3479 item-2 moved the provider-session persistence cluster
    (`resolve_persistable_provider_session_id`,
    `persist_watcher_provider_session_id`) verbatim into the `tmux_watcher/`
    child module `provider_session_persistence.rs` (~101);
    was 7022 after #3479 item-2 moved the orphan status-panel cleanup cluster
    (`cleanup_orphan_external_input_status_panel`,
    `complete_watcher_status_panel_v2`,
    `refresh_watcher_session_panel_from_lifecycle`) verbatim into the
    `tmux_watcher/` child module `orphan_status_panel_cleanup.rs` (~210);
    was 7241 after #3479 Phase-1 rank-2, 7485 after #3479 Phase-1 rank-1 and
    8122 after #3038 tmux_watcher S1 moved top-level decision
    clusters A/B/C/E/F/I/J/K
    into `tmux_watcher/` child modules: `liveness.rs` (301),
    `panel_decisions.rs` (372), `prompt_observe.rs` (109),
    `turn_identity.rs` (327), `completion_gate.rs` (307), and
    `commit_decisions.rs` (140); plus the #3479 rank-2 child modules
    `utf8_chunk_decoder.rs` (88) and `terminal_readiness.rs` (214). #3016 phase-5b2 removed the `mailbox_finalize_owed`
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
    `watcher_session_ready_for_input` — the SAME `FallbackPaneReadiness` route the
    5a far-backstop uses for `Unknown` (its constructor enforces
    `pane_ready_fallback_allowed`)),
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
    helper additionally fires the Claude-only AgentDesk-side `/compact` injection
    when exact token usage crosses the model-aware threshold formed from
    `context_compact_percent_claude` and
    `context_compact_lower_bound_tokens` (default 300,000). Claude launch scripts
    use `CLAUDE_CODE_AUTO_COMPACT_WINDOW` only when that absolute window is valid;
    see `src/services/claude_compact_trigger.rs` and
    `src/services/claude_compact_context.rs`.
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
    directory module (#3303 decomposition: `store.rs` durable marker/tombstone
    I/O and `deferred_claim.rs` own-identity pin + uncapped-while-pinned
    disposition; #4175 split `tombstone.rs` record/cover entry points, `drain.rs`
    terminal-commit + visible-completion consumption, and `sweep.rs` TTL sweep
    paths while keeping the shared cover gate in `mod.rs`); the #3296
    additions are offset in-file by compressing the #3016-S3 finalize/TOCTOU
    and #1670/#1708 decoupling comment blocks. #3038 tmux_watcher S1 then lowered
    the ratchet baseline to 8122 by moving pure decision clusters into the
    capped `tmux_watcher/` child modules. #3479 Phase-1 rank-1 then lowered the
    live root to 7485 (-738) by extracting the supervisor relay-forward +
    session-bound terminal-ACK cluster verbatim (pure move, zero logic change)
    into two cohesive `tmux_watcher/` child modules — `supervisor_relay.rs` (the
    forward half + the shared `SessionBoundRelayAckTarget`/`SupervisorRelayForward`
    types + `watcher_terminal_commit_fence`/`terminal_event_consumed_offset`) and
    `session_bound_ack.rs` (the `SessionBoundRelayAckOutcome` fold, per-sequence
    ACK snapshot resolvers, the watcher-direct-send gate,
    `WatcherTerminalResendAction` + the in-flight-sink-marker gate, the
    `RelaySlotGuard` emission-slot RAII guard, and the ACK delivery wait); split
    into two files only to keep each within the `tmux_watcher/**` 700-line
    namespace cap, with the moved unit tests in sibling `supervisor_relay_tests.rs`
    / `session_bound_ack_tests.rs`. The giant ratchet baseline was lowered
    8223 -> 7485 to lock in the shrink (zero logic change). #3479 Phase-1 rank-2
    then lowered the live root 7485 -> 7241 (-244) by extracting two more cohesive
    PURE clusters verbatim (pure move, zero logic change) into `tmux_watcher/`
    child modules — `utf8_chunk_decoder.rs` (the `Utf8ChunkDecoder` +
    `DecodedUtf8Chunk` streaming UTF-8 chunk decoder that buffers a split trailing
    multibyte scalar across read boundaries) and `terminal_readiness.rs` (the
    synchronous terminal-readiness / inflight-classification predicates —
    `adopt_watcher_terminal_message_ids_from_inflight`,
    `watcher_inflight_represents_external_input`/`_is_panel_eligible`/
    `_needs_anchor_lifecycle_cleanup`, `watcher_direct_terminal_should_commit_session_idle`,
    `watcher_terminal_token_update_status`, the JSONL `ready_for_input` sentinel
    probes, and the pure `discard_watcher_pending_buffer_after_suppressed_turn`
    reconciler); the async, `shared`-touching `commit_watcher_direct_terminal_session_idle`
    that sits BETWEEN the two readiness clusters deliberately STAYS in the root.
    Moved unit tests live in sibling `utf8_chunk_decoder_tests.rs` /
    `terminal_readiness_tests.rs`; both child modules sit under the
    `tmux_watcher/**` 700-line namespace cap.
  - `src/services/discord/tui_prompt_relay.rs` (no longer a registered giant as
    of #3833; external-input ownership, synthetic-start, and Claude idle
    tail/runtime/bridge glue moved into `tui_prompt_relay/` child modules while
    the #4018 compact-resume stale-mailbox follow-up stayed in those child
    modules: `tui_prompt_relay/claude_idle_bridge.rs` (662 prod LoC),
    `tui_prompt_relay/synthetic_start.rs` (1039 prod LoC; crossed the giant
    threshold in #4019 R2 to route idle-tail synthetic release through the
    finalizer, add session-key guarded cleanup, and preserve the global_active
    1:1 mailbox activation invariant under adoption; bugfix only until split),
    and `tui_prompt_relay/synthetic_start/stale_reclaim.rs` (225 prod LoC);
    worker-local relay lifecycle only, no PG lease/schema. The parent
    spawn/provider wiring surface stays preserved. Historical context:
    #3296
  - `src/services/discord/tui_prompt_relay/synthetic_start.rs` (1039 prod LoC;
    synthetic TUI-direct claim/adoption and idle-tail cleanup surface. Crossed
    the giant threshold in #4019 R2 when idle-tail cleanup moved from direct
    mailbox finish/counter decrement to finalizer authority, gained session-key
    guarded release, and stopped incrementing `global_active` when merely
    adopting an already-active mailbox. Bugfix only until split).
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
    `tui_task_card.rs` module now hosts only card render/parse/sanitizer logic;
    #4055 moved its process-local delivery/store authority into the durable
    `task_notification_delivery/` siblings, shrinking it to 818 prod LoC and
    removing it from the giant-file registry. +48 from #3075 codex P1
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
    the frozen baseline 5438 -> 5429 (-9, locks in the win). #3479 rank-5
    (behavior-preserving extraction): the pure injected-prompt
    classification/formatting cluster (`InjectedPromptClass` + its 3 predicates,
    `classify_injected_prompt`, the `is_*_prompt` / `is_start_anchored_*`
    classifiers, the `strip_leading_*` / `normalize_*` parsers,
    `slash_command_control_kind`, the `format_ssh_direct_prompt_notification` /
    `format_slash_command_control_note` / `format_system_continuation_note`
    formatters + their `extract_loop_body` / `format_count_with_commas` /
    `sanitize_inline_code` / `should_suppress_local_only_kind_note_after_continuation`
    helpers) moved verbatim to the capped `tui_prompt_relay/injected_prompt_policy.rs`
    sibling (318 prod LoC, below the giant threshold); names are re-imported via
    `use self::injected_prompt_policy::{…}` so call sites stay byte-identical and
    the stateful dedupe/bridge helpers (`slash_command_control_turn_is_first_sighting`,
    `record_system_continuation_note_rendered`,
    `local_only_kind_note_suppressed_by_recent_continuation`,
    `bridge_task_notification_to_live_panel`, `is_local_only_slash_command_prompt`)
    plus all `#[cfg(test)]` coverage stay in this root. Frozen baseline 5434 -> 5142
    (-292, locks in the shrink; zero logic change). #3479 rank-10 (behavior-preserving
    extraction): the pure transcript/rollout prompt scanners (the
    `ClaudeIdleTranscriptScan` / `CodexIdleRolloutScan` enums, the three
    `scan_*` byte-stream parsers, and the two `*_idle_prompt_observation_should_tail_response`
    predicates) moved verbatim to `tui_prompt_relay/idle_transcript_scan.rs`, and the
    Discord-IO/`SharedData`-coupled Claude TUI binding rehydration + dead/orphaned-session
    eviction pass (`rehydrate_existing_claude_tui_bindings`,
    `evict_dead_orphaned_claude_tui_mirrors`, `claude_tui_session_is_dead_orphaned`,
    `pane_is_confirmed_dead_orphaned`, `rehydrated_claude_tui_binding_for_tmux_session`)
    moved to `tui_prompt_relay/rehydration.rs` (deps reached via `use super::*;`,
    names re-imported so the call sites + tests stay byte-identical). The sibling
    helpers shared with non-rehydration code (`claude_tui_runtime_binding_matches_launch`,
    `resolve_rehydrated_claude_tmux_channel_id`, `claude_tui_rehydrate_start_offset`, the
    `DEAD_ORPHANED_PANE_PROBE_*` consts) STAY in this root (`parse_claude_tui_launch_script`
    + the `ClaudeTuiLaunchInfo` struct later moved to `tui_prompt_relay/launch_script.rs`
    in #3479 launch-script, see below). Frozen baseline 5142 ->
    4630 (-512, locks in the shrink; zero logic change). #3479 item-2
    (behavior-preserving extraction): the live-relay TUI-direct prompt anchor
    COMPLETION lifecycle (`⏳ → ✅`) cluster — `should_complete_tui_direct_anchor_lifecycle`,
    the `DeferredAnchorCompletionDrain` enum + `decide_deferred_anchor_completion_drain`
    drain decision, `complete_tui_direct_prompt_anchor_lifecycle_if_present`,
    `pinned_anchor_cleanup_target`, and `complete_tui_direct_anchor_lifecycle_for_inflight` —
    moved verbatim to `tui_prompt_relay/anchor_completion.rs` (deps reached via
    `use super::*;`, only `super::formatting` becomes `super::super::formatting`).
    The three helpers reached from sibling discord modules (`tmux_watcher.rs`,
    `recovery_engine.rs`) are re-exported at `pub(in crate::services::discord)`;
    the relay-internal drain decision/enum are re-imported privately, so the call
    sites + tests stay byte-identical. Frozen baseline 4630 -> 4458 (-172, locks in
    the shrink; zero logic change). +9 from #3540 root-prevention: the
    idle-transcript scanner identity-dedup threading — the resolved `entry_id`
    passed inline across the scan -> observe seam plus the `SuppressedReplayedEntry`
    handling that stops the scanner re-claiming an already-processed prompt across a
    watermark reset — is inline integration code on the scan/observe call path (not
    a self-contained helper) and the sibling `idle_transcript_scan.rs` is itself
    giant-capped, so the ratchet baseline was RAISED 4301 -> 4310 (a deliberate,
    reviewable admission per the baseline header) rather than split.
  - `src/services/discord/tui_direct_abort_marker/{mod,tombstone,drain,sweep}.rs`
    (838 / 70 / 262 / 177 production lines after #4175; formerly
    `tui_direct_abort_marker/mod.rs` at 1304 production lines after #4206).
    The facade keeps the shared commit-cover gate and Abort disposition helpers,
    `tombstone.rs` owns record/cover entry points, `drain.rs` owns terminal
    commit + visible-completion consumption, and `sweep.rs` owns the TTL sweep.
    All files are below the giant-file threshold; the #4175 registry entry is
    retired.
  - `src/services/discord/tui_direct_pending_start.rs` (1125 production lines;
    the deferred TUI-direct synthetic turn-start path — the pending-start claim
    queue, the no-evict promote of a stalled inflight, and the deferred-claim
    owner handoff. #3540 added the B′ "no-evict promote" path (a stalled inflight
    is safely promoted off the pending-start queue instead of evicted) plus its
    regression tests, which pushed the production surface over the 1000-line
    giant-file threshold, so this file is now a registered giant. Bugfix /
    queue-safety only; split before adding new pending-start behavior).
  - `src/services/discord/tmux_placeholder_suppression/{mod,evidence,ops}.rs`
    (348 / 259 / 584 production lines after #4176; formerly
    `tmux_placeholder_suppression.rs` at 1092 production lines. The facade keeps
    the pure placeholder-suppression decision core, `evidence.rs` owns frontier /
    proof / EOF helpers, and `ops.rs` owns Discord edit/delete cleanup operations.
    All three files are below the giant-file threshold).
  - `src/services/discord/tui_prompt_relay/injected_prompt_policy.rs` (318 prod
    lines; #3479 rank-5: pure injected-prompt classification + formatting policy
    extracted verbatim from `tui_prompt_relay.rs` — no `shared.`/`http.`/async-IO
    coupling, all items `pub(super)` and re-imported by the parent; below the
    giant-file threshold).
  - `src/services/discord/tui_prompt_relay/idle_transcript_scan.rs` (278 prod
    lines; #3479 rank-10: pure Claude/Codex transcript+rollout prompt scanners
    extracted verbatim from `tui_prompt_relay.rs` — no `shared.`/`http.`/async-IO
    coupling, all items `pub(super)` and re-imported by the parent; below the
    giant-file threshold).
  - `src/services/discord/tui_prompt_relay/rehydration.rs` (648 prod lines; #3479
    rank-10: the Discord-IO/`SharedData`-coupled Claude TUI binding rehydration +
    dead/orphaned-session eviction pass extracted from `tui_prompt_relay.rs`. #3711
    extends the same rehydrate surface to Codex TUI restart recovery: persist/use
    rollout markers, reject already-claimed marker paths, and allow markerless
    cwd-based fallback only when exactly one live markerless Codex TUI session
    owns that cwd. This file is still below the giant-file threshold, but split
    before adding another provider/recovery policy cluster).
  - `src/services/discord/tui_prompt_relay/anchor_completion.rs` (213 prod lines;
    #3479 item-2: the live-relay TUI-direct prompt anchor COMPLETION lifecycle
    (`⏳ → ✅`) — the visibility gate, the deferred `⏳`-completion drain decision,
    and the reaction-swap completers (shared-slot + pinned-injected-message paths)
    — extracted verbatim from `tui_prompt_relay.rs`. Deps reached via
    `use super::*;` (only `super::formatting` becomes `super::super::formatting`);
    the three externally-called helpers are `pub(in crate::services::discord)` and
    re-exported by the parent, the relay-internal drain decision/enum stay
    `pub(super)`; below the giant-file threshold).
  - `src/services/discord/tui_prompt_relay/launch_script.rs` (107 prod lines;
    #3479 launch-script: the Claude TUI launch-*script* parsing helpers — the
    parsed `ClaudeTuiLaunchInfo` record, `parse_claude_tui_launch_script` +
    `parse_claude_tui_launch_script_content`, and the minimal single-quote
    `shell_words_from_line` shell-word splitter — extracted verbatim from
    `tui_prompt_relay.rs` (all `#[cfg(unix)]`). Deps reached via `use super::*;`;
    `parse_claude_tui_launch_script` + the `ClaudeTuiLaunchInfo` record are `pub(super)`
    and `parse_claude_tui_launch_script` is re-imported by the parent so the
    `claude_tui_launch_context` caller and the sibling `rehydration` module keep
    byte-identical call sites, while the module-internal
    `parse_claude_tui_launch_script_content` + `shell_words_from_line` stay private;
    below the giant-file threshold).
  - `src/services/discord/tui_prompt_relay/idle_offset_resolution.rs` (100 prod
    lines; #3479: the idle-tail transcript start-offset resolution helpers — the
    #3154 timestamp-anchor choke point `resolve_idle_tail_start_offset`, the
    `claude_idle_response_start_offset_after_timestamp` timestamp scan, the
    stale-high `normalize_transcript_fallback_offset` guard, and the #3183
    `clamp_idle_tail_start_offset_to_committed` committed-offset clamp — extracted
    verbatim from `tui_prompt_relay.rs` (all `#[cfg(unix)]`). Deps reached via
    `use super::*;`; `resolve_idle_tail_start_offset` +
    `clamp_idle_tail_start_offset_to_committed` are `pub(super)` re-imported by the
    parent's prod call sites, `claude_idle_response_start_offset_after_timestamp`
    is `pub(super)` re-imported only under `#[cfg(all(unix, test))]` (its prod
    callers are now child-internal), and the module-internal
    `normalize_transcript_fallback_offset` stays private; below the giant-file
    threshold).
  - `src/services/discord/idle_recap.rs` (idle-recap card compose/post/clear
    surface; #3479 extracted the scrollback/summarizer and
    token-context-display clusters into the two submodules below, but #4079's
    recap UX/lifecycle fixes pushed the file back over the giant-file threshold
    and its registered ratchet now tracks 1378 prod lines. Remaining surface:
    the snapshot/compose/post/clear/CAS lifecycle, per-channel recap
    superseding via `sessions.idle_recap_message_id`, routine-session
    suppression, the recap button plan including "맥락 압축", the
    `channel_has_active_turn` mailbox/inflight probe, and the
    `post_recheck_action` seam that skips/undoes a recap post when a turn raced
    the compose window. `src/services/discord/idle_recap_interaction.rs` owns
    the corresponding button dispatch, suggested-reply enqueue, and `/compact`
    enqueue response copy).
  - `src/services/discord/idle_recap/scrollback.rs` (#3479 scrollback: the tmux
    `capture-pane` tail capture, the `claude-e` transcript-tail fallback
    (`capture_transcript_scrollback` + the unit-testable `extract_transcript_tail_text`
    / `parse_transcript_line_text` workers), and the Haiku `summarize_with_haiku`
    call plus #4079's user-perspective suggested-reply prompt contract. Deps
    reached via `use super::*;`;
    `capture_tmux_scrollback` / `capture_transcript_scrollback` / `summarize_with_haiku`
    are re-exported by the parent so the `server::routes::idle_recap` caller keeps
    byte-identical `idle_recap::<fn>` call sites, while the parsing workers stay
    `pub(super)` for the parent's in-file tests; below the giant-file threshold).
  - `src/services/discord/idle_recap/context_display.rs` (#3479 context display:
    the live/latest-turn/session token selection state machine
    (`select_recap_context` + `RecapContextDisplay`), the freshness and
    provider-match guards, and the `format_token_count` / `format_korean_duration`
    formatters — extracted verbatim from `idle_recap.rs`. Deps reached via
    `use super::*;`; `select_recap_context` / `RecapContextDisplay` /
    `format_token_count` / `format_korean_duration` / `provider_session_ids` /
    `normalized_text` are `pub(super)` and re-imported by the parent so
    `compose_recap_header` and `attach_live_context_usage` keep byte-identical
    call sites, while the rest of the selection helpers stay module-private;
    below the giant-file threshold).
  - `src/services/codex_tmux_wrapper.rs` (1403 lines; +30 from #3557 Codex review: cap the idle recv_timeout by the remaining hard-ceiling budget (+boundary tests); Codex tmux wrapper JSON
    event parser and relay bridge for native Codex session events — bugfix only
    outside an extraction plan; +65 from #3275: capture per-call
    `token_count.info.last_token_usage` and re-emit it as a Claude-compatible
    nested `usage` on the success result frame so watcher-owned codex turns
    persist token telemetry — never the session-cumulative
    `info.total_token_usage`; +66 from #3557 (B): bound the post-first-event
    `recv()` with an idle recv-timeout + absolute per-turn ceiling so a hung
    Codex process that stops emitting JSON without exiting is killed and rejoins
    the error path instead of looking "busy" to the watcher indefinitely — the
    13125s outlier source; +18 from #3557 (B) codex r2: this path runs `codex
    exec` over a pipe (no tmux pane to `capture-pane`), so the JSON stream is the
    only liveness signal — raised the generous idle default 1800s -> 3600s so a
    normal long SILENT tool run (e.g. a big build) is never mistaken for an idle
    hang, with the 4h hard ceiling as the real backstop, and noted the limitation
    in the idle-kill error message + a delayed-event test).
  - `src/services/tui_prompt_dedupe.rs` (2105 lines; -41 from #4591 R4: remove
    raw/envelope time-pair state so local slash-control representations may
    duplicate rather than swallowing a later human command; local stable entry
    IDs are now recorded only after the relay confirms its Discord session note,
    while generic direct-input identity replay behavior remains eager; +4 from #4295: retain the
    stable provider source-event id on observed TUI prompts so terminal-card
    delivery can reject an exact post-compaction replay durably; +117 from #4423: adopt Claude's
    actual continuation UUID from a hook payload only through the registered
    launch UUID and a real sibling transcript; retain the launch UUID as the
    live hook-routing identity, register the payload alias, require newer mtime
    for later hops, reset the cursor only on a genuine transition, and reject
    delayed rewind payloads; +2 from #4091 r6 mandatory env-first lock-order comment at TEST_LOCK; +23 from #4091 r3
    refresh_runtime_binding_activity so live transcript activity extends the
    24h binding-mapping TTL even when the relay offset never advances (the
    exact relay-dead state the anchor protects); shared TUI prompt
    fingerprinting/dedupe state for hook and rollout relay paths, bugfix only
    outside an extraction plan; +60 from #3956: add the
    `touch_prompt_anchor_on_activity` refresh primitive — an ANCHOR-ONLY single-map
    op (re-stamp an EXISTING submit anchor's `recorded_at` on observed streaming
    activity, channel-scoped, never creates an anchor; checks the 4h ceiling INLINE
    and evicts a >4h-dead anchor rather than calling the global `purge_expired`, so
    it never scans or mutates the `relayed_entry_ids_by_tmux` ledger or any other
    dedupe map on the per-chunk hot path) so a turn streaming continuously past
    `PROMPT_ANCHOR_SUBMIT_TTL` (4h) keeps a live anchor for the #3885 same-input
    follow-up-requeue peek — closes the deferred #3885 residual; the watcher calls
    it from its per-pane streaming-observation path, plus three regression tests;
    +55 from #3885 follow-up: decouple the PROMPT
    ANCHOR purge into `PROMPT_ANCHOR_SUBMIT_TTL` (4h) so a routine 30-60min
    streaming build/agent turn's anchor is no longer purged mid-stream — the
    bridge same-input correlation peek (and the watcher ⏳→✅ response match) would
    otherwise resolve `None` and re-fire the #3885 no-response duplicate; the
    `relayed_entry_ids_by_tmux` ledger deliberately keeps the 30min
    `PROMPT_ANCHOR_TTL` so the #3459/#3303 missed-prompt guard is untouched, plus
    a `record_prompt_anchor_aged_for_tests` helper + TTL-boundary tests; +20 from
    #3676: Codex rollout user prompts now
    prefer the stable message entry id when present so Codex TUI direct prompt
    relay can use the same entry-identity replay suppression as Claude while
    distinct Codex message ids still publish distinct direct prompts; +176
    from #3540: stable JSONL entry-identity
    (`uuid`) dedup — `extract_claude_transcript_user_prompt_with_entry_id`
    returns `(prompt, Option<uuid>)`, a `relayed_entry_ids_by_tmux` ledger
    (PROMPT_ANCHOR_TTL-purged, ring-capped) + `PromptObservation::SuppressedReplayedEntry`,
    and `observe_prompt_by_tmux_with_entry_id_at` suppress an already-relayed entry
    re-encountered after a relay-watermark reset / jsonl head rotation BY IDENTITY;
    +84 from #3818: user-prefixed subagent notification machine events now bypass
    the Discord self-relay duplicate filter after provider-reuse/TUI-chrome
    wrapper peeling so the sanitizer can render a card instead of raw XML
    (never by inflight/EOF observation), so the idle-transcript scanner cannot mint
    a phantom synthetic inflight; a genuinely new prompt carries a new uuid and is
    never suppressed (#3459/#3303 missed-prompt guard); +37 from #3527: `is_discord_relayed_user_prompt`
    skips re-observed `[User: … (ID: …)]` Discord-relay lines (whole-string scan —
    context like `[External Recall]` may precede the marker and the legacy pane
    observer collapses blocks mid-line) in the observation candidate filter so a
    quiescence-timeout re-observation never mints a spurious synthetic turn (notice
    + orphan panel); +88 from #3041 P1-4 codex: a per-record
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
    -8 from #3695: moved the synthetic TUI user prompt filter into
    `tui_prompt_dedupe/synthetic_prompt.rs` while adding exact Claude interrupt
    marker suppression for stop-control transcript envelopes; +62 from #3304:
    slash-command canonical prompt keys for `<command-*>` XML vs
    `/command args` dedupe, plus focused loop skill-expansion regressions).
  - `src/services/discord/relay_recovery.rs` (1131 production lines; +242 from
    #4030 fix-round review hardening: destructive watcher-owner Cancel now routes
    through the shared death-evidence gate (`destructive_cancel_gate.rs`), pins
    decision-time turn/mailbox/tmux identity before apply-time finalizer submit,
    and covers frozen nonzero-frontier / empty-capture variants. This admission
    is bugfix-only for PR #4035; further recovery policy expansion should extract
    decision/apply helpers instead of growing this file.)
  - `src/services/discord/recovery_engine/restore_inflight.rs` (2335 production
    lines; tracked #3834 follow-up giant after the r2 behavior-preserving split.
    Owns the restart-path inflight scan: retry-aware tmux liveness probes,
    `finish_recovered_turn_mailbox`, live output-path detection,
    `restore_inflight_turns`, watcher reattach, and the session-died generic
    retry handoff. #4111's codex rollout fallback output-path persist site moved
    here verbatim; the #4117 session-retry signal path moved here while the
    recovery-context take helper remains in `turn_bridge/recovery_text.rs`.
    Further work should split internal scan/session-retry helpers out of this
    child before adding behavior.)
  - `src/services/discord/recovery_engine.rs` (417 prod lines after #3834 r2;
    no longer a prod giant. The facade keeps module declarations, re-imports, and
    the shared `RecoveryPhase` / `RebindOutcome` / `RebindError` types.
    `relay_recovered_terminal_text_to_placeholder`, `finish_recovered_turn_mailbox`,
    and `restore_inflight_turns` remain available through the same
    `recovery_engine::...` paths.)
  - `src/services/discord/recovery_engine/completion_delivery.rs` (sub-1000;
    behavior-preserving #3834 r2 extraction of recovery terminal relay,
    visible completion/status-panel completion helpers, and their tests.)
  - `src/services/discord/recovery_engine/manual_rebind/mod.rs` (995 prod lines
    after #4455; remains below the giant threshold. Keeps the manual rebind entrypoints,
    rollback carrier, session refresh, active-turn re-registration hook, and
    watcher claim/spawn path. #4465's durable automatic lane performs the
    blocking exact-episode adoption on `spawn_blocking`, retains that same
    canonical flock through footer/session/mailbox/finalizer/runtime-binding
    mutation and watcher claim/spawn, and commits the episode-scoped readoption
    marker plus in-memory ledger before releasing authority. The
    `episode_handoff.rs` child never waits for a flock while holding
    `shared.core`. #4455 keeps the crossed-turn watcher selection in the
    30-line `watcher_claim.rs` child so the parent stays below the threshold.
    `src/services/discord/recovery_engine/manual_rebind/codex_tui_replay.rs`
    (363 prod lines) owns the Codex-TUI replay/resume helper cluster, and
    `src/services/discord/recovery_engine/manual_rebind/adoption.rs` (95 prod
    lines) owns transcript-adoption offset and binding decisions. The retired
    `manual_rebind.rs` giant registration was removed from
    scripts/giant_file_registry.toml.)
  - `src/services/discord/recovery_engine/rebind_runtime.rs` (980 prod lines
    after #4455; below the giant threshold) owns provider runtime resolution
    and normalized Codex relay conversion. Its 89-line
    `rebind_runtime/codex_relay_generation.rs` child owns the per-path
    generation registry, prepare/truncate gate, and fenced JSONL write.
  - `src/services/discord/health.rs` (417 prod lines after the #3038 Phase A
    directory decomposition; module root keeps the `HealthRegistry` core +
    re-export surface, and the former monolith body lives in flat
    `health/` submodules, all sub-1000 prod LoC: `runtime_resolve.rs` (321),
    `headless_turn.rs` (297), `relay_auto_heal.rs` (123),
    `stall_liveness.rs` (527). Previously 2240 after the #3034 dead-code
    sweep, 2292 after #3038 send-to-agent dispatch extraction to
    `outbound/send_to_agent.rs`, then S1 moved the manual outbound dispatch
    children (`send_target`, `send_gate`, `send_api`, `manual_delivery`) to
    `outbound/` while preserving the `health::` re-export API; #1879
    snapshot/mailbox extraction, and #3082 answer-flush-barrier field).
  - `src/services/discord/health/recovery.rs` (2566 lines; +37 from #4535 restricting the provider-known hard-stop finish — both the primary path and the global-handle fallback — to only the mailbox-owning sibling runtime (with a WARN when an observed-but-unresolved actor is declined) (ownership resolved via `local_mailbox_ownership`) so a non-owning/unresolved-ownership hard-stop no longer finishes another runtime's mailbox; +1 from #4465 mapping an exact-episode rebind CAS miss to HTTP 409; #4460 follow-up extracted non-destructive branch-4 paging into `health/recovery/stall_alert.rs` (174 prod lines): alerts use canonical `channel:<id>` plus the real provider session identity so Claude/Codex DMs select their provider bot while public channels keep `notify`, owner 0 and the TUI synthetic owner 1 never render mentions, and the production liveness decision suppresses pre-backstop producer-live pages while genuine stalls still page; the parent branch never cleans/cancels/deletes turn authority. The original #4460 change removed the branch-4 "desynced force-clean" execution and dropped `preserve_resume_selector_on_force_clean` plus the test-only force-clean hook seam; #4423 moved the rebind request parser into `health/rebind_request.rs`; +26 from #4198 snapshotting the owned role override before the yielding D-section cleanup and replacing the unconditional `role_overrides.remove` with the shared `remove_owned_role_override` guarded remove at both recovery bundles; +7 from #4178 computing `capture_advancing` via `stall_liveness::stall_watchdog_capture_offset_advancing` in `run_stall_watchdog_pass` and threading it into `stall_watchdog_should_force_clean` so a live-but-relay-stalled turn is not force-cleaned; +28 from #4111 r9 capturing the force-clean repair boundary before the watcher snapshot and threading it into the start-bounded stale-mailbox release, plus the test-only force-clean post-cleanup hook seam; +7 from #4111 r7 capturing repair_started_at and passing it to the start-bounded guarded finish so a same-message-id fresh mailbox claim in the clear->finish gap is never finished; +38 from #4111 r6 guarding the post-clear mailbox finish with `mailbox_finish_turn_if_matches` pinned to the cleared turn's user_msg_id (a fresh turn claiming the freed mailbox between clear and finish keeps its token; runtime/session cleanup now runs only when the guarded finish removed the cleared turn's token); +60 from #4111 r4 reworking `clear_idle_tmux_stale_turn` to clear-before-teardown — load ONE candidate row, capture the pin from it, re-check `idle_tmux_repair_has_unrelayed_tail_answer` on that same row (closes the manual stale-mailbox route's TOCTOU), run the generation-pinned guarded clear FIRST, and only on Cleared proceed to mailbox/runtime teardown; non-Cleared outcomes return None with WARNs, preserving mailbox/session/inflight; +4 from #4111 routing the leak-recover offset re-save through the identity-guarded locked field-patch helper (no unlocked whole-row save); +23 from #4048
    round 4 requiring strict provider-less stale-mailbox repair to verify a
    peeked local mailbox has an active token or queue before treating it as
    ownership evidence; +45 from #4048 round 3 scoping provider-less
    stale-mailbox repair to strict per-runtime ownership evidence before raw
    global hard-stop fallback; +8 from #4048
    warning when a raw global-mailbox hard-stop fallback preserves pending
    backlog without a resolvable runtime completion event; +73 from #4035
    guarding stale-mailbox idle-tmux inflight clear with the readiness-time
    finalizer/user-message identity plus `updated_at` and `save_generation` pin;
    +13 from #4024 F1 pairing health hard-stop finalize-path `thread_parents`
    cleanup with parent queue kickoffs; +75 from #4019 R2
    round 2 moving explicit-background hard-stop cleanup before finalizer mailbox
    release and making watcher stop tmux-session-conditional; +115 from #4019 R2
    watchdog identity revalidation — explicit-background destructive cleanup now
    carries full inflight identity from the snapshot, revalidates under the
    inflight flock immediately before deletion, routes mailbox release through
    the turn finalizer, and keeps runtime cleanup separate from token/counter
    authority; +13 from #4019 R1 resolving stale-hourglass cleanup and completed-leak recovery through the
    channel-owning runtime's Discord HTTP handle instead of the provider's first
    registered runtime; +89 from #3925
    finalizing the inflight turn-state after the out-of-band deadlock-manager leak
    recovery (`maybe_recover_completed_stale_leak`) delivers a completed answer —
    routing the recovered terminal through `finish_recovered_turn_mailbox` (mailbox
    token release + gated `global_active` decrement + idle-queue kickoff) and an
    identity-guarded inflight clear (`clear_recovered_leak_inflight`), so a
    relay-broken turn no longer pins the session to a phantom in-progress turn that
    queues new messages forever; #3872 removes
    visible continuation markers from long-message split paths and adds legacy-prefix
    recovery compatibility (+3 after review fix); -598 from #3839 moving
    pure stall-watchdog decisions to `health/recovery/watchdog_decisions.rs`
    and completed-stale leak range/render/ledger helpers to
    `health/recovery/leak_recovery_ledger.rs`; +2 from #3807 applying
    compact continuation context to stale-leak recovery split delivery; -3 from #3795
    routing session-key tail fallback through `SessionIdentity`; +2 from #3711/#3712 mapping direct TUI runtime-binding-unavailable rebind failures to 409 Conflict; #3676 moved
    `tmux_alive_relay_dead` watchdog reattach logic into sibling
    `health/relay_dead_reattach.rs`, leaving recovery.rs with only the
    pre-cleanup hook so final transcript output can be delivered without
    cancelling a healthy mid-first-output watcher; +3 from #3671 passing the turn's RAW restart-invariant age (`judgment_basis.turn_age_secs`, not the boot-floored anchor age) into `evaluate_stall_watchdog_liveness` so the stall-watchdog force-clean defers indefinitely under positive liveness up to an age-based absolute backstop (4h, aligned to the Codex per-turn hard ceiling) that repeated restarts cannot reset, instead of a brittle 20-tick count — fixes the ~40-minute restart-survived deploy turn that was force-killed while demonstrably live; +11 from #3668 F2 watchdog loop-top tail-answer guard — one early-`continue` that skips BOTH destructive branches (idle-clear + desynced force-clean) for the channel this tick when JSONL still holds an unrelayed final answer after last_offset; #3656 stall-watchdog force-clean ages from current turn `started_at` not `updated_at` (turn-scoped, net 0 after comment condense); +85 from #3629 NO_REPLY/empty orphan inflight identity-guarded cleanup in the completed-stale leak detection path; +3 from #3479 item-3 `shared.dispatch.<field>` nesting; health recovery
    extraction surface, split further before adding non-bugfix behavior; +70
    from #3126 stall-watchdog completed-idle false-positive guard tests; +88
    from #3169 stall-watchdog jsonl-mtime liveness guard + tests, closing the
    Death #1 force-clean false-positive on loop mid-write sessions; -4 from
    #3293 routing the mailbox probe through the non-creating
    `health/mailbox.rs::peeked_provider_mailbox_state` so repair probes stop
    minting permanent registry entries for non-existent channels; -4 from
    #3360 moving orphan pending-token auto-heal out to
    `health/relay_auto_heal.rs`; #3361 moved positive stall-watchdog liveness
    guard state/logging to `health/stall_liveness.rs`; #3410 wired the
    force-clean watcher-respawn follow-through + always-run cross-tick
    retry/dead-man (P1-a: no early return on zero candidates), delegating the
    new behaviour to `health/watcher_respawn.rs`).
  - `src/services/discord/router/message_handler/intake_turn.rs` (2728 lines; -110 from #4248/#4329 removing the now-dead busy pre-submit queued-card controller branch after reaction-only policy became unconditional; O1 Pending→Queued reaction promotion is deferred to #4598 because the reconciler intentionally rejects started→queued reversals; +0 net from #4329 gating the residual busy pre-submit queued-card render behind the reaction-only queue-status policy (guard lands fmt-stable net-zero); +3 from #4247 fail-safe queue-preservation P0 (reviewable admission, decompose tracked by #4552; net-zero proven impossible) — the turn-start DISPATCH-GUARD gains an fmt-unfoldable `&& !preserve_on_cancel` condition in its if-let chain, `set_followup_requeue_context` gains one `preserve_on_cancel` argument (one-arg-per-line under rustfmt), and the merged mutation-provable guard/wiring test mod adds one `#[cfg(test)] mod` framing line; +1 from #4309 threading the provider-known Claude-harness flag into worker-local per-turn prompt assembly; -10 net from #4485 extracting stale-busy intake recovery and channel/tmux name resolution into cohesive `tmux_reaper.rs` helpers; +9 from #4307 PR-B routing the voluntary tool_feedback reminder through the same take/inject/put-back path as the session-retry recovery context — intake folds the reminder stashed at the previous turn's end (provider-scoped kv key, codex dual-review r1) into `reply_context` via the sibling `take_and_merge_feedback_reminder` (turn_start.rs, non-baselined; take+merge logic lives there) so it reaches the prompt via `context_chunks` AND is carried forward inside `reply_context.clone()` on a TUI-busy requeue, and the refusal branch forwards `&provider` + the owned reminder to `apply_tui_busy_enqueue_refusal` (sibling `tui_followup.rs`, non-baselined) for a KV-only put-back under the same provider key; the stash itself lives in sibling `completion_postlude.rs` and the storage/format helpers in `recovery_text.rs`/`response_format.rs` (all non-baselined); +1 net from #4139 — the TUI-busy enqueue-refusal branch now calls `apply_tui_busy_enqueue_refusal` (sibling `tui_followup.rs`, non-baselined), which puts the taken session-retry recovery context back via a KV-only restore (no new audit row) before rewriting the refusal notice, so refusal branches no longer drop context the successful-requeue branch preserves; +2 from #4117 delaying the session-retry recovery-context take past the stale-dispatch abort and race-loss returns so unused context is never consumed/audit-stamped; +2 from #4107 moving hosted-TUI busy pre-submit mailbox release ahead of retry enqueue so the active-message retry passes the actor guard; -5 from #4049 S4-b routing queue-exit feedback through the reconciler; +5 from #4049 S4-a2 round-7 threading the reconciler's per-start-attempt token from the optimistic Pending record into the race-loss rollback path so stale delayed rollback cannot clobber a same-generation re-dispatch; +13 from #4049 S4-a2 routing queue-marker add/remove through `queue_marker.rs` so standalone 📬 notifications use `turn_view_reconciler` while ➕/🔄 stay auxiliary; -11 from #4019 R1 routing dequeue queue-marker cleanup through the shared 📬/➕/🔄 marker list and shortening the local contract note; +10 from #3813 Phase 3 (§4/AC#6) a single `edit_channel_message` call-site surfacing the hosted-TUI busy preflight readiness wait (⏳ TUI 준비 대기 중) on the intake placeholder so a safe up-to-45s wait doesn't look like a stalled session start — the render logic/const/tests live in sibling `tui_followup.rs` (non-baselined), the root only adds the one edit call before `wait_readiness` moves into spawn_blocking, and the compact state is transient (overwritten by dispatch streaming on ready or by the queued-card/delete/refusal-notice paths on still-busy); +18 from #3813 Phase 1a intake latency spans — six thin observation-only mark/emit call-sites (turn-claimed anchor + placeholder/prep/input marks + `submitted`/`deferred_busy` emit); all monotonic-`Instant` measurement + formatting lives in sibling `latency_spans.rs`, no control-flow change; -856 from #3837 behavior-preserving decomposition lifting three cohesive `handle_text_message` clusters verbatim into sibling `intake_turn/` submodules — `voice_intake::resolve_intake_voice_announcement` (voice-announcement resolution), `race_loss::handle_race_loss_enqueue` (the `if !started` mailbox enqueue + queued-placeholder render + queue-pending reaction lifecycle), and `turn_watchdog::spawn_text_turn_watchdog` (the per-turn watchdog spawn); pure code movement + path/visibility plumbing, no logic change; +21 from #3905 threading the intake gate's already-authorized, non-consuming voice-announcement resolution into direct dispatch (new `handle_text_message` `gate_resolved_voice_announcement` param + the trust-the-carry-forward resolution branch) so a sibling-gateway durable-consume race no longer WARN-drops an announce the gate authorized; #3464 single-dispatch dedup preserved via the unchanged per-message `route_voice_transcript_announcement_once` claim; +16 from #3811 recording the original-request turn anchor (`set_turn_request_anchor`) gated on the won mailbox claim (`started == true`) so a queued message never bleeds the active turn's deeplink; +1 from #3751 routing paused-watcher attach through owner-channel persistence helper; -65 from #3653 removing the separate session-restore notify bot send path so restore is absorbed by the session/status panel; -40 from #3591 100턴 세션 리셋(AssistantTurnCap) 제거: reset 판정/clear/DB clear/display 블록 삭제; -2 from #3588 idle 세션 리셋 제거(IdleExpired display arm + `now` 인자 정리); +23 from #3557 Codex review: cap the INITIAL watchdog deadline at the provider hard ceiling (+one-shot ceiling warn); +27 from #3557 (A) per-turn hard-ceiling clamp wired into the watchdog auto-extend block; +1 from #3479 item-3 `shared.dispatch.<field>` nesting; +9 from #4305 wiring `record_fresh_session_context_boundary` (durable /goal-fresh clear boundary) into the fresh-provider-session path;
    Discord message intake turn orchestration split from the router message
    handler; bugfix only outside a further extraction plan; #3464 extracted the
    unauthorized-voice-announcement scope decision to `voice_announcement_scope.rs`;
    #3479 extracted the voice-transcript announcement route cluster
    (`claim_voice_transcript_announcement_processing`,
    `VoiceTranscriptAnnouncementRouteOutcome`,
    `route_voice_transcript_announcement_once`) to `voice_announcement_route.rs`;
    +9 from #3082
    queued-only answer-flush gate (`is_queued_notice` on the two
    `send_intake_placeholder` call sites: `true` for the race-lost queued card,
    `false` for the active-turn placeholder); +57 from #3182 normal-dequeue
    queue-pending reaction cleanup (`queue_pending_reactions_to_clear` helper +
    `remove_reaction_raw` at the `started==true` promotion point, removing the
    stranded `📬`/`➕` so a processed message no longer shows `📬`+`✅`); +1 from
    #3038 S1 mechanical `.queued_placeholders` -> `.queued.queued_placeholders`
    re-wire after lifting cluster C into `QueuedPlaceholderState`; -2 from #3038
    S4 mechanical placeholder/status-panel `.ui` rewiring).
  - `src/services/discord/router/message_handler/headless_turn.rs` (1383 lines; +1 from #4309 threading the provider-known Claude-harness flag into worker-local per-turn prompt assembly; +1 from #4117 delaying the recovery-context take past the goal-lifecycle Consumed return; -207 from #4119 — inline watchdog loop extracted to the shared watchdog.rs timeout-notice helper; #3751 routes paused-watcher attach through owner-channel persistence helper with no net LoC change; +74 from #family-profile-probe DM-fresh provider session: `dm_fresh_routine_turn` discriminator routes a fresh DM routine turn through the shared `/goal fresh` machinery (`force_fresh_provider_session = goal_fresh || dm_fresh`) — thorough clear (in-memory + DB + stale id) + Claude TUI runtime-binding clear (`tui_prompt_dedupe::clear_tmux_runtime_binding`) + DB/live-TUI restore skip + launch fresh flag, so neither the persisted id nor the live tmux pane is reused (codex review P1/R2/R3 — four reuse layers: in-memory, DB, Codex wrapper, Claude TUI runtime-binding recovery); the `/goal` prompt rewrite stays goal-only so the probe prompt is sent verbatim; so a fresh DM routine turn never resumes the accumulated per-channel session (memento caseId is the only cross-run continuity); -45 from #3591 100턴 세션 리셋(AssistantTurnCap) 제거: reset 판정/clear/DB clear/display 블록 삭제; -2 from #3588 idle 세션 리셋 제거(IdleExpired display arm + `now` 인자 정리); +49 from #4305 recording the durable clear boundary for /goal-fresh and DM-fresh plus the routine identity-change path;
    headless Discord turn launch/terminal-response path split from the router
    message handler; bugfix only outside a further extraction plan; +54 from
    #3557 (A) codex r2: the headless watchdog was missing the per-turn hard
    ceiling cap that the foreground intake path already had — and it also
    `mark_async_managed`s the token so the sync watchdog stops enforcing, leaving
    this async loop as the ONLY bound. Added the initial-deadline
    `min(now+timeout, ceiling)` cap + one-shot ceiling warn and the auto-extend
    `clamp_auto_extend_deadline_ms` clamp, reusing the shared discord/mod.rs
    helpers, so headless Codex honors its 4h ceiling end to end).
  - `src/services/discord/meeting_orchestrator.rs` (3222 lines; +1 from #4055
    preserving the typed transient delivery result; #3034 dead-code sweep
    removed `is_meeting_channel`).
  - `src/services/discord/turn_bridge/tmux_runtime.rs` (993 prod lines; provider
    stop-token/tmux binding runtime + the async interrupt/cancel/hard-stop
    orchestration + session-teardown. #3169: the claude-anonymous-teardown
    SIGINT suppression guard (death #3) lives in the `interrupt_policy` child.
    #3479 initially decomposed the giant 1545 -> 964 by moving three cohesive, verbatim
    clusters into `tmux_runtime/` child modules (`interrupt_policy.rs`,
    `process_table.rs`, `pid_exit.rs` — see their entries below); no longer a
    giant-file. Bugfix only outside a further extraction plan).
  - `src/services/discord/terminal_ui_obligation.rs` (545 prod LoC; #3607
    worker-local durable terminal-UI obligation sidecar store plus isolated
    status-card reconciliation sweeper. The file owns
    `discord_terminal_ui_obligations/<provider>/<channel_id>.json`, pure
    record/reconcile predicates, and boot-resumed status-card edit convergence.
    It is below the giant-file threshold).
  - `src/services/discord/turn_bridge/cancel_finalize_policy.rs` (131 prod
    lines; pure cancel/finalize-policy decisions extracted from `mod.rs` by
    #3479: `classify_turn_finished_dispatch_kind`,
    `is_done_setting_terminal_frame`, `should_finalize_cancel_after_recv`,
    `should_suppress_headless_delivery_for_cancel`,
    `should_record_final_turn_transcript`, `resolve_bridge_owner_channel` +
    their tests. No IO/async; not a giant-file).
  - `src/services/discord/turn_bridge/streaming_edit_text.rs` (88 prod lines;
    pure streaming-edit text + pre-submission/transport TUI prompt-error
    classifiers extracted from `mod.rs` by #3479 rank-4:
    `build_turn_bridge_streaming_edit_text`,
    `bridge_pre_submission_tui_prompt_error`,
    `bridge_tui_transport_error_should_skip_quiescence` + their tests. No
    IO/async; not a giant-file).
  - `src/services/discord/turn_bridge/watcher_orphan_cleanup.rs` (119 prod
    lines; watcher-orphan spinner-cleanup decision + retry-spawn helpers
    extracted from `mod.rs` by #3479 rank-4:
    `should_delete_bridge_created_watcher_orphan_response`,
    `should_retry_watcher_orphan_spinner_cleanup`,
    `record_watcher_orphan_spinner_cleanup`,
    `spawn_watcher_orphan_spinner_cleanup_retry` + their tests. The retry-spawn
    routes through `task_supervisor`/`placeholder_cleanup` and takes all deps by
    value; not a giant-file).
  - `src/services/discord/turn_bridge/response_delivery.rs` (74 prod lines; pure
    response-delivery + transcript-event helpers extracted verbatim from `mod.rs`
    by #3479: `push_transcript_event`, `response_portion_after_offset`,
    `terminal_delivery_response_after_offset`,
    `done_result_requires_full_terminal_replay`. All `pub(super)` and re-imported
    so the parent call sites + inline tests stay byte-identical; the two
    discord-level `super::` refs (`response_sanitizer`, `DISCORD_MSG_LIMIT`)
    deepened to `super::super::`; no IO/async; not a giant-file).
  - `src/services/discord/turn_bridge/completion_postlude.rs` (917 prod
    lines; #4230 S3 completion postlude + inflight epilogue extracted verbatim
    from the final post-loop tail of `spawn_turn_bridge`: status-panel completion,
    final ADK status, watcher resume, transcript/memory/analytics persistence,
    metrics, restart-report cleanup, inflight preserve/clear, mailbox recovery
    marker cleanup, and the final queued-turn drain. The #4185 restart-cancel
    branch moved intact; discord-level `super::` refs are deepened one level;
    behavior-preserving decompose; not a giant-file).
  - `src/services/discord/turn_bridge/post_loop_finalize.rs` (720 prod lines;
    #4230 S4 post-loop owner classification + finalizer block extracted
    verbatim from `spawn_turn_bridge`: stream-exit placeholder cleanup, orphaned
    tool finalization, API friction extraction, follow-up requeue candidate,
    review dispatch guard, bridge output owner classification, `TURN_ACTIVE`
    publish, finalizing counters, early TUI gate, busy-watcher handoff, and
    single-authority finalizer submission. Discord-level `super::` refs are
    deepened one level; behavior-preserving decompose; not a giant-file).
  - `src/services/discord/turn_bridge/completion_guard.rs` (872 prod lines; no
    longer a giant-file after #3479 verbatim-extracted two leaf child modules
    under `completion_guard/` (1834 -> 872 prod). It now holds the
    review/verdict/decision extractors + `guard_review_dispatch_completion`, the
    `fail_dispatch_*` retry policy, and the `complete_work_dispatch_on_turn_end`
    orchestrator; the moved children are re-exported back so
    `turn_bridge/mod.rs` paths are unchanged. Its giant-file-registry
    `grandfathered` path and ratchet baseline were retired/win-locked).
  - `src/services/discord/turn_bridge/completion_guard/completion_postgres.rs`
    (530 prod lines; runtime-Postgres last-resort dispatch completion/failure
    paths + auto-queue reconciliation + dispatch-followup/reconcile-marker
    plumbing extracted from `completion_guard.rs` by #3479:
    `runtime_pg_complete_dispatch_with_result`,
    `runtime_pg_fail_dispatch_with_result`, the `runtime_pg_*_linked_auto_queue`
    helpers, `runtime_db_fallback_complete_with_result`,
    `streaming_final_complete_dispatch_with_result`,
    `queue_dispatch_followup_with_handles`, `store_reconcile_marker_with_handles`
    + their tests. Direct sqlx IO; not a giant-file).
  - `src/services/discord/turn_bridge/completion_guard/completion_context.rs`
    (462 prod lines; work-dispatch completion-context + commit attribution
    extracted from `completion_guard.rs` by #3479: `extract_commit_sha_from_output`,
    the `DispatchCompletionHints` lookup/parse helpers,
    `work_dispatch_completion_context`, `build_work_dispatch_completion_result`,
    and the `noop`/tracked-change context builders. Reads git history + Postgres
    completion hints; not a giant-file).
  - `src/services/discord/turn_bridge/tmux_runtime.rs` (993 prod lines; no longer
    a giant-file after #3479 — see the description above and the three child
    entries below).
  - `src/services/discord/turn_bridge/tmux_runtime/interrupt_policy.rs` (225 prod
    lines; pure provider turn-interrupt policy decisions + value types extracted
    verbatim from `tmux_runtime.rs` by #3479: `ProviderTurnInterruptPlan`,
    `ProviderTurnInterruptOutcome`, `interrupt_sigint_target_missing` (#3029 A),
    `provider_turn_interrupt_plan`, `fallback_sigint_pid_for_provider` (#3021),
    the `#3207` claude session-preserving delivery selection
    (`ClaudeTurnInterruptDelivery` / `claude_turn_interrupt_delivery` /
    `build_claude_interrupt_control_line`), and the `#3169`
    `ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON` sentinel +
    `claude_teardown_sigint_suppressed` + their tests. No IO/async; not a
    giant-file).
  - `src/services/discord/turn_bridge/tmux_runtime/process_table.rs` (248 prod
    lines; `ps`-backed process-table discovery extracted verbatim from
    `tmux_runtime.rs` by #3479: `ProcessRow`, `provider_cli_pid_in_tmux`,
    `pane_foreground_is_provider_wrapper`, `select_provider_pid_in_pane`, the
    descendant-walk/command-match scoring helpers, the wrapper-FIFO writer
    (`write_line_to_wrapper_fifo`), and the `send_sigint` primitive. Depends only
    on `platform::tmux::pane_pid` + `libc`/`std`; not a giant-file).
  - `src/services/discord/turn_bridge/tmux_runtime/pid_exit.rs` (176 prod lines;
    the `#2426` OS-level PID-exit observation family extracted verbatim from
    `tmux_runtime.rs` by #3479: `wait_for_pid_exit` (kqueue `NOTE_EXIT` on macOS,
    `pidfd_open`+`poll` on Linux, bounded-sleep fallback) + its tests. Depends
    only on `libc`/`std`/`tokio`; not a giant-file).
  - `src/services/discord/turn_bridge/terminal_delivery.rs` (604 prod lines;
    no longer a giant-file after the #3028 splitter fix corrected its inline
    `#[cfg(test)] mod` accounting (previously miscounted as 1341 prod). Its
    giant-file-registry [[entry]] was removed. #3038 turn_bridge S1 moved
    `advance_tmux_relay_confirmed_end` here; split the remaining lease wiring
    vs delivery helpers before adding behavior).
  - `src/services/discord/turn_bridge/stream_loop.rs` (979 prod lines; #4230 S6
    extracted the main stream-loop shell from `turn_bridge/mod.rs`, S7 moved the
    tool/result/task-notification arms to `stream_loop/tool_arms.rs`, and S8 moved
    the content/status/terminal arms to `stream_loop/content_arms.rs`. The root
    now retains the cancel gates, ready-frame drain, runtime-handoff delegation,
    stream/status ticks, and long-running placeholder state wiring. Its #4230
    giant registry entry was retired after S8; the measured 979-line cap remains
    below the 1000-prod-LoC threshold).
  - `src/services/discord/outbound/turn_output_controller.rs` (1228 prod lines;
    #4046 S1r-1 keeps the anchor-less `SendFresh` implementation in the 228-line
    `turn_output_controller/fresh_send.rs` child while the root owns only the
    shared verb/outcome contract and routing; crossed the giant threshold in
    #3998 E13 when the controller-facing lease guard moved from `TurnKey` to
    `DeliveryLeaseKey` for id-0 disambiguation. Tracked decompose target — see
    `giant-file-registry.md` (owner `discord-relay`, deadline 2026-08-31, issue
    #3405). Keep further controller growth in narrower outbound/controller
    helper modules).
  - `src/services/discord/turn_finalizer.rs` (1048 prod lines; single-authority
    turn-finalize state machine — ledger/actor-loop/reconciler. Crossed the
    giant-file threshold when #3041 P1-0 added the dormant `DeliveryLeaseCell`
    finalizer messages/handlers on top of #3143's `FinalizeContext::monitor()` +
    monitor turn-key/ledger-generation logic; tracked decompose target — see
    `giant-file-registry.md` (owner `discord-finalizer`, deadline 2026-08-31,
    issue #3016). #3479 r9 split −191 prod lines into the leaf child modules
    `turn_finalizer/completion_signal.rs` (CompletionSignal enum + pure
    `completion_signal_from_transcript`), `turn_finalizer/delivery_lease.rs`
    (dormant `DeliveryLeaseCell` handlers), and
    `turn_finalizer/watcher_backstop.rs` (watcher far-backstop tunables +
    terminal-or-defer verdict pair). Bugfix only outside a
    finalizer-decomposition plan). #4018 round-2 carries synthetic claim
    snapshots through terminal submissions so relay-ownership-only passive notes
    skip the backstop reaction fallback, routes stale synthetic release through
    the finalizer, and demotes expected backstop/reconcile guarded misses while
    preserving WARN for ordinary submitter misses; #4019 R2 adds the multi-live
    refusal for channel-only id-0 collapse so ambiguous terminals return the
    literal no-match key instead of releasing an arbitrary live entry;
    `turn_finalizer/finalize.rs` is now 246 prod LoC, `turn_finalizer/finalize_context.rs` 113 prod LoC,
    `turn_finalizer/reconcile.rs` 221 prod LoC, and
    `turn_finalizer/cleanup.rs` 565 prod LoC. No PG lease/schema change.
  - `src/services/discord/turn_view_reconciler.rs` (2356 prod lines; +55 from #4606 migrating queued-state persistence to schema v3, converging legacy v2 marker+hourglass records, and making queued user-message views marker-only while `Queued*` → `Pending` adds a fresh `⏳` through the target-set diff; +45 from #4248/#4329 review hardening: queued-state schema v2 invalidates v1 queue records while keeping v1 pending-anchor recovery compatible, and multi-reaction transitions compensate already-applied operations on partial failure; #4248 moves
    the derived reaction mapping into `turn_view_reconciler/reaction_set.rs` and
    originally made queued user-message views include an immediate `⏳` alongside
    their queue-kind marker; #4606 supersedes that queue presentation while
    terminal completion still converges to `✅` through the same persisted adder identity; +104 from
    #4049 S4-a2 round-9 adding an attempt-scoped clear API/current-generation
    shim so race-loss stale attempt-1 clears cannot wipe a same-generation
    attempt-2 Pending marker; #4049 S4-a2 extends the S4-a1 reaction reconciler
    with persisted queue-marker state for notification-only 📬/⏳/✅/⚠/🛑 updates,
    queue cancel cleanup, and requeue coalescing; bugfix-only until a follow-up
    can split persistence/tests from the runtime reconciler).
  - `src/services/discord/formatting.rs` (2566 lines; -296 from #4055 moving the durable continuation rollback journal into `formatting/rollback_journal.rs`; +41 from #4214 converting every Discord-limit length judgment in the send/chunk paths from UTF-8 byte length to unicode code-point count (Korean answers no longer split ~3x early at ~666 chars) with a safe char-budget→byte-index boundary mapper; code-fence preservation and the #1043 empty-chunk guard unchanged; +14 reconciled to the current module-inventory production surface per #4183 CI-red recovery (post-surgery inventory drift); -2 from #4049 S4-b doc-comment sync on the reconciler-routed reaction path; +25 from #3998 D1 exposing
    raw long-send created message ids and fallback replacement anchors for
    recovery anchor persistence while the existing `send_long_message_raw_with_reference`
    surface remains a unit-returning wrapper; presentation/chunking behavior unchanged. -25 from #4019 R1 moving
    shared reaction lifecycle helpers to `reaction_lifecycle.rs` while keeping
    the formatting re-export surface; #3805 P1 adds the watcher
    completion-footer re-anchor machinery here — the `ReplaceLastChunkAnchor`
    struct, the `&mut Option<..>` last-chunk out-param on
    `replace_long_message_raw_with_outcome`, and the pure
    `watcher_completion_footer_anchor` selection helper + its regression tests —
    worker-local presentation logic only, no relay ownership/lease change. #3807
    wires semantic
    sentence-boundary callsites while keeping the shared boundary classifier
    and compact continuation-context helper in `semantic_boundaries.rs`;
    worker-local presentation logic only, no relay ownership/lease change.
    #3818 keeps only the placeholder-status subagent-notification summary hook
    here while moving the shared streaming-rollover predicate to
    `subagent_notification_card.rs`. Historical freeze notes: net +0 from #3034
    scoped
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
  - `src/services/discord/runtime_bootstrap.rs` (285 production lines after #3479 item-3 grouped the 20 builder args into 6 param structs; was 274 after
    #3038 run_bot S0/S5; characterization tests pin the startup-doctor barrier,
    restored settings filters, queued-placeholder filtering/deletion, and
    gateway intents, then the low-risk clusters moved verbatim into
    `runtime_bootstrap/`: `restored_state.rs` (124), `queued_placeholders.rs`
    (193), `startup_doctor.rs` (156), `orphan_recovery.rs` (337),
    `session_gc.rs` (102 prod / 69 test), `framework_setup.rs` (287),
    `spawns.rs` (229), `recovery_flush.rs` (357), `voice.rs` (140),
    `gateway_lease.rs` (190), `shutdown.rs` (207), `intake.rs` (63),
    `shared_data.rs` (236, the `run_bot_build_shared_data` builder + its 4 #3479 param structs plus its
    side-effect-order doc), and `gateway_runtime.rs` (147, the leader runtime
    tail from restored logging through backend event-loop entry).
    The namespace is capped at 700 prod lines per child module in
    `audit_maintainability_config.toml`; the root is no longer a prod giant and
    was removed from `giant_file_registry.toml`; #3038 S5 locked the final
    root ratchet at 274 production lines).
  - `src/services/discord/voice_barge_in.rs` (2887 lines after #3906 added the
    deterministic voice intake feedback (P1 Phase-1 intake chime emitted right
    before `start_voice_turn` plus removal of the redundant foreground-start
    chime, and the P4 `DONE_CHIME_FILE_NAME` const; the bulky
    `ensure_done_chime_file` descending-tone generator + `play_done_chime` were
    kept in the `progress_playback.rs` submodule to hold the giant flat); #3914
    added the
    `FOREGROUND_MODEL_TIMEOUT_SLACK` const that de-duplicated the triplicated
    250ms timeout slack; #3038
    VoiceBargeInRuntime S1 moved the STT method cluster to
    `src/services/discord/voice_barge_in/stt.rs` (314 production lines) and
    S2 moved the progress playback method cluster to
    `src/services/discord/voice_barge_in/progress_playback.rs` (423 production
    lines), and S3 moved the final-result playback cluster to
    `src/services/discord/voice_barge_in/final_result_playback.rs` (243
    production lines), and S4 moved the routing-resolution cluster to
    `src/services/discord/voice_barge_in/routing.rs` (383 production lines),
    and S5 moved the live-cut playback cluster to
    `src/services/discord/voice_barge_in/live_cut_playback.rs` (120 production
    lines), and S6 moved the TTS pipeline cluster to
    `src/services/discord/voice_barge_in/tts_pipeline.rs` (86 production
    lines), and S7 folded the agent-voice routing helper block into
    `src/services/discord/voice_barge_in/routing.rs` (now 500 production
    lines), and S8 moved the foreground decision/parser cluster to
    `src/services/discord/voice_barge_in/foreground_decision.rs` (214
    production lines), and #3801 moved the real receive/barge-in hook into
    `src/services/discord/voice_barge_in/receive_hook.rs` (114 production
    lines) while adding deterministic PCM harness coverage through the real
    receive/barge-in path, and #3911 added the shared
    `InflightForegroundCancelGuard` drop guard (+19 prod lines) so an aborted
    foreground `generate().await` unregisters its CancelToken instead of
    leaking it (a leak left `has_inflight_foreground` permanently true and the
    channel misclassified the next fresh utterance as a barge-in), and #3910
    gated the File-mode streaming feed-task hook behind a synchronous
    `streaming_stt_enabled` atomic mirror and made `unregister_voice_guild` async
    so voice-channel teardown reaps per-channel feed-task buckets
    (`StreamingSttSessions::remove_channel`) AND discards the matching inner
    `WhisperStream` sessions (`VoiceSttRuntime::discard_stream_session`, with the
    stt read guard hoisted to a local so it is not held across the discard
    awaits) (+93 prod lines), closing a default-deployment memory/CPU leak where
    every ~20ms File-mode speaking tick spawned an immediately-returning feed
    task whose `JoinHandle` was never drained, plus a Stream-mode inner-session
    leak on mid-utterance channel leave;
    voice STT/TTS, lobby routing, progress mirroring, and barge-in
    orchestration surface; tracked decompose target — see
    `giant-file-registry.md` (owner `voice-runtime`, deadline 2026-08-31,
    #3036)).
  - `src/voice/receiver.rs` (1108 lines after #3914 added the songbird
    `ClientDisconnect` handler that drops a leaver's SSRC→user mapping to stop
    monotonic `ssrc_users` growth under channel churn; voice receive pipeline,
    utterance segmentation, artifact cleanup, and retention policy surface;
    split before adding non-bugfix behavior).
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
  - `src/services/discord/{commands/text_commands.rs,
    discord_config_audit.rs, router/intake_gate.rs}` (all 1000+ production
    lines).
- active_callsite_coverage: n/a.
- invariants: watcher single-owner per #1222; placeholder lifecycle invariants
  per #1112; `/api/inflight/rebind` is the only path that synthesises an
  inflight state file (through the `src/services/discord/inflight.rs` facade,
  with stale/removal loading in `src/services/discord/inflight/removal.rs` and
  clear/abandon CAS paths in `src/services/discord/inflight/clear_store/`).
  Cancel-induced death must trigger immediate re-attach
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
- 2026-06-16 refresh: #3479 decomposed `commands/diagnostics.rs` (1022 prod
  LoC) verbatim into the `commands/diagnostics/` directory module — the root
  `src/services/discord/commands/diagnostics/mod.rs` (389 production lines;
  metrics/health/sessions/status/inflight/queue/adk-phase/debug slash commands +
  the Gemini session helpers) re-exports the report builders from the new
  `src/services/discord/commands/diagnostics/reports.rs` (651 production lines;
  `build_health_report`/`build_status_report`/`build_inflight_report`/
  `build_queue_report` plus the snapshot-normalization and runtime-labeling
  helpers they consume). Behavior-preserving move; both files are sub-giant, so
  diagnostics graduates from the giant-file registry/baseline. No runtime or
  ownership path changed.
- tests: `src/high_risk_recovery.rs` cancel/recovery suites.
- related_issues: #964, #1112, #1138, #1222, #1223, #1283.

### `dashboard_routes`

- canonical_modules: `src/server/routes/*.rs` (per-domain route module), including
  the protected exact-ID `src/server/routes/message_outbox.rs` operator surface.
  `src/server/routes/auto_queue.rs` is now a small HTTP-only facade;
  its query/command/view/FSM behavior lives under
  `src/services/auto_queue/{query,command,view,fsm,phase_gate}.rs` plus
  smaller route-delegation slices.
  `src/services/auto_queue/activate_command.rs` (1506 lines, post-#1444
  idempotency-guard expansion + #3038 phase-helper decomposition) is the
  canonical activate/dispatch-next command surface; it is intentionally above
  the giant-file threshold and tracked here. The `activate_with_deps_pg`
  orchestrator was decomposed into named phase helpers (resolve-run-id,
  acquire-lock, promote, empty-run completion, capacity, group planning,
  finalize) under #3038 — the added doc-commented scaffolding nets a small
  file-LoC increase while shrinking the god-function from ~1158 to ~559 lines.
  Further growth requires a split issue.
  `src/services/auto_queue/cancel_run.rs` (1031 lines) is the canonical
  auto-queue cancellation and run-stop command surface; split before adding
  non-bugfix behavior.
- legacy_modules: none; retired route fallback history is documented in
  `known-legacy.md`.
- do_not_edit_without_migration_plan (giant-file routes):
  - `src/server/routes/kanban.rs` (2725 lines after #3037 backflow batch
    relocated the `require_explicit_bearer_token` /
    `resolve_requesting_agent_id_with_pg` auth/identity helpers to
    `crate::services::kanban`; +50 from #4038 slice-1 log-only caller
    observability instrumentation at the rereview/reopen/transition
    attribution sites — no route/behavior change).
  - `src/server/routes/docs.rs` was decomposed by #3836 into a thin route
    facade plus `src/server/routes/docs/{guides,inventory,taxonomy}.rs` and
    ordered endpoint inventory parts under
    `src/server/routes/docs/inventory/endpoints/`; keep new API-docs data in
    those child modules and preserve `scripts/check_api_docs_coverage.py`.
  - `src/server/routes/escalation.rs` (1379 lines; +3 from #4486 UtilityBotRole alias typing, mechanical/non-behavioral).
  - `src/server/routes/meetings.rs` (1290 lines; SQL extracted to `src/db/meetings.rs` in #3570 slice 1; +24 from #3742 explicit shared GitHub-only issue creation outcomes).
  - `src/server/routes/review_verdict/decision_route.rs` was decomposed in
    #3038 slice 1 and S1-relocated into a 26-line route shim delegating to
    `src/services/review_decision.rs` plus sub-1000-line service modules under
    `src/services/review_decision/`
    (`repo_card`/`repo_dispatch`/`worktree_stale`/`adapters`/`pending`/
    `accept`/`dispute`/`dismiss_finalize` plus review-state/tuning helpers).
    Conservatively still bugfix-only until the PG-backed characterization
    harness (slice 3) lands; re-inflation is ratcheted in
    `scripts/audit_maintainability_giant_baseline.toml`.
  - `src/server/routes/{agents,agents_crud,agents_setup,v1,resume}.rs` (all
    1000+ production lines). (`dispatches/thread_reuse.rs` dropped below the
    giant threshold in #3037 after its Postgres/Discord-API thread-map helpers
    were relocated to `services/dispatches/discord_delivery/thread_reuse.rs`.)
- active_callsite_coverage: retired DB compatibility history is tracked in
  `known-legacy.md`.
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
  - `src/cli/doctor/orchestrator.rs` (4381 lines).
  - `src/cli/migrate/apply.rs` (3237 lines; +1 from #3690 AgentDef preferred_intake_node_labels literal; +6 from #3697 OpenClaw --write-db non-leader roster-sync gate).
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
  - `src/config.rs` (2763 lines; +25 net from #4553 global Claude gateway-proxy fields, defaults, resolver, parse coverage, and corrected retained cache-TTL docs; +51 from #4130 shared TestEnvVarGuard + shared_test_env_lock — centralized env-pin guard for #3293-class test races; +11 from #3573 failure_pause_auto_resume_secs config field; +16 from #3655 DB pool default 12→18 + 2-node-boot sizing-rationale comment; +47 from #3651 DatabaseConfig.foreground_reserve field (best-effort advisory docs) + manual Default impl + default-consistency tests; +8 from #3690 AgentDef.preferred_intake_node_labels field + doc; #3683 config hot-reload restart-fingerprint config surface; #3736 documents the disabled remote-profile compatibility shim; #3749 adds the `cluster.intake_routing` config authority and parse coverage; +13 from #3870 ServerConfig.allow_insecure_nonloopback_bind escape-hatch field + Debug/Default wiring + doc; +10 from #3805 P2 PR-A two_message_panel_enabled PlaceholderConfig field (two-message model scaffolding, default OFF, restart-required; +18 from #4351 ClusterConfig.gateway_preferred_instance_id + gateway_yield_grace_secs fields, Default wiring, and the yield-grace default fn — the yield protocol lives in discord::runtime_bootstrap::gateway_lease; +7 from #4305 channel recent-context injection config (limit + enable, live-reload)).
  - `src/server/mod.rs` (2778 lines; -22 from #4449 extracting actionable-alert announce→notify delivery into `src/server/outbox_actionable_delivery.rs`; -21 from #4465 moving stale outbox/expired-held GC ownership into `services::message_outbox`; #1122 extends that shared GC owner to preserve scheduled-message permanent dedupe sentinels; +140 from #4089 claude-accounts cswap surface — leader/forced rate-limit refresh serialization (shared async Mutex critical section), fire-and-forget switch refresh with 8s bound, and the sync_claude_rate_limit_cache_once extraction; follow-up decomposition candidate: move the claude rate-limit sync block into a sibling module; +42 from #3573 auto-resume tick + backoff-race fix; #3628 wires failure→pause producer behind the same knob, net -1 line from comment condensation; #3651 net ~0 — the message_outbox_loop is the foreground headless-delivery drain and must NOT be backpressured, so its earlier backpressure gate was removed during codex review; #3740 adds the boot hook for token-analytics cache prewarm; #3722 removes duplicate startup reseed when callers already completed guarded startup initialization; +20 from #3870 fail-closed bind-security guard at the listener bind site — force-loopback when non-loopback host + no auth_token; +15 from #4260 the terminal outbox-failure alert call site in the message-outbox Fail arm (silent-loss vector 3) — the helper bodies (`note_terminal_outbox_delivery_failure` + snippet/target resolvers) live in the new sibling `src/server/outbox_delivery_alert.rs`, only the Fail-arm call + module wiring remain in root).
  - `src/receipt.rs` (1842 lines).
  - `src/github/sync.rs` (1504 lines).
  - `src/reconcile.rs` (1902 lines; +39 from #4104 standardized inflight-row
    removal logging at the `sweep_stale_inflight_files` site; #3685 rebind-origin
    stale-inflight preservation review hardening; periodic reconcile loop
    covering stale inflights, orphan uploads, dispatched-session drift, and
    queue-review drift — split before adding non-bugfix behavior).
  - `src/server/maintenance.rs` (1153 lines; #3909 added the leader-only voice
    TTS cache/temp sweep (`ProgressTtsCacheSweepJob`, 15th MaintenanceJob) +
    runtime-config threading, tipping the per-job-impl static registry over the
    1000-line giant threshold — also registered in `giant_file_registry.toml`.
    The sweep LOGIC lives in `services::maintenance::jobs::voice_cache_sweep`.
    #4231 promoted the per-job startup-stagger literals to named `*_STARTUP_STAGGER`
    constants with rationale comments (behavior-preserving; +105 doc/const lines).
    Bugfix/readability-only, decompose the storage/voice job-impl clusters into siblings).
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
- legacy_modules: retired SQLite migration history only (see
  `known-legacy.md`).
- do_not_edit_without_migration_plan (giant-file):
  - `src/db/auto_queue/tests.rs` is the migrated auto-queue test harness; it is a
    dedicated `*_tests.rs` file (excluded from the production giant-file count),
    so add coverage freely but keep it split-friendly.
  - `src/db/auto_queue/entries.rs` (1408 lines after #4448 extracted terminal
    dispatch-failure/outbox atomicity into `entries/dispatch_failure.rs`;
    awaiting follow-up split per auto-queue decompose epic #1782).
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
  - `src/db/postgres.rs` (1389 lines; #4249 adds typed connect-failure classification plus an eager 10s startup/migration pool followed by eager activation of the separate 3s runtime pool inside one retry/alert envelope; #3651: the `FOREGROUND_RESERVE` process-global, the `background_should_yield` backpressure predicate + pure `should_yield_for_counters` helper, the `clamp_foreground_reserve` helper that keeps the background budget >= 1 for small `pool_max` configs, reserve install+clamp in `connect`, and the predicate + clamp unit tests; #3690: preferred_intake_node_labels upsert/sync + COALESCE preserve; #3692: `agent_roster_sync_enabled` leader-ownership gate on the roster sync; #3722 adds the bounded startup advisory lock wrapper plus concurrency coverage for migration/config-audit/reseed startup sections).
  - `src/db/dispatched_sessions.rs` (1734 lines; dispatched session
    persistence helpers. #4091 r4: +63 keying the watermark to its raw session id
    with mismatch reset, the sticky raw_provider_transcript_growth_proven flag
    (non-destructive growth evidence), compare/flag-only observation for the kill
    path, and watermark+flag clears in the session-id clear APIs (migration 0078);
    #4091 r3: +38 for claude_session_id_recorded_at (upsert
    bumps it only when the cached id value changes, so heartbeats stop extending
    the flip-back window) and the monotonic raw_provider_transcript_len_watermark
    accessors backing restart-surviving growth evidence (migration 0077);
    #4091 r2: +6 exposing cache-entry age on provider-session
    rows so the selector flip-back window can prefer a recently written cached id;
    #3306: +48 for the narrow `load_session_channel_id_pg`
    durable-truth accessor the idle-relay drift self-heal reads; #3693: +2 to
    include `cwd` in provider resume selector lookup; #3718 makes runtime
    activity heartbeat refresh monotonic via `GREATEST`; -1 from #3795 using
    the central `SessionIdentity` tmux-tail helper).
  - `src/db/session_transcripts.rs` is a retained PG-cleanup surface (now below
    the giant-file threshold; bugfix only).
  - `src/db/prompt_manifests/` (directory, refactored).
  - `src/db/intake_outbox.rs` is the intake-node-routing claim/transition/sweep
    surface; its production LoC is now below the giant-file threshold once the
    `#[cfg(test)] mod` PG coverage is excluded (bugfix only).
- active_callsite_coverage: PG-only cleanup tracked per #1237/#1238/#1239 —
  see `known-legacy.md`.
- invariants: production reads/writes go through `pg_pool_ref()`; retired DB
  compatibility handles must not be reintroduced as live route fallbacks.
- allowed_changes: `bugfix` on existing path; `new_feature` MUST use PG.
- tests: `src/integration_tests/postgres_only/*`.
- related_issues: #843 epic, #1237, #1238, #1239.

### `services_misc_giants`

The remaining giant-file modules under `src/services/` not covered above.
Line counts are *production* LoC (the `Prod` column in `module-inventory.md`,
which excludes `#[cfg(test)] mod` blocks); the freshness gate keeps them in sync.

- `src/services/auto_queue.rs` (1545) and
  `src/services/auto_queue/activate_command.rs` (1506); auto-queue route
  behavior is split across `src/services/auto_queue/*` slices, with
  `activate_command.rs` now giant-file territory.
  `src/services/auto_queue/cancel_run.rs` (1031) is also giant-file territory;
  split before further non-bugfix growth.
- `src/services/onboarding/mod.rs` (2937),
  `src/services/dispatched_sessions.rs` (1650; #4091 r2 adds the two-sample
  growth-evidence selector cross-check wiring, claude_tui transcript-mtime
  runtime-activity anchors, and the flip-back window guard), and
  `src/services/settings.rs` (1112) — service-layer route support surfaces
  split out of the large dashboard route modules. (`src/services/onboarding.rs`
  and `src/services/api_friction.rs` have been removed/decomposed.)
- `src/services/dispatches/outbox_route.rs` (1177) — dispatch outbox route
  support extracted from the route layer; split before adding non-bugfix
  behavior.
- `src/services/claude.rs` (2969; +9 net from #4553 replacing dead native cache-TTL launch wiring with guarded gateway-proxy launch decisions and covering the simple-command spawn; -21 from #4113 backend_routing/availability extraction), `src/services/gemini.rs` (1358),
  `src/services/qwen.rs` (2198), `src/services/codex.rs` (3131),
  `src/services/opencode.rs` (2760), `src/services/provider.rs` (1818; +4 from #4566 publishing the session-generation registry binding as a monotonic max() guard with the token-local tmux-session name kept for SIGINT/pid tracking) —
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
  move). #3038 S3 relocates the TUI warm/follow-up hosting cluster into
  `src/services/claude_tui/hosting/` child modules, ratchets claude.rs at 2950
  production LoC, and leaves the #3262 turn-lock machinery in the claude.rs
  root. #3711 adds +12 in codex.rs to persist Codex TUI rollout markers when
  idle relay bindings are registered, so dcserver restart rehydrate has a
  stable rollout identity. #3744 retired the unwired generalized envelope and
  fresh-fork dev-role dedup stubs from provider.rs, leaving only the live Codex
  resumed-session compaction path. #3823 adds Codex launch binary/version
  diagnostics so skills/list failures caused by CLI/app skew are traceable.
  #4047 adds typed fallback pane-readiness plumbing so structured JSONL sessions
  cannot obtain a pane-scrape readiness value. #4411 adds the kill-switched
  Codex TUI warm-followup gate and per-pane turn serialization; detailed reuse
  policy remains isolated in `codex_tui/warm_followup.rs`.)
- `src/services/codex_tui/rollout_tail.rs` (1329) — Codex TUI rollout tail
  parsing and resume identity surface; split before adding non-bugfix behavior
  beyond the #2169 session identity fix and the #3343 message-boundary
  separator unified across the streamed `StreamMessage::Text` surface and the
  `final_text` assembly (one shared `push_message_text` boundary writer; the
  newline witness is the single source of truth so the two surfaces mirror);
  +4 from #3676 threading Codex rollout user-message entry ids into TUI prompt
  dedupe so restart/offset rewind cannot mint duplicate direct-input anchors;
  #3843 moved rollout parser state and JSON event mapping into
  `src/services/codex_tui/rollout_tail/parser.rs` without changing public
  tail/replay entry points or completion heuristics;
  +59 from #3711 persisting rollout markers as soon as the live rollout is
  discovered and adding claimed-rollout candidate selection for restart
  rehydrate; #4411 adds a pinned-path warm tail entry point that carries the
  Discord-origin prompt through turn-local dedupe.
- `src/services/codex_tui/input.rs` (1670) — Codex TUI input readiness
  detector and prompt delivery surface (#2399 hardened the post-turn
  handoff deadline). Treat as giant-file territory; split before adding
  non-bugfix behavior beyond the readiness/cancel contract. #4411 promotes the
  existing action planner to production, consumes composer-ready signals once,
  and requires two live draft snapshots before a warm submit may be replayed.
- `src/services/claude_tui/input.rs` (2187) — Claude TUI input readiness
  detector, prompt delivery, and cancellation/offset handoff surface. Treat as
  giant-file territory; split before adding non-bugfix behavior beyond the
  readiness/cancel contract. (+191 from the #685/#720 reliability fixes:
  startup-dialog auto-dismiss and keeping the follow-up readiness wait alive
  while the prior turn streams; +20 from #3637 centralizing post-paste error
  cleanup and making draft clearing cancel-agnostic. # #3889 +165: detect the
  MCP-authentication-required cold-boot welcome screen during readiness and fail
  fast with an actionable, non-timeout reason instead of false-submitting then
  blind-waiting/retrying the full timeout; gate every ready-return path —
  including the recorded-turn idle-transcript fallbacks — on the MCP-auth check.)
- `src/services/tmux_common.rs` (~1090 prod LoC) — Claude/Codex TUI pane-capture
  heuristics: ready-for-input, prompt-draft vs idle-suggestion-ghost, active-work
  streaming, MCP-auth banner, and `/effort` selector detection, plus session
  temp-file paths. Treat as giant-file territory; split focused detector helpers
  before adding non-bugfix behavior. (+ #3924 recognizing a STRANDED Discord
  follow-up draft — `❯ [User: …] <text>` whose submit Enter was dropped, sitting
  below a finished-turn block under idle chrome — as a recoverable draft so the
  recovery net's JSONL transcript cross-check fires instead of a 120s TUI kill,
  while keeping submitted history and non-injected idle ghosts classified as
  not-a-draft. The capture-side stranded gate keys ONLY on shape — `[User:]`
  composer line under idle chrome with NO response glyph (`⏺`/`✻`) below it; it
  deliberately does NOT use the `Tools: 0 done` footer as a running signal,
  because a FINISHED 0-tool turn prints it too (#3924 codex re-review). Whether
  such a shape is genuinely stranded vs a live just-submitted turn is decided by
  the AUTHORITATIVE transcript turn-state in `claude_tui_followup_stranded_prompt_
  draft_state`, not by the pane.)
- `src/services/memory/memento.rs` (1893).
- `src/services/dispatched_sessions.rs` (1633; +87 from #4091 server-selected session id — freshness cross-check picks the growing raw transcript over a stale cached claude_session_id) — dispatched session domain
  service. This is the post-#1515 SRP extraction target for route/database
  callsites, but the module itself is now giant-file territory; split focused
  helpers before adding non-bugfix behavior. (+5 from #3169 exposing the idle-
  kill `latest_runtime_activity_unix_nanos` jsonl-mtime probe to the stall-
  watchdog liveness guard; +47 from #3693 making kill-tmux's `resumable` claim
  match Claude TUI transcript-backed resume semantics; +105 from #3718 making
  idle-kill skip active-dispatch sessions, use runtime output age as the
  live-activity guard anchor, and log kill/skip timing decisions; +4 from #3795
  replacing inline session-key split errors with central `SessionIdentity`
  helper calls and explicit legacy/namespaced error messages.)
- `src/services/settings.rs` (1112) — settings domain service extracted from
  the route layer in #1519. Keep follow-up changes bugfix-only unless the file
  is split further.
- `src/services/routines/{store.rs (2844), migrated.rs (1286),
  discord_log.rs (1344), agent_executor.rs (1044)}` — durable routine storage,
  migrated launchd validation, Discord notification plumbing, and agent
  execution are the canonical scheduled JS routine surfaces. Split focused
  helper modules before growing these files again.
- `src/services/platform/binary_resolver.rs` (1412) — provider CLI resolver
  surface. #3823 adds macOS Codex.app fallback discovery and all-candidate
  Codex semver probing so AgentDesk prefers the newest compatible Codex binary
  instead of silently launching a stale npm shim. #4619 threads the opaque
  `ClaudeBinary` capability newtype through the resolver so the resolved Claude
  path can only be consumed via `ClaudeCommandBuilder` (raw `Command::new`
  by-construction blocked).
- `src/services/discord/mod.rs` (now 4152 prod LoC after #4049 S4-a2 moved
  queue-marker routing into `queue_marker.rs` and retired direct reaction
  mutation call sites; 4157 prod LoC after #4048 S3 extracted
  the mailbox-release completion publish helper to `turn_completion_events.rs`
  and the post-enqueue idle-drain scheduler to `queue_io.rs` (-46 from 4204,
  +12 from 4146); 4056 prod LoC after #3479 item-3 extracted
  the dispatch intake/routing cluster (`intake_dedup` + `dispatch_thread_parents`
  + `dispatch_role_overrides`) verbatim to `discord/shared_state.rs` as
  `DispatchRoutingState` (-18; call sites use `shared.dispatch.<field>`); 4074
  after #3479 item-2 extracted
  the dispatch-policy cluster verbatim to `discord/dispatch_policy.rs` (-169) on
  top of the earlier catch-up subsystem extraction to `discord/catch_up.rs`;
  4965; +34 from #3019 added the
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
  root, no baseline raise; -7 from #3038 S2 lifting cluster D — the eight
  session-override fields — into `shared_state::SessionOverrideState`, leaving
  a single `overrides: SessionOverrideState` group field on `SharedData` with
  the type re-exported for surface freeze; -15 from #3038 S3 lifting cluster E
  — the thirteen restart-lifecycle fields — into
  `shared_state::RestartLifecycle`, leaving a single `restart: RestartLifecycle`
  group field on `SharedData` with the type re-exported for surface freeze),
  `src/services/discord_config_audit.rs` (1288; +15 from #3692 leader-ownership gate on the config-audit agent sync path).
- `src/services/discord/inflight/save_store.rs` (1083; crossed the 1000
  threshold in #4185 — added the restart-only locked `full_response` patch
  helper `patch_restart_full_response_if_identity_unchanged` (identity +
  restart_mode/generation equality + rebind/output_path invariance + id-0
  offsetless refusal + response_sent_offset boundary check; +14 from the r2
  already-relayed raw/cleaned prefix-equality guard before replacing durable
  text) plus its regression tests, so the API_FRICTION-cleaned response reaches
  restart-preserved rows that the broad guarded save intentionally refuses) —
  guarded inflight save/patch authority. Keep growth bugfix-only; decomposition
  tracked in #4280 (move inline tests to a child module, extract identity-gate
  predicates).
- `src/services/discord/catch_up.rs` (1077; +70 from #4118 bugfix — retry-mode
  REST fetch failure re-arms `catch_up_retry_pending` with a bounded attempt
  cap (4) instead of one-shot consumption, so a transient fetch error no longer
  permanently drops the over-cap backlog into next-restart TooOld loss; state
  widened to `{checkpoint, fetch_failures}`) — catch-up phase 1/2 scan and
  checkpoint orchestration. Keep growth bugfix-only and prefer `catch_up/*`
  helpers for new classification or commit policy.
- `src/services/turn_orchestrator.rs` (3194; +3 from #3293 declaring the
  `registry_purge` child module — the non-creating `peek` lookup and the
  operator-gated `remove_idle_entry` purge live in
  `turn_orchestrator/registry_purge.rs`, outside the frozen module root; +95
  from #3864 moving SIGTERM queue-restore merge inside the mailbox actor; +10
  from #4018 round-2 adding the distinct `MonitorAutoTurn` active-turn marker
  while keeping monitor turns background for queue-yield/cancel semantics).
- `src/services/discord/session_relay_sink.rs` (1677 prod lines; +1 from #4046
  S1r-1 conservatively rejecting the dormant fresh-send-only outcome at this
  replace-only caller; -59 from #3998 S1-f2 retiring the A2b rollout getter/cache
  and flag-OFF pin tests; +7 from #3610 PR-1 passing the terminal anchor into the
  delivered-frontier shadow mirror; -1 prod from #4055 thin
  card-before-answer/context wiring, with task policy extracted to
  `session_relay_sink/task_notification_context.rs`).

Decomposed below the giant-file threshold (no longer frozen; bugfix-scoped but
normal test growth is allowed): `src/services/analytics.rs`,
`src/services/provider_hosting.rs`, `src/services/claude_tui/hook_bundle.rs`,
`src/services/observability/mod.rs`, `src/services/pipeline_override.rs`,
`src/services/routines/loader.rs`, `src/services/platform/shell.rs`,
`src/services/platform/tmux.rs`, `src/services/mcp_config.rs`,
`src/services/process.rs`, `src/services/discord/tmux_lifecycle.rs`,
`src/services/qwen_tmux_wrapper.rs`,
`src/services/discord/session_runtime.rs`,
`src/services/tui_turn_state.rs`,
`src/voice/turn_link.rs`, `src/services/discord/commands/config.rs`,
`src/services/discord/tui_task_card.rs` (#4055: 1167 -> 818 after durable
delivery/store authority moved to `task_notification_delivery/`).
(#3038 S2: 1054 -> 954 after the session-override bookkeeping helpers
moved verbatim to `discord/shared_state.rs` next to
`SessionOverrideState`; still ratcheted at 954 in the frozen baseline).
`src/services/session_backend.rs` left this list in #3344 (997 -> 1023 prod
LoC after the shared terminal-usage provenance helper) but #3405 brought it
back under the threshold (1023 -> 393 prod LoC) by splitting two verbatim
clusters into child modules: the stream-line state machine
(`StreamLineState`/`TaskStartInfo`, `process_stream_line`, and the synchronous
envelope parsers) into `src/services/session_backend/stream_line.rs` (568 prod
LoC), and the #3344 terminal-usage adoption gate plus the analytics re-parser
(`adopt_terminal_result_usage`, `extract_turn_analytics_from_output*`) into
`src/services/session_backend/terminal_usage.rs` (106 prod LoC). It is no
longer registry-tracked (the giant_file_registry.toml entry was removed).
`src/services/discord/session_runtime.rs` joined this list in #3842 (1657 ->
500 production lines) after the worktree, restore-cwd, and channel-routing
clusters moved into child modules. It is no longer registry-tracked.

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
