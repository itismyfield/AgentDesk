# Claude activity evidence — #8081 design model

Production code SHA: **5c3ed37cddc80036fa8a274419b0219f49926704**. No production file changed and no cargo/product test ran.
The model was prepared and checked on 2026-11-12 within the 30-minute modeling limit. Final wrapper: **16 configs, 7 GREEN, 9 expected RED, failed0/skip0, 32.720 seconds**. Official repository checker on the same frozen bytes: **16 configs / 39 TLC runs / expected sets all matched / 16.6 seconds**. Raw checker output is `sc-out/final-v7/ci-final.log`. No model/cfg/view edit follows this freeze.

## Interpretation and existing models

This is a bounded refinement of the activity input consumed by the existing lifecycle/input models, not a new complete session lifecycle proof. `TmuxSession` keeps its Crash/Boot/restore ownership actions; `HerdrSession` keeps identity, placement, exact close and input custody. The `refs/review/i8080-d2` `ClaudeReadiness` refinement is the input contract: root source/turn matching, hard-terminal eligibility, continuing-Stop latch, Idle-ready admission and native Busy injection remain distinct. Its independent restart proof does not prove suppression-history-loss progress. The b3 compact completion contract is an external input here, never a generic readiness terminal.

`observed` is a normalized activity/reacquire hint, **not** an idle key permit, provider state enum or kill permission. Current's missing positive pane busy bit is `unknown`, not a newly asserted Idle. Proposed's `busy` summarizes separately admitted Generating/ForegroundTool evidence; Submitted remains non-interruptible. A valid progress event is already source/floor/continuation-classified by an abstract adapter. The model does not establish which Claude JSON shape is sufficient to create that evidence.

A current active-pane=false scenario denies stop; that is not a model of every current stop branch. The separate Current physical-race scenario starts with a recognized active pane. CurrentBackground starts with all existing cleanup guards other than the missed background bit satisfied (stable complete root terminal, ready pane, no modal/busy footer, idle/unoccupied AgentDesk row). The code replay below describes those preconditions explicitly.

## Bounds fixed from code entrypoints

| Model domain | Code origin and projection |
|---|---|
| actual idle/submitted/generation/tool; transcript busy/idle | `src/services/discord/turn_bridge/tmux_runtime/claude_stop_delivery.rs:29`, `:206`; `claude_stop_pane.rs:5`. UserSubmitted alone has no generation evidence. |
| hint unknown/idle/busy; watcher absent/attached | `src/services/discord/tmux_watcher/liveness.rs:386`, `:390`; `loop_poll_prologue.rs:467`; `turn_presence/activity.rs:1043`. |
| one tool_use_id, pre/post open/closed, losses/reorder | `src/services/claude_tui/hook_bundle.rs:20`; `hook_server.rs:414`; `hook_server/hook_event.rs:11`. Pairing is proposed; accepted source classification is abstracted. |
| one pending child, SubagentStop, exact terminal, process positive veto | `src/services/tmux_common.rs:518`; `tmux_turn_liveness.rs:175`; `claude_tui/hook_server.rs:430`. A process absence is not a completion proof. |
| source epoch 1..2; one restart | existing `session-lifecycle/TmuxSession.tla:Crash/Boot` and #8080 `ClaudeReadiness.Restart`; `hook_server/hook_event.rs:46` carries captured binding context. |
| presence present/missing | `src/services/tmux_turn_liveness.rs:31`; only the established Missing row cleanup is modeled as cleanup permission. Present remains Keep. |
| one lease expiration and recovery | proposed 2-second generation lease / 30-second unmatched-tool lease are scaled to one Timeout. Timeout yields Unknown and cannot create Idle/kill permission. A new progress event, not rereading the same tail, renews a lease. This is an event-order proof, not a wall-clock upper-bound proof. |
| separate stop check and send | `src/services/discord/turn_bridge/tmux_runtime/claude_stop_delivery.rs:545` → `:580`. Token/source/composer fences protect ownership; they cannot lock provider completion. |

## Rules and the physical boundary

- `NoEscWithoutCurrentEvidence`: an actual Escape action needs a current source epoch, a live lease and generation/foreground-tool proof at the local delivery decision. A Post-first tombstone and later lease expiration revoke that authority.
- `NoEscUnlessGenerationOrForegroundTool`: the stronger predicate inspects actual provider state at the physical write. It is checked in the stable-generation Proposed/Restart/Submitted configs, and is deliberately **RED in both physical-race configs**. It is not weakened to pretend physical atomicity.
- `NoIdleKillWhileBusy`: the proposed policy never kills a Present Claude TUI automatically. Missing cleanup remains live via `MissingEventuallyCleanup`; green is not created by disabling every cleanup action.
- `NoPresentIdleCleanup`, `NoSubmittedEscape`, `NoBackgroundEscape`, `SubagentStopClosesChild` and `NoLatePreReopensClosedTool` state the remaining narrowed obligations.
- `EventuallyConverges`: once actual state stabilizes and its authoritative source remains eventually observable, the hint eventually stays equal to actual busy/idle in the current epoch. `ActivityFinish` separately models actual generation completion, delayed terminal arrival and a possibly permanently lost Stop hook.
- `EventuallyReacquires` checks the still-active generation scenario; `BackgroundEventuallySettles` checks one known child's eventual exact completion observation. A positive `NeverEscape` witness fails, proving at least one allowed Escape is reachable.

No unconditional convergence is claimed when all authoritative sources remain permanently unavailable. Current epoch accepted hooks, exact child terminal association and correctly classified source progress are abstract inputs, not proved ingress guards. `OldIncarnationHook` is intentionally a no-op after that abstract acceptance filter; its presence does not independently verify the actual filter. One tool ID and one child do not prove multi-child identity isolation, async Task acknowledgement or restart restoration of old unfinished tool/child ledgers. Those need implementation acceptance tests.

## Code-confirmed counterexamples

### W1: positive pane busy missing while generation continues

The repository's existing fixture `tmux_common.rs:3640-3651` (`live_early_spinner_with_stale_prompt`) contains `· Thinking…`, an empty stale composer and an MCP status line. Existing assertions require both `ready_for_input=true` and `busy=true`. `actively_streaming` at `tmux_common.rs:433-438` returns false immediately when readiness is true, so that fixture has a busy provider but the reacquire bit is false. No test was executed in this design lane; this is a direct replay of existing fixture/assertion bytes against the branch.

The model's Current hint stays unknown even as new source progress arrives. `liveness.rs:390-398` consumes only capture plus `actively_streaming`; a failed capture is another explicit false branch. `loop_poll_prologue.rs:467-472` requires that bit for the legacy post-terminal/missing-inflight reacquire, and `streaming_status_tick.rs:264` has the adjacent consumer. Repeated source progress does not change this gate. This confirms the model's missed-reacquire path, not a claim that a specific 46-column footer has been executed or that every transcript-mode channel uses this legacy branch.

### W2: physical completion between check and Escape

Both physical-race configs show exactly three steps: check current generation authority → provider completes → physical send. Current code reads pane/binding/structured state in the `recheck` closure at `claude_stop_delivery.rs:545-575`, then `write` calls `send_keys(... Escape)` at `:580-588`. Neither the composer mutex nor the interrupt registry serializes Claude's own completion. Proposed source revalidation narrows wrong sends and preserves ownership fences, but cannot provide an absolute physical-state guarantee for this interval. The limitation remains in both configs and must not be reported as RED→GREEN.

### W3: current idle cleanup misses background-only work

CurrentBackground is conditional on the existing unoccupied-row guard already returning true for the parent (provider-internal async work is not an AgentDesk child row). The root transcript is Idle and stable. `tmux_turn_liveness.rs:146-171` checks pane ready, modal, busy/interrupt and background. Its final bit at `:175-187` uses the management/footer strings plus footer work counts. A wrapped or unavailable background management row can miss that final predicate; the existence of in-process provider background work does not force the parent transcript Streaming. The model preserves these explicit prerequisites and explores the cleanup action before the child completes, reaching `NoIdleKillWhileBusy`. Proposed Present→Keep removes that permission, rather than interpreting process absence or timeout as positive Idle proof.

W3 is a predicate-level code replay, not an operational kill/reproduction. Main's existing schema/row fence and kill-under-source-authority call are not broadened.

### Design controls, not current bugs

The NoTombstone, LiveKill and NoLeaseRecovery REDs remove proposed guards; they are acceptance controls, not claims about current code. Post-first → late Pre must keep the tool closed. Deleting Present→Keep permits killing active background work. Removing source recovery after lease expiration leaves Unknown forever. The StopWitness RED is a reachability witness, not a bug: its shortest path is `StopRead` → `StopSend`, with `NeverEscape` violated and both activity-evidence and actual-active safety invariants still holding.

## Reproduce and evidence

For any config below, from the repository root:

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/session-activity/SessionActivity.tla --config docs/statecharts/session-activity/ActivityProposed.cfg --view docs/statecharts/session-activity/activity.view.mjs --out /tmp/i8081-activity-new
```

Final raw wrapper output, TLC logs/counterexamples, exact commands/exits/times and frozen model hashes are under `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8081/sc-out/final-v7/`. Earlier v1-v6 outputs are superseded. The graph hides only transitions that stay in the same displayed label. Configs over 500 model states have no complete chart; red ones retain the shortest bug-path charts. The source model and reports remain the complete checked scope.

This design model is not product DoD completion. No PG, cargo build, runtime probe or production test binary ran. Narrow/wrapped-pane fixtures, actual generation-progress shape, old-incarnation rejection, multi-tool/multi-child source association, lost hook reconciliation and async Task classification remain implementation acceptance work.

`ActivityPermanentUnread` deliberately keeps source reads unavailable forever: Esc remains zero, while convergence/reacquire are RED. It is an observation-limit control, distinct from NoLeaseRecovery, which removes renewal after a lease expires despite otherwise successful reads. Faults are bounded to one expiration, one restart and one tool/child identity; no proof covers endless source outages or infinite fault churn.

## Final per-config results

| Config | Result | States | Seconds | Rules that break |
|---|---|---:|---:|---|
| ActivityBackground | GREEN | 1,296 | 1.328 | - |
| ActivityCurrent | expected RED | 108 | 3.077 | EventuallyConverges, EventuallyReacquires |
| ActivityCurrentBackground | expected RED | 3,072 | 1.88 | NoIdleKillWhileBusy |
| ActivityFinish | GREEN | 3,762 | 1.305 | - |
| ActivityLiveKill | expected RED | 3,576 | 1.855 | NoIdleKillWhileBusy |
| ActivityMissing | GREEN | 360 | 1.898 | - |
| ActivityNoLeaseRecovery | expected RED | 162 | 3.515 | EventuallyConverges, EventuallyReacquires |
| ActivityNoTombstone | expected RED | 17,502 | 2.152 | NoLatePreReopensClosedTool |
| ActivityPermanentUnread | expected RED | 36 | 3.031 | EventuallyConverges, EventuallyReacquires |
| ActivityPhysicalRace | expected RED | 1,170 | 1.761 | NoEscUnlessGenerationOrForegroundTool |
| ActivityPhysicalRaceCurrent | expected RED | 708 | 1.783 | NoEscUnlessGenerationOrForegroundTool |
| ActivityProposed | GREEN | 342 | 1.823 | - |
| ActivityRestart | GREEN | 954 | 1.197 | - |
| ActivityStopWitness | expected RED | 342 | 2.42 | NeverEscape |
| ActivitySubmitted | GREEN | 198 | 1.778 | - |
| ActivityToolPair | GREEN | 15,522 | 1.917 | - |
