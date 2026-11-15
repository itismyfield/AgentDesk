# Withheld input notice retry investigation

Code SHA: `5726d52ca8e3bae256e6955f3464262632287110`.
Status: implementation approved after DESIGN_DELTA; Before remains frozen to main. After/Floor/Late/Ledger now describe the writer-local wait and exact floor exclusion in this change. Rust evidence is recorded in the lane report.

The closest existing model is index 130 (`InputNoticeSettlement`): it owns notice identity, historical attribution, settlement and replay. Its retry boundary is extended by the small companion `InputNoticeRetry` rather than multiplying its unrelated HTTP states. Index 400 concerns actor claim retention on missing sources; index 090 concerns input-route wake scheduling. Neither owns the notice evidence ledger.

## Discovered code paths and model domains

Raw inventory: `/home/adk/adk/lane-runs/i8173/logs/impl-1/notice-entrypoints-git-grep.txt`. It was generated with `git grep -n -E 'NoticeEntry::|input_notices\(\)|notice_pending|defer_notice_delivery|notice_delivery_ready|before_migration_floor' -- src ':!*tests*'`. All table paths are relative to `src/services/tui_o/` except the explicitly named `tui_input/` path, which is relative to `src/services/`. The evidence mutation search is separately saved as `evidence-mutations-git-grep.txt`.

A single `(source,start,end)` is sufficient to reproduce this bug. Presence of entries is Boolean; no arbitrary input-age or session-expiry range is assumed. `phase` enumerates the existing select, deliver and rotation calls. `posted` counts zero or one successful POST; `TypeOK` detects any second POST. `Mode` and `Scenario` choose code/control paths, not new production states.

| Entry point at code SHA above | Model value/action |
|---|---|
| `writer/input_notices.rs:139-211` validates Claude native row, UUID and original provenance, suppresses known injected input, returns Piece | seed `owed = TRUE`; one source-bound notice |
| `writer/pieces.rs:115-139`, `writer/rotation/owe.rs:24-26,102-122` enqueue a Piece on both replay/live capture | initial owed debt; later Body represented by `bodyPosted` |
| `writer/input_notices/evidence.rs:16-58` retains pending evidence, then `mem::take` consumes it | seed corresponds to the queue/evidence split after pending consumption; pending memory is not durable evidence |
| `writer/actor/notice_work.rs:177-182`, `writer/input_notices/evidence.rs:64-78` skip below-floor evidence before Materialized | `Scenario = Floor`, initial `evidence = FALSE`; Before classifies by migration lower/timestamp/binding; After terminal proof additionally requires a row offset or timestamp boundary |
| `writer/actor/notice_work.rs:207-213,284-298` writes Materialized and fills late origin | `ObserveEvidence`, missing/present evidence |
| `store/ledger/input_notices.rs:201-259` MigrationStarted freezes bounds; Materialized merges optional fields and inserts evidence | Floor proof and `ObserveEvidence`; production evidence is never removed by these actions |
| `store/ledger/input_notices.rs:264-331` LegacyImported / MigrationUnresolved | imported terminal entry maps to `ObserveLedger`; unresolved preserves pending evidence, no retry permission by itself |
| `writer/input_notices/migration.rs:202,239-278` emits MigrationUnresolved and reconciles terminal receipts through store writes | ledger-change scenario; no invented evidence deletion |
| `writer/actor/notice_settlement.rs:100-162`, `store/ledger/input_notices/settlement.rs:86-141,146-194` Policy/Owned settlement; retains evidence, sets exclusion, refuses settlement of prepared work | `ObserveLedger`, `excluded = TRUE`; evidence remains present if already materialized |
| `store/ledger.rs:583-610`, `writer/confirm.rs:89-99`, `writer/deliver.rs:860-875` Posted/Excluded, delivery reconciliation and SpoolGc | `posted`, `excluded`; SpoolGc changes retention metadata, never deletes durable evidence |
| `writer/input_notices/admission.rs:56-69,90-99,269-283` reads evidence before checking migration floor; existing Excluded path | `Deliver` refuses absent evidence in Before; After uses the existing floor policy only with proven floor evidence |
| `writer/deliver.rs:696-725`, `writer/input_notices/retry.rs:24-34`, `tui_input/human_frame/history/notice_owner/generation.rs:133-155` Withheld creates a five-second delivery timer | `delayed = TRUE`, `withheld = TRUE`; the model omits timer expiry since immediate erasure already suffices |
| `writer/actor/notice_settlement.rs:82-98`, `tui_input/human_frame/history/notice_owner/generation.rs:36-61` select removes delivery timers whose evidence is absent | `Select` in Before resets `delayed` without a wake |
| `writer/actor.rs:716-762` rotates an unacked input once per finite pass, lets later Body run | `Deliver`, `Rotate`, `bodyPosted`; no infinite inner pass is assumed |
| `writer/actor.rs:350-405`, `writer/rotation/readers.rs:198-263`, `writer/replay.rs:16-55`, `writer/deliver.rs:269-271` restart reconstructs owed from retained raw and restores durable terminal acknowledgements, resets cache | one optional `Restart`; one fresh boot observation is allowed, then the same no-retry rule |
| `writer/rotation/owe.rs:126-178`, `writer/input_notices/admission.rs:101-147` historical binding creates origin; admission has no live tmux-session existence predicate | optional `SessionGone` does not change evidence eligibility; late historical evidence must remain deliverable |

`After` maps to the implementation described below. A Floor seed means the classification was proven from raw evidence at the persistence boundary; an arbitrary missing notice cannot be assigned this scenario. `Late` assumes eventual Materialized evidence, and `Ledger` assumes eventual terminal ledger change. Weak fairness applies to the actor phases and those explicitly enabled environment writes. It does not assert that every absent evidence record eventually appears.

## Rules and controls

- `NoRetryWithoutWake`: a second Withheld admission in the same incarnation requires a relevant evidence/ledger change. A fresh incarnation is allowed one bootstrap observation.
- `NoDuplicate` and `EvidenceExactlyOnce`: materialized deliverable evidence is posted once, under the explicit successful-POST abstraction. HTTP uncertainty/reconciliation remain the existing writer contract, outside this model.
- `NoEarlyLoss`: potential late evidence cannot be discarded, including after `SessionGone`.
- `ImpossibleEventuallyClean`: a proven Floor record or existing terminal ledger entry eventually leaves owed.
- `BodyEventuallyPosts`: rotating a withheld notice does not block later normal output.

Before is frozen to current behavior. After/Floor/Late/Ledger specify the requested behavior. NoEvidenceWake and NoLedgerWake delete one dependency wake each and must violate the corresponding liveness property. The late-evidence case starts from an allowed absent-record debt state; this model does not claim a second normal producer bypasses the actor's synchronous persist boundary.

## Counterexample replay

Before: initial owed Piece / absent evidence → Select → Deliver returns Withheld and stores delay → Rotate → Select deletes delay because evidence is absent → Deliver retries without any evidence or ledger write. `NoRetryWithoutWake` breaks. With immutable floor proof and no Materialized producer, this cycle also violates `ImpossibleEventuallyClean`.

This matches the code and explains why the existing five-second timer does not limit this particular loop. The main actor still sleeps one second per outer poll; several stale notices and two log lines per refusal account for multiple lines per second. This is not evidence of a zero-sleep inner loop.

## Approved implementation

The coordinator approved aligning the existing migration-floor policy at the pending-evidence consumption boundary, using a writer-local missing-evidence wait. Current-session absence is explicitly excluded as a cleanup proof. Channel-specific original omission causes remain unconfirmed; operating log/CPU acceptance belongs to the coordinator after deployment.

| New or changed entry point (paths relative to `src/services/tui_o/`) | Model mapping |
|---|---|
| `writer/deliver.rs:210,273` missing UnitKey set created empty with each writer | After `delayed` Boolean; Restart clears only the in-memory wait |
| `writer/input_notices/retry.rs:45-65` missing evidence records a separate hold instead of a WorkCache timer | Deliver transitions to `delayed = TRUE` |
| `writer/input_notices/retry.rs:6-29` existing tuple evidence or existing durable Posted/Excluded releases the hold | ObserveEvidence / ObserveLedger; unrelated writes do not cause a re-admission |
| `writer/input_notices/evidence.rs:82-93` source present in frozen snapshot or valid parsed native timestamp | record-boundary Boolean: Floor can be terminal; old-binding-only omission is Untimed and remains owed |
| `writer/actor/notice_work.rs:173-226` record-boundary proof, exact UnitKey/index/payload/origin/record match and no Prepared, then existing Excluded before pending evidence is consumed | After Floor terminal branch; Deliver abstracts the exclusion/normal actor acknowledgement rather than a POST |
| `writer/actor/notice_work.rs:492-495` retain holds only for keys still queued after notice settlement | terminal wait removal; no new policy or timer |

The existing `before_migration_floor` predicate and evidence-consumption policy are unchanged. Known-source `start == lower`, an outside source with no timestamp and binding newer than the frozen checkpoint, and an outside source with a future timestamp remain eligible. The existing no-timestamp/old-binding fallback at `writer/input_notices/evidence.rs:74-75` is insufficient for the NEW terminal exclusion: an old binding cannot date a newly appended row. After therefore leaves that debt and raw record recoverable, without repeated admission, unless a record boundary proves the floor. No current-session test or TTL grants cleanup authority.

`Untimed` is the concrete combination already admitted by the original entry points: source absent in migration snapshot, native timestamp absent/unparseable, original binding at/below checkpoint. The new Rust test appends the row after MigrationStarted to demonstrate why this is not a terminal proof. Existing main can still omit its materialization; automatic delivery of that ambiguous legacy cohort is outside this change. This change preserves that debt instead of adding irreversible exclusion. `NoRecordBoundary` removes this one proof condition and must break `NoEarlyLoss`.

The primary floor Rust fixture now runs a previous writer without notice projection, actually captures the native row into the store, obtains its real cursor and prefix hash, freezes that boundary, then reopens/replays twice. It does not assume an arbitrary lower bound ahead of the source cursor. The health check seeds the real AlarmRouter waiting reason and advances beyond WAIT_LIMIT (300 seconds) after settlement.

## Execution

Only this companion model's nine configurations were checked using the repository checker against an isolated copy of their exact expected.tsv rows. No existing settlement or missing-source configuration was reclassified.

| cfg | Safety violation | Liveness violation | Result |
|---|---|---|---|
| Before | NoRetryWithoutWake | ImpossibleEventuallyClean | expected RED |
| After | — | — | GREEN |
| Floor | — | — | GREEN |
| Late | — | — | GREEN |
| Ledger | — | — | GREEN |
| Untimed | — | — | GREEN, uncertain debt retained |
| NoEvidenceWake | — | EvidenceExactlyOnce | expected RED |
| NoLedgerWake | — | ImpossibleEventuallyClean | expected RED |
| NoRecordBoundary | NoEarlyLoss | — | expected RED |

Final checker: **rc=0**, **9/9 configurations matched**, **38 TLC runs plus fairness prechecks**, **39.2 s**. Both removed-wake controls are RED, and the removed record-boundary proof is safety RED. Before's safety trace is separately recorded. These are model results, not Rust test counts.

Logs: `/home/adk/adk/lane-runs/i8173/logs/impl-1/tlc-proof-final.log` and `tlc-before-final-proof-raw.log`. Exact inputs and hashes are in the lane report. Earlier intermediate runs/syntax-error logs are retained separately and are not final-head verification evidence.
