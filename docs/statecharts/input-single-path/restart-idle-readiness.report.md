# Daemon restart over an idle herdr Claude pane (#8161, 3rd leg)

Base: `land/batch-1115c` 9b42e53297. Model: `ClaudeReadiness.tla` through `InputPath.tla` (`ReadinessSpec`), N = 2.
Design: [restart-idle-readiness.design.md](restart-idle-readiness.design.md). View: [restart-idle-readiness.view.mjs](restart-idle-readiness.view.mjs).

## What the model adds

| Element | Meaning | Code it stands for |
|---|---|---|
| `Restart` in `ready-restart-*` modes | the ledger empties; the first look sets the restart floor (`restored = "floor"`) | `readiness/mod.rs:136-142`, `:517-520` |
| `oldIncarnation` | the pane's incarnation was launched before this daemon run; a handoff mode's restart makes a new one (`Booting`, `needsStart`) | T4: `execution_context.created_at < STARTED` |
| `PreRestartSend`, `lag = "transcript"`, `inflight` | keys reached the provider before the restart; the transcript does not show the turn yet, the screen does; the durable in-flight record survives (`lag = "both"` only in the `late-blind` mutant) | herdr hold `claude/herdr_turn.rs:572`/`:264`, V2 `head_injection`, prior-process row `provider_dispatch/owner_input.rs:207`, `herdr_row_end/owner.rs:105`/`:111` |
| `RowEnd` | the recovery row end: on an idle screen of the same execution with the last turn closed it settles NoTerminal, drops the in-flight row and wakes the owner | `recovery_engine/herdr_row_end.rs:557` `idle_read` |
| `PersonLateSubmit`, `lag = "transcript"` | a person's Enter (or a continuation) not yet recorded; the screen shows the turn | screen check S1 (`herdr_turn/composer.rs:97` `restored_screen_idle`) |
| `WatchHeld` (restart-draft modes) | the held-front watch wakes a front held by a person's draft once the draft clears, on restored evidence + screen; `HumanClears` and `Observe` alone never wake it | `herdr_turn/first_input.rs:153-171` |
| `Unproven` | what `Watched::unproven` reads: not Booting, shared Idle, current source, no continuation/compact fence, no current evidence; never the screen, the restored proof or the provider phase | `readiness/mod.rs` `Watched::unproven` |
| Retained input notice | an expired unproven original owes one persistent resend notice; replay keeps its outbox identity | `src/services/turn_orchestrator/queued_head/v2/retained.rs:185`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:308`, `src/services/discord/queue_io/input_owner/notice.rs:68`, `src/services/discord/queue_io/retained_notice.rs:39` |
| `LateRecord` | the provider writes the late turn's first record | transcript |
| `LostStart` | the new incarnation idles but its SessionStart never reached this run | lost hook |
| `RestoredEvidence` | the fixed path: restart floor + hard terminal + current Idle observation + no continuation/compact + no durable in-flight + idle screen + old incarnation | `observe_restored` (`readiness/mod.rs:98`) |
| `Arrived` | inputs arrive only after the restart; a restored in-flight turn owns the channel | incident timing; D3 |
| `RestartIdleEventuallyReceives` | an idle pane that outlived the restart, with an empty composer and nothing unsettled, eventually takes an input | new liveness rule |

## Results

Implementation leg (this model revision): every restart config run directly with the statechart skill
(`sc-out/impl3`), then the CI selection (see the implementation report for its line).

| Config | Mode | Design leg | Implementation leg | Expected |
|---|---|---|---|---|
| `InputPathReadyRestartIdleBefore.cfg` | current code | RED liveness, 1,804 | RED `ReadyRestartIdleEventuallyReceives`, 1,804 | breaks: the incident ([chart](restart-idle-before.chart.mmd)) |
| `InputPathReadyRestartIdleAfter.cfg` | fixed | GREEN 1,960 | GREEN 1,960 | all hold |
| `InputPathReadyRestartLateAfter.cfg` | fixed, earlier send unrecorded | GREEN 3,626 (`lag = "both"`) | GREEN 3,626 (`lag = "transcript"` + `RowEnd`) | all hold |
| `InputPathReadyRestartLateIgnoreDurable.cfg` → `InputPathReadyRestartLateBlind.cfg` | mutant: was "no durable gate"; now "screen blind to the unrecorded send", so `RowEnd` frees the row early | RED `ReadyNoIdleRunningInjection`, 337 | RED `ReadyNoIdleRunningInjection`, 689 | breaks ([chart](restart-idle-late-blind.chart.mmd)) |
| `InputPathReadyRestartRunningAfter.cfg` | fixed, person's Enter unrecorded | GREEN 2,641 | GREEN 5,790 | all hold |
| `InputPathReadyRestartRunningIgnoreScreen.cfg` | mutant: no screen check | RED `ReadyNoIdleRunningInjection`, 540 | RED `ReadyNoIdleRunningInjection`, 540 | breaks ([chart](restart-idle-ignore-screen.chart.mmd)) |
| `InputPathReadyRestartDraftAfter.cfg` | fixed, person draft | GREEN 2,634 | GREEN 506 (held front wakes only through `WatchHeld`) | all hold |
| `InputPathReadyRestartDraftNoWatch.cfg` (new) | mutant: no restored-aware watch | — | RED `ReadyPendingEventuallySettles`, `ReadyRestartIdleEventuallyReceives`, 422 | both liveness break |
| `InputPathReadyRestartHandoffAfter.cfg` | fixed, new incarnation | GREEN 2,334 | GREEN 2,334 | all hold |
| `InputPathReadyRestartHandoffIgnoreIncarnation.cfg` | mutant: no incarnation check | RED `ReadyNoEarlyInput`, 614 | RED `ReadyNoEarlyInput`, 614 | breaks |

State counts are distinct states from TLC. `LateAfter` keeps 3,626 because with the screen showing the late turn
`RowEnd` is enabled only after `Finish`, which already clears `inflight`; `LateBlind` shows the same action freeing
the row early when the screen hides the turn. `DraftAfter` shrank because a restart-draft front no longer wakes on
`Observe`/`HumanClears` or idle wakes; it is reached only through `WatchHeld`, and `DraftNoWatch` proves that path is
needed.

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
  recorded is the existing unreported-key window (`transcript-readiness.design.md:418`). The liveness premise also
  requires `preSends = 0`, the same window.
- Screen and transcript both hiding a sent turn is the `late-blind` mutant, not a safe mode. Measurement Q5 (Claude
  Code 2.1.325) found the screen shows `running UserPromptSubmit hooks…` until the transcript row lands.
- The model's `Submit` rechecks readiness atomically; the code's screen check precedes the first key by one poll, the
  same check-to-key window as the existing Idle path.
- The trailing session-metadata fix (design §5.4) does not change what a hard terminal means, so the model has no
  action for it.

## Accepted continuation boundary after a daemon restart

관측 조건: 이어지는 턴 도중 데몬 재시작, 입력은 보류 알림 후 다시 보내야 한다.

`InputPathReadyIdleRestartContinuing.cfg` retains its original rules and transitions. The daemon can
lose the continuing Stop's in-memory latch, then read a summary as Idle while the provider still runs.
`Watched::unproven` cannot observe the provider phase. Its budget can therefore retain the pending
original before the late assistant record and final Stop arrive. A retained original is never retyped
automatically; its durable `retained_retry_required` notice asks the user to resend it.

The coordinator accepted this rare resend boundary on 2026-11-17: preserving the existing observation
policy avoids adding new runtime state or guards. The two original delivery properties remain checked
and explicitly expected RED. Safety and eventual settlement remain GREEN; no other cfg expectation
or transition is changed.

| Config/rule | Before merge (`da6376ced2`) | Merged observation (`9efad98c06`) | Accepted expectation |
|---|---|---|---|
| `InputPathReadyIdleRestartContinuing.cfg` / `ReadyPendingEventuallyExecutes` | GREEN | RED | RED: resend notice, original retained |
| same / `ReadyRestartedIdleEventuallyReceives` | GREEN | RED | RED: resend notice, original retained |
| same / `ReadyPendingEventuallySettles` | GREEN | GREEN | GREEN |
| same / all safety invariants | GREEN | GREEN | GREEN |

The counterexample is `SuppressStop` → `Restart` (latch lost) → `SoftSummary` → `Observe` (shared Idle)
→ `Expire(1)` → `LateAssistant` → `Expire(2)` → `Finish`: both originals remain retained, each owing its
notice. The liveness change records that existing outcome; it does not authorize a silent discard.

Notice entry points are the owner's `effect.rs:103` unsent settlement and `retained.rs:185` notice intent,
then the drive's `owner_input.rs:308` settlement callback, `input_owner/notice.rs:68` outbox handoff,
and `retained_notice.rs:39` persistent enqueue. The static outbox source is `RetainedInputNotice`
(`src/domain/outbound/outbox/source.rs:22,77`). The PG regression
`a_restarted_idle_source_retains_with_one_resend_notice_pg` exercises the restarted source,
unproven timeout, retained intent and actual persistent outbox enqueue with duplicate replay.
Since #8128 U the Herdr first input holds a wait whose screen shows the continuing turn and delivers
it after that turn (`HerdrComposerWaitCause.tla` `own_screen`); this boundary stays for tmux.
