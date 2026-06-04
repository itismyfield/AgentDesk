# Change Surfaces

> Source: [`docs/agent-maintenance/index.md`](index.md). For every "where do I
> add this?" question, consult this page first. The giant-file list is the
> auto-generated inventory in
> [`docs/generated/module-inventory.md`](../generated/module-inventory.md) and
> the owner/deadline registry in
> [`docs/generated/giant-file-registry.md`](../generated/giant-file-registry.md);
> the rows below project the operational meaning of each entry.
>
> Last refreshed: 2026-06-03 (against #3105 dead/orphaned TUI-session mirror eviction made flake-resistant and run off the Tokio executor, on top of #3099/#3100/#3103 system-continuation bridge-tail output delivery + anchored continuation classifier + injection-wrapper strip + pinned injected-message-id cleanup).

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
  - `src/dispatch/dispatch_status.rs` (1517 lines).
  - `src/services/dispatches/outbox_route.rs` (1118 lines; route extraction
    orchestration surface from #1722, split before adding non-bugfix behavior).
  - `src/services/dispatches/discord_delivery/orchestration.rs` (1652 lines;
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
  - `src/services/discord/watchers/lifecycle.rs` (2301 lines — canonical
    lifecycle extraction surface from #1435; split further before adding new
    lifecycle behavior).
  - `src/services/discord/tmux.rs` (2241 lines after #2558 dead-code sweep;
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
  - `src/services/discord/tmux_watcher.rs` (8168 lines after #2558
    dead-code sweep; #1520 watcher loop extraction + #2427 D/A
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
    split loop helpers further before adding behavior).
  - `src/services/discord/tui_prompt_relay.rs` (3849 lines; SSH-direct TUI
    prompt notification plus Codex rollout response relay surface, bugfix only
    outside an extraction plan; +4 from #3082 queued-only answer-flush gate
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
    preserve-recorded-id / missing-id regression tests).
  - `src/services/codex_tmux_wrapper.rs` (1222 lines; Codex tmux wrapper JSON
    event parser and relay bridge for native Codex session events — bugfix only
    outside an extraction plan).
  - `src/services/tui_prompt_dedupe.rs` (1064 lines; shared TUI prompt
    fingerprinting/dedupe state for hook and rollout relay paths, bugfix only
    outside an extraction plan; +9 from the #3099 re-review crate-visible
    `reset_state_for_tests` helper; +26 from #3105 codex-P1 sub-case B
    `evict_dead_tmux_mirror` tombstone helper that drops both the runtime and
    channel mirror for a dead/orphaned session and then allows re-registration).
  - `src/services/discord/recovery_engine.rs` (4037 lines; +36 from #3099
    task-notification anchor `⏳` cleanup for `user_msg_id == 0` recovery; +4
    from the #3099 re-review pinned-injected-message-id cleanup target; +55 from
    #3078 PR-2 routing recovery completion through `StatusPanelController` behind
    a shadow parity check (the controller adopts the recovered panel id and its
    chosen completion id is asserted equal to the legacy
    `recovery_status_panel_message_id_for_completion` result; the legacy path
    still executes the Discord IO, so behaviour is unchanged); +4 from #3017
    routing the recovery terminal through the single-authority finalizer
    (`submit_terminal` + `FinalizeContext::monitor`) instead of inline
    `mailbox_finish_turn`).
  - `src/services/discord/health.rs` (2354 lines after #1879 snapshot/mailbox
    extraction; +3 from #3082 answer-flush-barrier field in the test SharedData
    constructor).
  - `src/services/discord/health/recovery.rs` (2438 lines; health recovery
    extraction surface, split further before adding non-bugfix behavior; +70
    from #3126 stall-watchdog completed-idle false-positive guard tests).
  - `src/services/discord/router/message_handler/intake_turn.rs` (3620 lines;
    Discord message intake turn orchestration split from the router message
    handler; bugfix only outside a further extraction plan; +9 from #3082
    queued-only answer-flush gate (`is_queued_notice` on the two
    `send_intake_placeholder` call sites: `true` for the race-lost queued card,
    `false` for the active-turn placeholder)).
  - `src/services/discord/router/message_handler/headless_turn.rs` (1316 lines;
    headless Discord turn launch/terminal-response path split from the router
    message handler; bugfix only outside a further extraction plan).
  - `src/services/discord/meeting_orchestrator.rs` (3227 lines).
  - `src/services/discord/turn_bridge/tmux_runtime.rs` (1242 lines; provider
    stop-token/tmux binding runtime + PID-exit observation helper (#2426),
    split before adding non-bugfix behavior).
  - `src/services/discord/turn_bridge/completion_guard.rs` (1849 lines).
  - `src/services/discord/turn_bridge/tmux_runtime.rs` (1242 lines).
  - `src/services/discord/turn_finalizer.rs` (1011 prod lines; single-authority
    turn-finalize state machine — ledger/actor-loop/reconciler. Crossed the
    giant-file threshold when #3041 P1-0 added the dormant `DeliveryLeaseCell`
    finalizer messages/handlers on top of #3143's `FinalizeContext::monitor()` +
    monitor turn-key/ledger-generation logic; tracked decompose target — see
    `giant-file-registry.md` (owner `discord-finalizer`, deadline 2026-08-31,
    issue #3016). Bugfix only outside a finalizer-decomposition plan).
  - `src/services/discord/formatting.rs` (2802 lines; +46 from #3082
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
  - `src/services/discord/runtime_bootstrap.rs` (2762 lines after #2558
    thread-session GC loopback shim cleanup; +3 from #3082 answer-flush-barrier
    field in the SharedData constructor; +1 from #3037 cluster backflow path
    rewrite wrapping a longer `services::cluster::node_registry::*` call; +3 from
    #3078 PR-1 spawning the dormant `StatusPanelController` next to the finalizer
    in the SharedData constructor; +194 from #3038 behavior-preserving
    decomposition of the `run_bot` god-function — `run_bot`'s own body dropped
    from ~1515 to ~1028 lines by extracting eight ordered startup-phase helpers
    (`run_bot_rehydrate_voice_handoffs`, `run_bot_build_shared_data`,
    `run_bot_init_voice_workers`, `run_bot_maybe_spawn_intake_worker`,
    `run_bot_acquire_gateway_lease`, `run_bot_build_slash_commands`,
    `run_bot_spawn_gateway_lease_keepalive`, `run_bot_spawn_sigterm_handler`,
    `run_bot_run_gateway_backend`); the file-level count rose only from the
    helper signatures + ordering-guarantee doc comments since the moved bodies
    are net-zero. The poise framework-builder/setup closure (~580 lines) is left
    inline — its move-captured locals make a clean extraction risky and is
    deferred).
  - `src/services/discord/session_runtime.rs` (1396 lines).
  - `src/services/discord/voice_barge_in.rs` (4835 lines; voice STT/TTS,
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
  - `src/server/routes/kanban.rs` (2752 lines).
  - `src/server/routes/docs.rs` (5880 lines).
  - `src/server/routes/escalation.rs` (1733 lines).
  - `src/server/routes/meetings.rs` (1708 lines).
  - `src/server/routes/review_verdict/decision_route.rs` (4491 lines).
  - `src/server/routes/{agents,agents_crud,agents_setup,v1,resume,
    dispatches/thread_reuse}.rs` (all 1000+ production lines).
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
  - `src/cli/migrate/apply.rs` (3146 lines).
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
  - `src/config.rs` (2213 lines).
  - `src/server/mod.rs` (2239 lines).
  - `src/receipt.rs` (1842 lines).
  - `src/github/sync.rs` (1894 lines).
  - `src/reconcile.rs` (1809 lines; periodic reconcile loop covering stale
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
  - `src/db/dispatched_sessions.rs` (1554 lines; dispatched session
    persistence helpers).
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

- `src/services/auto_queue.rs` (1626) and
  `src/services/auto_queue/activate_command.rs` (1351); auto-queue route
  behavior is split across `src/services/auto_queue/*` slices, with
  `activate_command.rs` now giant-file territory.
  `src/services/auto_queue/cancel_run.rs` (1032) is also giant-file territory;
  split before further non-bugfix growth.
- `src/services/onboarding/mod.rs` (2936),
  `src/services/dispatched_sessions.rs` (1326), and
  `src/services/settings.rs` (1007) — service-layer route support surfaces
  split out of the large dashboard route modules. (`src/services/onboarding.rs`
  and `src/services/api_friction.rs` have been removed/decomposed.)
- `src/services/dispatches/outbox_route.rs` (1118) — dispatch outbox route
  support extracted from the route layer; split before adding non-bugfix
  behavior.
- `src/services/claude.rs` (3786), `src/services/gemini.rs` (1416),
  `src/services/qwen.rs` (2200), `src/services/codex.rs` (2928),
  `src/services/opencode.rs` (1881), `src/services/provider.rs` (1738) —
  provider adapters.
- `src/services/codex_tui/rollout_tail.rs` (1738) — Codex TUI rollout tail
  parsing and resume identity surface; split before adding non-bugfix behavior
  beyond the #2169 session identity fix.
- `src/services/codex_tui/input.rs` (1492) — Codex TUI input readiness
  detector and prompt delivery surface (#2399 hardened the post-turn
  handoff deadline). Treat as giant-file territory; split before adding
  non-bugfix behavior beyond the readiness/cancel contract.
- `src/services/claude_tui/input.rs` (1296) — Claude TUI input readiness
  detector, prompt delivery, and cancellation/offset handoff surface. Treat as
  giant-file territory; split before adding non-bugfix behavior beyond the
  readiness/cancel contract.
- `src/services/memory/memento.rs` (1893).
- `src/services/observability/pg_io.rs` (1047).
- `src/services/dispatched_sessions.rs` (1326) — dispatched session domain
  service. This is the post-#1515 SRP extraction target for route/database
  callsites, but the module itself is now giant-file territory; split focused
  helpers before adding non-bugfix behavior.
- `src/services/settings.rs` (1007) — settings domain service extracted from
  the route layer in #1519. Keep follow-up changes bugfix-only unless the file
  is split further.
- `src/services/routines/{store.rs (2844), migrated.rs (1286),
  discord_log.rs (1353), agent_executor.rs (1044)}` — durable routine storage,
  migrated launchd validation, Discord notification plumbing, and agent
  execution are the canonical scheduled JS routine surfaces. Split focused
  helper modules before growing these files again.
- `src/services/platform/binary_resolver.rs` (1246).
- `src/services/discord/mod.rs` (4465; +34 from #3019 added the
  single-authority `increment_global_active` helper + doc mirroring the
  existing decrement helper — offset by removing 6 inline raw `fetch_add`
  blocks across the relay turn-start sites that now route through it; +12 from
  #3082 answer-flush-barrier field/init/doc; +81 from #3105 the authoritative
  `TmuxWatcherRegistry` gained a `restored_owner_by_tmux_session` map plus
  `restore_owner_channel_for_tmux_session`/`clear_restored_owner_for_tmux_session`
  so a live thread-suffixed TUI session with no live watcher slot can be
  re-registered authoritatively instead of dropped forever),
  `src/services/discord_config_audit.rs` (1459).
- `src/services/turn_orchestrator.rs` (2760).

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
