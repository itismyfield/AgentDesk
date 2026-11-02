# TUI relay scenario execution contract

**`coverage_class: live` YAML files are manual execution specifications and are
not executed in CI.** Their presence is not evidence that a live relay test ran.
No dedicated live E2E CI lane is provided. Release deployment conditionally runs
the small operational smoke subset listed below; it does not execute this whole
directory. Reclassifying live scenarios as fixtures is separate work.

## Classification and CI evidence

The current inventory is **41 active YAML files: 36 live, 2 fixture, 3 unsupported**.
The [original audit in #6026](https://github.com/itismyfield/AgentDesk/issues/6026)
counted 35 files, including 30 live specifications; those counts are historical.
The six live specifications added since (`E-37`, `E-50`–`E-54`), retirement of
`E-32`–`E-34`, and addition of `E-60`–`E-62` account for the difference. Retired
manual specifications are archived in `retired/` and excluded from the active
top-level scan; fixture and known-gap counts are unchanged.

| `coverage_class` | Meaning | CI execution |
| --- | --- | --- |
| `live` | A specification requiring live runtime/provider boundaries; all current rows declare `agent_mode: real_live`. An operator must select and run it with the required environment. | The census inventories and excludes these rows; it does not execute their steps. |
| `fixture` | Deterministic local replay without Discord, tmux, dcserver or provider contact. `E-24` and `E-25` cover parser/finalization behavior. | The census replays these two fixtures through production parsers. A fixture PASS cannot satisfy live relay acceptance. |
| `unsupported-known-gap` | An unimplemented boundary with acceptance criteria and a machine-readable `skip_reason` (`E-26`, `E-27`, `E-28`). | Inventoried and excluded, never counted as passing live coverage. |

`agent_mode` describes provider contact (`none`, `controlled`, `real_live`);
`coverage_class` describes the evidence class, not a CI schedule. Metadata such
as `--required-agent-mode real_live` and `--required-coverage-class live` checks
the selected run's evidence; it does not arrange a run.

The `rust_tests` workers run
`services::discord::tui_prompt_relay::tests::scenario_census_e2e` in the single nextest run, and
`rust_build` requires it on that run's list
([`.github/workflows/ci-pr.yml:718`](../../../../.github/workflows/ci-pr.yml#L718)).
Its independent pins remain `CENSUS_FIXTURE=2`, `CENSUS_LIVE=36`, and
`CENSUS_UNSUPPORTED=3`
([`scenario_census_e2e.rs:30`](../../../../src/services/discord/tui_prompt_relay/tests/scenario_census_e2e.rs#L30)).
Only `fixture` rows are runnable there: `is_runnable` (`:106`) selects inputs for
`runnable_scenarios_replay_their_declared_markers_through_production_parsers`
(`:314`). Python harness unit tests are wired under the
`TUI relay e2e harness unit tests (#5065/#5997)` guard in
[`scripts/ci-script-checks.sh:451`](../../../../scripts/ci-script-checks.sh#L451);
mocked/local checks do not execute the live specifications.

## Why live specifications do not run in CI

The issue's audit identified seven prerequisites that the census job does not
provision:

1. A running dcserver exposing `/api/health` and `/api/health/detail` with mailbox
   evidence. Discord POSTs go through that server; Discord credentials belong
   to the server, not the Python runner.
2. `tmux` on the runner for pipe/TUI session inspection and key injection.
3. Authenticated real model sessions for the original four-cell matrix:
   `claude-pipe`, `claude-tui`, `codex-pipe`, `codex-tui`. Later Herdr scenarios
   additionally need their declared backend and isolated-server prerequisites.
4. Dedicated Claude/Codex E2E channels bound to the tested workers, plus a
   thread channel for `requires_thread_channel` scenarios.
5. Isolation from unrelated work. Live rows declare `isolation: required`;
   cross-cell `E-11` refuses dispatch when foreign live mailbox/session state
   makes the run unsafe.
6. A separate wall-clock budget. The original audit counted about 3.3 hours of
   declared waits, not measured elapsed time. The census runs inside the
   `rust_tests` workers' 75-minute timeout
   (`ci-pr.yml:741`); it is not a live-matrix time budget.
7. A disposable environment for destructive cases, including pane termination
   and dcserver restart. These require explicit destructive opt-in.

## Manual execution

Run from the repository root with Python 3 and PyYAML installed. Prepare the
dedicated worker, channel, authenticated provider session, server and isolated
runtime directory first. The variables below must identify that E2E environment:

```bash
python3 scripts/e2e/run_tui_relay.py \
  --base-url "$E2E_BASE_URL" \
  --cell claude-tui --channel-id "$E2E_CHANNEL_ID" \
  --queue-runtime-root "$E2E_RUNTIME_ROOT" \
  --scenarios tests/e2e/tui_relay/scenarios --filter E-1 \
  --required-agent-mode real_live --required-coverage-class live \
  --output out/e2e/tui_relay/manual-e1
```

Add `--dry-run` to inspect the plan first; a dry run is not execution evidence.
`--filter` uses exact comma-separated IDs and `cells` must include the requested
cell. The default per-scenario reset cancels inflight work and truncates relay
queues, so the explicit runtime root and dedicated channels matter. Use
`--no-reset-before-each` when the selected scenario requires session continuity.
Destructive scenarios require both `--allow-destructive` and
`AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1`; restart scenarios also need an appropriate
`--restart-script`. Restart steps do not name a server: the run restarts the one
`--base-url` points at (port 8791 release, 8797 dev), or `--restart-target-override`
(`AGENTDESK_E2E_RESTART_TARGET`) on other ports, falling back to release. A
scenario target or override that disagrees with the port is refused before any
send. Thread scenarios need `--thread-channel-id`.

`E-11` and `E-17` have `cells: []` and are owned by the matrix orchestrator;
the single-cell driver does not execute them. For that matrix, pass the isolated
environment's own configuration and runtime root explicitly:

```bash
python3 scripts/e2e/run_multi_provider_matrix.py \
  --config "$E2E_CONFIG" --base-url "$E2E_BASE_URL" \
  --queue-runtime-root "$E2E_RUNTIME_ROOT" \
  --output out/e2e/tui_relay/manual-matrix --dry-run
```

Inspect each selected YAML's prerequisites and acceptance criteria before live
execution. In particular, `E-36` needs exclusive `--filter E-36`,
`--phase-deadline-s 3540`, and `--e36-intake-log`; unfiltered sweeps omit it.
Feature-gated scenarios also require an accurate `AGENTDESK_E2E_FEATURES`
attestation, as described in the [driver guide](../../../../docs/e2e/multi-provider-e2e.md#driver).
Inspect `report.<cell>.json` for executed, failed, skipped/dormant and unevaluable
outcomes and actual provider contact. A skipped row or zero executed scenarios
does not establish live coverage.

## Automatic post-deploy smoke is a subset

After `DEPLOY_OK`, the `smoke-disposition region` in
[`scripts/deploy-release.sh`](../../../../scripts/deploy-release.sh)
automatically calls `_run_post_deploy_functional_smoke`. Its default `full` scope
conditionally selects these live specifications:

| Scenario | Operational check and selection | Source function |
| --- | --- | --- |
| `E-1` | Headless relay round-trip on the configured relay cell (default `claude-tui`); not applicable when Herdr owns that channel. | `_post_deploy_smoke_check_relay_round_trip`, `--filter E-1` |
| `E-35` | Normal Discord short turn with exact durable receipt and covering frontier on the resolved relay cell; not applicable when Herdr owns that channel. | `_post_deploy_smoke_check_durable_record`, `--filter E-35` |
| `E-50` | Consecutive Claude turns, `!clear`, then a new-session turn; selected only for a `claude-*` relay cell. | `_post_deploy_smoke_check_turn_scenarios`, `_post_deploy_smoke_run_turn_scenario E-50` |
| `E-51` | Consecutive Codex turns, `!clear`, then a new-session turn; configured Codex cell defaults to `codex-tui`. | `_post_deploy_smoke_check_turn_scenarios`, `_post_deploy_smoke_run_turn_scenario E-51` |

Defaults are declared by `POST_DEPLOY_SMOKE_SCOPE`, `POST_DEPLOY_SMOKE_RELAY_CELL`
and `POST_DEPLOY_SMOKE_CODEX_TURNS_CELL` in the deploy script's initialization.
In `_run_post_deploy_functional_smoke`, `AGENTDESK_POST_DEPLOY_SMOKE_SCOPE=api` or
unconfirmed startup recovery leaves all four **not evaluated**.
`_post_deploy_smoke_check_relay_round_trip` records `relay E-1=skipped` for standby,
disabled gateway, missing channels and unproven/busy target state.
`_post_deploy_smoke_check_durable_record`'s `E-35 channel unavailable` check and
`_post_deploy_smoke_check_turn_scenarios`'s `E-1 resolved no relay channel` check
make `E-35/E-50/E-51` depend on the relay channel resolved during `E-1`.
`_post_deploy_smoke_run_turn_scenario` performs a fresh `post_deploy_turn_smoke.py preflight`.
Before the first scenario message the smoke records the relay cell's session host
(`relay cell=… path=herdr|tui`, from `session_hosts.herdr.channels`) and waits, at most
`AGENTDESK_POST_DEPLOY_SMOKE_GATEWAY_READY_S` seconds, until `/api/health/detail` shows a
recovered, connected gateway and a Discord read of the channel answers; not ready by then is
one FAIL and no scenario runs. On a Herdr-owned relay channel `E-1` (headless `turn/start`
is refused there by design) and `E-35` (Herdr has no durable receipt/frontier proof) are each
reported once as not applicable. `E-50`/`E-51` pass only when every marker body arrived
exactly once; a visible `!clear` refusal is a FAIL that quotes the reply. Each verdict line
carries `path=` and the per-step durations.
These conditions mean a successful deployment does not imply that any particular live scenario ran.
Before E-1, `cell-reset` acquires each configured legacy TUI E2E cell's existing driver lease and always inspects the turn lease; only for residue performs CAS release and `!clear`, preserving idle cells without residue (inflight without a lease needs only `!clear`).
A foreign driver lease skips that cell; reset failure blocks only that cell's scenarios and is reported without stopping the other cells.

The `smoke-disposition region` is fail-open after a healthy deploy;
`_run_post_deploy_functional_smoke` results need their own evidence.
[`post_deploy_turn_smoke.py`](../../../../scripts/e2e/post_deploy_turn_smoke.py)'s
`main` registers `cell-guard`, `cell-reset`, `cell-path`, `gateway-ready`, `reset-status`, `preflight` and `judge`,
while the deploy script invokes the scenario runner. The judge evaluates its report
and run-scoped dcserver log excerpt; it is not an additional live execution lane.

The separate [relay-continuity runbook](../../../../docs/runbooks/post-deploy-relay-continuity-smoke.md)
is operator-invoked: its wrapper runs `E-9,E-19` with explicit `--confirm-live`
and a deploy/restart command. Those scenarios are not part of the automatic
four-scenario smoke above. Neither path establishes CI coverage for the whole
live inventory.
