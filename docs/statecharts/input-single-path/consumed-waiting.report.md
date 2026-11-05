# Consumed waiting row after a refused re-adoption (#7807)

Code baseline: `b0bab6ebf693502f86c0b2e2c0307033fa411c29`.
The After column models the coordinator's impl-5 contract, including the retained current
native-idle gate and main's unconditional shutdown marker refresh. Model checking verifies
that contract; the implementation lane's entry-point
tests separately check its wiring into production.

This is a sibling extension of `InputOwnership.tla`, whose slice F already models the carried
row, mailbox episode, original input, native busy/idle state and owner wake. Its dedicated
`CWNext` splits A's terminal from the later native turn N instead of using the original
`TurnEnds`, which releases the mailbox and native turn together. Historical configs are unchanged.

## Results

| Check | Before (O-owned) | After (O-owned) | NonO/Unknown |
|---|---|---|---|
| Statechart command | exit 1, expected temporal lasso | exit 0 | exit 0 |
| Plain TLC | exit 13, temporal violation | exit 0, temporal check completed | exit 0, safety only |
| Distinct states | 65 | 159 | 74 |
| `PendingBEventuallyStarts` (`PendingB ~> BStarted`) | breaks | holds | outside this config's scope |
| `NoDuplicateStart`, `SuccessorUntouched`, `NoAReplay` | hold | hold | hold |
| `NoDuplicateAnswer` | holds | holds | holds |
| `InputNoticeDebtIndependent` | input-notice debt stays 20 | input-notice debt stays 20 | input-notice debt stays 20 |
| `RefusedMarkerConsumed` | repair implication inactive | holds | O-only implication inactive |
| `RefusedMarkerPreservedOutsideO` | nonO implication inactive | nonO implication inactive | holds for legacy and Unknown |
| `FreshShutdownMarker` | holds | holds | holds |
| `ReceiptBeforeArchive`, `NoCloseUnderBClaim`, `NoDeadlock` | hold | hold | hold |

The repository's `scripts/ci/statecharts_tlc.py` checked the original, complete
`docs/statecharts` tree with no `--root` override: **exit 0, 63 configs, 130.3 seconds,
zero mismatches**. It performs an applied-fairness precheck and
separate plain-TLC safety and temporal runs for each config with a PROPERTY. The manifest
retains Before's expected liveness failure and After's passing expectation, and adds the
safety-only NonO config. This full run replaces the earlier impl-3 target-only check; its
raw log and exact source hashes are recorded below.

The generated [Before chart](consumed-waiting-before.chart.mmd) has 18 visible states and
34 arrows; [After](consumed-waiting-after.chart.mmd) has 23 states and 45 arrows. Neither view
hides a state-changing action. Same-label steps are omitted by the renderer (23 Before,
88 After); the After view additionally hides 24 terminal self-stutters (`CWDone`). The
[NonO/Unknown chart](consumed-waiting-nono.chart.mmd) has 36 states and 66 arrows, separated by
observed ownership; it omits eight same-label steps and no state-changing action by name.

## Before lasso and code replay

The statechart report's counterexample has 17 transitions. Its final state stutters forever:
B is still pending, A's durable row remains, the mailbox is empty, the native turn is idle,
A output debt is zero and unrelated input-notice debt is twenty.

| Steps | Model action and observation | Baseline production coordinate |
|---|---|---|
| 1 | `OwnerPastesA`: busy owner paste starts A once | `router/message_handler/provider_dispatch/owner_input.rs:576` (`execute_owned`) |
| 2 | `ConsumeA`: A's exact input identity settles, while its waiting row remains | `router/message_handler/provider_dispatch/owner_input.rs:647` busy observer; `turn_orchestrator/queued_head/v2.rs:376` keeps a settled identity held during adoption |
| 3 | `ShutdownRestamps`: planned shutdown writes A's restart marker | `runtime_bootstrap/spawns.rs:179` → `inflight.rs:785`, writes at 803–805 |
| 4–5 | `SettleBoot`, `RestoreAlive`: live pane remains, A's mailbox claim is refused; successful-only marker update is not called | `recovery_engine/restore_inflight.rs:1924`; `recovery_engine/runtime.rs:325`, `:341` |
| 6–8 | Another shutdown refreshes the old marker; another boot and refused adoption retain it | `inflight.rs:803`; same restore/claim branch |
| 9–11 | A terminates, N starts, and O captures/delivers A output; input-notice work remains | native boundary predicate `tui_turn_state/completion_scan.rs:207`; channel aggregate `health/recovery/carried_turn_close.rs:376` |
| 12 | B is admitted while N remains busy | owner queue ingress; `discord/queue_io/input_owner/drive.rs:100`–112 retains the admitted front |
| 13–14 | B takes its own temporary claim; row create collides with A; B releases that claim and keeps its front | `router/message_handler/foreign_row.rs:44` → `:53` for headless; intake `:70` → `:80` |
| 15 | N becomes idle and the existing boundary wake gives B another pass | `discord/turn_presence/supervisor.rs:644`–673 owner boundary wake |
| 16–17 | B claims and collides once more, then retains its front with no remaining wake | the same `foreign_row` admission and unwind paths |
| lasso | Existing health ticks keep A because the aggregate O debt includes twenty input-notice items | `health/recovery/carried_turn_close.rs:366`–403 |

These code reads confirm the modeled branch sequence. They do not replace the implementation
lane's runtime-entry RED/GREEN test. The operational row and O-store observations are recorded
in that lane's implementation report rather than being inferred from this model.

## After behavior and retained guards

`RestoreAlive` consumes a prior-generation marker after refused adoption only when the
observed output owner is O. It does not require a separate O confirmation and does not claim
that A was readopted. Successful adoption follows its existing marker consumption and normal
owner completion (`FinishReadoptedA`). `ShutdownRestamps` always writes the current generation
in both Before and After; `FreshShutdownMarker` checks this for every shutdown, including the
second restart and nonO/Unknown rows. The model does not introduce a no-restamp exception or
accelerate stale-row reaping. The positive model assumes marker writes succeed; failed marker
I/O remains an implementation-test concern.

`OutputOwners={"owned"}` leaves both existing O configs' unconditional B liveness formula
unchanged. The separate NonO config observes `{"legacy", "unknown"}` and checks preservation
safety without claiming `PendingB ~> BStarted`. O neither captures their output nor closes
their rows in this model, and refused markers remain present after either boot. Legacy relay
completion and a later ownership-resolution event are outside this bounded safety config;
their absence is not presented as a liveness failure of the O repair. Existing input-delivery
uncertainty (`unconfirmed`) is independent of output ownership and remains a close guard.

`ForeignAdmission` contains no closer. B first holds its own `BClaim`; the closer cannot run
under that claim (`NoCloseUnderBClaim`). A conflict releases B's claim and preserves its front.
Once N becomes idle, the existing health pass can take the transition lock, commit A's receipt,
recheck A's row/episode/native-idle/delivery guards and archive A by captured identity. The
existing kickoff then wakes the owner, and B starts once. A late A callback leaves B's row
and token untouched.

The repaired P gate counts only `odebt.aOutput`. `odebt.inputNotice=20` stays constant because
its separately owned ledger does not represent unfinished A output. Before retains the
channel-wide sum. The model retains A's consumed/terminal/captured evidence and all current
native-idle, mailbox episode, transition-lock, input-delivery and row-CAS conditions; it does
not model a foreign-admission closer or removal of `native_end`.

## Fairness and bounds

- Weak fairness lets the existing owner, boot/restore, source capture, health pass, receipt
  commit, archive and kickoff execute once continuously enabled.
- The environment eventually ends N (`WF(NativeIdle)`), after B has arrived during N's busy
  interval and its first foreign conflict has released the claim. It does not promise progress
  while Claude remains busy forever, or across an unbounded series of later native turns.
- Only two shutdown/boot cycles are explored. B has at most the arrival wake and N's one idle
  edge before close; once A closes, the existing kickoff supplies B's successful wake. There is
  no new timer or fixed retry loop.
- Source proof parsing, PG/I/O failures, transcript replacement, unconfirmed-injection races,
  non-O delivery/progress, explicit Cancel/clear/stop, and more than one queued successor are outside
  this bounded scenario. Production guards are represented by their predicates; the lane's
  negative evidence, race and successor tests must exercise their rejection paths.

## Reproduce and evidence

From `docs/statecharts/input-single-path`, with a new output directory:

```sh
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.codex/skills/statechart/scripts/statechart InputOwnershipConsumedWaiting.tla \
  --config InputOwnershipConsumedWaitingBefore.cfg --view consumed-waiting.view.mjs \
  --out /tmp/i7807-before-new
~/.codex/skills/statechart/scripts/statechart InputOwnershipConsumedWaiting.tla \
  --config InputOwnershipConsumedWaitingAfter.cfg --view consumed-waiting.view.mjs \
  --out /tmp/i7807-after-new
~/.codex/skills/statechart/scripts/statechart InputOwnershipConsumedWaiting.tla \
  --config InputOwnershipConsumedWaitingNonO.cfg --view consumed-waiting.view.mjs \
  --out /tmp/i7807-nono-new
"$JAVA" -XX:+UseParallelGC -Xmx512m -cp "$TLA2TOOLS" tlc2.TLC -workers 1 \
  -metadir /tmp/i7807-plain-new -config InputOwnershipConsumedWaitingAfter.cfg \
  InputOwnershipConsumedWaiting.tla
```

From the repository root, check the complete original manifest:

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py
```

Raw impl-5 evidence is under
`/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7807/logs-impl5/`:

- `model-source/`: exact model, its base module, three configs and view used for the charts.
- `model-before/`, `model-after/`, `model-nono/`: original chart, report and `tlc.out`;
  Before additionally includes its property-specific lasso trace. Wrapper logs and exit-code
  files use the same basename.
- `plain-before.log`, `plain-after.log`, `plain-nono.log` and matching `.rc` files:
  direct plain-TLC runs of these three configs.
- `statecharts-help.log`: official command's supported arguments.
- `statecharts-full.log` and `.rc`: complete repository manifest check, without an isolated
  root or a target-only filter.
- `evidence.json`: command, environment, exit codes and SHA256 of verification inputs.

TLC 1.7.4 JAR SHA256:
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`.
Model source SHA256:
`23c519ffd318a1556ef28c9d9869fbf66bfc67def112fd54a0c0d3db03b98472`.
Runs used `/bin/zsh`, one TLC worker, Java's Parallel GC and a 512 MiB heap for plain TLC.
Each verification shell began by unsetting `TMUX` and `TMUX_PANE`. No cargo, PG or production
store access was involved in this subtask.
