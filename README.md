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
2. Run `agentdesk --init` (`agentdesk.exe --init` on Windows) to create the runtime under `~/.adk/release` or `%USERPROFILE%\.adk\release`.
3. Start `agentdesk dcserver` directly, or register the generated service path with `systemd --user` on Linux or `nssm` / `sc.exe` on Windows.
4. Verify the host runtime with `agentdesk doctor` and use `agentdesk doctor --fix` for service/runtime repairs.

When tmux is unavailable, provider turns automatically fall back to `ProcessBackend` instead of tmux sessions. That path still posts ADK session heartbeats every 30 seconds, but live child processes cannot be reattached after a `dcserver` restart. After restart, AgentDesk starts a fresh backend process on the next turn and only restores provider-native session IDs when the underlying CLI supports resume.

### Build from Source

```bash
git clone https://github.com/itismyfield/AgentDesk.git
cd AgentDesk
cargo build --release

# Verify the dashboard with the same command CI uses (Node >=22)
./scripts/verify-dashboard.sh

# Initialize
./target/release/agentdesk --init
```

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
Create Discord bots in the [Discord Developer Portal](https://discord.com/developers/applications). You need at minimum:

| Bot | Role | Required Permissions |
|-----|------|---------------------|
| **Command Bot** | Runs AI agent sessions (Claude or Codex) | Send Messages, Read Message History, Manage Messages |
| **Communication Bot** | Agent-to-agent messaging + channel management | Administrator (simplest) or Manage Channels + Manage Roles |
| **Notification Bot** *(optional)* | System alerts (agents don't respond to this bot) | Send Messages |

**Important:** On the Bot tab, enable the **MESSAGE CONTENT** Privileged Gateway Intent. Without this, bots cannot read message content and will not function properly.

After entering and validating each bot token, the wizard generates OAuth2 invite links with the correct permissions pre-configured — just click to invite each bot to your Discord server.

### Step 2: Provider Verification
The wizard checks whether Claude Code or Codex CLI is installed and authenticated on your machine. If not, it provides installation and login instructions. Provider setup is **not required** to complete onboarding — you can configure it later.

### Step 3: Agent Selection
Choose from three built-in templates or create custom agents:

| Template | Agents | Use Case |
|----------|--------|----------|
| **Household** | Scheduler, Household, Cooking, Health, Shopping | Home automation and family management |
| **Startup** | PM, Developer, Designer, QA, Marketing | Small team software development |
| **Office** | Schedule Manager, Email, Document Writer, Researcher, Data Analyst | Business process automation |

Custom agents can be added with a name and description. The "AI Generate" button creates a system prompt using your configured provider CLI.

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
| `kanban-rules.js` | Core lifecycle: dispatch completion, PM decision gates, worktree auto-merge |
| `review-automation.js` | Counter-model review dispatch, verdict processing, PR tracking |
| `auto-queue.js` | Batch-phased card queuing, phase gate dispatch, slot management |
| `timeouts.js` | Stale card detection, deadlock detection/recovery, idle session cleanup |
| `triage-rules.js` | GitHub issue auto-classification and agent assignment |
| `pipeline.js` | Multi-stage workflow progression |
| `merge-automation.js` | PR auto-merge, worktree cleanup after merge |
| `deploy-pipeline.js` | E2E test dispatch and deploy stage advancement |

Policies are not checked into the repository — they are created at runtime and loaded from the configured `policies.dir` path.

### Tiered Tick System
Policies hook into a 3-tier periodic tick system running on a dedicated OS thread:

| Hook | Interval | Typical Use |
|------|----------|-------------|
| `onTick30s` | 30s | Retry recovery, notification delivery, deadlock detection |
| `onTick1min` | 1m | Stale card timeouts, auto-queue walk, CI recovery |
| `onTick5min` | 5m | Idle session cleanup, merge queue processing, reconciliation |
| `onTick` | 5m (legacy) | Backward compatibility for older policies |

### Multi-Bot Architecture
Each bot has a distinct role to prevent message conflicts:

- **Command bots** trigger AI sessions when they receive messages
- **Communication bot** handles agent-to-agent messaging and channel management
- **Notification bot** sends alerts without triggering agent responses

Dual-provider mode lets you run both Claude and Codex simultaneously, each through its own command bot.

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
Agent turn lifecycle is managed by a dedicated orchestration layer (extracted from the Discord adapter):
- **Heartbeat monitoring** — Tracks agent session liveness
- **Completion guard** — Validates dispatch results before card transitions
- **Deadlock detection** — Identifies stuck sessions and auto-recovers with configurable thresholds
- **Inflight tracking** — Per-provider inflight files for concurrent session management

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
    command:
      token: "your-command-bot-token"
    announce:
      token: "your-announce-bot-token"
    notify:
      token: "your-notify-bot-token"
    codex:
      token: "your-codex-bot-token"
      provider: codex
  guild_id: "your-guild-id"

agents:
  - id: my-agent
    name: My Agent
    provider: claude          # or codex
    channels:
      claude:
        id: "channel-id"
        name: agent-cc
        workspace: ~/.adk/release/workspaces/my-project
      codex:
        id: "channel-id"
        name: agent-cdx

github:
  repos:
    - "owner/repo-name"
  sync_interval_minutes: 10

memory:
  backend: auto
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

kanban:
  timeout_requested_minutes: 45
  timeout_in_progress_minutes: 100
  max_review_rounds: 3
```

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
| `AGENTDESK_SERVER_PORT` | Override HTTP server port (default: 8791) |
| `AGENTDESK_DCSERVER_LABEL` | Override launchd service label |
| `AGENTDESK_STATUS_INTERVAL_SECS` | Status polling interval (default: 5) |
| `AGENTDESK_TURN_TIMEOUT_SECS` | Turn watchdog timeout (default: 3600) |
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

```javascript
// Database
agentdesk.db.query("SELECT * FROM agents WHERE id = ?", ["my-agent"])
agentdesk.db.execute("UPDATE kv_meta SET value = ? WHERE key = ?", ["true", "my_flag"])

// Cards (typed queries with filters)
agentdesk.cards.list({ status: "ready", unassigned: true })
agentdesk.cards.get(cardId)
agentdesk.cards.assign(cardId, agentId)
agentdesk.cards.setPriority(cardId, "high")

// Kanban state transitions (fires hooks automatically)
agentdesk.kanban.setStatus(cardId, "in_progress")
agentdesk.kanban.setStatus(cardId, "done", true)  // force
agentdesk.kanban.getCard(cardId)
agentdesk.kanban.reopen(cardId, "ready")

// Review state
agentdesk.reviewState.sync(cardId, "idle")
agentdesk.kanban.setReviewStatus(cardId, "awaiting_dod", { awaiting_dod_at: "now" })

// Dispatch
agentdesk.dispatch.create(cardId, agentId, "implementation", "Task title")

// Pipeline
agentdesk.pipeline.resolveForCard(cardId)
agentdesk.pipeline.kickoffState(config)
agentdesk.pipeline.isTerminal(status, config)
agentdesk.pipeline.terminalState(config)

// Agents
agentdesk.agents.get(agentId)

// Configuration
agentdesk.config.get("review_max_rounds")

// Messaging
agentdesk.message.queue("channel:123456", "Hello", "announce", "system")

// External commands (gh, git, tmux only)
agentdesk.exec("gh", ["issue", "close", "42", "--repo", "owner/repo"])
agentdesk.exec("git", ["-C", repoDir, "log", "--oneline", "-5"])

// Session control
agentdesk.session.sendCommand(sessionKey, "/compact")
agentdesk.session.kill(sessionKey)

// Inflight turn tracking
agentdesk.inflight.list()
agentdesk.inflight.remove(provider, channelId)

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
agentdesk restart-dcserver                        # Graceful restart with crash context
agentdesk doctor                                 # System diagnostics

# Discord messaging
agentdesk discord-sendfile <PATH> --channel <ID> --key <HASH>
agentdesk discord-sendmessage --channel <ID> --message <TEXT>
agentdesk discord-senddm --user <ID> --message <TEXT>
agentdesk send --target channel:<ID> --content <TEXT>
agentdesk discord read <CHANNEL_ID> [--limit <N>]

# Review / docs / sessions
agentdesk review-verdict --dispatch <ID> --verdict pass|improve|rework|reject|approved
agentdesk review-decision --card <CARD_ID> --decision approve|rework|escalate
agentdesk docs [CATEGORY]
agentdesk force-kill --session-key <KEY>

# Kanban / dispatch / auto-queue
agentdesk cards                                  # List kanban cards
agentdesk card create --from-issue <NUMBER> [--status ready] [--agent <ID>]
agentdesk card status <CARD_ID|ISSUE_NUMBER>
agentdesk dispatch <ISSUE_GROUPS...>             # Dispatch issue groups
agentdesk dispatch list
agentdesk dispatch retry <CARD_ID>
agentdesk dispatch redispatch <CARD_ID>
agentdesk resume                                 # Resume stuck cards
agentdesk advance                                # Promote card to review
agentdesk queue                                  # Auto-queue status
agentdesk auto-queue activate [--run <ID>] [--agent <ID>]
agentdesk auto-queue add <CARD_ID> [--run <ID>] [--priority <N>] [--phase <N>]
agentdesk auto-queue config --max-concurrent <N> [--run <ID>]

# Git / runtime
agentdesk github-sync [--repo <OWNER/REPO>]
agentdesk cherry-merge <BRANCH> [--close-issue]
agentdesk status                                 # Runtime health summary
agentdesk config get                             # Read runtime config
agentdesk config set '<JSON>'                    # Set runtime config
agentdesk config audit [--dry-run]               # Reconcile yaml/DB drift
agentdesk agents                                 # List agents
agentdesk terminations                           # Session termination events
agentdesk api GET /api/health                    # Direct API call

# Process wrappers (internal)
agentdesk tmux-wrapper                           # Claude session wrapper
agentdesk codex-tmux-wrapper                     # Codex session wrapper
```

## API Overview

AgentDesk exposes 150+ REST API endpoints. Key groups:

| Group | Endpoints | Description |
|-------|-----------|-------------|
| `/api/agents` | CRUD + signal, skills, timeline | Agent management |
| `/api/kanban-cards` | CRUD + assign, retry, redispatch, bulk actions | Work item management |
| `/api/dispatches` | CRUD + cancel | Task assignment tracking |
| `/api/auto-queue` | Generate, activate, reorder, status, slots | Batch-phased work queuing |
| `/api/sessions` | List, update, cleanup | Agent runtime sessions |
| `/api/round-table-meetings` | Start, transcript, issues | Multi-agent meetings |
| `/api/offices` | CRUD + agent assignment, ordering | Virtual office management |
| `/api/departments` | CRUD + ordering | Department management |
| `/api/pipeline` | Stages, config, graphs, card history | Pipeline configuration |
| `/api/settings` | Company + config/runtime/escalation subroutes | Platform configuration surfaces |
| `/api/github` | Repo sync, dashboard views, issue actions | GitHub integration |
| `/api/discord` | Channel messages, bindings, DM reply hooks | Discord access layer |
| `/api/health` | Health check + detailed metrics | Service status |
| `/api/onboarding` | Status, validate, complete | Setup wizard backend |
| `/api/docs` | Endpoint discovery + category drill-down | Self-documenting API |

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
│  │ Service  │  │  Engine   │  │ (SQLite) │  │Broadcast│  │
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
1. **Single Binary** — One Rust binary, no external runtime dependencies
2. **Single Process** — No inter-process communication, minimal failure points
3. **Single Database** — SQLite for all state (agents, cards, dispatches, sessions)
4. **Hot-Reloadable Policies** — Business logic in JS, editable without rebuild
5. **Self-Contained** — No Node.js, Python, or other runtimes needed at deploy time
6. **Pipeline-Driven** — State machines defined in YAML, not hardcoded in Rust or JS

## Limitations

- **Installer is macOS-focused** — The `curl | bash` installer and launchd integration target macOS. Linux systemd and Windows service support exist in `--init`, but native runtime setup is still a manual path.
- **Local execution** — Agents run on the same machine as AgentDesk. Distributed agent execution is not supported.
- **Discord-dependent** — Agent communication requires Discord. There is no built-in alternative messaging backend.
- **tmux optional** — Agent sessions use tmux by default, but a backend process mode is available that does not require tmux. That fallback keeps heartbeats, not tmux-style watcher reattachment after restart.
- **Single SQLite database** — Not designed for multi-instance or clustered deployment.
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
│   │   └── ops/                # 15 bridge namespaces (cards, kanban, dispatch, ...)
│   ├── github/                 # Issue sync, auto-triage, DoD mirroring
│   ├── server/                 # Axum HTTP server + WebSocket
│   │   └── routes/             # 150+ API route handlers
│   └── services/               # Provider integrations + platform abstractions
│       ├── discord/            # Serenity/Poise gateway, router, recovery
│       ├── dispatches.rs       # Dispatch service layer
│       ├── turn_orchestrator.rs # Turn lifecycle management
│       ├── retrospectives.rs   # Terminal card retrospectives
│       └── api_friction.rs     # API friction reporting
├── policies/                   # JavaScript policy files (hot-reload, runtime-only)
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
