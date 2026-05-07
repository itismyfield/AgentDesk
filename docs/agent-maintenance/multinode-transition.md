# Multinode Transition

> Source: [`docs/agent-maintenance/index.md`](index.md). Use this page before
> moving any AgentDesk runtime, worker, dispatch, provider, MCP, merge, or test
> execution path from one dcserver node to multiple nodes.
>
> Last refreshed: 2026-05-01 (against #876 worker_nodes bootstrap, #877 leader-only self-fence, and #878 dispatch_outbox claim ownership).

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
