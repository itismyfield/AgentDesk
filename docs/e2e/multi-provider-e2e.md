# Multi-Provider Discord E2E

The cell-based E2E harness exercises every (provider, runtime) cell against a
dedicated Discord channel. The legacy single-pair smoke against
`adk-dashboard-e2e` is being archived in favour of this matrix.

**Live scenarios are manual execution specifications, not CI-executed coverage.**
The [scenario execution contract](../../tests/e2e/tui_relay/scenarios/README.md)
defines the live prerequisites, manual commands, fixture/known-gap meanings and
the conditional automatic post-deploy subset (`E-1`, `E-35`, `E-50`, `E-51`).
CI inventories live rows and executes the two local fixture rows.

## Cells

| Cell           | Provider | Runtime    | Worker agent            | Channel                  |
| -------------- | -------- | ---------- | ----------------------- | ------------------------ |
| `claude-pipe`  | claude   | `pipe`     | `adk-claude-pipe-e2e`   | `adk-claude-pipe-e2e`    |
| `claude-tui`   | claude   | `tui`      | `adk-claude-tui-2-e2e`  | `adk-claude-tui-2-e2e`   |
| `codex-pipe`   | codex    | `pipe`     | `adk-codex-pipe-e2e`    | `adk-codex-pipe-e2e`     |
| `codex-tui`    | codex    | `tui`      | `adk-codex-tui-e2e`     | `adk-codex-tui-e2e`      |

The four worker channels above, the `claude-herdr` worker channel
`adk-claude-tui-e2e` (Herdr-mapped) and the orchestrator channel
`adk-e2e-orchestrator` (six channels total) all live under the dedicated
`ADK E2E` Discord category. Channel IDs are looked up from `agentdesk.yaml`
— no hard-coded ids in scripts or docs.

## Scenario schema

`tests/e2e/tui_relay/scenarios/E-*.yaml` files carry a `cells:` list naming the
cells they apply to. The driver runs a scenario only when the requested cell
is in that list.

```yaml
id: E-1
agent_mode: real_live
coverage_class: live
# Manual execution specification; not run in CI.
cells: [claude-pipe, claude-tui, codex-pipe, codex-tui]
steps:
  - send_prompt: ...
assertions:
  - ...
```

Every scenario must declare `agent_mode`:

| `agent_mode` | Use when | Relay example |
| --- | --- | --- |
| `none` | No AgentDesk agent/provider is contacted; the harness checks local state-machine or replay fixtures only. | `E-24`/`E-25` local fixture replay. These can prove parser/finalization contracts, but cannot satisfy live relay gates. |
| `controlled` | A deterministic test agent, fake provider, scripted responder, or controlled runtime hook owns completion. | Future fake-provider / production-parser injection. A deterministic forced quiescence timeout would also land here once a runtime hook exists. |
| `real_live` | The scenario sends work through a real provider/cell and live Discord/runtime boundary. | Normal relay matrix prompts, direct TUI input, cross-channel `E-11`, restart-guard `E-17`, the `E-16` completion-quiescence release window, and post-deploy relay continuity smoke. |

Every scenario must also declare `coverage_class`:

| `coverage_class` | Use when | Gate behavior |
| --- | --- | --- |
| `live` | The row exercises the live Discord/runtime/provider path, or a controlled production-parser/hook path that is allowed to count as live coverage while `agent_mode` records whether a real provider was contacted. | Can satisfy `--required-coverage-class live` if its actual class remains live. |
| `fixture` | The row replays deterministic local data without Discord, tmux, dcserver, or provider contact. | Useful parser/finalization coverage, but fails `--required-coverage-class live`. |
| `unsupported-known-gap` | The row is a machine-readable gap placeholder with `skip_reason` and acceptance criteria. | Excluded from green live-gate claims and fails `--required-coverage-class live`. |

`agent_mode` and `coverage_class` are independent axes. A future safe
production-parser injection can be `agent_mode: controlled` and
`coverage_class: live`; a local replay remains `agent_mode: none` and
`coverage_class: fixture`. Gate runners can pass `--required-coverage-class live`
to make selected fixture or known-gap rows fail instead of being counted as live
confidence.

The driver validates metadata against the scenario shape. A scenario that
declares `none` but sends a prompt, or declares `real_live` while switching to a
fixture execution lane, fails before it can produce a misleading green report.
Gate runners can also pass `--required-agent-mode controlled` or
`--required-agent-mode real_live`; selected scenarios below that lane fail
instead of silently downgrading. Machine-readable reports include
`agent_mode`, `agent_mode_actual`, `provider_identity`, `real_provider_contacted`,
`coverage_class`, `coverage_class_actual`, `run_id`, and `failure_attribution`
on each scenario.

A `wait_for_discord_text` step (or an `E-36` request) with `relay_author: provider`
matches only bodies posted by the cell's provider bot, and `provider_text_present`
applies the same author check to the final presence assertion. Notify and
announce bot posts that quote a marker therefore never count as delivery.
`E-4` scopes its direct-input body; `E-1`, `E-2`, `E-36`, `E-50` and `E-51` scope
every turn marker, on legacy and Herdr cells.

Provider-specific scenarios (`E-6` claude `/compact`, `E-7` codex `/compact`)
narrow the list. TUI-keystroke-only scenarios (`E-4`, `E-10`, `E-12`) include
only the two `*-tui` cells. `E-13` covers the Claude Code pipe/headless
scheduled wakeup/monitor path: the first turn must call `ScheduleWakeup`, and
the automatic wake turn must relay `[E2E:E13:WAKE]` to Discord. It intentionally
runs only on `claude-pipe`; `claude-tui` keeps normal relay
coverage because this matrix does not create a persistent Claude Code wake
session for that cell. `E-16` and `E-17` close the #2935 live regression gap
(#3797). `E-16` is an executable `claude-tui` `real_live`/`live` scenario: it
delivers a first response and then sends a second prompt immediately at the
first response marker — the completion-quiescence release window after delivery
(`TUI_COMPLETION_QUIESCENCE_TIMEOUT`, the 3s const in
`src/services/discord/tmux.rs`) — and proves the second prompt starts and replies
without an indefinite queued placeholder. Deterministically *forcing* the 3s gate
would need a production hook, so `E-16` exercises the natural release window on
the live relay path; the always-on post-scenario idle invariant
(`assert_cell_idle` → `post_scenario_idle.mailbox_idle_evidence`) captures the
`/api/health/detail` proof that the tested mailbox returned to idle
(`agent_turn_status=idle`, `queue_depth=0`, no cancel token, no inflight state, no
active user message, no stale queued placeholder/pending callback). `E-17` is an
orchestrator-owned restart-guard scenario (`cells: []`,
`orchestration: foreign_active_restart_guard`, run by
`run_multi_provider_matrix.py`): it holds a foreign `claude-tui` turn active via
the provider-hold witness, waits until `/api/health/detail` surfaces that mailbox
busy, then attempts a `codex-tui` destructive restart and proves the harness
refuses before restart while naming the foreign mailbox evidence. SAFETY: `E-17`
drives only the restart-refusal guard (`_guard_no_foreign_active_turns`) and never
invokes the real restart/kickstart, so even a regressed guard cannot harm
unrelated active production work; the foreign hold is released (`cancel_turn`) and
both channels asserted idle on teardown. `E-18` is an executable
but destructive-gated `cancel_turn` stop-mid-turn scenario for relay-backed
pipe/TUI cells; it uses the provider hold witness primitive to exercise the
relay-backed stop/cancel lifecycle.
`E-19` captures tmux pane identity across dcserver restart for TUI cells and
requires the post-restart turn to recall a pre-restart secret token. `E-20`
uses same-session near-concurrent prompt fan-out to pressure dispatch
serialization while asserting both markers arrive once. `E-21` covers TUI
direct input with an actual `C-u` key sequence: a stale draft marker is typed,
cleared, and then the real prompt must relay with a complete head-to-tail body
while the stale marker and terminal controls stay absent. `E-11`
(cross-cell concurrency) and `E-17` (foreign-active restart guard) are both
`cells: []` — the orchestrator owns those scenarios.
`E-22` covers tool-use to text completeness for Claude relay-backed pipe/TUI
cells by waiting for a current-turn provider hold witness after a real tool call
and then asserting the post-tool body remains complete. `E-23` is the dedicated
Claude live premature-completion guard: completion chrome must exist and must
not appear before the final body marker. `E-24` and `E-25` are local fixture scenarios:
they run through the YAML harness without Discord, tmux, or live dcserver state.
`E-24` replays an exact `CronCreate`/`Background` task notification and asserts
result-text relay plus clean finalization. `E-25` replays modern Codex
`response_item` plus `event_msg/task_complete` frames and asserts final text
relay, task-complete finalization, follow-up readiness, and no stale
health/queue degradation. `E-26` through `E-28` are explicit
`unsupported-known-gap` rows for live coverage not yet implemented: exact
CronCreate live creation, Codex modern-schema production-parser/live stream,
and Codex live tool-command coverage.

`E-35` preserves the headless `E-1` and adds one normal Discord short-response
turn. It passes only when the newly observed outbound Discord response ID has
exactly one authoritative receipt and that receipt has a same-generation
covering frontier in the local atomic sidecar record. Inbound prompt IDs, old
frontiers, completed-turn ledgers, leases, and in-memory offsets cannot pass it.
For production-written records, that receipt also binds the provider, owner
filename, source and delivery channels, and range; requires nonempty tmux session
and turn nonce identities; and was current-generation at writer-lock time. On
this probed short terminal path, reaching the writer follows that turn's
successful lease commit. E-35 therefore detects both a frontier-only partial
write and a normal-to-headless `send_prompt` regression that omits the receipt.
The post-deploy invocation disables reset/force-cancel and rechecks standby,
target-idle/queue state, and its cell lease before setup and immediately before
the normal prompt. Busy or dirty residue becomes `unevaluable` without cleanup.
The prompt-time recheck narrows the nominal gate-return-to-prompt window from
368 seconds to 0 seconds, and the last-mailbox-snapshot-to-prompt window from
373 seconds to 5 seconds. It does not close the TOCTOU: local filesystem and
scheduling have no hard bound, and a turn can start after the recheck and before
send.

The component timeouts total **1,422 seconds**: two safety gates contribute
`2 × (5 + 5 + 5) = 30`; setup POST/sleep and pre-send fetch contribute
`180 + 8 + 180`; prompt POST/sleep, response wait, and durable poll contribute
`180 + 3 + 240 + 15`; two final refetches plus settle contribute
`2 × 180 + 1 = 361`; post-scenario idle and teardown contribute `45 + 180`.
The single **900-second operational cutoff** covers the gate, lease,
setup/send/fetch HTTP, response wait, record poll, final refetch, idle check, and
teardown, but it is deliberately not the component sum. Expiry is a fail-open
finding after `DEPLOY_OK`.

#5264 is a hard prerequisite only for default `claude-tui` live acceptance.
`claude-pipe` is allowed as `partial coverage`, but is not S8 TUI-surface
evidence. The minimum positive claim is: 이 노드의 이번 배포 세대에서 E-35의
한 short-response normal Discord turn이 confirmed durable record funnel을
통과해, 그 새 Discord response에 결속된 exact receipt와 covering frontier를
atomic sidecar record에 남겼다. Here “이번 배포 세대” means the local
post-`DEPLOY_OK` probe transaction; it does not bind a peer, deployed commit SHA,
or poll-time-current tmux generation. The proof also does not cover power-loss
durability, every provider/call site, long/fallback/recovery/headless paths,
completed-ledger persistence, lease acquisition/clear/restart reconciliation,
continuous health, or S8 rollout. E-35 PASS does not distinguish the spawn writer
from the `restore.rs` old-generation adoption writer because the durable record
does not encode which writer produced it.

The durable funnel depends on exactly **five symbols** arranged as **four
independent enforcement stages**:

1. `shadow_mirror_delivered_frontier_inner` rejects a zero generation and an
   empty or invalid source range.
2. `exact_receipt_from_inflight` binds the current inflight/source identity to
   the receipt and current generation.
3. `write_confirmed_delivery_at_with_lock_authority` enforces the receipt/frontier
   coupling. Inside this delivery-writer stage it calls the fifth symbol,
   `ExactJsonlSourceIdentity::is_authoritative`, as a subordinate predicate that
   rejects zero or incomplete source identity; that predicate is not a fifth
   independent enforcement stage.
4. `write_confirmed_frontier_guarded_at_with_lock_authority` holds writer-lock
   authority and rereads the current generation to reject a stale writer.

When `.generation` remains absent, ledger-only state cannot satisfy the
receipt/frontier conjunct. Adoption can create the marker first and allow the
redundant current-generation defenses to pass together, so without #5264 default
`claude-tui` acceptance can be a nondeterministic green. An E-35 PASS also includes
post-scenario mailbox/queue idle evidence. The safety gate depends directly on
private `lease._read_lease` because no public read API exists; an underscore-
function refactor can leave E-35 fail-closed and `unevaluable` until the coupling
is updated.

E-35 alone supplies the durable-record provenance above. The full post-deploy
E-1 + E-35 sequence additionally checks that the existing headless relay
round-trip still works; it does not turn the headless E-1 response into S8 TUI
durability evidence or broaden E-35 to the excluded paths.

## #2943 Scenario Coverage And Gaps

Covered P0/P1 backlog items:

- `tool_use->text completeness`: `E-22`, Claude relay-backed pipe/TUI cells.
- `stop-mid-turn`: `E-18`, relay-backed pipe/TUI cells, destructive-gated.
- `cron self-prompt relay`: `E-13` covers the available Claude Code
  `ScheduleWakeup`/monitor self-prompt path on `claude-pipe`; `E-24` adds the
  deterministic local `CronCreate`/`Background` fixture primitive.
- `restart context continuity`: `E-19`, TUI cells, with tmux identity and
  pre-restart token recall.
- `premature-completion guard`: `E-23`, Claude cells with live tool execution.
- `followup-during-busy` / same-session pressure: `E-20`, all cells.
- `direct-input body_complete + control-byte strip`: `E-21`, TUI cells.
- `codex modern schema turn completeness + follow-up readiness`: `E-25`,
  deterministic local fixture replay for Codex cells.
- `completion-quiescence release after delivery` (#2935): `E-16`, `claude-tui`,
  immediate follow-up after the first response marker plus the idle invariant.
- `foreign-active destructive restart refusal` (#2935): `E-17`, orchestrator-owned
  hold of a foreign `claude-tui` mailbox while a `codex-tui` restart is attempted
  (guard-only, no real restart).
- `/compact recap not relayed as input`: `E-6`, Claude cells. The oracle is the
  two continuation openings `injected_prompt_policy.rs` classifies; no bot post may
  quote either, so a relayed recap or a direct-input notice for it fails.
- `multi-unit characters across chunk boundaries`: `E-15`'s second (UTF-16) turn,
  all cells. Every post stays within 2000 UTF-16 units with no U+FFFD or lone
  surrogate, and the emoji payload must reach at least two relay bodies.

Remaining exact gaps:

- Exact `CronCreate` live creation is still not claimed by `E-24`; `E-26` is
  the `unsupported-known-gap` row for the live CronCreate creation/background
  notification/finalization lane.
- Codex modern-schema live or production-parser injection is still not claimed
  by `E-25`; `E-27` is the `unsupported-known-gap` row for the safe parser
  injection or real runtime stream lane.
- Live tool-command coverage is not claimed for `codex-pipe` or `codex-tui`;
  `E-28` is the `unsupported-known-gap` row because those worker roles cannot
  execute shell/tool commands in the current E2E contract.

## Driver

Full matrix baseline:

```bash
scripts/e2e/run_multi_provider_matrix.py --twice
```

The matrix runner looks up channel ids from
`~/.adk/release/config/agentdesk.yaml`, runs all cells sequentially, and writes
`matrix.json` plus one `report.<cell>.json` per pass/cell under
`out/e2e/tui_relay/matrix-<run-id>/`.

Single cell:

```bash
scripts/e2e/run_tui_relay.py \
    --cell claude-pipe \
    --channel-id <id of adk-claude-pipe-e2e> \
    [--base-url http://127.0.0.1:8791] \
    [--scenarios tests/e2e/tui_relay/scenarios] \
    [--filter E-1,E-5] \
    [--output out/e2e/tui_relay/<run-id>] \
    [--required-coverage-class live] \
    [--dry-run]
```

The driver writes `report.<cell>.json` so an orchestrator that aims all 4 cells
at one `--output` directory does not overwrite sibling reports. Per-cell lease
files (`/tmp/agentdesk-e2e-relay.<cell>.lease`) let cells run in parallel from
separate operator sessions. The top-level report includes `agent_mode_totals`
`coverage_class_totals`, `coverage_class_violations`, and
`real_provider_contacted`; individual scenario rows include the provider,
runtime, worker agent, channel id, run id, raw failure attribution,
`coverage_class_actual`, and whether a real provider was contacted.

Cell report schema version 2 adds feature accounting while retaining the existing
`pass`/`fail`/`skipped` scenario totals. Consumers must ignore unknown fields and
must not sum every value in `totals`: the additional counts describe coverage.
For the pipe/TUI driver, `AGENTDESK_E2E_FEATURES` is an explicit operator attestation
of the target's enabled features. Set it to an empty string to attest that all
gated features are disabled, or to a comma-separated list such as
`two_message_panel,e2e_discord_control`. An unset, unreadable or malformed list
is unevaluable when a selected scenario needs feature evidence; it fails before
scenario actions and stops the phase. This declaration is not a runtime flag probe.

Disabled E32–34 remain `status: skipped` for older consumers and also report
`execution_status: dormant`, `dormant_reason`, `required_features` and
`disabled_features`. Required observed coverage still fails for dormant scenarios.
Each skipped E9/E15 assertion reports `status: skipped`, `passed: null`, its
`required_feature` and reason. Executed assertions alone contribute to the
per-scenario `assertion_totals.pass`/`fail` counts; `skipped`, `unevaluable` and
`not_applicable` are separate. Legacy assertion rows containing only `passed`
remain readable. A scenario can pass with `assertion_coverage: partial`; this
means its executed checks passed, not that the gated capability was covered.

`totals.executed`, `dormant` and `unevaluable` count disjoint execution outcomes;
an execution interrupted by unavailable evidence counts as unevaluable. Legacy
`fail` includes unevaluable and required-gate failures; legacy `skipped` includes
dormant. `totals.skipped_assertions` counts only recorded skipped checks, not
checks inside scenarios that never ran. Dry runs contribute no executed passes.
The matrix forwards these totals and reports partial coverage when either dormant
scenarios or skipped assertions remain, even if all executed scenarios passed.

Every prompt send appends a `prompt_sends` row (`send_state: attempted`) before it
leaves and fills `message_id` with `send_state: acknowledged` when the response
returns; provider contact is claimed only after that acknowledgement. When the
phase deadline expires, including inside a failure handler's teardown or inside
one of E-18's Herdr text-stop subcases, the report row still carries those rows
and the contact fact.

Post-deploy relay continuity uses a narrower operational wrapper around the
same driver:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py --self-check
python3 scripts/e2e/post_deploy_relay_continuity.py --fixture pass
python3 scripts/e2e/post_deploy_relay_continuity.py \
    --cell claude-tui \
    --dry-run \
    --deploy-command 'AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 scripts/deploy-release.sh'
```

The live form adds `--confirm-live` and runs only TUI cells (`claude-tui` or
`codex-tui`) through `E-9,E-19` with the release deploy command as the restart
boundary. See
[`docs/runbooks/post-deploy-relay-continuity-smoke.md`](../runbooks/post-deploy-relay-continuity-smoke.md)
for the full runbook and pass/fail evidence.

Auto-queue harnesses use the same lane vocabulary. The sandbox
auto-queue preflight defaults to `agent_mode=none` or a controlled finalizer
because it validates generate/dispatch/slot/entry/run truth, not LLM behavior.

Destructive steps (`restart_dcserver`, `kill_pane`, `send_keys_no_enter`,
`cancel_turn`) are gated by both `--allow-destructive` and
`AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1`.
Before a destructive restart, the driver now fails closed if
`/api/health/detail.mailboxes` shows any foreign channel/provider with active
mailbox state, cancel token, inflight state, queue depth, recovery/finalizing
state, pending Discord callback, stale thread proof, or relay stall state.

### Mid-turn provider SIGKILL (`--midturn-kill`)

`run_tui_relay.py --cell codex-tui|claude-tui --midturn-kill --allow-destructive`
(with `AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1`) runs only scenario `MK-1` from
`scripts/e2e/tui_relay/midturn_kill.py`. It is not a YAML scenario, so default
and `--filter` sweeps never select it, and YAML cannot use its steps.

- Target: the cell's provider process only. The harness follows the runtime
  session record (`sessions/*-<session>.owner`, `runtime-kind`, launch script
  `AGENTDESK_BINDING_CONTEXT`) to a binding whose `tmux_session`, `channel_id`,
  provider and owner match the run, then the session's single live pane. A
  wrapper pane process (Codex `node`) is never signalled; its one native
  `codex` child is. The agent must end in `-e2e`; Herdr and pipe cells are
  refused. dcserver, the tmux server and the harness itself are protected.
- Before any send, the newest runtime binding of the cell's session must name
  `--channel-id` and the target mailbox must have no busy reason. The mode
  turns off the pre-scenario force-cancel reset, which tears down an idle tmux
  session.
- Hold: the E2E worker roles forbid shell commands, so Claude keeps the turn
  running with a fixed chain of `Read` calls after the early marker. Codex has
  only the shell command; on `codex-tui` it answers the early marker as its
  final answer, and the run fails before any kill (`provider not observed
  mid-turn`).
- Kill: the identity is first read once the hold prompt's early marker is in
  the provider inflight row with a tool running (`provider_hold_observed`) and
  the mailbox is busy. Right before `SIGKILL` it is read again; any change of
  pid, start time, cmdline, parent, pane or binding refuses without a signal.
  The turn's inflight row is also read again: it must still be this turn, hold
  the early marker without the final one, and have no committed terminal
  delivery. A delivery-only busy mailbox does not license the kill.
- Verdicts (`midturn_kill.capabilities` in the report): `provider_killed`,
  `turn_settled` (two polls in a row with a readable idle mailbox, empty queue
  files and no live processing status as any message's current text, within
  300 s; a first idle poll at the deadline is not enough),
  `pre_kill_output_delivered` (the early marker in a provider-bot body within
  30 s of settlement; otherwise the run fails before the follow-up is sent),
  `next_turn_new_provider` (a fresh marker answered by the provider bot after
  the follow-up, by a process other than the killed pid/start time). Each
  failing capability fails its own step, so the run cannot pass with one.

Health waits parse the JSON payload, not just HTTP 2xx. By default a run is
considered ready only when the health body is healthy (`status: healthy`, `ok`
not false, `fully_recovered` not false, and no unallowed degraded reasons). A
`restart_dcserver` step may declare `require_status` and
`allowed_degraded_reasons` with the same meaning as `assert_health`; they relax
only the status and the `ok`/`degraded` flags, never `fully_recovered` or an
unlisted degraded reason. A failed `launchctl kickstart` fails the step. Other
bodies keep polling until timeout and then fail with the last health summary.

`/api/health/detail.boot_incarnation` is a random id generated once per dcserver
process (the public body does not carry it). A restart step reads it before the
kickstart and passes only when health returns with a different value; an
unchanged value fails the step as `restart not observed`, and a missing value is
unevaluable before any kickstart. Herdr restarts and the observe-only `verify`
use the same comparison; `verify` falls back to the uptime inference only when
either side lacks the field.

Unknown channels, including other cells, still fail default strict readiness.
Public health probes request `/api/health/detail` for an Unknown waiver only
when a `turn_presence_unknown` reason actually matches an explicit allowlist.
That one detail body supplies the entire readiness decision; public and detail
facts are never combined. Waiving Unknown requires complete, matching group and
channel evidence proving every Unknown channel is outside the tested target.
The target channel is never waived, regardless of provider, and missing,
malformed, ambiguous, or inconsistent evidence is never waived. Herdr applies
the same attribution check to its existing detail probe. For public probes,
other explicit reason waivers retain public-only readiness; counter and idle
checks can independently require detail.

After every executed scenario, the driver also asserts the tested cell's
mailbox is idle via `/api/health/detail`: `agent_turn_status=idle`,
`queue_depth=0`, no cancel token, no inflight state, no active user message, no
pending Discord callback, and no stale/relay-stall proof. It also checks the
tested provider/channel's on-disk pending queue and queued placeholder files
under the runtime root are empty.

Scenario steps can also run an explicit `assert_health` probe. The probe reads
`/api/health` and, for counter caps or a matching explicit Unknown waiver,
`/api/health/detail`; it can
require `status: healthy`, forbid degraded reason substrings such as
`global_active_counter_out_of_bounds`, and cap `global_active` /
`global_finalizing`. Destructive scenarios use this after the tested turn has
settled so counter underflow and stuck-finalizing regressions are visible in the
E2E report.

Control-flow steps include `cancel_turn` (POSTs
`/api/turns/<channel_id>/cancel?force=<bool>` and remains destructive-gated),
`send_prompts_concurrent` (starts multiple prompt dispatches without the normal
per-step sleep), `capture_session_identity`, and `assert_session_preserved`
(compares tmux session name, pane ids, pane pids, and cwd after restart).
`send_keys_sequence` sends explicit tmux key args such as `C-u` and `C-m`.
By default it uses one tmux `send-keys` call; scenarios can set
`key_interval_s` to send the same ordered key list one key at a time when a TUI
composer must process a control key before Enter. `E-21` uses that mode to
cover direct TUI input where a stale draft is cleared by a real control key
before the submitted prompt is relayed.

Observation assertions now include negative and edit-aware primitives:
`raw_text_absent`, `marker_absent`, `chrome_count`,
`completion_chrome_after_body`, `raw_message_count_between_markers`,
`body_not_overwritten`, and `no_suppressed_label_chrome`. The observation
window updates messages by Discord id during final re-fetches, so assertions see
the final edited body rather than only the first placeholder/chrome body.
`completion_chrome_after_body` checks ordering by default and can set
`required: true` when a scenario wants to fail on missing completion chrome.
It judges ordering on the channel's final state: completion chrome that a later
re-fetch no longer returns (a panel moved below the body and deleted) is ignored,
and while surviving chrome is still above the body the runner re-fetches for up
to 35s after the body was first seen, covering the O-channel panel move window.
Latency budgets use the first prompt timestamp to make
`relay_latency_within` meaningful even for one-response scenarios. If a prompt
timestamp exists but no later timestamped relay body is observed, the assertion
fails instead of silently no-oping. Current baselines are deliberately loose:
simple/turn-separation cases use 240s, restart/compact/long-response cases use
300s, scheduled wakeup uses 360s, and the tmux-kill/cancel guards use 180s to
bound the prompt-to-first-relay phase before the destructive step. Raw-message
budgets count status chrome but exclude harness-sent control/prompt messages
unless a scenario opts into `include_our_send`.

## Orchestrator (`adk-e2e-orchestrator`)

The orchestrator agent owns the dedicated channel and parses natural-language
commands:

- `전체 e2e 시작` → runs all 4 cells sequentially (claude-pipe → claude-tui →
  codex-pipe → codex-tui).
- `claude의 tui 테스트 시작`, `codex의 pipe 테스트 시작`, ... → single cell.
- `claude-pipe 시작` (explicit cell name) → single cell.

The orchestrator creates a result thread on the orchestrator channel (named
`<KST-ISO-short>-<cell>` for single-cell or `<KST-ISO-short>-all-e2e` for
matrix), runs `scripts/e2e/run_multi_provider_matrix.py` or the single-cell
driver from `/Users/itismyfield/.adk/release/workspaces/agentdesk`, and writes
a one-line status per cell into the thread.

The orchestrator drives worker channels from outside the workers. Workers must
not run `run_tui_relay.py` against their own channel; doing so makes their
mailbox busy and can recursively start nested E2E runs.

## Provisioning (cold start)

Initial setup uses the announce-bot-backed CLI added in PR 2. The order
below matters — every step depends on the previous one being on disk in
release.

1. Confirm PR 1 and PR 2 are merged AND staged in release: `agentdesk
   discord --help` must list `category-create`, `channel-create`, and
   `thread-create`. If not, run `scripts/deploy-release.sh` first.
   `thread-create` scans active and archived threads and serializes concurrent
   invocations by parent/name. Forum and media parents require `--message`;
   pass repeatable `--tag-id <ID>` values when the parent requires or accepts
   forum tags. News parents create announcement (`NewsThread`) threads.
2. Confirm the operator's Obsidian vault has the 7 prompt files
   (`adk-claude-pipe-e2e.prompt.md` ... `adk-e2e-orchestrator.prompt.md`)
   under `~/ObsidianVault/RemoteVault/adk-config/agents/`. Re-run
   `scripts/deploy-release.sh` if the release `config/agents/` mirror is
   missing them.
3. Provision the Discord side:

   ```bash
   scripts/setup-multi-provider-e2e.sh           # live
   scripts/setup-multi-provider-e2e.sh --dry-run # preview the CLI invocations
   ```

   The script is idempotent (list-then-create against the parent
   guild/category) and emits one JSON line per resource.
4. Paste each printed channel id into the matching `PLACEHOLDER_ADK_*`
   slot in `~/.adk/release/config/agentdesk.yaml`. Refer to
   `agentdesk.example.yaml` for the entry shape.
5. Restart dcserver so the new agents come online:

   ```bash
   agentdesk restart-dcserver
   ```
6. Smoke a single cell against its newly bound channel:

   ```bash
   scripts/e2e/run_tui_relay.py \
       --cell claude-pipe \
       --channel-id <id from step 4>
   ```

   Expect `report.claude-pipe.json` under `out/e2e/tui_relay/claude-pipe/<run-id>/`
   with no failed scenarios.

## Channel id lookup

After step 4, fetch ids from the running config when scripts/skills need
them:

```bash
yq '.agents[] | select(.id == "adk-claude-pipe-e2e") | .channels.claude.id' \
    ~/.adk/release/config/agentdesk.yaml
```

Substitute the cell's worker agent id (`adk-<cell>-e2e`, except
`claude-tui`, which uses `adk-claude-tui-2-e2e`). The
orchestrator entry follows the same shape with id `adk-e2e-orchestrator`.

## Skill

Use the `agentdesk-relay-e2e` skill for E2E debugging requests. It documents
the cell-based command shapes and the diagnostics paths to consult first.

## Legacy

The legacy `adk-dashboard-e2e` single-pair smoke was archived alongside this
rollout. The two shell wrappers it depended on were removed in the
multi-provider-e2e migration's final PR; consult `git log` if you need the
historical surface. New work uses the per-cell driver and the orchestrator
described above.
