# Multinode Transition

> Source: [`docs/agent-maintenance/index.md`](index.md). Use this page before
> moving any AgentDesk runtime, worker, dispatch, provider, MCP, merge, or test
> execution path from one dcserver node to multiple nodes.
>
> Last refreshed: 2026-06-15 (against `main` @ `9594a4d94`).
>
> Last refreshed: 2026-07-05 (#4089 — `worker_registry.rs` exposes the local RateLimitSync leader-worker active flag (`rate_limit_sync_active`) so the claude-accounts switch endpoint can report whether the receiving node performs usage collection. Read-only exposure: leader election, lease, and singleton ownership assumptions are unchanged; the Keychain auth switch itself is node-local by design (MVP), so non-leader switches surface `rate_limit_sync_not_active_on_this_node` instead of racing the leader loop.)
>
> Last refreshed: 2026-07-11 (#4424 — message_outbox source authorization and leader-owned durable failed-row recovery).
>
> Last refreshed: 2026-07-11 (manual: scheduled-message leader worker ownership and touch gate).
>
> PR #3456 made the `src/server/worker_registry.rs` worker-lifecycle log fields
> consistent: every started / stopped / future-exited / self-fenced /
> supervisor-shutdown tracing event now emits the same structured spec fields
> (stage, order, restart, shutdown, owner, health, responsibility, notes), so
> failover/observability correlation across nodes no longer depends on which
> lifecycle edge logged it.

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
- canonical_modules: `src/services/discord/runtime_bootstrap.rs` preserves the
  bootstrap call order; `runtime_bootstrap/gateway_runtime.rs` builds the
  Serenity client and enters the leader gateway event loop;
  `runtime_bootstrap/gateway_lease.rs` owns gateway lease acquisition,
  keepalive, and self-fencing; `runtime_bootstrap/shutdown.rs` owns SIGTERM
  persistence and gateway backend execution; `runtime_bootstrap/intake.rs` owns
  the standby intake-worker spawn.
- legacy_modules: none. The current gateway owner is the active dcserver process
  for that provider.
- do_not_edit_without_migration_plan:
  `src/services/discord/runtime_bootstrap.rs` gateway startup order plus
  `src/services/discord/runtime_bootstrap/gateway_runtime.rs`,
  `src/services/discord/runtime_bootstrap/gateway_lease.rs`, and
  `src/services/discord/runtime_bootstrap/shutdown.rs` lease/shutdown paths,
  especially watcher cancellation on gateway lease loss.
- active_callsite_coverage: single-node runtime with an existing gateway lease.
  It does not yet define cluster role discovery, worker heartbeats, or a
  capability registry for non-gateway workers.
- 2026-05-18 audit note (#2558): stale thread-session GC in
  `runtime_bootstrap` now calls the leader runtime's Postgres pool directly
  instead of looping back through the internal HTTP cleanup route. The singleton
  assumption remains unchanged: the task is still spawned from the leased
  gateway runtime.
- 2026-06-12 audit note (#3089 S0; updated #3560): `runtime_bootstrap` only
  initializes the `single_message_panel` flag for startup logging. Since #3560
  the flag is default-ON (opt-out via `AGENTDESK_SINGLE_MESSAGE_PANEL=0|false`).
  Gateway lease, startup order, worker ownership, and singleton assumptions are
  unchanged.
- 2026-06-17 audit note (#3548): PR analyzer hygiene guard work is confined to
  `scripts/analyze_prs.py`; gateway lease, startup order, worker ownership, and
  singleton assumptions are unchanged.
- 2026-06-17 audit note (#3546): SQLite rowid compatibility work is confined to
  `src/engine/ops/db_ops.rs`; gateway lease, startup order, worker ownership,
  and singleton assumptions are unchanged.
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
  `src/server/mod.rs:208` starts workers after boot reconcile. The
  `ScheduledMessages` spec registers `scheduled_message_loop` as
  `WorkerExecutionScope::LeaderOnly`; its durable claim/recovery implementation
  lives in `src/services/scheduled_messages.rs` and
  `src/db/scheduled_messages.rs`.
- legacy_modules: none. Workers are centrally registered, but most entries still
  assume that every server process may start its local loop.
- do_not_edit_without_migration_plan: `src/server/worker_registry.rs` and the
  worker starts in `src/server/mod.rs`. Scheduled-message ownership changes must
  review `src/services/scheduled_messages.rs`,
  `src/db/scheduled_messages.rs`, and the `ScheduledMessages` registry spec
  together; do not make the loop worker-local without a replacement ownership
  and Discord side-effect plan.
- active_callsite_coverage: partial. Cluster identity and heartbeat are persisted
  through `src/server/cluster.rs`, and `src/server/worker_registry.rs` now
  classifies supervised workers as `leader_only` or `worker_local` before
  startup. `policy_tick_loop` already uses a PG advisory lock at
  `src/server/mod.rs:297`, and `github_sync_loop` uses one at
  `src/server/mod.rs:2798`; leader lease loss still needs per-loop self-fencing
  before every side effect is considered failover-safe. Scheduled messages are
  leader-started and additionally fence each delivery attempt with a Postgres
  lease, a per-attempt `claim_token`, and a durable fire-slot uniqueness key.
- invariants: `singleton_on_leader`, `pg_lease_backed_claim`.
- allowed_changes: `bugfix` for existing workers. `new_feature` workers must add
  a leader-only, lease-backed, or worker-local classification in the same change.
  Any scheduled-message worker/service ownership change must refresh this page
  in the same change.
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
- legacy_modules: removed SQLite-only fallback paths are historical context;
  current `src/server/routes/dispatches/outbox.rs` behavior is PostgreSQL-first.
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
| Discord gateway singleton | `src/services/discord/runtime_bootstrap.rs`, `src/services/discord/runtime_bootstrap/gateway_lease.rs`, `src/services/discord/runtime_bootstrap/shutdown.rs` | Two nodes starting a gateway for the same provider can duplicate command intake, watcher startup, and shutdown cleanup unless #877 fences gateway ownership to the leader. |
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

- #4234/#4235/#4236 voice connection lifecycle registries:
  `src/services/discord/voice_lifecycle.rs`. Three process-static singletons —
  `lifecycle_router()` (`DashMap<provider, UnboundedSender<ReconnectRequest>>`),
  `rejoin_inflight()` (`DashSet<(provider, guild)>`), and the pre-existing
  `voice_occupancy()` in `commands/voice.rs` — are all **worker-local**. A guild's
  songbird voice connection is pinned to whichever node's bot token holds it
  (Discord allows one voice connection per bot-token per guild), so the rejoin
  supervisor, its in-flight guard, and the occupancy desired-state map never need
  cross-node coordination: a node only supervises the connections it itself owns.
  The `reconnect-degraded` alert dedup reuses the existing per-process
  `voice_notify_dedup` set, so it is likewise once-per-node, not cluster-global.

## Required Invariants

### `singleton_on_leader`

- Only the elected/leased leader may run cluster-wide side effects: Discord
  gateway, GitHub merge/issue mutation, singleton policy ticks, global cleanup,
  and any unleased outbox delivery.
- Current anchors: gateway lease keepalive at
  `src/services/discord/runtime_bootstrap/gateway_lease.rs`, policy tick advisory lock at
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

- #4237 DAVE/E2EE voice-close observability: the existing worker-local
  `DriverDisconnect` handler now classifies Discord voice close codes 4016/4017,
  records a structured counter event, and routes a deduplicated operator alert
  through the existing notify bot. Multinode class: **worker-local likely** —
  the metric, alert dedup, Songbird connection, and rejoin supervisor remain
  pinned to the node/provider that owns the guild voice connection; no shared
  authority, PG lease, or leader-only side effect changes.
- #4249 PostgreSQL bootstrap timeout hardening runs migration/reseed on an eager
  startup pool with a 10s acquire deadline, then eagerly activates the separate
  runtime pool with the original 3s deadline before the shared six-attempt
  retry/alert envelope can succeed. Typed `sqlx::Error::PoolTimedOut` failures
  get timestamped, source-attributed bootstrap diagnostics.
  Classification: **worker-local** — every node owns its own connection pool,
  wait budget, retry loop, and stderr; this changes no shared row, schema,
  leader-only side effect, cross-node routing rule, or PG lease/claim authority.

- #4247 S0 reaction status-only containment removes the guild and DM reaction
  gateway subscriptions plus the only destructive `ReactionRemove` intake
  route. This narrows connection-level event intake on every node; it does not
  change gateway lease acquisition, singleton ownership, worker routing, or
  node-local/shared-Postgres authority. Explicit authenticated `/steer` and
  `/stop` cancellation remain on their existing owners.

- #4424 message_outbox source-contract recovery: the protected
  `GET /api/message-outbox/failed` inspection route is read-only on any control
  node, while `POST /api/message-outbox/failed/redrive` is classified as a
  **leader-owned operator side effect** and the deployment runbook must target
  the active leader. The mutation itself is shared-Postgres durable and
  identity-safe: migration 0081 records a unique
  `(message_outbox_id,idempotency_key)` audit claim, the service locks exact
  requested rows, revalidates the central Loopback source contract, suppresses
  active/sent semantic siblings and duplicate failed identities, and only then
  updates the same failed row to pending. Consequently an accidental retry or
  concurrent call on another node converges to `idempotent_replay`/no-op rather
  than a second pending row or send. The normal message_outbox worker retains
  its existing PG claim-owner fencing; no worker scope, gateway lease, target
  authorization, or worker-local relay ownership changes. Operational live
  redrive remains outside the implementation PR and is performed by the
  orchestrator only after deploy and independent review.

- #4230 S11 turn-bridge helper-zone retirement: the remaining free helpers in
  `turn_bridge/mod.rs` moved verbatim to `retry_state.rs`, `stream_receiver.rs`,
  `activity_heartbeat.rs`, `tmux_runtime.rs`, `cancel_finalize_policy.rs`,
  `panel_lifecycle.rs`, and `thinking.rs`; their inline tests
  moved with the owning behavior. Classification: worker-local — this is a
  module-boundary-only refactor of the existing per-turn receiver, retry,
  heartbeat, panel, cancel, and transcript helpers. It adds no state field,
  database/schema write, PG lease, leader-only effect, cross-node read, or
  singleton assumption; call sites and side-effect ordering are unchanged.
- #4275 watcher/jsonl segment-boundary separator: `tmux_output_stream.rs`
  (WATCHER) `process_watcher_lines_for_turn` now guards the bare
  `full_response.push_str(text)` for `assistant` text blocks — when the running
  response is non-empty and `semantic_boundaries::semantic_chunk_separator_needed`
  reports a genuine sentence boundary (not a decimal, file extension, markdown
  continuation, open inline-code span, or — r2, folded into the shared predicate
  via `text_ends_inside_open_code_fence` so the watcher matches the streaming
  path's `streamed_text_inside_open_code_fence` guard — inside an open ``` code
  fence), it inserts `"\n\n"` before appending.
  This is the watcher-path twin of the #3608 streaming-path separator: Claude
  interleaves prose with `tool_use`, so pre-/post-tool text arrive as two discrete
  events that previously glued into one run-on line. The `content_block_delta`
  intra-block streaming push (qwen `--include-partial-messages`) is deliberately
  left a bare `push_str` so a separator never fractures a single sentence.
  Classification: worker-local — the change only reshapes the per-node watcher's
  in-memory `full_response` assembly before relay; it adds no new inflight-row
  field, delivery record, PG lease, leader gate, or cross-node routing state, and
  no DB/schema change.
- #3805 P2 PR-D two-message rollover re-anchor: `turn_bridge/mod.rs` (SINK) and
  `tmux_watcher.rs` (WATCHER) each re-anchor the separate two-message status panel
  BELOW the new tail answer after a mid-turn answer rollover, gated on the
  default-OFF `two_message_panel_enabled` flag. All logic lives in the non-giant
  siblings `{turn_bridge,tmux_watcher}/two_message_panel.rs` (send the new panel,
  durably pre-register the new panel in the worker-local orphan store, persist or
  bind the new `status_message_id`, retire the stranded old panel, and bump the
  per-turn `status_panel_generation` epoch); the giants carry only thin wiring.
  Classification: worker-local — the epoch bump is persisted through the SAME
  per-`(provider, channel)` inflight sidecar flock the create already used (sink:
  `save_inflight_state` before old-panel delete; watcher: atomic
  `bind_status_panel` with expected old panel id + in-lock generation bump), which
  is worker-local runtime state, not a PG lease / leader gate / cross-node routing
  field. The watcher re-anchor gate now also requires the loaded inflight row to
  be watcher-panel-eligible, so Managed bridge-owned turns delegated to watcher
  relay cannot hijack the bridge-owned panel. The generation epoch and
  `status_message_id` are pre-existing persisted inflight fields (PR-A/B/C);
  PR-D adds no new field, delivery record, or schema change, and item4's
  fire-and-forget session banner (`session_banner.rs`) is untouched. Delete
  failures fall back to the existing durable status-panel orphan store (also
  worker-local). OFF path is byte-identical.
- #3038 (b) early TUI completion gate extraction: `turn_bridge/mod.rs` moved the
  #2293/#2780 early TUI quiescence gate (the eligibility filter + bounded
  `run_tui_completion_gate` probe + timed-out warning that compute
  `bridge_tui_gate_outcome_early` + `bridge_early_gate_timed_out`) verbatim into
  the new `early_tui_completion.rs` sibling; context is threaded in by shared
  reference (`inflight_state`, `provider`) and `Copy` value, and the two outputs
  are returned. Classification: worker-local — a pure behavior-preserving
  decompose: control flow, conditions, order, and side effects are byte-identical
  to the inline block, and it adds no new inflight-row field, delivery record, PG
  lease, leader gate, or cross-node routing state (the gate only reads
  worker-local tmux / inflight runtime state and is `#[cfg(unix)]`). No DB/schema
  change.
- #3813 Phase 2 status-panel low-pri + Bridge-spans (AC#1 tail):
  `turn_bridge/mod.rs` streaming loop now defers the v2 status-panel / footer edit
  off the shared per-channel rate lane while the opening answer is still
  un-relayed (pure `status_panel_edit_defer_for_first_answer` in
  `single_message_footer.rs`), so the #4006 fast lane relays the first answer
  first; and it emits observation-only bridge-side latency spans
  (`turn_start`->first_output / ->first_relay, struct in the new
  `bridge_latency_spans.rs`) reusing the existing `turn_start` `Instant` anchor.
  Classification: worker-local — the low-pri deferral only reorders local
  edit-timing within the shared per-channel lane (no `discord_io` min_gap change),
  keeps `status_panel_dirty` set so the panel renders on the next interval
  (coalesced, never dropped), and the #3477 live-panel guard (`first_answer_text_pending`)
  means tool-only / watcher- or standby-owned relay turns are never suppressed.
  The spans are pure `Instant` deltas emitted once at loop exit — no new
  inflight-row field, delivery record, PG lease, leader gate, or cross-node
  routing state, and no new await/lock on the hot path; watcher-owned relay
  latency is out of scope (`tmux_watcher.rs`). No DB/schema change.
- #3813 Phase 1b first-output fast-lane status-edit gate: `turn_bridge/mod.rs`
  streaming loop gained a single-shot fast lane so the FIRST non-empty assistant
  text is relayed immediately instead of waiting up to `status_interval`
  (default 5s); a `first_answer_relayed` flag + the pure
  `bridge_streaming_edit_gate_open` predicate (`streaming_edit_text.rs`) open the
  gate once for the opening answer, then it reverts to the normal interval
  throttle (at most +1 edit per turn). Classification: worker-local — this only
  relaxes the local edit-timing throttle inside the per-node bridge turn loop; it
  writes no new inflight-row field, delivery record, PG lease, leader gate, or
  cross-node routing state, and the `!done` guard, rollover, and finalize
  ownership counters are untouched. No DB/schema change.
- #3906 voice intake feedback P1+P4: `process_completed_utterance` now plays the
  deterministic Phase-1 intake chime into the active songbird call right before
  `start_voice_turn` (and the redundant non-deterministic foreground-start chime
  in `try_handle_voice_transcript_announcement` was removed), while the turn-done
  branch in `progress_playback.rs` plays a new distinct descending done chime
  (`DONE_CHIME_FILE_NAME` + `ensure_done_chime_file` / `play_done_chime`).
  Classification: worker-local. The chime is audio emitted into the per-node
  songbird voice call the worker is already connected to; it is upstream of the
  durable-reservation/announce/dedup machinery and touches no delivery record, PG
  lease, leader gate, or cross-node routing. No DB/schema change.
- #3976 orphan-relay reclaim durable delivered guard — prevents prior-tail
  re-emit on /loop non-Managed turn start: the `SessionBoundRelay` TUI-direct
  confirmed-POST route advanced only the resettable in-memory
  `confirmed_end_offset` watermark and wrote nothing else to the inflight row, so
  a DELIVERED-but-unmirrored row was byte-identical to a never-delivered
  black-hole; on a watermark reset (generation change / output regression /
  restart) below the turn body, orphan-reclaim downgraded the delivered row to
  ownerless and recovery re-emitted the already-delivered tail. The fix stamps a
  durable per-row `session_bound_delivered` marker ONLY after a genuinely
  confirmed delivery (POST landed AND identity gate matched AND watermark advance
  fired) via a single-flock identity-re-gated RMW
  (`mark_session_bound_relay_delivered_locked`), and excludes a marked row from
  `session_bound_relay_external_input_orphan_shape_at` (plus the symmetric
  ownerless predicate). Classification: worker-local — a per-worker inflight-row
  marker. The inflight row is per-node sidecar state the owning sink/watcher
  reads/writes for its own turn; the new field is additive `#[serde(default)]`
  (legacy rows deserialize as `false`), so it adds no leader gate, cross-node
  routing, or PG-lease assumption and is forward/backward compatible on disk.
  Independent of `AGENTDESK_DELIVERY_RECORD_AUTHORITY` / `_SHADOW` (it touches no
  delivery records) and of #3933.

- #3871 rollover duplicate-relay fix: `tmux_watcher.rs` records the streamed
  rollover-prefix message ids it FROZE during streaming and, on the terminal
  full-body fallback, deletes them (via `delete_watcher_rollover_frozen_prefixes`
  in `tmux_placeholder_suppression.rs`) so the full-body re-post does not
  duplicate the frozen prose — watcher parity with the sink's existing
  `terminal_full_replay_cleanup_msg_ids`. For durability across `'watcher_loop`
  iterations and watcher restarts the id set is PERSISTED on the inflight row
  (new additive `#[serde(default)] streaming_rollover_frozen_msg_ids` on
  `InflightTurnState`, union-merged via the streaming-progress patch and restored
  through the watcher seed), so a fallback in a later iteration / after a restart
  still deletes every accumulated prefix. Classification: worker-local relay
  cleanup + node-local inflight state. The inflight row is per-node sidecar state
  the owning watcher reads/writes for its own turn; the new field is additive
  `#[serde(default)]` (legacy rows deserialize as empty), so it adds no leader
  gate, cross-node routing, or PG-lease assumption and is forward/backward
  compatible on disk. Independent of the `AGENTDESK_DELIVERY_RECORD_AUTHORITY`
  flag (it touches no delivery records).

- #3837 intake_turn decomposition (behavior-preserving): three cohesive
  `handle_text_message` clusters were lifted verbatim into sibling
  `router::message_handler::intake_turn::{voice_intake,race_loss,turn_watchdog}`
  submodules — voice-announcement resolution, the `if !started` race-loss
  mailbox enqueue + queued-placeholder render + reaction lifecycle, and the
  per-turn watchdog spawn. Classification: UNCHANGED. Intake/turn-execution
  stays worker-local — workers invoke the unchanged `execute_intake_turn_core`
  facade after claiming a PG-lease-backed `intake_outbox` row, and the leader
  runs the same in-process `handle_text_message`; this is pure code movement
  with no new leader gate, cross-node routing, or PG-lease assumption.

- #3870 fail-closed control-plane bind: `server::run` now resolves the HTTP
  listener host through `routes::resolve_secure_bind_host`, which force-binds to
  loopback when `server.host` is non-loopback AND `server.auth_token` is unset
  (escape hatch: `server.allow_insecure_nonloopback_bind=true`). Classification:
  control-plane, per-node startup — every node (leader or follower) runs this
  guard at its own dcserver boot; it is NOT leader-gated and adds no cross-node
  routing or PG-lease assumption. Multinode note: all live cross-node
  coordination (heartbeat/leader-epoch/dispatch claims) is Postgres-based and
  deploy is SSH-based, so force-loopback does not affect cluster comms. The only
  cross-node HTTP path (session-forwarding to a peer's `cluster.api_base_url`)
  is inbound-only on the receiver and already requires `auth_token`, which by
  design exempts that node from the force-loopback downgrade.

- #3739 worker-local loop-owned terminal supervision: `worker_registry` now records
  unexpected worker-local `LoopOwned` Tokio task return/panic as local runtime
  status and tracing with `auto_restart=false`. Shutdown remains worker-local:
  the registry first lets the inner worker observe runtime shutdown and run its
  own cleanup, only aborting after a bounded grace timeout. This does not move
  the worker to leader-only ownership, add cross-node routing, or change PG lease
  assumptions.

- #3698/#3710 `/node` channel picker: Discord command registration now exposes a
  select-menu based node override for intake routing. The override is stored in
  shared bot settings and read only by the existing intake gate/hook path when
  the effective intake routing authority is enforce
  (`cluster.intake_routing.enabled=true` + `mode=enforce`, or emergency
  `ADK_INTAKE_ROUTING_MODE=enforce` override). Available choices are filtered
  from `worker_nodes` nodes that advertise the active provider's intake-worker
  capability. This adds no gateway ownership, leader-election, or lease
  assumption; it only constrains the already-clustered intake target decision.

- #3749 intake routing config authority: `cluster.intake_routing` is now the
  YAML source of truth for disabled/observe/enforce mode, with
  `ADK_INTAKE_ROUTING_MODE` retained as an emergency override. The leader hook,
  `/node`, worker spawn gate, and `/api/health.intake_routing` read the same
  effective authority. Classification: PG-lease-backed worker-local execution;
  no new gateway owner, no extra leader election surface.

- #4350 session-owner intake affinity: leader-only routing resolves the existing
  PG `sessions.instance_id` owner before `/node` or preferred labels, and every
  Discord/skill/queued producer shares one admission path. Stale or conflicting
  owners, distinct open routes, and foreign-owner node-local attachments fail
  safe without local execution; queued items are front-requeued before marker
  teardown. Worker execution remains instance-local and is the one intentional
  admission bypass after an outbox claim. No new lease or migration.

- #3630 frontier mirror for cancel/stop + prompt_too_long terminal arms:
  turn_bridge now mirrors only Delivered+committed terminal-replace lease ranges
  into the durable delivery-record frontier keyed by `watcher_owner_channel_id`.
  Classification: worker-local relay frontier; no leader election, PG lease, or
  cross-node gossip change.

- #3573 `pause_reason` DB field + opt-in failure-pause auto-resume:
  `routines` table gains a nullable TEXT column (`pause_reason`) populated with
  `'failure'` (run failed/timed-out), `'manual'` (operator pause), or
  `'migration_invalid'` (migrated-launchd structural-validation failure). All
  routine scheduling logic (supervisor tick, store methods `close_run`,
  `pause_routine`, `list_failure_paused_routines`, `auto_resume_failure_paused_routine`)
  runs on the single leader node that owns the PG pool — no cross-node state,
  no gossip, no worker-local cache. The new `failure_pause_auto_resume_secs`
  config knob (default 0 = disabled) gates the auto-resume scan. The existing
  `ResumeRequiresNextDueAt` guard is preserved: schedule-less routines with no
  `next_due_at` are skipped. `pause_reason = NULL` (pre-existing rows) and
  `'manual'`/`'migration_invalid'` pauses are never touched by auto-resume.
  No leader election, gateway lease, startup order, worker ownership, or
  singleton assumption outside the existing PG-lease-gated routine supervisor is
  touched.
- #3610 (Phase B PR-1c) long-chunk terminal anchor recording: `turn_bridge/mod.rs`
  gained a single helper call in the long-chunk terminal-delivery arm (site 4 —
  `send_ordered_long_terminal_response`, the send-new-chunks + placeholder-delete
  path) so a LONG (`len > DISCORD_MSG_LIMIT`) terminal answer records the durable
  delivered-frontier anchor PR-1/PR-1b covered only for the short-replace sites.
  The anchor is the LAST sent chunk's message id (the placeholder is deleted, so the
  tail chunk is the only stable anchor); it is recorded on the SAME full-commit
  `Delivered` lease commit, gated identically to the cutover. The frontier KEY stays
  `watcher_owner_channel_id` (the offset authority — UNCHANGED), and the recorded
  anchor PAIR is `(panel_channel_id = delivery channel_id, panel_msg_id = last chunk)`;
  the helper body + gating live in `outbound/delivery_record.rs`
  (`record_long_chunk_terminal_delivery`). This is purely worker-local delivery
  instrumentation behind the existing shadow flag (`AGENTDESK_DELIVERY_RECORD_SHADOW`,
  default OFF → no-op): the recorded panel fields have NO production reader (the sole
  durable-frontier reader consumes only `.range.1`), so the #3593/#3520 dedup, #3604
  window, and the `watcher_owner_channel_id` offset-authority key are all unaffected.
  No leader election, gateway lease, PG ownership, startup order, worker ownership, or
  singleton assumption is touched; the recovery-fallback re-post criterion remains
  deferred to a later #3610 PR.
- #3593 synthetic-resume relay-duplicate guard: `tmux_watcher.rs` extends the
  resend-dedup decision so a non-reconciled, already-committed JSONL range (the
  background-agent-completion synthetic resume that restores the placeholder and
  rewinds `response_sent_offset`) routes to the EXISTING non-destructive
  `SkipAlreadyCommitted` arm instead of re-sending the prior body. The dedup
  reads the worker's OWN per-channel relay watermark (`effective_committed_offset`,
  generation self-healed before the read) and preserves the restored placeholder —
  it is a worker-local, in-memory/durable-record dedup, NOT a routing authority and
  never read cross-node. The pure `range_already_committed` helper + tests live in
  `outbound/delivery_record.rs`. No leader election, gateway lease, PG ownership,
  startup order, worker ownership, or singleton assumption is touched.
- #3607 terminal-delete protection guard + durable delete observability
  (Phase A): `turn_bridge/mod.rs` gained a worker-local cleanup guard that
  preserves a committed terminal anchor (a finished turn's retired message
  recorded in the per-process `PlaceholderCleanupRegistry` tombstone, signal-c,
  fused with the live-inflight fast path, signal-a) from the orphan-spinner
  cleanup, plus `emit_relay_delete` observability on every wired delete site
  (orphan-spinner, full-terminal replay-prefix). The hot-file body shrank
  (orphan-spinner cleanup lifted whole into the `watcher_orphan_cleanup.rs`
  sibling); only a dispatch call + guard remain inline. The guard reads the same
  per-process tombstone registry the watcher already owns and the turn's OWN
  inflight handle — it is NOT a routing authority, never read cross-node. No
  leader election, gateway lease, PG ownership, startup order, worker ownership,
  or singleton assumption is touched; the recovery-fallback criterion is
  deferred to #3610.
- #3607 terminal-UI obligation durable sidecar + sweeper: the
  TimedOut+committed terminal path writes a worker-local sidecar obligation and
  edits only the existing status card to "delivered / session-end confirming";
  `terminal_ui_obligation.rs` owns the durable sidecar and isolated sweeper that
  converges the same card to ✅ on pane idle or ⚠ on deadline. This is a
  worker-local UI reconcile over per-channel runtime state, not a body-delivery
  authority: no assistant body repost, no response/confirmed offset movement,
  no `delivery_record.rs` frontier reuse, and no new leader election, gateway
  lease, PG ownership, startup order, worker ownership, or singleton assumption.
- #3560 single_message_panel default-ON + footer-mode migration guard: the
  `single_message_panel` flag is now default-ON (opt-out via
  `AGENTDESK_SINGLE_MESSAGE_PANEL=0|false`) and `turn_bridge/mod.rs` gained a
  deployment-boundary migration guard. When a turn that created a *separate*
  status panel under the old default-OFF runtime resumes under footer mode, the
  bridge now reconciles (edits to a migration notice) and clears its OWN
  `inflight_state.status_message_id` instead of orphaning that Discord message.
  This is purely worker-local: it operates on the resuming turn's own per-turn
  inflight handle and its own gateway, with no node ownership, lease, or
  singleton implication. Gateway lease, startup order, worker ownership, and
  singleton assumptions are unchanged.
- #3540 phantom-synthetic-inflight fix: two worker-local, in-memory-state-only
  touches with no multinode ownership/lease/singleton implications.
  (A) `tui_prompt_dedupe.rs` gained a process-global `relayed_entry_ids_by_tmux`
  ledger keyed on the Claude transcript `user` entry's stable `uuid`
  (TTL-purged + ring-capped) and the `SuppressedReplayedEntry` observation, so
  the idle-transcript scanner suppresses an already-relayed prompt re-encountered
  after a relay-watermark reset / jsonl head rotation BY IDENTITY. This is the
  same per-process dedupe state the relay already owns (it is NOT a routing
  authority and is never read cross-node); the channel→tmux routing invariant is
  untouched. (B) `tui_direct_pending_start.rs` adds a no-evict post-abort queue
  promote: on the terminal backstop ABORT it kicks the EXISTING
  `schedule_deferred_idle_queue_kickoff` once (after the pending record is
  deleted) so a queued follow-up dispatches through the unchanged
  `mailbox_try_start_turn_kinded` FSM — NO inflight is cleared/reset/deleted, so
  the worst case is a normal merge with zero live-turn loss. The detached worker
  remains per-`(provider, channel_id)` worker-local under the existing channel
  lock; queue ownership, lease semantics, and singleton assumptions are
  unchanged. (C) absorbed warm-followup strand fix in
  `turn_bridge/watcher_handoff.rs`: the #3277 proven-delivered guard
  (`busy_turn_already_proven_delivered`) now reads the transcript's on-disk EOF
  (`std::fs::metadata(output_path).len()`) instead of the racy in-memory
  `tmux_last_offset` to decide whether a quiescence-timeout turn already grew
  its full response past `turn_start_offset`. Both `turn_start_offset` and
  `tmux_last_offset` are seeded to the same per-turn `inflight_offset` (itself a
  `std::fs::metadata().len()` of the same `output_path`), so the new EOF read is
  in the SAME single-file byte-space — purely a worker-local disk read on the
  bridge's own transcript path, with no node ownership, lease, or singleton
  implication. A rotated/truncated transcript shrinks the EOF and the guard
  conservatively fails open to the existing #3268 watcher handoff.
- #3038 run_bot S5: the leader gateway runtime tail moved verbatim from
  `runtime_bootstrap.rs` into `runtime_bootstrap/gateway_runtime.rs`: restored
  generation/model/fast-mode logging, health registry registration,
  slash-command/framework/client construction, gateway lease keepalive spawn,
  SIGTERM handler spawn, and backend event-loop entry remain in the same order.
  The root `run_bot` body now delegates that tail after the lease succeeds; no
  gateway ownership, worker routing, singleton, or lease semantics changed.
- #3038 run_bot S4: `run_bot_build_shared_data` (and its side-effect-order
  doc comment) moved verbatim from `runtime_bootstrap.rs` into
  `runtime_bootstrap/shared_data.rs`, unblocked by the merged SharedData
  S1-S3 slices. This is a **behavior-preserving module split**: the builder
  body is token-identical apart from the documented `super::` →
  `crate::services::discord::` path substitutions (8) and the `pub(super)`
  visibility needed by the root re-import; the `run_bot` body, the builder's
  side-effecting initializer order (`load_queue_exit_placeholder_clears` ↔
  `load_generation` ↔ `TurnFinalizer::spawn` ↔ `broadcast::channel`), and the
  standby-before-lease call order are
  unchanged. Worker-local module move only: no multinode ownership,
  singleton, or lease assumption changes.
- #3038 SharedData S3: `runtime_bootstrap.rs` gained restart-lifecycle
  characterization tests that pin the deferred-restart marker quick-exit path
  (`run_bot_spawn_deferred_restart_poller`) and the
  `shutdown_counted`/`shutdown_remaining` exactly-once protocol through the
  `run_bot_build_shared_data` injection seam, observed via the test's own
  handle on the injected counter. The thirteen restart-lifecycle fields are
  now initialized through the `RestartLifecycle` group literal wrapped at the
  first member's original position (member expressions byte-identical; the
  three trailing members hoisted above the actor-spawn calls are
  side-effect-free, so every side-effecting initializer keeps its relative
  order). `run_bot` body changes are two single-token field-path renames (one
  comment, one tracing argument); the SIGTERM handler, poller, and
  gateway-lease/recovery helpers received the same mechanical
  `shared.<field>` → `shared.restart.<field>` substitution with no
  statement, ordering, or lock-span changes. The process-global
  `global_active`/`global_finalizing`/`shutdown_remaining` counters remain
  injected `Arc` handles (no flattening), so worker-local state grouping
  only: no multinode ownership, singleton, or lease assumption changes.
- #3038 SharedData S2: `runtime_bootstrap.rs` changed only inside
  `run_bot_build_shared_data` — the eight session-override fields are now
  initialized through the `SessionOverrideState` group literal wrapped at the
  first member's original position (member expressions byte-identical,
  evaluation order preserved; `run_bot` body byte-identical). Worker-local
  state grouping only: no multinode ownership, singleton, or lease assumption
  changes.
- #3038 run_bot S0/S1: `runtime_bootstrap.rs` gained characterization tests and
  moved restored-state, queued-placeholder, startup-doctor, orphan-recovery, and
  session-GC helpers into `runtime_bootstrap/` submodules. This is a
  **behavior-preserving module split**: `run_bot` body, gateway lease acquisition,
  keepalive self-fence, shutdown ordering, and the recovery/spawn callsites remain
  in their original order. Multinode classification is unchanged: gateway
  ownership remains leader-only under the existing lease, while thread/session GC
  and queued-placeholder cleanup retain their previous worker-local/runtime-local
  assumptions.
- #3038 run_bot S2/S3: setup/spawn/recovery/voice helpers moved into
  `framework_setup.rs`, `spawns.rs`, `recovery_flush.rs`, and `voice.rs`; gateway
  lease, shutdown/backend, and intake-worker helpers moved into
  `gateway_lease.rs`, `shutdown.rs`, and `intake.rs`. This is a
  **behavior-preserving module split**: the `run_bot` call order, gateway
  acquisition before client startup, keepalive self-fence, SIGTERM
  persist-before-I/O ordering, and standby intake-worker placement are unchanged.
  S4 (`run_bot_build_shared_data`) remains gated on the SharedData slice.
- #3274 residual cleanup: `turn_finalizer.rs` now invokes a small
  `turn_finalizer/cleanup.rs` helper when a terminal loser receives
  `AlreadyFinalized`, clearing only same-`user_msg_id` mailbox/inflight
  active-state. This is **worker-local**: it runs in the same process that owns
  the channel mailbox, finalizer actor, and local inflight file, and it adds no
  leader-only side effect, durable queue authority, or PG lease assumption.
- #3334 reaction lifecycle cleanup: abnormal finalizer backstops and restart
  catch-up now run idempotent same-message reaction cleanup through local Discord
  HTTP helpers. This is **worker-local**: it touches only the recovered/finalized
  Discord message id on the worker processing that channel and adds no shared
  scheduling authority or cross-node lease dependency.
- TUI hook registry upstream-port audit: `runtime_bootstrap.rs` bootstrap
  wiring now feeds the Claude TUI hook buffer/claim registry, but the registry
  is **worker-local** in-memory state scoped to a provider session / tmux key.
  It adds no leader-only side effect, durable queue, cross-node singleton, or
  PG lease; on restart it can only lose buffered hook events and falls back to
  the legacy readiness path.
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
- #3078 (status-panel controller shadow-parity): **SUPERSEDED — removed in #3479.**
  The `StatusPanelController` cutover was frozen 2026-06-12 and superseded by
  #3089 M1 (single-message footer; the "one owner" goal was met there); the
  dormant shadow-parity substrate (`status_panel_controller.rs`,
  `watcher_panel_parity.rs`, `shadow_parity_warn.rs`) was deleted. It had only
  ever been a per-process in-memory shadow performing no Discord IO, so no
  multinode ownership/singleton/lease assumption was introduced or removed.
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
- #3350 (synthetic-anchor hourglass bound for inline claims): the INLINE
  synthetic claim (`tui_prompt_relay.rs`) now records the same #3303
  `DeferredClaim` marker as the deferred worker (shared helper generalized in
  `tui_direct_pending_start.rs`), and `turn_finalizer.rs::do_finalize` calls a
  new `turn_finalizer/cleanup.rs` hook that idempotently ENSURES the marker
  (`tui_direct_abort_marker/deferred_claim.rs::ensure_marker_for_own_synthetic_turn`)
  for watcher-owned TUI-direct synthetic rows before the inflight clear. Both
  are WRITES into the existing node-local
  `runtime/discord_tui_direct_abort_marker/` store from the same node's own
  relay observer / finalizer actor; reconcile/delivery stays with the existing
  #3303 drain/sweep owners unchanged (zero new reaction call sites).
  **Worker-local**: no PG lease, no cross-node reads, no leader-only side
  effect. No new multinode ownership, singleton, or lease assumption is
  introduced.
- #3038 S5 (SharedData cluster G — `RuntimeHttpCache`): pure field relocation;
  `cached_serenity_ctx` / `cached_bot_token` and the
  `serenity_http_or_token_fallback()` accessor moved verbatim from
  `discord/mod.rs` into `shared_state.rs::RuntimeHttpCache`, with call sites
  (including `runtime_bootstrap*` init and 2 single-token sites in the frozen
  `turn_bridge/mod.rs`) rerouted `shared.cached_*` → `shared.http.cached_*`.
  The leader-vs-standby semantics of the accessor (gateway ctx preferred,
  token-built Http fallback on standby nodes) are byte-identical and stay
  process-local. No new multinode ownership, singleton, or lease assumption
  is introduced.
- #3641 (boot-time orphan inflight `.lock` sweep): `inflight.rs` now removes
  old `discord_inflight/{provider}/*.json.lock` sidecars only when the matching
  `.json` inflight row is absent and the lock mtime is past the conservative
  age floor. This is worker-local filesystem hygiene for advisory-lock sidecars:
  it does not touch live `.json` rows, durable queues, leases, leader/standby
  ownership, or cross-node routing semantics.
- Active-session audit: `active_session_audit` adds read-only health diagnostics
  plus optional local repair-path metadata for stale running-session rows. It
  does not move Discord gateway startup, worker ownership, durable queue claims,
  or PG lease boundaries; each reported repair action still targets the existing
  node-local/runtime owner. No new multinode ownership, singleton, or lease
  assumption is introduced.
- #3543 follow-up (OpenCode warm-server race/timeout hardening):
  `opencode.rs` now marks a warm server as retiring before the exclusive
  hard-kill fallback and rejects new leases on retiring servers; pre-SSE
  `/session` and `/prompt_async` REST calls use bounded request timeouts.
  This remains a worker-local provider process pool on the node that owns the
  OpenCode turn. It adds no durable queue, cross-node read, leader-only side
  effect, or PG lease assumption, so multinode ownership semantics are
  unchanged.
- #3558 (watcher offset TOCTOU root fix): the two watcher write paths that ran an
  unlocked `load_inflight_state` -> mutate -> `save_inflight_state` (the streaming
  `persist_watcher_stream_progress` and the terminal-commit
  `mark_watcher_terminal_delivery_committed`) now delegate to two new single-flock
  read-modify-write helpers in `inflight.rs`
  (`persist_watcher_stream_progress_locked` /
  `commit_watcher_terminal_delivery_locked`). Both acquire the EXISTING
  per-`(provider, channel_id)` sidecar `flock` ONCE, reload the on-disk row,
  re-check the caller's identity/session guards against the freshly reloaded row,
  patch only watcher-owned fields, and persist via `persist_under_lock` (never
  re-entering `save_inflight_state`, so the non-reentrant flock cannot
  self-deadlock). This is **worker-local**: it operates entirely on the same
  per-channel inflight sidecar file the watcher already owns, under the same
  advisory lock. No lease, durable queue, leader/standby ownership, or singleton
  assumption changes — the only behavioural change is that the streaming path now
  PRESERVES the non-owned `last_offset` from the in-lock reload (instead of
  clobbering it backward) and the commit path `max`-serializes its watermark
  against the reload, eliminating the backward-write race with the owner-gated
  `refresh_inflight_last_offset_*` advance.
- #3671 (stall-watchdog force-clean deferral backstop): `health/stall_liveness.rs`
  replaces the tick-count cleanup gate with an age-based absolute backstop. While
  positive liveness is observed (`evaluate_stall_watchdog_liveness`) the force-clean
  is deferred indefinitely up to `STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS` (4h, aligned
  to the Codex per-turn hard ceiling); only a turn whose anchor age
  (`started_at.max(boot)`, unchanged from `from_snapshot`) crosses that bound is
  force-cleaned (finite detection ceiling per #3582 R1), and a dead relay
  (`reason_codes == none`) still cleans on the first tick. The age is the turn's own
  `judgment_basis.inflight_age_secs` threaded in from `health/recovery.rs`. This is
  **worker-local**: the watchdog runs against the node-local per-channel inflight
  snapshot and its own `DEFERRAL_STATE`/`OFFSET_OBSERVATIONS` dashmaps. No lease,
  durable queue, leader/standby ownership, gateway startup order, or singleton
  assumption is touched.
- #3646 (relay-owner observability — OBSERVATION-ONLY): splits the relay flight
  recorder's collapsed `relay_owner_kind` into two distinct signals so the #3607
  None-ledger vs Watcher-finalize ambiguity is PG-resolvable, and adds three
  terminal lifecycle events (`terminal_body_commit` / `terminal_ui_transition` /
  `inflight_clear` + a NON-FATAL invariant signal). The watcher side
  (`tmux_watcher.rs`) emits `inflight_relay_owner` from the node-local pre-relay
  inflight snapshot; the finalizer side (`turn_finalizer.rs`) emits
  `finalizer_ledger_owner` reading the **worker-local** `turn_finalizer` actor
  ledger entry's `relay_owner` — the same per-process in-memory map already
  documented above (re-seeded by #3293's `reseed_watcher_owned_finalizer_ledger`).
  The two signals JOIN on `discord:<channel>:<user_msg_id>`. All payload/derivation
  logic lives in the non-hot `relay_owner_observability.rs`. NO relay/cleanup
  behaviour, branch, ordering, or condition changes; the emits only gate the EMIT
  (never the cleanup) and the invariant is an error-event + `debug_assert!` (no
  operational panic). **Worker-local**: both owner reads are node-local (inflight
  file + in-process ledger on the node that holds the live pane); the events flow
  through the existing `emit_inflight_lifecycle_event` PG/jsonl sink. No lease,
  durable queue, leader/standby ownership, or singleton assumption is introduced.
- #3909 voice TTS cache/temp disk-exhaustion fix — two classifications:
  - **Worker-local** (leak E): `tts/edge.rs` `EdgeTtsTempGuard` is a `Drop` guard
    that unlinks the partially-written `agentdesk-edge-tts-*.mp3` when the synth
    future is dropped mid-`.await` (barge-in abort) or any error returns. It runs
    in-process on whichever node performed the synthesis, deleting only that node's
    own temp file. No cross-node state, lease, or ownership; every node cleans up
    after its own aborted synthesis.
  - **Leader-only** (leak A): `server::maintenance::ProgressTtsCacheSweepJob`
    (logic in `services::maintenance::jobs::voice_cache_sweep`) bounds the progress
    TTS cache dir (TTL + capacity LRU) and mops up orphaned edge-tts temp mp3s. It
    is a `MaintenanceJob` on the static registry, run through the existing
    `worker_registry::MaintenanceScheduler` whose `WorkerExecutionScope` is
    `LeaderOnly` — mirroring `voice.turn_link_gc`. Gated leader-only so N cluster
    nodes do not each spin a redundant sweeper. The sweep dirs are resolved from
    the loaded runtime `VoiceConfig` (`Config::from_voice_config`, tilde-expanded)
    — the same source of truth the TTS write path uses — so operator overrides of
    `voice.tts.progress_cache_dir` / `voice.audio.temp_dir` are swept, not the
    defaults. Pool-less, no new lease, durable queue, leader-election surface, or
    singleton assumption.
- #3914 voice P3 cleanup bundle (observability / leak / validation) — **all
  Worker-local**: every state surface touched is process-global on the node that
  holds the live voice session, with no cross-node coordination introduced.
  - `src/voice/receiver.rs`: the songbird `ClientDisconnect` handler prunes the
    leaver's SSRC→user entries from the in-process `ssrc_users` map. The map is
    pinned to the node running that voice connection (songbird driver is
    node-local), so disconnect cleanup is purely worker-local.
  - `src/voice/metrics.rs`: the new STT outcome counters and `voice_stt_outcome`
    structured events are process-local telemetry (same class as the existing
    `voice_latency_turn` registry), flowing through the node-local observability
    event sink. No leader/standby ownership.
  - `src/voice/cancel_tombstone.rs`: the re-fire guard remains an explicitly
    process-local `OnceLock<RwLock<HashMap>>` (documented in its module header) —
    the poison-recovery + read-path prune changes do not alter that boundary; a
    dcserver restart between the two cancel attempts is still covered by the
    background turn's own cancel-on-restart recovery.
  - `src/voice/tts/edge.rs` keeps the edge-tts subprocess timeout a worker-local
    constant; making it configurable + adding TTS synth/cache hit-miss metrics is
    explicitly deferred (informational sub-item) and would also be worker-local.
- #4002 (recap duplicate root fix — SystemContinuation Path-X convergence): the
  compact-resume suppress branch (`tui_prompt_relay.rs`) used to post its neutral
  note and fall through INFLIGHT-LESS, so the observer spawned an un-arbitrated
  BridgeAdapter idle tail that raced the real turn's inflight-owned relayer → two
  live panels. The active-turn else-branch relay-ownership wiring (prior-view →
  defer/claim → adopt resolved `relay_owner` into the lease) was extracted VERBATIM
  into a new node-local module (`tui_prompt_relay/synthetic_start_wiring.rs`,
  `wire_tui_direct_synthetic_turn_start`) and the suppress branch now reuses it,
  installing a PASSIVE synthetic inflight (relay-ownership only — no ⏳/anchor and,
  per the round-2 fix below, no completion lifecycle either) so the post-block
  bridge-tail gate honours cross-relayer single-ownership. Round-2 (dual-review P2):
  that passive inflight kept `user_msg_id = note.id` (≠ 0) and `rebind_origin =
  false`, so the watcher completion **Path B** (`tmux_watcher.rs` `⏳→✅` reaction +
  `session_transcripts` / `turn_analytics` persistence) would have branded the
  neutral compact-resume note with a `✅` and written a phantom user-turn
  analytics/transcript row (`turn_id=discord:<channel>:note.id`). The fix adds an
  additive `#[serde(default)] relay_ownership_only` flag on the node-local inflight
  row (`inflight/model.rs`; legacy rows deserialize as `false`), set at the
  SystemContinuation synthetic birth site (`claim_tui_direct_synthetic_turn`, and
  re-derived from the durable prompt text on the deferred worker path), and gates
  Path B on `terminal_readiness::watcher_completion_lifecycle_applies` so a
  relay-ownership-only row is skipped. **Worker-local**: the passive inflight, the
  in-memory external-input relay lease, and the durable pending-start / claim-marker
  stores are all the SAME per-node surfaces the active-turn synthetic path already
  uses; the fix only ROUTES the compact-resume observation onto them and suppresses
  its per-node completion bookkeeping. It introduces no new PG lease, cross-node
  read, leader-only side effect, or singleton assumption — the cross-relayer
  single-owner invariant it enforces, and the completion Path B it gates, both
  already lived on the per-node inflight row. The only persisted change is the
  additive node-local inflight-row field (no PG schema change; relay-ownership
  adoption / bridge-tail stand-down / response finalize are all unaffected).
- #4018 compact-resume stale mailbox follow-up - **Worker-local relay lifecycle,
  no PG lease/schema**: the passive synthetic completion guard is confined to
  `tmux_watcher/completion_gate.rs` and `turn_bridge/early_tui_completion.rs`,
  finalizer identity-release diagnostics stay in `turn_finalizer/finalize.rs`,
  stale-owner reclaim stays in `tui_prompt_relay/synthetic_start.rs` plus
  `synthetic_start/stale_reclaim.rs`, and frame-decision logging stays in
  `tui_prompt_relay/claude_idle_bridge.rs`. All touched state is the existing
  per-node mailbox/inflight/relay-owner surface; no leader election, PG lease,
  PG schema, cross-node read, or singleton assumption is introduced. The
  watchdog observe_only/force-clean behavior remains a follow-up audit item.
- #4370 restart-resume stale mailbox (generalises #4018 to the restart path) -
  **Worker-local relay lifecycle, no PG lease/schema**: #4018 keyed its stale
  reclaim on the synthetic relay owner, but a dcserver restart re-adopts the REAL
  user turn (`recovery_engine/runtime.rs::reregister_active_turn_from_inflight`,
  mailbox owner == `request_owner_user_id`), so the synthetic-owner-only reclaim
  could never free that mailbox and follow-up injection / task-notification
  synthetic turns starved for relay ownership. The fix records the re-adopt in two
  node-local places, each for a different reclaim shape: (a) an additive inflight-
  row marker `readopted_from_inflight` (`inflight/model.rs`, `#[serde(default)]`,
  legacy rows deserialize `false`; no `INFLIGHT_STATE_VERSION` bump), written by an
  identity-guarded single-field patch `mark_readopted_from_inflight_if_identity_unchanged`
  (NOT a blind whole-row save — it preserves `restart_mode` and never resurrects a
  concurrently-cleared row), used for the PRESENT-row reclaim; and (b) an
  IN-MEMORY, per-process `SharedData::readopted_mailbox_ledger`
  (`DashMap<(provider, channel_id), ReadoptedMailboxOwner>`), the authority for the
  ROW-ABSENT reclaim (the row was cleared but the mailbox stayed stuck). The ledger
  is deliberately NOT persisted and NOT shared across nodes: only the process that
  performed the re-adopt can know a mailbox is a re-adopted real turn, and a fresh
  process re-derives the mailbox from disk, so its lifetime is exactly one process.
  A stale entry is inert — a live successor turn owns a different
  `active_user_message_id` and can never match — reinforced by the `age >= 120s`
  gate on the resulting `OwnerInflightAbsent` reason. (c) `synthetic_start/stale_reclaim.rs`
  eligibility is widened to a re-adopted-from-inflight real-user owner reusing the
  EXISTING absent/`terminal_delivery_committed` predicate and the age gate. All
  touched state is the same per-node mailbox / inflight / relay-owner surface #4018
  used plus the new in-memory ledger; the marker is DELIBERATELY DISTINCT from
  `relay_ownership_only` so the re-adopted turn's own `✅`/footer + analytics/transcript
  still fire. No leader election, PG lease, PG schema, cross-node read, or singleton
  assumption is introduced. The core-4 serial hotfiles (`turn_bridge/mod.rs`,
  `tmux_watcher.rs`, `session_relay_sink.rs`, `turn_finalizer.rs`) are untouched, as
  in #4018.
- #4055 task-notification card authority — **PG-shared card and per-turn response
  authority plus node-local response frontier**: `task_notification_card_state` uniquely keys
  `(channel_id, provider, session_key, event_key)` and stores the selected bot,
  stable Discord create nonce, message id/revision, and short lease. The separate
  `task_notification_response_delivery` table is 1:N from that semantic event and
  uniquely keys each restart-stable `response_turn_key`; it stores the exact
  referenced card id, owner token/lease, and `claimed → sent → delivered` state.
  Multiple workers therefore converge through PG CAS. An ambiguous card create retries the same
  `enforce_nonce=true` nonce within Discord's bounded nonce-replay window; the
  PG row/message id, rather than the nonce window, remains the durable card
  authority. Every response POST chunk separately derives a bounded Discord
  nonce from `(response_turn_key, chunk_index)` and enforces it on both the sink
  and watcher required-reference transports. If Discord accepts the reply but
  the `sent` CAS fails, an expired-lease takeover therefore reconciles the same
  returned message id instead of creating a duplicate reply. A structured
  Discord missing-reference rejection can CAS-replace the missing card and then
  rebind only the still-claimed exact response owner from the old id to the new
  id; no unreferenced response fallback is allowed. Prompt-side footer deferral and the
  `SessionRelayParser` context remain node-local observations, but the terminal
  sink must confirm the PG-owned card before sending/committing its node-local
  answer frontier, bind the exact turn in PG, and reference that confirmed card.
  Exact footer
  eviction is node-local UI state keyed by `tool_use_id`; it occurs only after
  the shared card is confirmed. The watcher queries the exact event key or
  restart-stable response-turn key in PG and fails closed for missing/error/card-
  pending state. `sent` is a no-POST tombstone even after lease expiry, so a
  final delivered-CAS failure is observable without allowing another worker to
  duplicate the Discord POST; `delivered` completes the response fence.
  An unrelated event in the same session cannot release or suppress that turn.
  The in-memory card store is used only when PG is absent (tests/non-release
  fallback), never as multi-worker authority. This adds no leader-only singleton
  assumption.
- #3805 P2 PR-A (two-message model scaffolding — worker-local UI flag): adds the
  additive `two_message_panel_enabled` flag to `PlaceholderConfig` and threads it
  through the per-node UI plumbing (`runtime_bootstrap.rs` RunBotContext /
  UiFeatureFlags → `runtime_bootstrap/shared_data.rs` → `shared_state.rs`
  PlaceholderState) plus an additive `status_panel_generation` field on the
  node-local inflight row (`inflight/model.rs`; `#[serde(default)]`, legacy rows
  deserialize as 0). **Worker-local**: the flag is a per-node UI feature toggle
  (default OFF, restart-required — the `placeholder` config section is already
  restart-scoped) and the generation counter lives on the same per-node inflight
  row the bridge/watcher already own. No behavior reads either yet (pure additive
  no-op scaffolding for the later two-message PRs); introduces no new PG lease,
  cross-node read, leader-only side effect, singleton assumption, or PG schema
  change.
- #3805 P2 PR-B (two-message SINK creation order — worker-local, first reader of
  the PR-A flag): when `two_message_panel_enabled` is ON the bridge sink creates
  the `status_panel_v2` status panel as a NEW message BELOW the answer
  (answer-first layout) instead of the legacy panel-above swap. All logic lives
  in the new node-local sibling `turn_bridge/two_message_panel.rs`; `mod.rs` /
  `single_message_footer.rs` carry only thin call-site wiring plus a per-turn
  `status_panel_generation` epoch (already the node-local inflight row's field
  from PR-A) threaded from the pinned inflight snapshot into the create + the
  terminal completion edit. **Worker-local**: this is pure per-node msg-id / HTTP
  bookkeeping on the bridge/watcher-owned inflight row and Discord messages — the
  panel handle (`status_message_id`), the answer anchor (`current_msg_id`), and
  the epoch counter all already lived on the node-local row. It never tears down
  the per-channel `StatusPanelState`, so item4's `session_banner` exactly-once
  claim (`session_banner_emitted_key`) is untouched. Gated on the default-OFF
  flag so the OFF path is byte-identical; introduces no new PG lease, cross-node
  read, leader-only side effect, singleton assumption, or PG schema change. The
  watcher-path parity for this ordering is a later PR (PR-C, `tmux_watcher.rs`).
- #3805 P2 PR-C (two-message WATCHER creation-order parity — worker-local): mirrors
  PR-B on the fully-independent tmux WATCHER relay path. When
  `two_message_panel_enabled` is ON the watcher defers its `status_panel_v2` panel
  creation until the answer placeholder exists so the panel lands BELOW the answer
  (answer-first), opens this turn's `status_panel_generation` epoch atomically with
  the existing `bind_status_panel` publish (the bind guard bumps the generation
  from the on-disk row under the inflight flock in `inflight/ownership_ops.rs`),
  and skips a stale
  completion edit superseded by a newer epoch for the SAME owned panel — reusing
  the SAME generation-staleness predicate as the sink (re-exported from
  `turn_bridge/two_message_panel.rs`, parity). All P2 logic lives in the new
  node-local sibling `tmux_watcher/two_message_panel.rs` (pure predicates + the
  panel-completion tail moved verbatim out of the 700-capped
  `single_message_footer.rs`); `tmux_watcher.rs` carries only thin call-site wiring
  plus a per-turn epoch local seeded from the node-local inflight snapshot.
  **Worker-local**: this is per-node msg-id / HTTP bookkeeping on the
  watcher-owned inflight sidecar row (the `status_message_id` panel handle and the
  `status_panel_generation` epoch already lived on the node-local row; the
  generation write is under the same per-turn inflight sidecar flock as the panel
  bind) and per-node Discord messages. It never tears down the per-channel
  `StatusPanelState`, so item4's `session_banner` exactly-once claim is untouched.
  Gated on the default-OFF flag so the OFF path is byte-identical; introduces no
  new PG lease, cross-node read, leader-only side effect, singleton assumption, or
  PG schema change.
