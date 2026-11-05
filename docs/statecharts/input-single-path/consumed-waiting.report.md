# Consumed waiting row after a refused re-adoption (#7807)

Model contract: coordinator impl-7 (`resume-impl-7.md`), based on merged lane head
`0f27c5335a602ed315dc8287a2b5828cb7302d0d`, main base
`b2b5a672ad7c68102f869cff481532c82f00bb71`.
The original lasso describes code baseline `b0bab6ebf693502f86c0b2e2c0307033fa411c29`;
the two new RED cases isolate the defects reviewed at `bfd3c269211da84b0db70ddb988a205e8d6f8623`.
After verifies the approved contract. The implementation lane's actual intake/restart/health
RED/GREEN tests separately establish its production wiring; model PASS is not that evidence.

This sibling extends `InputOwnership.tla`'s carried-row slice F but uses `CWNext`, splitting
A's completion from the later native turn N. Historical models and configs are unchanged.

## Results

| Config | Statechart exit | Distinct states | Expected result |
|---|---|---|---|
| [Before](consumed-waiting-before.chart.mmd) | 1 | 68 | `PendingBEventuallyStarts` lasso: channel debt includes notice20 |
| [PinSupplyBefore](consumed-waiting-pin-supply-before.chart.mmd) | 1 | 68 | Same liveness lasso despite the repaired P gate: old observer supplies no pin |
| [OutcomeBefore](consumed-waiting-outcome-before.chart.mmd) | 1 | 100 | `OrphanAReceiptCompleted` breaks when N's cancellation becomes A's receipt |
| [After](consumed-waiting-after.chart.mmd) | 0 | 162 | O-owned B eventually starts exactly once; orphan A is Completed |
| [AfterStamped](consumed-waiting-after-stamped.chart.mmd) | 0 | 162 | A's own Cancelled stamp is honored in refused and successful-adoption cases |
| [InvalidPin](consumed-waiting-invalid-pin.chart.mmd) | 0 | 204 | Other-attempt, other-session and unsettled-write evidence cannot pin or close A |
| [NonO](consumed-waiting-nono.chart.mmd) | 0 | 80 | Legacy/Unknown refused markers remain; O cannot close their rows |

`NoDuplicateStart`, `NoAReplay`, `SuccessorUntouched`, `NoDuplicateAnswer`,
`InputNoticeDebtIndependent`, `RefusedMarkerConsumed`, `RefusedMarkerPreservedOutsideO`,
`FreshShutdownMarker`, `ReceiptBeforeArchive`, `NoCloseUnderBClaim`, `PinEvidenceVerified`,
`ObserverFollowsEffect`, `NoCloseWithoutPin`, `OwnStampRespected` and deadlock checks hold
in all seven configs. `OrphanAReceiptCompleted` holds except in its intentional RED case.
`PendingBEventuallyStarts` remains the original unconditional `PendingB ~> BStarted`
formula: After, AfterStamped and OutcomeBefore pass it; Before and PinSupplyBefore break it.
NonO and InvalidPin deliberately assert safety only, without promising B progress for rows
which this O repair cannot safely retire.

The official whole-repository checker checked the original `docs/statecharts` directory,
with no `--root` override or target-only filter: **exit 0, 74 configs, 138.0 seconds,
zero mismatches**. It applies a fairness precheck and checks each PROPERTY separately with
plain TLC. The seven target configs required 13 plain-TLC runs, all matching the manifest;
the source hashes remained identical to the frozen inputs throughout the complete check.

## Pin supply and proof boundaries

The old model conflated terminal input settlement with the existence of a historical native
pin. Impl-7 makes the production ordering explicit:

1. `OwnerPastesA` begins the real busy effect once.
2. `BusyEffectReturns` observes the actual run result. Only success arms this attempt;
   WriteUncertain leaves `writeSettled=FALSE`.
3. `SettleBusySubmission` records that attempt's Submitted or WriteUncertain history terminal.
4. `ObserveBusyReceipt` represents the existing answer observer finding an actual native user
   row. `ObserverFollowsEffect` checks that both effect return and history settlement precede it.
5. `SupplyBusyPin` represents the new callback decision. Only an armed, matching native user
   persists a pin through A's existing birth-episode callback. The old callback setting, a
   different attempt/session, or an unsettled write leaves `pinVerified=FALSE`.

The observer still runs for WriteUncertain. A pin is never inferred just from Submitted,
terminal history membership, or a receipt-sink wrapper. The matching predicate abstracts the
approved checks: observed range start, original session/path/file, captured pane identity,
exact attempt nonce's `RowOwner::User`, and the existing UUID/range/source/digest validation.
Those byte-level checks and real I/O remain implementation-test responsibilities.

This positive scenario begins planned shutdown after the observer's user-row decision and
pin hook have run, as the R1 test does. It does not assume that every attempt obtains a pin:
PinSupplyBefore and InvalidPin complete that same sequence without one and preserve A.
A crash before the asynchronous observer decision is outside the B-progress scenario; it is
not silently modeled as a successful save. Failed pin I/O likewise requires the production
negative tests. Neither case justifies dropping historical I verification.

## Orphan A outcome

`receiptOutcome`, A's `ownStamp`, and N's `nAborted` are independent state. The current native
idle boundary decides when the closer may run; its last aborted bit does not identify A's
outcome. In the no-episode, refused-adoption path, an existing A stamp wins, otherwise the
repaired contract commits Completed. OutcomeBefore changes only that outcome policy back to
N's bit and produces a Cancelled receipt for normally completed A.

`FinishReadoptedA` remains the separate normal-owner path for successful adoption and respects
A's own stamp. AfterStamped checks explicit Cancelled preservation through both adoption
outcomes. The model does not claim to replace the production `interrupted_close_cancels`
regression or finalizer-event tests. No historical T scanner is introduced; current native
idle is still required at close and archive.

## Counterexamples and code replay

The original Before and PinSupplyBefore lassos have 20 transitions and end with B pending,
A's row present, an empty mailbox, current native idle, output debt zero and input-notice debt
20. Both traverse actual busy return, Submitted settlement and the native-user observer.
Before retains the channel sum; PinSupplyBefore has the repaired P gate but lacks the pin.

| Path steps | Model action | Fixed-source correspondence |
|---|---|---|
| 1 | `OwnerPastesA` | Original `owner_input.rs:576` (`execute_owned`) creates/pastes the busy attempt |
| 2–3 | `BusyEffectReturns`, `SettleBusySubmission` | Reviewed `queue_io/input_owner/effect.rs:201–205` returns from `inject_owned` and collects attempt history |
| 4–5 | `ObserveBusyReceipt`, `SupplyBusyPin` | Existing `hosting/warm_followup.rs::busy_owner_observer` actual-user branch; impl-7 adds only its approved callback and the arm/source guard |
| 6–8 | `ShutdownRestamps`, `SettleBoot`, `RestoreAlive` | Original `inflight.rs:803–805` refreshes the marker; `restore_inflight.rs:1924` re-registers; refused adoption does not own A |
| 9–11 | Second shutdown, boot and restore | Same existing lifecycle, with current-generation shutdown refresh preserved |
| 12–14 | A ends, N starts, output is captured/delivered | Original native boundary predicate `completion_scan.rs:207`; O gate `carried_turn_close.rs:376` |
| 15–17 | B is admitted, claims, collides, and releases its own claim | Original `foreign_row.rs:44–53` headless / `:70–80` intake; queue front stays pending |
| 18–20 | N ends, B retries once and collides again | Existing owner boundary wake and foreign-admission unwind |
| lasso | `HealthKeepsA` | Before's aggregate gate includes notice20; reviewed closer `:171–175` refuses a missing historical pin even after P passes |

The OutcomeBefore safety path has 17 transitions. Steps 1–8 supply a real pin, mark shutdown,
restore the live pane and refuse adoption. Steps 9–15 complete normal A, capture its output,
start N, admit/refuse B and interrupt N. Step 16 takes the existing health transition lock;
step 17 commits Cancelled for A using N's bit. This is the reviewed
`carried_turn_close.rs:220,398–410` chain. It is an outcome defect even though B subsequently
starts; the separate liveness property holds in that RED config.

These coordinates describe the original and fixed-review snapshots, not mutable production
files during implementation. Source-level runtime entry tests provide the final wiring proof.

## Retained behavior and fairness

- O-only refused marker consumption never calls a refused row readopted. NonO/Unknown retain
  their marker. Every shutdown, in every config, refreshes the current generation;
  `FreshShutdownMarker` rules out the rejected restamp-skip policy.
- Input-notice debt stays at 20 in every config. Repaired P counts only A output debt. Source
  capture, current native idle, input-delivery, episode, transition-lock and archive identity
  conditions remain represented; B's claimed token prevents A's closer from running.
- Foreign admission never calls the closer. It releases B's claim and retains the queue front.
  An idle health tick commits A's receipt before archive, preserves the captured identity,
  and the existing kickoff wakes B. A late A result leaves B's row and token intact.
- Weak fairness lets each continuously enabled owner, observer, boot, source-capture, health,
  PG commit, archive and kickoff action execute. The environment eventually ends N after B's
  arrival and first foreign conflict. There are only two shutdown/boot cycles and two owner
  attempts before close; no timer or new retry loop is modeled.
- Legacy delivery/progress, PG failures and long-failure stale cleanup, restart-report branch
  wiring, pin byte parsing/I/O failures, transcript replacement, arbitrary crashes before the
  observer, repeated later native turns, explicit Cancel/clear/stop, and multiple successors
  are outside this bounded model. Their production guards and entry tests remain necessary.

## Charts and evidence

The generated charts retain every state-changing action by name. Before and PinSupplyBefore
draw 21 states/37 arrows and omit 23 same-label steps each. After and AfterStamped draw
26/48, omit 88 same-label steps and hide only 24 terminal `CWDone` stutters. OutcomeBefore
draws 25/44, omits 43 same-label steps and hides 16 terminal stutters. InvalidPin draws 23/39
and NonO 42/72. The renderer's raw report lists their same-label omissions.

Raw evidence directory:
`/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7807/logs/impl7-model/`.

- `source/`: exact model/base/config/view snapshot for all seven target checks.
- `chart-<Config>/`, `.log` and `.rc`: each statechart output, TLC logs, report and exit code.
- `source-sha256.json`: source head, merged base and hashes of all original repository
  TLA/config files, manifest and checker inputs frozen before the full check.
- `statecharts-help.log`, `statecharts-full.log`, `.rc`: supported CLI and complete official run.
- `evidence.json`: final execution counts, environment, source-equality check and full result.

Model source SHA256:
`02987b885758f9f5d2615d5c0b0ac73502991cd108e4e1d2dcab69ed0e1bae76`.
TLC 1.7.4 JAR: `/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar`, SHA256
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`.

Reproduce a target from `docs/statecharts/input-single-path` with a fresh output directory:

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.codex/skills/statechart/scripts/statechart InputOwnershipConsumedWaiting.tla \
  --config InputOwnershipConsumedWaitingAfter.cfg --view consumed-waiting.view.mjs \
  --out /tmp/i7807-impl7-after-new
```

From the repository root, using the same environment, run
`python3 scripts/ci/statecharts_tlc.py` without a root override. Verification uses `/bin/zsh`,
one TLC worker and Java Parallel GC. No cargo, PG, production store, or commit action belongs
to this model subtask.
