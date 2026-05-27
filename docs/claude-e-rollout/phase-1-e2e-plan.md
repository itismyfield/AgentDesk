# Phase 1 — Discord e2e plan

When the Phase 1 counter-reviews clear, execute this in order. Each step is
revertible by the corresponding rollback line.

## Pre-flight

- `claude-e` binary present (`which claude-e` → `/opt/homebrew/bin/claude-e`,
  version `0.1.9`).
- Local build clean (`cargo build`, fmt, all `provider_hosting` tests).
- e2e channel identified: `#adk-dash-cc-e2e` Discord channel id
  `1506295332949196840` (Claude provider, `adk-dashboard-e2e` agent).

## Step 1 — Deploy the new binary

Use the release pipeline. We have an active feature branch that has not
merged to main yet, so override the source-identity guard.

```sh
cd /Users/itismyfield/.adk/release/worktrees/claude-adk-cc-20260527-104753
AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 \
AGENTDESK_DEPLOY_ALLOW_DIRTY=1 \
./scripts/deploy-release.sh
```

The release dcserver (`com.agentdesk.release` launchd job) restarts
automatically when `deploy-release.sh` finishes. Until the channel config
in step 2 changes, every Claude dispatch still routes through `TuiHosting`
exactly as before — the new binary is dormant for claude-e until the
selector flips.

**Rollback**: redeploy from `main` (no `ALLOW_NON_MAIN`).

## Step 2 — Flip the e2e channel selector

Edit the operator's runtime config (NOT committed):

```sh
$EDITOR /Users/itismyfield/ObsidianVault/RemoteVault/adk-config/agentdesk.yaml
```

Locate the `adk-dashboard-e2e` agent's Claude channel and add the
`runtime` line:

```yaml
- id: adk-dashboard-e2e
  name: ADK Dashboard E2E
  channels:
    claude:
      id: '1506295332949196840'
      name: adk-dash-cc-e2e
      prompt_file: ~/.adk/release/config/agents/project-agentdesk.prompt.md
      workspace: ~/.adk/release/workspaces/agentdesk
      provider: claude
      tui_hosting: true       # left intact — claude-e wins via explicit runtime
      runtime: claude-e       # ← add this line
```

Reload runtime config (the dcserver re-reads on SIGHUP via the
restart-dcserver subcommand):

```sh
agentdesk restart-dcserver
```

Inspect the install log to confirm the runtime mirror picked up the
override:

```sh
log show --predicate 'process == "agentdesk"' --info --last 1m \
  | grep -E 'provider runtime_mode|per-channel runtime_mode'
```

Expect a line like:
`provider per-channel runtime_mode config installed
 channel_summary=claude:1506295332949196840=claude-e`.

**Rollback**: delete the `runtime:` line and `agentdesk restart-dcserver`.

## Step 3 — Drive a turn from Discord

Send a benign prompt to the channel:

> "Please run `pwd` and reply with the path."

What success looks like:

- A normal assistant reply arrives in `#adk-dash-cc-e2e`.
- Server logs show `claude_e.execute_streaming spawning` immediately
  before the reply window opens.
- The `inflight_state` row for the turn stamps
  `runtime_kind=claude_e_adapter`.
- The `claude-e` subprocess exits with code 0 (`tracing::info` line at
  end of `execute_streaming`).

## Step 4 — Cancel cascade smoke

Send a long-running prompt then issue a stop request from Discord:

> "Sleep with a `Bash` call for 90 seconds."
> (then send `/stop` or the AgentDesk-specific cancel)

Verify with:

```sh
ps -axf | grep -E 'claude-e|claude' | grep -v grep
```

Expect no orphaned `claude-e` or child `claude` processes after the stop
returns.

## Step 5 — Hook conflict probe

After the first claude-e turn, check whether any AgentDesk hook bundle
fired unexpectedly:

- `memento` consolidate should NOT trigger on every turn (per existing
  memory policy). Confirm by tailing memento writes:
  ```sh
  ls -ltr ~/.adk/memento/ | tail
  ```
- The new `stop_hook_summary` record visible in trace logs should list
  only AgentDesk-installed hooks plus claude-e's `hook-relay.sh` —
  no duplicates.

## Step 6 — Rollback decision

If any of Steps 3–5 fail, the immediate revert is:

1. Remove the `runtime: claude-e` line from operator yaml.
2. `agentdesk restart-dcserver`.
3. Optional: redeploy `main` binary if the new binary itself misbehaves.

The decision log gets an entry naming the failure and the next-fix plan.

## Phase 1 → Phase 2 gate

When Steps 3–5 succeed in a single sitting and the cancel/orphan smoke
returns 0, append a "PASS" line to the decision log with the run
timestamp and Discord message URL, then promote to Phase 2 (additional
channels via announcebot).
