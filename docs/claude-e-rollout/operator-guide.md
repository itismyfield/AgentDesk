# Operator guide — Claude runtime selection

AgentDesk supports three Claude runtimes. They are mutually exclusive
per channel and per provider; the resolver picks one at dispatch time.

| Mode | Selector | What it runs | When to pick |
|---|---|---|---|
| `pipe` | `runtime: pipe` (**default** in `agentdesk.example.yaml`) *(also implicit when `tui_hosting: false` and no `runtime` set)* | `claude -p` invocation wrapped by AgentDesk's tmux session supervisor | Default. Simplest path, lowest moving parts. |
| `tui` | `runtime: tui` *(or legacy `tui_hosting: true` with no `runtime` set)* | Long-running interactive Claude TUI inside a tmux pane, keystroke-relayed | When you want `tmux attach` observability or follow-up turns to share an in-process state cache (warm MCP, project knowledge). |
| `claude-e` | `runtime: claude-e` *(opt-in only — requires `npm install -g claude-e` on the host)* | Per-turn `claude-e` PTY-backed wrapper around `claude` | When you want clean per-turn process boundaries (cancel / recovery / lifecycle become process exit codes). Operator must opt in explicitly. |

The `tui_hosting` boolean is preserved for back-compat: configs that
predate the `runtime` field continue to resolve exactly as before.
When both are set, `runtime` wins.

## Scope of the selector

- **Provider-level** (`providers.claude.runtime`) — default for every
  channel served by the Claude provider.
- **Per-channel** (`agents[].channels.claude.runtime`) — channel
  override; wins over the provider-level setting.

Channels with no explicit `runtime` inherit the provider-level value;
provider with no explicit `runtime` falls back to the legacy
`tui_hosting` boolean.

## Live toggle

1. Edit either
   - `~/.adk/release/config/agentdesk.yaml` (the operator's running
     config — picked up on the next dcserver restart), or
   - `~/ObsidianVault/RemoteVault/adk-config/agentdesk.yaml` (the
     source-of-truth that `scripts/deploy-release.sh` stages into the
     release config).
2. Update the `runtime:` line under the desired provider or channel.
3. Restart dcserver to reload config:
   ```sh
   agentdesk restart-dcserver
   ```
4. Verify in the log:
   ```sh
   grep "runtime_mode" ~/.adk/release/logs/dcserver.stdout.log | tail -2
   ```
   Look for `channel_summary="claude:<channel id>=<mode>"`.

## Per-runtime observability

| Signal | `pipe` | `tui` | `claude-e` |
|---|---|---|---|
| Process under inspection | `tmux attach -t AgentDesk-claude-...` → wraps `claude -p` | `tmux attach -t AgentDesk-claude-...` → interactive Claude TUI | `pgrep -lf claude-e` (per-turn process) |
| Transcript path | tmux output capture file | `~/.claude/projects/.../<session_id>.jsonl` | streamed in-memory through the bridge (no disk file in Phase 1) |
| Watchdog log on success | `▶ Response sent` via tmux watcher direct relay | `▶ Response sent` via TUI direct relay | `▶ Response sent` from `discord_turn_bridge`; cleanup-cancel kill suppressed by the `completion_cleanup` flag |

## Rollback contract

From `docs/claude-e-rollout/rollout-plan.md` rollback matrix
(reproduced here for quick reference):

- **Config-only rollback** — flip `runtime:` back to the previous
  value and `agentdesk restart-dcserver`. Mirrors are rebuilt on
  startup. Inflight rows on disk retain `runtime_kind` and survive
  the change because the deserializer drops unknown variants safely.
- **Binary single-commit rollback** — `git revert <commit>` followed
  by `scripts/deploy-release.sh` if a specific binary change
  regresses behaviour.
- **Full binary rollback** — `git revert` each rollout commit in
  reverse order; the back-compat shim on `tui_hosting` ensures
  pre-`runtime` configs continue working at every revert step.

## Known gaps under `runtime: claude-e` (Phase 1 record)

These are tracked in `decision-log.md` and slated for Phase 1.x:

- `cache_ttl_minutes` is accepted in config but not yet forwarded to
  the underlying claude CLI argument.
- `rate_limit_event` records are not yet surfaced under `claude-e`
  output. 429s currently surface as a hard error rather than a
  wait-and-retry hint.
- `result.duration_ms`, `num_turns`, `total_cost_usd`, and
  `modelUsage` are absent in the claude-e `result` record; token
  usage still flows through per-message `assistant.usage`, but cost
  / duration / turn-count telemetry are `None` for claude-e turns.

If any of these are operationally important, switch the affected
channel to `runtime: tui` (or `runtime: pipe`) until Phase 1.x lands
the missing fields.

## Quick reference

- Decision log (chronological design record):
  `docs/claude-e-rollout/decision-log.md`
- Rollout plan + rollback matrix:
  `docs/claude-e-rollout/rollout-plan.md`
- Phase 1 / Phase 2 e2e procedures:
  `docs/claude-e-rollout/phase-1-e2e-plan.md`,
  `phase-2-e2e-plan.md`
