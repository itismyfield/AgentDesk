# Restarted Herdr Busy-Held readiness (#8222 design)

Base: main `8e80f9b06ad039a8817641773c5fa1ce2e2d4119`. Products unchanged. Design: [full entry inventory and slices](../../design/8222-restart-held-readiness.design.md). Projection: [chart](restart-held-readiness.chart.mmd).

The index's existing `ClaudeReadiness` restart modes, `ReadinessRequiet` (#8217 prepare wait), and `HerdrBusyReobserve` (#8114 captured held probe) were inspected. This revision extends **HerdrBusyReobserve.tla** with `RHSpec`; the original `Spec` and its 24 configs stay unchanged. The new refinement models both the readiness evidence boundary and the watcher that the incident reaches before prepare. No new TLA module is introduced.

## State and rule mapping

All value domains come from the design's §2 entry table (E01–E22). One original and one immutable source are enough to expose the loop; IDs 1/2 stand for exact/replaced nonce, native ID, inode, front and generation. `age=0..5` projects five 2-second reads of the proposed 10-second deadline. `pinned`, `complete`, `afterLaunch`, `fence`, `screen`, `gate` are the corresponding positive/negative predicates, not new runtime facts. The fixed cfg does not assume another user input, SessionStart, or native turn completion.

`RHEnter` → `RHBusyEntry` → `RHFirstKeep` makes proof loss, Busy Held, probe Unknown and an actual Keep explicit. `RHRestoreWake` restores only current reply evidence and delegates to `RHIdleReserve`/`RHWrite`. `RHRace` replaces the source after wake. `RHKeep`/`RHTick` spend a budget that is never renewed for that original; `RHExpire` is owner pre-key settlement and `RHNotice` the one durable reason notice. Lifecycle settles through its existing owner disposition. `RHReplay` preserves the original deadline and replays no body.

The first Keep's fixture publishes available evidence (or an explicit invalid condition). A continuously admissible reply is tried on the next read before another Keep; the healthy flow's recheck/write are finite driver steps. The model checks eventual resolution, not OS/IPC wall-clock latency. §5 of the design requires an independent deadline backstop and race tests to establish the implementation's time behavior. Permanent mailbox/storage/outbox failure is outside the fairness premise. First-key commitment and refusal are atomic competing owner actions in this refinement; implementing that gate is required, not proven by this document.

Invariants:

- `RHOneDisposition`: at most one delivery or one refusal, and exactly one reason notice in the explained terminal state.
- `RHNoRequeue`, `RHNoBusyDirect`, `RHRunningContract`: body requeue 0, restored Busy direct write 0, running fallback creates Idle 0.
- `RHProofNeedsEvidence`, `RHKeyNeedsRecheck`: source/launch/content/screen guards and actual driver's current evidence.

Properties: `RHReachedKeep` prevents a vacuous scenario; `RHNoInfiniteKeep` and `RHEndExactlyOnce` require terminal resolution, not a holding notice. Healthy reply and lost-SessionStart cfgs additionally require `RHHealthyDelivers`.

## Config matrix

| Group | Config suffixes | Required result |
|---|---|---|
| Main incident (1) | Before | RED `RHNoInfiniteKeep`, `RHEndExactlyOnce`; safety GREEN |
| Recoverable (2) | Valid, LostStart | GREEN, including actual delivery |
| No restoration (15) | Nonce, Native, Inode, Prompt, Tool, StopContext, Pending, Compact, Continuation, Missing, Legacy, NewIncarnation (old reply), OldReply, Draft, PaneUnreadable | GREEN via final reasoned no-send |
| Race/replay/lifecycle (6) | Race, Replay, Cancel, Clear, Terminate, Stop | GREEN; Race may deliver before replacement or refuse after failed recheck |
| Deadline mutants (2) | NoDeadline, ResetDeadline | RED both resolution properties |
| Evidence mutants (5) | SkipNonce, SkipNative, SkipInode, SkipContinuation, SkipLaunch | RED `RHProofNeedsEvidence`; driver still prevents an unauthorized key |
| Driver mutation (1) | SkipDriverRecheck | RED `RHKeyNeedsRecheck` |
| Authority mutations (2) | BusyDirect, FallbackIdle | RED `RHNoBusyDirect` / `RHRunningContract` |
| Disposition mutations (2) | RequeueBody, RepeatNotice | RED `RHNoRequeue` / (`RHTypeOK`, `RHOneDisposition`) |

These are cfg-controlled model mutants. No product source was mutated or tested by this leg. No expected rule of a pre-existing cfg was changed. Final observed results and raw logs follow.

## Observed WSL TLC result

The same installed WSL TLC jar used for static statechart checks was used directly and through `scripts/ci/statecharts_tlc.py::check_cfg`, including its fairness validation and separate property runs. Jar: `/home/adk/.cache/tla2tools/tla2tools.jar`, SHA256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`; banner `TLC2 Version 2.19 of 08 August 2024 (rev: 5a47802)`. No product build was needed.

- Final full module check: **60 cfg / 257 TLC rule runs / unexpected 0**. Existing 24 cfg unchanged; new 36 cfg = 23 all-GREEN + Before expected liveness RED + 12 expected mutant RED. Fairness checks are additional calls, excluded from 257.
- Before direct run: exit **13**, 21 generated / **8 distinct** states. Trace: restart → queued → Busy Held → Unknown Keep(age1) → Keep(age2..5) → stutter forever, no key, no refusal, no notice. The checker attributes both violated liveness properties independently.
- Valid direct run: exit **0**, 14 generated / **7 distinct** states; all safety and four liveness properties hold. It reaches an actual Keep before restored wake → Idle reservation → one delivered original.
- Driver guard mutant direct run: exit **12**, 19 generated / **10 distinct** states; `RHKeyNeedsRecheck` fails after native replacement between wake and reservation. The source replacement and changed proof are read from the state; the mutant cannot hide them by omitting a diagnostic flag.
- Registry coverage: **1343 cfg / 0 problems**. Existing expected rules were not edited. The README table was regenerated only with `python3 scripts/ci/statechart_index.py`.

Raw logs live under `/home/adk/adk/lane-runs/i8222/logs/`: `tlc-restart-checker-2.log`, `HerdrBusyReobserveRestartBefore-final.log`, `HerdrBusyReobserveRestartValid-final.log`, `HerdrBusyReobserveRestartSkipDriverRecheck-final.log`. The earlier exploratory log `tlc-restart-checker.log` contains one healthy-delivery failure: unconstrained clock advancement skipped an already admissible next probe. The final refinement makes the next read attempt restoration before another Keep and preserves deadline progress after a failed driver recheck. It did not change the expected healthy result to RED. This result models liveness under the scheduling premise above; implementation wall-clock race tests remain required.

Modeling ran from approximately 17:29 to 17:40 KST, below the brief's 30-minute modeling limit. Product PG/first-key tests, deployment observation and independent pro design review remain pending; these model results do not claim their completion.
