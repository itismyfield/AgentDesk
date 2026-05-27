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

All six channels live under the dedicated `ADK E2E` Discord category, plus the
orchestrator channel `adk-e2e-orchestrator`. Channel IDs are looked up from
`agentdesk.yaml` — no hard-coded ids in scripts or docs.

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
only the two `*-tui` cells. `E-11` (cross-cell concurrency) is `cells: []` —
the orchestrator owns that scenario.

## Driver

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

Destructive steps (`restart_dcserver`, `kill_pane`, `send_keys_no_enter`) are
gated by both `--allow-destructive` and `AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1`.

## Orchestrator (`adk-e2e-orchestrator`)

The orchestrator agent owns the dedicated channel and parses natural-language
commands:

- `전체 e2e 시작` → runs all 5 cells sequentially (claude-pipe → claude-tui →
  claude-e → codex-pipe → codex-tui).
- `claude의 tui 테스트 시작`, `codex의 pipe 테스트 시작`, ... → single cell.
- `claude-pipe 시작` (explicit cell name) → single cell.

The orchestrator creates a result thread on the orchestrator channel (named
`<KST-ISO-short>-<cell>` for single-cell or `<KST-ISO-short>-all-e2e` for
matrix), dispatches each worker via `send-to-agent`, and writes a one-line
status per cell into the thread.

The orchestrator dispatches workers in series even though their lease files
are isolated, so the visible progression in the thread stays linear.

## Provisioning

Initial setup uses the announce-bot-backed CLI added in PR 2:

```bash
scripts/setup-multi-provider-e2e.sh           # live
scripts/setup-multi-provider-e2e.sh --dry-run # preview the CLI invocations
```

The script is idempotent (list-then-create against the parent guild/category).
Operator pastes the printed JSON ids into the matching `PLACEHOLDER_ADK_*`
slots in `~/.adk/release/config/agentdesk.yaml`, then restarts dcserver.

## Skill

Use the `agentdesk-relay-e2e` skill for E2E debugging requests. It documents
the cell-based command shapes and the diagnostics paths to consult first.

## Legacy

`scripts/e2e-tui-relay-scenarios.sh` and `scripts/e2e-wrapper-relay-toggle.sh`
are deprecated; they target the soon-to-be-archived `adk-dashboard-e2e`
channel pair. New work must use the per-cell driver instead.
