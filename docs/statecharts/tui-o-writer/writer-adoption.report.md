# Writer adoption and the TUI Legacy body (#7651 stage 1)

Code baseline: `13f9acdd9a0f5e3aa500d20cd1b9a31a91e7ced4`; line numbers below are at this change's head, which moves `channel_policy.rs`, `channel_policy/adoption.rs` and `cutover/channel_gate.rs`. The stage-1 behavior is plan r2 with its r2.1 corrections (lane `report-p1-2.md`); `Fix = "gateOnly"` is that plan without the warm takeover from the Legacy delivery frontier.

One TUI-bound channel carries one turn; one non-TUI channel in the same config carries a headless turn. The model checks who posts the TUI turn's body, when its row and the watcher dispatch may be settled, and whether a held turn is eventually answered.

## Current disposition contract (2026-11-17)

The current decision first normalizes the same execution, then adopts it or emits one body-free rejection reason and discards that episode. The old Refused-body retention and owner-return replay decision below is historical. `confirm_store` restores channel readiness only; it cannot revive a rejected output. New input receives a different output identity. Normal late-child handoff remains valid for an episode that has not been finally rejected.

D1a covers the fresh policy, notice receipts and atomic adoption/rejection foundation; it installs no production policy: the single installation boundary is the `#[cfg(test)] mod output_disposition` declaration in `channel_policy/adoption.rs`. Its normalization, trust and receipt helpers are compiled for direct policy fixtures. D2 connects every live consumer and body store before enabling that child. D1a validates only the explicit fresh-policy fixtures; pass/fail evidence belongs to its implementation report. Existing production fresh, warm and resume behavior remains unchanged. Continuation disposition/proof/turn-window normalization is an unverified D1b follow-up and is excluded from D1a completion. It must bind a genuinely new native input window to the current input admission; renaming a rejected old window with another input/nonce is not a new episode. Production activation and zero rejected retention across all stores remain D2 work.

`NormSpec` checks one total deadline, old-episode non-revival, a new-input episode, and body/reason effect exclusion. Its abstract episodes do not establish that continuation Rust is implemented or verified. Existing stage-1/Herdr retention tables below remain versioned controls; they do not prove the new discard policy. Rejection notice reservations do not become body Posted/input-success receipts, and an uncertain reservation is metadata-only pending rather than a blind repeat POST.

## Values and where they come from

| Variable | Values | Code |
|---|---|---|
| `sel` | default, explicitOff, listExcl, lateOff, defaultOff | `shadow/tap.rs:61-67`, `channel_policy.rs:73-134` (implicit default off when no cluster ids or a binding is rejected) |
| `site` | home, foreign | `channel_policy/adoption.rs:384-390`; off the home `seeded` makes no candidate (`channel_policy.rs:316-324`) |
| `root` | runtime root present or not | `channel_policy/adoption.rs:419-421` reads every channel as `Held` without a root |
| `exec` | none, permit, unverifiedHeld | `channel_policy.rs:446-513`, `ownership/execution.rs:72-78`, gate `cutover/channel_gate.rs:461-471` |
| `bound` | none, fresh, withOutput | `writer/late_adopt/codex.rs:205-207` (no new bind, Retry), `late_adopt/codex/prefix.rs:205-213` with `codex.rs:229-230` (bound rollout already has output, Skip) |
| `store` | none, init, damaged | `channel_policy/adoption.rs:409-434` |
| `adopt` | None, Pending, Deferred, Committed, Held, Released, Refused | `channel_policy/adoption.rs:16-28`; `Refused` is the stage-1 refusal (plan r2 G5) |
| custody | another turn's row, pending start or open intake; the held turn's own row | `runtime_bootstrap/o_writer_host.rs:306-339`, `writer/deferred.rs:89-92`; the kinds block alike, so the model keeps whose custody it is |
| `nativeEnded` | the TUI turn's own end record written before anyone delivered the turn | `writer/late_adopt/codex/prefix.rs` `frontier` (a `task_complete` ends its turn and never moves where delivery stands) |
| `endedBeforeBase` | that end was written before the takeover's baseline | `writer/late_adopt/codex.rs` `start_on` (`ended_before`) |
| `prevOwed` | the rollout bound before holds a turn Legacy may have sent part of | `writer/late_adopt/codex.rs` `owed` (`Start::Legacy` retries) |
| `attempts` | 0..3, tmux absent | `discord/inflight.rs:341-344`, `recovery_paths/shared.rs:156-171` |

Actions map to the bridge tick and terminal (`turn_bridge/stream_tick.rs:389-397`, `terminal_outcome_delivery.rs:440-453`), restart recovery (`recovery_engine/completion_delivery.rs:117-151`), the watcher dispatch finalizer (`tmux_watcher.rs:2351-2392`), the takeovers (`late_adopt.rs:182-186`, `late_adopt/codex.rs:203-310`), the stale end-of-file handoff (`writer/deferred.rs:26-29`), refusal (`writer/host.rs:659-678`, `host/first_init.rs:79-80`, `writer/activation.rs:223-225`) and headless delivery (`turn_bridge/headless_delivery.rs:588-608`).

A restart keeps the turn: a running turn's bridge is restored and judges again (`restore_inflight.rs:2378`); a held row waits for the once-per-boot recovery.

## Rules

- `NoLegacyBodyOnTui`: Legacy posts no body on a configured/nonmember TUI channel. The sole inherited-child exception is main's late reservation: its pre-init open turn is sent once by Legacy on its own candidate/send counter, then handed to O. A configured binding cannot enter that exception.
- `AnswerAtMostOnce`: O and Legacy together post the turn at most once.
- `UnadoptedNotOwned`: without an O store the gate never answers that O owns the body.
- `RowClearedHasEvidence`, `NoEarlyDispatch`: the row and the watcher dispatch settle only after a post, or while the O store owns the turn's range unposted.
- `HeldBodyNotSkipped`: a commit while the turn is held starts at the Legacy delivery frontier, not at the end of file, even after Codex wrote the turn's end.
- `OtherCustodyBlocks`: no commit while another turn's custody is open.
- `NoCommitPastOwed`: no commit while the rollout bound before still holds a turn Legacy may have sent part of.
- `HeadlessKept`: the non-TUI turn is never held and posts its body.
- `HeldTurnAnswered` (PROPERTY): a held turn on an adoptable channel (home, default selection) is posted by O unless a refusal happened.
- `AdoptionSettles` (PROPERTY): a Deferred adoption with a turn on an adoptable channel commits unless a refusal happened.
- `RowEventuallySettled` (PROPERTY): a turn O posted gets its row settled.

`NativeEnd` (Codex writing the turn's end, before or after the baseline) is environment, like `Input`; `PrevDrains` (the earlier rollout's turn delivered or proven unsent) is weakly fair. `rejected` records any refusal: the `Reject` action, and a seed that is `Refused` (no runtime root, or an unverified execution view without a store). `Reject`, `Input`, `Restart`, the stale handoff and the watcher finalizer carry no fairness; writer, recovery and turn-end actions are weakly fair.

## Results (TLC 1.7.4, `scripts/ci/statecharts_tlc.py`)

| Config | Fix, Mut | Breaks |
|---|---|---|
| Main | main | `NoLegacyBodyOnTui`, `UnadoptedNotOwned`, `RowClearedHasEvidence`, `NoEarlyDispatch`; liveness `AdoptionSettles` |
| GateOnly | gateOnly | `HeldBodyNotSkipped`, `RowClearedHasEvidence`; liveness `HeldTurnAnswered`, `AdoptionSettles` |
| Stage1 | stage1 | none |
| OwnedMut | stage1, a held body read as owned | `UnadoptedNotOwned`, `RowClearedHasEvidence` |
| UnadoptedOwned | stage1, refusal kept as Held and the execution view answering owned | `UnadoptedNotOwned`, `RowClearedHasEvidence` |
| ExemptAll | stage1, every custody exempt | `OtherCustodyBlocks` |
| WatcherNoGuard | stage1, dispatch without evidence | `NoEarlyDispatch` |
| BudgetMut | stage1, waiting and refusal counted in the recovery budget | `RowClearedHasEvidence` |
| HeadlessMut | stage1, the non-TUI turn held too | `HeadlessKept` |
| AdvanceOnEnd | stage1, a warm takeover that moves the frontier past a turn Codex ended but nobody delivered | `HeldBodyNotSkipped`, `RowClearedHasEvidence`; liveness `HeldTurnAnswered` |
| RelayedByTime | stage1, a turn that ended before the baseline counted delivered by time alone | `HeldBodyNotSkipped`, `RowClearedHasEvidence`; liveness `HeldTurnAnswered` |
| SwallowOwed | stage1, the new rollout committed while the one before still owes a retry | `NoCommitPastOwed` |
| NoSerial | stage1, a second recovery of the kept row without the restore lock and disk recheck | `MarkerAtMostOnce` |
| NoRetry | stage1, the adoption wake gives up at its first judgement when a claim holds the row; the claim still ends | liveness `RowEventuallySettled` |
| Retries4 | stage1, a fourth wake retry allowed | `TriesBound` |

Main's violations are code paths on the baseline. Legacy posts the Deferred turn (the 11-08 trace: Deferred seed, a bound rollout with output skipped, warm input, Legacy W14/W10). Without a runtime root, or with an execution view and no store, the gate answers owned (`adoption.rs:419-421`, `channel_gate.rs:461-471`), so the bridge commits and the watcher settles with nothing posted. GateOnly holds the body but the warm channel never commits, or commits at end of file and skips the held turn.

## Left out, and how it relates

- Codex turn-mode confirmation (`cutover.rs:81-91`) waits for input after a commit; it does not change adoption.
- Herdr row-end was excluded from the stage-1 model. Its notice can carry a body; the companion below audits the held row and execution/body split.
- Watcher streaming (W02) uses the same gate; only the terminal and dispatch are modelled.
- The output lock and delivery lease (`terminal_outcome_delivery.rs:735-757`) prevent duplicates; a held body takes no lease.
- Turn presence keeps a held turn Busy; that is `turn-presence`'s `EventuallyStableBusy`.
- Census instrumentation (denominators, end evidence) is an observation contract checked by Rust tests.
- A transiently failed recovery send is not modelled (`Answer` is the gate's answer only); the wake's retry after one is fixed by the Rust test `a_transient_recovery_failure_is_judged_again_and_settles_the_kept_row_pg`.
- The kick's claim is assumed to stay open across at most `MaxSpan` judgements, within the wake's retry window (`MaxSpan` = `Retries`); the claim ends on its own (`ClaimEnds`) at any judgement. Only Stage1, NoRetry and Retries4 explore an open claim; the other configs start without one to keep their state space. A claim outlasting the window exhausts the wake (`WakeGiveUp`) and leaves the row to the next ready transition or a restart; that path is explored with `NoRetry`.
- A wake's transient send failures spend no restart budget; that and the four judgements with 1, 2, 3 s backoff are fixed by the Rust test `a_wake_whose_sends_keep_failing_ends_after_three_retries_and_keeps_the_row_pg`.
- tmux presence: with tmux alive the recovery budget never clears a row, so only the absent case is modelled.

## Herdr row-end boundary on the relanding head

The outer-only proposal below is historical and **withdrawn**. The current design
audit is [Herdr execution retirement and retained body debt](herdr-body-hold.design.md).
The notice can carry a response body; the old nonbody exclusion is not a body
settlement guarantee.

Code: `13355ee75c74ef65b2c5564f0a95efb37f988d26`, containing main
`ad04f6f3ecc33e86e7dc0968822ce2ce3f087e90` and the existing body-adoption gate.
`WriterAdoptionHerdrRowEnd.tla` is a companion leaf of this adoption model. It checks
an already-retired prior-process Herdr row, with readable canonical identity,
no active actor, healthy routing and successful transport if offered a notice.
The existing episode/nonce guarded clear is retained. It does not model an
unconditional permission to start a new Herdr execution.

The outer ownership observation is not the final body boundary. Mapping
AwaitingAdoption and AdoptionRefused to false there still selects `Settle::Notice`.
That branch can contain recovered assistant text, and its helper peeks at ownership
again before even classifying a body-free marker. The proposed outer mapping
therefore does not establish the requested progress.

| Value/action | Existing code on the code SHA above |
|---|---|
| owner=Deferred | `channel_policy.rs:345`, `channel_policy/adoption.rs:181`; `cutover/channel_gate.rs:445` returns AwaitingAdoption |
| owner=Refused | `channel_policy/adoption.rs:343`; `cutover/channel_gate.rs:446` returns AdoptionRefused |
| owner=NoCandidate | `channel_policy.rs:320`, `cutover/channel_gate.rs:509`; selected channel without a local candidate or execution view |
| owner=Committed | `writer/activation.rs:243`; the output peek answers true |
| owner=UnknownKind | `cutover/channel_gate.rs:487`; a genuine identity error remains unreadable |
| bound | `writer/late_adopt.rs:221,250`; `late_adopt/codex.rs:260`; no-source retry cannot commit |
| row/inputHold | `discord/recovery_engine/herdr_row_end.rs:52,238,254,269,495`; a prior-process row and the execution's input hold are distinct |
| noticeBody | `herdr_row_end/outcome.rs:57-72`; completed answer, saved partial, or body-free empty-answer marker; `completion_delivery.rs:316` classifies it |
| Observe/Commit | `herdr_row_end.rs:238,254`; commit releases the execution hold before the notice |
| Notice | `herdr_row_end.rs:310` calls `completion_delivery.rs:392`; the latter peeks again at `:412` and returns held at `:416` |
| Clear/Closed | `herdr_row_end.rs:269,279`; neither executes after notice_not_delivered |
| gatewayHeld/nextInput | `herdr_row_end_settle_tests.rs:347-385`; both gateway and REST new starts queue while this old row remains |
| Bind/Adopt | new-source progress can follow the input passing that old-row guard; with a source and row-only custody adoption may commit (`deferred.rs:793`, `host/activation_io.rs:26`) |

`NewStartEventuallyPassesRow` means passing the **old row's guard**. It does not
bypass the separate `OWriterNotReady` check at `turn_host.rs:308`. The no-binding
cases permit adoption never arriving, as required by this audit. Bound-source
and already-owned controls separately check progress when adoption is possible.
The safety invariant remains `NoLegacyBodyOnTui`; the companion also checks
`UnadoptedNotOwned`, `UnknownOwnerKept` and `ClearHasEvidence`. All row-end actions
have weak fairness, so unfair scheduling is not the reason for the counterexample.

| Companion cfg | b2 before outer mapping | With the proposed outer mapping / control | Safety |
|---|---|---|---|
| HerdrBefore | row settlement and next-input row-guard liveness RED | baseline negative control | all four invariants GREEN |
| HerdrOuterOnly | same unadopted Deferred/Refused/NoCandidate inputs as Before | both liveness properties still RED at the inner notice gate | all four invariants GREEN |
| HerdrBound | bound source permits fair adoption | both liveness properties GREEN | all four GREEN |
| HerdrOwned | already Committed | both liveness properties GREEN | all four GREEN |
| HerdrUnknown | true identity error | row remains held; no progress property is claimed | all four GREEN |
| HerdrMain | historical false/Legacy delivery behavior | both liveness properties GREEN | NoLegacyBodyOnTui RED when the notice carries a body |

All six configs were run through `scripts/ci/statecharts_tlc.py`: **6 configs,
17 TLC runs, 9.8 s, exact expected violation sets matched**. The checker's exit 0
means the registered negative controls reproduced; it does **not** mean the
proposed change passed liveness. Three additional plain TLC runs, one each for
Deferred, Refused and NoCandidate, all returned liveness exit 13. Their trace is
Observe -> Commit -> Notice -> stutter: row=true, gatewayHeld=true, nextInput=queued,
bound=false, noticeSent=false. The execution input hold has already been released,
which still does not clear the row or unblock the next input.

Raw logs and per-owner traces:
`/home/adk/adk/lane-runs/i7651/logs/reland-b2/resume2-tlc/`.
TLA tools jar SHA256:
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`.
The initial model-initialization error is retained as `checker-initial-error.log`;
it generated zero states and is excluded from all RED/GREEN counts.

The actual body helper's existing tests also ran on the unchanged source:
`a_pending_adoption_holds_every_recovery_notice_until_o_adopts` and
`a_held_recovery_body_spends_no_restart_budget_and_keeps_its_row` passed 2/2,
failed/ignored 0. They confirm that pending/refused recovery bodies remain held,
and that even body-free markers passed through this helper wait for adoption.
The approved outer-only production mapping has not been applied: the remaining
body boundary prevents the requested liveness, so this leg stops for DESIGN_DELTA.


## Execution hold versus body hold (design audit)

The companion now compares C, N and explicit negative controls. C archives an old
body under its own episode identity before releasing its active row; N with only
existing storage loses a pathless unadopted body. The detailed note fixes the
production entry points, provenance/receipt boundaries, implementation slices and
acceptance tests. No production gate, row clear, fixture or Rust assertion changed.

C frees the old-row guard with Deferred/no source, Refused and NoCandidate. It
keeps NoLegacyBodyOnTui, UnadoptedNotOwned, UnknownOwnerKept, loss/duplicate and
ordering safety under the proposed durable-copy/O-receipt protocol. An optional
source bind is **not fair**; the NoSource control disables it forever. Actual
O-ready admission remains separate from old-row-guard progress.

Historical design decision point (resolved by user decision A below): existing b2 explicitly holds
Refused bodies and its original liveness permits rejection. A recovered verified
store may commit them, but there is no guaranteed body finalizer. Permanent
source-free Deferred also cannot promise body visibility. CRefused exposes the
missing RefusedExplicitlySettled property; CRefusedRepair assumes external
recovery and CRefusedDecision assumes a new per-input visible failure contract.
Neither assumption is silently added to b2. The Released alarm is not an input
receipt or a body disposition. Both C and stored N need that product decision.

Current companion results: **21 cfg, 9 fully GREEN / 12 expected RED**, exact
violation sets, tool errors and skips 0. Before/OuterOnly still fail both row
liveness properties; Main still fails NoLegacyBodyOnTui. Full cfg-by-cfg
before/after table and limitations are in the design note. The 6-cfg figures
above belong to the archived resume-2 model, not this expanded model.


## Historical user decision A and C1 implementation (superseded 2026-11-17)

The user selected C and existing b2 retention for accepted Refused bodies. No new
per-input failure receipt is introduced. N and CRefusedDecision are not selected.
Refused body visibility has liveness only under the fairness assumption that a
verified owner/store eventually recovers (CRefusedRepair GREEN); no unconditional
body-visibility liveness is claimed. Permanent no-source Deferred body retention
has the same conditional recovery assumption. These are known conditional
results; freeing the old row does not claim that O replay has finished. C2 remains
the next slice. The 21 unchanged configs rerun after merging main d40c06eb47
retain 9 GREEN / 12 expected RED with no tool errors, mismatches or skips.

## C2 original-output authority counterexample (2026-11-16)

Pinned main `56fad66678` and C2-preparation head `3ac13c9f4` have identical
`owner_record.rs` and `owner_record/output.rs`. Their original-output effect gate
requires the old admission's generation to remain active. Owner return increments
the generation, and recovering its original artifact does not revive it. The
existing real-PG test
`output_origin_original_admission_survives_owner_return_and_rejects_old_native_pg`
confirms both facts in one scenario (output_tests.rs:254-272).

`WriterAdoptionHerdrCRefusedOwnerReturned.cfg` and
`WriterAdoptionHerdrCNoCandidateOwnerReturned.cfg` add that boolean effect-permit
boundary to C's abstraction. They retain fair verified current-store/custody
recovery, NoLegacyBodyOnTui, UnadoptedNotOwned, UnknownOwnerKept, and the old
execution's row/start progress. The old body remains retained; visibility is
expected RED because OCreate is refused. A scope-valid original remains covered
by the existing GREEN controls. Current custody alone must not be described as
fair recovery of an old debt's output authority.

See the [C2 boundary audit](herdr-body-hold.design.md#c2-boundary-audit-current-custody-does-not-revive-an-old-original-unresolved).
C2 implementation is paused at DESIGN_DELTA; no new permission, sender, receipt
or GC behavior has been added. Decision A and C1's v1 storage/key stay unchanged.


## C2 selected recovery capability (design review pending)

The coordinator selected a separate debt recovery capability, preserving decision
A and refusing the scope reduction to still-valid original admissions. The
[C2 capability section](herdr-body-hold.design.md#c2-debt-recovery-capability)
specifies current custodian/store verification, immutable original provenance,
shared effect identity, run-scoped grants, a committed reservation before HTTP,
one-use guarded admission, receipt consumption, and lifecycle invalidation.
No production code, schema or runtime store is changed in this design leg.

The historical 23 cfg stay 9 GREEN/14 expected RED. The new 25 capability cfg
include Before controls, four recovered-owner liveness controls, UnknownKind
safety, permanently unknown plan/remote-receipt sensitivity controls, and sixteen
guard mutants. The fair recovery condition includes an accessible original
snapshot, a certified plan and conclusive effect evidence; a restored owner
alone is no receipt. Exact unknown Prepared remains pending. Native/unstamped
output does not obtain a recovery grant. The RefusalAccount action is restricted
to its historical unselected B controls; the revoked-origin Before cases do not
select a new per-input failure policy.

Current fields/bounds come from the entry-point table: active owner generation
plus one successor (owner_record.rs:413-439), per-run replacement (host.rs:855),
stop/suspension and exact identity/hash fields. Four bounded adverse events
(transfer, writer restart, stop/clear, proven-no-effect refusal) explore monotone
run revisions 0 through 4. This is a finite exploration bound, not a runtime
retry/TTL/delete rule. The successful POST/receipt path is abstracted separately
from the ReceiptUnproved sensitivity case; the model does not claim a distributed
DB transaction makes Discord HTTP atomic.

Implementation ownership request: twelve added production files, total union
27/+2300 estimate; C2a 12/+900, C2b 11/+650, C2c 14/+750. Each delta stays under
the PLAN +1200 line, with a stop/replan if measurement contradicts the estimate.
Issuance helpers with no operating caller remain cfg(test) until their consuming
slice; no new allow, warning baseline, independent sender or legacy repair.
The source/effect/receipt interfaces must pass independent pro design review
before those implementation slices start.

Final scoped official TLC run: **48 cfg, 14 GREEN /34 expected RED**, zero
unexpected failures/tool errors/skips; 324.6 seconds. Historical23 remain9/14;
new25 are5 GREEN/20 expected RED. All four recovered-body controls (Deferred,
Refused, NoCandidate, already Committed) satisfy row/start/body/receipt/GC progress;
UnknownKind is a safety-only GREEN control. Two new Before controls and the
permanently unproved plan/receipt controls retain their explicit liveness RED.
Sixteen mutants break their named safety guards. Each cfg was actually executed
by scripts/ci/statecharts_tlc.py; the source snapshot and per-cfg invocation counts
are recorded in the lane design report. Modeling plus final fixed-snapshot check
finished within30 minutes from the first capability model edit.

## C2a first-commit model correction (P2-1)

The previous CapCertainNotSent guard used lifecycle mismatch as if evidence had
arrived. It now requires an explicit no-effect witness whose attempt identity
matches the reserved attempt. CapPrepare and the actual HTTP effect CapPost are
separate. Reserved-only/Prepared-before-HTTP crashes without proof remain pending
and send nothing; those liveness controls are expected RED. Proof-supplied
controls proceed, and both proof-check-removal/run-change-reset mutants break
NoResetWithoutProof. Evidence fairness applies only to conclusive evidence that
is supplied, never to a new owner/run or an assumed absence of HTTP. No production
code is included in this first correction commit.

P2 correction: the audited old reset is RED NoResetWithoutProof; final54cfg
are16 GREEN/38 expected RED, 276 actual TLC invocations, mismatch/error/skip0
(rc0,702.8s). The original48 retain14/34. Both no-proof crash cuts are
conditional liveness RED with pending/POST0 safety; proof-supplied reserved and
Prepared cuts are GREEN; removing proof checking or resetting on run change is
RED NoResetWithoutProof. The explicit evidence event replaces, rather than
renames, the lifecycle inference.


## C2b rowless plan boundary

The companion model and all 54 configs remain byte-identical to C2a. C2b implements
validated original plan/coverage inputs, not CapPost/CapConfirm/GC. No fairness
or recovery assumption changes. A fresh TLC run checks the same 16 GREEN and
38 expected RED configurations; expected liveness RED is retained as a condition,
not hidden by removing a property.

| Entry point | Model / concrete boundary |
|---|---|
| `src/services/discord/runtime_store.rs:51` | C1 archive directory appends runtime; no live configuration assumption |
| `src/services/tui_o/writer/herdr_body_debt.rs:24` | Exact C1 v1 path |
| `src/services/tui_o/writer/herdr_body_debt.rs:47` | Original episode/input and immutable snapshot digest |
| `src/services/tui_o/writer/herdr_body_debt.rs:182` | Compatible snapshots; ambiguous or conflicting input stays retained |
| `src/services/tui_o/writer/herdr_body_debt.rs:234` | row=false/copyId=old, no active-row lookup |
| `src/services/tui_o/writer/herdr_body_debt/replay.rs:78` | Pinned source/range plus old Original admission, not a new source stamp |
| `src/services/tui_o/writer/owed.rs:52` | Unique original native coordinates |
| `src/services/tui_o/writer/pieces.rs:56` | Coordinates remain evidence, never an input permit |
| `src/services/cluster/intake_router_hook/owner_record/output/debt_recovery/registry.rs:59` | Shared stable manifest/effect prerequisite; cap/op/oldAck stutter, no grant or phase reset |
| `src/services/tui_o/writer/host.rs:746` | Fresh and Recovered read before unchanged metadata admission |
| `src/services/tui_o/writer/host.rs:871` | Host start/resume rereads retained bytes |
| `src/services/discord/runtime_bootstrap/o_writer_host.rs:409` | Existing runtime PG pool and already-open port bot; no new sender |
| `src/services/discord/runtime_bootstrap/o_writer_host/adoption_wake.rs:23` | Only locally owned debt channels use existing queue wake |

Boot, restart and host resume reread the immutable copy even when the active row
is absent. New source/incarnation alone supplies no coverage or current input
identity. Human, machine and routine inputs retain their original requester,
primary/absorbed message and origin IDs, original intake delivery and policy;
missing fields remain unknown. Cancel/Terminate/clear/stop neither delete debt
nor create grants in C2b; actual session invalidation belongs to C2c. No legacy
turn_bridge/relay_recovery/watcher behavior is changed.

Native aliases require old O admission evidence in addition to exact pinned
bytes/range. A newer source with equal text but a different native unit cannot
borrow that evidence. Snapshot conflicts and unknown evidence stay retained.
Read/merge/registration stutter with respect to cap/op/oldAck; they establish
the existing CapIdentity/CapIssue prerequisites without changing the permission
or receipt state. Registration does not call OCreate/OConfirm or mark a body delivered. The C1
archive remains unchanged; the C2a issuer/strict-receipt harnesses stay test-only.

Refused and permanent no-source Deferred body visibility is conditional on
verified owner/store recovery and eventual exact effect or conclusive no-effect
proof. There is no unconditional liveness claim. C2c remains required for actual
replay, old-before-new, receipt reconciliation and GC.

### C2b reader compatibility repair

C1's production rows omit host_locator. The reader accepts their original logical
session/nonce/episode without creating a current owner locator. Unreadable files
remain retained and isolate only their own episode; other episode order is
unchanged. Native overlap compares the ordered saved/native split payload arrays,
then retains the existing pinned range and original admission checks.

The 54 companion configurations do not encode JSON locator fields or splitter
payload transformations. This repair changes no model/config or protocol
transition; runtime entry tests, rather than the earlier 16G/38R result, verify
the compatibility change. Periodic/foreign-executor debt wake filtering remains
an explicit C2c limitation.

## Configured member / late child composition

`WriterAdoptionThreadComposition.tla` extends the actual main
`WriterThreadActive` transition system with an independent configured member's
b2 Deferred/Refused/Committed gate. `ConfiguredLegacyZero` checks the configured
member (including main's bound/non-child case); `LateTurnLegacyOnce`,
`ExactlyOnce`, `OwnAuthority` and `ReadyNeedsInit` retain main's own handoff rules.
`MemberUnadoptedNotOwned` prevents a permit from substituting for adoption.
Both member and child progress properties run in the same state space.

Before checks the formerly universal prohibition against main's late child and
is expected RED. After scopes that prohibition to configured members while
preserving the exact late-child exception. Leak deliberately applies the child
exception to a Deferred/Refused configured member and must be RED.

Entry points at main `5c144746da`: `channel_policy/children.rs:38-44` derives the
child body owner; `cutover/channel_gate.rs:469-471,500-521` keeps the reserved
turn on its own counter; `cutover/thread_child_record_tests.rs:134-180` checks
Legacy once before commit and OwnedByO afterward. The merged b2 member path
still calls `adopted` before answering owned. Placement records the checked
child relationship but does not change the member's adoption.

| Composition config | Before/after result | TLC runs |
|---|---|---:|
| WriterAdoptionThreadBefore | Expected RED: NoLegacyBodyOnTui | 5 |
| WriterAdoptionThreadAfter | GREEN, MemberEventuallyAnswered / ChildInputAnswered / HeldResolves included | 4 |
| WriterAdoptionThreadLeak | Expected RED: ConfiguredLegacyZero / NoLegacyBodyOnTui | 6 |

Plain TLC via `scripts/ci/statecharts_tlc.py`, 2026-11-17: 3 selected / 3
matched, 15 actual fairness/TLC invocations, mismatch/tool error/skip 0, 55.3 s.
The composition checks the adoption boundary only. Archived debt replay and
Legacy/debt evidence reconciliation remain in the C2c acceptance scope.

### Refused child repair refinement

The child recovery component now records Refused and Released separately, the
verified destination/parent/runtime store result, absent local authority and a
body observation before and after recovery. These values come from
`channel_policy/children/tests.rs`'s real `start_managed` first-init/recovery
scenario and the Recovered::Store branch in `writer/host.rs`. A different
parent/kind store is not verified recovery. No local authority starts no actor.

| Config | Before / repaired result | TLC runs |
|---|---|---:|
| ThreadReleasedRecovery | Expected RED: VerifiedRecoveryEventuallyCommits | 5 |
| ThreadUncheckedRecovery | Expected RED: RecoveryVerifiedOnly | 6 |
| ThreadLeakingRecovery | Expected RED: RecoveryNoLegacy | 6 |
| ThreadVerifiedRecovery | GREEN: verified repair liveness, verified-only commit, Legacy 0 | 5 |

All seven composition configs were run with the official TLC runner: 2 GREEN,
5 expected RED, 37 actual TLC invocations, 646.5 seconds; expectation mismatch,
tool error and skip 0. Logs and exact model/config hashes are in the lane report.
The original failed-init test changes only Released to Refused; its subsequent
ChildHeld and visible-halt assertions execute. The new entry-point test first
failed because a different policy parent was accepted, then passed after the
verified-surface recovery guard. It also checks another runtime kind, missing
authority and successful repair in the same process, with Legacy body 0.

### One-shot authorized debt wake

The C2b periodic-wake limitation above is repaired at the actual bootstrap
completion boundary. `discord/runtime_bootstrap.rs` claims the first reconcile
completion on SharedData before scanning debt. Standby registration takes the
same one-shot boundary. `o_writer_host.rs` no longer scans from the execution
writer refresh path. Ordinary queue admission and its existing wake are unchanged.

`o_writer_host/adoption_wake.rs` checks a managed channel's local configured
executor, current permit gate and matching active PG owner/generation; an
unverified/stale/foreign execution view cannot borrow the provider gateway gate.
For an unmanaged channel, this SharedData must be the unique registered provider
gateway runtime and its gateway gate must be Owned. The scan only selects valid
C1 snapshot channels already owned by O; it grants no POST permission.

WriterAdoptionBootWake uses precisely these two authority cases (managed and
unmanaged), four caller cases (gateway, local execution, foreign execution,
stale execution), debt versus ordinary channel and bootstrap versus repeated
refresh. The bounded repeat count exposes duplicate wake; it is not a production
timeout/retry limit. Ordinary enqueue has its own fair action.

| Config | Before / repaired result | TLC runs |
|---|---|---:|
| BootWakeBefore | Expected RED: WakeAuthority, WakeOnce | 5 |
| BootWakeAfter | GREEN: WakeAuthority, WakeOnce, OnlyDebt; owned wake and ordinary enqueue liveness | 3 |

The official runner matched 2/2 configurations, 8 actual invocations, 5.9 seconds,
with mismatch/tool error/skip 0. The earlier exploratory run found a terminal
model deadlock; the final model explicitly stutters once bounded observations
are complete. It is not counted as the production Before counterexample.
The actual bootstrap/refresh test observes one queue wake on the gateway,
none on the same-provider worker, no repeated scan/wake, and an independent
ordinary queue wake. The PG test checks local, foreign, unverified and stale
execution views against a real C1 archive and owner table.
