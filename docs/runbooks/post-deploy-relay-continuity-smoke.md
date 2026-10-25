# Post-Deploy Relay Continuity Smoke

This runbook covers issue #3729: prove that a live TUI relay survives the
release deploy or restart boundary. A server-up check is not enough; the smoke
must show that tmux survives, the restarted dcserver reattaches relay ownership,
and post-restart output reaches Discord.

This is an operator-invoked live procedure for `E-9,E-19`, not a CI run or the
automatic deployment smoke. See the [scenario execution contract](../../tests/e2e/tui_relay/scenarios/README.md)
for manual prerequisites and the conditional automatic post-deploy subset
(`E-1`, `E-35`, `E-50`, `E-51`). Live YAML files alone do not establish CI coverage.

For a coordinator-approved degraded cause, set `AGENTDESK_DEPLOY_ALLOW_DEGRADED='tui_o:schema_blocked:1479671301387059200'` for that deploy: this nonempty ERE (no commas or line breaks) must match each waived reason in full; it applies to local and peer deploy verdicts, logs the additional waived reasons or `none`, and leaves server, DB, dashboard, unhealthy, and rollback checks unchanged. The obsolete `--skip-health` and `--skip-review` flags are rejected.

## Offline Checks

These modes are CI-safe and do not require Discord credentials:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py --self-check
python3 scripts/e2e/post_deploy_relay_continuity.py --fixture pass
python3 scripts/e2e/post_deploy_relay_continuity.py --fixture relay-missing
python3 scripts/e2e/post_deploy_relay_continuity.py --fixture bad-state
```

`relay-missing` and `bad-state` are expected to fail. They validate that the
smoke distinguishes local output from Discord relay continuity and catches
ownerless inflight, relay-dead, stale proof, and orphaned target states.

Offline checks report `agent_mode: none` and `real_provider_contacted: false`.
They are fixtures only; they must never satisfy a required live relay gate.

## Live Dry Run

Use dry run before a release deploy to validate config, channel id lookup, and
the exact command that will be delegated to the TUI relay driver:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py \
  --cell claude-tui \
  --dry-run \
  --deploy-command 'AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 scripts/deploy-release.sh'
```

The script resolves the cell's worker agent channel id (`claude-tui`:
`adk-claude-tui-2-e2e`) from
`~/.adk/release/config/agentdesk.yaml` unless `--channel-id` is supplied.
Dry run declares the planned live lane but reports `agent_mode_actual: none` and
`real_provider_contacted: false`; it verifies wiring without contacting the TUI
provider.

## Agent Mode Contract

The live smoke is an explicit `agent_mode: real_live` scenario. The summary and
wrapped TUI relay report must include the requested lane, observed lane, cell
identity, provider identity, `real_provider_contacted`, and failure attribution.
The wrapper rejects live success unless both `E-9` and `E-19` report
`agent_mode: real_live` and `real_provider_contacted: true`.

## Live Smoke

Run this only against the dedicated TUI E2E worker channels:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py \
  --cell claude-tui \
  --confirm-live \
  --deploy-command 'AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 scripts/deploy-release.sh'
```

For Codex TUI coverage, change `--cell codex-tui`.

The wrapper runs `scripts/e2e/run_tui_relay.py` with scenarios `E-9,E-19`,
`--restart-target-override release`, and a generated restart wrapper around the
deploy command. Evidence lands under `out/e2e/post_deploy_relay_continuity/`
with both the cell report and `post_deploy_relay_continuity.summary.json`.

## Pass Criteria

The smoke passes only when all of these are true:

- `E-9` sees a pre-restart stream marker and a post-restart Discord tail marker.
- `E-19` records unchanged tmux session identity across the restart boundary.
- The post-restart prompt relays to Discord.
- The target mailbox drains to idle with no cancel token, queue, inflight row,
  stale thread proof, relay-dead state, desync, or orphaned callback target.
- The wrapped driver exits cleanly and reports zero failed scenarios.

The existing E2E driver refuses destructive restart steps when unrelated live
turns are active outside the tested E2E cell. Do not pass
`--hard-reset-session-each` for this smoke; preserving the live TUI session is
the invariant under test.
