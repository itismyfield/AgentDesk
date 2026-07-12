# Multi-Provider Discord E2E

The cell-based E2E harness exercises every (provider, runtime) cell against a
dedicated Discord channel. The legacy single-pair smoke against
`adk-dashboard-e2e` is being archived in favour of this matrix.

## Cells

| Cell           | Provider | Runtime    | Worker agent            | Channel                  |
| -------------- | -------- | ---------- | ----------------------- | ------------------------ |
| `claude-pipe`  | claude   | `pipe`     | `adk-claude-pipe-e2e`   | `adk-claude-pipe-e2e`    |
| `claude-tui`   | claude   | `tui`      | `adk-claude-tui-e2e`    | `adk-claude-tui-e2e`     |
| `claude-e`     | claude   | `claude-e` | `adk-claude-e-e2e`      | `adk-claude-e-e2e`       |
| `codex-pipe`   | codex    | `pipe`     | `adk-codex-pipe-e2e`    | `adk-codex-pipe-e2e`     |
| `codex-tui`    | codex    | `tui`      | `adk-codex-tui-e2e`     | `adk-codex-tui-e2e`      |

The five worker channels above plus the orchestrator channel
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
cells: [claude-pipe, claude-tui, claude-e, codex-pipe, codex-tui]
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

Provider-specific scenarios (`E-6` claude `/compact`, `E-7` codex `/compact`)
narrow the list. TUI-keystroke-only scenarios (`E-4`, `E-10`, `E-12`) include
only the two `*-tui` cells. `E-13` covers the Claude Code pipe/headless
scheduled wakeup/monitor path: the first turn must call `ScheduleWakeup`, and
the automatic wake turn must relay `[E2E:E13:WAKE]` to Discord. It intentionally
runs only on `claude-pipe`; `claude-tui` and `claude-e` keep normal relay
coverage because this matrix does not create a persistent Claude Code wake
session for those cells. `E-16` and `E-17` close the #2935 live regression gap
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
pipe/TUI cells; it uses the provider hold witness primitive and omits
`claude-e` because the stop/cancel path under test is relay-backed lifecycle.
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
health/queue degradation. `E-26` through `E-29` are explicit
`unsupported-known-gap` rows for live coverage not yet implemented: exact
CronCreate live creation, Codex modern-schema production-parser/live stream,
Codex live tool-command coverage, and `claude-e` tool-use-to-text completeness.

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
- `tool_use->text completeness` for `claude-e` is not claimed by `E-22`; `E-29`
  is the `unsupported-known-gap` row until the harness has a live witness or
  fixture proving the same `any_tool_used=true` and
  `has_post_tool_text=false` inflight state before post-tool text.

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

The driver writes `report.<cell>.json` so an orchestrator that aims all 5 cells
at one `--output` directory does not overwrite sibling reports. Per-cell lease
files (`/tmp/agentdesk-e2e-relay.<cell>.lease`) let cells run in parallel from
separate operator sessions. The top-level report includes `agent_mode_totals`
`coverage_class_totals`, `coverage_class_violations`, and
`real_provider_contacted`; individual scenario rows include the provider,
runtime, worker agent, channel id, run id, raw failure attribution,
`coverage_class_actual`, and whether a real provider was contacted.

Post-deploy relay continuity uses a narrower operational wrapper around the
same driver:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py --self-check
python3 scripts/e2e/post_deploy_relay_continuity.py --fixture pass
python3 scripts/e2e/post_deploy_relay_continuity.py \
    --cell claude-tui \
    --dry-run \
    --deploy-command 'AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 scripts/deploy-release.sh --skip-review'
```

The live form adds `--confirm-live` and runs only TUI cells (`claude-tui` or
`codex-tui`) through `E-9,E-19` with the release deploy command as the restart
boundary. See
[`docs/runbooks/post-deploy-relay-continuity-smoke.md`](../runbooks/post-deploy-relay-continuity-smoke.md)
for the full runbook and pass/fail evidence.

Auto-queue and voice harnesses use the same lane vocabulary. The sandbox
auto-queue preflight defaults to `agent_mode=none` or a controlled finalizer
because it validates generate/dispatch/slot/entry/run truth, not LLM behavior.
Deterministic PCM voice E2E should use `agent_mode=controlled`; the opt-in live
Discord media smoke is the voice `agent_mode=real_live` lane and must say
whether a real provider was contacted. The live media runner is
`scripts/e2e/run_voice_live_media_smoke.py`; it fails closed unless explicit
test guild/channel ids, an AgentDesk bot id, a separate speaker bot token, and
live-safety confirmation env vars are set. Its report separates Discord media,
STT/TTS, provider response, cleanup, and reporting failures.

Destructive steps (`restart_dcserver`, `kill_pane`, `send_keys_no_enter`,
`cancel_turn`) are gated by both `--allow-destructive` and
`AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1`.
Before a destructive restart, the driver now fails closed if
`/api/health/detail.mailboxes` shows any foreign channel/provider with active
mailbox state, cancel token, inflight state, queue depth, recovery/finalizing
state, pending Discord callback, stale thread proof, or relay stall state.

Health waits parse the JSON payload, not just HTTP 2xx. A run is considered
ready only when the health body is healthy (`status: healthy`, `ok` not false,
`fully_recovered` not false, and no unallowed degraded reasons). Degraded or
unhealthy bodies keep polling until timeout and then fail with the last health
summary.

After every executed scenario, the driver also asserts the tested cell's
mailbox is idle via `/api/health/detail`: `agent_turn_status=idle`,
`queue_depth=0`, no cancel token, no inflight state, no active user message, no
pending Discord callback, and no stale/relay-stall proof. It also checks the
tested provider/channel's on-disk pending queue and queued placeholder files
under the runtime root are empty.

Scenario steps can also run an explicit `assert_health` probe. The probe reads
`/api/health` and, when counter caps are requested, `/api/health/detail`; it can
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

- `전체 e2e 시작` → runs all 5 cells sequentially (claude-pipe → claude-tui →
  claude-e → codex-pipe → codex-tui).
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
2. Confirm the operator's Obsidian vault has the 6 prompt files
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

Substitute the cell id for the agent id (`adk-<cell>-e2e`). The
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
