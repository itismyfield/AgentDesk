# Missing Bound execution: model and implementation checks

Source coordinate: `1161162a18c87a0514f53dd7ae5dac3c830149cb`, fetched and confirmed equal to `origin/main` at lane start. The model records the frozen failure and the repair policy. Model checks alone do not establish the production result; runtime and static evidence is recorded separately in the implementation lane report.

The issue and source confirm two independent gaps: reconnect omits Missing from cleanup (`herdr_reader.rs:487`), and idle admission leaves the front Held (`owned_busy.rs:90`). A third acceptance gap is reached after repairing those: a failed replacement launch goes through `provider_dispatch/start_failure.rs:83` and `queued_head/v2/start_failure.rs:95`, which retries or retains the original. A retire-only fix therefore still violates the requested reason+disposal rule.

Existing models were inspected: `HerdrSession` models Pending/Bound and pane loss, `HerdrRootRecovery` models strict replaced-root recovery, and `HerdrHandoffHeld`/`HerdrHandoffLaunch` model legacy handoff and failure custody. None includes the missing-pane early admission path. The new small model composes just that path. See the [68-row entrypoint table](herdr-missing-bound.entrypoints.md) and [chart](herdr-missing-bound.chart.mmd).

## Rules and limits

- `MissingRetired`: after a successful exact Missing judgement/CAS, the old Bound row is retired. Failed reads, unavailable writes and a successor nonce are not permission to retire; those inputs must instead be refused visibly.
- `NoIdleHeld` and `NoRetainedRefusal`: idle refusal leaves neither a permanent watch nor retained failure custody. Working-turn temporary holds remain allowed until their real boundary.
- `InputResolves` (PROPERTY): the human input eventually becomes delivered or discarded. `NoticeOnce` requires exactly one reason for a discarded input.
- `NoUnknownRetire`, `NoSuccessorRetire`, `NoOldDelivery`: unreadable evidence is not Missing, a CAS cannot retire the successor, and a Missing input cannot be delivered on the old execution.

Weak fairness is applied to actual enabled reconnect/input/read/proof/CAS/launch/settlement actions, including the existing working-turn boundary. It does not assume a dead pane's probe ever changes. Read, CAS, launch and reason publication each have a final result in this abstraction; infrastructure-level indefinite calls are outside the proof. `Reject` abstracts one publication plus exact claim disposal, and still needs integration tests.

`Launch` projects the existing Pending/location/evidence/first-key protocol. A replacement means a new hosted nonce/pane; this design does not add a provider-native session reset policy. The retire-only failure cfg projects the existing finite ordinary-start retry budget into retained failure custody; it is a code-confirmed design counterexample, not a Rust execution trace. Cancel/clear project explicit removal acknowledgements. Stop preserves the input and releases the active turn; terminate uses its existing end proof, preserves the input and allows normal admission afterward. These cfgs do not verify the entire control implementations.

## Checked cfgs: frozen before / repaired policy

All 19 cfgs were checked with the repository's `scripts/ci/statecharts_tlc.py`: 46 TLC rule runs, all expected rule sets matched. Four Before cfgs reproduce the old gap, one RetireOnly cfg reproduces incomplete repair, and 14 Fixed cfgs satisfy the proposed policy. “Fixed” identifies the repaired model policy; runtime evidence is required for the corresponding code paths.

| cfg | expected to break | broken | seconds |
|---|---|---|---|
| session-lifecycle/HerdrMissingBoundBeforeInput.cfg | InputResolves, MissingRetired, NoIdleHeld | InputResolves, MissingRetired, NoIdleHeld | 5.4 |
| session-lifecycle/HerdrMissingBoundBeforeRestart.cfg | InputResolves, MissingRetired, NoIdleHeld | InputResolves, MissingRetired, NoIdleHeld | 5.5 |
| session-lifecycle/HerdrMissingBoundBeforeRestored.cfg | InputResolves, MissingRetired, NoIdleHeld | InputResolves, MissingRetired, NoIdleHeld | 5.2 |
| session-lifecycle/HerdrMissingBoundBeforeUnknown.cfg | InputResolves, NoIdleHeld | InputResolves, NoIdleHeld | 4.1 |
| session-lifecycle/HerdrMissingBoundFixedAlive.cfg | - | - | 3.1 |
| session-lifecycle/HerdrMissingBoundFixedCancel.cfg | - | - | 3.0 |
| session-lifecycle/HerdrMissingBoundFixedClear.cfg | - | - | 3.0 |
| session-lifecycle/HerdrMissingBoundFixedInput.cfg | - | - | 2.9 |
| session-lifecycle/HerdrMissingBoundFixedLaunchFailure.cfg | - | - | 3.2 |
| session-lifecycle/HerdrMissingBoundFixedNonceRace.cfg | - | - | 3.4 |
| session-lifecycle/HerdrMissingBoundFixedReboot.cfg | - | - | 3.6 |
| session-lifecycle/HerdrMissingBoundFixedRepairFailure.cfg | - | - | 3.5 |
| session-lifecycle/HerdrMissingBoundFixedRestart.cfg | - | - | 3.2 |
| session-lifecycle/HerdrMissingBoundFixedRestored.cfg | - | - | 3.1 |
| session-lifecycle/HerdrMissingBoundFixedRunning.cfg | - | - | 3.1 |
| session-lifecycle/HerdrMissingBoundFixedStop.cfg | - | - | 3.0 |
| session-lifecycle/HerdrMissingBoundFixedTerminate.cfg | - | - | 2.9 |
| session-lifecycle/HerdrMissingBoundFixedUnknown.cfg | - | - | 2.9 |
| session-lifecycle/HerdrMissingBoundRetireOnlyLaunchFailure.cfg | InputResolves, NoRetainedRefusal | InputResolves, NoRetainedRefusal | 3.7 |

total 35.3s, 19 configs


The raw before traces follow `Bound + Missing -> Input -> Read -> held, watching=TRUE, notices=0 -> stutter`. Restart first performs a refused Missing reconnect while preserving Bound, then reaches the same idle path. A restored queue starts with `input=queued` and repeats it. Unknown also remains held in the frozen idle path, but never provides retire authority.

The proposed path follows `Read(Missing) -> Proof -> Retire(exact nonce CAS) -> Launch(new execution) -> Deliver`. Failed proof/CAS/launch follows `Reject -> one reason + discarded`; Unknown directly follows the refusal path without retirement. A crash during proof preserves the durable queue/row, forgets the proof and re-enters reconnect. The fixed running cfg needs the existing boundary before re-admission.

## Evidence and integration status

The check used TLC 1.7.4 and the repository fairness precheck. Model SHA256: `ecee7ff348d7f48c4663e91a2356613158a38f671f8ea7e2bef22a8509032666`. Jar SHA256: `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`.

The staged model and cfg bytes were copied to a lane-local checker root with its own 19-row expected manifest. The same 19 cfgs are registered in `docs/statecharts/expected.tsv` together with this model. The checker finished successfully in 35.3 seconds. Ten direct raw trace runs separately returned the expected five safety exit codes 12 and five temporal exit codes 13. All processes finished before reporting.

The implementation report records Rust RED/GREEN tests, mutants, source scans, classifier, clippy, Windows cross-check and prepush results separately. The table above reports actual TLC outcomes, including expected negative controls; it does not label those counterexamples as production successes.


The official repository manifest check after registration also matched all 19 cfgs (46 rule runs, 44.4 seconds). `list_merge_guard.py --ci --normalize` returned `LIST_MERGE_GUARD_OK` with no manifest diff. The implementation delta in the entrypoint table maps the common proof, idle repair, failure settlement and working probe to the checked actions.

The retired-native launch refinement carries the existing verified native binding and exact owner/Retired/nonce proof to the legacy-close decision. It enables the existing `Launch` action without changing the model's state or fairness assumptions. An unverified or different incarnation remains subject to its existing resume checks and can take `Reject`. The actual idle-to-first-write test and a legacy-close mutant check this refinement; no new native reset or legacy close authority is modeled.

## Busy effect and refusal-notice refinement

`EarlyBusy -> LosePane -> BusyEffect -> EffectWatch` models entry 43, including a failed first capture after admission. The watch requests a fresh owner judgement; it neither retires nor writes. `ActiveOriginalHeld` preserves the original until the existing `Boundary`. Temporary idle watching in `effect-held` is permitted by `NoIdleHeld`; `WatchResolves` rules out an immutable Missing watch. Unknown is not Missing; the Unknown control assumes a later readable observation rather than proving that an unknown read must recover.

`Reject -> FirstNoticeFails -> RestRecovers -> OutboxPost` separates input settlement from notification custody. `NoticeOnce` permits an outstanding durable obligation while `ReasonDelivered` and `InputResolves` require eventual visible delivery. Event retries retain the same identity. Database handoff availability and eventual REST recovery are assumptions; settlement/handoff crash atomicity is outside this model.

The official runner checked 27 configs: the old 19 and eight new controls/counterexamples. BeforeEffectIdle breaks InputResolves and WatchResolves; BeforeNoticeRetry breaks InputResolves, NoticeOnce and ReasonDelivered. The five new fixed configs and BeforeEffectActive preserve all selected rules. Production-path tests are a separate requirement.
