# Claude Idle readiness refinement — design d2

Production code SHA: 574af580b0c5d6e6ce280994f4ed243463de5d2d (production unchanged).
Document baseline: 0b0f39c582a9073fdb20b62512bc7794a18d1281.
Main: 4f30c67d9a48eacad96a99faff9fd442d0d4dd3c.
SC_RUN=mac-needed: /home/adk/.claude/skills/statechart/scripts/statechart is absent.
Chart/view are hand-authored projections, not wrapper-generated state-space output.

Supplemental repository TLC checker actually ran:
**30 cfg / 86 reported TLC runs / 19 GREEN / 11 expected RED / failed0 / skip0**, 70.1 seconds.
All d1 selected 23 cfg were rerun against changed model bytes; 7 new cfg cover the two preserved busy entries,
busy-hold regression, suppressed Stop/compact and latch-removal mutants. The whole repository suite was not run.

## Review decisions reflected

P1 inventory gaps are closed: the active owner hand_busy, intake native Running→into_busy_pane,
and owner native Running→Busy leaf→inject_owned paths are explicitly unchanged.
The coordinator withdrew the d1 global Running-no-keys premise and the active-branch hold proposal.
ReadyNoRunningInjection is replaced by **ReadyNoIdleRunningInjection**; this rule covers only Idle-ready Submit.
BusyInject retains paste+Enter and queues the same original in Claude; BusyAbsorb can consume it in the current
turn, or BusyConsume can start it in the next turn. Receipt remains separate and effects/writes are each at most one.
BusyEventuallyQueues makes zero busy injections a real progress failure, not a vacuous GREEN.
BusyEmpty-composer key plans are modeled; existing stash/Cancel/receipt behavior still needs unchanged product tests.

P2 distinguishes shared observer Idle from an approved readiness terminal:
tui_turn_state.rs:612 includes stop_hook_summary, while discord/tmux_output_stream.rs:610-626 emits a soft candidate
and :918-940 explicitly retains later assistant text. The design adds a local terminal whitelist and source/turn-scoped
continuation disposition before hook_server.rs:465 notify/:531 registry. The shared observer/parser stay unchanged.

The model forces suppressed Stop→summary→timer read→late assistant→actual hard Finish.
Observe deliberately reports Idle from a summary while provider phase stays Running.
The armed continuation latch blocks Idle Submit and CompactSubmit until the later final.
Removing only latch arming causes ReadyNoContinuationEarlyInput plus the appropriate Idle/compact safety violations.
The raw matching Stop boundary is a candidate; its continuation disposition is encoded by the same latch.
No suppressed Stop is re-published through the existing ready registry.

## Scope and assumptions

The rule is Idle-ready keys0 while Running, not a global ban on busy input.
No new hold/Idle wait is inserted ahead of active/native Busy branches. Generic Idle gate adapters must preserve Busy context.
Existing ownership, native queue, first-key gates, exact receipts, #8077 exact close/typed Held/Unknown retention are unchanged.

New cfgs queue two originals at Init, separating queue admission, physical writes, native consumption and receipt.
Busy fixtures keep the initial native turn open until one busy injection, then allow current-turn absorption or next-turn
execution; this represents both existing native queue outcomes rather than forcing idle-hold semantics.
NoRevalidate remains RED for Idle-ready safety. BlockBusy removes BusyInject and remains RED for progress.

The model abstracts the approved hard-terminal class; it does not enumerate every JSON envelope or prove hook timing.
The design whitelist treats result/current turn_duration/canonical interrupt as hard candidates behind source/floor/latch
checks; summary needs a matching root Stop with non-continuing disposition and cannot qualify from quiescence alone.
Only the suppressed-summary continuation sequence is refined here. Positive confirmed-summary promotion,
unknown restoration, all other hook dispositions, and actual 2.1.322 ordering are not proved by these cfgs.
The startup/resume/clear transport ordering and suppressed Stop→late assistant sequence on 2.1.322 remain [U].
A suppression history lost on restart with no later evidence has no unconditional progress proof;
the independent pre-key Restart GREEN does not cover that composition.

Existing fairness covers actual provider finish, enabled submissions/receipts, periodic reads, transient draft recovery
and human draft clearing. PermanentUnread stays progress RED; no unread→Empty assumption is introduced.
Post-key crash/Unknown success liveness and unreported native-key atomicity remain outside the readiness refinement.
Existing Herdr blocked-composer MessageProgress/JournalProgress RED is preserved.

## Per-cfg before/after and expected/actual results

RED rows name expected violated rules; every expected set matched. d1 labels are historical results and were not reused.

| cfg | d1 actual | d2 expected | d2 actual | TLC runs |
|---|---|---|---|---:|
| input-single-path/InputPathBootDesign.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathClean.cfg | GREEN | - | - | 1 |
| input-single-path/InputPathReadyAfterNarrow.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathReadyBeforeNarrow.cfg | RED: ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 2 |
| input-single-path/InputPathReadyBlockBusy.cfg | new | ReadyBusyEventuallyQueues,ReadyPendingEventuallyExecutes | ReadyBusyEventuallyQueues,ReadyPendingEventuallyExecutes | 3 |
| input-single-path/InputPathReadyBusyActive.cfg | new | - | - | 3 |
| input-single-path/InputPathReadyBusyCompact.cfg | RED: ReadyCompactAfterStopIdle | ReadyCompactAfterStopIdle | ReadyCompactAfterStopIdle | 4 |
| input-single-path/InputPathReadyBusyNative.cfg | new | - | - | 3 |
| input-single-path/InputPathReadyClear.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathReadyCompact.cfg | GREEN | - | - | 3 |
| input-single-path/InputPathReadyDraft.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathReadyDraftBypass.cfg | RED: ReadyNoHumanDraftExecution | ReadyNoHumanDraftExecution | ReadyNoHumanDraftExecution | 3 |
| input-single-path/InputPathReadyFresh.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathReadyLostStop.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathReadyNoLatch.cfg | new | ReadyNoContinuationEarlyInput,ReadyNoIdleRunningInjection | ReadyNoContinuationEarlyInput,ReadyNoIdleRunningInjection | 4 |
| input-single-path/InputPathReadyNoLatchCompact.cfg | new | ReadyCompactAfterStopIdle,ReadyNoContinuationEarlyInput,ReadyNoIdleRunningInjection | ReadyCompactAfterStopIdle,ReadyNoContinuationEarlyInput,ReadyNoIdleRunningInjection | 6 |
| input-single-path/InputPathReadyNoRevalidate.cfg | RED: ReadyNoRunningInjection | ReadyNoIdleRunningInjection | ReadyNoIdleRunningInjection | 3 |
| input-single-path/InputPathReadyNoWake.cfg | RED: ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 2 |
| input-single-path/InputPathReadyPermanentUnread.cfg | RED: ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 2 |
| input-single-path/InputPathReadyRace.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathReadyReplay.cfg | RED: ReadyNoDuplicate,ReadyNoSilentLoss | ReadyNoDuplicate,ReadyNoSilentLoss | ReadyNoDuplicate,ReadyNoSilentLoss | 4 |
| input-single-path/InputPathReadyRestart.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathReadyResume.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathReadySuppressedCompact.cfg | new | - | - | 3 |
| input-single-path/InputPathReadySuppressedStop.cfg | new | - | - | 2 |
| input-single-path/InputPathReadyUnread.cfg | GREEN | - | - | 2 |
| input-single-path/InputPathStopGateLive.cfg | GREEN | - | - | 3 |
| session-lifecycle/HerdrBusyHandoffAfterBusy.cfg | GREEN | - | - | 5 |
| session-lifecycle/HerdrBusyHandoffAfterFirstRefusal.cfg | GREEN | - | - | 5 |
| session-lifecycle/HerdrBusyHandoffBlockedComposerBoundary.cfg | RED: JournalProgress,MessageProgress | MessageProgress,JournalProgress | JournalProgress,MessageProgress | 5 |

## Reproduction and evidence

Command:
python3 scripts/ci/statecharts_tlc.py --root /home/adk/adk/lane-runs/i8080/logs/r2/sc-final --jar /home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar --jobs 2

Exact final model/cfg input: logs/r2/sc-final under /home/adk/adk/lane-runs/i8080.
Final checker stdout: logs/r2/tlc-final.log. Freeze hashes: logs/r2/model-freeze-final.sha256.
Final raw control/counterexamples: logs/r2/tlc-final-raw-*.log; exact commands/exits: raw-final-results.json.
Focused NoLatchCompact raw run checks ReadyCompactAfterStopIdle alone, so prompt safety cannot mask the compact witness.
The focused config is evidence-only under logs/r2/focused, not an unlisted repository cfg.

Initial r2 pass1 (76.4s) preceded adding current-turn BusyAbsorb; its bytes are in sc-root and are superseded by sc-final.
Final checks/direct traces correspond to final model hashes; no earlier pass was reused on the changed source.
No model edit follows the final freeze, 10.55 minutes after turn start, inside the 25-minute limit.
Java21/TLC1.7.4, SHELL=/bin/bash, TMPDIR unset; environment.json records versions.
TLC jar SHA256: 936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88.

No cargo, PG or production test binary ran. This is a design/model report, not runtime DoD acceptance.
See transcript-readiness.design.md for the complete updated design, code coordinates, budgets and product test plan.
