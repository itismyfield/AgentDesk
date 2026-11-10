# Server campaign: one verification run (#7438 E3-1 design leg)

Code reference: `975200a8e73aa169b22f4d5c91e90bc54dd93693`. Design reference: bundle 2 design r3 plus the r4 gap supplement (lane documents, not in this repository).
The commands that record verification facts do not exist yet, and the execution path is dormant in production.
This is a design model: it checks the rules the new commands must follow against the landed state machine. It is not a test of the code.

## What is modelled

One node, one registered verification execution (V) with a preparation phase and a test phase, the Work attempt that owns V and the Review attempt that may consume it.
`kind` picks a start check (must be collected before review) or a finish check (a review prerequisite).
The environment may stop the campaign (now, or after the stage), escalate the stop, stop the single task, and crash the server once. Processes survive a crash; the driver's in-memory handles do not.
The Work session's launch may still be in flight when a stop arrives (`Launches`), and the subject slot takes one of two candidate values (`SubjectValues`).

| Code | Model |
|---|---|
| `CampaignControl`, `StopMode`, `Run.control_version` (`model.rs`) | `control`, `stop`, `cv` |
| stop acknowledgements and confirmation (`control.rs:351-443`) | `acked`, `confirmed`, `AckFence`, `ConfirmFence`, `ConfirmStopped` |
| `NodeRuntime.stage`, `NodeFence` | `stage`, `epoch`, `interrupted` |
| `Attempt.outcome` | `attA`, `attR` |
| `FinishGrant` | `grant` |
| the Work attempt's Launch action, `Attempt.started` (`control.rs:832-841`) | `launch`, `LaunchStarted`, `LaunchUnsent` |
| proposed: checks kept for a launch in flight at a finish stop | `prov` |
| V slots and `cancellation`/`collection` (`verification_run.rs`) | `subject`, `first`, `plan`, `inb`, `vstate` |
| `Slot::bind` and `preserves_execution_facts` (`verification_run.rs`, `write.rs`) | the guard of `Materialize`, rule `FillOnce` |
| `ActionState` of each phase action | `act` |
| phase initiation and result | `init`, `res` |
| `validate_verification_start` (`stage_fsm.rs:394-488`) | `Auth` |
| `revoke_unstarted`, `interrupt_node` (`stage_fsm.rs:537-565`, `:577-641`) | `RevokedAct`, `RevokedV`, `Interrupt`, `Close` |
| G1 gate and host journal (`host_gate.rs`) | `gateCv`, `gateEpoch`, `journal`, `jres`, `delivered`, `GateInitiate`, `GateRefuse`, `GateFinish`, `Deliver` |
| runner process and its nonce (`verification_runner.rs`) | `proc`, `starts`, `ProcExit`, `ObserveNonce` |
| driver memory lost by a restart | `lsrc`, `lcv`, `lep`, `fresh`, `handle`, `Crash`, `Restart` |

Sealing the candidate and binding the subject are one step (`Materialize`); it needs the session's output, so a Work attempt needs its launch receipt first. Review never begins while stopping; that is a guard, not a separate rule.

## Rules

Safety: `NoStartWithoutAuthority`, `StartedAtMostOnce`, `NoPassWithoutCollection`, `ResultNeedsStart`, `StoppedMeansQuiet`, `ImmediateRevokes`, `NoCancelWhileAuthorized`, `FillOnce` (the subject keeps the first value it was bound to), `FinishScopeKept` (a finishing attempt whose session did start may still run its open check).
Progress: `StopNowCompletes`, `StopFinishCompletes`, `VSettles`, `DeliveredEventually`, `UnsealedClosed` (a V that never got a sealed subject is closed once the campaign stopped).

Two fairness assumptions are kept apart. `SpecQuietSession` has weak fairness on the driver, process exit and restart only. `Spec` adds weak fairness on `SessionAnswers`: the steps that need the session's own submission (the output that gets sealed, a work result no start check decides, the review verdict).
`StopNowCompletes` holds without the session assumption (`…QuietSession`); `StopFinishCompletes` needs it.

## Configs

| Config | Changed constant | Breaks |
|---|---|---|
| `CampaignVerification` | proposed design | none |
| `…LateLaunch` | the Work session launch is still in flight at the start | none |
| `…TwoValues` | two candidate subject values, safety rules only | none |
| `…ClaimKept` (mutant) | revocation keeps claims, a claim is enough to launch | `ImmediateRevokes`, `NoStartWithoutAuthority` |
| `…NoRedelivery` (mutant) | recovery does not offer a kept result again | `DeliveredEventually`, `StopNowCompletes`, `StopFinishCompletes`, `VSettles` |
| `…FactsRefused` (mutant) | facts are refused after an Immediate stop | `DeliveredEventually`, `StopNowCompletes`, `UnsealedClosed`, `VSettles` |
| `…Overwrite` (mutant) | a bound subject may be bound again to the other value, safety rules only | `FillOnce` |
| `…LateLaunchBefore` | a finish stop drops the checks of a launch in flight, as `RequestStop` does | `FinishScopeKept` |
| `…QuietSession` | no assumption that the session ever submits (`SpecQuietSession`) | `StopFinishCompletes` |
| `…SettleBefore` | settle only collects, as the landed `collect` does | `StopFinishCompletes`, `UnsealedClosed`, `VSettles` |
| `…NotSentBefore` | a refused start always cancels the action, as `observe_action` does | `NoCancelWhileAuthorized` |
| `…FencedStartBefore` | start receipts follow the `observe_action` fence check | `DeliveredEventually`, `StopFinishCompletes`, `VSettles` |
| `…FinishDeadBefore` | a finishing attempt whose V can never pass keeps waiting | `StopFinishCompletes` |
| `…InterruptBefore` | an interrupted task's attempt is never closed | `StopFinishCompletes` |
| `…Indeterminate` | the server may die between the journal intent and the receipt | `StopNowCompletes`, `StopFinishCompletes`, `VSettles` |
| `…IndeterminateSettled` | the same, plus: an unresolved start with no live process ends as interrupted | none |
| `…Chart` | chart-sized: start check, test phase only, no crash, no single-task stop | none |

`scripts/ci/statecharts_tlc.py` over these seventeen configs finished in 267 seconds with four jobs; the slowest config took 130 seconds.
In `SettleBefore`, `FencedStartBefore`, `FinishDeadBefore` and `InterruptBefore` only the finish-stage stop fails to complete: escalating to an Immediate stop ends each of them.
The base config reaches 145,087 distinct states. The chart is drawn from `…Chart` (482 states) and hides bindings, claims, the launch hand-off, fence bookkeeping, process exit, delivery and restart.
For a config that is expected to break, pass `--view verification-trace.view.mjs`: it labels every step, which the statechart command requires on a counterexample path. With it the mutant configs exit 1 and the base config exits 0.

## Findings checked against the code

Each "Before" config encodes what happens if the new commands call the landed methods as they are.

1. **A V whose remaining phases were cancelled never closes** (`SettleBefore`). `RequestStop` cancels unstarted phase actions without touching V (`control.rs:410-416`). `revoke_unstarted` skips a collected V and, when `cancel` fails, leaves V open (`stage_fsm.rs:548-554`). `collect` returns `NotFinished` while any phase has no result, so a V with a cancelled later phase is never collected (`verification_run.rs:1231-1233`); production code never assigns `collection`. Under a finish-stage stop a collected V can also keep a Planned phase action, so the stop never becomes quiet. Proposed: settling either collects V or closes it as cancelled when no remaining phase can start, and cancels the unstarted phase actions it leaves behind.
2. **A refused start cancels a phase its holder may still run** (`NotSentBefore`). `observe_action` turns every `NotStarted` into Cancelled (`control.rs:778-788`), while `reconcile_initiation(Unstarted)` returns the phase to Planned (`verification_run.rs:1006-1013`). The gate tells a stale launch from a revoked one (`host_gate.rs:615-629`). Proposed: the action returns to Planned while a holder is valid and is cancelled only when it is revoked.
3. **A start the gate already made cannot be recorded after the fence acknowledgement** (`FencedStartBefore`). `observe_action` refuses a new start receipt once the target acknowledged the stop unless the action is in a finish grant (`control.rs:751-765`); `interrupt_node` removes the grant (`stage_fsm.rs:629-632`). The process then runs with no record and the stop never completes unless the user escalates. Proposed: a start receipt proven by the gate journal is always recorded.
4. **A finish-stage stop waits forever on a check that was interrupted or cancelled** (`FinishDeadBefore`). The stage verdict stays "wait". Proposed: close that attempt as user-interrupted.
5. **A single-task stop under a finish-stage stop never closes the attempt** (`InterruptBefore`). No landed transition confirms it; this is the r4 §2.6 rule and belongs to E3-2.
6. **A start lost between the journal intent and the receipt has no end** (`Indeterminate`). When no live process carries the nonce (`host_gate.rs:578-589`), neither a start nor a non-start is proven and `cancel`, `collect` and the result methods all refuse. Proposed for the driver slice (D1): end the phase as an interruption and never rerun it (`IndeterminateSettled`).
7. **A launch receipt that arrives after a finish-stage stop leaves its attempt without finish authority** (`LateLaunchBefore`: stop after this stage, then the session's start receipt). `finish_grants` freezes only attempts already marked started (`stage_fsm.rs:810-817`), `RequestStop` cancels the unstarted phase actions outside the grants (`control.rs:410-416`), and the late receipt only adds the attempt to `finish_attempts` (`control.rs:832-841`). The session then runs a stage whose checks can never start. Proposed: the checks of a launch in flight are kept until the launch resolves; a proven start turns them into the grant, a proven non-start revokes them and closes the attempt.
8. **A finish-stage stop waits on the session** (`QuietSession`: review begins, stop after this stage, the review session never submits). This is the stated assumption of `StopFinishCompletes`, not a defect: the user ends it with an Immediate stop, which completes without the session.

`Overwrite` has no finding in the code: `Slot::bind` refuses a different value and `preserves_execution_facts` refuses the write. The config shows the rule tells the two apart.

## Limits

- One V and one node. Several V on one attempt, and several nodes, are not explored.
- TUI sessions and their Gone observation are not modelled; an interrupted task is quiet when its phase actions are. A launch proven started after an Immediate stop leaves a session to end, which is outside this model.
- Only the subject slot has two values; the plan and phase inputs stay booleans. Only the Work session's launch is modelled, the Review session's is not.
- A launch refused while its holder is still valid stays in flight (it is planned again), so `LaunchUnsent` exists only after a stop or a single-task stop.
- The driver killing processes on an Immediate stop is not modelled; a process exits by itself under fairness.
- One crash at most. Repeated refused starts are not bounded here.
- A campaign may reach Stopped while V is still open and be closed by a later settle; the model allows it.
