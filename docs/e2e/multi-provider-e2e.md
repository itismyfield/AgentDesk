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
cells: [claude-pipe, claude-tui, claude-e, codex-pipe, codex-tui]
steps:
  - send_prompt: ...
assertions:
  - ...
```

Provider-specific scenarios (`E-6` claude `/compact`, `E-7` codex `/compact`)
narrow the list. TUI-keystroke-only scenarios (`E-4`, `E-10`, `E-12`) include
only the two `*-tui` cells. `E-13` covers the Claude Code pipe/headless
scheduled wakeup/monitor path: the first turn must call `ScheduleWakeup`, and
the automatic wake turn must relay `[E2E:E13:WAKE]` to Discord. It intentionally
runs only on `claude-pipe`; `claude-tui` and `claude-e` keep normal relay
coverage because this matrix does not create a persistent Claude Code wake
session for those cells. `E-16` and `E-17` are #2935 regression stubs: they are
loaded by the relevant cells but skipped until the runtime/orchestrator exposes
hooks to force Claude TUI completion-quiescence timeout and to hold a foreign
active mailbox during a destructive restart attempt. `E-18` is an executable
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
(cross-cell concurrency) is `cells: []` — the orchestrator owns that scenario.
`E-22` covers tool-use to text completeness for relay-backed pipe/TUI cells by
waiting for a current-turn provider hold witness after a real tool call and
then asserting the post-tool body remains complete. `E-23` is the dedicated
premature-completion guard: completion chrome must exist and must not appear
before the final body marker.

## #2943 Scenario Coverage And Gaps

Covered P0/P1 backlog items:

- `tool_use->text completeness`: `E-22`, relay-backed pipe/TUI cells.
- `stop-mid-turn`: `E-18`, relay-backed pipe/TUI cells, destructive-gated.
- `cron self-prompt relay`: `E-13` covers the available Claude Code
  `ScheduleWakeup`/monitor self-prompt path on `claude-pipe`.
- `restart context continuity`: `E-19`, TUI cells, with tmux identity and
  pre-restart token recall.
- `premature-completion guard`: `E-23`, all cells.
- `followup-during-busy` / same-session pressure: `E-20`, all cells.
- `direct-input body_complete + control-byte strip`: `E-21`, TUI cells.

Remaining exact gaps:

- Exact `CronCreate` Background classification is not scenario-expressible yet:
  the harness can exercise `ScheduleWakeup` (`E-13`) and unit coverage pins
  Background result delivery, but there is no scenario step that injects or
  creates a `CronCreate`/Background task-notification turn deterministically.
  Follow-up needed if exact CronCreate live coverage is required.
- Deterministic Codex modern-schema replay (`response_item` + `event_msg`
  `task_complete`) is not scenario-expressible yet: normal Codex cells exercise
  the live schema indirectly, but the harness has no JSONL/frame injection step
  that can force that exact envelope sequence without production runtime state.
  Follow-up needed for fixture-level schema replay.
- `tool_use->text completeness` for `claude-e` is not claimed by `E-22`: the
  scenario relies on `wait_for_provider_hold_state`, and there is no current
  local/non-live fixture proving `claude-e` persists the same
  `any_tool_used=true` and `has_post_tool_text=false` inflight witness before
  post-tool text. Follow-up needed before adding `claude-e` to `E-22`.

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
    [--dry-run]
```

The driver writes `report.<cell>.json` so an orchestrator that aims all 5 cells
at one `--output` directory does not overwrite sibling reports. Per-cell lease
files (`/tmp/agentdesk-e2e-relay.<cell>.lease`) let cells run in parallel from
separate operator sessions.

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
