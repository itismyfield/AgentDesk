# Watcherless presence identity

Code reference: `97b7f6b352`; design reference: `75726b3b1d`. Main abstracts the strict admission/final-files defects while sharing S1 control flow; it is not an exact replay of the base implementation. This model compares completed admission decisions and publication events, not the last displayed screen against an asynchronously changing process.

The ready/open reader starts without a watcher. Progress cycles indefinitely. A single non-wrapping boundary changes session, owner, spawn/turn nonce, source, or core basis; each is an equality class. Missing and unreadable represent absence and failed guards/read sources respectively. Crash discards the Reading and Boot collects the same surviving row under a new, non-wrapping process identity. A separate unrelated owner epoch bump exercises the one admission retry. Fault recovery occurs before the next read. This deliberately small bound checks one perturbation while proofs exist; it does not establish liveness under indefinitely repeated owner changes or failed reads. Notice is continuous Unknown >= 2, with affected input assumed; durable receipt/HTTP are covered by Rust notice tests.

`accepted` is the last completed admission decision. `published` is cleared at each Begin, so PublicationContinues cannot pass on a retained Busy screen. Work has weak fairness; Progress is never required to stop.

| Config | Expected violations |
|---|---|
| Fixed | none |
| Main / M1 | NoProgressIdentityUnknown, NoProgressIdentityNotice, EventuallyStableBusy, PublicationContinues |
| M2 final-files full row | PublicationContinues |
| M3 session-name only | NoWrongAdmission, NoWrongPublication, NoOldProofAfterObservedBoundary |

The final full runner completed all 65 repository configurations with no manifest mismatch in 219.1 seconds (`models-release.log`). The five presence configurations completed with exactly their expected violations; every configuration stayed below the 300-second CI budget. The final model SHA256 is `48403b68fa116d12c8100368785eee1c590bfd18da3a4619e221cb5335e846e2`.

An isolated Main manifest expecting no violations exits 1; the same isolated Fixed check exits 0. Direct TLC confirms safety counterexamples with exit 12 and temporal counterexamples with exit 13. Fixed directly checks 242,965 generated / 100,392 distinct states, completes with an empty queue and exit 0, and checks both temporal properties. Raw traces and exits are in lane-runs/i7769/logs/s1-2/model-release-direct.log and model-release-traces/. These are model results, not Rust test results.

FinalCheckAndPublish checks a publication event against its sampled inputs, including process identity. It does not establish a global owner lock across external I/O and callback; that additional synchronous fence is outside this change. The one-perturbation bound checks identity change, read fault or restart separately; it does not enumerate every combination of repeated failures and restarts.

- Actual base (`97b7f6b352`): the first strict admission-pair mismatch goes directly to Unbound (inflight_bound.rs:37-41). Its fresh-pair and pinned publication comparisons are both full-row strict (live.rs:578-579).
- S1 M1: first strict pair mismatch -> recollect -> second strict pair mismatch -> Unbound.
- Current Main configuration: reproduces the strict admission/final-files comparison defects, but shares S1 recollection and the projected publication-pair control flow.

In the shared Main/M1 abstraction, repeating the failed two-pair cycle reaches the notice threshold and defeats stable Busy. M2: admit the same execution, read open, pass publication pair, then Progress before FinalCheckAndPublish; the strict final compare at base live.rs:606 can fail every cycle. M3: change a non-session identity between collections or after pinning; name equality accepts a different turn, matching the need for turn_nonce and owner/source axes in both comparison paths.
