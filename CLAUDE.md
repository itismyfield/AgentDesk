# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is AgentDesk

AgentDesk is a single-binary AI agent orchestration platform written in Rust. It manages multiple AI agents (Claude, Codex) through Discord, orchestrates task dispatch via a Kanban board, and uses hot-reloadable JavaScript policies for business logic. Release runtime state is canonical in PostgreSQL; remaining SQLite code is transitional compatibility work under epic #834.

## Documentation

When modifying code or investigating issues, refer to these documents first:

- **`ARCHITECTURE.md`** — Code navigation guide. Directory structure, call chains for common scenarios ("message not processing" → which files), API endpoint list, session lifecycle, DB schema.
- **`FEATURES.md`** — Feature specification. Kanban state flow, auto-queue, review automation, timeout rules, dispatch system, Discord commands, policy engine hooks and bridge functions.
- **`docs/background-task-pattern.md`** — Notify-bot delivery convention for `Bash run_in_background` results plus the safe / read-only / needs-confirm / destructive action classification (#796). Read this before adding any auto-progression logic that fires off another action after a background task completes.

## Build & Run

```bash
# Build (debug)
cargo build

# Build (release — optimized for size, LTO enabled)
cargo build --release

# Runtime build/promotion entrypoint (release only)
./scripts/build-release.sh
./scripts/promote-release.sh

# Run the server (loads agentdesk.yaml from working dir or ~/.agentdesk/)
cargo run

# Dashboard (React frontend)
./scripts/verify-dashboard.sh                # CI-aligned install + build + test (Node >=22)
cd dashboard && npm install && npm run dev   # dev server
cd dashboard && npm run build                # production build → dist/

# Dashboard deploy (IMPORTANT: server serves from dashboard/dist/, NOT dashboard/)
scripts/deploy-dashboard.sh release           # build + deploy to ~/.adk/release/dashboard/dist/
```

**Dashboard deploy path**: The server serves static files from `$RUNTIME_ROOT/dashboard/dist/` (see `src/server/mod.rs`). Never copy build output to `dashboard/` root — always target `dashboard/dist/`.

**Runtime policy**: release only. Do not use `scripts/deploy-dev.sh` as a build or restart entrypoint, and do not start `dcserver` from `~/.adk/dev`. `scripts/deploy-dev.sh` is a cleanup shim for removing stray dev artifacts.

After the 2026-04-19 PostgreSQL cutover (#461), AgentDesk runtime state is canonical in PostgreSQL. Ongoing SQLite retirement work is tracked in epic #834. `~/.adk/release/data/agentdesk.sqlite` may still exist as a pre-cutover backup, but the release runtime does not read it.

**Phase C split status**: the smaller Phase C cleanup landed transcript-search `tsvector` migration, dead `cron_history` removal, and docs cleanup first. `AppState.db` removal, `dispatch.create` PostgreSQL porting, `DbPool` retirement, and startup SQLite-init removal are intentionally deferred to follow-up issues rather than mixed into an oversized PR.

**Signing guidance**: local operators may use ad-hoc signing during release promotion when Developer ID signing is unavailable, and should clear `com.apple.quarantine` on promoted macOS artifacts when needed. Keep signing policy in deploy scripts and operator docs, not hardcoded in Rust source.

## CLI Subcommands

The binary doubles as a CLI. Subcommands are parsed before the tokio runtime starts:

- `--dcserver [token]` — start the Discord bot server standalone
- `--init` / `--reconfigure` — interactive setup wizard
- `--doctor` — system diagnostics
- `--tmux-wrapper` / `--codex-tmux-wrapper` — process wrappers for agent providers
- `--discord-sendfile`, `--discord-sendmessage`, `--discord-senddm` — Discord message utilities
- `--restart-dcserver` — restart with crash report context

Default (no subcommand) starts the full HTTP server + policy engine + Discord gateway.

## Database Path

- Canonical runtime store is PostgreSQL: use `config.database.{host,port,dbname}` from `agentdesk.yaml` or the resolved `DATABASE_URL`.
- Ongoing cleanup is tracked in epic #834. The 2026-04-19 cutover (#461) made PostgreSQL the only runtime authority for release operations.
- `~/.adk/release/data/agentdesk.sqlite` is a pre-cutover backup artifact, not a live runtime database.
- Do not point `sqlite3` at guessed paths such as `~/.adk/release/agentdesk.db` or `~/.adk/release/data.db`. SQLite will create empty files there, which then mislead diagnostics.
- Prefer the HTTP API first for operational inspection; use direct DB access only when the task explicitly requires it and the canonical PostgreSQL target is confirmed.

## Architecture

### Core Components (src/)

- **`main.rs`** — Entry point. CLI dispatch, then tokio runtime for the server.
- **`config.rs`** — Parses `agentdesk.yaml` (YAML config).
- **`db/`** — PostgreSQL runtime schema/migrations plus transitional SQLite compatibility helpers that have not been removed yet.
- **`server/`** — Axum HTTP server + WebSocket broadcast (`ws.rs`). 30+ route modules under `routes/`.
  - `/ws` WebSocket endpoint is mounted at top-level (not under `/api/`).
  - All other API endpoints are nested under `/api/`.
- **`engine/`** — QuickJS (rquickjs) policy engine:
  - `mod.rs` — JS runtime init, injects `agentdesk.*` global namespace
  - `ops.rs` — ~30 Rust↔JS bridge functions (db queries, Discord sends, kanban transitions, dispatch creation)
  - `loader.rs` — File watcher for hot-reloading `policies/*.js`
  - `hooks.rs` — 10 lifecycle hooks: `OnSessionStatusChange`, `OnCardTransition`, `OnCardTerminal`, `OnDispatchCompleted`, `OnReviewEnter`, `OnReviewVerdict`, `OnTick`, `OnTick30s`, `OnTick1min`, `OnTick5min`
- **`dispatch/`** — Task dispatch creation, routing to agents, result handling.
- **`github/`** — GitHub issue sync (`sync.rs`), auto-triage (`triage.rs`), DoD mirroring (`dod.rs`). Uses `gh` CLI.
- **`services/discord/`** — Serenity/Poise Discord bot. Key files:
  - `router.rs` — Message routing, intake dedup, mention filtering
  - `turn_bridge.rs` — Agent turn lifecycle, heartbeat, watchdog
  - `tmux.rs` — Session output watcher, orphan cleanup, session lifecycle
  - `meeting.rs` — Round-table meetings
  - `recovery.rs` — Inflight turn recovery after restart
  - `commands/` — Slash commands (/start, /stop, /clear, /model, /skill, /diagnostics, /meeting, /help)
- **`services/`** — Provider integrations:
  - `claude.rs` / `codex.rs` — AI provider streaming execution
  - `session_backend.rs` — ProcessBackend: child process spawning, stdin pipe IPC
  - `tmux_common.rs` — Temp file paths, owner markers
  - `tmux_diagnostics.rs` — Exit reason tracking, death diagnostics
  - `tmux_wrapper.rs` / `codex_tmux_wrapper.rs` — Process execution wrappers
  - `provider.rs` — ProviderKind enum, session name construction
  - `platform/` — Cross-platform abstractions: binary resolver, shell commands

### Policies (policies/)

JavaScript files loaded by the QuickJS engine. Each exports a default object with lifecycle hook handlers. They are hot-reloaded on file change when `policies.hot_reload: true` in config.

Current policies: `kanban-rules.js`, `review-automation.js`, `auto-queue.js`, `pipeline.js`, `triage-rules.js`, `timeouts.js`

### Dashboard (dashboard/)

React 19 + TypeScript + Vite + Tailwind. Uses Pixi.js for the office visualization. Connects to the backend API at the same host.

## Key Patterns

- **AppState** — Struct with fields `db`, `engine`, `broadcast_tx`, `batch_buffer`, `health_registry`, passed to all Axum route handlers via `.with_state()`. Defined at `server/routes/mod.rs`. This still carries a transitional SQLite compatibility handle; do not treat it as the runtime source of truth.
- **Db** — transitional SQLite compatibility wrapper used by legacy routes/tests. PostgreSQL is the runtime authority.
- **PolicyEngine** — Thread-safe wrapper around QuickJS. Call hooks via `engine.fire_hook(hook, payload)`.
- **3-Tier Tick** — Dedicated OS thread runs tiered hooks: `OnTick30s` (30s), `OnTick1min` (1m), `OnTick5min` (5m), `OnTick` (5m legacy). See `server/mod.rs` `policy_tick_loop()`.
- **SessionKey** — Format: `"hostname:session-name"` — uniquely identifies agent sessions.
- **ProviderKind** — Enum (`Claude` / `Codex`) for routing to the correct AI backend.
- **ProcessBackend** — Spawns agent CLI as child process with stdin pipe for input.
- **Config** is loaded once at startup from `agentdesk.yaml` (searched in CWD, then `~/.agentdesk/`).
- **Auto-Queue Storage** — Uses `auto_queue_runs` and `auto_queue_entries` tables (not `dispatch_queue`).

## Git Safety Rules

- **NEVER checkout `wt/*` branches on the main workspace repo.** These branches belong to git worktrees only. Checking them out on the main repo breaks all sessions when the worktree directory is cleaned up. If you need to inspect a worktree branch, use `git log wt/branch-name` or `git diff wt/branch-name` without switching branches.
- **NEVER run `git checkout` to switch away from `main`** on the workspace root unless explicitly instructed by the user. Feature work should use git worktrees (`git worktree add`), not branch switching on the main repo.

## Search Safety

- Prefer `rg` or the harness Grep tool over `grep -r` / `grep -rn`. They respect ignore files and avoid crawling `target/` and other generated trees by default.
- Scope searches to the smallest relevant directory (`src/`, `config/`, `docs/`, specific module paths) instead of the workspace root whenever possible.
- If recursive `grep` is unavoidable, always pass `--exclude-dir={target,node_modules,.git,dist,build,.next}`.
- Do not recursively scan build artifacts or dependency directories in ways that can stall a turn without output.

## Environment Variables

- `AGENTDESK_TOKEN` — Auth token for the HTTP server
- `AGENTDESK_ROOT_DIR` — Override data directory (default: `~/.agentdesk`)
- `AGENTDESK_STATUS_INTERVAL_SECS` — Status polling interval (default: 5)
- `AGENTDESK_TURN_TIMEOUT_SECS` — Turn watchdog timeout (default: 3600)
- `RUST_LOG` — Standard tracing filter (default directive: `agentdesk=info`)

## Config

See `agentdesk.example.yaml` for the full config structure. Key sections: `server`, `discord.bots`, `agents`, `github`, `policies`, `data`, `kanban`.
