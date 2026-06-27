# Post-Deploy Relay Continuity Smoke

This runbook covers issue #3729: prove that a live TUI relay survives the
release deploy or restart boundary. A server-up check is not enough; the smoke
must show that tmux survives, the restarted dcserver reattaches relay ownership,
and post-restart output reaches Discord.

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

## Live Dry Run

Use dry run before a release deploy to validate config, channel id lookup, and
the exact command that will be delegated to the TUI relay driver:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py \
  --cell claude-tui \
  --dry-run \
  --deploy-command 'AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 scripts/deploy-release.sh --skip-review'
```

The script resolves the `adk-<cell>-e2e` channel id from
`~/.adk/release/config/agentdesk.yaml` unless `--channel-id` is supplied.

## Live Smoke

Run this only against the dedicated TUI E2E worker channels:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py \
  --cell claude-tui \
  --confirm-live \
  --deploy-command 'AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 scripts/deploy-release.sh --skip-review'
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
