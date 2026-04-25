# Agent Onboarding Guide

End-to-end runbook for adding a new agent to AgentDesk through the dashboard
**Setup Wizard**, plus the failure-recovery procedure when a step half-applies
and a clear separation between two superficially similar surfaces:

- `/api/onboarding/*` — first-time **ADK installation** wizard (one-shot).
- `/api/agents/setup` — **per-agent** wizard, called every time a new agent is
  added or duplicated after ADK is already installed.

Tracking issue: [#1111](https://github.com/itismyfield-org/agentdesk/issues/1111)
("docs/agent-onboarding.md + E2E smoke test + 2-week report template", 912-6).

Cross-references:

- [`docs/source-of-truth.md`](source-of-truth.md) — canonical edit paths the
  wizard mutates (`agentdesk.yaml`, prompts, workspaces, skills manifest).
- [`docs/recovery-paths.md`](recovery-paths.md) — broader recovery vocabulary
  used by the rollback section below.
- `dashboard/src/components/agent-manager/setupWizardHelpers.ts` — pure
  validators / body builders the wizard component imports.
- `src/server/routes/agents_setup.rs` — server endpoint behind the "Confirm"
  step, including the rollback logic this doc documents.

---

## 1. When To Use The Wizard

| Scenario                                            | Surface to use                                  |
|-----------------------------------------------------|-------------------------------------------------|
| First time bringing AgentDesk up on a fresh host    | `/api/onboarding/*` (ADK setup wizard)          |
| ADK is already running, **adding a new agent**      | Dashboard Setup Wizard → `/api/agents/setup`    |
| Cloning an existing agent under a new id            | Dashboard Setup Wizard (duplicate mode) → `/api/agents/{id}/duplicate` |
| Pulling an archived agent back into rotation        | Agent detail → Unarchive → `/api/agents/{id}/unarchive` |
| Hotfixing `agentdesk.yaml` by hand                  | **Don't.** Use the wizard so the prompt / DB / skill manifest stay in sync. |
| Discord-side ADK setup needs to fall back to legacy | See §7 *Discord legacy path criteria* below.    |

---

## 2. The 6 Wizard Steps

The wizard is implemented in
`dashboard/src/components/agent-manager/AgentSetupWizard.tsx` and the pure
validation/body builders in `setupWizardHelpers.ts`. The step IDs are stable
(`role`, `discord`, `prompt`, `workspace`, `cron`, `preview`).

### Step 1 — Role

| Field             | Required | Notes                                                                   |
|-------------------|----------|-------------------------------------------------------------------------|
| `agent_id`        | yes      | 2–64 chars, regex `^[a-zA-Z0-9_-]+$`. Becomes the canonical id everywhere (DB, workspace path, prompt destination). |
| `name`            | yes      | English display name shown in the dashboard header.                     |
| `name_ko`         | no       | Korean display name for Discord embeds. Defaults to `name`.             |
| `department_id`   | no       | Department badge / grouping; `null` means "no department".              |
| `avatar_emoji`    | no       | One-char emoji used in Discord avatars; not validated server-side.      |

### Step 2 — Discord

| Field        | Required | Notes                                                                          |
|--------------|----------|--------------------------------------------------------------------------------|
| `channel_id` | yes      | Existing Discord channel snowflake (10–32 digits). The wizard does **not** create channels — see §7. |
| `provider`   | yes      | `claude` / `codex` / `gemini` / `qwen` / `opencode` / `copilot` / `antigravity` / `api`. |
| Suffix hint  | auto     | `setupWizardHelpers.detectProviderSuffix` auto-fills the provider when `channel_id`'s human name ends with `-cc` / `-cdx` / `-gem` / etc. |

The provider determines which DB column the channel lands in
(`discord_channel_id` for primary, `discord_channel_alt` for secondary, or one
of the typed `discord_channel_cc` / `discord_channel_cdx` columns).

### Step 3 — Prompt

| Field                  | Required | Notes                                                            |
|------------------------|----------|------------------------------------------------------------------|
| `prompt_template_path` | yes (create mode) | Path to a template prompt file. Usually `~/.adk/release/config/agents/_shared.prompt.md`. The wizard copies this to `~/.adk/release/config/agents/<agent_id>/IDENTITY.md`. |
| `prompt_content`       | no       | Inline override edited via `AgentPromptEditor`. Bypasses the template copy step.                                                |

In **duplicate** mode the source agent's `IDENTITY.md` is copied byte-for-byte;
the template path is informational.

### Step 4 — Workspace

| Field         | Required | Notes                                                                  |
|---------------|----------|------------------------------------------------------------------------|
| Workspace dir | auto     | Always `~/.adk/release/workspaces/<agent_id>/`. Created empty.         |
| `skills`      | no       | Comma- / newline-separated skill names. Each must already exist under `~/.adk/release/skills/<skill>`. The skills manifest gets a `(skill, agent)` mapping appended. |

### Step 5 — Cron

| Field         | Required | Notes                                                                              |
|---------------|----------|------------------------------------------------------------------------------------|
| `cronEnabled` | no       | Off by default.                                                                    |
| `cronSpec`    | when enabled | At least 5 fields (standard 5-field cron). Validated client-side; runtime validation happens when the schedule actually fires. |

Cron is wizard-only metadata in the current release; the LaunchAgent /
scheduler integration is tracked separately and does **not** block agent
creation.

### Step 6 — Preview / Confirm

The Confirm step shows the aggregate validation result, then runs the
endpoint twice:

1. `POST /api/agents/setup` with `dry_run=true` — server returns the
   `planned[]` mutation list and any conflicts. Nothing on disk changes.
2. On user "Apply", same body with `dry_run=false`. The endpoint executes the
   mutations sequentially in the order documented in §4 below.

---

## 3. `/api/onboarding/*` vs `/api/agents/setup`

Both surfaces are reachable from the dashboard, but they answer different
questions:

| Question                                              | Surface                  |
|-------------------------------------------------------|--------------------------|
| "Is ADK installed on this host?"                      | `/api/onboarding/status` |
| "Validate the discord bot token"                      | `/api/onboarding/validate-token` |
| "Pick which command bots own which channels"          | `/api/onboarding/channels`, `/api/onboarding/draft` |
| "Run the one-shot installer to materialize `agentdesk.yaml` for the first time" | `/api/onboarding/complete` |
| "Add a new agent now that ADK is installed"           | `/api/agents/setup`      |
| "Duplicate an agent into a new id + channel"          | `/api/agents/{id}/duplicate` (calls setup under the hood, sensitive fields stripped) |
| "Pull an archived agent back into the active set"     | `/api/agents/{id}/unarchive` |

Operationally:

- `/api/onboarding/*` writes to `~/.adk/release/config/agentdesk.yaml`'s
  global `discord:` section and seeds command-bot state. It is expected to
  run **once** per host and is idempotent only across re-runs of the same
  installer state machine; it is *not* designed to be re-invoked to add
  agents.
- `/api/agents/setup` is per-agent and idempotent: re-running with the same
  body returns `200 OK` with all steps `skipped`. The same endpoint is the
  inner call site for `duplicate` and `unarchive`.

If a screen lets the user toggle between the two, the convention is:

- empty / fresh host → ADK setup wizard.
- host with `agentdesk.yaml` and ≥1 agent already → Setup Wizard for new
  agents. The dashboard checks `/api/onboarding/status` to decide which one
  to render first.

---

## 4. What `/api/agents/setup` Mutates

Mutations run in this order. Each step records a `(step, target, status)`
tuple in the response so a failed run can be diagnosed without reading
server logs.

| # | Step                | Target                                                   | Idempotent?        |
|---|---------------------|----------------------------------------------------------|--------------------|
| 1 | `agentdesk_yaml`    | Append agent block to `~/.adk/release/config/agentdesk.yaml` | Yes (no-op if equal) |
| 2 | `discord_binding`   | Bind the existing `channel_id` under `agents[].channels.<provider>` | Yes               |
| 3 | `prompt_file`       | Copy `prompt_template_path` → `config/agents/<id>/IDENTITY.md` | Yes (byte-equal short-circuit) |
| 4 | `workspace_seed`    | `mkdir -p workspaces/<id>/`                              | Yes                |
| 5 | `db_seed`           | `INSERT INTO agents` with provider/channel columns       | Yes (matches existing row) |
| 6 | `skill_mapping`     | Append `(skill, agent)` rows to `skills.manifest.json`   | Yes (entry-equal short-circuit) |

Discord channel creation is **not** in this list. Channels are created in
Discord first (manually or by the legacy preview-bridge — see §7) and the
snowflake is then handed to the wizard.

---

## 5. Failure Recovery (Rollback Procedure)

Server-side rollback is automatic for all `prompt_file` / `workspace_seed` /
`db_seed` / `skill_mapping` failures: the response carries a `rolled_back[]`
array listing every step the server reversed. The user-visible recovery
procedure exists for the cases the server cannot self-heal — usually a process
crash mid-mutation, or a partial network failure on the dashboard side.

### 5.1 Detection

Symptom matrix:

| Symptom                                         | Likely partial state                                   | First check                              |
|-------------------------------------------------|--------------------------------------------------------|------------------------------------------|
| Wizard ended with HTTP 500 + `rolled_back[]` populated | Server already reverted everything in `rolled_back[]`. Confirm one-by-one. | Inspect response body in browser devtools. |
| Wizard timed out (no response)                  | Any subset of steps may be applied                     | `~/.adk/release/config/agentdesk.yaml` audit log under `config/.audit/` |
| Wizard succeeded but Discord channel never receives messages | Channel binding was registered but `provider` in DB is mismatched | `SELECT provider, discord_channel_* FROM agents WHERE id=?` |
| `cargo` errors complaining about a missing prompt file at boot | `agentdesk_yaml` applied, `prompt_file` did not | `ls config/agents/<id>/IDENTITY.md` |

### 5.2 Manual rollback procedure

Run these commands from the AgentDesk runtime root (`~/.adk/release/` in
production, the worktree root in dev). Each step is idempotent — repeat on
failure.

1. **Snapshot first**:
   ```bash
   cp config/agentdesk.yaml config/agentdesk.yaml.rollback-$(date +%s)
   ```
2. **Remove the agent block from `agentdesk.yaml`**: open the file, delete
   the entry under `agents:` whose `id:` matches the new agent. (Validated
   by `agentdesk config audit --dry-run`.)
3. **Remove the prompt destination**:
   `rm -rf config/agents/<agent_id>/`
4. **Remove the workspace seed**:
   `rm -rf workspaces/<agent_id>/`
5. **Remove the DB row**:
   ```sql
   DELETE FROM agents WHERE id = '<agent_id>';
   ```
6. **Remove skill manifest entries**: open
   `config/skills.manifest.json`, drop any `workspaces[]` or `providers[]`
   element that references `<agent_id>`. Delete the whole skill key if it
   becomes empty.
7. **Re-run the wizard**. The dry-run preview should now show all six steps
   as `planned`, confirming the rollback is clean.

### 5.3 Failure injection in tests

`AGENTDESK_TEST_AGENT_SETUP_FAIL_AFTER=<step>` (env var, `#[cfg(test)]`
only) forces the named step to return a synthetic failure after applying its
mutation. This is what the E2E smoke test in
`src/integration_tests/tests/agents_setup_e2e.rs` uses to verify rollback.
Steps recognized: `agentdesk_yaml`, `prompt_file`, `workspace_seed`,
`db_seed`, `skill_mapping`.

---

## 6. Archive / Unarchive / Duplicate

These three are *not* wizard steps but they share the same configuration
surface, so they live here for completeness.

### 6.1 Archive

`POST /api/agents/<id>/archive` snapshots the `agents[]` block, the prompt
path, the role-map (legacy), and the `discord_action` (the operator decides
whether to also leave the Discord channel). The archive row goes into
`agent_archive` with `state='archived'`.

Refused with `409 Conflict` while a session for the agent is `working` —
clear the active turn first.

### 6.2 Unarchive

`POST /api/agents/<id>/unarchive` rehydrates `agentdesk.yaml` from the
snapshot and flips `agent_archive.state` to `unarchived`. The agent is
immediately available again. The DB row, prompt file, workspace, and skill
mappings are restored to the snapshot — no second wizard pass is needed.

### 6.3 Duplicate

`POST /api/agents/<id>/duplicate` takes a small allow-listed body
(`new_agent_id`, `channel_id`, optional `name`/`name_ko`/`department_id`/
`provider`/`skills`) and:

1. Loads the source agent's metadata.
2. Resolves the source `prompt_path` (or falls back to the conventional
   `config/agents/<source_id>/IDENTITY.md`).
3. Calls `agents_setup::setup_agent` internally with the new id + new
   channel.
4. Updates `agentdesk.yaml` `name` / `name_ko` / `department` /
   `avatar_emoji` to match the request body.

Sensitive fields **explicitly excluded** from duplication, even if the
caller tries to inject them:

- `id` / `agent_id` — locked to `new_agent_id`.
- `discord_channel_id` (raw column) — must come from the request `channel_id`,
  never the source row.
- `token`, `api_key`, `system_prompt` — never copied. The new agent's prompt
  comes from the *source's `IDENTITY.md`*, not from any body parameter.

The smoke test in §8 asserts every one of these.

---

## 7. Discord Legacy Path Criteria

The "legacy" Discord onboarding path is the pre-#1067 preview-bridge runbook
that creates Discord channels via the `agentdesk --discord-*` CLI directly,
bypassing `/api/onboarding/*`. We keep it documented because there are still
two valid reasons to fall back.

| Criterion (any one is sufficient)                                     | Fall back to legacy? |
|-----------------------------------------------------------------------|----------------------|
| The host has no working dashboard yet (e.g. `dashboard/` build broken) | Yes — bootstrap with the CLI, finish in dashboard later. |
| Discord API rate-limit on a freshly-created server (>30 channels in 5 min) | Yes — the legacy path batches with built-in backoff that the wizard does not have. |
| Recovering an `agentdesk.yaml` from a backup tarball                   | No — the wizard's idempotent re-run handles it; legacy will double-create. |
| Adding a single new agent in normal operation                          | No — always use the wizard. |
| Channel needs to exist in Discord before any AgentDesk state          | Manual click in Discord, then `agents/setup`. The CLI is *not* required. |

Operationally, the legacy CLI path is now mostly used as a recovery seam,
not a happy path. Issues opened against it should reference #1067 (skill
deprecation) and the new `/api/discord/send` endpoint as the canonical
replacement for outbound messaging.

When in doubt: **try the wizard first**. If it fails *and* one of the rows
above applies, document the failure mode in the issue body and use the
legacy CLI to unblock; otherwise fix the wizard.

---

## 8. End-to-End Smoke Test

`src/integration_tests/tests/agents_setup_e2e.rs` exercises the full chain
through the public HTTP surface:

1. Boot an axum router on top of an in-memory DB and a tempdir runtime root.
2. Seed a fake skill (`memory-read`) and a shared prompt template.
3. **Mock Discord channel creation**: wire `MockDiscord` from the
   `discord_flow` harness as the outbound transport; the test pre-records a
   "channel exists" snowflake the wizard will bind to.
4. **Dry-run** `POST /api/agents/setup` — assert nothing on disk changed,
   `planned[]` lists all 6 steps.
5. **Execute** the same body — assert
   `agentdesk.yaml` / `IDENTITY.md` / workspace dir / DB row / skills
   manifest are all present.
6. **Discord delivery check** — relay a message via
   `OutboundDeduper::deliver_outbound` to the bound channel and assert the
   mock saw exactly one POST.
7. **Failure injection**: re-run with
   `AGENTDESK_TEST_AGENT_SETUP_FAIL_AFTER=prompt_file` against a fresh
   tempdir. Assert HTTP 500 and a non-empty `rolled_back[]` containing both
   `prompt_file` and the earlier `agentdesk_yaml` step.
8. **Archive → unarchive**: archive the agent, confirm
   `agentdesk.yaml` no longer has it; unarchive, confirm it is restored
   byte-for-byte from the snapshot.
9. **Duplicate without sensitive fields**: send `id`, `agent_id`,
   `discord_channel_id` (source), `token`, `api_key`, `system_prompt` in
   the duplicate body; assert the new row uses `new_agent_id` /
   `new_channel`, source channel does not appear in any of the four channel
   columns, and `system_prompt` is **not** populated from the body.

Run with:

```bash
cargo test --bin agentdesk integration_tests::tests::agents_setup_e2e
```

---

## 9. 2-Week Observation

`docs/reports/agent-onboarding-2-week-template.md` is filled at T+14 days
after #1111 lands. Headline metrics:

- New-onboarding failure rate (rolled_back / total).
- Rollback occurrence by step.
- Discord legacy CLI usage frequency vs wizard.
- Time-to-first-message for wizard-onboarded agents.

Action thresholds and SQL queries live in the template.
