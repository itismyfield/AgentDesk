# Daemon restart over an idle herdr Claude pane (#8161, 3rd leg)

Base: `land/batch-1115c` 9b42e53297. Model: `ClaudeReadiness.tla` through `InputPath.tla` (`ReadinessSpec`), N = 2.
Design: [restart-idle-readiness.design.md](restart-idle-readiness.design.md). View: [restart-idle-readiness.view.mjs](restart-idle-readiness.view.mjs).

## What the model adds

| Element | Meaning | Code it stands for |
|---|---|---|
| `Restart` in `ready-restart-*` modes | the ledger empties; the first look sets the restart floor (`restored = "floor"`) | `readiness/mod.rs:136-142`, `:517-520` |
| `oldIncarnation` | the pane's incarnation was launched before this daemon run; a handoff mode's restart makes a new one (`Booting`, `needsStart`) | T4: `execution_context.created_at < STARTED` |
| `PreRestartSend`, `lag = "both"`, `inflight` | keys reached the provider before the restart; neither transcript nor screen shows the turn yet; the durable in-flight record survives | herdr hold `claude/herdr_turn.rs:572`/`:264`, V2 `head_injection`, inflight recovery `runtime_bootstrap/recovery_flush.rs:48` |
| `PersonLateSubmit`, `lag = "transcript"` | a person's Enter (or a continuation) not yet recorded; the screen shows the turn | screen check S1 (`herdr_turn/composer.rs` `observation`) |
| `LateRecord` | the provider writes the late turn's first record | transcript |
| `LostStart` | the new incarnation idles but its SessionStart never reached this run | lost hook |
| `RestoredEvidence` | the fixed path: restart floor + hard terminal + current Idle observation + no continuation/compact + no durable in-flight + idle screen + old incarnation | `observe_restored` (new) |
| `Arrived` | inputs arrive only after the restart; a restored in-flight turn owns the channel | incident timing; D3 |
| `RestartIdleEventuallyReceives` | an idle pane that outlived the restart, with an empty composer and nothing unsettled, eventually takes an input | new liveness rule |

## Results

`python3 scripts/ci/statecharts_tlc.py --base 9b42e53297 --jobs 4 --timeout 600`: 104 configs selected (every
`ClaudeReadiness*`/`InputPath*` config), every result as listed in `expected.tsv`, exit 0, 273 s.

| Config | Mode | Distinct states | Expected | Result |
|---|---|---|---|---|
| `InputPathReadyRestartIdleBefore.cfg` | current code | 1,804 | `ReadyRestartIdleEventuallyReceives` breaks | RED as expected: restart → floor → both inputs retained, forever ([chart](restart-idle-before.chart.mmd)) |
| `InputPathReadyRestartIdleAfter.cfg` | fixed | 1,960 | all hold | GREEN |
| `InputPathReadyRestartLateAfter.cfg` | fixed, earlier send unrecorded | 3,626 | all hold | GREEN |
| `InputPathReadyRestartLateIgnoreDurable.cfg` | mutant: no durable gate | 337 | `ReadyNoIdleRunningInjection` | RED as expected ([chart](restart-idle-ignore-durable.chart.mmd)) |
| `InputPathReadyRestartRunningAfter.cfg` | fixed, person's Enter unrecorded | 2,641 | all hold | GREEN |
| `InputPathReadyRestartRunningIgnoreScreen.cfg` | mutant: no screen check | 540 | `ReadyNoIdleRunningInjection` | RED as expected ([chart](restart-idle-ignore-screen.chart.mmd)) |
| `InputPathReadyRestartDraftAfter.cfg` | fixed, person draft | 2,634 | all hold | GREEN (no draft overwrite, sent after the person clears) |
| `InputPathReadyRestartHandoffAfter.cfg` | fixed, new incarnation | 2,334 | all hold | GREEN (Start path, or retained when the Start is lost) |
| `InputPathReadyRestartHandoffIgnoreIncarnation.cfg` | mutant: no incarnation check | 614 | `ReadyNoEarlyInput` | RED as expected |

Every config checks `ReadyTypeOK`, `ReadyNoDuplicate`, `ReadyNoSilentLoss`, `ReadyNoIdleRunningInjection`,
`ReadyNoHumanDraftExecution`, `ReadyNoEarlyInput`, `ReadyCompactAfterStopIdle`, `ReadyNoContinuationEarlyInput`,
`ReadyBusyKeyPlanPreserved`, `ReadyRetainedOnce`, `ReadyExpiryOnlyUnproven` and the properties
`ReadyPendingEventuallySettles`, `ReadyRestartIdleEventuallyReceives`.

## The before counterexample is the incident

`Before restart: turn running` → `Finish` (transcript ends in a hard terminal) → `Restart` (floor) → `Observe`
→ `Expire` input 1 → `Expire` input 2 → every input retained, the pane still idle with an empty composer. That is
게임-cc on 2026-11-15: transcript last written 15:03 KST, restarts 15:25 and 15:56, refusals 15:34 and 16:22, both
retained `known_not_sent: vetoed … Refused(Unproven)`.

## Boundaries the model does not prove

- `PersonLateSubmit` is enabled only while the restart floor stands. After a passed floor, a person's Enter not yet
  recorded is the existing unreported-key window (`transcript-readiness.design.md:418`); a first draft of this mode
  that allowed it there broke `ReadyNoIdleRunningInjection` on the old path, not the new one.
- Screen and transcript both lagging with no durable record (a person's Enter while the UserPromptSubmit hook waits on
  the stopped daemon) is not modelled as safe; it is measurement Q5 in the design.
- The model's `Submit` rechecks readiness atomically; the code's screen check precedes the first key by one poll, the
  same check-to-key window as the existing Idle path.
- D3 (a restored in-flight turn keeps the channel off the Idle first-input path) is an assumption of `Arrived`; the
  design's test Q4 checks it in code.
