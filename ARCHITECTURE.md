# AgentDesk Architecture Guide

Code navigation guide for contributors. When something breaks, this tells you where to look.

## Directory Structure

```
src/
├── main.rs                        # Entry point — CLI dispatch, then tokio runtime
├── config.rs                      # Parses agentdesk.yaml
├── credential.rs                  # Credential storage
├── kanban.rs                      # Kanban state machine helpers
├── reconcile.rs                   # Boot-time state reconciler
├── pipeline.rs                    # Pipeline routing and orchestration
├── receipt.rs                     # Receipt/log handling
├── error.rs                       # Error types
│
├── cli/                           # CLI subcommands
│   ├── dcserver.rs                # --dcserver: Discord bot standalone mode
│   ├── init.rs                    # --init / --reconfigure: setup wizard
│   ├── doctor.rs                  # --doctor: system diagnostics
│   ├── client.rs                  # CLI client utilities
│   ├── utils.rs                   # --reset-sessions, base64 helpers
│   └── discord.rs                 # --discord-send* message utilities
│
├── db/                            # SQLite database
│   ├── mod.rs                     # DB init (WAL mode, foreign keys)
│   ├── schema.rs                  # Migrations (versioned)
│   ├── agents.rs                  # Agent SQL queries
│   └── session_transcripts.rs     # Session transcript storage
│
├── server/                        # Axum HTTP server
│   ├── mod.rs                     # Server boot, router assembly, 3-tier tick loop
│   ├── ws.rs                      # WebSocket broadcast
│   └── routes/                    # 30+ route modules (see API section)
│       ├── dispatches/            # Task dispatch API (split from dispatches.rs)
│       │   ├── crud.rs            # Dispatch CRUD operations
│       │   ├── discord_delivery.rs # Discord channel delivery routing
│       │   ├── outbox.rs          # Dispatch notification outbox
│       │   ├── thread_reuse.rs    # Thread reuse logic
│       │   └── tests.rs          
│       └── review_verdict/        # Review verdict API (split from review_verdict.rs)
│           ├── decision_route.rs  # Review-decision accept/dispute
│           ├── verdict_route.rs   # Review pass/reject
│           ├── tuning_aggregate.rs # Tuning outcome aggregation
│           └── tests.rs          
│
├── engine/                        # QuickJS policy engine
│   ├── mod.rs                     # JS runtime init, hook execution
│   ├── ops.rs                     # Rust↔JS bridge (~30 functions)
│   ├── hooks.rs                   # 10 lifecycle hook definitions (7 event + 3 tiered tick)
│   ├── transition.rs              # State transition logic
│   ├── intent.rs                  # Intent parsing
│   └── loader.rs                  # File watcher + hot-reload
│
├── services/                      # Core service layer
│   ├── claude.rs                  # Claude provider (streaming, tool exec)
│   ├── codex.rs                   # Codex provider
│   ├── gemini.rs                  # Gemini provider
│   ├── qwen.rs                    # Qwen provider
│   ├── provider.rs                # ProviderKind enum, session name construction
│   ├── provider_exec.rs           # Provider execution dispatcher
│   ├── session_backend.rs         # ProcessBackend — session spawn via child process
│   ├── tmux_common.rs             # Temp file paths, owner markers (cross-platform)
│   ├── tmux_diagnostics.rs        # Exit reason tracking, death diagnostics
│   ├── tmux_wrapper.rs            # Claude process wrapper (--tmux-wrapper)
│   ├── codex_tmux_wrapper.rs      # Codex process wrapper
│   ├── qwen_tmux_wrapper.rs       # Qwen process wrapper
│   ├── process.rs                 # Process list/kill (for /ps command)
│   ├── remote_stub.rs             # Remote provider stub
│   │
│   ├── platform/                  # Cross-platform abstractions
│   │   ├── binary_resolver.rs     # which/where with login shell fallback
│   │   ├── shell.rs               # bash -c (Unix) / cmd /C (Windows)
│   │   ├── dump_tool.rs           # Process dump collection
│   │   └── mod.rs                 # Platform API exports
│   │
│   └── discord/                   # Discord bot (see dedicated section below)
│       ├── mod.rs                 # SharedData, bot boot, event handler
│       ├── router/                # Message routing (split from router.rs)
│       │   ├── message_handler.rs # Core message processing
│       │   ├── intake_gate.rs     # Intake dedup, dispatch guard, drain mode
│       │   ├── thread_binding.rs  # Thread binding resolution
│       │   └── tests.rs          
│       ├── turn_bridge/           # Agent turn lifecycle (split from turn_bridge.rs)
│       │   ├── mod.rs             # Turn dispatch, dequeue, lifecycle
│       │   ├── completion_guard.rs # Turn completion guard
│       │   ├── context_window.rs  # Context window management
│       │   ├── retry_state.rs     # Retry state tracking
│       │   ├── stale_resume.rs    # Stale session resume
│       │   ├── tmux_runtime.rs    # Tmux runtime helpers
│       │   └── tests.rs          
│       ├── tmux.rs                # Session output watcher, orphan cleanup
│       ├── recovery.rs            # Inflight turn recovery after restart
│       ├── health.rs              # Health registry, agent heartbeat HTTP server
│       ├── meeting.rs             # Round-table meetings
│       ├── model_catalog.rs       # Provider model catalog
│       ├── handoff.rs             # Agent handoff logic
│       ├── inflight.rs            # Inflight message tracking
│       ├── metrics.rs             # Performance metrics
│       ├── prompt_builder.rs      # Prompt construction with context
│       ├── org_schema.rs          # Organization schema management
│       ├── role_map.rs            # Role mapping for agents
│       ├── runtime_store.rs       # Runtime state storage
│       ├── settings.rs            # Per-channel settings
│       ├── shared_memory.rs       # Shared memory store
│       ├── adk_session.rs         # ADK session handling
│       ├── formatting.rs          # Discord message formatting
│       ├── restart_report.rs      # Crash report formatting
│       └── commands/              # Slash commands (see table below)
│
├── dispatch/                      # Task dispatch
│   └── mod.rs                     # Dispatch creation, agent routing
│
├── github/                        # GitHub integration (via `gh` CLI)
│   ├── sync.rs                    # Issue → kanban card sync
│   ├── triage.rs                  # Auto-triage
│   └── dod.rs                     # Definition of Done mirroring
│
├── ui/                            # Terminal UI
│   └── ai_screen.rs              # Interactive terminal screen
│
└── utils/
    └── format.rs                  # String formatting helpers

policies/                          # JS policy files (hot-reloadable)
├── kanban-rules.js                # Card transition rules
├── auto-queue.js                  # Auto-queuing + dispatch
├── review-automation.js           # Review automation
├── timeouts.js                    # Timeout detection
├── deploy-pipeline.js             # Deployment pipeline
├── merge-automation.js            # Merge automation rules
├── ci-recovery.js                 # CI pipeline recovery
├── pipeline.js                    # Pipeline routing
└── triage-rules.js                # Issue triage

dashboard/                         # React 19 + Vite + TypeScript + Tailwind
├── src/App.tsx                    # Main app
├── src/api/client.ts              # HTTP client
├── src/types/index.ts             # Type definitions
└── src/components/
    ├── agent-manager/             # Kanban board, agent management
    ├── office-view/               # Pixi.js office visualization
    ├── dashboard/                 # Dashboard widgets
    └── session-panel/             # Session detail view
```

---

## Troubleshooting: Where to Look

### "Discord message not processing"

```
Message received → discord/router.rs (intake_message)
  → dedup check (intake_dedup)
  → discord/turn_bridge.rs (dispatch_turn) — session spawn
    → claude.rs (execute_command_streaming) — ProcessBackend path
      → session_backend.rs (create_session) — spawn child process
        → tmux_wrapper.rs — actual Claude CLI execution
```

**Key files:** `discord/router/message_handler.rs` → `discord/turn_bridge/mod.rs` → `claude.rs` / `codex.rs` / `gemini.rs` / `qwen.rs`

### "Session died / no response"

```
Session health:   tmux_diagnostics.rs → session liveness check
Session kill:     discord/tmux.rs → kill_session_by_name()
Output watcher:   discord/tmux.rs → session_output_watcher() — JSONL file polling
Recovery:         discord/recovery.rs → restore_inflight_turns()
```

### "Kanban card state is wrong"

```
Card CRUD:        server/routes/kanban.rs
Card transitions: policies/kanban-rules.js → onCardTransition hook
Auto-queuing:     policies/auto-queue.js → onTick30s hook (every 30s)
Dispatch:         dispatch/mod.rs + engine/ops.rs (agentdesk.dispatch.create)
```

**State flow:** `backlog → ready → requested → in_progress → review → done`

### "API endpoint not working"

```
Route registration: server/routes/mod.rs → api_router()
Auth:               AGENTDESK_TOKEN env var check
DB queries:         individual routes/*.rs files
Policy hooks:       engine/mod.rs → fire_hook()
```

### "Policy not executing"

```
Policy loading: engine/loader.rs — file watcher, hot-reload
JS execution:   engine/mod.rs — QuickJS runtime
Rust bridge:    engine/ops.rs — agentdesk.db.query(), agentdesk.dispatch.create(), etc.
Tick trigger:   server/mod.rs → 3-tier tick: OnTick30s (30s), OnTick1min (1m), OnTick5min (5m)
```

### "Server won't start"

```
Entry point:    main.rs → tokio runtime creation
Config load:    config.rs → agentdesk.yaml
DB init:        db/mod.rs → db/schema.rs (migrations)
Server start:   server/mod.rs → axum router
dcserver mode:  cli/dcserver.rs → standalone Discord bot
```

---

## Discord Bot Internals

`src/services/discord/` — full bot logic. Total ~28,000 lines.

### Core Files

| File | Lines | Purpose |
|------|-------|---------|
| `mod.rs` | ~3,500 | SharedData struct, bot boot, event handler |
| `tmux.rs` | ~2,270 | Session lifecycle — output watcher, orphan cleanup, kill |
| `meeting.rs` | ~1,700 | Round-table meeting orchestration |
| `recovery.rs` | ~1,570 | Post-restart recovery — inflight turn restoration |
| `settings.rs` | ~1,530 | Per-channel settings |
| `formatting.rs` | ~1,180 | Discord message formatting |
| `model_catalog.rs` | ~1,080 | Provider model catalog |
| `health.rs` | ~720 | Health registry, agent heartbeat HTTP server |
| `org_schema.rs` | ~710 | Organization schema management |
| `adk_session.rs` | ~700 | ADK session handling |
| `prompt_builder.rs` | ~480 | Prompt construction with org context |
| `restart_report.rs` | ~480 | Crash report formatting |
| `handoff.rs` | ~260 | Agent-to-agent handoff |

### Split Modules (from #159 epic)

| Module | Files | Total Lines | Purpose |
|--------|-------|-------------|---------|
| `router/` | 5 | ~3,530 | Message routing — intake, dedup, dispatch guard, drain |
| `turn_bridge/` | 8 | ~3,300 | Agent turn lifecycle — spawn, cancel, completion, watchdog |
| `commands/` | 11 | ~3,570 | Slash commands (see table below) |

### Slash Commands

| Command | Description | File |
|---------|-------------|------|
| `/start [path]` | Start session with optional working dir | commands/session.rs |
| `/stop` | Cancel in-progress AI request | commands/session.rs |
| `/clear` | Reset conversation + session | commands/control.rs |
| `/model [name]` | Switch AI model (opus/sonnet/haiku) | commands/control.rs |
| `/skill <name>` | Execute provider skill | commands/skill.rs |
| `/diagnostics` | Session diagnostic info | commands/diagnostics.rs |
| `/config` | Per-channel settings | commands/config.rs |
| `/meeting start <agenda>` | Start round-table meeting | commands/meeting_cmd.rs |
| `/help` | Help text | commands/help.rs |

---

## HTTP API

Registered in `server/routes/mod.rs`. All endpoints prefixed with `/api/` except WebSocket.

| Endpoint | Methods | File | Description |
|----------|---------|------|-------------|
| `/ws` | WebSocket | ws.rs | Real-time updates (top-level, not under /api/) |
| `/api/health` | GET | health_api.rs | Health check |
| `/api/agents` | GET, POST | agents_crud.rs | Agent CRUD |
| `/api/agents/{id}` | GET, PATCH, DELETE | agents_crud.rs | |
| `/api/agents/{id}/timeline` | GET | agents.rs | Agent activity history |
| `/api/kanban-cards` | GET, POST | kanban.rs | Card CRUD |
| `/api/kanban-cards/{id}` | GET, PATCH, DELETE | kanban.rs | |
| `/api/kanban-cards/{id}/assign` | POST | kanban.rs | Assign to agent |
| `/api/kanban-cards/{id}/retry` | POST | kanban.rs | Retry card |
| `/api/kanban-cards/stalled` | GET | kanban.rs | Stalled cards |
| `/api/kanban-repos` | GET, POST | kanban_repos.rs | Target repositories |
| `/api/dispatches` | GET, POST | dispatches.rs | Dispatch CRUD |
| `/api/dispatches/{id}` | GET, PATCH | dispatches.rs | |
| `/api/dispatched-sessions` | GET | dispatched_sessions.rs | Session list |
| `/api/dispatched-sessions/{id}` | PATCH | dispatched_sessions.rs | |
| `/api/auto-queue/generate` | POST | auto_queue.rs | Manual queue generation |
| `/api/auto-queue/activate` | POST | auto_queue.rs | Manual activation |
| `/api/auto-queue/status` | GET | auto_queue.rs | Queue status |
| `/api/offices` | GET, POST | offices.rs | Office CRUD |
| `/api/departments` | GET, POST | departments.rs | Department CRUD |
| `/api/github/repos` | GET, POST | github.rs | GitHub integration |
| `/api/pipeline/stages` | GET, PUT, DELETE | pipeline.rs | Pipeline stages |
| `/api/review-verdict` | POST | review_verdict.rs | Review decisions |
| `/api/settings` | GET, PUT | settings.rs | Global settings |
| `/api/stats` | GET | stats.rs | Statistics |
| `/api/round-table-meetings` | GET, POST | meetings.rs | Meetings |
| `/api/skills/catalog` | GET | skills_api.rs | Skill catalog |
| `/api/onboarding/*` | GET, POST | onboarding.rs | Setup wizard backend |
| `/api/docs` | GET | docs.rs | API documentation |

---

## Policy Hook System

Handlers in `policies/*.js` are called by the Rust engine on lifecycle events.

### Lifecycle Hooks (10 total)

| Hook | Trigger | Primary Policy |
|------|---------|---------------|
| `onSessionStatusChange` | Agent session status changes | kanban-rules.js |
| `onCardTransition` | Card state transition | kanban-rules.js |
| `onCardTerminal` | Card reaches terminal state (done/cancelled) | auto-queue.js |
| `onDispatchCompleted` | Dispatch result received | kanban-rules.js |
| `onReviewEnter` | Card enters review stage | review-automation.js |
| `onReviewVerdict` | Review decision applied | review-automation.js |
| `onTick` | Every 5 minutes (legacy, backward compat) | auto-queue.js, timeouts.js |

### 3-Tier Tick Hooks (#127)

Fires on a dedicated OS thread (`policy-tick`) to avoid engine lock deadlock with request handlers.

| Hook | Interval | Purpose |
|------|----------|---------|
| `onTick30s` | 30s | Retry, unsent notification recovery, deadlock detection [I], orphan recovery [K] |
| `onTick1min` | 1 min | Non-critical timeouts [A][C][D][E][L], stale detection |
| `onTick5min` | 5 min | Non-critical reconciliation [R][B][F][G][H], context check |
| `onTick` (legacy) | 5 min | Backward compat for policies that only register onTick |

Implementation: `server/mod.rs` `policy_tick_loop()` uses a single 30s interval and fires higher tiers at multiples (2nd tick = 1min, 10th tick = 5min).

### JS Bridge Functions (`engine/ops.rs`)

```javascript
agentdesk.db.query(sql, params)                    // SELECT → array
agentdesk.db.execute(sql, params)                  // INSERT/UPDATE/DELETE → {changes: N}
agentdesk.dispatch.create(card_id, agent_id, type, title)  // Create dispatch
agentdesk.http.post(url, body, headers)            // External HTTP call
agentdesk.config.get(key)                          // Config value lookup
agentdesk.log.info(msg) / .warn(msg) / .error(msg) // Logging
```

### Policy Priority

Lower number runs first:

| Priority | Policy | Role |
|----------|--------|------|
| 10 | kanban-rules.js | Card transition rules, PM gate |
| 50 | review-automation.js | Review automation |
| 60 | merge-automation.js | Merge automation rules |
| 100 | timeouts.js | Timeout detection |
| 150 | ci-recovery.js | CI pipeline recovery |
| 200 | deploy-pipeline.js | Deployment pipeline |
| 200 | pipeline.js | Pipeline stages |
| 300 | triage-rules.js | Auto-triage |
| 500 | auto-queue.js | Auto-queuing |

---

## Session Lifecycle

### Unix (tmux)
```
1. Message → discord/turn_bridge.rs: dispatch_turn()
2. Session create → tmux new-session → tmux_wrapper runs Claude CLI
3. Output: tmux capture-pane + JSONL file
4. Kill: tmux kill-session
5. Recovery: tmux list-sessions to find surviving sessions
```

### Windows (ProcessBackend)
```
1. Message → discord/turn_bridge.rs: dispatch_turn()
2. Session create → session_backend.rs: ProcessBackend.create_session()
   └─ Spawn wrapper as child process with stdin pipe
3. Input: stdin pipe writes
4. Output: JSONL file polling
5. Kill: taskkill /T /F /PID
6. Recovery: claude --resume
```

---

## DB Schema (Key Tables)

```sql
agents (id, name, discord_channel_id, provider, model, ...)

kanban_cards (id, title, status, priority, repo_id, assigned_agent_id,
             github_issue_number, started_at, completed_at, ...)
  -- status: backlog → ready → requested → in_progress → review → done | cancelled

task_dispatches (id, kanban_card_id, agent_id, provider, status, result, ...)

sessions (id, session_key, agent_id, provider, status, model, ...)

auto_queue_runs (id, repo, agent_id, status, ai_model, ai_rationale,
                timeout_minutes, unified_thread, created_at, completed_at)

auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank,
                   reason, status, dispatch_id, created_at, dispatched_at, completed_at)

github_repos (id, display_name, sync_enabled, default_agent_id)

kv_meta (key, value)
```

---

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `AGENTDESK_TOKEN` | HTTP server auth token | none |
| `AGENTDESK_ROOT_DIR` | Data directory | `~/.agentdesk` |
| `AGENTDESK_STATUS_INTERVAL_SECS` | Status polling interval | 5 |
| `AGENTDESK_TURN_TIMEOUT_SECS` | Turn watchdog timeout | 3600 |
| `RUST_LOG` | Logging filter | `agentdesk=info` |
