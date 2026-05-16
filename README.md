# AgentDesk

> AI agent orchestration platform — a single Rust binary that manages teams of AI agents through Discord, with a web dashboard, kanban pipeline, and hot-reloadable policy engine.

AgentDesk lets you run multiple AI agents (Claude Code, Codex, or any CLI-based provider) as a coordinated team. Agents communicate through Discord, execute tasks via tmux sessions, and follow configurable workflows defined in JavaScript policy files.

## Quick Start

### One-Click Install (macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/itismyfield/AgentDesk/main/scripts/install.sh | bash
```

This will:
1. Download the latest release (or build from source if no release is available)
2. Install to `~/.adk/release/`
3. Register a launchd service (auto-starts on boot)
4. Open the web dashboard for guided onboarding

### Windows and Linux Native Runtime

Windows and Linux run natively today, but they use the manual/runtime-first path instead of the macOS `curl | bash` bootstrap.

1. Download the matching release artifact (`agentdesk-linux-<arch>.tar.gz` or `agentdesk-windows-<arch>.zip`) or build from source with `cargo build --release`.
2. Run `agentdesk init` (`agentdesk.exe init` on Windows) to create the runtime under `~/.adk/release` or `%USERPROFILE%\.adk\release`.
3. Start `agentdesk dcserver` directly, or register the generated service path with `systemd --user` on Linux or `nssm` / `sc.exe` on Windows.
4. Verify the host runtime with `agentdesk doctor`. Mutating repairs require explicit flags such as `agentdesk doctor --fix --allow-restart` or `agentdesk doctor --fix --repair-sqlite-cache`.

When tmux is unavailable, provider turns automatically fall back to `ProcessBackend` instead of tmux sessions. That path still posts ADK session heartbeats every 30 seconds, but live child processes cannot be reattached after a `dcserver` restart. After restart, AgentDesk starts a fresh backend process on the next turn and only restores provider-native session IDs when the underlying CLI supports resume.

### Build from Source

```bash
git clone https://github.com/itismyfield/AgentDesk.git
cd AgentDesk
cargo build --release

# Verify the dashboard with the same command CI uses (Node >=22)
./scripts/verify-dashboard.sh

# Initialize
./target/release/agentdesk init
```

#### Shared rustc Cache with `sccache`

AgentDesk intentionally keeps a separate `target/` directory per worktree. Sharing `CARGO_TARGET_DIR` across always-parallel worktrees causes Cargo lock contention, so the supported acceleration path is a shared `sccache` rustc cache instead.

- `.cargo/config.toml` points `build.rustc-wrapper` at `scripts/rustc-wrapper-sccache.sh`
- the wrapper transparently uses `sccache` when it is on `PATH` and falls back to a plain `rustc` exec when it is not — bare `cargo build`/`cargo test` works on every machine without a `RUSTC_WRAPPER=` override
- worktree builds use the documented env default `SCCACHE_CACHE_SIZE=10G`; export another value before building to override it
- set `RUSTC_WRAPPER_DISABLE=1` to bypass the wrapper entirely (rare; debugging only)

Install `sccache` to actually engage caching:

```bash
brew install sccache
# or, if a package manager is unavailable:
cargo install sccache --locked
```

```powershell
winget install Mozilla.sccache
```

Quick verification / troubleshooting:

```bash
sccache --stop-server || true
sccache --zero-stats || true
cargo build --bin agentdesk
sccache --show-stats
```

The historical `No such file or directory (os error 2)` failure mode no longer applies — Cargo invokes the wrapper script, which gracefully degrades when sccache is missing.

### Dawn LaunchDaemon Operations (macOS)

If you also install observability skills such as `memory-dream`, `service-monitoring`, `version-watch`, and `hardware-audit`, use `scripts/manage_dawn_launchdaemons.py` to manage their dawn `LaunchDaemon` jobs from a single entrypoint.

The script automatically searches common skill install roots in this order:
- `$AGENTDESK_SKILLS_ROOT`
- `$CODEX_HOME/skills`
- `~/.codex/skills`
- `~/.adk/release/skills`
- `<repo>/skills`

First-time bootstrap:

```bash
sudo /opt/homebrew/bin/python3 ./scripts/manage_dawn_launchdaemons.py bootstrap
```

What that one command does:
- installs the `agentdesk-dawn-manager` sudoers drop-in
- installs or refreshes the selected dawn `LaunchDaemon` plists
- keeps later `status` checks on the non-root path by default

Useful follow-ups:

```bash
python3 ./scripts/manage_dawn_launchdaemons.py preflight
python3 ./scripts/manage_dawn_launchdaemons.py status
python3 ./scripts/manage_dawn_launchdaemons.py install
python3 ./scripts/manage_dawn_launchdaemons.py uninstall
```

If your skills live outside the default roots, pass one or more `--skills-root <path>` flags.

## Onboarding

After installation, the web dashboard opens automatically at `http://127.0.0.1:8791`. The onboarding wizard walks you through:

### Step 1: Discord Bot Setup
Create Discord bots in the [Discord Developer Portal](https://discord.com/developers/applications). AgentDesk uses three distinct bot identities — each with its own Discord application and token:

| Bot key | Role | Required Permissions |
|---------|------|---------------------|
| **`announce`** | Authoritative trigger source: posts dispatch envelopes (`DISPATCH:<uuid>`), PM/escalation cards, and agent-to-agent routing via `/api/discord/send`. Other agent bots treat its messages as turn triggers. | Administrator (simplest) or Send Messages + Manage Messages + Manage Channels + Manage Roles |
| **`notify`** | Informational notifications only — issue announcement cards (`📋 새 이슈 #N`, `✅ #N 완료`), system alerts. Its user id is intentionally NOT in `allowed_bot_ids`, so its messages do not trigger agent turns (#1448 follow-up). | Send Messages, Read Message History |
| **Per-provider** (`claude`, `codex`, …) | User-facing bots humans @-mention in agent channels. When a user posts in a channel mapped to one of these bots, AgentDesk runs a turn for that provider. Add one bot per provider you plan to use. | Send Messages, Read Message History, Manage Messages, Create Public Threads, Send Messages in Threads (the wizard's generated invite includes all of these — required for dispatch threads) |

**Important:** On the Bot tab for every application, enable the **MESSAGE CONTENT** Privileged Gateway Intent. Without this, bots cannot read message content and will not function properly.

After entering and validating each bot token, the wizard generates OAuth2 invite links with the correct permissions pre-configured — just click to invite each bot to your Discord server.

### Step 2: Provider Verification
The wizard checks whether Claude Code or Codex CLI is installed and authenticated on your machine. If not, it provides installation and login instructions. Provider setup is **not required** to complete onboarding — you can configure it later.

#### Claude TUI Hosting Security Note
When AgentDesk hosts Claude Code through the Claude TUI session path, it starts the CLI with `--dangerously-skip-permissions` so the Discord relay can drive the interactive TUI without repeated local permission prompts. Treat the configured working directory as the security boundary for that hosted process: use a dedicated worktree or workspace for each task, and do not point hosted sessions at broad home, credential, or operator directories.

Prompt submission also rejects terminal control characters before it writes to the TUI. Newline, carriage return, and tab are allowed for normal prompt text, but escape sequences such as bracketed paste markers (`\x1b[200~` / `\x1b[201~`), DEL, and C1 controls are blocked before literal send or tmux paste-buffer paths can relay them.

### Step 3: Agent Selection
Choose from three built-in role presets or compose a custom team:

| Preset | Agents | Use Case |
|--------|--------|----------|
| **Delivery Squad** | PM, Designer, Developer, QA | Role-based execution team focused on shipping |
| **Operations Cell** | Ops Lead, Scheduler, Support, Records | Stabilizing recurring workflows and incident triage |
| **Insight Desk** | Researcher, Analyst, Strategist, Writer | Research, analysis, and shareable writeups |

Presets ship with Korean-first prompts; English copy is shown side-by-side in the wizard. Custom agents can be added with a name, description, and system prompt — the "AI Generate" button drafts the prompt using your configured provider CLI.

### Step 4: Channel Setup
Map each agent to a Discord channel. The wizard recommends channel names based on agent IDs (e.g., `scheduler-cc` for a Claude-powered scheduler). You can select existing channels or enter new names.

### Step 5: Owner & Confirmation
Enter your Discord User ID (found via Developer Mode → right-click your profile → Copy User ID). The owner gets direct command access and admin privileges. Review the complete setup summary and click "Complete Setup".

## Features

### Kanban Pipeline
Cards flow through a configurable pipeline with automated transitions:

```
backlog → ready → requested → in_progress → review → done
                                    ↓            ↓
                              pending_decision  blocked
```

- **Pipeline-driven transitions** — States, gates, and hooks are defined in YAML pipeline configs
- **Dispatch-driven lifecycle** — Cards advance via task dispatches, not manual status changes
- **Counter-model review** — Claude reviews Codex's work and vice versa, with configurable max rounds
- **Auto-queue with batch phases** — Automatic card progression with priority scoring, dependency-aware phased execution, and phase gate verification
- **GitHub sync** — Bidirectional issue synchronization with DoD checklist mirroring
- **Escalation routing** — PM/user/time-based escalation mode switching
- **Audit logging** — Every state transition and dispatch event is recorded

### Policy Engine
Business logic lives in JavaScript files under `policies/`, hot-reloaded without restarting:

| Policy | Purpose |
|--------|---------|
| `00-escalation.js` | Manual-intervention routing, PM cooldown management, escalation flush loop |
| `00-pr-tracking.js` | Card→PR linkage cache and PR sync helpers |
| `kanban-rules.js` | Core lifecycle: dispatch completion, PM decision gates, worktree auto-merge |
| `review-automation.js` | Counter-model review dispatch, verdict processing, review state sync |
| `auto-queue.js` | Batch-phased card queuing, phase gate dispatch, slot management |
| `phase_gate.js` | Phase gate verification before opening the next batch |
| `ci-recovery.js` | CI failure detection, auto-rerun, and recovery card creation |
| `timeouts.js` (+ `timeouts/` modules) | Stale card detection, deadlock recovery, idle session kill, branch guard, dispatch maintenance |
| `triage-rules.js` | GitHub issue auto-classification and agent assignment |
| `pipeline.js` | Multi-stage workflow progression |
| `merge-automation.js` | PR auto-merge, worktree cleanup after merge |

Repository policy files under `policies/` are canonical for shipped behavior. Release copies under `~/.adk/release/policies/` are deployment replicas, and operator-local policy directories are extensions selected by `policies.dir`. For the complete policy/config source map, see [`docs/source-of-truth.md`](docs/source-of-truth.md).

### Policy Tests
Local policy development now has a dedicated Node runner for the repository copies under `policies/`.

Run the full suite:

```bash
npm run test:policies
```

What the harness does:
- Loads each policy file in a Node VM without booting the Rust server
- Injects a mocked `agentdesk` bridge so hooks can execute side-effect paths
- Lets tests assert dispatch creation, status transitions, review state sync, and SQL-driven branches

Where to add tests:
- `policies/__tests__/kanban-rules.test.js`
- `policies/__tests__/auto-queue.test.js`
- `policies/__tests__/review-automation.test.js`
- `policies/__tests__/phase_gate.test.js`
- `policies/__tests__/timeouts.test.js`
- Shared helpers live in `policies/__tests__/support/harness.js`

Test-writing rules:
- Prefer hook-level coverage first (`onCardTransition`, `onDispatchCompleted`, `onReviewEnter`)
- Mock only the SQL and bridge calls the scenario actually needs
- Keep assertions on observable side effects: `agentdesk.dispatch.create`, `agentdesk.kanban.setStatus`, `agentdesk.kanban.setReviewStatus`, `agentdesk.reviewState.sync`
- Export only Node-safe test surfaces from policy files via guarded `module.exports` blocks so QuickJS runtime behavior stays unchanged

### Tiered Tick System
Policies hook into a 3-tier periodic tick system running on a dedicated OS thread:

| Hook | Interval | Typical Use |
|------|----------|-------------|
| `onTick30s` | 30s | Retry recovery, notification delivery, deadlock detection |
| `onTick1min` | 1m | Stale card timeouts, auto-queue walk, CI recovery |
| `onTick5min` | 5m | Idle session cleanup, merge queue processing, reconciliation |
| `onTick` | 5m (legacy) | Backward compatibility for older policies |

### Multi-Bot Architecture
Each bot identity has a distinct role so message intent stays unambiguous:

- **`announce`** is the authoritative trigger source. It posts dispatch envelopes (`── implementation dispatch ──\nDISPATCH:<uuid>…`), PM/escalation cards, and agent-to-agent messages routed through `/api/discord/send`. Other agent bots accept its messages as turn triggers when its user id is listed under `auth.allowed_bot_ids`.
- **`notify`** delivers informational notifications — issue announcement cards (`📋 새 이슈 #N`, `✅ #N 완료`) and system alerts. Its user id is intentionally omitted from `allowed_bot_ids`, so its posts never trigger an agent turn (the routing change landed in #1448 follow-up; the message router still suppresses pre-deploy `announce`-authored issue cards via `is_legacy_announce_issue_card` until the catch-up window expires on 2026-06-01).
- **Per-provider bots** (`claude`, `codex`, optionally `gemini` / `opencode` / `qwen`) are the user-facing identities humans @-mention in agent channels. A user message in a channel mapped to one of these bots starts a turn for that provider. Each agent can be wired to multiple provider bots — see `agents[].channels` in `agentdesk.yaml` — letting Claude and Codex (or any combination) work side-by-side on the same agent role.

### Web Dashboard
A React-based dashboard served from the same binary:

- **Office View** — Virtual 2D office with agent avatars (Pixi.js)
- **Kanban Board** — Drag-and-drop card management with column filters
- **Agent Manager** — Agent configuration, skills, timeline, sessions, kanban tab
- **Control Center** — Runtime controls, dispatch monitoring, system health
- **Analytics** — Streaks, achievements, activity heatmaps, audit logs
- **Meeting Minutes** — Round-table meeting transcripts with issue extraction
- **Settings** — Runtime configuration, onboarding re-run, policy management, escalation routing

### Round-Table Meetings
Coordinate multi-agent discussions with structured rounds, automatic transcript recording, and post-meeting issue extraction to GitHub.

### Turn Orchestration
Agent turn lifecycle is managed by a dedicated orchestration layer (`src/services/turn_orchestrator.rs`, extracted from the Discord adapter):
- **Heartbeat monitoring** — Tracks agent session liveness
- **Completion guard** — Validates dispatch results before card transitions
- **Deadlock detection** — Identifies stuck sessions and auto-recovers with configurable thresholds; the stall watchdog and THREAD-GUARD stale-check landed in #1446
- **Watcher lifecycle** — Tmux watcher shutdown rules live in `src/services/discord/watchers/lifecycle.rs`, split out of `tmux.rs` in #1435
- **Reaction cleanup** — Mixed ⏳/❌ dispatch reactions are normalized by the cleanup pass added in #1445
- **Inflight tracking** — Per-provider inflight files for concurrent session management

### Multinode Runtime
AgentDesk can run multiple release instances against one PostgreSQL control plane. Each node advertises identity, labels, provider capabilities, and node-local MCP health through `worker_nodes`, while PostgreSQL leases prevent duplicate cluster-wide side effects.

- **Leader/worker split** — Each node boots with `cluster.instance_id`, acquires the cluster leader advisory lease when eligible, and records its effective role through `/api/cluster/nodes`.
- **Capability-aware routing** — Dispatch claims compare required labels/providers/MCP health against registered worker capabilities and record routing diagnostics when a node is ineligible.
- **Lease-backed work claims** — `task_dispatches` and dispatch outbox rows use PG claim owner, expiry, and idempotency fields so multiple nodes do not process the same work item concurrently.
- **Exclusive resource locks** — `/api/cluster/resource-locks*` serializes node-local resources such as Unreal Editor, MCP endpoints, and project-level test execution.
- **Issue-as-spec phase evidence** — `issue_specs`, `test_phase_runs`, and `test_results` connect GitHub issue acceptance criteria to deterministic phase runs, and `policies/merge-automation.js` blocks merge when required phase evidence is missing for the current PR head SHA.
- **Regression coverage** — `.github/workflows/ci-nightly.yml` runs the `multinode_regression` job; the physical MacBook + Mac mini smoke plan lives in `docs/agent-maintenance/multinode-two-node-smoke.md`.

## Configuration

### agentdesk.yaml

The main configuration file at `~/.adk/release/config/agentdesk.yaml`:

```yaml
server:
  port: 8791              # HTTP server port
  host: "0.0.0.0"         # Bind address
  auth_token: "secret"    # Optional API authentication token

discord:
  bots:
    announce:                       # Authoritative trigger bot — DISPATCH cards, PM cards, /api/discord/send
      token: "your-announce-bot-token"
      description: Announce
      agent: announce
    notify:                         # Informational bot — issue/completion cards (#1448 follow-up)
      token: "your-notify-bot-token"
      description: Notify
      agent: notify
    claude:                         # Per-provider user-facing bot for Claude Code
      token: "your-claude-bot-token"
      description: Claude
      provider: claude
      auth:
        allowed_bot_ids: [<announce-user-id>]   # accept turns triggered by announce-bot
        allow_all_users: true
    codex:                          # Per-provider user-facing bot for Codex
      token: "your-codex-bot-token"
      description: Codex
      provider: codex
      auth:
        allowed_bot_ids: [<announce-user-id>]
        allow_all_users: true
  guild_id: "your-guild-id"
  owner_id: 123456789012345678
  dm_default_agent: family-counsel  # optional: route DMs without a binding to this agent

# Optional top-level MCP server registry (used by Memento and other MCP-aware agents)
mcp_servers:
  memento:
    url: http://127.0.0.1:57332/mcp
    auth:
      type: bearer
      token_env_var: MEMENTO_ACCESS_KEY
review_mcp_allowlist: [memento, github] # optional: extra MCP servers exposed to review/review-decision dispatches

shared_prompt: ~/.adk/release/config/agents/_shared.prompt.md

agents:
  - id: my-agent
    name: My Agent
    provider: claude                # default provider for this agent
    channels:
      claude:                       # bind one channel per provider you want to use
        id: "channel-id"
        name: my-agent-cc
        workspace: ~/.adk/release/workspaces/my-project
        prompt_file: ~/.adk/release/config/agents/my-agent.prompt.md
      codex:
        id: "channel-id"
        name: my-agent-cdx
        workspace: ~/.adk/release/workspaces/my-project
    department: engineering
    avatar_emoji: "🔧"

github:
  repos:
    - "owner/repo-name"
  sync_interval_minutes: 10

# PostgreSQL is the live datastore (SQLite is legacy / test-only since the #868 / #1239 cutover).
database:
  enabled: true
  host: 127.0.0.1
  port: 5432
  dbname: agentdesk
  user: agentdesk

# Optional multinode runtime identity. Nodes that share the same PostgreSQL
# database coordinate through worker heartbeats, advisory leadership leases,
# dispatch claims, and resource locks.
cluster:
  enabled: true
  instance_id: mac-mini-release    # use a unique value per host, e.g. mac-book-release
  role: auto                       # auto | leader | worker
  heartbeat_interval_secs: 10
  lease_ttl_secs: 30
  labels: [mac-mini, release]
  capabilities:
    providers: [codex, claude, gemini, qwen]
    max_agent_turns: 1
    max_unreal_tests: 0
    mcp:
      unreal_editor:
        healthy: false
  dispatch_routing:
    default_preferred_labels: [mac-book]
    opt_out_dispatch_types: ["create-pr", "github-sync"]
    constraints: [noop]

# Optional file/MCP memory configuration. Omit the section entirely to use defaults.
memory:
  backend: auto                     # auto | file | memento
  file:
    sak_path: "memories/shared-agent-knowledge/shared_knowledge.md"
    sam_path: "memories/shared-agent-memory"
    ltm_root: "memories/long-term"
    auto_memory_root: "~/.claude/projects/*{workspace}*/memory/"
  mcp:
    endpoint: "http://127.0.0.1:8765"
    access_key_env: "MEMENTO_API_KEY"

policies:
  dir: "./policies"
  hot_reload: true

# Live-tunable kanban runtime knobs. Most timeout values live under `runtime:` (see
# `RuntimeSettingsConfig`) and can be hot-edited from the dashboard runtime panel.
kanban:
  manager_channel_id: "channel-id"  # optional: PM/manager Discord channel
  pm_decision_gate_enabled: true

review:
  enabled: true
  max_rounds: 3
```

For canonical edit paths across runtime config, prompts, policies, memory, `CLAUDE.md`, and MCP mirrors, see [`docs/source-of-truth.md`](docs/source-of-truth.md). Legacy config snapshots (`*.pre-*`, `*.bak`, `*.migrated`) are archival only and belong under `~/.adk/release/config/.backups/YYYY-MM-DD/`; use `scripts/archive-config-backups.sh` instead of leaving them beside canonical files.

### Runtime Configuration

AgentDesk keeps settings in multiple surfaces on purpose. The contract is per-surface canonical owner plus explicit precedence and restart semantics, not a single physical store. The full decision record lives in [`docs/adr-settings-precedence.md`](docs/adr-settings-precedence.md).

| Surface | Canonical owner | Storage / precedence | Persistence and restart semantics | API |
|---------|------------------|----------------------|-----------------------------------|-----|
| Company settings | General settings UI / callers that own the merged JSON | `kv_meta['settings']` only. No YAML baseline. | Persists until replaced. `PUT /api/settings` is full replace, so callers must merge hidden keys before saving. Retired legacy keys are stripped server-side. | `GET/PUT /api/settings` |
| Runtime config | Dashboard live-runtime controls | Hardcoded defaults < `agentdesk.yaml` `runtime:` < `kv_meta['runtime-config']` override JSON | Applies immediately. On reboot, YAML-backed keys are re-applied; saved keys without YAML baselines persist unless `runtime.reset_overrides_on_restart=true`, in which case the whole surface resets to baseline. | `GET/PUT /api/settings/runtime-config` |
| Policy/config keys | Dashboard policy controls and automation helpers | Hardcoded defaults < YAML sections (`review:`, `runtime:`, `automation:`, `kanban:`) < individual `kv_meta` rows | `PATCH` writes live overrides, including merge policy keys like `merge_strategy_mode`. YAML-backed keys are re-seeded on restart, while hardcoded-only keys keep their DB override unless the reset flag is on. `server_port` is surfaced as read-only config metadata. | `GET/PATCH /api/settings/config` |
| Escalation routing | Dashboard escalation panel and Discord `!escalation` command | `escalation:` config baseline plus fallback owner/channel defaults, overridden by `kv_meta['escalation-settings-override']` | Override persists until changed back to defaults. When `runtime.reset_overrides_on_restart=true`, the stored escalation override is cleared on boot. | `GET/PUT /api/settings/escalation` |
| Onboarding/secrets | Dedicated onboarding wizard | Dedicated onboarding keys and flows | Tokens and setup secrets stay outside the general settings form. | `/api/onboarding/*` |

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `AGENTDESK_ROOT_DIR` | Override runtime directory (default: `~/.adk/release`) |
| `AGENTDESK_CONFIG` | Override config file path |
| `AGENTDESK_REPO_DIR` | Override resolved AgentDesk repo path used by `git`/`gh` exec helpers |
| `AGENTDESK_SERVER_PORT` | Override HTTP server port (default: 8791) |
| `AGENTDESK_API_URL` | Override base URL the CLI client uses to reach the local API |
| `AGENTDESK_TOKEN` | Optional Discord bot token forwarded to `dcserver` at startup. **Not** used by `discord-send*` CLI commands (those load configured bot tokens via the wizard, or require `--key`) and **not** used for `/api/*` auth — that comes from `server.auth_token` in `agentdesk.yaml` |
| `AGENTDESK_DCSERVER_LABEL` | Override launchd service label |
| `AGENTDESK_STATUS_INTERVAL_SECS` | Status polling interval (default: 5) |
| `AGENTDESK_TURN_TIMEOUT_SECS` | Turn watchdog timeout in seconds (default: 3600) |
| `AGENTDESK_GH_PATH` / `AGENTDESK_CODEX_PATH` / `AGENTDESK_GEMINI_PATH` | Override resolved provider/CLI binary paths |
| `RUST_LOG` | Standard tracing filter (default: `agentdesk=info`) |

## Customization

### Writing Custom Policies

Create a `.js` file in the `policies/` directory. It will be automatically loaded and hot-reloaded:

```javascript
var myPolicy = {
  name: "my-custom-policy",
  priority: 50,  // Lower = runs first (range: 1-999)

  onSessionStatusChange: function(payload) {
    // payload: { agentId, status, dispatchId, sessionKey }
    agentdesk.log.info("Agent " + payload.agentId + " is now " + payload.status);
  },

  onCardTransition: function(payload) {
    // payload: { card_id, from, to, status }
  },

  onCardTerminal: function(payload) {
    // payload: { card_id, status }
  },

  onDispatchCompleted: function(payload) {
    // payload: { dispatch_id, kanban_card_id, result }
  },

  onReviewEnter: function(payload) {
    // payload: { card_id, from }
  },

  onReviewVerdict: function(payload) {
    // payload: { card_id, dispatch_id, verdict, notes, feedback }
  },

  // Tiered ticks — choose the interval that fits your use case
  onTick30s: function() { /* fast: retries, notifications */ },
  onTick1min: function() { /* normal: timeouts, queue walk */ },
  onTick5min: function() { /* slow: reconciliation, cleanup */ }
};

agentdesk.registerPolicy(myPolicy);
```

### Bridge API (available in policy JS)

The QuickJS engine installs the `agentdesk.*` global with ~21 namespaces (sources in `src/engine/ops/`). The most commonly used surface:

```javascript
// Database (Postgres-backed; use ? placeholders, sqlx rebinds them)
agentdesk.db.query("SELECT * FROM agents WHERE id = ?", ["my-agent"])
agentdesk.db.execute("UPDATE kv_meta SET value = ? WHERE key = ?", ["true", "my_flag"])

// Key-value store (TTL-aware; replaces direct kv_meta writes for ephemeral state)
agentdesk.kv.set("pr:42", JSON.stringify(meta), 3600)   // ttlSeconds optional
agentdesk.kv.get("pr:42")                                // null if missing or expired
agentdesk.kv.delete("pr:42")

// Cards (typed queries with filters)
agentdesk.cards.list({ status: "ready", unassigned: true })
agentdesk.cards.get(cardId)
agentdesk.cards.assign(cardId, agentId)
agentdesk.cards.setPriority(cardId, "high")

// Kanban state transitions (fires hooks automatically)
agentdesk.kanban.setStatus(cardId, "in_progress")
agentdesk.kanban.setStatus(cardId, "done", true)        // force
agentdesk.kanban.getCard(cardId)
agentdesk.kanban.reopen(cardId, "ready")
agentdesk.kanban.setReviewStatus(cardId, "awaiting_dod", { awaiting_dod_at: "now" })

// Review state
agentdesk.reviewState.sync(cardId, "idle")
// Create-PR lifecycle helpers (#743 redesign)
agentdesk.reviewAutomation.handoffCreatePr(cardId, { /* dispatch payload */ })
agentdesk.reviewAutomation.recordPrCreateFailure(cardId, "gh exit 128", stampGen)
agentdesk.reviewAutomation.reseedPrTracking(cardId)

// Dispatch
agentdesk.dispatch.create(cardId, agentId, "implementation", "Task title")
agentdesk.dispatch.markFailed(dispatchId, "Stale dispatch auto-failed after 24h")

// Pipeline
agentdesk.pipeline.resolveForCard(cardId)
agentdesk.pipeline.getConfig()
agentdesk.pipeline.kickoffState(config)
agentdesk.pipeline.nextGatedTarget(currentStatus, config)
agentdesk.pipeline.isTerminal(status, config)
agentdesk.pipeline.terminalState(config)

// Auto-queue + phase gate
agentdesk.autoQueue.activate("run-123")                   // or activate({ run_id, repo, agent_id, ... })
agentdesk.autoQueue.updateEntryStatus(entryId, "active", "policy")
agentdesk.autoQueue.pauseRun(runId, "manual-hold")
agentdesk.autoQueue.resumeRun(runId, "policy-resume")
agentdesk.autoQueue.completeRun(runId, "policy", { reason: "all entries done" })
agentdesk.autoQueue.savePhaseGateState(runId, phase, state)
agentdesk.queue.status()                                  // run/slot snapshot used by dashboards

// Agents
agentdesk.agents.get(agentId)

// Configuration (live runtime keys + yaml-backed config)
agentdesk.config.get("review_max_rounds")

// Messaging — third arg picks the bot:
//   "announce" → trigger-bearing card (DISPATCH, PM, escalation routing)
//   "notify"   → informational only (does NOT trigger an agent turn, post-#1448)
agentdesk.message.queue("channel:123456", "Hello", "announce", "system")
agentdesk.message.queue("channel:123456", "📋 새 이슈 #42", "notify", "issue-sync")

// HTTP — loopback only, used to call the local AgentDesk API from policy JS
agentdesk.http.post(
  "http://127.0.0.1:8791/api/sessions/" + sessionKey + "/force-kill",
  { retry: false, reason: "idle 30분 초과 — 자동 정리" }
)

// External commands (allow-list: gh, git, tmux, etc.)
agentdesk.exec("gh", ["issue", "close", "42", "--repo", "owner/repo"])
agentdesk.exec("git", ["-C", repoDir, "log", "--oneline", "-5"])

// Session control
agentdesk.session.sendCommand(sessionKey, "/compact")
agentdesk.session.kill(sessionKey)

// Inflight turn tracking
agentdesk.inflight.list()
agentdesk.inflight.remove(provider, channelId)

// Quality / friction reporting
agentdesk.quality.emit({
  event_type: "api_friction",
  card_id: cardId,
  payload: { note: "duplicate dispatch on retry" }
})

// Runtime helpers (signals, retrospectives, inventory refresh)
agentdesk.runtime.emitSignal("escalation_routed", { card_id })
agentdesk.runtime.recordCardRetrospective(cardId, "done")
agentdesk.runtime.refreshInventoryDocs(worktreePath)

// CI recovery primitives
agentdesk.ciRecovery.setBlockedReason(cardId, "ci_failure:lint")
agentdesk.ciRecovery.getCardStatus(cardId)
agentdesk.ciRecovery.getReworkCardInfo(cardId)
agentdesk.ciRecovery.listWaitingForCi()

// DM reply hooks (used by escalation/triage flows)
agentdesk.dmReply.register(sourceAgent, userId, context, ttlSeconds)
agentdesk.dmReply.pending(userId)
agentdesk.dmReply.consume(userId)

// Logging
agentdesk.log.info("message")
agentdesk.log.warn("message")
agentdesk.log.error("message")
```

### Custom Agent Templates

During onboarding, you can create custom agents with:
- **Name** — Display name for the agent
- **Description** — One-line purpose description
- **System Prompt** — Full behavioral instructions (can be AI-generated)

Each agent maps to a Discord channel where it receives and responds to tasks.

## CLI Reference

```
# Server
agentdesk dcserver                               # Start Discord control plane
agentdesk init                                   # Interactive setup wizard
agentdesk reconfigure                            # Re-run setup (preserves data)
agentdesk restart-dcserver                       # Graceful restart with crash context
agentdesk doctor [--json] [--profile quick|deep|security]
agentdesk doctor --fix --allow-restart           # Explicit service restart repair
agentdesk doctor --fix --repair-sqlite-cache     # Explicit legacy SQLite cache repair
agentdesk emit-launchd-plist --flavor release|dev [--output <PATH>]

# Discord messaging
agentdesk discord-sendfile <PATH> --channel <ID> --key <HASH>
agentdesk discord-sendmessage --channel <ID> --message <TEXT>
agentdesk discord-senddm --user <ID> --message <TEXT>
agentdesk send --target channel:<ID> --content <TEXT> [--bot announce|notify|<provider>]
agentdesk discord read <CHANNEL_ID> [--limit <N>] [--before <ID>] [--after <ID>]
agentdesk monitoring start --channel <ID> --key <KEY> --description <TEXT>
agentdesk monitoring stop --channel <ID> --key <KEY>

# Review / docs / sessions
agentdesk review-verdict --dispatch <ID> --verdict pass|improve|rework|reject|approved
agentdesk review-decision --card <CARD_ID> --decision approve|rework|escalate|accept|dispute|dismiss|requeue|resume
agentdesk docs [CATEGORY] [--flat]
agentdesk force-kill --session-key <KEY> [--retry]

# Kanban / dispatch / auto-queue
agentdesk cards [--status <STATUS>]              # List kanban cards
agentdesk card create --from-issue <NUMBER> [--status ready] [--agent <ID>] [--repo <OWNER/REPO>]
agentdesk card status <CARD_ID|ISSUE_NUMBER> [--repo <OWNER/REPO>]
agentdesk dispatch <ISSUE_GROUPS...> [--agent <ID>] [--concurrent <N>] [--no-activate]
agentdesk dispatch list
agentdesk dispatch retry <CARD_ID>
agentdesk dispatch redispatch <CARD_ID>
agentdesk resume <CARD_ID> [--force] [--reason <TEXT>]
agentdesk advance <ISSUE_NUMBER>                 # Promote card to review
agentdesk queue                                  # Auto-queue status with thread links
agentdesk auto-queue activate [--run <ID>] [--agent <ID>] [--repo <OWNER/REPO>] [--active-only]
agentdesk auto-queue add <CARD_ID> [--run <ID>] [--priority <N>] [--phase <N>] [--thread-group <N>] [--agent <ID>]
agentdesk auto-queue config --max-concurrent <N> [--run <ID>] [--repo <OWNER/REPO>] [--agent <ID>]

# Git / runtime / migrations
agentdesk github-sync [--repo <OWNER/REPO>]
agentdesk cherry-merge <BRANCH> [--close-issue]
agentdesk status                                 # Runtime health summary
agentdesk config get                             # Read runtime config
agentdesk config set '<JSON>'                    # Set runtime config
agentdesk config audit [--dry-run]               # Reconcile yaml/DB drift
agentdesk agents                                 # List agents
agentdesk diag <AGENT_ID|CHANNEL_ID> [--json]    # Turn/session diagnostics
agentdesk terminations [--card-id <ID>] [--dispatch-id <ID>] [--session <KEY>] [--limit <N>]
agentdesk api GET /api/health                    # Public safe health summary
agentdesk api GET /api/health/detail             # Authenticated/local detailed health
agentdesk deploy                                 # Build workspace + promote to release
agentdesk migrate openclaw <ARGS>                # Import OpenClaw durable state
# `migrate postgres-cutover` is retired (production cutover landed 2026-04-19); the
# subcommand is parsed but returns an error so help/scripts don't break. Do not run it.
agentdesk provider-cli <SUBCOMMAND>              # Provider CLI safe-migration ops (status/plan/upgrade/canary/promote/rollback/cleanup/run/resume/smoke)

# Process wrappers (internal — invoked by tmux session lifecycle)
agentdesk tmux-wrapper                           # Claude session wrapper
agentdesk codex-tmux-wrapper                     # Codex session wrapper
agentdesk qwen-tmux-wrapper                      # Qwen session wrapper
agentdesk reset-tmux                             # Kill AgentDesk-* tmux sessions and clean temp files
```

## API Overview

AgentDesk exposes 150+ REST API endpoints. Key groups:

| Group | Endpoints | Description |
|-------|-----------|-------------|
| `/api/agents` | CRUD + signal, skills, timeline | Agent management |
| `/api/kanban-cards` | CRUD + assign, `/transition`, `/retry`, `/redispatch`, `/rereview`, `/reopen`, batch actions | Work item management. `/transition`, `/retry`, `/redispatch`, and `/queue/generate` are **single-call complete** — do not chain them (#1442). Response fields differ per endpoint: `/retry` and `/redispatch` return `new_dispatch_id` + `next_action`; `/transition` returns `cancelled_dispatch_ids`, `created_dispatch_id`, and `next_action_hint` (and requires `force=true` when an active dispatch exists, #1444); `/queue/generate` returns `run`, `entries`, and the `skipped_due_to_active_dispatch` / `skipped_due_to_dependency` / `skipped_due_to_filter` arrays. |
| `/api/dispatches` | CRUD + cancel | Task assignment tracking |
| `/api/queue` | Generate, dispatch-next, reorder, status, slots | Batch-phased work queuing |
| `/api/sessions` | List, update, cleanup | Agent runtime sessions |
| `/api/round-table-meetings` | Start, transcript, issues | Multi-agent meetings |
| `/api/offices` | CRUD + agent assignment, ordering | Virtual office management |
| `/api/departments` | CRUD + ordering | Department management |
| `/api/pipeline` | Stages, config, graphs, card history | Pipeline configuration |
| `/api/settings` | Company + config/runtime/escalation subroutes | Platform configuration surfaces |
| `/api/github` | Repo sync, dashboard views, issue actions | GitHub integration |
| `/api/discord` | `/send`, `/send-to-agent`, `/send-dm`, channel messages, bindings, DM reply hooks. Legacy top-level `/api/send*` aliases were removed. | Discord access layer |
| `/api/cluster` | Nodes, routing diagnostics, resource locks, task-dispatch claims, issue specs, phase evidence | Multinode coordination |
| `/api/health` | Public safe health summary | Service status |
| `/api/health/detail` | Authenticated/local detailed diagnostics | Provider/runtime diagnostics |
| `/api/onboarding` | Status, validate, complete | Setup wizard backend |
| `/api/docs` | Endpoint discovery + category drill-down + decision-tree guides such as `/api/docs/card-lifecycle-ops` (#1443) | Self-documenting API |

Full API documentation is available at `/api/docs` when the server is running, with category-based grouping and parameter details.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   AgentDesk Binary (Rust)                │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────┐  │
│  │ Discord  │  │   Turn   │  │   HTTP   │  │ GitHub │  │
│  │ Gateway  │  │Orchestr. │  │ Server   │  │  Sync  │  │
│  │(serenity)│  │  (tmux)  │  │  (axum)  │  │  (gh)  │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └───┬────┘  │
│       │              │             │             │       │
│  ┌────┴──────────────┴─────────────┴─────────────┴────┐  │
│  │           Supervised Worker Registry                 │  │
│  └────┬──────────────┬─────────────┬─────────────┬────┘  │
│       │              │             │             │       │
│  ┌────┴─────┐  ┌─────┴────┐  ┌────┴─────┐  ┌───┴────┐  │
│  │ Dispatch │  │  Policy   │  │ Database │  │   WS   │  │
│  │ Service  │  │  Engine   │  │(Postgres)│  │Broadcast│  │
│  │ +outbox  │  │(QuickJS)  │  │          │  │        │  │
│  └──────────┘  └──────────┘  └──────────┘  └────────┘  │
│                     │                                    │
│              ┌──────┴──────┐                             │
│              │  policies/  │  ← JS files (hot-reload)    │
│              │  *.js       │                             │
│              └─────────────┘                             │
└─────────────────────────────────────────────────────────┘
         │
    ┌────┴────┐
    │ React   │  ← Dashboard (static build)
    │Dashboard│
    └─────────┘
```

### Design Principles
1. **Single Binary** — One Rust binary; PostgreSQL is the only required external runtime dependency
2. **Single Process Per Node** — Each node is one `agentdesk dcserver` process; multinode coordination is persisted in PostgreSQL rather than a sidecar coordinator
3. **Single Database Control Plane** — PostgreSQL holds all live state (agents, cards, dispatches, sessions, kv_meta, worker nodes, resource locks, phase evidence). The legacy SQLite path is gated behind a `legacy-sqlite-tests` cargo feature and only used by tests after the #868 / #1239 cutover
4. **Hot-Reloadable Policies** — Business logic in JS, editable without rebuild
5. **Self-Contained** — No Node.js, Python, or other runtimes needed at deploy time
6. **Pipeline-Driven** — State machines defined in YAML, not hardcoded in Rust or JS

## Limitations

- **Installer is macOS-focused** — The `curl | bash` installer and launchd integration target macOS. Linux systemd and Windows service support exist in `--init`, but native runtime setup is still a manual path.
- **Node-local provider execution** — Agents run on registered AgentDesk nodes. Each node still needs its own authenticated provider CLIs and local MCP/tooling for the capabilities it advertises.
- **Discord-dependent** — Agent communication requires Discord. There is no built-in alternative messaging backend.
- **tmux optional** — Agent sessions use tmux by default, but a backend process mode is available that does not require tmux. That fallback keeps heartbeats, not tmux-style watcher reattachment after restart.
- **Single PostgreSQL control plane** — Multiple AgentDesk nodes can share one PostgreSQL instance. Multi-PostgreSQL HA, sharding, and split-brain recovery are outside the current design.
- **Provider CLI required** — AI providers (Claude Code, Codex) must be installed and authenticated on the host machine for agents to function.
- **GitHub integration via CLI** — GitHub features require the `gh` CLI tool to be installed and authenticated.

## Project Structure

```
AgentDesk/
├── src/                        # Rust source
│   ├── main.rs                 # Entry point + CLI dispatch
│   ├── config.rs               # YAML configuration
│   ├── kanban.rs               # Kanban state machine + transition hooks
│   ├── pipeline.rs             # Pipeline config resolution
│   ├── cli/                    # CLI commands (dcserver, init, client)
│   ├── db/                     # SQLite schema, migrations, typed queries
│   ├── dispatch/               # Dispatch creation, outbox, delivery
│   ├── engine/                 # QuickJS policy engine + bridge ops
│   │   └── ops/                # ~21 bridge namespaces (cards, kanban, dispatch, kv, http, runtime, quality, ...)
│   ├── github/                 # Issue sync, auto-triage, DoD mirroring
│   ├── server/                 # Axum HTTP server + WebSocket
│   │   └── routes/             # 150+ API route handlers
│   └── services/               # Provider integrations + platform abstractions
│       ├── discord/            # Serenity/Poise gateway, router, recovery
│       │   └── watchers/lifecycle.rs   # Tmux watcher lifecycle (extracted from tmux.rs in #1435)
│       ├── dispatches.rs       # Dispatch service layer
│       ├── turn_orchestrator.rs # Turn lifecycle management
│       ├── issue_announcements.rs # Notify-bot routing for issue cards (#1448 follow-up)
│       ├── retrospectives.rs   # Terminal card retrospectives
│       └── api_friction.rs     # API friction reporting
├── policies/                   # JavaScript policy files (repo canonical; release mirror hot-reload)
├── dashboard/                  # React 19 + TypeScript + Vite + Tailwind
├── docs/                       # ADRs and design documents
└── scripts/                    # Install, build, deploy, verify scripts
```

## Acknowledgments

AgentDesk incorporates and builds upon code from the following projects:

- **[cokacdir](https://github.com/itismyfield/cokacdir)** (MIT License) — A Rust-based Telegram relay for Claude Code sessions. AgentDesk was originally forked from cokacdir's Telegram relay foundation, then extended with Discord support, session management, tmux lifecycle, and turn bridge functionality.
- **[claw-empire](https://github.com/GreenSheep01201/claw-empire)** (Apache 2.0 License) — Sprite images used in the office view dashboard were sourced from claw-empire.

## License

AgentDesk is licensed under the [MIT License](LICENSE).

You are free to use, modify, and distribute this software, including for commercial purposes. **Attribution is required** — you must retain the copyright notice and include the [NOTICE](NOTICE) file in any distribution or derivative work.

See [LICENSE](LICENSE) and [NOTICE](NOTICE) for full details.
