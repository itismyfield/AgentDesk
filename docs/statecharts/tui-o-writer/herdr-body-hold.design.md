# Herdr execution retirement and retained body debt

Design audit on `13355ee75c74ef65b2c5564f0a95efb37f988d26`, containing main
`ad04f6f3ecc33e86e7dc0968822ce2ce3f087e90`. This note and its model change no
production behavior. The outer-only ownership mapping is withdrawn. C is a
candidate, not an approved implementation: the Refused body disposition below
needs a product decision.

## What Refused means today

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
| C | Verify the old execution ended; fsync a body-debt record keyed by the old episode; guarded removal of that active row; release gateway/input hold. After adoption, O settles the old body and records its receipt before GC | Requires a new durable debt carrier and an O replay/receipt adapter. Keeps old and new output identities separate; Refused still lacks a terminal policy |
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

## C's proposed storage and output contract

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
   may progress. The explicit Refused decision below can also end that ordering
   obligation without pretending the body was delivered.

The model assumes truthful durable effect/receipt reconciliation and an O adapter
capable of representing old debt. It checks protocol obligations, not a proof that
today's transport or storage already implements them. The foreign NoCandidate
control uses the brief's explicit assumption that home returns with valid O
authority **and access to this old debt**. It does not grant a foreign host new
authority or assume node-local files move automatically.

## Decision still required

Neither C nor stored N is selected for implementation. Two Refused policies are
shown separately rather than silently changing b2:

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
NoSource control. Separating the row guard does not create O readiness. The
implementation must either demonstrate valid source-free O initialization and an
old-body replay adapter, or obtain a visible failure policy for this case too;
this note adds no timeout-based discard or forced adoption. Existing fresh owner
initialization can use empty sources (`writer/host/first_init.rs:282-324`), but
requires fenced authority and Free custody; connecting it to old-body debt is
part of C2, not an already proven recovery service.

CRefusedRepair is a separate conditional control: a **fair external verified
store recovery** is assumed. Its GREEN result is not evidence that current b2
will recover permanently Refused channels. The silent-refusal mutant is RED.

## Model checks and before/after comparison

`WriterAdoptionHerdrRowEnd.tla` keeps NoLegacyBodyOnTui, UnadoptedNotOwned and
UnknownOwnerKept. New invariants are NoBodyLoss, BodyExactlyOnce,
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
