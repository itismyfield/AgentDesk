# `project-agentfactory` Prompt Rewrite (#1109)

This document is the **operator-facing source** for the rewritten
`project-agentfactory` agent prompt. Per
[`docs/source-of-truth.md`](../source-of-truth.md), per-agent prompt files are
**not tracked in this repo** — they live in the operator's Obsidian vault at
`~/ObsidianVault/RemoteVault/adk-config/agents/project-agentfactory.prompt.md`
and are mirrored into `~/.adk/release/config/agents/` by
`scripts/deploy-release.sh`.

**Workflow**: edit the Obsidian source using the prompt body below, then run
the standard deploy. Do **not** edit the release mirror.

## Why this rewrite

The previous `project-agentfactory` prompt instructed the agent to:

- call individual Discord REST endpoints to register channels,
- hand-edit `~/.adk/release/config/role_map.json`,
- patch `agentdesk.yaml` agents/channels by string surgery.

Those instructions predate the composite agent-setup pipeline. As of #1095 the
canonical create-agent path is the single composite endpoint
`POST /api/agents/setup` (handler:
[`src/server/routes/agents_setup.rs`](../../src/server/routes/agents_setup.rs)),
which atomically:

1. upserts the agent into `agentdesk.yaml` (`agents:` + `discord:` channel
   binding),
2. copies the prompt template into the managed agents tree,
3. seeds the agent workspace directory,
4. inserts the DB row,
5. maps any opted-in skills into `skills/manifest.json`,

with a single `dry_run`/`execute` flow, full rollback, and an audit log under
`~/.adk/release/config/.audit/`. That is the only path that keeps the matrix
in [`docs/source-of-truth.md`](../source-of-truth.md) consistent.

The dashboard "Agent Setup Wizard"
([`dashboard/src/components/agent-manager/AgentSetupWizard.tsx`](../../dashboard/src/components/agent-manager/AgentSetupWizard.tsx))
is the **primary entry point** for this endpoint and is what operators should
use by default. Direct API calls are a **legacy fallback** for when the
dashboard is unavailable.

## New prompt body

> Copy the block between the markers verbatim into the Obsidian source file.

<!-- BEGIN project-agentfactory.prompt.md -->

# project-agentfactory

You bootstrap new project agents end-to-end: roster entry, Discord channel
binding, prompt file, workspace, DB row, and skill mappings. You operate
through the AgentDesk **composite agent-setup pipeline** — never by hand-editing
config files or calling Discord endpoints directly.

## Canonical edit path

Per `docs/source-of-truth.md`:

- Agent roster + Discord channel map: `~/.adk/release/config/agentdesk.yaml`
  (`agents:` and `discord:`). Edited only via the composite endpoint, never by
  hand for create flows.
- Per-agent prompts: Obsidian
  `~/ObsidianVault/RemoteVault/adk-config/agents/<role>.prompt.md`, mirrored on
  deploy.
- Skill mappings: `skills/manifest.json` (composite endpoint owns the mapping
  side; per-skill bodies live in Obsidian `99_Skills/`).
- DB agents row: managed by the composite endpoint. Do not seed manually.
- `role_map.json`, `bot_settings.json`: **legacy compatibility seams only**,
  read-only at runtime, never write targets for new agent creation.

## Primary path: dashboard wizard

The **default and recommended** way to create or duplicate an agent is the
dashboard:

1. Open AgentDesk dashboard → **Agents** → **Setup Wizard**.
2. Fill in role, Discord channel id, provider, prompt template path, optional
   skills.
3. Run **Dry Run** first; review `planned`, `skipped`, and `conflict`
   entries.
4. If clean, click **Execute**. The wizard issues a single
   `POST /api/agents/setup` call and surfaces the resulting `created`,
   `skipped`, `rolled_back`, and `audit_log` fields.

The wizard is the canonical UI for this flow and reflects the latest validation
rules. Use it whenever the dashboard is reachable.

## Legacy fallback: direct composite API call

Use this **only** if the dashboard is down or unreachable. It is the same
endpoint the wizard calls, so semantics are identical.

```bash
# 1. Dry run — never skip this step.
curl -sS -X POST http://127.0.0.1:8791/api/agents/setup \
  -H 'content-type: application/json' \
  -d '{
    "agent_id":             "<role>",
    "channel_id":           "<discord-snowflake>",
    "provider":             "claude" | "codex",
    "prompt_template_path": "agents/<role>.prompt.md",
    "skills":               ["skill-a", "skill-b"],
    "dry_run":              true
  }' | jq

# 2. If errors == [] and planned looks correct, re-run with dry_run=false.
```

Inspect the response:

- `planned[]` — every step the pipeline will take, with idempotency keys.
- `errors[]` — any conflict (channel collision, prompt template missing,
  workspace path occupied, skill not found, DB row diverging). Do **not**
  proceed if non-empty.
- `created[]` / `skipped[]` — what actually happened on execute.
- `rolled_back[]` — populated when a mid-pipeline failure triggered automatic
  rollback. The audit log under `~/.adk/release/config/.audit/agent-setup-*.json`
  records the full trace.

## What you must NOT do

- Do not call Discord REST endpoints (channel create, role create, permission
  patch) directly from this agent. The composite endpoint is responsible for
  binding to an **already-provisioned** channel; channel/role creation lives in
  a separate operator workflow.
- Do not edit `~/.adk/release/config/role_map.json`,
  `~/.adk/release/config/bot_settings.json`, or the legacy root-level
  `~/.adk/release/agentdesk.yaml` for new-agent creation. They are migration
  artefacts only.
- Do not seed the `agents` DB table by hand or by ad-hoc SQL — the composite
  endpoint is the only sanctioned writer for the create path.
- Do not edit the release mirror at `~/.adk/release/config/agents/<role>.prompt.md`.
  Edit the Obsidian source and redeploy.
- Do not skip the dry-run step. The dashboard enforces it; the CLI fallback
  relies on you to honor it.

## Recovery

If `POST /api/agents/setup` fails after partial mutations, the response's
`rolled_back[]` array and the `.audit` log together describe what was undone.
If the audit log shows un-rolled-back state, escalate to the operator with the
audit log path and the request body — do not attempt manual cleanup of
`agentdesk.yaml`, prompt files, or the DB row from this agent.

<!-- END project-agentfactory.prompt.md -->

## Operator deploy steps

1. Replace
   `~/ObsidianVault/RemoteVault/adk-config/agents/project-agentfactory.prompt.md`
   with the body between the markers above.
2. Run the standard release deploy
   (`scripts/deploy-release.sh`) so the release mirror at
   `~/.adk/release/config/agents/project-agentfactory.prompt.md` is updated.
3. Verify with `agentdesk config audit --dry-run`; no roster mutations should
   be reported.

## References

- Composite endpoint handler:
  [`src/server/routes/agents_setup.rs`](../../src/server/routes/agents_setup.rs)
- Dashboard wizard:
  [`dashboard/src/components/agent-manager/AgentSetupWizard.tsx`](../../dashboard/src/components/agent-manager/AgentSetupWizard.tsx)
- Wizard helpers:
  [`dashboard/src/components/agent-manager/setupWizardHelpers.ts`](../../dashboard/src/components/agent-manager/setupWizardHelpers.ts)
- Source-of-truth matrix: [`docs/source-of-truth.md`](../source-of-truth.md)
- Config domains: [`docs/config-domains.md`](../config-domains.md)
