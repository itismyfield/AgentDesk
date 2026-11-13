# Notice evidence lifetime: design model

Code baseline: `79bebdd4eaa29b289bf88412825baa7d70a6ac77`.
Model SHA256: `a120173404fe7cf8e28cafd39f57776e16712019622d56a748e2d3955684c0f7`.
Design: [notice-settle.design.md](../../notice-settle.design.md).
Status: design GREEN under explicit recoverability assumptions; production unchanged, independent review pending.

The module EXTENDS WriterWake and interleaves its real Route transitions with notice transitions.
Notice actions leave bootAll unchanged, preserving custody, checkpoint, execute-once and the existing
blocked self-wake guard. The original BootSpec and source/UUID InputNoticeSettlement models are
also checked without modification. The Mermaid drawing is a hand-authored overview; counts and
counterexamples come from TLC, not the drawing.

The code-entrypoint table in the design supplies every domain: retained complete native record,
unknown/Owned/Unowned/policy evidence, strict binding source boundary, Prepared/effect/Posted,
reason rotation and one bounded crash. The single-record model does not prove attribution among
twins or foreign registrations; those belong to InputNoticeSettlement and the required Rust tests.
The liveness model uses weak fairness, no SYMMETRY, and a fairness precheck. It does not model
unbounded process failure, permanent delivery denial or missing evidence as recoverable facts.

## Result matrix

| Config | TLC runs excluding fairness precheck | Broken rules (expected) |
|---|---:|---|
| NoticeEvidenceAfterDirect.cfg | 4 | GREEN |
| NoticeEvidenceAfterOwned.cfg | 3 | GREEN |
| NoticeEvidenceAfterPolicy.cfg | 3 | GREEN |
| NoticeEvidenceBefore.cfg | 3 | EveryPendingTerminates |
| NoticeEvidenceDropLog.cfg | 4 | PendingLoggedOnce |
| NoticeEvidenceDropReplay.cfg | 4 | ReplayKeepsTerminal |
| NoticeEvidenceExpireUnproven.cfg | 6 | DirectEventuallyPosted, NoExpireOwedDirect, NoQuietLoss |
| NoticeEvidenceNoSettle.cfg | 3 | EveryPendingTerminates |
| NoticeEvidenceReasonLog.cfg | 4 | PendingLoggedOnce |
| NoticeEvidenceUnavailable.cfg | 3 | EveryPendingTerminates |
| InputNoticeSettlementBefore.cfg | 4 | AcceptedEventuallyEchoed, OwnedEventuallyExcluded |
| InputNoticeSettlementAfter.cfg | 4 | GREEN |
| WriterWakeI8055Design.cfg | 2 | GREEN |
| WriterWakeI8055RouteDesign.cfg | 3 | GREEN |

`MODEL_MATRIX configs=14 failed=0`, 50 actual TLC runs plus 14 fairness checks.
A RED config matching expected.tsv is an expected counterexample, not a production PASS.
The three After configs passed all listed safety rules and temporal properties.

## Counterexamples and code correspondence

1. **Before / NoSettle:** materialize → ownership proof remains unavailable to the original
   scope (or settlement connection removed) → arbitrary pending-reason rotation forever.
   Baseline `notice_work.rs:256` selects this record on every prepare; `:328` has no record terminal
   action other than the existing key-based admission; `notice_owner.rs:225`, `:241`, `:407`
   preserve mismatched/unbound registrations as Pending. The Before fixture represents this
   specific stuck path, not every owned record in main. NoSettle removes the proposed cause fix.
2. **ReasonLog:** two defer calls with different reasons increment logs twice.
   Baseline `notice_work.rs:411` compares the immediately previous reason, matching the issue.
3. **DropLog:** defer → crash → defer violates once-per-record if the bit is memory-only.
   Baseline `deliver.rs:265` reconstructs an empty map, and `notice_work.rs:330` can clear a record
   even without a crash. Durable PendingObserved is the proposed stronger contract.
4. **ExpireUnproven:** materialize a direct row → close session before proof recovery → expire.
   NoQuietLoss and NoExpireOwedDirect fail, and the direct row never posts. This is a rejected
   shortcut, not baseline behavior. Baseline never treats `fresh_resume.rs:24`'s Codex rescope
   as Claude notice expiry and `admission.rs:177` only excludes on its explicit clear proof.
5. **DropReplay:** settle → crash reconstructs Pending despite saved terminal. This rejects
   an optional Materialized field that an old reader ignores; the design uses explicit ledger variants.
6. **Unavailable:** materialize → proof permanently missing → Pending forever. This is a retained
   limitation, including after the proposed fix. `notice_owner_tests.rs:314` and `:639` require
   missing/ambiguous proof to remain debt; a timeout cannot satisfy safety and unconditional
   liveness together. This RED config prevents a false zero-pending claim.

## Scope of the liveness claim

EveryPendingTerminates means Pending eventually becomes Posted, Excluded (proven no notice required)
or Expired (the same exemption proof plus a confirmed old-source boundary). DirectEventuallyPosted
separately forbids treating the direct row as a successful expiry. After assumes necessary evidence
returns and the existing actor/admission/transport confirmation progresses fairly. Session closure
can happen before or after evidence recovery, and a direct obligation survives either ordering.
No terminal/action grants input execution or wake authority.

The model abstracts a validated source/UUID proof as one value. Actual proof validation, data races,
old-format parsing, append durability, unknown variants, migration candidate reservation, source
replacement, first POST polling and channel accounting require the implementation tests in the design.
The existing Prepared/history-confirmation contract is abstracted as at most one effect; this lane
has not re-proven all network ambiguity cases. No Rust tests or cargo commands were run for this design.

## Reproduction and evidence

TLA2TOOLS jar (1.7.4, TLC prints version 2.19):
`/home/adk/.cache/tla2tools/tla2tools.jar`, SHA256
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`.
Java: OpenJDK 21.0.13. Linux/WSL; one TLC worker per run, two configs at a time.

    unset TMUX TMUX_PANE
    TLA2TOOLS=/home/adk/.cache/tla2tools/tla2tools.jar python3 scripts/ci/statecharts_tlc.py --base origin/main --jobs 2

This checks the ten new configs after committing the design. The actual 14-config check uses the
repository check_cfg/check_fairness runner, including separate temporal-property runs, and records
raw output through this external wrapper:

    python3 /home/adk/adk/lane-runs/i8112/logs/design-b1/check_models.py

Evidence root: `/home/adk/adk/lane-runs/i8112/logs/design-b1/`.
`model-results.json` and `model-matrix.log` contain the matrix; each
`*_NoticeEvidence*-NN.log` contains one raw checker/TLC result with PROCESS_EXIT.
The first manual smoke run encountered a missing config caused by a working-directory mistake;
its corrected rerun is `tlc-after-owned.log` (1,204 states, GREEN). That setup failure is not
counted as a model RED or a successful check. Mode-based mutants leave model bytes unchanged;
no production mutation or restoration claim is made before implementation.
