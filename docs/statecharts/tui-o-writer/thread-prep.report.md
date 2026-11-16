# O-a reader/view model validation

The two projections extend the existing WriterAdoption and ExecutionAuthority models. They prepare reader/view assumptions only; child output admission, permit installation, writer ready and Held effects are O-b obligations. Existing cfg and known-gap outcomes are unchanged.

| cfg | distinct states | result |
|---|---:|---|
| ExecutionChildViewAlias.cfg | 5 | Invariant ChildOwnPermit is violated |
| ExecutionChildViewBefore.cfg | 75 | ChildRecordRead liveness |
| ExecutionChildViewFixed.cfg | 150 | GREEN |
| ExecutionChildViewRest.cfg | 5 | Invariant ChildRecordedNoRest is violated |
| WriterThreadPrepAlias.cfg | 19 | Invariant OwnCandidate is violated |
| WriterThreadPrepBefore.cfg | 19 | Invariant SharedSnapshot is violated |
| WriterThreadPrepFixed.cfg | 180 | GREEN |
| WriterThreadPrepRest.cfg | 19 | Invariant RecordedRestZero is violated |
| WriterThreadPrepReverse.cfg | 21 | Invariant FloorBeforeInit is violated |
| WriterThreadPrepRollback.cfg | 58 | Invariant OldTargetRefused is violated |
| WriterThreadPrepUnshared.cfg | 19 | Invariant SharedSnapshot is violated |

Before → After: clone-local storage loses the child entry (SharedSnapshot RED); shared complete tuples restore it (GREEN). Execution enumeration omits a recorded child in Before (ChildRecordRead RED); the reader projection exposes it while keeping its permit absent in O-a (GREEN).

Safety checks: own candidate and permit identity, recorded REST0, floor before init, floor retained after failed init, old/O-a target refused, retirement remains a request. Liveness checks: RecordEventuallyRead and ChildRecordRead use SPECIFICATION ChildSpec and weak fairness on ReadRecord/ReadChild; authority projection retains the original Fairness. No SYMMETRY. No liveness is claimed during permanent REST/PG failure or for a child writer that O-a never starts.

Counterexample replay: the Rust snapshot clone, record-first reader, independent child candidate and init failure fixtures follow the same transitions; deploy fixtures exercise the actual rollback and common artifact pre-stage blocks. O-b must add child admission/ready, REST failure → recovery legacy once and boot-discovered late adoption replays.

Logs: `/home/adk/adk/lane-runs/i8118/logs/tlc`. Plain TLC 1.7.4 JAR; 11 cfg checked in under 1 minute on 2026-11-12. Model validation is limited to this projection and is not evidence of production child activation.

The activation contract these projections left to O-b is modelled in [thread-active.report.md](thread-active.report.md).
