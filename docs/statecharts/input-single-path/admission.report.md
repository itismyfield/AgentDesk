# F5/F6 admission model integration

The integration uses F5 `107fe59871725337cac306a807cee0ee11bf7249`, F6
`0ce3611b2bd5f95901622b01bc00593a49800471`, and the main merge at `57ac1b884e`.
No production behavior is added here. The new assertions check that the model represents
existing admission behavior, using the existing `row`, `src` and `pastes` variables.

## Code branches and model repair

The following production coordinates refer to the fixed F5 source above:

| Branch | Production path | Model |
|---|---|---|
| Ordinary Discord owner, idle mailbox, busy native pane | `intake_turn.rs:1720-1771` calls `owner.into_busy_pane` and returns before inflight create/admit | `DriveIdleBusyPane` keeps `row # "carried"` and preserves `fvars`, including a marked row |
| Headless start, including an owner front with preserved admission context | `owner_input.rs:464-470` selects `start_owner_headless`; `headless_turn.rs:916-919` creates/adopts before provider execution; `foreign_row.rs:43-47` settles a marked row and retries create | `DriveStartBusy` uses `RowClear`, writes `row' = "none"`, and preserves `ep`, `leak`, `outside`, `observer` |
| Ordinary idle start | F5's create/settle/retry admission | `DriveIdle` updates `row` and explicitly preserves `observer` alongside the other F6 variables |

`row = "none"` after admission means the *previous blocking row* is gone. The newly admitted
turn's durable row is outside this abstraction. `HeadlessSettlesMarked` checks that an input
pasted by headless admission no longer has the previous marked row. It is a model fidelity
assertion of existing production behavior, not a new runtime cleanup contract. The owner
contrast assertion is scoped to the one-step admission probe, not every busy input in the
complete historical model.

## Red → green and mutants

The actual merge snapshot supplied before observer preservation was
`f56-land-merged-before-observer-preserve.tla`, SHA256
`6547c4a4d8b07362edcdaf6f31cb728e04f884791ce9bcad5760a81ee9ffeb4c`.
The automatic merge left `DriveIdle` with `row' = "none"` and
`UNCHANGED <<ep, leak, outside>>`, even after the three textual conflicts were resolved.

| Replay | Before | After |
|---|---|---|
| F4 ordinary idle, previous row none | TLC rc 75, `observer` unassigned | rc 0 |
| F5 ordinary idle, previous row none or marked | TLC rc 75, `observer` unassigned | rc 0 for both initial states |
| F4/F5 headless start into busy pane, previous row marked | rc 12, `HeadlessSettlesMarked` after one paste | rc 0, previous row cleared |
| F4/F5 ordinary owner busy shortcut, previous row marked | marked row preserved | rc 0, unchanged |
| `headlessKeepsMarked` mutant | repaired headless path leaves the previous marked row | rc 12, `HeadlessSettlesMarked` |
| `ownerClearsMarked` mutant | headless cleanup is incorrectly applied to the ordinary owner shortcut | rc 12, owner contrast assertion |

The missing-observer variant also fails the official `statecharts_tlc.py` integrity-negative
control with rc 1 over an expected-empty row: its underlying TLC rc 75 is reported as an
untrustworthy run. It is deliberately **not** added to `expected.tsv` as an expected broken
invariant. The permanent `InputOwnershipIdleMerge.cfg` runs the legacy `DriveIdle`, while
F6's isolated observer configs continue checking their own actions.

## Safety and liveness matrix

The official invariant/deadlock checker ran all **32 configs**, rc **0**, 118.8 seconds.
A checker row marked `ok` means its failures match `expected.tsv`; the Before/mutant columns
below intentionally remain RED. The merged main checker is invariant-only; temporal properties
were checked separately with plain TLC and explicit weak fairness.

| Config | Safety / deadlock result | Separate temporal result |
|---|---|---|
| `InputOwnershipF4.cfg` | expected `NoSilentLoss`, `NoSilentLossObserved`; all other rules, including headless fidelity, hold | no global eventual-delivery assertion; selected headless/owner busy admission reaches one paste under WF |
| `InputOwnershipF5.cfg` | all 15 invariants and `NoDeadlock` hold | no global eventual-delivery assertion; selected headless busy, owner busy and ordinary idle admission reach one paste under WF |
| `InputOwnershipF6.cfg` | all six invariants and `NoDeadlock` hold | `ObserverEventuallyDelivered` holds, 24 states |
| `InputOwnershipF6Before.cfg` | `ObserverResumes` breaks | temporal violation, rc 13, 18 states |
| `InputOwnershipF6NoWake.cfg` | `ObserverResumes` breaks | temporal violation, rc 13, 18 states |
| `InputOwnershipF6DoubleWake.cfg` | `ObserverAttachmentWakesOnce` breaks | delivery property still holds, 24 states; wake-count safety catches this mutant |
| `InputOwnershipF6InitiallyAttached.cfg` | all rules hold | delivery property holds, 8 states |
| `InputOwnershipF6NoFront.cfg` | all rules hold | property is vacuously true: no input waits, 3 states |
| `InputOwnershipIdleMerge.cfg` | all rules hold for previous row none and marked | both initial states reach one paste under admission WF |
| `InputOwnershipHeadlessMarked.cfg` | headless fidelity and deadlock hold | one paste under admission WF |
| `InputOwnershipHeadlessMarkedMutant.cfg` | `HeadlessSettlesMarked` breaks | one paste still occurs; progress alone cannot detect wrong row cleanup |
| `InputOwnershipOwnerBusyMarked.cfg` | owner contrast and deadlock hold | one paste under admission WF |
| `InputOwnershipOwnerBusyMarkedMutant.cfg` | owner contrast breaks | one paste still occurs; progress alone cannot detect over-broad cleanup |

The admission temporal probe has one enabled handoff followed by terminal stuttering and uses
`WF_vars(AdmissionNext)`, checking `<> (pastes = 1)`. This is intentionally narrower than a
complete historical channel, whose screen can remain blocked indefinitely. The observer
property retains its existing `ObserverSpec` fairness, including token release. No liveness
claim is extended to a short native Busy observation missed by presence, busy submission
policy, permanent provider refusal, or a bridge token which never releases.

## F7 merge surfaces

The available F7 reference is **base** `6d7c1851e5`; no F7 tip/diff is claimed here.
These are the concrete overlapping surfaces when that lane rebases onto the merged model:

| F7 base coordinates | Merged surface / preservation requirement |
|---|---|
| `VARIABLES:21`, `vars:65`, `fvars:68`, `Init:78` | retain F5's marked-row initialization and F6's observer record; every action must assign/preserve every variable |
| `DriveIdleBusyPane:168-176` | ordinary owner busy shortcut preserves the previous marked row; no admission cleanup |
| `DriveStartBusy:182-193` | headless admission uses `RowClear`, clears the previous row and preserves `observer` |
| `DriveIdle:202-214` | F5's explicit row update must preserve `observer`; the merge probe must remain runnable |
| `DriveMeetsRow:478`, `CarriedCloses:491` | retain F5's row behavior and the explicit F6 observer preservation |
| `Next:524`, model tail | keep all existing action names, the observer actions/spec and admission fidelity assertion; new F7 variables also need preservation in standalone `ObserverLegacy` |

`RowClear`/`Retries` are F5 additions absent from that F7 base. The standalone observer record,
`ObserverTurnReleases`, and admission probe module are also absent there. A clean textual merge
is insufficient: run both legacy `Next` configs and the isolated observer configs after rebase.

## Reproduction and raw evidence

Use `JAVA=/opt/homebrew/opt/openjdk/bin/java` and
`TLA2TOOLS=$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar`:

```sh
python3 scripts/ci/statecharts_tlc.py
```

The temporal scratch configs replace `INIT/NEXT` with `SPECIFICATION ObserverSpec` and
`PROPERTY ObserverEventuallyDelivered`, or extend `InputOwnershipAdmission` with:

```tla
AdmissionSpec == AdmissionInit /\ [][AdmissionNext]_vars /\ WF_vars(AdmissionNext)
AdmissionEventuallyPastes == <> (pastes = 1)
```

Lane evidence is `logs/land1-statecharts-all.log`, `land1-statecharts-integrity-negative.log`,
`land1-statecharts-summary.md/json`, and `land1-statecharts-probes/` (raw TLC outputs,
immutable before/fixed snapshots and the exact temporal configs). The integrity-negative
control is separate from the expected semantic counterexamples. No cargo or operational
state was used for this model repair.
