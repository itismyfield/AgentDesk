# CLAUDE.md Load Hierarchy

Claude Code loads multiple `CLAUDE.md` files when it launches inside an agent workspace. This document pins down which files exist, which ones actually affect AgentDesk agents, and which order wins when the same rule shows up in more than one place.

Pair it with [`source-of-truth.md`](source-of-truth.md), which owns the canonical write paths for each file below.

## Layers (highest merge priority last)

Claude Code concatenates matching `CLAUDE.md` content into its instruction stream. Later layers do not overwrite earlier ones — they append, so the last file to speak on a topic effectively wins for the model. The ordering below reflects how Claude Code traverses the filesystem from global to workspace.

1. **Global user** — `~/.claude/CLAUDE.md`
   - Optional operator symlink into Obsidian, for example `~/ObsidianVault/RemoteVault/10_Claude/<host>/claude-home/CLAUDE.md`.
   - Loaded on every Claude Code session regardless of working directory.
   - Scope: personal Claude Code add-ons (Subagent Strategy, Sidecar, RTK include, etc.).
   - Edit: the symlink target. Do not replace the symlink with inline content.

2. **Repo root** — `<repo>/CLAUDE.md`
   - Loaded when Claude Code's cwd is the AgentDesk repo root (including campaign worktrees created from the repo).
   - In this repo the file is 12 lines and simply delegates to [`docs/source-of-truth.md`](source-of-truth.md). It deliberately does not re-encode operational paths.
   - Edit: this file, for repo-local agent instructions only. All operational path mappings belong in `source-of-truth.md`.

3. **Downstream workspace** — e.g. `~/CookingHeart/CLAUDE.md`
   - Only applies when Claude Code is launched from that workspace (i.e. not from the AgentDesk repo). Each workspace owns its own contract.
   - Edit: the file in the target workspace repo directly.

### Agent-specific layer (optional)

A workspace-local `CLAUDE.md` applies when it actually exists in the working directory hierarchy. AgentDesk has no loader for the archived per-agent CLAUDE/MEMORY directories in `config/memories/archive/`; they are not active instruction layers.

## ADK-affecting layers

For an agent running under AgentDesk the layers that actually affect behavior are, in order:

- Layer 1 (`~/.claude/CLAUDE.md`) — always.
- Layer 2 (`<repo>/CLAUDE.md`) — only when the agent is working inside the AgentDesk repo itself (i.e. campaign worktrees, release promotions). For agents working on CookingHeart or similar downstream repos, layer 2 is the workspace CLAUDE.md of that repo instead.

Layer 3 is out of AgentDesk's scope — it belongs to the downstream workspace's own contract.

## Deduplication rule

If the same instruction appears in more than one layer, keep it in the most-specific layer that needs it and remove it from the broader layers. Concretely:

- Agent-identity content (role name, responsibilities, tool preferences) belongs in the per-agent prompt (`~/.adk/release/config/agents/<role>.prompt.md`, optionally mirrored from `~/ObsidianVault/RemoteVault/adk-config/agents/<role>.prompt.md`) — not in any `CLAUDE.md`.
- Project-specific instructions live in the target workspace repository’s own `CLAUDE.md`.
- Repo-wide edit rules live in layer 2 (`<repo>/CLAUDE.md`).
- Personal Claude Code preferences live in layer 1 (`~/.claude/CLAUDE.md`).

When removing a duplicate, leave a one-line commit note pointing at the surviving canonical location rather than an inline comment in the file.

## Related

- [`source-of-truth.md`](source-of-truth.md) — canonical edit paths for all config, prompt, and memory surfaces, including global, repo, and downstream workspace CLAUDE.md files.
- `scripts/archive-config-backups.sh` — archives legacy config snapshots (`*.pre-*`, `*.bak`, `*.migrated`) into `~/.adk/release/config/.backups/YYYY-MM-DD/`. Run during config cleanup; never leave snapshots beside canonical files.
