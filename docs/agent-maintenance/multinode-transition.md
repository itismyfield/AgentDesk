# Multinode Transition

> Source: [`docs/agent-maintenance/index.md`](index.md). Use this page before
> moving any AgentDesk runtime, worker, dispatch, provider, MCP, merge, or test
> execution path from one dcserver node to multiple nodes.
>
> Last refreshed: 2026-05-30 (against #2896 runtime bootstrap thread-session GC safety).

## Read This First

- This page is an enablement map for #876 through #884. It is not a signal that
  multinode execution is already safe.
- Treat every surface below as single-node until its listed invariants and tests
  are implemented. A PostgreSQL table alone is not enough; every side effect
  needs an explicit owner, lease, idempotency key, or node-local routing rule.
- "leader" means the node that owns cluster-wide side effects. "worker" means a
  node that may execute provider CLI, local tools, tmux sessions, MCP calls, and
  node-local telemetry.

## Surface Map (by feature)

### `multinode / discord_gateway_singleton`

- feature: `multinode / discord_gateway_singleton`
- canonical_modules: `src/services/discord/runtime_bootstrap.rs:1526` builds the
  Serenity client, `src/services/discord/runtime_bootstrap.rs:1531` starts the
  gateway lease keepalive task, `src/services/discord/runtime_bootstrap.rs:1549`
  self-fences on lease loss, and `src/services/discord/runtime_bootstrap.rs:1730`
  starts the gateway client.
- legacy_modules: none. The current gateway owner is the active dcserver process
  for that provider.
- do_not_edit_without_migration_plan:
  `src/services/discord/runtime_bootstrap.rs` gateway startup and shutdown paths,
  especially watcher cancellation at `src/services/discord/runtime_bootstrap.rs:1622`.
- active_callsite_coverage: single-node runtime with an existing gateway lease.
  It does not yet define cluster role discovery, worker heartbeats, or a
  capability registry for non-gateway workers.
- 2026-05-18 audit note (#2558): stale thread-session GC in
  `runtime_bootstrap` now calls the leader runtime's Postgres pool directly
  instead of looping back through the internal HTTP cleanup route. The singleton
  assumption remains unchanged: the task is still spawned from the leased
  gateway runtime.
- invariants: `singleton_on_leader`,
  `heartbeat_capability_registry_routing`.
- allowed_changes: `bugfix` only before #876/#877. New gateway, reconnect,
  watcher, or command-router behavior must state whether it is leader-only or
  worker-local.
- tests: leader failover, worker heartbeat expiry, and two-node Discord gateway
  regression from #884.
- related_issues: #876, #877, #884.

### `multinode / supervised_workers_singleton`

- feature: `multinode / supervised_workers_singleton`
- canonical_modules: `src/server/worker_registry.rs:134` defines the supervised
  worker inventory, with `dispatch_outbox_loop` at
  `src/server/worker_registry.rs:206`. `src/server/mod.rs:201` creates the
  registry, `src/server/mod.rs:207` runs boot-only steps, and
  `src/server/mod.rs:208` starts workers after boot reconcile.
- legacy_modules: none. Workers are centrally registered, but most entries still
  assume that every server process may start its local loop.
- do_not_edit_without_migration_plan: `src/server/worker_registry.rs` and the
  worker starts in `src/server/mod.rs`.
- active_callsite_coverage: partial. Cluster identity and heartbeat are persisted
  through `src/server/cluster.rs`, and `src/server/worker_registry.rs` now
  classifies supervised workers as `leader_only` or `worker_local` before
  startup. `policy_tick_loop` already uses a PG advisory lock at
  `src/server/mod.rs:297`, and `github_sync_loop` uses one at
  `src/server/mod.rs:2798`; leader lease loss still needs per-loop self-fencing
  before every side effect is considered failover-safe.
- invariants: `singleton_on_leader`, `pg_lease_backed_claim`.
- allowed_changes: `bugfix` for existing workers. `new_feature` workers must add
  a leader-only, lease-backed, or worker-local classification in the same change.
- tests: leader failover, duplicate singleton worker suppression, and the #884
  chaos suite.
- related_issues: #876, #877, #878, #884.

### `multinode / merge_review_side_effects`

- feature: `multinode / merge_review_side_effects`
- canonical_modules: `policies/merge-automation.js:46` handles terminal cards,
  `policies/merge-automation.js:522` retries direct pushes, and
  `policies/merge-automation.js:1761` enables auto-merge. GitHub issue mutation
  lives in `src/github/mod.rs:166`, `src/github/mod.rs:218`,
  `src/github/dod.rs:122`, and the issue creation route at
  `src/server/routes/github.rs:212`.
- legacy_modules: none. The JS policy is the active automation surface.
- do_not_edit_without_migration_plan: `policies/merge-automation.js` merge,
  PR-tracking, review-gate, and worktree cleanup paths.
- active_callsite_coverage: single-node policy engine. The current flow assumes
  one automation owner is allowed to run `git push`, `gh issue edit`, `gh issue
  create`, and `gh pr merge --auto`.
- invariants: `singleton_on_leader`,
  `merge_gate_tested_head_sha_phase_evidence`.
- allowed_changes: `bugfix` only before #877/#883. Any new merge/review side
  effect must be leader-only and must preserve tested head SHA checks.
- tests: merge gate tested-head-SHA regression, required phase-evidence
  regression, and two-node duplicate-merge prevention.
- related_issues: #877, #881, #882, #883, #884.

### `multinode / tmux_provider_sessions`

- feature: `multinode / tmux_provider_sessions`
- canonical_modules: `src/services/discord/mod.rs:538` owns the in-process tmux
  watcher registry, `src/services/discord/router/message_handler.rs:874` builds
  the ADK session key, `src/services/discord/router/message_handler.rs:1266`
  posts working status, `src/services/discord/router/message_handler.rs:1349`
  stores the session key in inflight state, and
  `src/services/discord/router/message_handler.rs:1420` passes provider
  execution context into provider CLI execution. Claude local tmux execution is
  at `src/services/claude.rs:1166` and new local tmux session creation is at
  `src/services/claude.rs:1300`.
- legacy_modules: none. Provider execution is local to the node that owns the
  tmux session.
- do_not_edit_without_migration_plan: watcher ownership and relay coordination in
  `src/services/discord/mod.rs`, Discord turn dispatch in
  `src/services/discord/router/message_handler.rs`, and provider tmux wrappers.
- active_callsite_coverage: node-local. Session rows and inflight files help
  recovery, but the live tmux pane, FIFO, output JSONL, watcher handle, and relay
  slot are process/node-local.
- 2026-05-18 refresh: SSH-direct TUI prompt relay is still node-local. Codex
  wrapper JSONL and rollout JSONL carry separate output paths/offsets, and
  watcher ownership must be claimed by both tmux session and output path before
  any future multinode routing can move this surface.
- 2026-05-30 audit note (#2896): stale thread-session GC in
  `runtime_bootstrap` may reap only session keys whose tmux name parses as a
  thread channel, belongs to the current runtime owner marker, and exists in the
  local tmux server. Fixed/main-channel sessions are not reap candidates for
  this path.
- invariants: `heartbeat_capability_registry_routing`,
  `resource_locks_before_exclusive_editor_test`.
- allowed_changes: `bugfix`, or `new_feature` only when routed through the
  capability registry planned by #879.
- tests: node-local routing, worker heartbeat expiry, and provider-session
  failover/recovery assertions.
- related_issues: #876, #879, #880, #884.

### `multinode / mcp_routing`

- feature: `multinode / mcp_routing`
- canonical_modules: provider MCP availability and sync are in
  `src/services/mcp_config.rs:34`, `src/services/mcp_config.rs:71`, and
  `src/services/mcp_config.rs:128`. Discord checks MCP capability at
  `src/services/discord/router/message_handler.rs:1048`. Memento MCP sessions
  are cached and retried in `src/services/memory/memento.rs:262` and
  `src/services/memory/memento.rs:275`. The local credential watcher starts at
  `src/services/discord/runtime_bootstrap.rs:1088` and runs its notify thread at
  `src/services/discord/mcp_credential_watcher.rs:349`.
- legacy_modules: none.
- do_not_edit_without_migration_plan: provider-specific MCP config mutation in
  `src/services/mcp_config.rs` and Memento MCP HTTP session reuse in
  `src/services/memory/memento.rs`.
- active_callsite_coverage: node-local. Runtime MCP configs and cached MCP
  session IDs are local to the provider/node combination that makes the call.
- invariants: `heartbeat_capability_registry_routing`.
- allowed_changes: `bugfix`; `new_feature` only with explicit provider/node
  capability routing.
- tests: node-local MCP routing, credential-change behavior on a worker node, and
  expired-worker routing rejection.
- related_issues: #876, #879, #884.

### `multinode / dispatch_outbox`

- feature: `multinode / dispatch_outbox`
- canonical_modules: `src/server/worker_registry.rs:206` registers the worker,
  `src/server/routes/dispatches/outbox.rs:248` claims PostgreSQL rows with
  `FOR UPDATE SKIP LOCKED`, `src/server/routes/dispatches/outbox.rs:596`
  processes a batch, `src/server/routes/dispatches/outbox.rs:654` relies on the
  Discord delivery reservation guard, and `src/server/routes/dispatches/outbox.rs:719`
  applies retry/permanent-failure state.
- legacy_modules: SQLite test-only fallback paths in
  `src/server/routes/dispatches/outbox.rs` remain behind
  `legacy-sqlite-tests`.
- do_not_edit_without_migration_plan:
  `src/server/routes/dispatches/outbox.rs` claim, notify, followup, status
  reaction, retry, and failure paths.
- active_callsite_coverage: partial. The PostgreSQL claim path prevents two
  consumers from processing the same pending row concurrently, but #878 still
  owns explicit PG lease/idempotency semantics for multinode delivery.
- invariants: `pg_lease_backed_claim`, `singleton_on_leader`.
- allowed_changes: `bugfix` before #878; `new_feature` only if the new action has
  an idempotency key and a tested lease/claim story.
- tests: duplicate outbox claim prevention, retry idempotency, leader failover
  while rows are processing, and two-node Discord delivery dedupe.
- related_issues: #877, #878, #884.

### `multinode / memory_cache`

- feature: `multinode / memory_cache`
- canonical_modules: backend selection is in `src/services/memory/mod.rs:159`;
  local memory reads are in `src/services/memory/local.rs:10`; Memento
  dedupe/cache state is a process-wide `OnceLock` at
  `src/services/memory/memento_throttle.rs:128`; recall and remember cache
  helpers are at `src/services/memory/memento_throttle.rs:175` and
  `src/services/memory/memento_throttle.rs:196`. Other process-local telemetry
  includes observability globals in `src/services/observability/mod.rs:460` and
  `src/services/observability/metrics.rs:238`.
- legacy_modules: none.
- do_not_edit_without_migration_plan: process-global memory and observability
  cache state, especially `memento_throttle.rs` and observability global
  registries.
- active_callsite_coverage: node-local. These caches are optimization and
  telemetry state, not cluster consensus.
- invariants: `heartbeat_capability_registry_routing`.
- allowed_changes: `bugfix`; cluster-visible memory changes must use a durable
  store or clearly document node-local semantics.
- tests: local-cache isolation across workers, telemetry capture per node, and
  worker restart without stale capability routing.
- related_issues: #876, #879, #884.

## Single-Node Assumptions

| assumption | owning module | current risk |
| --- | --- | --- |
| Discord gateway singleton | `src/services/discord/runtime_bootstrap.rs:1526`, `src/services/discord/runtime_bootstrap.rs:1531`, `src/services/discord/runtime_bootstrap.rs:1730` | Two nodes starting a gateway for the same provider can duplicate command intake, watcher startup, and shutdown cleanup unless #877 fences gateway ownership to the leader. |
| Supervised workers singleton | `src/server/worker_registry.rs:151`, `src/server/mod.rs:202`, `src/server/mod.rs:209` | Cluster-enabled worker nodes skip `leader_only` supervised workers unless they hold the startup leader lease; lease-loss self-fencing for already-running loops remains follow-up work. |
| Merge/review side effects local | `policies/merge-automation.js:46`, `policies/merge-automation.js:522`, `policies/merge-automation.js:1761` | Duplicate policy runners can race direct pushes, PR auto-merge, review notifications, and worktree cleanup. |
| GitHub issue/body mutation local | `src/github/mod.rs:166`, `src/github/mod.rs:218`, `src/github/dod.rs:122`, `src/server/routes/github.rs:275` | Multiple nodes can create, close, comment, or edit issue bodies unless calls are leader-only or idempotent. |
| Tmux/provider sessions local | `src/services/discord/mod.rs:538`, `src/services/discord/router/message_handler.rs:1420`, `src/services/claude.rs:1166`, `src/services/claude.rs:1300` | Live provider state depends on local tmux panes, FIFOs, output files, watcher handles, and wrapper processes. |
| MCP routing local | `src/services/mcp_config.rs:34`, `src/services/mcp_config.rs:71`, `src/services/memory/memento.rs:262`, `src/services/discord/mcp_credential_watcher.rs:349` | MCP availability, config mutation, cached MCP session IDs, and credential watcher notifications are node/provider local. |
| `dispatch_outbox` local retry loop | `src/server/worker_registry.rs:206`, `src/server/routes/dispatches/outbox.rs:248`, `src/server/routes/dispatches/outbox.rs:596`, `src/server/routes/dispatches/outbox.rs:719` | Current PG claim narrows duplicate row processing, but multinode delivery still needs explicit lease/idempotency tests before multiple nodes drain it. |
| Memory/cache local | `src/services/memory/local.rs:10`, `src/services/memory/memento_throttle.rs:128`, `src/services/memory/memento_throttle.rs:175`, `src/services/observability/metrics.rs:238` | Process-local recall, dedupe, and telemetry caches must not be treated as cluster truth. |

## Leader-Only Side Effects

- GitHub merge and auto-merge: `policies/merge-automation.js:46`,
  `policies/merge-automation.js:522`, `policies/merge-automation.js:1761`.
- GitHub issue/body mutation: `src/github/mod.rs:166`,
  `src/github/mod.rs:218`, `src/github/dod.rs:122`,
  `src/server/routes/github.rs:275`.
- Singleton policy ticks and global hook side effects: `src/server/mod.rs:284`,
  `src/server/mod.rs:297`, `src/server/mod.rs:367`,
  `src/server/mod.rs:379`.
- Global cleanup and maintenance jobs registered as supervised workers:
  `src/server/worker_registry.rs:177`, `src/server/worker_registry.rs:206`,
  `src/server/worker_registry.rs:219`.
- `dispatch_outbox` delivery if the action is not proven lease-backed and
  idempotent: `src/server/routes/dispatches/outbox.rs:248`,
  `src/server/routes/dispatches/outbox.rs:654`.

## Worker-Local Side Effects

- Provider CLI and tmux execution: `src/services/claude.rs:1166`,
  `src/services/claude.rs:1300`, `src/services/codex_tmux_wrapper.rs:139`,
  `src/services/qwen_tmux_wrapper.rs:183`.
- Node-local MCP calls and credentials: `src/services/mcp_config.rs:71`,
  `src/services/mcp_config.rs:128`, `src/services/memory/memento.rs:275`,
  `src/services/discord/mcp_credential_watcher.rs:349`.
- Local resource locks for exclusive editor/test execution: #880 owns the durable
  lock implementation; the lock acquisition is worker-local but must be recorded
  in PG before the local tool starts.
- Deterministic test phase evidence: #881 starts at
  `src/server/test_phase_runs.rs`, `migrations/postgres/0033_test_phase_runs.sql`,
  and `/api/cluster/test-phase-runs*`. `start` acquires the durable resource
  lock and records a running row; `complete` records terminal evidence and can
  release the lock. Each phase/head SHA pair has an idempotent evidence row so
  later merge gates can require the exact tested commit before accepting a phase.
- Local telemetry capture and cache state: `src/services/memory/memento_throttle.rs:128`,
  `src/services/observability/mod.rs:460`, `src/services/observability/metrics.rs:238`.
- #3262 Claude auto-compact `/compact` injection trigger state:
  `src/services/claude_compact_trigger.rs`. The once-per-fill-cycle "armed" latch
  is a **worker-local** process-global `LazyLock<Mutex<HashMap<channel_id, bool>>>`
  and the injection drives the node-local tmux pane via
  `claude_tui::input::send_followup_prompt`. A channel's live tmux session is
  pinned to a single worker (see `tmux_provider_sessions`), so the per-channel
  latch never needs cross-node coordination; a worker restart simply re-arms the
  channel (re-injecting at most one redundant `/compact` is harmless and idle-gated).

## Required Invariants

### `singleton_on_leader`

- Only the elected/leased leader may run cluster-wide side effects: Discord
  gateway, GitHub merge/issue mutation, singleton policy ticks, global cleanup,
  and any unleased outbox delivery.
- Current anchors: gateway lease keepalive at
  `src/services/discord/runtime_bootstrap.rs:1531`, policy tick advisory lock at
  `src/server/mod.rs:301`, GitHub sync advisory lock at `src/server/mod.rs:2818`,
  server cluster leader lease bootstrap at `src/server/cluster.rs`, and
  leader-only worker self-fence at `src/server/worker_registry.rs:522`.
- Enablement condition: every supervised worker in
  `src/server/worker_registry.rs:151` is classified as leader-only,
  lease-backed multi-consumer, or worker-local.

### `pg_lease_backed_claim`

- Any durable queue that can be drained by more than one node must claim work in
  PostgreSQL, record ownership/attempt state, and make delivery idempotent.
- Current anchors: `dispatch_outbox` records `claimed_at`/`claim_owner` through
  `migrations/postgres/0030_dispatch_outbox_claims.sql`, uses `FOR UPDATE SKIP
  LOCKED` plus stale processing reclaim at `src/server/routes/dispatches/outbox.rs:250`,
  calls delivery with a reservation guard at `src/server/routes/dispatches/outbox.rs:681`,
  and clears claim fields on done/retry/failure.
- Enablement condition: #878 still needs the same claim model on
  `task_dispatches`; `dispatch_outbox` now has a stale-claim regression test.

### `resource_locks_before_exclusive_editor_test`

- Any exclusive editor, simulator, or hardware-bound test must acquire a durable
  `resource_locks` record before launching local work, and must release or expire
  that record on worker death.
- Current anchors: auto-queue slots already use PG compare-and-set style claims
  through `src/db/auto_queue/claim.rs:316` and release ownership through
  `src/db/auto_queue/slots.rs:98`; slot cleanup clears local runtime state
  through `src/services/auto_queue/runtime.rs:146`; #880 owns the missing
  resource-lock table and API.
- Enablement condition: two workers contending for the same Unreal editor/test
  resource cannot run the exclusive phase concurrently.

### `heartbeat_capability_registry_routing`

- The dispatcher must route provider, MCP, and tool work only to live workers
  that advertised the required capability. Expired heartbeats must remove the
  worker from routing before new work is assigned.
- Current anchors: `worker_nodes` stores `labels` and `capabilities` through
  `src/server/cluster.rs`; provider execution context carries node-local details at
  `src/services/discord/router/message_handler.rs:1420`; MCP capability checks
  are local in `src/services/mcp_config.rs:34`; tmux watcher state is in-process
  at `src/services/discord/mod.rs:538`.
- Enablement condition: #876/#879 add worker heartbeats, capability rows, and a
  dispatcher path that rejects stale workers.

### `merge_gate_tested_head_sha_phase_evidence`

- A merge may only proceed when the required phase evidence exists for the same
  head SHA that will be merged.
- Current anchors: PR tracking treats `head_sha` as authoritative at
  `policies/00-pr-tracking.js:37`; latest completed work head SHA is loaded at
  `policies/merge-automation.js:271`; merge readiness rejects tracked/current
  SHA mismatch at `policies/merge-automation.js:1190`.
- Enablement condition: #881/#882/#883 define phase-run evidence and make
  merge-automation require that evidence before direct merge or auto-merge.

## Multinode Issue Map

| issue | invariant coverage |
| --- | --- |
| #876 `[multinode 1] worker_nodes + cluster role/heartbeat bootstrap` | `singleton_on_leader`, `heartbeat_capability_registry_routing` |
| #877 `[multinode 2] leader-only singleton fence for supervised workers and merge side effects` | `singleton_on_leader` |
| #878 `[multinode 3] task_dispatches / dispatch_outbox PG lease claim + idempotency` | `pg_lease_backed_claim`, `singleton_on_leader` |
| #879 `[multinode 4] worker capability registry + node-local MCP routing` | `heartbeat_capability_registry_routing` |
| #880 `[multinode 5] Unreal resource_locks for exclusive editor/test execution` | `resource_locks_before_exclusive_editor_test`, `heartbeat_capability_registry_routing` |
| #881 `[multinode 6] Unreal test_phase_runs + deterministic phase runner` | `resource_locks_before_exclusive_editor_test`, `merge_gate_tested_head_sha_phase_evidence` — evidence store/API and lock-backed start/complete runner API added |
| #882 `[multinode 7] issue_specs + Issue-as-Spec / phase-plan generation` | `merge_gate_tested_head_sha_phase_evidence` |
| #883 `[multinode 8] merge gate: required phase evidence + tested head SHA` | `merge_gate_tested_head_sha_phase_evidence`, `singleton_on_leader` |
| #884 `[multinode 9] two-node nightly regression / chaos suite for MacBook + Mac mini` | all invariants |

## Tests Required Before Enablement

- Two-node nightly regression (#884): CI runs `multinode_regression::` plus
  merge-gate policy tests nightly; the physical MacBook + Mac mini smoke
  procedure lives in `docs/agent-maintenance/multinode-two-node-smoke.md`.
- Worker heartbeat expiry (#876/#879): start a worker with provider and MCP
  capabilities, stop heartbeats, and assert new work is not routed to it after
  the expiry window.
- Leader failover (#877): kill the leader while policy tick, GitHub sync, and
  `dispatch_outbox` have pending work; assert exactly one replacement leader
  takes ownership and no duplicate merge/issue mutation occurs.
- Duplicate outbox claim prevention (#878): run two workers draining
  `dispatch_outbox`; inject a crash after claim and before completion; assert the
  row is retried once, delivery remains idempotent, and retry counts stay in
  bounds.
- Node-local MCP routing (#879): advertise different MCP capabilities on two
  workers; dispatch an MCP-dependent turn; assert the selected worker has the
  capability and the non-capable worker never receives the MCP call.
- Resource lock contention (#880/#881): enqueue two exclusive Unreal editor/test
  phases for the same resource; assert one acquires the PG lock and the other
  waits, fails fast, or is rescheduled according to the phase plan.
- Merge gate phase evidence (#881/#882/#883): create phase evidence for an old
  head SHA, advance the PR head, and assert direct merge/auto-merge is blocked
  until evidence exists for the new head SHA.

## Updating This Page

- Update this page in the same PR that changes any owning module listed above.
- When #876 through #884 land, replace "single-node" or "partial" coverage notes
  with the concrete module and test anchors that prove the invariant.
- If a new worker, durable queue, provider, MCP integration, or exclusive test
  resource is added, classify it as leader-only, PG-lease-backed, or worker-local
  before merging.

### Audited touches

- #3038 S1 (SharedData `QueuedPlaceholderState` extraction): `runtime_bootstrap.rs`
  changed in two helpers only — `run_bot_build_shared_data` (three consecutive
  queued-placeholder members wrapped into the new `queued:` group field;
  initialization expressions and their evaluation order byte-identical) and
  `run_bot_spawn_recovery_and_flush_restart_reports` (mechanical `.queued.`
  field-path rewrite). Pure behavior-preserving extraction. **Worker-local**:
  no leader-only side effect, no durable queue, no PG lease — multinode
  assumptions unchanged.
- #3082 (queued-card / answer-flush barrier): `runtime_bootstrap.rs` changed
  only to initialize the new in-process `answer_flush_barrier` field on
  `SharedData`. The barrier is **worker-local** (a per-process, per-channel
  in-memory gate guarding queued-card ordering) — it owns no leader-only side
  effect, no durable queue, and no PG lease, so it introduces no new multinode
  ownership/singleton/lease assumption.
- #3038 (`run_bot` god-function decomposition): `runtime_bootstrap.rs` changed
  by a **pure, behavior-preserving extraction** of `run_bot`'s inline
  leader-only background-spawn block into six `run_bot_spawn_*` free helpers
  (deferred_restart_poller, skills_hot_reload, recovery_and_flush_restart_reports,
  upload_cleanup, stale_session_gc, voice_auto_join). The leader/standby gating,
  the gateway-lease acquisition order, every `tokio::spawn` point, and all
  captured clones are **unchanged** — each helper recreates the same named clones
  internally and is invoked at the identical position in `run_bot`. No new
  multinode ownership, singleton, or lease assumption is introduced; the
  leader-only vs worker-local classification of every spawned task is preserved.
- #3038 (`execute_streaming_local_tui_tmux` god-function decomposition, follow-up
  slice to #3196): `claude.rs` changed by a **pure, behavior-preserving
  extraction** of the orchestrator's inline fresh-turn dispatch + completion gate
  into one `run_claude_tui_fresh_turn_and_finalize` free helper (skip-stale-bytes
  offset, ready-retry fresh-turn run, and the three terminal outcomes:
  start-failure, `SessionDied`, and delivery with watcher handoff + producer-exit
  log). Every original `return Ok/Err` and the audit + exit-reason + kill +
  owner-marker cleanup ordering are **unchanged** — the helper is invoked at the
  identical position as a tail call and returns the same `Result` directly. The
  tmux session ownership marker, turn lock, and termination-audit side effects are
  all **process-local / per-session**; no new multinode ownership, singleton, or
  lease assumption is introduced.
- #3038 (`health.rs` send-to-agent dispatch decomposition): the agent-to-agent
  relay entry point (`handle_send_to_agent` + `parse_send_to_agent_body` +
  `ParsedSendToAgentRequest`) was moved by a **pure, behavior-preserving
  extraction** from `services::discord::health` into the new
  `services::discord::outbound::send_to_agent` module. The new module is a
  **stateless** request parser/router: it owns no global state, no durable queue,
  and no lease, and it still delivers through the unchanged
  `health::send_message_with_backends`. Call sites (`route_request_generate.rs`,
  `health_api.rs`) were re-pointed to the new path with identical arguments. No
  new multinode ownership, singleton, or lease assumption is introduced.
- #3142 (committed-output turn-aliasing safety): `tmux_watcher.rs` changed by
  adding one **pure** decision helper
  (`committed_anchor_cleanup_is_stale_for_newer_turn`, the id==0-inclusive sibling
  of #3141's `committed_completion_is_stale_for_newer_turn`) and gating four
  consumers of the committed-output block (dispatch finalization, TUI history
  push, the two anchor-cleanup branches, and the status-panel completion
  identity) on the offset-pinned stale test so an older committed range cannot
  act on a NEWER same-session turn's inflight identity. The gate is computed
  purely from the **process-local** in-memory inflight snapshots
  (`inflight_before_relay` / late-read `inflight_state`) and the per-session JSONL
  `current_offset` already owned by THIS watcher loop — it reads no cross-node
  state, acquires no lease, and changes no leader/standby ownership. The watcher
  loop remains **session-owner-local** (a watcher only processes a tmux session
  it owns); this change only narrows which in-process side-effects fire on a
  turn boundary and introduces no new multinode ownership/singleton/lease
  assumption.
- #3016 phase-5a (reconciler far-backstop enabler): `turn_finalizer.rs` changed
  by arming a generous `WATCHER_REGISTER_BACKSTOP` (the legacy 1800s
  placeholder-sweeper horizon) on watcher-owned `register_start` Pending entries
  and adding a reconciler pass that finalizes them ONLY after a liveness
  re-check (`watcher_backstop_turn_is_terminal`: never a `PausedLive`/paused/
  pane-busy turn; `Unknown` non-JSONL runtimes gated on pane-idle). The
  `TurnFinalizer` actor is already **worker-local** (one actor per in-process
  `SharedData`, owning its ledger and a `Weak<SharedData>`); the backstop reads
  only this process's ledger plus the local tmux pane/transcript via the
  existing `tmux_watchers` registry and `tui_turn_state`/`provider` probes. It
  acquires no lease, touches no durable queue, and changes no leader/standby
  ownership — finalize remains the same per-process exactly-once unit. No new
  multinode ownership/singleton/lease assumption is introduced. (codex HIGH
  follow-up: the reconcile cache `Weak<SharedData>` is now primed at the FIRST
  `register_start` — the `Start` message carries a `Weak` downgraded from the
  caller's `Arc<SharedData>` — instead of only at the first `Terminal`, so the
  far-backstop is deterministic for a fresh worker-local actor whose first
  watcher turn never submits its own terminal. Still worker-local `Weak`, no
  cross-node reference.)
- #3016 phase-5b1 (make `mailbox_finalize_owed` write-only by replacing its
  CONSUMERS, not removing it): two consumer rewrites, both behaviour-equivalent
  to today. (1) `tmux_watcher.rs` — the fresh-idle `Unknown` (non-JSONL runtime)
  arm no longer reads the flag to gate finalize; it routes to the SAME pane-idle
  `Finalize` path as `Done` (`watcher_session_ready_for_input` — the SAME
  `pane_ready_fallback_allowed && tmux_session_ready_for_input` predicate the 5a
  far-backstop uses for `Unknown` — is already proven at this arm), so an empty
  `Unknown` completion finalizes promptly instead of at the 1800s far-backstop;
  the paused-live/paused/epoch/stale-for-newer-turn race guards are kept. (2)
  `turn_bridge/mod.rs` — the `bridge_handoff_finds_watcher_handle` invariant now
  queries the ledger via `turn_finalizer.has_live_watcher_pending(channel_id,
  current_generation)` instead of loading `mailbox_finalize_owed`; the two are
  equivalent because `owed` is set atomically with the
  `register_start(.., RelayOwnerKind::Watcher)` keyed by the same channel/
  generation. Both consumers stay **worker-local**: the watcher reads only the
  local tmux pane/transcript, and `has_live_watcher_pending` is a read-only query
  of THIS process's actor-owned ledger (the `TurnFinalizer` is one actor per
  in-process `SharedData`). No lease, no durable queue, no leader/standby
  ownership change — finalize remains the same per-process exactly-once unit. No
  new multinode ownership/singleton/lease assumption. (The flag field/producers
  remain until phase-5b2.)
- #3016 phase-5b2 (delete the `mailbox_finalize_owed` flag entirely): pure
  dead-write elimination, behaviour-identical. Phase-5b1 had already replaced
  every finalize-decision CONSUMER, leaving the flag write-only. 5b2 removes: the
  `TmuxWatcherHandle.mailbox_finalize_owed` field (`mod.rs`); both producers — the
  `turn_bridge/mod.rs` runtime-handoff `store(true)` and the legacy `TmuxReady`
  `store(true)`, keeping the adjacent `register_start(RelayOwnerKind::Watcher)`
  ledger authority that already drives the gate-timeout defer; the
  `tui_prompt_relay.rs` dead `publish_tui_direct_watcher_finalize_debt` producer;
  all revoke sites (`tmux.rs` watcher-finalize `store(false)`, the
  `turn_bridge/mod.rs` non-delegation CAS plus its `bridge_published_finalize_owed_for_this_turn`
  tracking var, and the `turn_finalizer.rs` backstop `store(false)`); the residual
  `swap(false)` observability reads in `tmux_watcher.rs` (the `owed_finalize`
  event field is dropped); the now-unreachable `LegacyFlagGated`
  `FreshIdleFinalizeDecision` variant; and the `delegated_finalize_owed` parameter
  of `finish_restored_watcher_active_turn` (redundant under `normal_completion =
  true` at both production call sites). The stale-skip kickoff suppression the
  flag-derived term used to provide is already covered by the live-`has_active_turn`
  gate, so the finalize path is identical. Nothing here is worker-non-local: the
  removed flag was a per-handle in-process atomic, and the surviving authority (the
  per-process actor-owned ledger) is unchanged. No new multinode
  ownership/singleton/lease assumption.
- #3078 (status-panel controller — faithful watcher CREATE/ADOPT shadow-parity):
  completes the create/adopt sub-step PR-4 deferred. `status_panel_controller.rs`
  gains a **pure** `watcher_create_parity_decision` (a `WatcherCreateDecision`
  re-derived from the SAME raw inputs the legacy
  `watcher_should_create_external_input_status_panel` branch reads — no actor
  round-trip, no ledger read, no IO); `watcher_panel_parity.rs` gains the
  `assert_watcher_create_parity` shadow check (independent legacy derivation +
  `debug_assert`/bounded-warn, legacy still executes the real create/adopt IO);
  `tmux_watcher.rs` adds a single net-zero hook at the deferred-comment seam
  (production LoC held at the 9598 freeze, generated docs unchanged). The
  controller stays a **per-process** in-memory actor on `SharedData` (peer of
  `TurnFinalizer`); the parity decision reads only the watcher's process-local
  raw inputs and the still-dormant ledger, performs no Discord IO, acquires no
  lease, and changes no leader/standby ownership. Behaviour-preserving (shadow
  only). No new multinode ownership/singleton/lease assumption.
- #3231 (worktree GC 강화 — `storage.worktree_orphan_sweep`): extends the hourly
  orphan sweep with (A) a GUID-primary resumable keep-set + a runtime naming
  whitelist that protects manual dev worktrees, and (B) a one-level recursion into
  the managed root (`worktrees/<repo_name>/`) that the flat 1-depth scan missed,
  removing terminal dispatch/automation worktrees via the existing
  `cleanup_managed_worktree` guards (dirty/unmerged skip). **Multinode class:
  WORKER-LOCAL maintenance job.** It is one of the `services::maintenance::jobs`
  registered on the dynamic (non-leader) maintenance scheduler (peer of
  `storage.target_sweep` / `reconcile.zombie_resources`), NOT the leader-only
  `worker_registry::MaintenanceScheduler` that owns persistent PG-lease state
  (`voice.turn_link_gc` / `storage.cancel_tombstone_prune`). The sweep reads PG
  read-only (the active-dispatch + resumable-GUID keep-set), probes the
  **process-local** tmux server for live AgentDesk panes (fail-closed on query
  failure), and deletes only directories on the local filesystem under this host's
  `~/.adk/release/worktrees`. It acquires no lease, owns no durable queue, and
  asserts no singleton — each node sweeps its OWN worktree root. The keep/discard
  predicates derive solely from PG rows + this host's tmux + local disk, so the
  job stays worker-local and introduces no new multinode
  ownership/singleton/lease assumption. (Caveat for multinode: the keep-set is
  global PG state but the live-tmux owner check is host-local — a worktree owned by
  a pane on ANOTHER node would not be protected by THIS node's tmux probe; today
  each host only provisions worktrees under its own root, so the sweep never sees
  another node's directories. If worktree roots ever become shared storage, the
  live-owner check would need to fan out cross-node.)
- #3037 (backflow hotfile re-point): `tmux_watcher.rs` changed by a **pure import
  path correction** — the single `global_monitoring_store()` call in the
  suppressed-placeholder monitor-entry-key snapshot now resolves the function via
  its canonical service home (`crate::services::monitoring_store::*`) instead of
  the `crate::server::routes::state::*` compatibility facade (which re-exports the
  same symbol). The resolved function, the per-node in-memory `MonitoringStore`,
  and every call argument are **byte-identical**; the store remains
  **process-local** (one in-memory `Arc<Mutex<MonitoringStore>>` per node, no PG
  lease, no durable queue, no leader-only side effect). No behavior, ownership,
  singleton, or lease assumption changes — this is a layering/import fix only.
- #3037 (thread_reuse backflow relocation): the Postgres/Discord-API thread-map
  helpers (`get_thread_for_channel_pg`, `get_mapped_thread_for_channel_pg`,
  `set_thread_for_channel_pg`, `set_thread_for_channel_map_only_pg`,
  `clear_thread_for_channel_pg`, `try_reuse_thread`, and
  `validate_channel_thread_maps_on_startup_with_backends`) were moved by a
  **pure, behavior-preserving relocation** from
  `server/routes/dispatches/thread_reuse.rs` into the new
  `services/dispatches/discord_delivery/thread_reuse.rs`; the axum route handlers
  (`get_card_thread`, `link_dispatch_thread`, `get_pending_dispatch_for_thread`)
  stay in the route layer and now consume the relocated helpers. `runtime_bootstrap.rs`
  changed only by re-pointing its startup thread-map validation call from the
  `crate::server::routes::dispatches::*` facade to the relocated
  `crate::services::dispatches::discord_delivery::*` home with **byte-identical**
  arguments. The thread-map state these helpers read/write is the per-card
  `kanban_cards.channel_thread_map`/`active_thread_id` columns in **shared
  Postgres** — already authoritative and node-agnostic — and every SQL statement,
  Discord-API probe, and clear/reuse decision is unchanged. No new multinode
  ownership, singleton, or lease assumption is introduced; this is a layering/move
  fix only.
- #3248 (deploy-survivable relay, gap-1): `recovery_engine.rs` changed by adding
  `reseed_watcher_owned_finalizer_ledger` and calling it from the two success
  paths of `reregister_active_turn_from_inflight` (the pane-alive reattach run by
  recovery after a mid-turn dcserver restart). The new call re-seeds the
  **in-process** single-authority finalizer ledger (`turn_finalizer` actor) with
  the watcher-owned `register_start` that the in-memory ledger lost on restart, so
  the watcher's gate-timeout arms its backstop instead of finalizing-as-orphan.
  The finalizer ledger is **worker-local** (a per-process in-memory map owned by
  the watcher/recovery on the SAME node that holds the live tmux pane) — it owns
  no leader-only side effect, no durable queue, and no PG lease — so re-seeding it
  introduces no new multinode ownership/singleton/lease assumption. The register
  is idempotent vs. a later bridge handoff and only ever seeds a full-identity
  Watcher entry (id-0 guarded); the normal non-restart path is unaffected.
- #3256 (incremental operator-prose relay): `tui_prompt_relay.rs` changed so the
  Claude external-input idle path STREAMS the operator's prose THROUGH a single
  bridge turn (`stream_tui_idle_response_through_bridge` +
  `forward_idle_stream_into_bridge`) instead of pre-collecting the whole response
  and posting one batched `[Text{full}, Done]` at turn end. The transcript
  reader, the bridge `(tx, rx)`, the intake placeholder, and `spawn_turn_bridge`
  are all **worker-local** — they run on the SAME node that holds the live tmux
  pane, exactly as the prior collect-then-send path did; the change only moves
  WHEN frames reach the in-process bridge (live vs. batched), not WHO owns the
  relay. The single-card / single-`spawn_turn_bridge` per-turn invariant and the
  exactly-once bridge finalize (terminal `Done`, "first wins") are preserved. The
  `committed_relay_offset` clamp (read-side dedupe) and the runtime-binding
  offset advance on success (write-side ledger) are unchanged — no new multinode
  ownership, singleton, or lease assumption is introduced. Classification:
  **worker-local relay path**.
- #3263 (Codex context-window fallback): `provider.rs` changed by adding a
  **pure** max-of-cache fallback to Codex context-window resolution
  (`codex_context_window_from_cache`: exact slug → max-of-cache on slug drift →
  documented `CODEX_FALLBACK_CONTEXT_WINDOW` last-resort), documenting each
  provider's hardcoded `default_context_window` intent, and a unit-test module.
  The resolver is a **worker-local** read of the local CLI cache
  (`~/.codex/models_cache.json`) on the node that owns the session — it owns no
  global state, no durable queue, and no lease, and touches no PG-lease/leader
  path. No new multinode ownership, singleton, or lease assumption is introduced.
- #3296 (aborted-anchor reaction reconcile): the synthetic turn-start ABORT path
  (`tui_prompt_relay.rs` / `tui_direct_pending_start.rs`) now records a durable
  `AbortedAnchorMarker` under the new node-local
  `runtime/discord_tui_direct_abort_marker/` root instead of swapping `⏳ → ⚠`;
  the watcher terminal-commit chokepoint (`tmux_watcher.rs`) drains it `⏳ → ✅`
  on a covering commit and the placeholder sweeper applies the TTL'd `⏳ → ⚠`
  fallback (`tui_direct_abort_marker.rs`). Codex r2 adds a sibling node-local
  `runtime/discord_tui_direct_commit_tombstone/` root (`CommitTombstone`):
  written only by the same node's watcher chokepoint BEFORE its inflight-row
  clear, read only by that node's ABORT path / sweeper 대조, GC'd by the same
  sweeper — the same worker-local surfaces. **Worker-local**: both stores live
  on the SAME node's filesystem as the pending-start store they mirror, are
  written and drained only by that node's own relay worker / watcher loop /
  sweeper task, and the reaction ops resolve the process-local
  `serenity_http_or_token_fallback()` bot identity — no PG lease, no cross-node
  reads, no leader-only side effect. No new multinode ownership/singleton/lease
  assumption is introduced.
