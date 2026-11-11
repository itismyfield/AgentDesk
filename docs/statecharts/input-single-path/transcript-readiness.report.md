# Claude readiness refinement — design d1

Code SHA: 574af580b0c5d6e6ce280994f4ed243463de5d2d (production unchanged).
Main: 4f30c67d9a48eacad96a99faff9fd442d0d4dd3c.
SC_RUN=mac-needed: /home/adk/.claude/skills/statechart/scripts/statechart is absent.
The chart/view are hand-authored projections, not generated state-space output.

Supplemental verification actually ran the repository checker with Java 21 / TLC 1.7.4:
**23 cfg / 62 TLC runs / 15 GREEN / 8 expected RED / failed 0 / skip 0**. New refinement: 17 cfg; legacy regression sample: 6 cfg.
The wrapper was not run, so this does not claim a mac statechart rendering run.
The whole repository statechart suite was not run.

## Model and code correspondence

InputPath now explicitly instances ClaudeReadiness under ReadinessSpec. Legacy custody/boot/stop specifications freeze
the refinement record; the readiness specification freezes legacy state. Boot fairness retains the original variable tuple.
HerdrBusyHandoff's equivalent ComposerAdmitted guard is documented as the abstraction of this boundary.
No exact-close/typed-Held/Unknown-retention contract changes are modeled.

Ready before the change requires a screen confirmation that can remain false. The narrow cfg abstracts a permanently
failing readiness screen read, including resize; it is not an executable 46-column production fixture.
The code witness is input.rs:685/688, :709/710, :772, :1006/1007, :1028/1073:
Stop/Idle still returns through screen acceptance. BeforeNarrow can finish its current turn, observe Idle, and hold the
same originals forever. The proposed cfg removes that screen readiness predicate; existing draft admission remains.

The two accepted originals are queued from Init, so progress is not vacuous through zero arrivals. Submit and Receipt
are separate, and local turn generation advances with the first key before the next transcript observation.
NativeStart can occur after Prepare and before Submit: NoRevalidate then injects while Running; the real proposal revokes
the stale proof and eventually retries. Replay breaks both duplicate and done-backed-by-one-effect rules.
BusyCompact injects before terminal Idle. DraftBypass executes a hidden Person draft.
NoWake retains the originals but violates progress. These are model mutants; Rust mutants remain implementation work.

## Assumptions and exposed limits

Fairness is applied to individual observations, ongoing provider completion, enabled preparation/submission/receipt,
transient draft recovery and human clearing. It never equates an unread composer to Empty.
PermanentUnread deliberately disables recovery and remains progress RED. Permanent Person draft or post-key
crash/Unknown is not covered by unconditional execution liveness. The existing Herdr blocked boundary stays RED.

Fresh/Clear/Resume begin before SessionStart, and a first submission requires that signal.
Clear/Resume cfgs represent the post-boundary first-input phase; they do not model an arbitrary /clear racing all native
I/O. Epoch invalidation on a pre-key daemon restart is separate. User-native input that has not yet been reported by a
hook/transcript is not atomic with our composer lock and remains an implementation limit.
Actual 2.1.322 input acceptance ordering is [U]; local version is 2.1.316.

## Expected and actual results

Every row below matched expected.tsv. A listed RED is an expected violation, not a checker error.

| cfg | expected to break | broken | seconds |
|---|---|---|---|
| input-single-path/InputPathBootDesign.cfg | - | - | 2.8 |
| input-single-path/InputPathClean.cfg | - | - | 2.0 |
| input-single-path/InputPathReadyAfterNarrow.cfg | - | - | 2.9 |
| input-single-path/InputPathReadyBeforeNarrow.cfg | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 2.7 |
| input-single-path/InputPathReadyBusyCompact.cfg | ReadyCompactAfterStopIdle | ReadyCompactAfterStopIdle | 6.0 |
| input-single-path/InputPathReadyClear.cfg | - | - | 2.8 |
| input-single-path/InputPathReadyCompact.cfg | - | - | 4.5 |
| input-single-path/InputPathReadyDraft.cfg | - | - | 3.0 |
| input-single-path/InputPathReadyDraftBypass.cfg | ReadyNoHumanDraftExecution | ReadyNoHumanDraftExecution | 4.4 |
| input-single-path/InputPathReadyFresh.cfg | - | - | 2.8 |
| input-single-path/InputPathReadyLostStop.cfg | - | - | 2.8 |
| input-single-path/InputPathReadyNoRevalidate.cfg | ReadyNoRunningInjection | ReadyNoRunningInjection | 4.3 |
| input-single-path/InputPathReadyNoWake.cfg | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 2.7 |
| input-single-path/InputPathReadyPermanentUnread.cfg | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 2.8 |
| input-single-path/InputPathReadyRace.cfg | - | - | 3.1 |
| input-single-path/InputPathReadyReplay.cfg | ReadyNoDuplicate, ReadyNoSilentLoss | ReadyNoDuplicate, ReadyNoSilentLoss | 6.0 |
| input-single-path/InputPathReadyRestart.cfg | - | - | 3.4 |
| input-single-path/InputPathReadyResume.cfg | - | - | 3.0 |
| input-single-path/InputPathReadyUnread.cfg | - | - | 3.1 |
| input-single-path/InputPathStopGateLive.cfg | - | - | 12.5 |
| session-lifecycle/HerdrBusyHandoffAfterBusy.cfg | - | - | 5.1 |
| session-lifecycle/HerdrBusyHandoffAfterFirstRefusal.cfg | - | - | 5.3 |
| session-lifecycle/HerdrBusyHandoffBlockedComposerBoundary.cfg | JournalProgress, MessageProgress | JournalProgress, MessageProgress | 5.2 |


## Reproduction and evidence

Command:
python3 scripts/ci/statecharts_tlc.py --root /home/adk/adk/lane-runs/i8080/logs/sc-final --jar /home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar --jobs 2

The subset root contains exact model/config bytes for the 23 selected cfgs.
Raw checker log: /home/adk/adk/lane-runs/i8080/logs/tlc-final.log (49.1 seconds).
Model freeze hashes: logs/model-freeze.sha256 under that lane directory.
Direct raw TLC counterexamples/control are tlc-raw-BeforeNarrow.log (exit13), tlc-raw-AfterNarrow.log (exit0),
tlc-raw-NoRevalidate.log (exit12); exact commands and exit codes are in raw-run-results.json.
Java/tool/environment data is environment.json. TLC jar SHA256:
936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88.

An initial legacy BootDesign check exposed the new variable being included in an old fairness tuple
(tlc-legacy.log: inputReadiness undefined). The tuple was corrected before the final model freeze.
The final 23-cfg run supersedes that failed development run; no failure was relabeled PASS.
Models froze approximately 21 minutes after task start, within the 30-minute modeling stop line.

No cargo/PG/production tests ran in this design leg. No claim of runtime DoD completion.
See transcript-readiness.design.md for the complete entry inventory, implementation budget, tests and review limits.
