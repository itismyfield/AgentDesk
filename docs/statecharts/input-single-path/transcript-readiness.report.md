# Claude Idle readiness refinement — implementation A

Implementation source SHA: 664d1589dd99c3d51d9082194e0c09189035b4b1.
Implementation base: c3b660544bab3971630931b444c67a985f4c9169.
Current model evidence: 37 cfg / 105 reported runs / 23 GREEN / 14 expected RED / failed0 / skip0; a3 direct TLC run.
Document baseline: 0b0f39c582a9073fdb20b62512bc7794a18d1281.
Main: 4f30c67d9a48eacad96a99faff9fd442d0d4dd3c.
SC_RUN=direct-TLC (a1 run reused); the statechart wrapper is absent.
Chart/view are hand-authored projections, not wrapper-generated state-space output.

## Historical design d2 execution

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

## Implementation A refinement

Implementation base c3b660544bab3971630931b444c67a985f4c9169. Busy selection, write and publication are separate actions. Finish can interleave before write or after write, and duplicate Idle wakes occur before receipt. ReexposeBusy deliberately returns the written original to the Idle route.

Direct TLC results (all 33 selected configs rerun, no skip; the complete repository suite was not run):

| cfg | expected to break | broken | seconds |
|---|---|---|---|
| input-single-path/InputPathBootDesign.cfg | - | - | 3.3 |
| input-single-path/InputPathClean.cfg | - | - | 1.9 |
| input-single-path/InputPathReadyAfterNarrow.cfg | - | - | 3.7 |
| input-single-path/InputPathReadyBeforeNarrow.cfg | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 3.1 |
| input-single-path/InputPathReadyBlockBusy.cfg | ReadyBusyEventuallyQueues, ReadyPendingEventuallyExecutes | ReadyBusyEventuallyQueues, ReadyPendingEventuallyExecutes | 4.1 |
| input-single-path/InputPathReadyBusyActive.cfg | - | - | 5.5 |
| input-single-path/InputPathReadyBusyCompact.cfg | ReadyCompactAfterStopIdle | ReadyCompactAfterStopIdle | 9.7 |
| input-single-path/InputPathReadyBusyFinishAfter.cfg | - | - | 5.2 |
| input-single-path/InputPathReadyBusyFinishBefore.cfg | - | - | 5.3 |
| input-single-path/InputPathReadyBusyNative.cfg | - | - | 5.4 |
| input-single-path/InputPathReadyBusyReexpose.cfg | ReadyBusyKeyPlanPreserved, ReadyNoDuplicate | ReadyBusyKeyPlanPreserved, ReadyNoDuplicate | 8.1 |
| input-single-path/InputPathReadyClear.cfg | - | - | 3.5 |
| input-single-path/InputPathReadyCompact.cfg | - | - | 6.5 |
| input-single-path/InputPathReadyDraft.cfg | - | - | 3.8 |
| input-single-path/InputPathReadyDraftBypass.cfg | ReadyNoHumanDraftExecution | ReadyNoHumanDraftExecution | 6.4 |
| input-single-path/InputPathReadyFresh.cfg | - | - | 3.5 |
| input-single-path/InputPathReadyLostStop.cfg | - | - | 3.3 |
| input-single-path/InputPathReadyNoLatch.cfg | ReadyNoContinuationEarlyInput, ReadyNoIdleRunningInjection | ReadyNoContinuationEarlyInput, ReadyNoIdleRunningInjection | 5.9 |
| input-single-path/InputPathReadyNoLatchCompact.cfg | ReadyCompactAfterStopIdle, ReadyNoContinuationEarlyInput, ReadyNoIdleRunningInjection | ReadyCompactAfterStopIdle, ReadyNoContinuationEarlyInput, ReadyNoIdleRunningInjection | 11.3 |
| input-single-path/InputPathReadyNoRevalidate.cfg | ReadyNoIdleRunningInjection | ReadyNoIdleRunningInjection | 5.8 |
| input-single-path/InputPathReadyNoWake.cfg | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 3.1 |
| input-single-path/InputPathReadyPermanentUnread.cfg | ReadyPendingEventuallyExecutes | ReadyPendingEventuallyExecutes | 3.1 |
| input-single-path/InputPathReadyRace.cfg | - | - | 4.1 |
| input-single-path/InputPathReadyReplay.cfg | ReadyNoDuplicate, ReadyNoSilentLoss | ReadyNoDuplicate, ReadyNoSilentLoss | 10.0 |
| input-single-path/InputPathReadyRestart.cfg | - | - | 4.6 |
| input-single-path/InputPathReadyResume.cfg | - | - | 3.4 |
| input-single-path/InputPathReadySuppressedCompact.cfg | - | - | 6.7 |
| input-single-path/InputPathReadySuppressedStop.cfg | - | - | 3.6 |
| input-single-path/InputPathReadyUnread.cfg | - | - | 3.9 |
| input-single-path/InputPathStopGateLive.cfg | - | - | 14.0 |
| session-lifecycle/HerdrBusyHandoffAfterBusy.cfg | - | - | 5.5 |
| session-lifecycle/HerdrBusyHandoffAfterFirstRefusal.cfg | - | - | 5.6 |
| session-lifecycle/HerdrBusyHandoffBlockedComposerBoundary.cfg | JournalProgress, MessageProgress | JournalProgress, MessageProgress | 5.4 |

total 91.6s, 33 configs


## 구현 A 재개 검증

a2에서는 모델·cfg를 바꾸지 않았다. a1 직접 TLC 최종 실행의 33 cfg / 97 reported runs /
21 GREEN / 12 expected RED / failed0 / skip0을 재사용한다. 저장된 실행 스냅샷의 TLA/CFG
52개 파일이 현재 파일과 바이트 단위로 일치한다. compact GREEN은 분할 B까지 포함한 설계
결과이며 A의 compact 제품 구현을 주장하지 않는다. partial-record/producer clock 검사는
이 추상 모델의 새 활동 술어를 구체화하며 별도 Rust RED/GREEN 시험으로 검증한다.


## A review repair — permission and startup refinement

Implementation source SHA: 664d1589dd99c3d51d9082194e0c09189035b4b1. Direct TLC reran the existing33 plus4 new configs:37 cfg/105 reported runs/GREEN23/expected RED14/unexpected failed0/skip0,61.9s.

| cfg | before/after | actual violation |
|---|---|---|
| InputPathReadyPermissionPoison.cfg | before | ReadyPendingEventuallyExecutes |
| InputPathReadyPermissionLater.cfg | after | none |
| InputPathReadyPreflightOnly.cfg | before | ReadyPendingEventuallyExecutes |
| InputPathReadyLateDialog.cfg | after | none |

Permission metadata clears the current terminal while later completion remains eligible. A late approved startup picker must be polled again before SessionStart; its dialog Enter is separate from original payload writes. The concrete workspace plan is tested by callers, not proved by these abstract paths. Existing compact GREEN remains a design-B claim only.


## Historical B1 implementation / direct TLC correspondence

Implementation source SHA `efe1ba3647bea52f0a6dc19b662eaf17219d3316`. B measure base / merged A ref is `6be9c44583b0cb24f49e31ede6a4c59cfdf88515`. No TLA/cfg change was needed: this implementation follows the already reviewed compact-pending actions. Direct TLC was rerun from the current 56 model/cfg files (all byte-identical to the execution snapshot): 37 cfg / 105 reported runs, GREEN23 / expected RED14 / unexpected failure0 / skip0, 66.1s. These results are specification checks, not a CLI execution or code extraction.

| Model value/action | Production coordinate |
|---|---|
| usage crossing / pending callback | src/services/claude_compact_trigger.rs:356 |
| generation / occupancy / captured authority | src/services/claude_compact_trigger.rs:229 |
| source / Start / file identity capture | src/services/claude_tui/readiness/compact.rs:22 |
| terminal-only / reserve-current / D / pre-Enter | src/services/claude_tui/readiness/compact.rs:40 |
| event hint / 250ms timer | src/services/claude_tui/readiness/compact.rs:147 |
| wake after hook ledger guard release | src/services/claude_tui/readiness/mod.rs:277 |
| blocking composer witness | src/services/claude_tui/composer_lock.rs:80 |
| completed absence vs error / successor | src/services/discord/compact_turn_authority.rs:81 |

| Before / after cfg | Actual result | Broken rules |
|---|---|---|
| InputPathReadyBusyCompact.cfg | expected RED | ReadyCompactAfterStopIdle |
| InputPathReadyCompact.cfg | GREEN | - |
| InputPathReadySuppressedCompact.cfg | GREEN | - |
| InputPathReadyNoLatchCompact.cfg | expected RED | ReadyCompactAfterStopIdle,ReadyNoContinuationEarlyInput,ReadyNoIdleRunningInjection |
| InputPathReadyDraft.cfg | GREEN | - |
| InputPathReadyUnread.cfg | GREEN | - |
| InputPathReadyRace.cfg | GREEN | - |

The following paragraph records the historical B1 assumption, superseded by the B completion repair below. The model abstracts a valid pending generation, eventual approved terminal evidence, recoverable pre-key capture, and a non-ambiguous compact write. Source invalidation and unknown transport are tested by callers; the liveness GREEN does not prove progress after an uncertain first key. The installed WSL CLI is 2.1.316, not operational 2.1.322. An older 2.1.283 capture has PreCompact / SessionStart(compact) / PostCompact and no following root Stop, so the exact 2.1.322 native compact-completion activity/terminal sequence remains [U]. SessionStart(compact) is not used to authorize a first input. No PostCompact readiness authority was added. The next-input-after-compact caller case uses explicitly synthetic new native activity and a hard terminal; it does not settle that operational assumption.

Busy hand_busy / native Running writes and #8077 custody / exact-close / first-input behavior stay on their existing paths. P2-4 (late old clear Start after resume) remains deferred.


## B 완료 수리 / own compact completion

Current implementation source SHA `e46154b856cdc6ce4f00b3a628c045e26e16fd0e`; repair base `eee9d323e23576b7e6eb98cf92890ac9726d60da`. The older B1 synthetic native-terminal assumption above is disproved by the coordinator's isolated Claude Code 2.1.322 capture. The committed redacted fixture is tests/fixtures/claude_compact/local-command-2.1.322.jsonl (post-compact rows 46 through 64, no assistant/stop-hook-summary/turn-duration). The lane did not access an operational session or run that target CLI itself.

Only confirmed Enter creates a local-control ticket. The persistent worker accepts one source-bound /compact command followed by a closed local stdout/stderr envelope after its reserved file offset. Success also needs a manual compact boundary and the known Compacted stdout. Failure is completion with success=false. Current source/native/path/nonce/file and Start are rechecked; generation is rechecked and held stable through memory-only exact-token publication. No file/source-authority I/O is done under the generation leaf lock. The worker never sends another key during completion. PostCompact remains ignored; SessionStart(compact) can only wake the existing worker and never authorizes readiness.

Continuation latches and recovery floors remain intact. A recovery floor already proven by the reservation can support that same completion; a newer recovery floor cannot. A new approved Start/file incarnation or real native submit discards old completion evidence. Reservation by itself keeps the evidence behind pending, allowing a proven pre-key NotSent to retry; a mutation remains pending until a new native turn or the owned completion proves progress. Unknown transfer, foreign/multiple command, old output before the floor, wrong source/generation/token never release another reservation. Existing Busy injection, draft policy, and #8077 custody/close/composer contracts are unchanged. P2-4 hook source ordering remains deferred.

| Model action / contract | Production coordinate |
|---|---|
| Submitted worker remains alive | src/services/claude_tui/readiness/compact.rs:202 |
| completion parse / source checks before publication | src/services/claude_tui/readiness/compact.rs:86 |
| closed local-command output classifier | src/services/claude_tui/readiness/compact/completion.rs:13 |
| exact reservation / Start / recovery publication | src/services/claude_tui/readiness/compact/completion.rs:121 |
| completion proof consumption | src/services/claude_tui/readiness/mod.rs:158 |
| generation gate permits same-cycle re-arm | src/services/claude_compact_trigger.rs:243 |
| memory-only generation guarded publication | src/services/claude_compact_trigger.rs:424 |
| same file new Start invalidates completion | src/services/claude_tui/readiness/mod.rs:376 |

`CompactCompletionEvidence` emits no native terminal. `FinishSelfCompactFence` provides the owned completion proof; PROPERTY `ReadySelfCompactEventuallyReleases` states that a confirmed self compact followed by completion evidence eventually lets the next Idle original execute. Direct TLC: 39 cfg / 111 reported runs, GREEN24 / expected RED15 / unexpected0 / skip0, 104s. All 58 TLA/cfg source files match the executed snapshot. This is a specification check and manual code correspondence, not code extraction. Source/generation invalidation, Unknown transport, recovery, NotSent, and draft admission are caller-test boundaries; unconditional progress is not claimed after an ambiguous write or a foreign generation.

| Before/after cfg | Actual result | Broken property/rule |
|---|---|---|
| InputPathReadyBusyCompact.cfg | expected RED | ReadyCompactAfterStopIdle |
| InputPathReadyCompact.cfg | GREEN | - |
| InputPathReadyCompactCompletion.cfg | GREEN | - |
| InputPathReadyCompactNoCompletion.cfg | expected RED | ReadySelfCompactEventuallyReleases |
| InputPathReadyNoLatchCompact.cfg | expected RED | ReadyCompactAfterStopIdle,ReadyNoContinuationEarlyInput,ReadyNoIdleRunningInjection |
| InputPathReadySuppressedCompact.cfg | GREEN | - |

Completion retirement reads the owned historical prefix; current Idle evidence separately rejects later native activity. A new native turn winning the timer cannot hide the completed control or authorize Running injection; its later hard terminal can advance the next original without UserPromptSubmit. Caller compact_completed_prefix_cannot_hide_a_new_native_turn_or_require_its_submit_hook covers this interleaving. TLA/cfg bytes are unchanged; retirement and CurrentEvidence remain separate obligations.


## Clear Pending before file publication

Merge source baseline: ca4ba220aa35c4d4c31d417bcbd9659eb7e1da89. The refinement preserves the old runtime binding until the first new transcript is registered. A current clear Start and the same execution's published canonical Pending permit only an explicit new path. Proofs retain the Pending seq and Start revision; reserve/current recheck both. Readiness observes an already published fold and never reloads or repairs the writer log.

`InputPathReadyClearPendingBefore.cfg` separates `oldBinding` from `filePublished` and breaks `ReadyPendingEventuallyExecutes`: no payload means no file and therefore no new binding. `InputPathReadyClearPendingAfter.cfg` passes safety and that liveness property. `InputPathReadyClearPendingRecheck.cfg` changes the Pending seq after Prepare; the stale proof is revoked, and a fresh proof progresses. The diagrams are hand-authored projections; the lane report records actual TLC runs and fingerprints.

Clear repair implementation source SHA: d0d8d711b8be5668aacf90a7cd7c362e15ef7ec0. Final normal Rust source includes the unchanged-path native-ID fallback. Targeted callers and negative evidence tests are described in the lane report.

Direct TLC:62 configs/226 calls, GREEN34, expected RED28, unexpected0. The required base/head invocation selected79 configs and completed270 calls. The concrete nonce/channel/path checks are exercised by caller tests; the model separates old binding, Pending seq and new file publication.

## Clear first-input Held watch refinement

Code base SHA: b63a97c5115bf469c85996242d61732557122427 (the before/None watcher).
Candidate first_input.rs SHA256: b27be5fb6760f3875ff9703c8abe4b7ed7ee5be861d80086ba9e3b7b8137bbe3.
Repository checker run: `TLA2TOOLS=/home/adk/.cache/tla2tools/tla2tools.jar python3 scripts/ci/statecharts_tlc.py --base 193a57168f --head HEAD --jobs 2`.
81 configs / 274 TLC invocations / 40 GREEN / 41 expected RED / unexpected0.
The working model inputs were checked; committed-head selection is verified separately before reporting completion.

| cfg | expected / actual | liveness violation |
|---|---|---|
| InputPathReadyClearHeldBefore.cfg | RED / RED | ReadyClearHeldEventuallySends |
| InputPathReadyClearHeldAfter.cfg | GREEN / GREEN | none |

ClearHeld forces a first-input draft veto only after current clear readiness was observed.
Recovering the composer does not itself wake, and the ordinary Observe action cannot wake this Held input.
Old Stop and external IdleWake are disabled in this scenario. Only the level watch may wake it.
The before watch asks for the implicit source and stays blocked behind the old runtime binding.
The after watch preserves the attached path and reuses current clear/Pending authority.
Submit creates the first new file; PublishClearFile changes the old binding later.
Stable current nonce/path/Start/Pending and fair watch execution are liveness premises.
Runtime owned-caller tests check changed nonce/path, transient capture failure, exact original custody and one paste/Enter.
The model does not grant replacement-source authority or change Unknown replay, Busy injection or draft/focus policy.
