# Source Of Truth

This document answers one question: when the same knowledge appears in repo files, runtime files, provider config, or legacy backups, which file do we edit?

The rules below are derived from the current runtime/layout code in `src/runtime_layout/paths.rs`, `src/services/discord_config_audit.rs`, `src/services/mcp_config.rs`, and the release copy flow in `scripts/promote-release.sh`.

## Rules

- Edit the canonical path only. Replicas and fallback files are read-only unless a migration or restore workflow explicitly says otherwise.
- Repo-tracked files are edited in this repo and then promoted into `~/.adk/release/`.
- Operator-managed runtime files under `~/.adk/release/config/` are edited in place. They are not currently git-tracked, so audit uses `agentdesk config audit`, targeted diffs, and structured backups instead of `git log`.
- Legacy snapshots (`*.pre-*`, `*.bak`, `*.migrated`) are archive-only. They belong under `~/.adk/release/config/.backups/YYYY-MM-DD/`, not next to canonical files.
- Compatibility seams such as `role_map.json`, `bot_settings.json`, the root-level legacy `agentdesk.yaml`, and `_shared.md` aliases are not canonical write targets.

## Matrix

| Vector | Canonical file | Allowed replicas / consumers | Edit path | Audit command |
| --- | --- | --- | --- | --- |
| Runtime baseline config | `~/.adk/release/config/agentdesk.yaml` | `~/.adk/release/agentdesk.yaml` is legacy fallback only. `agentdesk.example.yaml` documents shape, not live state. Runtime overrides in `kv_meta['runtime-config']` are a separate live-override surface. | Edit `~/.adk/release/config/agentdesk.yaml`. Do not edit the root fallback. | `./target/debug/agentdesk config audit --dry-run` |
| Discord bot bindings, agent roster, channel map | `~/.adk/release/config/agentdesk.yaml` (`discord:` and `agents[].channels`) | Legacy `~/.adk/release/config/role_map.json`, legacy `~/.adk/release/config/bot_settings.json`, and DB materialization can exist during migration, but `agentdesk.yaml` wins. | Edit `~/.adk/release/config/agentdesk.yaml` only. | `./target/debug/agentdesk config audit --dry-run` |
| Per-agent prompt files | Repo `config/agents/<role>.prompt.md` | Release mirror `~/.adk/release/config/agents/<role>.prompt.md`; one-way audit copy `~/ObsidianVault/RemoteVault/adk-config/agents/<role>.prompt.md` from `scripts/promote-release.sh`. | Edit repo prompt file, then redeploy. | `git log -- config/agents/<role>.prompt.md` |
| Shared prompt | Repo `config/agents/_shared.prompt.md` | Release mirror `~/.adk/release/config/agents/_shared.prompt.md`; symlink aliases `~/.adk/release/config/agents/_shared.md` and `~/.adk/release/config/_shared.md`. | Edit `config/agents/_shared.prompt.md`, then redeploy. Do not edit `_shared.md` aliases. | `git log -- config/agents/_shared.prompt.md` |
| Policy hooks | Repo `policies/*.js` | Release mirror `~/.adk/release/policies/*.js`. | Edit repo policy file, then redeploy. | `git log -- policies/<name>.js` |
| Default pipeline | Repo `policies/default-pipeline.yaml` | Release mirror `~/.adk/release/policies/default-pipeline.yaml`; example pipelines under `policies/examples/` are references only. | Edit `policies/default-pipeline.yaml`. | `git log -- policies/default-pipeline.yaml` |
| Workspace agent contract (`CookingHeart/AGENTS.md`) | `~/CookingHeart/AGENTS.md` | None. This lives in the target workspace repo, not in AgentDesk. | Edit the workspace repo file directly. | `git -C ~/CookingHeart log -- AGENTS.md` |
| Claude home guidance | Symlink target of `~/.claude/CLAUDE.md` (`/Users/itismyfield/ObsidianVault/RemoteVault/10_Claude/mac-mini/claude-home/CLAUDE.md` on this machine) | `~/.claude/CLAUDE.md` is a symlink entry point only. | Edit the symlink target or use the CLAUDE relocation workflow. Do not replace the symlink with ad hoc content. | `readlink ~/.claude/CLAUDE.md` |
| Workspace CLAUDE guide | `~/CookingHeart/CLAUDE.md` | Workspace-local copies or exports may exist elsewhere, but this file is the workspace contract. | Edit the workspace repo file directly. | `git -C ~/CookingHeart log -- CLAUDE.md` |
| Shared agent knowledge | `~/.adk/release/config/memories/shared-agent-knowledge/shared_knowledge.md` | Provider prompts and context builders consume it; it is not mirrored back into repo history. | Edit the runtime file or the managed memory workflow that owns it. Do not store turn history here. | `sed -n '1,80p' ~/.adk/release/config/memories/shared-agent-knowledge/shared_knowledge.md` |
| Shared agent memory (SAM) | `~/.adk/release/config/memories/shared-agent-memory/<agent>.json` | Runtime readers and memory sync tooling consume these JSON files. | Prefer tool-driven updates; manual edits are repair-only. | `ls ~/.adk/release/config/memories/shared-agent-memory/<agent>.json` |
| Long-term memory (LTM) | `~/.adk/release/config/memories/long-term/<agent>/` | Historical Obsidian long-term paths are legacy inputs; current managed path is under `config/memories/long-term/`. | Edit through the memory workflow or the canonical runtime files under this tree. | `find ~/.adk/release/config/memories/long-term/<agent> -maxdepth 2 -type f | sort` |
| MCP server declarations | `~/.adk/release/config/agentdesk.yaml` (`mcp_servers:`) | Codex mirror: `~/.codex/config.toml` via `sync_codex_mcp_servers()`. Claude consumer: `~/.claude/.mcp.json`. Provider config is a consumer surface, not the source. | Edit `~/.adk/release/config/agentdesk.yaml`, then restart or resync. Do not hand-edit provider mirrors unless repairing a broken sync. | `rg -n "mcp|mcp_servers" ~/.adk/release/config/agentdesk.yaml ~/.codex/config.toml ~/.claude/.mcp.json` |
| Archived config snapshots | `~/.adk/release/config/.backups/YYYY-MM-DD/` | None. This is the only allowed home for `*.pre-*`, `*.bak`, and `*.migrated` snapshots. | Never edit in place. Restore or diff explicitly if needed. | `find ~/.adk/release/config/.backups -maxdepth 2 -type f | sort` |

## Notes

- `~/.adk/release/config.backup-v1/` is a separate one-time migration archive. Leave it untouched unless a dedicated cleanup issue handles it.
- The settings precedence contract for YAML vs `kv_meta` runtime overrides lives in [docs/adr-settings-precedence.md](adr-settings-precedence.md).
- If `agentdesk.yaml` and a legacy file disagree, follow the current code rule: `agentdesk.yaml` wins and legacy files are migration inputs or stale snapshots.
