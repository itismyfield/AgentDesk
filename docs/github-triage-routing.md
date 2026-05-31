# GitHub Triage Agent Routing

GitHub issue triage uses a deterministic first stage before PMD fallback.

1. If an issue already has an `agent:<id>` label, that explicit label wins.
2. If no explicit agent label exists, `src/github/triage.rs` evaluates the declarative signal table.
3. If exactly one owner is matched, the card is assigned to that agent and AgentDesk attempts to write back `agent:<id>` to GitHub.
4. If no owner matches, or signals point at multiple owners, the card remains unassigned so the existing PMD classification path handles it.

Current routing signals:

| Signals from title, body, paths, or labels | Agent label |
| --- | --- |
| `dashboard`, `frontend`, `KanbanHeaderSurface`, `dashboard/` | `agent:adk-dashboard` |
| `relay`, `discord`, `tui`, `tmux`, `codex-tui`, `turn_bridge`, `inflight`, `watcher` | `agent:project-agentdesk` |
| `token`, `rate limit`, `rate_limit`, `rate-limit`, `quota`, `usage` | `agent:token-manager` |
| `e2e`, `tui-relay-e2e`, `scenario`, `tests/e2e/` | `agent:project-agentdesk` |
| `area:security`, `ci-red` | `agent:project-agentdesk` |

E2E currently routes to `project-agentdesk` because the GitHub repository has no `agent:adk-e2e-orchestrator` label. Add that label and update the table when the orchestrator becomes a routable issue owner.
