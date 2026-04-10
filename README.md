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
Cards flow through a managed lifecycle with automated transitions:

```
backlog → ready → requested → in_progress → review → done
                                    ↓            ↓
                                 blocked    suggestion_pending
```

- **Dispatch-driven transitions** — Cards only advance via task dispatches, not manual status changes
- **Counter-model review** — Claude reviews Codex's work and vice versa, with configurable max rounds
- **Auto-queue** — Automatic card progression with priority scoring
- **GitHub sync** — Bidirectional issue synchronization with DoD checklist mirroring
- **Audit logging** — Every state transition is recorded

### Policy Engine
Business logic lives in JavaScript files under `policies/`, hot-reloaded without restarting:

| Policy | Purpose |
|--------|---------|
| `kanban-rules.js` | Core lifecycle: session status → card transitions, PM decision gates |
| `review-automation.js` | Counter-model review dispatch, verdict processing |
| `auto-queue.js` | Automatic card queuing on terminal state |
| `timeouts.js` | Stale card detection (45min requested, 100min in-progress) |
| `triage-rules.js` | GitHub issue auto-classification |
| `pipeline.js` | Multi-stage workflow progression |

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
- **Agent Manager** — Agent configuration, skills, timeline, sessions
- **Analytics** — Streaks, achievements, activity heatmaps, audit logs
- **Meeting Minutes** — Round-table meeting transcripts with issue extraction
- **Settings** — Runtime configuration, onboarding re-run, policy management

### Round-Table Meetings
Coordinate multi-agent discussions with structured rounds, automatic transcript recording, and post-meeting issue extraction to GitHub.

### OpenClaw Migration
Import OpenClaw agent/runtime state into AgentDesk with `agentdesk migrate openclaw`. Generated role prompts point at the imported AgentDesk memory/workspace paths so migrated agents use runtime-managed data instead of raw OpenClaw source paths. See [`docs/openclaw-migrate.md`](docs/openclaw-migrate.md) for dry-run, apply, resume, and audit output details, and [`docs/openclaw-migration.md`](docs/openclaw-migration.md) for the contract-level migration spec and current implementation notes.

## Configuration

### agentdesk.yaml

The main configuration file at `~/.adk/release/agentdesk.yaml`:

```yaml
server:
  port: 8791              # HTTP server port
  host: "0.0.0.0"         # Bind address
  auth_token: "secret"    # Optional API authentication token

discord:
  bots:
    claude:
      token: "your-claude-bot-token"
    announce:
      token: "your-announce-bot-token"
    notify:
      token: "your-notify-bot-token"

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

Additional settings are stored in the database (`kv_meta` table) and exposed through four distinct surfaces:

| Surface | Storage | API | Notes |
|---------|---------|-----|-------|
| Company settings | `kv_meta['settings']` JSON | `GET/PUT /api/settings` | Full-replace payload; callers should send a merged object. Retired legacy keys are stripped server-side. |
| Runtime config | `kv_meta['runtime-config']` JSON | `GET/PUT /api/settings/runtime-config` | Live numeric tuning applied without restart. |
| Policy/config keys | Individual `kv_meta` rows | `GET/PATCH /api/settings/config` | Whitelisted review, timeout, context, Discord, and merge-automation keys. |
| Onboarding/secrets | Dedicated onboarding keys and flows | `/api/onboarding/*` | Tokens and setup secrets stay outside the general settings form. |

The dashboard Settings page only edits the surfaces that truthfully map to those APIs.

### Whitelisted policy/config keys

Key individual `kv_meta` entries exposed via `/api/settings/config`:

| Key | Default | Description |
|-----|---------|-------------|
| `review_enabled` | `true` | Enable counter-model review |
| `counter_model_review_enabled` | `false` | Enable cross-model review |
| `max_review_rounds` | `3` | Maximum review rounds before escalation |
| `pm_decision_gate_enabled` | `false` | Require PM decision gate before next transition |
| `merge_automation_enabled` | `false` | Enable automated PR merge flow |
| `merge_strategy` | `squash` | Auto-merge strategy (`squash`, `merge`, `rebase`) |
| `merge_allowed_authors` | — | Comma-separated authors allowed for auto-merge |
| `requested_timeout_min` | `45` | Timeout for cards in requested state |
| `in_progress_stale_min` | `120` | Timeout for stale in-progress cards |
| `context_compact_percent` | `60` | Global context compaction threshold |
| `kanban_manager_channel_id` | — | Discord channel for PM notifications |

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `AGENTDESK_ROOT_DIR` | Override runtime directory (default: `~/.adk/release`) |
| `AGENTDESK_CONFIG` | Override config file path |
| `AGENTDESK_SERVER_PORT` | Override HTTP server port (default: 8791) |
| `AGENTDESK_DCSERVER_LABEL` | Override launchd service label |

## Customization

### Writing Custom Policies

Create a `.js` file in the `policies/` directory. It will be automatically loaded and hot-reloaded:

```javascript
export default {
  name: "my-custom-policy",
  priority: 50,  // Lower = runs first (range: 1-999)

  // Fires when an agent session changes status
  onSessionStatusChange: function(payload) {
    // payload: { agentId, status, dispatchId, sessionKey }
    agentdesk.log.info("Agent " + payload.agentId + " is now " + payload.status);
  },

  // Fires when a kanban card transitions between states
  onCardTransition: function(payload) {
    // payload: { cardId, from, to, reason }
  },

  // Fires when a card reaches a terminal state (done, blocked, failed)
  onCardTerminal: function(payload) {
    // payload: { cardId, status }
  },

  // Fires when a dispatch completes
  onDispatchCompleted: function(payload) {
    // payload: { dispatchId, result }
  },

  // Fires when a card enters review
  onReviewEnter: function(payload) {
    // payload: { card_id, from }
  },

  // Fires when a review verdict is submitted
  onReviewVerdict: function(payload) {
    // payload: { card_id, dispatch_id, verdict, notes, feedback }
  },

  // Fires every ~60 seconds (for timeouts, cleanup, etc.)
  onTick: function() {
    // Periodic maintenance
  }
};
```

### Bridge API (available in policy JS)

```javascript
// Database
agentdesk.db.query("SELECT * FROM agents WHERE status = ?", ["idle"])
agentdesk.db.execute("UPDATE kv_meta SET value = ? WHERE key = ?", ["true", "my_flag"])

// Kanban (use instead of direct SQL for status changes)
agentdesk.kanban.setStatus(cardId, "in_progress")
agentdesk.kanban.getCard(cardId)

// Dispatch
agentdesk.dispatch.create(cardId, agentId, "implementation", "Task title")

// Configuration
agentdesk.config.get("review_max_rounds")

// HTTP (localhost only)
agentdesk.http.post("/api/some-endpoint", { key: "value" })

// External commands (gh and git only)
agentdesk.exec("gh", ["issue", "close", "42", "--repo", "owner/repo"])

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
agentdesk dcserver                              # Start Discord control plane
agentdesk init                                  # Interactive setup wizard
agentdesk reconfigure                           # Re-run setup (preserves data)
agentdesk restart-dcserver                      # Graceful restart
agentdesk discord-sendfile <PATH> --channel <ID> --key <HASH>
agentdesk discord-sendmessage --channel <ID> --message <TEXT>
agentdesk discord-senddm --user <ID> --message <TEXT>
agentdesk status                                # Runtime health summary
agentdesk api GET /api/health                   # Direct API call
```

## API Overview

AgentDesk exposes 50+ REST API endpoints. Key groups:

| Group | Endpoints | Description |
|-------|-----------|-------------|
| `/api/agents` | CRUD + signal, skills, timeline | Agent management |
| `/api/kanban-cards` | CRUD + assign, retry, bulk actions | Work item management |
| `/api/dispatches` | CRUD | Task assignment tracking |
| `/api/sessions` | List, update, cleanup | Agent runtime sessions |
| `/api/auto-queue` | Generate, activate, reorder | Automatic work queuing |
| `/api/round-table-meetings` | Start, transcript, issues | Multi-agent meetings |
| `/api/offices` | CRUD + agent assignment | Virtual office management |
| `/api/settings` | Company settings + config/runtime subroutes | Platform configuration surfaces |
| `/api/health` | Health check | Service status |
| `/api/onboarding` | Status, validate, complete | Setup wizard backend |

Full API documentation is available at `/api/docs` when the server is running.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   AgentDesk Binary (Rust)                │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────┐  │
│  │ Discord  │  │ Session  │  │   HTTP   │  │ GitHub │  │
│  │ Gateway  │  │ Manager  │  │ Server   │  │  Sync  │  │
│  │(serenity)│  │  (tmux)  │  │  (axum)  │  │  (gh)  │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └───┬────┘  │
│       │              │             │             │       │
│  ┌────┴──────────────┴─────────────┴─────────────┴────┐  │
│  │               Core Event Bus (channels)             │  │
│  └────┬──────────────┬─────────────┬─────────────┬────┘  │
│       │              │             │             │       │
│  ┌────┴─────┐  ┌─────┴────┐  ┌────┴─────┐  ┌───┴────┐  │
│  │ Dispatch │  │  Policy   │  │ Database │  │   WS   │  │
│  │ Engine   │  │  Engine   │  │ (SQLite) │  │Broadcast│  │
│  │          │  │(QuickJS)  │  │          │  │        │  │
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

## Limitations

- **Installer is macOS-focused** — The `curl | bash` installer and launchd integration target macOS. Linux systemd and Windows service support exist in `--init` but are not yet covered by the one-click installer.
- **Local execution** — Agents run on the same machine as AgentDesk. Distributed agent execution is not supported.
- **Discord-dependent** — Agent communication requires Discord. There is no built-in alternative messaging backend.
- **tmux optional** — Agent sessions use tmux by default, but a backend process mode is available that does not require tmux.
- **Single SQLite database** — Not designed for multi-instance or clustered deployment.
- **Provider CLI required** — AI providers (Claude Code, Codex) must be installed and authenticated on the host machine for agents to function.
- **GitHub integration via CLI** — GitHub features require the `gh` CLI tool to be installed and authenticated.

## Project Structure

```
AgentDesk/
├── src/                    # Rust source
│   ├── main.rs             # Entry point
│   ├── config.rs           # YAML configuration
│   ├── cli/                # CLI commands (init, dcserver)
│   ├── db/                 # SQLite schema & migrations
│   ├── engine/             # QuickJS policy engine
│   ├── server/routes/      # 50+ HTTP API handlers
│   ├── services/discord/   # Discord gateway & bot management
│   └── services/           # Session management, providers
├── policies/               # JavaScript policy files (hot-reload)
├── dashboard/              # React frontend (Vite + TypeScript)
├── migrations/             # SQL schema migrations
└── scripts/                # Install, build, deploy scripts
```

## Acknowledgments

AgentDesk incorporates and builds upon code from the following projects:

- **[cokacdir](https://github.com/itismyfield/cokacdir)** (MIT License) — A Rust-based Telegram relay for Claude Code sessions. AgentDesk was originally forked from cokacdir's Telegram relay foundation, then extended with Discord support, session management, tmux lifecycle, and turn bridge functionality.
- **[claw-empire](https://github.com/GreenSheep01201/claw-empire)** (Apache 2.0 License) — Sprite images used in the office view dashboard were sourced from claw-empire.

## License

AgentDesk is licensed under the [MIT License](LICENSE).

You are free to use, modify, and distribute this software, including for commercial purposes. **Attribution is required** — you must retain the copyright notice and include the [NOTICE](NOTICE) file in any distribution or derivative work.

See [LICENSE](LICENSE) and [NOTICE](NOTICE) for full details.
