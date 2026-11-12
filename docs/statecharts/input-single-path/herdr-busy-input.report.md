# Herdr Claude busy input — design model (#8114)

Code baseline: `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`.
This checks a proposed design, not implemented Rust behavior. No cargo build/test or runtime command was run.

## Existing model assumption audit

There is no explicit `herdr && busy => hold` rule in the existing models. This is a missing host distinction, rather than a proved host exclusion:

| Existing coordinate | Existing abstraction | Consequence |
|---|---|---|
| `InputOwnership.tla:187-204`, `:215-224`, `:232-242` | `OwnedPaste`/`DriveBusy`/`DriveIdleBusyPane` branch on screen/native state, with no host value | They model one writer but cannot detect a Herdr-only exclusion. |
| `InputOwnership.tla:883-927` | F7 keeps a busy original when its transcript source is missing and retries at a boundary | This is an adjacent source-availability counterexample, not a proof that every Herdr busy input must wait. |
| `InputPath.tla:116-145` | An unread composer holds before the first key; `HerdrNoCtrlS` changes draft parking | It lacks a host-specific transcript readiness proof. |
| `HerdrBusyHandoff.tla:137-161`, `HerdrBusyHandoffBlockedComposerBoundary.cfg` | External `Blocked`/draft refuses the first composer and disables `WakeFirst` until cleared | This is the documented expected-RED composer boundary after handoff. `Blocked` must not be equated with native busy. |

`HerdrBusyInput` is a small focused refinement in the existing `input-single-path` area. It projects the existing owner/front, preparation hold, may-send fence and receipt settlement; it does not replace the broader models.

## Code-to-action projection

| Model action/value | Production coordinate / required design behavior |
|---|---|
| `HostReady`, `Drive` | `router/message_handler/intake_turn.rs:1109-1113,1751-1766` and `router/message_handler/provider_dispatch/owner_input.rs:1060-1086`: make the existing owned busy predicates/read host-aware. `intake_gate/busy_inject.rs:82-99` is the legacy offer and retains its Herdr exclusion; `owner_takes` keeps owner traffic on one path. |
| `ObserveEarly`, `LegacyDiagnostic` | `intake_turn.rs:1551-1552`: observe an eligible common busy target before entering the legacy tmux diagnostic block. This is entry ordering, without a new host/readiness classification policy. |
| `ClearState`, `ExistingIdle` | `claude/herdr_turn.rs:293,320,365`: a verified same-execution `awaited_clear` delegates to the existing Idle/cleared-source path in both main and the proposal. It must never be fed to strict Busy source validation. |
| `OReady` | `tui_o/turn_mode.rs:41`, `tui_o/writer/host.rs:372`: confirmed transcript turn mode and a writer that currently accepts the channel are prerequisites for the Herdr extension. Configuration alone is insufficient. |
| No extra answer observer | `tui_prompt_dedupe/injected_steer.rs:243-249`: confirmed O owns the row's answer. The design calls the existing owned busy path with no extra `until_done`/legacy busy observer. |
| `InitialBlock`, `Notify`, `RetryHeld` | The existing preparation failure remains an owned original; publish its held state, watch readiness and wake the same front after a genuine pre-key cause clears. |
| `may`, `Effect`, `NativeProof`, `Settle` | Existing durable may-send boundary, one body+Enter transport write, then exact provider user/queued-row evidence. Transport return is not modeled as proof. |
| `Crash`, `Boot` | Existing attempt/input records survive one process restart. A may-send attempt restores as uncertain and is never repasted; only provider evidence settles it. |
| `Replace`, `target/generation` | The captured pane incarnation cannot silently rebind after reservation. Replacement makes the old attempt uncertain/visible with zero new-target effect. |

## Rules and assumptions

- Safety: `InputAtMostOnce` (one input is never delivered twice), `CompletedExactlyOnce` (settled completion has exactly one delivery and native proof), `NoSilentLoss` (the admitted original is never silently removed), `NoForeignTarget` (replacement panes get no bytes).
- Liveness: `BusyResolvedWithoutTurnEnd` eventually delivers the input or shows a retained pre-key/uncertain hold. `HealthyBusyEventuallyInjected` additionally requires delivery when composer/native proof and O readiness are valid. A visible hold alone must not make that rule pass. `ClearedHoldEventuallyInjected` requires one delivery after a real temporary pre-key refusal clears.
- `ReceiptEventuallySettled` checks that a delivered input with eventual provider evidence settles once. `ScenarioReached` prevents restart/replacement controls from passing without reaching the scenario.
- There is deliberately no turn-end transition. Thus GREEN means progress does not depend on the previous turn ending; it is not a wall-clock latency bound.
- Fairness applies to the local owner/writer/notice/boot actions and provider receipt generation after actual acceptance. A permanent genuine composer or O-not-ready condition is allowed to retain a visible hold. The provider acceptance/evidence relation is an explicit assumption to verify with Rust integration fixtures.
- Source recovery assumes the existing `held_watch` probes the exact bound source and wakes the retained front when that source becomes readable. It must not depend on a source-publication helper that waits for the active turn/end boundary; the implementation fixture must keep the native turn busy during recovery.
- One durable admitted front and the existing serialized owner/lease are assumed. The model has at most one crash and one pane replacement; it does not prove arbitrary concurrent queues, durable store transactions, notice delivery transport, output answer uniqueness, hook authentication or Herdr's PTY implementation.
- People, allowed bots and person-origin synthetic inputs enter through the same admitted owner. Machine start/dispatch inputs do not obtain a new right to interrupt a native busy turn. Admission/source classification is tested by the implementation slice, not duplicated as a new owner in this model.
- Cancel/Terminate/clear/stop retain their existing revocation/fence policies; this design adds no control operation. Their before-first-key cancellation and native stop proofs remain in `InputPath`/`InputNativeStop`. The implementation must run those adjacent contracts after sharing the busy writer.

## TLC results — initial design snapshot

At the initial design snapshot, all 22 configs were run against that snapshot's exact model source. The current P2 extension and its fresh results are recorded below; the old table is historical evidence. Eight baseline/proposal scenario pairs plus six one-rule mutants finished; no property is listed as unchecked. Every cfg selects only applicable temporal properties, avoiding a vacuous constant formula that TLC optimizes away.

| cfg suffix | exit | states / steps | Broken rules | Coverage |
|---|---:|---:|---|---|
| Before | 1 | 3 / 5 | HealthyBusyEventuallyInjected | 완료 |
| Blocked | 0 | 3 / 5 | 없음 | 완료 |
| BlockedBefore | 0 | 3 / 5 | 없음 | 완료 |
| Cleared | 0 | 15 / 31 | 없음 | 완료 |
| ClearedBefore | 1 | 6 / 13 | ClearedHoldEventuallyInjected | 완료 |
| DropHeld | 1 | 5 / 9 | NoSilentLoss, BusyResolvedWithoutTurnEnd | 완료 |
| ExcludeHerdr | 1 | 3 / 5 | HealthyBusyEventuallyInjected | 완료 |
| Fixed | 0 | 5 / 9 | 없음 | 완료 |
| NoBootWake | 1 | 4 / 7 | BusyResolvedWithoutTurnEnd | 완료 |
| RebindTarget | 1 | 6 / 11 | NoForeignTarget | 완료 |
| Replace | 0 | 5 / 9 | 없음 | 완료 |
| ReplaceBefore | 1 | 3 / 5 | ScenarioReached | 완료 |
| ReplayUnknown | 1 | 24 / 55 | InputAtMostOnce, CompletedExactlyOnce, ReceiptEventuallySettled | 완료 |
| RestartMay | 0 | 5 / 9 | 없음 | 완료 |
| RestartMayBefore | 1 | 3 / 5 | ScenarioReached | 완료 |
| RestartSent | 0 | 12 / 26 | 없음 | 완료 |
| RestartSentBefore | 1 | 3 / 5 | ScenarioReached | 완료 |
| SilentHold | 1 | 2 / 3 | BusyResolvedWithoutTurnEnd | 완료 |
| Tmux | 0 | 5 / 9 | 없음 | 완료 |
| TmuxBefore | 0 | 5 / 9 | 없음 | 완료 |
| WriterHeld | 0 | 3 / 5 | 없음 | 완료 |
| WriterHeldBefore | 0 | 3 / 5 | 없음 | 완료 |


Baseline/proposal pairs: `Before/Fixed`, `TmuxBefore/Tmux`, `BlockedBefore/Blocked`, `ClearedBefore/Cleared`, `RestartMayBefore/RestartMay`, `RestartSentBefore/RestartSent`, `ReplaceBefore/Replace`, `WriterHeldBefore/WriterHeld`.

The `Before` no-key path is code-confirmed by the host exclusions and the reported first-key composer refusal. The model deliberately grants baseline the best-case visible hold, irrespective of whether the current notice path already displays it: the owned busy original can still remain held forever while the current turn never ends. The generic visible-hold rule passes already; the healthy immediate-injection rule is RED. This explicitly prevents a display-only fix from being mistaken for delivery repair. For restart/replacement baseline controls, `ScenarioReached` is RED because the old host gate prevents reaching the send boundary at all, rather than because baseline restart recovery was proved broken.

The proposal is GREEN for healthy Herdr/tmux injection, persistent genuine hold, recovery from a temporary hold, both sides of the may-send crash boundary, missing O readiness, and pane replacement. The six mutants are intentionally RED: restoring Herdr exclusion, suppressing held notices, dropping custody, replaying uncertain attempts, removing boot wake, and accepting another pane incarnation. `ReplayUnknown` reaches two effects in six steps; its consequent completion/settlement failures are also expected.

## Charts and reproduction

- [Proposed flow, including failure-to-held and recovery](herdr-busy-input.chart.mmd) is the generated `Cleared` chart.
- [Healthy Herdr baseline counterexample](herdr-busy-input.before.chart.mmd) shows the permanent visible hold.
- Charts coalesce same-label steps, including provider proof arrival and environmental composer clearing. The underlying TLC output retains those steps.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.codex/skills/statechart/scripts/statechart \
  docs/statecharts/input-single-path/HerdrBusyInput.tla \
  --config docs/statecharts/input-single-path/HerdrBusyInputFixed.cfg \
  --view docs/statecharts/input-single-path/herdr-busy-input.view.mjs \
  --out /tmp/i8114-fixed-new
```

Raw wrapper/TLC logs: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8114/logs/sc-out-checked/<suffix>/` (`tlc.out`, rule-specific output, generated `report.md`). `run-results.json` records every cfg exit. Preliminary directories `sc-out`/`sc-out-final` are superseded by `sc-out-checked`; an early interrupted exploratory runner was stopped and left no process.

`docs/statecharts/expected.tsv` receives exactly 22 new rows for this model; no existing row/rule/threshold changes. New rows were generated from the cfg filename list and observed invariant/property violations, then checked with `scripts/ci/statecharts_tlc.py` using an isolated root containing byte-identical copies of these 22 configs and model. The isolated root keeps this design-only check from running the whole repository model suite. Checker: exit 0, 22/22 cfg matched, 86 actual TLC runs plus 22 applied-fairness prechecks, 161.1 seconds. Result is recorded in the lane's `logs/tlc-ci-check.log`. Wrapper raw TLC output has 32 `.out` files across 22 cfg directories. Model/config/view/jar SHA256 values and selected environment are in `logs/state-model-sha256.json` (Java Homebrew OpenJDK 27.0.1.0.3; PG/cargo not run).


Plain checker command (the isolated root contains the exact new manifest rows and byte-identical model/config copies):

```sh
python3 scripts/ci/statecharts_tlc.py \
  --root /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8114/logs/tlc-ci \
  --jobs 1
```

Manifest registration command: `python3 /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8114/logs/state-model-manifest.py`; it reproduces only this model's 22 new expected rows on stdout from the observed wrapper reports. The repository manifest contains those new rows and preserves every old row byte for byte.


Chart-label correction: the sent state now reads `본문·Enter 기록, 영수증 대기` (body and Enter written, receipt pending); only `NativeProof` establishes the provider receipt. Before and Cleared were regenerated with the official statechart tool into `logs/sc-out-chart-labels` (expected exits 1 and 0, respectively). The TLA module and all 22 cfg SHA256 values remain identical to the 22-cfg/86-run CI check. `logs/sc-out-chart-labels/hashes-and-runs.json` records the old/new view hashes and unchanged source hashes; the earlier SHA file remains the historical proof record. No all-cfg source recheck was needed for this display-label change.


## P2 extension — early observation and existing clear delegation

Implementation-resume baseline: `6e9bf145ce5f649bda77a9ffd6c071d54cc9b254` (the prior design-only commit). This extension remains design-only. The target-host boundary is pending the parent lane's `DESIGN_DELTA`; these results neither establish a new host classification policy nor claim implemented Rust behavior.

The model now makes two existing entry boundaries explicit:

1. `Entry=early` denotes an eligible common-target busy candidate. `ObserveEarly` runs before the unrelated legacy tmux diagnostic. `NoUnrelatedEarlyDiagnostic` catches placing the common path inside/after that diagnostic, even when eventual delivery would otherwise succeed. The parameter does not mean that every early Idle/Cold input is Busy and does not select a host.
2. `ClearState=verified` means the existing same-nonce `awaited_clear` helper has authorized the pending clear. Both Before and Fixed delegate to `ExistingIdle`; **main already supports this path and both controls must be GREEN**. `VerifiedClearNeverBusy` and `VerifiedClearEventuallyDelegates` prevent a proposed strict Busy guard from blocking it. `IgnoreClear` is a deliberately broken proposal mutant, not a historical main bug. `ClearState=unverified` conveys no delegation authority; its controls set `InitialBlock=TRUE` to represent the independently uncertain common source observation. They do not introduce a new policy that every absent/unverified clear forces Held.

`ExistingIdle` is a delegation boundary. Its write action abstracts the existing Idle readiness/clear-source creation contract; a clear marker grants no direct permission to write keys. Exact source creation, nonce/target checks and Idle readiness remain required Rust integration contracts. The model does not invent those host rules.

| New cfg suffix | exit | states / steps | Broken rules | Coverage |
|---|---:|---:|---|---|
| EarlyBefore | 1 | 4 / 7 | NoUnrelatedEarlyDiagnostic, HealthyBusyEventuallyInjected | 완료 |
| EarlyFixed | 0 | 6 / 11 | 없음 | 완료 |
| EarlyDiagnosticFirst | 1 | 6 / 11 | NoUnrelatedEarlyDiagnostic | 완료 |
| AwaitedClearBefore | 0 | 5 / 9 | 없음 | 완료 |
| AwaitedClearFixed | 0 | 5 / 9 | 없음 | 완료 |
| AwaitedClearBusy | 1 | 5 / 9 | VerifiedClearNeverBusy, VerifiedClearEventuallyDelegates | 완료 |
| AwaitedClearUnverifiedBefore | 0 | 3 / 5 | 없음 | 완료 |
| AwaitedClearUnverifiedFixed | 0 | 3 / 5 | 없음 | 완료 |


Early counterexample: original front → legacy tmux diagnostic before common target observation; the ordering invariant breaks immediately. The main Before also retains the healthy busy input. Fixed observes the common target first and writes through the one existing owner path. The `DiagnosticFirst` mutant still eventually writes once, proving the ordering invariant adds coverage beyond healthy-delivery liveness.

Verified clear controls: original front → existing Idle delegation → body/Enter write → native receipt settlement, exactly once. The bad strict-Busy proposal instead writes through Busy and never reaches existing Idle. The direct main Before and Fixed both pass; there is no claim that main's pending clear path was broken.

- [Early Fixed chart](herdr-busy-input.early.chart.mmd), [early Before counterexample](herdr-busy-input.early-before.chart.mmd).
- [Existing clear delegation](herdr-busy-input.clear.chart.mmd), [strict-Busy proposal mutant](herdr-busy-input.clear-busy-mutant.chart.mmd).
- New P2 wrapper logs and original exit reports: `logs/sc-impl1/p2-final/`. Earlier `logs/sc-impl1/AwaitedClearBefore/` modeled an incorrect first draft that treated main's clear as a Busy hold; it is superseded and must not be used as main evidence.
- The original 22 expected.tsv rows remain byte-identical. Their 22 observed violation sets remain identical after adding `Entry=late`, `ClearState=none`. Exactly 8 new P2 manifest rows are added; no old expected set is weakened.
- The official plain checker runs all 30 final configs against byte-identical isolated source copies at `logs/sc-impl1/tlc-ci`. Its per-invocation JVM stdout/config/exit is preserved in `logs/sc-impl1/tlc-ci-raw`, including the applied-fairness checks. Final checker: exit 0; 30/30 expected sets match, 16 GREEN and 14 expected RED, 119 actual TLC runs plus 30 applied-fairness prechecks in 108.4 seconds. All 149 JVM stdout/config/exit records are retained; final model/config/view hashes and raw exit distribution are in `logs/sc-impl1/final-hashes-and-counts.json`. No property is unexecuted, no model process remains, and no Rust build/test/runtime operation was performed.

Reproduce the final isolated checker with raw capture:

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8114/logs/sc-impl1/run-ci-with-raw.py \
  --root /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8114/logs/sc-impl1/tlc-ci \
  --jobs 1
```
