# Codex readiness, busy, end and /stop judgment (#8082 design d2)

Model: [`CodexReadiness.tla`](CodexReadiness.tla), view [`codex-readiness.view.mjs`](codex-readiness.view.mjs).
Written against production code `082c77aa4e06b81c76b614f55b8fed73ce01923f`. Design: [`docs/codex-readiness-rollout.design.md`](../../codex-readiness-rollout.design.md).
Design model only: no production code changed.

## What the model covers

One Codex pane on tmux or Herdr, two queued inputs, and these events: Codex becomes the pane's running process, a person presses Enter in the TUI, `task_started` reaches the rollout a moment later, a turn ends, the end record reaches the rollout late / never / unnamed, the Stop hook arrives or is lost, Codex archives the idle rollout to `.zst`, the process exits, dcserver restarts, a follow-up enters the tmux 45 s pane wait and the wait times out, a new incarnation is launched, and `/stop` is asked on an owned running turn.

- `before` is the code as it is. tmux: the intake pre-check sends a follow-up to the queue while the lenient rollout reader says busy; otherwise the follow-up waits up to 45 s for the pane to read ready, and on a timeout the pane is replaced and the prompt goes in the new argv. Herdr: the pane is read and the input is refused when it does not read ready. `/stop` is judged from the pane.
- `after` judges from the bound rollout's latest turn read from this incarnation's floor, the Stop hook of the same turn, the UserPromptSubmit hook of a turn not yet recorded, and the provider process. It re-reads its proof right before the first key. A replacement happens only when the process is gone, or when the rollout is unreadable, has been quiet, and the process has no child.
- The other modes each remove one guard of `after`.

What changed from d1: the host axis and the tmux timeout replacement (`Admit`, `TimeoutReplace`, `ArgvLaunch`); the window between a person's Enter and the `task_started` record (`PersonEnter`, `RecordStart`, `ups`); and the removal of the `Quiet` oracle. The model no longer knows whether a quiet turn has ended: `QuietObserved` and `NoChildObserved` are what the code can observe, and `Silent` lets a running turn show the same readings as an ended one.

The model takes the process reading as proof that a booted Codex takes keys. Codex 0.157.1 reports SessionStart only with the first prompt (`tests/fixtures/hook_payload/codex-0.157.1.json`, run `tui_bypass_startup_clear`), so no hook marks the boot; the design keeps a reduced pane check for the first input of a new execution. A turnless incarnation that the model calls ready is, in the code, the launch's own first-input path.

Assumptions: a turn that AgentDesk starts is recorded as started at once (its own submission is serialized by the channel mailbox; the `InputOwnership*` models cover that order). Whether Codex archives the rollout of a live TUI is not confirmed; the model lets a turn start after the archive.

Not modelled: a person's draft and modals (unchanged by #8082), the check after Enter (a draft read, design section 1.6), the input owner's reservation and receipt, and the relay of a turn's output.

Value ranges come from the entry-point table of the design (section 1): `NativeEnd{Open, Ended, NoStart, Unknown}` (`codex_tui/busy_inject/rollout.rs:35`), the lenient reader (`tui_turn_state.rs:343`), pane liveness (`platform/tmux.rs:1005`, `session_host/herdr_gate.rs:61`·`:78`), the session-keyed ready latch (`codex_tui/input.rs:298`~`:362`), the hook buffer (`claude_tui/hook_registry.rs:52`), the tmux wait and replacement (`codex_tui/warm_followup.rs:542`~`:557`, `codex/absence_launch.rs:248`~`:286`), and `TurnVerdict::NonSteerable` for a pane without a busy entry (`busy_inject/rollout.rs:14`).

## Rules

| Rule | Meaning |
|---|---|
| `CodexNoEarlyInput` | no Idle-route key reaches a running turn |
| `CodexNoShellInput` | no key reaches a pane whose Codex has not booted or has exited |
| `CodexNoDuplicate`, `CodexNoSilentLoss` | each input is written at most once, and a written input is counted |
| `CodexNoRunningKill` | a replacement or relaunch never kills a running turn |
| `CodexStopNoFalseIdle` | `/stop` never answers "nothing is running" while its own turn runs |
| `CodexStopKeyOnOpenTurn` | the stop key is written only while its own turn is open |
| `CodexPendingEventuallyDelivered` (PROPERTY) | a missing or late signal never holds or refuses an input for good |

## Results

`scripts/ci/statecharts_tlc.py` on the 49 configs: GREEN 23, expected RED 26, unexpected 0. The `statechart` command gave the same rule sets; every run ended with exit 0 (23) or 1 (26).

### Before and after, per scenario and host (pane unread unless noted)

"L" is `CodexPendingEventuallyDelivered`. Cfg names drop the `CodexReady` prefix.

| Scenario | Before, Herdr | Before, tmux | After |
|---|---|---|---|
| narrow pane only | `BeforeNarrow` RED L | `BeforeTmuxNarrow` GREEN (the pane is replaced after 45 s) | `AfterNarrow`, `AfterNarrowHerdr` GREEN |
| a person's turn | `BeforePerson` RED L | `BeforeTmuxPerson` (readable pane) RED `CodexNoRunningKill` | `AfterPerson`, `AfterPersonHerdr` GREEN |
| end record lost, Stop hook seen | `BeforeLostRecord` RED L | `BeforeTmuxLostRecord` RED L | `AfterLostRecord` GREEN |
| end record and Stop hook lost | `BeforeLostBoth` RED L | `BeforeTmuxLostBoth` RED L | `AfterLostBothBusy` GREEN with the busy entry; `AfterLostBothHeld` RED L without it |
| unnamed end record | `BeforeUnnamed` RED L | `BeforeTmuxUnnamed` GREEN | `AfterUnnamedHook`, `AfterUnnamed` GREEN |
| rollout archived to `.zst` | `BeforeCompressed` RED L | `BeforeTmuxCompressed` GREEN | `AfterCompressed` GREEN; `AfterCompressedHerdr` RED L before #8128, `AfterCompressedHerdr8128` GREEN |
| dcserver restart after a lost end record, busy entry available | `BeforeRestart` RED L | `BeforeTmuxRestart` GREEN | `AfterRestart` GREEN |
| process exit | `BeforeExit` RED L | `BeforeTmuxExit` GREEN | `AfterExit`, `AfterExitHerdr` GREEN |

Liveness, RED before and GREEN after: all eight scenarios on Herdr except the archived rollout before #8128, and the two lost-end-record scenarios on tmux (with the busy entry for the both-lost case). `CodexReadyBeforeWide` (Herdr, readable pane) is GREEN: the pane judgment is correct when it can be read, so the design must replace what the pane covered and not only remove it.

tmux before is not a liveness defect on an unread pane. Its defects are the relaunch cost, the kill of a turn a person started while a follow-up waits (`BeforeTmuxPerson`), and follow-ups that stay queued while the lenient reader keeps saying busy after a lost end record.

### After, still RED (residuals; design section 8)

| cfg | Breaks | Meaning |
|---|---|---|
| `CodexReadyAfterLostBothHeld` | L | a turn that reads open, both end signals lost, no busy entry: the input stays held. Opening the relaunch here kills running turns (`CodexReadyReclaimOpen`) |
| `CodexReadyAfterCompressedHerdr` | L | Herdr has no replacement path before #8128; the input is refused |
| `CodexReadyAfterSilentTool` | `CodexNoEarlyInput` | dcserver restarts between a person's Enter and the `task_started` record; the UserPromptSubmit evidence is lost |
| `CodexReadyAfterArchivedSilentTurn` | `CodexNoEarlyInput`, `CodexNoRunningKill` | an archived rollout, a person's turn, a silent model turn without children, and a restart together |

`CodexReadyAfterSilentModel` (a quiet running turn that reads open) is GREEN: an open turn is never relaunched.

### Guards of the design, one removed at a time

| cfg | Removed guard | Breaks |
|---|---|---|
| `CodexReadyNoFallback` | Stop-hook tier and the relaunch | L |
| `CodexReadyLatchAuthority` | the latch only wakes | `CodexNoEarlyInput` |
| `CodexReadyNoProcess` | process check | `CodexNoShellInput` |
| `CodexReadyNoRevalidate` | proof read again before the first key | `CodexNoEarlyInput` |
| `CodexReadyNoFloor` (Herdr) | incarnation floor | L |
| `CodexReadyNoChildCheck` | child-process check before the relaunch | `CodexNoEarlyInput`, `CodexNoRunningKill` |
| `CodexReadyTimeoutReplace` | no replacement on a timeout while the judgment is not ready | `CodexNoRunningKill` |
| `CodexReadyReclaimOpen` | the relaunch opens only on an unreadable rollout | `CodexNoRunningKill` |
| `CodexReadyNoUps` | UserPromptSubmit evidence | `CodexNoEarlyInput` |

`CodexReadyNoChildCheck` also shows the restart residual of `CodexReadyAfterSilentTool`; its own break is `CodexNoRunningKill`.

### /stop (Herdr)

| cfg | Judgment | Result |
|---|---|---|
| `CodexStopBeforeWide` | pane, readable | GREEN |
| `CodexStopScreenNarrow` | pane, unread (ready already moved) | RED `CodexStopNoFalseIdle` |
| `CodexStopAfterNarrow` | rollout | GREEN |
| `CodexStopAfterCompressed` | rollout unreadable: answers "unobserved", not "idle" | GREEN |
| `CodexStopAfterLateNoHook` | rollout, end record late and Stop hook lost | RED `CodexStopKeyOnOpenTurn` (known residual) |

## Counterexamples replayed against the code

| cfg | Path | Code | Confirmed |
|---|---|---|---|
| `CodexReadyBeforeNarrow` | boot → Idle; the pane never reads as a composer; the input stays queued | Herdr `composer_ready` times out (`codex/herdr_turn.rs:316`~`:336`); the rollout ready latch is ignored when the captured pane is not accepted (`codex_tui/input.rs:274`) | yes, Herdr and the owner path |
| `CodexReadyBeforeTmuxNarrow` (GREEN) | boot → Idle → 45 s wait → the pane is replaced and the prompt goes in the argv | `try_codex_tui_warm_followup` returns `Fallback` after the wait (`warm_followup.rs:542`~`:557`); `absence_launch.rs:248`~`:286` runs `Replacement::begin`, kills the pane and relaunches | yes |
| `CodexReadyBeforeTmuxPerson` | a follow-up waits; a person presses Enter; the wait times out; the replacement kills the person's turn | `Replacement::begin` checks the turn row and the nonce only (`discord/codex_absence/replacement.rs:63`~`:103`); nothing reads the rollout or the process children | yes, by reading; not reproduced by a test |
| `CodexReadyBeforeTmuxLostRecord` | the turn ends, its end record is lost; the lenient reader keeps saying busy; follow-ups stay queued | the intake pre-check queues while the lenient reader says busy (`intake_turn.rs:1549`~`:1575`, `tui_turn_state.rs:343`) | by reading; not reproduced |
| `CodexReadyNoFallback` | the turn's end is never provable; the input waits for good | the owner start maps `NoStart \| Unknown` to `Screen::Unknown` and watches again without bound (`provider_dispatch/owner_input.rs:829`, `:771`) | yes, for the owner path as it is today |
| `CodexReadyLatchAuthority` | turn 1 ends and latches; a person starts turn 2; the latch still reads ready | the latch is removed only by `take` (`input.rs:342`); `task_started` does not clear it (`rollout_tail/parser.rs:467`~`:480`). Today the pane check vetoes it, so this is a rule for the new code | guard needed |
| `CodexReadyNoFloor` | the process exits before the end record is written; the resumed incarnation reads the old turn as open | `read_native_end` scans back to the file's latest `task_started` and knows no incarnation boundary (`busy_inject/rollout.rs:98`~`:105`) | yes |
| `CodexReadyTimeoutReplace` | boot → a person presses Enter → the timeout replaces the pane | the same `Fallback` path, if the new wait keeps it for a `Busy` judgment | guard needed (design section 2) |
| `CodexReadyReclaimOpen` | an owned turn runs and reads open; quiet and childless; the relaunch kills it | a read-only launch has no busy entry (`TurnVerdict::NonSteerable`), and a long model response writes nothing and has no child | guard needed |
| `CodexReadyNoUps`, `CodexReadyAfterSilentTool` | a person presses Enter; `task_started` is not written; the rollout still reads the previous end; an Idle-route input is written | the rollout is the only turn signal without the hook; Codex queues the input, so nothing is lost | residual for hookless launches and a restart inside the window |
| `CodexStopScreenNarrow` | own turn runs; the pane shows no running marker; `/stop` answers idle | `herdr_turn_in_progress` needs `recent_codex_active_turn_marker` (`input/herdr_cancel.rs:14`); `deliver` returns `Idle` (`codex_stop_delivery.rs:776`~`:785`) | yes |
| `CodexStopAfterLateNoHook` | the turn ended in the TUI; its record is not written and no Stop hook arrived; the rollout still reads open | design residual; the pane has the same window while the spinner is still drawn | accepted, see design section 8 |

A related candidate found while replaying, not a model path: the lenient reader classifies `turn_aborted` as Streaming (`tui_turn_state.rs:727`~`:736`), and `tui_busy_followup_diagnostic` reads Codex through it (`tui_followup.rs:455`). Not reproduced by a test or in production.

## Charts

- [`codex-readiness.chart.mmd`](codex-readiness.chart.mmd): `CodexReadyAfterPerson`.
- [`codex-readiness.before.chart.mmd`](codex-readiness.before.chart.mmd): `CodexReadyBeforeNarrow` (Herdr).
- [`codex-readiness.before-tmux.chart.mmd`](codex-readiness.before-tmux.chart.mmd): `CodexReadyBeforeTmuxPerson`.
- [`codex-readiness.held.chart.mmd`](codex-readiness.held.chart.mmd): `CodexReadyAfterLostBothHeld`.
- [`codex-readiness.no-fallback.chart.mmd`](codex-readiness.no-fallback.chart.mmd): `CodexReadyNoFallback`.
- [`codex-readiness.stop-screen.chart.mmd`](codex-readiness.stop-screen.chart.mmd): `CodexStopScreenNarrow`.

The charts do not draw steps that stay in one label (a restart that changes nothing visible, a refused proof, the idle wait step).

## Run

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/input-single-path
~/.claude/skills/statechart/scripts/statechart CodexReadiness.tla --config CodexReadyAfterPerson.cfg --view codex-readiness.view.mjs --out /tmp/sc-codex-ready
```
