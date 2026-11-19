# Restarted Herdr Busy-Held readiness (#8222 design)

Base: main `8e80f9b06ad039a8817641773c5fa1ce2e2d4119`. Products unchanged. Design: [full entry inventory and slices](../../design/8222-restart-held-readiness.design.md). Projection: [chart](restart-held-readiness.chart.mmd).

The index's existing `ClaudeReadiness` restart modes, `ReadinessRequiet` (#8217 prepare wait), and `HerdrBusyReobserve` (#8114 captured held probe) were inspected. This revision extends **HerdrBusyReobserve.tla** with `RHSpec`; the original `Spec` and its 24 configs stay unchanged. The new refinement models both the readiness evidence boundary and the watcher that the incident reaches before prepare. No new TLA module is introduced.

## State and rule mapping

All value domains come from the design's §2 entry table (E01–E22). One original and one immutable source are enough to expose the loop; IDs 1/2 stand for exact/replaced nonce, native ID, inode, front and generation. `age=0..5` projects five 2-second reads of the proposed 10-second deadline. `pinned`, `complete`, `afterLaunch`, `fence`, `screen`, `gate` are the corresponding positive/negative predicates, not new runtime facts. The fixed cfg does not assume another user input, SessionStart, or native turn completion.

`RHEnter` → `RHBusyEntry` → `RHFirstKeep` makes proof loss, Busy Held, probe Unknown and an actual Keep explicit. `RHRestoreWake` restores only current reply evidence and delegates to `RHIdleReserve`/`RHWrite`. `RHRace` replaces the source after wake. `RHKeep`/`RHTick` spend a budget that is never renewed for that original; `RHExpire` is owner pre-key settlement and `RHNotice` the one durable reason notice. Lifecycle settles through its existing owner disposition. `RHReplay` preserves the original deadline and replays no body.

The first Keep's fixture publishes available evidence (or an explicit invalid condition). The clock (`RHTick`) runs in every unstarted phase — held, waking and reserved — whether or not evidence is Ready, so a wake, a reserve and a writer permit can each meet the due. `RHExpire` settles only an attemptless front (held or waking) and revokes its grant; `RHLeafExpire` settles a reserved attempt through its own NotSent and closes its writer gate. `RHArm` hands a writer its permit before due and `RHWrite` commits the first key through that gate, so in one state both the delayed key and the deadline are enabled and either may win. `RHSwap` replaces the front or its generation and `RHStaleCallback` delivers the old deadline to the new front. `RHReboot` keeps the durable hold, due, refusal and notice intent, drops proof, watch and an unacknowledged notice drain; `RHNoticeEnqueue`/`RHNoticeAck` split the outbox record from its durable ack. The model checks eventual resolution, not OS/IPC wall-clock latency; permanent mailbox/storage/outbox failure is outside the fairness premise. Entry, ProofOrigin and actual leaf proof issuance stay NOT_MODELED and are required as S1/S2/S3 tests.

Invariants:

- `RHOneDisposition`: at most one delivery or one refusal, and exactly one reason notice in the explained terminal state.
- `RHNoRequeue`, `RHNoBusyDirect`, `RHRunningContract`: body requeue 0, restored Busy direct write 0, running fallback creates Idle 0.
- `RHProofNeedsEvidence`, `RHKeyNeedsRecheck`: source/launch/content/screen guards and actual driver's current evidence.
- `RHBoundedHold`: an unstarted original never outlives the due its first typed hold fixed (`elapsed` is real time, `age` the stored budget).
- `RHStaleNoop`, `RHOneNoticeRecord`: an old deadline callback never removes a new front or generation; a crash between outbox enqueue and ack leaves one record.
- Witnesses `RHNo*` (front/leaf due race, deadline win over a permitted writer, key at due, swap after reserve, stale callback, mid-hold reboot, crash replay) are expected to break: each proves its competing state is reached.

Properties: `RHReachedKeep` prevents a vacuous scenario; `RHNoInfiniteKeep` and `RHEndExactlyOnce` require terminal resolution, not a holding notice. Healthy reply and lost-SessionStart cfgs require `RHHealthyDelivers` under `Entry = "prompt-driver"`, the premise that a driver step runs before the clock while it is enabled. Without that premise `DueRace`, `Reboot` and `OldSnapshot` require `RHHealthyOrDeadline`: a healthy pane delivers or loses only the due race, never for an evidence reason.

## Config matrix

| Group | Config suffixes | Required result |
|---|---|---|
| Main incident (1) | Before | RED `RHNoInfiniteKeep`, `RHEndExactlyOnce`; safety GREEN |
| Recoverable (2) | Valid, LostStart | GREEN, including actual delivery |
| No restoration (15) | Nonce, Native, Inode, Prompt, Tool, StopContext, Pending, Compact, Continuation, Missing, Legacy, NewIncarnation (old reply), OldReply, Draft, PaneUnreadable | GREEN via final reasoned no-send |
| Race/replay/lifecycle (6) | Race, Replay, Cancel, Clear, Terminate, Stop | GREEN; Race may deliver before replacement or refuse after failed recheck |
| Due race (2) | DueRace, ReserveSwap | GREEN except the witnesses: front due race, leaf due race, deadline win over a delayed writer, key at due; swap after reserve with 0 keys |
| Race mutants (3) | NoFirstKeyGate, AttemptlessLeaf, NoReservedExpire | RED `RHOneDisposition`+`RHEndExactlyOnce` / same / both resolution properties |
| Stale identity (2+2) | StaleFront, StaleGeneration; IgnoreFront, IgnoreGeneration | GREEN except witness `RHNoStaleCallback`; mutants RED `RHStaleNoop` |
| Reboot/replay (3+3) | Reboot, OldSnapshot, NoticeCrash; RebootResetDue, RegrantMissing, EpochNoticeKey | GREEN except witnesses `RHNoRebootMidHold` / `RHNoCrashReplay`; mutants RED `RHBoundedHold` / `RHBoundedHold` / `RHOneNoticeRecord` |
| Deadline mutants (2) | NoDeadline, ResetDeadline | RED both resolution properties |
| Evidence mutants (5) | SkipNonce, SkipNative, SkipInode, SkipContinuation, SkipLaunch | RED `RHProofNeedsEvidence`; driver still prevents an unauthorized key |
| Driver mutation (1) | SkipDriverRecheck | RED `RHKeyNeedsRecheck` |
| Authority mutations (2) | BusyDirect, FallbackIdle | RED `RHNoBusyDirect` / `RHRunningContract` |
| Disposition mutations (2) | RequeueBody, RepeatNotice | RED `RHNoRequeue` / (`RHTypeOK`, `RHOneDisposition`) |

These are cfg-controlled model mutants. No product source was mutated or tested by this leg. No expected rule of a pre-existing cfg was changed. Final observed results and raw logs follow.

## Landing base 765ffc33c4

The #8212 early admission checks for an active turn before installing a watch (`src/services/discord/router/message_handler/intake_turn/owned_busy.rs:118`). A still-live pane that reaches Held without an active turn fails missing-pane retirement (`src/services/herdr_launch/retire.rs:58`). The same front is settled before a key with a reason notice (`owned_busy.rs:264`, `provider_dispatch/start_failure.rs:114`). `RestartNoHolder` covers this entry with `RHNoUnheldKeep` and `RHEndExactlyOnce`. The original model reaches Held and violates the new invariant. The revised model reaches explained with an 8-state generation and 4 distinct states. `RestartValid` still reaches Keep and then delivery, with 14 generated and 7 distinct states.

## Observed WSL TLC result

The same installed WSL TLC jar used for static statechart checks was used directly and through `scripts/ci/statecharts_tlc.py::check_cfg`, including its fairness validation and separate property runs. Jar: `/home/adk/.cache/tla2tools/tla2tools.jar`, SHA256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`; banner `TLC2 Version 2.19 of 08 August 2024 (rev: 5a47802)`. No product build was needed.

- Final full module check: **60 cfg / 257 TLC rule runs / unexpected 0**. Existing 24 cfg unchanged; new 36 cfg = 23 all-GREEN + Before expected liveness RED + 12 expected mutant RED. Fairness checks are additional calls, excluded from 257.
- Before direct run: exit **13**, 21 generated / **8 distinct** states. Trace: restart → queued → Busy Held → Unknown Keep(age1) → Keep(age2..5) → stutter forever, no key, no refusal, no notice. The checker attributes both violated liveness properties independently.
- Valid direct run: exit **0**, 14 generated / **7 distinct** states; all safety and four liveness properties hold. It reaches an actual Keep before restored wake → Idle reservation → one delivered original.
- Driver guard mutant direct run: exit **12**, 19 generated / **10 distinct** states; `RHKeyNeedsRecheck` fails after native replacement between wake and reservation. The source replacement and changed proof are read from the state; the mutant cannot hide them by omitting a diagnostic flag.
- Registry coverage: **1343 cfg / 0 problems**. Existing expected rules were not edited. The README table was regenerated only with `python3 scripts/ci/statechart_index.py`.

Raw logs live under `/home/adk/adk/lane-runs/i8222/logs/`: `tlc-restart-checker-2.log`, `HerdrBusyReobserveRestartBefore-final.log`, `HerdrBusyReobserveRestartValid-final.log`, `HerdrBusyReobserveRestartSkipDriverRecheck-final.log`. The earlier exploratory log `tlc-restart-checker.log` contains one healthy-delivery failure: unconstrained clock advancement skipped an already admissible next probe. The final refinement makes the next read attempt restoration before another Keep and preserves deadline progress after a failed driver recheck. It did not change the expected healthy result to RED. This result models liveness under the scheduling premise above; implementation wall-clock race tests remain required.

Modeling ran from approximately 17:29 to 17:40 KST, below the brief's 30-minute modeling limit. Product PG/first-key tests, deployment observation and independent pro design review remain pending; these model results do not claim their completion.

## Design review P2 revision (S2 first commit)

The pro design review found that deadline and first key never met in one state (P2-1) and that front, generation and replay never changed (P2-2). Before = design head `1209814ec0` RHSpec; after = this revision. Before rows ran TLC 1.7.4 on the old module with witness invariants added over its existing variables; after rows come from `scripts/ci/statecharts_tlc.py::check_cfg`.

| Check | Before | After |
|---|---|---|
| waking at due (`phase=waking ∧ age=5`) | unreachable: invariant holds, 7 distinct states | reached (`RHNoFrontDueRace` breaks as expected) |
| reserved/permitted writer at due | unreachable: `phase=reserved ∧ age=5` holds | reached (`RHNoLeafDueRace`); deadline win (`RHNoDeadlineWin`) and key at due (`RHNoKeyAtDue`) both reached |
| source swap after reserve | unreachable in Race (14 distinct states) | reached (`RHNoReservedSwap`), `RHKeyNeedsRecheck` holds |
| first-key gate removed / attemptless refusal of a reserved leaf | not expressible | RED `RHOneDisposition`, `RHEndExactlyOnce` |
| reserved attempt without its own expiry | not expressible | RED `RHNoInfiniteKeep`, `RHEndExactlyOnce` |
| front/generation comparison removed from `RHOwner` | 0 violations in Valid, Missing, Nonce, Race, Replay, Cancel | separate `IgnoreFront` / `IgnoreGeneration` RED `RHStaleNoop`; old callback reaches new front (`RHNoStaleCallback`) |
| due renewed at reboot / old-format hold regranted at reboot | not expressible (replay only set a flag) | RED `RHBoundedHold` each |
| crash between outbox enqueue and ack | not modeled | reached (`RHNoCrashReplay`); per-epoch key mutant RED `RHOneNoticeRecord` |

Result: 75 HerdrBusyReobserve cfg, 336 TLC rule runs, 0 unexpected (log `s2-tlc-2.log` in the lane log directory).

## tmux metadata-only recurrence

The idle owner reserves a tmux attempt before it waits for readiness. The idle screen is readable,
but the metadata-only tail has no completed reply. It reaches the actual preparation wait with
proof absent. The leaf finishes with exact NotSent, the same original keeps its queue slot, and
the owner commits its first readiness due in that transition. The existing drive arms its timer.
A retry preserves the due. Reboot retains the due and restores the timer, or settles an expired
original during boot. None of these observations grants a key.

Before, the tmux watch returns Keep with no stored due. Both `RHNoInfiniteKeep` and
`RHEndExactlyOnce` fail. After, the same fixture reaches one explained refusal. Reboot's expected
`RHNoRebootMidHold` witness demonstrates a pre-existing hold and an independent liveness check
covers resolution. `NoWiring` removes the new due assignment and restores the liveness failures.
