# Herdr execution retirement and retained body debt

<!-- doc-path-refs: proposed herdr_body_debt/receipts.rs -->

Design audit on `13355ee75c74ef65b2c5564f0a95efb37f988d26`, containing main
`ad04f6f3ecc33e86e7dc0968822ce2ce3f087e90`. This note and its model change no
production behavior. The outer-only ownership mapping is withdrawn. C is selected by user decision A (2026-11-16). Accepted Refused bodies remain
retained until a verified owner/store recovers. C1 implements storage and execution
retirement; C2 must implement O replay/receipts. No per-input failure receipt is added.

## Current decision and D1 boundary (2026-11-17)

The user superseded decision A: normalize the original execution first; when that fails or expires, emit its body-free reason and discard the rejected episode. Retention for future owner/store replay is no longer the target policy. The earlier decision-A/C1/C2 sections below record existing behavior and compatibility obligations, not permission to retain newly rejected bodies.

D1a is the fresh-policy/receipt/atomic-adoption foundation: typed disposition, one total normalization deadline, constrained trust/binding repair and metadata notice receipts sit behind the `channel_policy/adoption.rs` test-only installation boundary. Its execution evidence is reported per head. Continuation disposition/proof/turn-window normalization is an unverified D1b follow-up, excluded from D1a completion. Existing production fresh, warm and resume contracts remain unchanged until D2 connects consumers and stores. D1a makes no claim that all storage paths retain zero rejected bodies. Original, DebtPrepared and DebtReceipt readers remain required to reconcile historical effects without promoting them to a new admission.

D2 must include independent `terminal_outcome_delivery/foreign_terminal_handoff.rs` custody: stop new rejected snapshots before `preserve_or_publish`, identify existing custody by its original episode, bind it to durable body-free reason intent, and retire it before checking HTTP availability. Scrub full_response, local prompt/body, admitted.result and delivery_body. Pending reason receipt cannot retain a raw snapshot or extend `retains_channel()`. Custody cleanup completion is not body Delivered/O Posted/input success; successor rows remain untouched.

## Historical Refused behavior at the audit baseline

Plan r2 section 10 requires visible rejection **before acceptance** for an invalid
O setup, preservation of accepted input/receipts while O recovers, and no Legacy
body fallback. It does not specify a terminal disposition for an already accepted
body whose adoption is refused permanently. The original writer-adoption model's
`HeldTurnAnswered` and `AdoptionSettles` both exempt `rejected`; a refusal is not
proof that the body reached its user.

| Existing boundary | Code and consequence on the audit SHA |
|---|---|
| Refuse pending adoption | `src/services/tui_o/channel_policy/adoption.rs:341`: Refused for the rest of the process, no store written, body held |
| Common body gate | `src/services/tui_o/cutover/channel_gate.rs:445`: Pending/Deferred gives AwaitingAdoption; `:446`: Refused/Released gives AdoptionRefused |
| Recovery disposition | `src/services/discord/recovery_paths/shared.rs:183`: both adoption errors preserve the row and spend no restart budget; the same applies to tmux TUI bodies |
| Host release | `src/services/tui_o/writer/host.rs:662`: releases adoption and raises Released; it does not settle a turn's body |
| Possible recovery | `src/services/tui_o/channel_policy/adoption.rs:369`: a verified recovered store can commit even Refused, but Released stays released. Boot classification at `:449` needs an actual valid store/config; a restart alone is no delivery proof |
| Alarm | `src/services/tui_o/alarm.rs:261` needs optional alert routing and a notifier; `:302` can return without sending; `:339` sends incident metadata, not a per-input body disposition. It is not an answer receipt |

Thus there is no existing actor or deadline that **guarantees** settling a Refused
body. An operator may restore a valid owner/store and then O can recover it.
Without that external event, tmux TUI has the same potentially permanent body
hold. Neither a restart nor the Released alarm can be counted as body settlement.
This note does not repair the legacy paths.

## Entry values and the cycle

These are the inputs of the main+b2 production-entry audit, not assumptions about
live deployment. No operational configuration or store was read.

| Model value/action | Production entry or proposed boundary |
|---|---|
| Deferred, no source | `channel_policy.rs:345`, `channel_policy/adoption.rs:181`; `writer/late_adopt.rs:221,250` and `late_adopt/codex.rs:260` cannot run deferred retry without a source |
| Refused | `channel_policy/adoption.rs:343`; body remains held as above |
| NoCandidate | `channel_policy.rs:320`, `cutover/channel_gate.rs:509`; foreign selected channel without a local execution/candidate |
| Committed / UnknownKind | `writer/activation.rs:243` / `cutover/channel_gate.rs:487`; a genuine identity-read error is distinct from both adoption errors |
| Prior-process row; actor absent | `discord/recovery_engine/herdr_row_end.rs:52,331`; canonical record/nonce check at `:503` |
| Observe / Commit | `herdr_row_end.rs:238,254`; the existing commit retires the execution/input hold, not the inflight row |
| Notice body present or absent | `herdr_row_end/outcome.rs:57-72`: full response, saved partial or empty marker. A row-end notice can be an answer body |
| Inner body gate | `herdr_row_end.rs:310` calls `completion_delivery.rs:392`; `:411-416` holds on adoption errors before the body-free check at `:419` |
| Row / gateway guard | clear at `herdr_row_end.rs:269`, closed event at `:279` do not run after notice failure. `herdr_row_end_settle_tests.rs:347-385` demonstrates gateway and REST start hold |
| Restart | `restore_inflight.rs:212` preserves a held row. The model permits one crash at any cut and retains durable copy/effect/ACK state |
| O ownership / readiness | `herdr_launch.rs:75`, `turn_host.rs:308`: passing an old row guard is **not** permission to bypass OWriterNotReady or launch a runtime |
| Proposed archive identity | full old row/nonce/generation/user-message provenance, anchored in existing `inflight/clear_store/herdr_row_end.rs:32-46` identity guards; separate per-episode storage is new work |
| O effect / uncertain confirmation | `discord/outbound/o_writer_io.rs:100,129,158`: reuse O owner scope, original provenance and history/receipt confirmation; the model abstracts these as OCreate/OConfirm |

The current outer gate stops before commit. OuterOnly passes that gate, retires
the execution, then stops at the inner body gate. In either case the row keeps
the new input queued. A source needed by adoption can depend on a later start.

`NewStartEventuallyPassesRow` covers this **old row** obstruction only. Actual
runtime admission still needs O readiness. Optional source binding is not fair
in the revised model: it may never occur. The NoSource control disables it
entirely. C can free the row without adoption; it cannot promise an actual new
Herdr launch or delivery without a valid O owner.

## Candidate flows

| Option | Flow | Cost and risk |
|---|---|---|
| C | Verify the old execution ended; fsync a body-debt record keyed by the old episode; guarded removal of that active row; release gateway/input hold. After adoption, O settles the old body and records its receipt before GC | Requires a new durable debt carrier and an O replay/receipt adapter. Keeps old and new output identities separate; body visibility remains conditional on verified recovery under decision A |
| N with existing storage | Send a body-free interruption notice, clear/close the row, rely on existing storage for the answer | Rejected: no generally available durable body copy for pathless unadopted rows. NoBodyLoss and BodyAfterAdoption are RED |
| N with new storage | Archive as C, then send Neutral, clear/close and replay through O later | At least C's cost plus another notice effect to reconcile after crashes. Safe storage alone does not resolve Refused |
| Existing STALLED/retry link | Make `writer/deferred.rs:385-455` retry run with no source | Not a sufficient small fix: late-adopt only calls it with a bound source; the retry needs source/snapshot/pin. Empty initialization does not account for an old saved response or Refused disposition |

The existing `completed_turn_ledger/o_pending.rs` is not C's debt carrier:
`:110-140` captures only after an O-owned peek, `:81-109` requires native manifest
paths/ranges, `:59-65,178,194` permit size/age eviction, and `:235-311` reconcile
checks existing O evidence rather than publishing a pathless saved response.
`terminal_delivery_custody.rs:244-264` offers an atomic-write primitive, but its
`:377-392` retained-episode guard blocks the channel; it is not already a
nonblocking answer archive. `inflight/store.rs:90-95` has one file per
provider/channel: leaving the old row there lets a new turn conflict with or
overwrite it. Circuit handoff (`inflight/circuit_removal.rs:13-42`) preserves
circuit evidence, not the entire answer.

## C storage and output contract (C1 storage, C2 delivery)

1. Under the existing episode/nonce/canonical-owner guard, prove this is the ended
   prior-process execution. Snapshot the full row, saved body/partial, old input
   IDs, runtime/provider, nonce, generation and available native source/range.
   Unknown or changed identity remains held. No timeout creates cleanup authority.
2. Atomically persist and fsync an immutable per-episode debt record and digest,
   independent of the channel's active-row filename. Only a verified durable
   copy or the **old body's** O receipt permits guarded detach. A newly committed
   owner/source is not evidence that it covers the old body.
3. Retire only this execution's input hold, remove only this pinned active row,
   emit closed once under the existing effect guard. A restart between copy and
   removal reconciles the same key; a restart after removal reloads the debt.
   A different/new row or a before-start receipt is never removed.
4. Replay the debt only through O with its original input/episode identity.
   Adoption wakes must scan this separate debt, since the current wake reads the
   active inflight row (`runtime_bootstrap/o_writer_host/adoption_wake.rs:29`).
   Restore the old source/range when certified; otherwise use an explicit saved
   snapshot provenance. Never relabel the body as the new turn's native output.
5. A later native transcript and the saved partial must share one reconciliation
   plan. Do not post both the saved text and its native replay. Ambiguous overlap
   stays durable with an explicit unresolved status until a certified O plan is
   possible; this is an implementation acceptance boundary, not an assumption
   that all sources can be reconstructed.
6. Keep old debt until O's durable delivery proof. On an uncertain POST, reconcile
   the same effect identity/history before retry; no blind second POST. New-turn
   output cannot overtake unresolved old debt, although its execution/row guard
   may progress. Under decision A, Refused does not end that obligation; only proven delivery
   or an existing explicit no-visible-result policy can do so.

The model assumes truthful durable effect/receipt reconciliation and an O adapter
capable of representing old debt. It checks protocol obligations, not a proof that
today's transport or storage already implements them. The foreign NoCandidate
control uses the brief's explicit assumption that home returns with valid O
authority **and access to this old debt**. It does not grant a foreign host new
authority or assume node-local files move automatically.

## User decision A and conditional body visibility

C is selected. N is rejected and CRefusedDecision remains a historical, unselected
counterfactual. The user chose the first policy below; the second is not implemented:

- **Keep b2's current hold:** archive preserves the body; external verified
  owner/store recovery may later allow O settlement. Execution/row progress is
  GREEN, but permanent Refused has no explicit final disposition and old-body
  output can remain invisible forever. CRefused is deliberately liveness RED.
- **Define a visible terminal failure per accepted input:** retain the answer,
  durably record a user-visible failure receipt and explicit disposition, and
  unblock later output after that receipt. This is CRefusedDecision, a hypothetical
  new product contract. An optional ops alarm is insufficient; later recovery
  must still avoid duplicating any already delivered body. Recipient, wording,
  retry/recovery semantics and availability must be approved before production.

A permanently source-free Deferred channel also keeps an invisible body in the
NoSource control. Separating the row guard does not create O readiness. C2 must demonstrate valid source-free O initialization when available and an
old-body replay adapter. Otherwise the body remains retained until verified
owner/store recovery under decision A. There is no timeout-based discard, forced
adoption or new visible failure policy. Existing fresh owner
initialization can use empty sources (`writer/host/first_init.rs:282-324`), but
requires fenced authority and Free custody; connecting it to old-body debt is
part of C2, not an already proven recovery service.

CRefusedRepair is a separate conditional control: a **fair external verified
store recovery** is assumed. Its GREEN result is not evidence that current b2
will recover permanently Refused channels. The silent-refusal mutant is RED.

## Model checks and before/after comparison

`WriterAdoptionHerdrRowEnd.tla` keeps NoLegacyBodyOnTui, UnadoptedNotOwned and
UnknownOwnerKept. NoLegacyBodyOnTui has main's narrow late-child exception: a
parent-unrecorded late-reserved child finishes its pre-init open turn exactly
once through Legacy, under its own candidate/send counter and checked-parent
init handoff; configured members and all other nonmember TUI/Deferred/Refused
paths retain the strict gate. The configured/member and child handoff rules are
checked together by WriterAdoptionThreadComposition. Converting this exception
to O processing remains for the later complete Legacy removal stage of #7651.
New invariants are NoBodyLoss, BodyExactlyOnce,
BodyIdentityKept, OldBodyBeforeNew and NoSilentRefusal; ClearHasEvidence also
requires a retained old copy/receipt unless existing owned/notice evidence applies.
BodyAfterAdoption is conditional on adoption; it is vacuous in the permanently
no-source Deferred control. RefusedExplicitlySettled exposes the separate gap.
Row-end, archive and delivery actions are weakly fair; Bind and Restart are not.
One optional crash bounds the state space; arbitrary crash counts and two-host
storage races remain implementation tests.

| CFG suffix (all `WriterAdoptionHerdr*.cfg`) | Before / proposed after | Expected broken rules |
|---|---|---|
| Before | unchanged b2, three held owners | row settlement + new start row-guard liveness |
| OuterOnly | withdrawn outer mapping | same two liveness properties |
| Bound | old path with a bound source | none |
| Owned | old path already O-owned | none |
| Unknown | old path genuine identity error | none; preserve row, no progress claim |
| Main | old false/Legacy control | NoLegacyBodyOnTui |
| CDeferred | C, source optional and may never bind | none |
| CDeferredNoSource | C, binding disabled forever | none; body retained, no unconditional delivery claim |
| CRefused | C, existing b2 refusal | RefusedExplicitlySettled only; both row liveness rules GREEN |
| CNoCandidate | C, eventual home return assumption | none |
| CUnknown | C, genuine identity error | none; row/holds retained |
| CRefusedRepair | C plus hypothetical external recovery fairness | none, conditional |
| CRefusedDecision | C plus hypothetical visible per-input refusal disposition | none, requires user decision |
| NExisting | N, no new archive | NoBodyLoss, BodyAfterAdoption, RefusedExplicitlySettled |
| NStored | N plus C's new archive | RefusedExplicitlySettled only |
| CNoDurable | detach before copy | ClearHasEvidence, NoBodyLoss, BodyAfterAdoption |
| CDuplicate | blind repeated O POST | BodyExactlyOnce |
| COverwrite | new turn overwrites old debt | BodyIdentityKept, ClearHasEvidence, NoBodyLoss, BodyAfterAdoption |
| COwnedShortcut | new O ownership skips archiving old body | NoBodyLoss, BodyAfterAdoption |
| CEarlyOutput | allow new output past unresolved old debt | OldBodyBeforeNew |
| CSilentRefusal | call refusal settled with no visible per-input receipt | NoSilentRefusal, RefusedExplicitlySettled |

Run evidence, exact cfg selection, model/config hashes and raw counterexamples
are recorded in the lane report. Expected RED controls are not accepted fixes.

## Proposed implementation slices, after the decision

Each slice must be measured independently against prod 30 files / +1500 and the
+1200 planning stop line. These are estimates, not permission to expand legacy
recovery. A reviewed slice must include an operational caller, not dormant code.

| Slice | Proposed production files | Estimate |
|---|---|---|
| C1: durable debt and execution retirement | `discord/recovery_engine/herdr_row_end.rs`, `herdr_row_end/outcome.rs`, `discord/inflight/clear_store/herdr_row_end.rs`, `discord/inflight.rs`, new `discord/inflight/herdr_body_debt.rs`, new `herdr_body_debt/store.rs` and `herdr_body_debt/reconcile.rs`, `discord/recovery_engine/restore_inflight.rs` | 8 files / +650 |
| C2: O replay, receipt and ordering | `discord/runtime_bootstrap/o_writer_host.rs`, its `adoption_wake.rs`, `tui_o/writer/mod.rs`, `tui_o/writer/host/first_init.rs`, `writer/host/activation_io.rs`, new `tui_o/writer/herdr_body_debt.rs` with new `herdr_body_debt/replay.rs` and `herdr_body_debt/receipts.rs`, `tui_o/writer/confirm.rs`, `tui_o/store/delivery_evidence.rs`, `tui_o/store/ledger.rs`, `discord/outbound/o_writer_io.rs` | 12 files / +850; approved original-provenance adapter and refusal policy required |

C1 alone preserves body debt and proves row-guard progress; it does not meet the
full answer-delivery acceptance criteria and cannot be landed as the completed
repair. If integrating an operational debt reader pushes either slice toward
+1200, split and report PLAN_READY before implementation. New file ownership and
exact O integration paths must be approved in the implementation brief.

Acceptance scenarios (new names are proposed, not registered tests):

- `a_restarted_unadopted_herdr_row_archives_body_and_releases_only_its_gateway_pg`:
  old ended row blocks gateway on current b2 (RED); verified archive then pinned
  detach permits gateway/REST input past the old guard (GREEN). Repeat Refused
  and no-local-candidate variants; do not bypass O readiness.
- `an_unknown_herdr_owner_keeps_row_body_and_holds_pg`: a genuine UnknownKind and
  canonical mismatch remain held, with no newly created cleanup/kill authority.
- `a_crash_at_each_herdr_debt_handoff_cut_keeps_one_old_body_pg`: before copy,
  after file/dir fsync, before/after guarded removal, before closed and after a
  new row appears; restore is idempotent and never removes a successor row.
- `a_late_o_adoption_settles_the_archived_old_episode_once_pg`: old input IDs and
  nonce survive; a saved partial followed by native binding produces one O answer,
  not two overlapping bodies or a new-turn relabel. Unknown native overlap stays
  preserved rather than being guessed away.
- `an_uncertain_old_body_post_reconciles_before_retry_or_gc_pg`: POST accepted but
  receipt lost, restart/retry/history failure; no duplicate, early GC or new-turn
  output overtaking unresolved debt. Repeat after native-path rotation.
- `a_new_o_owner_without_old_body_evidence_cannot_drop_the_debt_pg`: ownership
  commits before archive; no silent clear based only on Ok(true).
- Refused acceptance depends on the user decision: verified external recovery
  test is conditional; a new per-input visible terminal receipt needs its own
  unavailable-send/restart/dedup tests and must never count an ops alarm as delivery.

Keep `a_pending_adoption_holds_every_recovery_notice_until_o_adopts` and
`a_held_recovery_body_spends_no_restart_budget_and_keeps_its_row` unchanged: the
generic body helper still holds and spends no restart budget. Keep
`a_restarted_herdr_row_holds_gateway_and_rest_starts_pg`: an unsettled/invalid row
still blocks; add the archive-qualified release case rather than weakening it.
After this design is approved and implemented, align only the peek assertion in
`a_late_reserved_channel_is_not_o_owned_for_a_herdr_turn` to AwaitingAdoption;
retain its name, fixture and `!o_ready_at` assertion. This design commit does not
change that failing assertion.

Mutants: omit archive/fsync; delete or overwrite old debt on new start; clear on
new ownership without old-body evidence; blind duplicate O post after uncertain
receipt; release ordering before old-body proof; treat an alarm/silent refusal as
settlement; map UnknownKind to not-owned; restore the withdrawn outer-only mapping.
Each must produce its corresponding safety/liveness RED before claiming GREEN.


Refused body visibility has liveness only under the fairness assumption that a
verified owner/store eventually recovers (CRefusedRepair GREEN); no unconditional
body-visibility liveness is claimed. Permanently no-source Deferred bodies are
retained under the same conditional recovery contract. CDeferredNoSource proves
old-row guard release and retention, not eventual body visibility.


## C1 durable debt format and model refinement

C1 writes `runtime/discord_herdr_body_debt/<provider>/<channel>-<episode>-<snapshot>.json`.
`episode` is SHA256 of the compact serde_json array `[provider, channel_id,
request_owner_user_id, user_msg_id, turn_nonce, born_generation, started_at]`.
`snapshot` is SHA256 of the exact UTF-8 record file bytes. Existing snapshots are
immutable: a later row/snapshot has a separate filename. No expiry, eviction,
body-post, completion receipt or garbage collection is implemented in C1.

Version 1 fields:

| Field | Meaning |
|---|---|
| version | integer 1; unknown versions remain on disk and do not authorize a clear |
| episode_key | the old birth-identity digest above, independent of any new turn |
| provider / channel_id | must match the parsed raw row and directory/filename |
| row_json | complete, byte-preserved original inflight JSON, including unknown fields and saved partial/input/receipt/provenance |
| evidence | execution_nonce; idle/end observation; optional source session_id/path/dev/ino; outcome |
| evidence.outcome | kind terminal/no_terminal/mixed/unknown; terminal includes original result, ordered text/tool parts, terminal_kind, start/end; mixed includes after_prompt |
| recovery_notice | rendered snapshot of the old recovery notice; raw text is also retained in row_json and terminal evidence |
| disposition | always retained; not a delivery or completion assertion |

A source in evidence is a binding snapshot. An Unknown outcome does not certify
its native range. C2 must reconcile saved partial and any later native range
under the original episode/input identity, check O delivery evidence and avoid a
second post if O already delivered. It must coalesce multiple snapshots by the
stable episode key and may collect them only after the appropriate durable O
proof. C1 also archives an O-owned row: current ownership alone does not prove
that a newly bound source retained this old answer.

| Model action | C1 production entry / refinement |
|---|---|
| Observe | settle_row retains actor/canonical execution/nonce/end/idle and routing guards; only Ok(true) or the two adoption errors enter the archive branch; other identity errors remain held |
| Archive | archive_restarted_herdr_body locks and rechecks the exact active row; store::publish uses existing durable publication but accepts only Flushed, including parent/ancestor directories |
| Detach | commit retains the existing retire CAS and hold release; clear_restarted_herdr_body locks again, verifies the same raw snapshot and adoption/error class, preserves circuit evidence, removes only that row and syncs its directory |
| Start | existing closed/queue kickoff after clear; O-ready admission remains separate |
| Restart | reconcile validates and re-syncs retained files without recreating active rows or marking bodies delivered; a missing active row can reissue the idempotent queue wake lost between clear and closed |
| OCreate/OConfirm | C2 only; C1 has no O replay, new Legacy send or body receipt |

The active-row clear's existing generation, nonce, episode pin, receipt,
hot-swap/rebind/replay guards remain. The body copy is verified under that row's
lock; a successor or a newer raw row cannot use the old snapshot witness.
A closing wake is a queue retry, not permission to clear a successor or launch
without readiness. Reconcile skips a present/unreadable active row.

The 21 CFG files are unchanged in C1. Before and after merging main d40c06eb47:
9 GREEN / 12 expected RED, including the deliberately conditional Refused results.
CRefusedDecision remains an unselected historical comparison, not an implemented
policy or a claimed unconditional recovery service.

## C2 boundary audit: current custody does not revive an old original (unresolved)

The C2 entry-point audit found a narrower authority boundary than C's abstract
OCreate assumption. This is present unchanged in pinned main `56fad66678` and
C1+C2-preparation head `3ac13c9f4`; it is not a b2 gate regression.

| Entry point | Actual authority after owner return |
|---|---|
| `cluster/intake_router_hook/owner_record.rs:411-439` | Transfer supersedes the active original generation and inserts generation + 1. |
| `owner_record/output.rs:174-201` | `source_artifact` can still recover the exact immutable old admission; it does not grant it fresh authority. |
| `owner_record/output.rs:98-112` | `Scope::acquire` requires that old delivery's executor/generation to join the **active** owner, as well as its original receipt/nonce/incarnation/binding. A returned home owner/store cannot satisfy an old superseded generation. |
| `discord/outbound/o_writer_io.rs:129-155` | O's actual POST calls that same acquire; an old artifact is Withheld before HTTP. |
| `owner_record/output_tests.rs:254-272` | The real PG control restores the unchanged old artifact after transfer and expressly asserts that acquire is refused. |

Thus a C1 body can be durable and its row retired, with current O custody/store
verified at home, but with no admissible OCreate through the existing original
output port. In the new counter configs, `OldOriginalAdmitted = FALSE` is exactly
that SQL `valid` result. RefusedRepair and NoCandidate HomeReturns are still fair;
body visibility nevertheless fails. Row settlement and passing the old row guard
remain live. The original 21 configurations retain their historical results.

| Counter cfg | Historical abstraction | Actual original-output boundary |
|---|---|---|
| `CRefusedOwnerReturned` | CRefusedRepair GREEN assumes recovery also makes OCreate admissible | Expected RED BodyAfterAdoption and RefusedExplicitlySettled |
| `CNoCandidateOwnerReturned` | CNoCandidate GREEN assumes current home O authority can replay old debt | Expected RED BodyAfterAdoption |

Decision A is unchanged: retain accepted Refused and permanent no-source Deferred
bodies; no input-specific failure receipt. The fair recovery assumption must now
state whether it includes **a separately verified authority to replay this exact
retained debt**, rather than just current owner/store availability. The latter
alone is insufficient. Neither rewriting an old artifact to the new owner nor
loosening ordinary source metadata/owner checks is an acceptable implementation.

A possible next design is a distinct recovery capability: keep original
provenance immutable, bind the immutable debt digest and original episode/input
to the current custodian, and authorize only its receipted recovery effect under
the existing gate/lease/stop protocol. Its shared identity/receipt semantics and
revocation boundary require design review before implementation; this audit does
not grant it or select a new product policy. Restricting C2 to still-valid original
admissions would instead leave owner-return cases permanently retained and would
need an explicit scope/liveness decision. No production code is changed here.

## C2 debt recovery capability

### Failed child init and verified repair

A failed first child init remains Refused. Only a recovered store matching the
checked child destination, policy parent and runtime kind may confirm it in the
same process. A different parent/kind or absent local authority cannot confirm
it. The body gate remains ChildHeld before repair and O-owned afterwards;
neither state sends that body through Legacy. The narrow pre-init late-child
exception above does not extend to Refused children.

`WriterAdoptionThreadComposition` now distinguishes this state from Released.
The Released control cannot satisfy VerifiedRecoveryEventuallyCommits; the
unchecked-recovery control violates RecoveryVerifiedOnly and the leaking
control violates RecoveryNoLegacy. Verified recovery checks all three together.
The production entry point is `writer/host.rs`'s Recovered::Store branch: it
checks the existing VerifiedSurface before Candidate::confirm_store. The real
managed-host child test reaches both the Refused and Committed body gates and
also attempts a different parent, kind and unverified authority.

**Selected for independent design review, 2026-11-16; not implemented.** The
coordinator selected the limited recovery capability. Restricting recovery to a
still-active original admission is rejected. Decision A keeps accepted bodies
and eventually publishes them after verified recovery; it adds no per-input
failure notice. The preceding unresolved audit is the Before case, not the final
selected design. C1's version-1 record, exact byte digest and episode key stay
unchanged.

### Issuer and verified recovery

Only the current O custodian's live writer may register a validated debt and
issue a grant. A path, new source, runtime kind, current owner's name, or an
artifact deserialized from disk is insufficient. The issuer consumes the
following checked results; implementation must fail closed on every unknown:

| Check | Existing entry point / planned use |
|---|---|
| Current managed custodian | `tui_o/ownership/execution.rs:136-177` reads the active owner in one PG snapshot, compares the local executor and returns OwnerPermit. Bound references additionally pass `execution.rs:189-225` validate_local. Issuance and effect admission re-read the active tuple under the existing custody advisory lock; a boot snapshot alone never authorizes HTTP. |
| Current unmanaged gateway custodian | `discord/runtime_bootstrap/o_writer_host.rs:501-512,837-867` and `runtime_bootstrap/gateway_lease.rs:742-753,842,897`: only the existing provider gateway gate driven by its real PG lease may supply this authority. The registry uses a distinct gateway authority domain, provider, process/run UUID and lease epoch. It does not pretend a local epoch is an intake owner generation. A newly managed channel or a lost/unknown gate invalidates this domain. No dormant o_channel_homes table is used as authority. |
| Port and bot | `runtime_bootstrap/o_writer_host/owner_port.rs:49-85` checks permit/local/provider identity, resolves the current runtime and verifies the REST bot. The unmanaged host supplies its existing gateway port and gate. A new debt-specific typed attachment carries this checked custodian into the existing port; an arbitrary local string cannot create it. |
| Actual O store | `tui_o/store/mod.rs:409-450,473-493` recovers init/ledger; damage, I/O failure or ledger violation holds issuance. Record a stable store identity from the exact era/init bytes, not mutable ledger length. `writer/host.rs:713-736` applies this to Fresh and Recovered::Store. Fresh source-free creation still needs `host/first_init.rs:282-324` fenced activation and Free custody; no metadata guard is weakened. |
| Authentic retained debt | C1's typed reader validates v1/provider/channel/episode/nonce and exact filename byte digest (`discord/inflight/herdr_body_debt.rs:57-77`, `herdr_body_debt/store.rs:39-48`). The full old row, input identities and evidence are retained. The issuer receives a private validated snapshot/plan witness, never a caller-selected filename or user-supplied JSON. This trusts the same local durable-store boundary as C1, not a new arbitrary-file import API. |
| Ended old execution and plan | The snapshot was created only after C1's nonce/canonical/ended guards (`discord/recovery_engine/herdr_row_end.rs:215-279`). C2 additionally seals a compatible saved/native coverage plan and checks existing strict O/shared evidence; ambiguous overlap holds. A new source does not prove coverage. No new terminate/delete operation is introduced. |
| Live writer incarnation | `writer/host.rs:855-874`, `writer/host/transition.rs:18-39` and `writer/deliver.rs:330-337,433-450` identify the live run, stop signals and halted writer. A new C2 registry run UUID is replaced under custody on each boot/resume. It is distinct from the old execution nonce, local readiness generation and PG owner generation. |

The C1 reader above is private to Discord; those coordinates fix the validation
contract, not an already callable public recovery API. C2b's proposed
writer/herdr_body_debt.rs supplies a read-only v1 decoder and private validated
snapshot witness, checked against actual C1-produced fixtures byte for byte.
It preserves unknown raw fields and rejects unknown identity/runtime data. It
does not export or weaken C1's private Archived clear witness, and it does not
require an unlisted C1 production-file edit.

A managed authority revision is `(execution, executor, owner_generation)`; an
unmanaged revision is `(gateway, provider, process_boot, gateway_lease_epoch)`.
Both also bind the current writer run UUID and store identity. Under the custody
lock the gateway case must still have no managed intake owner/receipt; it also
requires the live matching gateway gate at the final local admission. These are
two existing authority sources, not a RuntimeHandoffKind host split. A foreign
NoCandidate can issue nothing locally. Home recovery must include access to the
original retained bytes, as the existing C1 model assumes; files are not assumed
to migrate automatically.

### Persistent identity and narrow grant

Use a new shared PG registry, leaving the C1 files immutable. Three logical
records can live in one migration: debt manifests/coverage and effects, writer
sessions, and issued grants. No clock, TTL or cache eviction grants authority.

| Record | Required binding and durability |
|---|---|
| Manifest / coverage | Provider/channel, original C1 episode and complete original input identity, exact snapshot digest(s), original row/evidence, sealed plan/version and payload bytes/hash. Compatible snapshots share coverage; an incompatible plan is held. Preserve bytes needed to reconcile a later native binding even after local snapshot GC. |
| Shared effect | A stable effect ID allocated once for `(provider, channel, original episode/input, canonical coverage fragment, identity/split version)`. Payload hash and bot are immutable; a key with a different payload is a conflict. Current owner/generation/run and changing snapshot digests are **not** effect-key components. Thus a new custodian or a second compatible snapshot cannot manufacture a second send budget. |
| Grant | An opaque unique ID referencing one existing effect and one verified snapshot/plan, current authority domain/revision, writer run, store identity, channel/provider, original input/episode, payload hash and sender bot. States issued/revoked/consumed are monotone for that grant ID. A later grant has a new ID; it inherits the effect's pending/confirmed state, not a fresh effect. |
| Writer session | Current verified authority tuple and current run UUID/version. Replacement invalidates old grants even when a crash prevented physical revocation. Neither a stale row nor its serialized token can reactivate itself. |

Original provenance is an explicit union: an immutable original intake admission
when present, or an explicit C1-validated accepted snapshot retaining its original input IDs.
For a managed snapshot retain its original delivery ID and any recoverable exact
old admission; missing old metadata is never filled from the current input.
Contradictory historical PG evidence holds issuance. A local older row with no
PG intake ID does not acquire a fabricated delivery/receipt ID. Neither form is
relabeled as a current input. Snapshot digests bind which old evidence a
grant consumed; multiple snapshots of one body remain one reconciliation plan.

The wire `Artifact` gains a **distinct debt-recovery variant**, not a new owner
seal on the original variant. Preserve the old original artifact JSON exactly
with a strict, disjoint decoder. Native `Origin::seal` still produces only the
original variant. A recovery variant carries registry grant/effect references,
original provenance and payload hash; it cannot overwrite the original
executor/generation/nonce/incarnation. Its live one-use attachment is never
serialized. Recovery from disk starts with no attachment and must revalidate
current custody. Unknown versions/mixed shapes fail closed. Older readers'
`deny_unknown_fields` (`owner_record/output.rs:23-28`) must continue to reject a
recovery artifact rather than falling back to original/unstamped output.

### Admission and use through the existing O sender

The debt branch is a separate case inside `owner_record/output.rs` acquire. The
original JOIN at lines 100-107 stays exact, and managed unstamped denial at
110-114 stays exact. A debt reference passes only if its registry row, original
provenance, manifest/plan/payload/bot, current authority revision, store and run
all match, its grant is issued, and the shared effect is neither confirmed nor
owned by another pending attempt. A current owner alone satisfies none of the
old-debt or delivery-proof checks.

1. Acquire the channel's existing delivery lease. Reconcile any previous shared
   reservation, local Prepared or O receipt before requesting another send.
2. Under existing PG custody, check all issuance bindings and CAS the shared
   effect from ready/certainly-not-sent to a unique reserved attempt. Commit
   that reservation **before any HTTP request**. It survives a crash; another
   run never interprets it as fresh. This is shared send exclusion, not Posted.
3. Acquire the existing PG effect guard in a second custody transaction and
   recheck the grant/current custodian/attempt. This guard is handed to one
   private runtime attachment; no clone creates another request. If the second
   check refuses before request construction, only the owning live attempt may
   record certainly-not-sent. Owner transfer alone cannot make that assertion.
4. `writer/deliver.rs:475,631-664` checks the debt plan/barrier and live attachment
   **before** OutputOrigin/Prepared and before its first POST poll. The existing
   ownership gate, stop receivers and lease remain in force. The registry has a
   durable reservation even if local Prepared cannot be appended. Normal native
   work still uses its normal exact/source admission; the recovery permission
   is scoped to this one old debt piece, never a channel-wide delivery override.
5. `discord/outbound/o_writer_io.rs:129-155` consumes the exact one-use attachment
   and runs the same HTTP future under the retained effect guard. Do not create
   a sender outside ChannelWriter, call raw post to bypass origin, or reacquire
   an unrelated new grant after Prepared. A token loaded by an outbox/old actor
   without its live debt preflight cannot send.
6. Owner transfer and the effect guard serialize on the existing custody lock
   (`owner_record.rs:413-439`, `output.rs:94-108,139-158`). Transfer before the
   guard refuses the old grant; transfer after an admitted request waits for its
   guard. A waiter abort does not release the request's owned guard early; keep
   the behavior tested by `output_tests.rs:344-406`.

The two PG transactions are deliberate: recording Prepared only in the guard
transaction would roll it back on process death after HTTP, reopening a duplicate
send. A committed reservation with an indeterminate outcome remains pending.
Neither NotFound, a lease deadline nor a new current owner turns it into a
receipt or an automatic fresh POST. The existing stable re-post nonce can help
reconciliation (`repost/send.rs:87-98`); nonce retention is not assumed unbounded.

### Receipt, retirement, GC and ordering

- A correct bot/content response produces the existing strict PostConfirmation
  (`writer/deliver.rs:765-790`, `store/ledger.rs:121-156`). Bind it to the stable
  shared effect and immutable payload/coverage. History settlement may supply
  proof only after exact bot/message/payload/identity checks using the existing
  bounded confirmation path (`writer/confirm.rs:142`, `store/delivery_evidence.rs:85`).
  A generic Settled outcome is insufficient: ledger.rs:348-375 also calls
  NotFound/Unresolved settled. A shared LocalPosted receipt that lacks exact
  content proof must be revalidated, not trusted as exact coverage.
- Local strict Posted and shared receipt consumption are separate crash cuts.
  If local Posted is durable first, boot must verify/backfill the same shared
  effect receipt without another POST. If PG confirmation wins before the local
  mirror, the next run imports that same proven effect. Once confirmed, the
  receipt/tombstone survives owner/run changes and consumes **all** grants for
  that effect. No receipt key includes the current owner/run.
- A real permanent refusal or a certainly-unsent preflight failure revokes that
  grant; it does not mark the body delivered, delete it or create a user-visible
  B receipt. Later verified repair may issue a new grant for the same effect
  only after a conclusive no-effect outcome. An uncertain previous attempt
  instead has confirmation-only work, with no new POST permission.
- clear/stop/terminate close the matching live run immediately at the local
  admission gate and monotonically revoke its grant/run. PG unavailability does
  not reopen local admission. Boot replaces the registry run before any debt
  POST and treats old tokens as read-only evidence. An already admitted owned
  request is settled with the existing stop/guard lifetime; no new kill or
  early release authority is added. A future legitimate resumed run reissues
  from retained debt; it never revives a revoked grant ID.
- Ordinary Cancel of a different/new input is not evidence about an older
  archived body and does not delete or settle it. If that cancel actually stops
  the writer/channel, the same run-revocation rule applies. Terminate's held
  exclusion remains held for its existing postlude and must inhibit debt
  admission too; it is not a debt receipt.
- Grant rows may remain as invalid tombstones; “revoked” means unusable, not
  physical deletion. Owner generation/run mismatch rejects stale rows even if
  crash prevented their status update. No TTL/attempt-count cleanup is added.
- Snapshot GC needs the old body's durable shared exact receipt plus its local
  durable mirror/coverage proof. Preserve C1's archive while its guarded clear
  can still need it; an active/unreadable row lookup conservatively retains it.
  A successor never inherits that archive's clear witness. GC does not delete
  manifests/receipts needed for later native dedup, nor pending/unknown debt.
- Actor boot/resume/incarnation drains old debt before ordinary owed work; an
  additional pre-Prepared barrier catches a newly visible archive. Execution
  input/gateway holds remain separate: an old debt barrier must not reassert
  C1's retired-row hold or weaken ordinary input metadata. No new output may
  overtake an unresolved older accepted body.

Saved/native reconciliation remains part of the permission, not text-only global
dedup. Bind saved coverage to original episode/input. When later native evidence
has a unique matching overlap, use one plan with existing confirmed prefix and
only the proven unpaid suffix. Do not declare a whole native unit Posted merely
because a prefix was visible. A durable coverage alias may suppress that native
unit only after every span has exact delivery proof. Unknown/rewritten overlap
stays retained and unposted. Normal native derivation/admission must consult these
aliases even after local debt-file GC; owed.rs:18-31 and pieces.rs:24-50 carry the
source/range/old-input coordinates needed to distinguish it from a new turn.

### Boot, neighbouring consumers and input classes

| Entry / consumer | Planned boundary; existing code coordinate |
|---|---|
| C1 restart restore | `discord/recovery_engine/restore_inflight.rs:103` keeps its missing-row queue wake. Debt body registration is independent of active-row presence; no new input hold. |
| Fresh / recovered boot | `writer/host.rs:713-736`: validate debt/session and reconcile uncertain receipts before metadata. Fresh creation still uses first_init's fence, not a special weaker store. |
| Actor restart / resume | `writer/host.rs:855-874`, `writer/actor.rs:412`: replace session/run, load shared effects and drain old debt before normal delivery, including verified Codex. |
| New native source / incarnation | `writer/actor/fresh_resume.rs:24-45`, `writer/suspension.rs:48-76,80-96`: capture the source-selection nonce/run boundary, invalidate the old attachment before rescope, recheck a new grant. A binding change is neither old-body coverage nor a receipt. |
| Actual final sends / re-post | `writer/deliver.rs:631-664,997-1034`, `repost/dispatch.rs:175-205`: ordinary and re-post sends keep their lease/gate/first-poll path. Only the exact admitted debt piece can use a recovery attachment; unrelated PG re-post pieces cannot pass its barrier. |
| Writer/host stop | `writer/deliver.rs:433-450`, `writer/host.rs:605-609`: invalidate local run admission before releasing it. Recoverable store failure retains pending effect/receipts. |
| clear / terminate | `discord/herdr_terminate/http_force.rs:30-83`, `herdr_terminate/retired_exclusion.rs:28-51`; a small new-path invalidation hook and final live-fence check inhibit debt sends under the captured exclusion. No legacy clear/tmux repair. |
| Cancel / successor row | Existing captured turn token and terminate ownership guard at `discord/herdr_terminate.rs:489-495,545-555` remain exact. Cancel of a new input cannot spend or delete an old effect. C1's guarded exact-row clear is unchanged. |
| Ordinary source observers | Original acquire/metadata rules stay exact; neither an unverified new source nor a suspension can mint coverage or a current custodian proof. UnknownKind stays held. |

Human/machine/routine are original-input provenance, not issuer roles.
`discord/inflight/model.rs:138-161` preserves requester/message, absorbed source
IDs, origin IDs and optional intake delivery. For managed inputs retain their
original receipt classification; a local accepted input retains its C1 raw row
and does not fabricate an intake ID. Missing classification is not “human”.
Routine presentation follows the persisted policy in model.rs:385 and
`discord/routine_presentation.rs:41-83`: a proven NoVisibleResult can produce a
policy disposition with no body; missing/ambiguous terminal evidence keeps the
full body. This is the existing policy, not a new visible failure receipt.
RuntimeHandoffKind never chooses the permission or turns UnknownKind into false.

### Model guarantees, assumptions and implementation review

The companion now has a capability record separate from old body/receipt state.
CapRecovery means that the checks in the first table have actually succeeded;
it does not manufacture authority from current owner text. CapIssue/Reserve/Acquire/Post
model the narrow grant, PG custody and the first HTTP effect; CapEnd/Confirm/GC
keep its pending effect and exact receipt across run/owner changes. Transfer,
stop, writer restart and a proven-no-effect refusal are each allowed once in
this finite model. Their run/authority revisions are distinct; the finite event
bound is a model exploration bound, not production TTL, retry limit or cleanup.

With fair verified recovery, a compatible certified plan, a successful O effect
and eventual truthful receipt evidence, Refused/NoCandidate/Deferred/already-owned
controls progress. No extra failure receipt is used. The Before configs have the
same recovery events but no capability and stay liveness RED. Permanent absence
of store/source/authority is still an external-recovery condition under A.
The explicit UnknownPlan and ReceiptUnproved controls also stay liveness RED:
restoring the owner/store cannot prove an ambiguous overlap or an indeterminate
remote effect. Safety forbids treating either as success. The model does **not**
claim unconditional HTTP exactly-once delivery from a DB transaction, an unlimited
Discord nonce, or a fresh retry after an unproven Prepared. These limits must be
visible to the independent reviewer, not hidden in a GREEN count.

This is a proposal for pro design review. It establishes no production guard
until the implementation calls and validates the listed boundaries. The
post-reservation/pre-HTTP crash cut is particularly important: its safe pending
state and the condition for eventual conclusive evidence must be reviewed before
C2c can claim end-to-end liveness.

### Implementation slices and one additional ownership request

The previous 15-file/+1050 estimate omitted the new authority, durable shared
reservation, receipt/coverage bridge and lifecycle invalidation. Plan **27 unique
production files / approximately +2300 net code lines**, in three sequential
review slices. Each slice stays below 30 files/+1500 and the +1200 PLAN stop
line. These are estimates, not a cap exception: measure each slice and stop for
PLAN_READY if its projected net exceeds +1200. No production code is in this
design commit. No other lane is merged in advance.

| Slice | Production files | Estimated net | Reviewable result |
|---|---:|---:|---|
| C2a: registry / capability boundary | 12 | +900 | New shared schema, strict original/debt artifact split, registration/issue/consume validation and acquire tests. Existing native/unstamped rules stay exact. Issuance/transport helpers without a live caller remain cfg(test) until their consuming slice; no allow or warning-baseline increase. This does not claim body recovery is wired. |
| C2b: durable plan / native coverage | 11 | +650 | Real rowless debt reader, compatible snapshot/input merge, preserved provenance, shared coverage/effect registration, boot wake and source-coordinate propagation. Promote only helpers actually called by the host; no new sender. Ambiguous overlap remains retained. |
| C2c: writer / lifecycle / receipt ordering | 14 | +750 | Pre-metadata uncertainty reconciliation, actor drain and final pre-Prepared barrier; one-use guarded POST; strict receipt backfill, GC, old-before-new and clear/stop/restart invalidation. Only this combined result can complete C2. |

C2a files: output.rs and the four proposed debt_recovery modules below, migration,
owner_port.rs, outbound/o_writer_io.rs, writer/mod.rs, store/ledger.rs,
store/delivery_evidence.rs and writer/confirm.rs. C2b files:
runtime_bootstrap/o_writer_host.rs, its adoption_wake.rs, writer/host/first_init.rs,
writer/host/activation_io.rs, writer/herdr_body_debt.rs, its replay.rs,
writer/pieces.rs, writer/owed.rs, writer/host.rs, registry.rs and writer/mod.rs.
C2c files: writer/host.rs, actor.rs, deliver.rs, herdr_body_debt/receipts.rs,
actor/fresh_resume.rs, suspension.rs, herdr_terminate/retired_exclusion.rs,
outbound/o_writer_io.rs, runtime_bootstrap/o_writer_host/owner_port.rs,
writer/mod.rs, writer/herdr_body_debt.rs, authority.rs, receipt.rs and
store/ledger.rs. Shared files are reviewed as sequential deltas, not parallel
ownership. The union is the original 15 plus the following **12 additions**.


| Additional file (and its tests) | Purpose |
|---|---|
| `src/services/cluster/intake_router_hook/owner_record/output.rs` | Disjoint original/recovery artifact dispatch; preserve existing original JOIN and unstamped denial. |
| `src/services/cluster/intake_router_hook/owner_record/output/debt_recovery.rs` | Private validated debt/plan/claim/one-use attachment types. |
| `src/services/cluster/intake_router_hook/owner_record/output/debt_recovery/registry.rs` | Shared manifests, coverage aliases, run and effect/attempt CAS; no TTL cleanup. |
| `src/services/cluster/intake_router_hook/owner_record/output/debt_recovery/authority.rs` | Custodian verification, issue, guarded acquire and monotone revoke. |
| `src/services/cluster/intake_router_hook/owner_record/output/debt_recovery/receipt.rs` | Exact proof bridge and consumption independent of current owner/run. |
| `migrations/postgres/0195_herdr_body_debt_recovery.sql` | Shared schema, immutable key/hash constraints, unique active run and effect/attempt constraints. Renumbered from 0191 because parallel lanes took 0191-0194. |
| `src/services/discord/runtime_bootstrap/o_writer_host/owner_port.rs` | Attach the validated current custodian/bot to the existing port; no new transport. |
| `src/services/tui_o/writer/actor/fresh_resume.rs` | Invalidate old run/attachment on an actual rescope before it can be reused. |
| `src/services/tui_o/writer/suspension.rs` | Expose a scoped live selection/stop witness at debt admission; do not weaken source policy. |
| `src/services/discord/herdr_terminate/retired_exclusion.rs` | Invalidate only the matching new-path writer run under the existing exclusion. |
| `src/services/tui_o/writer/pieces.rs` | Carry proven source/input/coverage coordinates; preserve ordinary identity/split rules. |
| `src/services/tui_o/writer/owed.rs` | Associate later native units with certified old coverage and truthful receipts; no blanket suppression. |

Tests and required generated test-inventory additions travel with each slice.
Use the official generation commands; no hand-edited checksum/inventory baseline
or new empty test-support module. The host/actor/deliver edits remain small call
sites because #8118/#8148 also touch them. #8192 overlaps the transport/bootstrap
boundaries; record that overlap without modifying its worktree or premerging it.

Acceptance before implementation approval: real PG owner transfer then debt
recovery while ordinary old native remains denied; wrong owner/provider/channel/
episode/input/snapshot/effect/payload refusals; same effect across compatible
snapshots and owner/run changes; concurrent reservation single winner; local
Posted/shared receipt crash cuts; pre-HTTP reservation crash retains pending;
old-before-new; rowless boot/restart/incarnation wake; clear/stop/terminate with
an owned in-flight guard; source-free saved/native-prefix/suffix coverage; exact
routine NoVisibleResult; UnknownKind and ambiguous overlap remain held.
Mutants remove the reservation CAS or transfer lock, reuse a revoked run, swap
any identity/hash, count an uncertain result as a receipt, allow unrelated native
POST, skip the final barrier or GC an unconfirmed snapshot. Each must be RED at
the real boundary; no 0-test/ignored count can satisfy it.

Liveness assumes an eventually stable recovered custodian long enough to admit
and settle a request; unlimited hostile owner churn is not a recovery guarantee.
The model has an explicit committed CapReserve before CapAcquire/Post. Its
CapCertainNotSent is an abstract **conclusive owning-attempt proof**, not a rule
that a changed owner/run implies no HTTP effect. CapReceiptUnproved disables
that progress by retaining indeterminate Prepared. Implementations must not
replace the proof with an age, NotFound or restart heuristic. Historical Legacy
response offsets are likewise not strict O receipts; ambiguous prior visibility
stays held rather than being silently counted as O coverage. Unknown original
runtime/identity data remains unreadable, never a false/unowned cleanup result.

The model projects a qualified provider/channel target, original input/episode,
snapshot, effect and payload to old/other identities. It checks the permission
protocol, not JSON decoding, actual SQL constraints or native-prefix alignment.
Those are explicit C2a/C2b acceptance tests; their runtime guards are not treated
as already implemented because a model control is GREEN.

### P2-1: an explicit witness, not a lifecycle inference

A reserved attempt now has a separate identity and optional no-effect witness.
Only a witness for **that same attempt/effect/payload** can authorize reset.
Generation, run, live-state, timeout and process disappearance do not create it.
The implementation boundary is a private owning-attempt proof factory: a request
known not to have been polled, or another conclusive no-effect transport result.
It cannot be constructed by restoring a grant, reading NotFound, or observing a
new owner. The witness remains attributable after a later attempt is reserved;
a witness for attempt A cannot release attempt B.

CapPrepare is separate from CapPost. Reserved-only and durable-Prepared-before-
HTTP crash cuts are both explicit. With no evidence, both stay pending with
POST count zero and conditional body liveness RED. CapProvideNoEffect is an
external, attempt-bound evidence event; fairness applies only when such evidence
actually becomes available. With a conclusive exact effect or no-effect witness
eventually supplied, the attempt can settle or be re-reserved and posted. This is
the GREEN scope, not a guarantee from owner/store recovery alone. The two proof
mutants (skip witness validation; reset merely on changed run) violate
NoResetWithoutProof. C2a's shared schema stores attempt identity and no-effect
proof explicitly; no code may infer or fabricate the witness from run metadata.


### C2a implementation boundary

C2a implements the strict original/debt artifact union in `owner_record/output.rs`
and the shared tables in `migrations/postgres/0191_herdr_body_debt_recovery.sql`.
Existing original artifact JSON, the original admission JOIN, and managed
unstamped denial stay unchanged. The recovery branch requires the same custody
lock plus a nonserialized, one-use live attachment. A deserialized token cannot
restore that attachment. The registry rechecks immutable effect/plan/snapshot
bindings and current owner/run/store/bot, and commits a reserved attempt before
returning the second transaction's effect guard. Dropping that guard retains the
committed reservation; it never means no effect.

Only the owning guard's pre-poll path records an `owning_not_polled` witness,
bound to the exact attempt, effect and payload. A started request, owner transfer,
new run or dead process supplies no such witness. The SQL witness constraint
also rejects missing or mismatched binding fields. An exact shared receipt
blocks every grant for that effect, including grants issued before confirmation.

`output/debt_recovery/authority.rs` issuance and the strict Posted bridge in
`output/debt_recovery/receipt.rs` remain test-only until their real callers arrive.
The test issuer is a boundary harness, not a verified C1 reader or a production
registration API. C2b must supply the private validated snapshot/coverage and
actual host/store/bot/run witnesses before promoting issuance. C2c supplies the
live writer preflight, receipt/backfill, lifecycle invalidation, ordering and GC.
The existing port, ledger and writer APIs already carry the Artifact union;
C2a needs no additional calls in those files. This slice does not claim that
stored bodies are replayed or that end-to-end conditional liveness is complete.

A live acquisition rejected at the second guarded check still owns its exact
never-polled Attempt and records that witness before returning a refusal. The
changed owner/run is only the reason for refusal, never the no-effect evidence.
A dropped/crashed acquisition has no such in-memory ticket and leaves pending;
a DB failure that prevents proof commit also leaves pending. Attempt state is
boxed so ordinary output futures do not carry the complete recovery reference.


### C2b implementation boundary

The rowless reader in `writer/herdr_body_debt.rs` opens C1's actual directory,
`runtime/discord_herdr_body_debt/<provider>`, relative to the host runtime root.
It validates the v1 filename's exact-byte SHA, provider/channel, original episode,
known Herdr/TUI identity and typed original input fields. C1 may omit its physical
host locator: the original logical session, nonce and episode remain the identity,
without synthesizing the current owner's host fields. It neither loads nor
recreates an active row. The manifest retains the exact encoded snapshot and
raw row JSON, including unknown fields and missing historical input metadata.
A real C1 archive/guarded-clear fixture checks this directory and byte contract;
a separately constructed JSON fixture is not its only evidence.

Compatible snapshots have the same original episode/input, sealed body plan and
noncontradictory known intake delivery. They union immutable snapshot bytes and
positive native aliases. Missing source/input-origin lists stay missing in their
original snapshots; a later list does not relabel an earlier accepted input.
Changed body plans, conflicting known deliveries, uncertain prior visibility,
unknown policy, and unproved saved/native overlap remain retained.
An unreadable snapshot remains on disk and holds only its original episode;
other valid episodes keep their existing order. Returned plans are candidates,
never proof that no other retained file or unresolved body exists.

Native text equality or a new source is insufficient coverage proof. The reader
requires an exact pinned source/range, an unambiguous native UnitKey/piece, and
an existing O ledger Original artifact with the same original intake delivery,
provider/channel and exact payload. It preserves that old artifact verbatim;
saved and native coverage compare ordered split payload bytes, including the
splitter's newline boundaries and fence closing/reopening, rather than unsplit text.
it never seals the text with the current owner's Origin. A rewritten source
whose unit belongs to another input therefore cannot adopt the old body even
when its text matches. Unmanaged or historical inputs without this evidence
keep their native overlap retained. Original admission evidence is not a Posted
receipt; C2c must still reconcile the actual effect and historical PG evidence.

Shared registration uses C2a's existing tables under the channel custody lock.
The unique effect key is provider/channel/original episode/input/coverage;
coverage is `body:v<identity>:s<split>:<piece>` using the existing identity and
split versions. Plan SHA seals those versions, original synthetic saved-unit
key, piece indices and exact payload bytes. Snapshot hashes and current owner,
generation, run or source are not effect-key components. A compatible repeat
adds evidence and returns zero new effects; it cannot reset a reserved or
confirmed attempt. Changed payload/plan/bot/input or native coverage claimed by
another episode is refused atomically.

The shared provenance JSON is version 1, kind `c1_accepted_snapshot`, with
`snapshots` mapping each exact SHA to bytes/row_json/evidence/recovery_notice and
`native` retaining old UnitKey, piece index, payload/hash, old output_origin and
source coordinate. C1's v1 files and C2a's schema remain unchanged. Neither
registration nor a queue wake creates a run, grant, receipt, settled flag or GC
right. The issuance and receipt harnesses remain cfg(test): they have no real
caller in this slice.

Recovered and Fresh hosts validate retired snapshots before the unchanged
metadata guard. They register after the existing port wait proves the sender,
and reread for every host actor start/resume. The debt wake currently checks O
output ownership, which is not a local execution permit, and spawn_execution
also reaches it from periodic refresh. C2c must move this scan to the actual
one-time bootstrap boundary and filter execution permits/gateway authority;
foreign-executor exclusion is not claimed here. Existing queue admission still
judges every wake. First-init fences and ordinary source guards are unchanged.

C2b supplies the immutable plan/coverage prerequisite of CapIssue. CapRecovery,
Archive/Detach and CapIdentity/CapUnknownPlan already describe its rowless and
fail-closed states, so the 54 companion configs and their assumptions are
unchanged. Runtime tests cover bytes, merge and original native coordinates,
which the abstract permission model deliberately does not encode.
Locator decoding and split payload comparison are outside that model too. The
repair changes no TLA/config assumption or permission transition; actual C1
publication, host start/resume and long-body PG tests cover these boundaries.

C2b is not completed debt delivery. C2c must add pre-metadata uncertainty
reconciliation, actual guarded capability issuance, drain/POST barriers,
old-before-new, strict receipt/GC and lifecycle invalidation. In particular the
ordinary actor is not given permission to POST a saved PieceWork by registration.
Refused and permanent no-source Deferred visibility remains conditional on
verified owner/store recovery and eventual exact-effect or conclusive no-effect
evidence; unconditional body liveness is not claimed.


C2b stores the full manifest once, on the effect for piece zero. Every effect's
provenance has version 1, kind `c1_accepted_snapshot` and `manifest_coverage`.
Only the manifest effect also contains `snapshots` (exact SHA to encoded bytes,
row_json, evidence and recovery_notice) and `native` (original UnitKey/index,
payload/hash, old Original artifact and source coordinate). Resolve a manifest
reference using the same provider/channel/episode/input/plan SHA plus its coverage;
a coverage string by itself is not an identity. Each effect retains the union
of snapshot SHA references C2a's acquire validates. Registration commits the
manifest and its references atomically under custody; no shared row is deleted.
This avoids multiplying the full archived body by the number of split pieces.
C2c must retain that manifest/alias evidence after local C1 snapshot GC.

An effect's initial `ready` state is not proof of no historical POST. C2b's
registration cannot issue a grant. Before issuing one, C2c must reconcile the
original saved/native/Legacy visibility and pending attempts against exact effect
or owning-attempt no-effect evidence; neither source arrival nor an offset of
zero supplies that proof.


### C2c remaining implementation estimate after the main merge

This is a proposed review split, pending the lane's PLAN_READY decision. No
C2c issuer, sender, receipt, lifecycle or GC production implementation is in
this planning change. The approved semantics and C1/registry formats stay exact.

The earlier +750 estimate counted insertion points but not the live backend
operations still hidden behind test-only helpers. At merged main `5c144746da`
with b2 `a4cdeeedb8`, the existing `deliver.rs:642-664` appends OutputOrigin and
Prepared before polling `o_writer_io.rs:138-151`, where acquire reserves the
effect. C2c therefore needs an owned preflight/request handoff to reserve and
validate before Prepared, while keeping the same lease, ownership gate and
raw request guard. It cannot be only an actor call to the test issuer.

| Required remaining work | Estimated net production lines |
|---|---:|
| Live execution/gateway run installation, replacement, invalidation and exact store/plan-bound issuance (`authority.rs`, `owner_port.rs`, `o_writer_io.rs`) | +300 |
| Pre-Prepared owned preflight/request handoff, one-use guard and known-unpolled handling (`writer/mod.rs`, `deliver.rs`, `o_writer_io.rs`) | +230 |
| Pre-metadata reconciliation, serial drain and final ordinary/re-post barrier (`herdr_body_debt/receipts.rs`, `host.rs`, `actor.rs`, `deliver.rs`) | +260 |
| Strict local/shared receipt backfill and truthful native alias mirrors (`receipt.rs`, `herdr_body_debt/receipts.rs`, `store/ledger.rs`) | +220 |
| Stop/clear/terminate/rescope hooks and authority-filtered one-time boot wake | +100 |
| Receipt-only local GC, shared-only manifest/alias loading after GC and crash recovery | +280 |
| Total (merge resolution and tests excluded) | +1390 |

The estimate exceeds the +1200 planning stop line, even though it fits the
+1500 review maximum. Do not shorten proof checks or combine SQL statements
merely to fit a line budget.

Proposed C2c-I: P2-2/P2-3 in its first implementation commit, then live
issuance/preflight, pre-metadata and drain/final barriers, strict receipt
backfill/local aliases, and lifecycle invalidation (about 18 files/+1110).
Keep every C1 file; local GC is deliberately absent. Registration remains
insufficient to send; uncertain/unproved work stays retained. This slice does
not claim C2 complete.

Proposed C2c-II: receipt-only local GC plus the shared-only manifest/alias
reader used after that GC, with boot/restart/crash-cut and alias-retention
acceptance (about 6 files/+280, overlapping the first slice's reader/receipt
files). Only the combined result can claim C2 complete. All old-before-new,
conditional liveness and late-child/old-debt exact-once requirements remain.
The proposed file set stays inside the approved C2c/wake ownership.


## C2c-Ia support boundary

The C2c-I implementation is split at the operational caller boundary. Ia includes
the typed lower registration port, custodian/runtime/session support, strict shared
receipt and native coverage proof, and the local ledger's atomic `DebtPrepared`
and truthful `DebtReceipt` mirror. Issuer, inspection and coverage helpers without
an operational caller remain `cfg(test)` until Ib. The only production registration
implementation is the immutable validated writer `Plan`; there is no JSON issuer
or independent sender. C1 version 1 snapshots and Original artifacts are unchanged.

`DebtPrepared` contains its original debt artifact and Prepared in one fsynced
ledger line. An incomplete line restores neither; an invalid admission must not
install an origin first. Existing ordinary Original OutputOrigin/Prepared bytes
and admission are unchanged. A shared-only `DebtReceipt` advances the observed
anchor and preserves exact effect identity without manufacturing a native
Prepared, Posted, local serial, or native POST confirmation. Coverage aliases are
usable only with the registered original artifact, unit/index and payload and
strict shared evidence for every canonical fragment. Same text from another
input is insufficient; repeated ambiguous payload fragments remain retained.

Ia does not install a driver, select sources, start POSTs, or delete C1 snapshots.
Ib still owns pre-metadata reconciliation, actor drain/final admission barriers,
reservation-before-Prepared/HTTP, lifecycle invalidation and old-before-new. II
still owns local snapshot GC after strict shared receipt plus durable local mirror,
and shared-only manifest/alias lookup after that GC. C2 is incomplete until II.

The fourteen WriterAdoptionDebtDelivery configurations describe that composed
implementation target. Ia closes StrictSharedReceipt's support boundary and
Restored/SharedFirst's exact-proof/mirror support, and preserves Unproved's pending
state across a new run. Their full host liveness, AtMostOnce,
ReservationBeforeHttp and OldBeforeNew require Ib's operational connections;
a GREEN abstract target is not a claim that Ia delivers debt. The nine child/boot
composition and fifty-four capability configurations are unchanged by this split.
Refused/no-source visibility remains conditional on verified owner/store repair
and eventually obtaining exact effect evidence or an attempt-bound certain
no-effect witness. No unconditional liveness is added.
