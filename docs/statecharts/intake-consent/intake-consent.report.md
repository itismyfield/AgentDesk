# Intake consent authority — r2 PLAN

Production code SHA: `08e2336e47986d70d0cbe95e56215858377aed5c` (unchanged).
Reviewed d1 design SHA: `4bf484ed8363bdbdc7734e21013bd157ffc43bd6`.
Precedence fix commit: `e8f13ac6a744b27bf85da308c8be5ea40766904c`.

Model investigation/refinement: 2026-11-11 05:10–05:33 UTC (23 minutes).

`SC_RUN=mac-needed`. No new SANY/TLC runs, state counts, GREEN or RED observations
are claimed on WSL. The coordinator reported the corrected old model had 4,372
states in Current and Proposed; those were identical systems and do not verify r2.
The chart is hand-authored. expected.tsv contains predicted outcomes, not results.

## Review fixes

- P1: remove CAS receipt and public proof conversion. The CAS stays bool. Only the
  existing `o_input` can construct a private-field, nonclone WorkerScopeKey, consumed
  by a keyed scope service child of the opaque capability module. That API returns
  the original future's result, never a proof/receipt/key. The existing destructive
  take/request filter remains the only proof exit. See [exact ownership, visibility
  and same-crate API-composition probes](design.md).
- P2: Boundary now guards distinct actions, not just ASSUME. Current has direct private
  issue; d1 retains public receipts and converts outside scope; r2 has private key
  construction and keyed scope entry. Proofless effects and post-effects rollback
  are explicit, as are process epochs and repeated claims of the same row.
- Model line 159: first committed the coordinator's precedence fix separately before
  replacing the model. Every IF/CASE used as a record value is parenthesized in r2.

## Systems, expected counts and actions

| Config | Boundary-specific actions | State count | Expected result |
|---|---|---|---|
| Current | CurrentPrivateIssue; RejectOutsideConversion | unmeasured, Mac needed | GREEN |
| D1Public | D1RetainReceipt; D1EnterScope; D1ConvertOutsideScope | unmeasured, Mac needed | RED: NoUnauthorizedIssue, NoEffectsThenRollback |
| Proposed (r2) | R2PrivateScopeKey; R2EnterKeyedScope; RejectOutsideConversion | unmeasured, Mac needed | GREEN |

For a sibling's spawned CAS success, Current/r2 return no convertible value. d1 enables
RetainReceipt and later ConvertOutsideScope. For a worker at spawned, Current enters
scope directly; r2 first constructs the private key then consumes it to enter. These
are syntactically and operationally different Next branches. A distinct enabled-action
witness is not a measured TLC state count or a refinement/equivalence theorem.
The extra r2 step abstracts the private key argument construction at the old scope call;
it does not publish a new authority value or move proof issuance ahead of that call.

## Rules and cfg predictions

All cfgs keep the prior six invariants and two liveness properties. The seventh
invariant NoEffectsThenRollback rejects rollback commit after any effects for the
same original input, and rejects a second execution across its repeated claims.
Consumption means linear move/drop, not a guarantee of one successful SQL mutation.

| Config | Expected violated invariants | Expected liveness violations |
|---|---|---|
| IntakeConsentCurrent.cfg | none (GREEN) | none |
| IntakeConsentD1Public.cfg | NoUnauthorizedIssue, NoEffectsThenRollback (RED) | none |
| IntakeConsentProposed.cfg | none (GREEN) | none |
| IntakeConsentForge.cfg | NoUnauthorizedIssue (RED) | none |
| IntakeConsentReuse.cfg | ProofConsumedOnce (RED) | none |
| IntakeConsentStale.cfg | NoStaleProofWrite (RED) | none |
| IntakeConsentBookkeeping.cfg | RollbackExactAndBookkeeping (RED) | none |
| IntakeConsentOwnerOnly.cfg | NoStaleProofWrite (RED) | none |
| IntakeConsentScopeOpen.cfg | NoUnauthorizedIssue, NoEffectsThenRollback (RED) | none |

ProofEventuallySettles and TransactionEventuallyEnds use weak fairness of each
instance's Advance and release. No fairness for successful claim or recovery from
permanent infrastructure failure is asserted. CHECK_DEADLOCK FALSE is not a progress
proof. The cfg parser's acceptance does not mean TLA expressions are well-typed.

## Predicted P1 witness and code replay

D1Public: Claim(1,sibling) → accepted CAS → spawned CAS returning permit →
D1RetainReceipt(1) → EnterExecutorWithoutProof(1) → ExecutorEffectsWithoutProof(1) →
D1ConvertOutsideScope(1) → RollbackMove(1) → LockOwner(1) → ReadForUpdate(1) →
PrepareCAS(1) → Commit(1). The row is exact, spawned, live and unmarked, so d1 rollback
can make it pending after effects; claim 2 can execute the same input again.

This is the review-confirmed *d1 design* exploit, not a production Current bug or an
executed trace. Current `o_input.rs:75,107–118` has no public receipt conversion;
`worker_entry.rs:58` consumes its scoped proof before effects; normal admission drops
it. `consent_handoff.rs:208–226` cannot infer past effects from the unchanged row.
R2 blocks the exploit at acquisition of authority, not with a new DB effects ledger.
Its sibling cannot construct WorkerScopeKey or call a naked proof factory. ScopeOpen
predicts the same late-proof exploit after widening that key constructor. The negative
compile probe must include the full API composition, not merely handoff(permit).

## FS observation gap (preserved)

ObserveAndMove may read TRUE; ChangeConsent may then set the file fact FALSE; hold
CAS and commit can still succeed on stored observed=TRUE. Both Current and r2 permit
this existing gap because the actual order is load/validate → begin → owner lock →
row lock → CAS → commit (`consent_handoff.rs:255–312`). GREEN here never means the
consent file is still latest at commit. No new FS/PG lock or extra observation closes
that gap. A stronger freshness requirement needs separate design.

## Production owner coupling

The prior C-owner candidate remains blocked: production `owner_record/custody.rs`
:190–192 retires old open rows, then :201–203 transfers owner in the same transaction.
:323–324 and :383–392 change old spawned rows to unknown; the no-receipt path is
`db/replay_disposition/write.rs:217–228`. replay_only=true rows are unknown by
`0136_request_replay_disposition.sql:15–25` and `write/start.rs:151–158`. Thus old hold's
status='spawned' CAS fails after production transfer. OwnerOnly removes that existing
coupling. No additional live-owner hold query or cleanup authority is introduced.

## Bounds, restart and exclusions

- MaxClaims=2 gives fresh authority instances (process epoch, serial) on one durable
  row, including rollback→restart→claim. Claim tokens can repeat within a process;
  the serial is a ghost authority identity, not a stronger DB claim predicate.
  One process loss clears every key/receipt and
  unspent proof; claimed-only stale reclaim may return an old epoch's pre-accept row
  to pending. Spawned rows are not automatically rearmed on restart.
- Row payload/source/stamp/marker drift is bounded to one event. Existing transaction
  locks, marker once rules, and coupled retirement are explicit; complete routing,
  replay/operator child transactions and FS publication internals remain outside.
- Legacy null/half stamps and exact wire bytes need Rust/PG controls; the model uses
  a stamped row. Cipher/hash collisions, clock rollback and permanent outages are out.
- Arbitrary repeated direct executor calls and raw SQL/unsafe Rust fabrication are not
  a security model. The property covers authority-enabled rollback/reclaim replay;
  it does not add a durable exactly-once execution ledger.
- Cancel/Terminate/clear/stop/legacy return use their existing distinct authority and
  remain unchanged; all entry points and visibility are mapped in design.md.

## Mac handoff

Run all nine configs and record actual state counts, action traces, invariant sets,
PROPERTY results and timings. Compare the three core systems explicitly. Validate
expected.tsv predictions from real results. Use the existing statechart tool and
repository checker; preserve the 300-second/config and 15-minute/job budgets. No
new model execution is claimed by this document.
