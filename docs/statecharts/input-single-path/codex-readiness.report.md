# Codex readiness, busy, end and /stop judgment (#8082 design d1)

Model: [`CodexReadiness.tla`](CodexReadiness.tla), view [`codex-readiness.view.mjs`](codex-readiness.view.mjs).
Written against production code `082c77aa4e06b81c76b614f55b8fed73ce01923f`. Design: [`docs/codex-readiness-rollout.design.md`](../../codex-readiness-rollout.design.md).
Design model only: no production code changed.

## What the model covers

One Codex pane, two queued inputs, and these events: Codex becomes the pane's running process, a person starts a turn in the TUI, a turn ends, the end record reaches the rollout late / never / unnamed, the Stop hook arrives or is lost, Codex archives the idle rollout to `.zst`, the process exits, dcserver restarts, a new incarnation is launched, and `/stop` is asked on an owned running turn.

- `before` judges ready and `/stop` from the pane. A pane that cannot be read (narrow pane, new status line) proves nothing.
- `after` judges from the bound rollout's latest turn read from this incarnation's floor, the Stop hook of the same turn, and the provider process. It re-reads its proof right before the first key.
- The other modes each remove one guard of `after`.

The model takes the process reading as proof that a booted Codex takes keys. Codex 0.157.1 reports SessionStart only with the first prompt (`tests/fixtures/hook_payload/codex-0.157.1.json`, run `tui_bypass_startup_clear`), so no hook marks the boot; the design keeps a reduced pane check for the first input of a new execution. A turnless incarnation that the model calls ready is, in the code, the launch's own first-input path.

Not modelled: a person's draft and modals (unchanged by #8082), the input owner's reservation and receipt (the `InputOwnership*` models), and the relay of a turn's output.

Value ranges come from the entry-point table of the design (section 1): `NativeEnd{Open, Ended, NoStart, Unknown}` (`codex_tui/busy_inject/rollout.rs:35`), pane liveness (`platform/tmux.rs:977`, `session_host/herdr_gate.rs:61`·`:78`), the session-keyed ready latch (`codex_tui/input.rs:298`~`:362`) and the hook buffer (`claude_tui/hook_registry.rs:52`).

## Rules

| Rule | Meaning |
|---|---|
| `CodexNoEarlyInput` | no Idle-route key reaches a running turn |
| `CodexNoShellInput` | no key reaches a pane whose Codex has not booted or has exited |
| `CodexNoDuplicate`, `CodexNoSilentLoss` | each input is written at most once, and a written input is counted |
| `CodexNoRunningKill` | a relaunch never kills a running turn |
| `CodexStopNoFalseIdle` | `/stop` never answers "nothing is running" while its own turn runs |
| `CodexStopKeyOnOpenTurn` | the stop key is written only while its own turn is open |
| `CodexPendingEventuallyDelivered` (PROPERTY) | a missing or late signal never holds or refuses an input for good |

## Results

`scripts/ci/statecharts_tlc.py` on the 31 configs: GREEN 15, expected RED 16, unexpected 0 (30.1s). The `statechart` command gave the same rule sets; every run ended with exit 0 or 1.

### Liveness, before and after, per scenario (pane unread in both)

| Scenario | Before cfg | Before | After cfg | After |
|---|---|---|---|---|
| narrow pane only | `CodexReadyBeforeNarrow` | RED | `CodexReadyAfterNarrow` | GREEN |
| a person's turn, late end record, busy entry | `CodexReadyBeforePerson` | RED | `CodexReadyAfterPerson` | GREEN |
| end record lost, Stop hook seen | `CodexReadyBeforeLostRecord` | RED | `CodexReadyAfterLostRecord` | GREEN |
| end record and Stop hook lost | `CodexReadyBeforeLostBoth` | RED | `CodexReadyAfterLostBothBusy` (busy entry), `CodexReadyAfterLostBothReclaim` (no busy entry) | GREEN |
| unnamed end record | `CodexReadyBeforeUnnamed` | RED | `CodexReadyAfterUnnamedHook`, `CodexReadyAfterUnnamed` | GREEN |
| rollout archived to `.zst` | `CodexReadyBeforeCompressed` | RED | `CodexReadyAfterCompressed` | GREEN |
| dcserver restart after a lost end record | `CodexReadyBeforeRestart` | RED | `CodexReadyAfterRestart` | GREEN |
| process exit | `CodexReadyBeforeExit` | RED | `CodexReadyAfterExit` | GREEN |

RED means `CodexPendingEventuallyDelivered` breaks. `CodexReadyBeforeWide` (readable pane) is GREEN: the pane judgment is correct when it can be read, which is why the design must replace what the pane covered and not only remove it. `CodexReadyAfterSilentTool` is GREEN.

### Guards of the design, one removed at a time

| cfg | Removed guard | Breaks |
|---|---|---|
| `CodexReadyNoFallback` | Stop-hook tier and the reclaim | `CodexPendingEventuallyDelivered` |
| `CodexReadyLatchAuthority` | the latch only wakes | `CodexNoEarlyInput` |
| `CodexReadyNoProcess` | process check | `CodexNoShellInput` |
| `CodexReadyNoRevalidate` | proof read again before the first key | `CodexNoEarlyInput` |
| `CodexReadyNoFloor` | incarnation floor | `CodexPendingEventuallyDelivered` |
| `CodexReadyNoChildCheck` | child-process check before the reclaim | `CodexNoRunningKill` |

### /stop

| cfg | Judgment | Result |
|---|---|---|
| `CodexStopBeforeWide` | pane, readable | GREEN |
| `CodexStopScreenNarrow` | pane, unread (ready already moved) | RED `CodexStopNoFalseIdle` |
| `CodexStopAfterNarrow` | rollout | GREEN |
| `CodexStopAfterCompressed` | rollout unreadable: answers "unobserved", not "idle" | GREEN (750 states, no chart) |
| `CodexStopAfterLateNoHook` | rollout, end record late and Stop hook lost | RED `CodexStopKeyOnOpenTurn` (known residual) |

## Counterexamples replayed against the code

| cfg | Path | Code | Confirmed |
|---|---|---|---|
| `CodexReadyBeforeNarrow` | boot → Idle; the pane never reads as a composer; the input stays queued | `PromptDraftPolicy::accepts` needs `composer_marker_detected` (`codex_tui/input.rs:235`); the rollout ready latch is ignored when the captured pane is not accepted (`:274`); Herdr `composer_ready` times out (`codex/herdr_turn.rs:316`~`:336`) | yes |
| `CodexReadyNoFallback` | the turn's end is never provable; the input waits for good | the owner start maps `NoStart \| Unknown` to `Screen::Unknown` and watches again without bound (`provider_dispatch/owner_input.rs:829`, `:771`) | yes, for the owner path as it is today |
| `CodexReadyLatchAuthority` | turn 1 ends and latches; a person starts turn 2; the latch still reads ready | the latch is removed only by `take` (`input.rs:342`); `task_started` does not clear it (`rollout_tail/parser.rs:467`~`:480`). Today the pane check vetoes it, so this is a rule for the new code, not a defect on main | guard needed |
| `CodexReadyNoFloor` | the process exits before the end record is written; the resumed incarnation reads the old turn as open | `read_native_end` scans back to the file's latest `task_started` and knows no incarnation boundary (`busy_inject/rollout.rs:98`~`:105`) | yes |
| `CodexStopScreenNarrow` | own turn runs; the pane shows no running marker; `/stop` answers idle | `herdr_turn_in_progress` needs `recent_codex_active_turn_marker` (`input/herdr_cancel.rs:14`); `deliver` returns `Idle` (`codex_stop_delivery.rs:776`~`:785`) | yes |
| `CodexStopAfterLateNoHook` | the turn ended in the TUI; its record is not written and no Stop hook arrived; the rollout still reads open | design residual; the pane has the same window while the spinner is still drawn | accepted, see design section 8 |

A related candidate found while replaying, not a model path: the lenient reader classifies `turn_aborted` as Streaming (`tui_turn_state.rs:727`~`:736`), and `tui_busy_followup_diagnostic` reads Codex through it (`tui_followup.rs:455`). Not reproduced by a test or in production.

## Charts

- [`codex-readiness.chart.mmd`](codex-readiness.chart.mmd): `CodexReadyAfterPerson`.
- [`codex-readiness.before.chart.mmd`](codex-readiness.before.chart.mmd): `CodexReadyBeforeNarrow`.
- [`codex-readiness.no-fallback.chart.mmd`](codex-readiness.no-fallback.chart.mmd): `CodexReadyNoFallback`.
- [`codex-readiness.stop-screen.chart.mmd`](codex-readiness.stop-screen.chart.mmd): `CodexStopScreenNarrow`.

The charts do not draw steps that stay in one label (a restart that changes nothing visible, a refused proof, the idle wait step).

## Run

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/input-single-path
~/.claude/skills/statechart/scripts/statechart CodexReadiness.tla --config CodexReadyAfterPerson.cfg --view codex-readiness.view.mjs --out /tmp/sc-codex-ready
```
