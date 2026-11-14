# Handoff journal custody and the fronts it holds

Code modelled as `held1`: `bdaf84168524af741060f001dbde0d9686a55fcc`; the `fixed` policy is the
change that follows it. Companion to [HerdrHandoffLaunch](herdr-handoff-launch.report.md): that
model starts from a launch that fails; this one starts from a journal no launch consumes.

A handoff journal records two things that end separately: the old pane whose end is not yet
confirmed, and the retry budget of the input that currently leads the queue.

## Decision

- **A channel that left Herdr.** Boot config is the handoff target. Its journal is dropped once
  the old pane ended or the journal's original is no longer queued, and the front takes the
  ordinary path.
- **A refusal no later look clears** ends the front on a finite budget. The channel left Herdr
  before the old pane ended, the mapped host refuses, the session key cannot be resolved or
  names another session, the saved pin cannot be read, a Bound row stands over an unended
  journal, or the old pane's close keeps failing: each look is counted, and the fifth retains
  the front with its notice in one commit.
- **Custody outlives the input.** Retaining a front never drops an unended journal: its id and
  pin stay and only the budget is emptied. No timer runs while no input waits.
- **The budget belongs to the front that spends it.** A later front starts from zero, and a
  cancelled front takes its count with it.
- **Any later front continues the journal's close.** Only a person's input opens a journal; a
  routine, scheduled or HTTP start is judged under one that stands and is never started before
  the old pane ended.
- **A start of another session.** Once the old pane ended, a front whose turn runs under
  another session key finishes the journal (its original no longer queued) and proceeds on its
  own target. The reserve's session check is unchanged.
- **Cancelling inputs is not ending the session.** A single cancel and cancel-all keep an
  unended journal; an ended one goes with its input as before.

## What is and is not guaranteed

Refusals of the kind listed above end by the finite budget. A look held by the session
transition lock or by a running turn is not counted; it proceeds once the lock is released and
the turn ends. An indefinitely held lock is outside this change.

No input reaches a host before the old pane's end is confirmed, with two stated exceptions:

- A session reset (`/clear`, idle cleanup) discards the queue and the journal together. It is
  the operator giving up the old pane's custody, not a confirmation that the pane ended.
- An effort continuation is not judged here; the reserve still refuses it while a journal is
  unended. Its producer is not wired. Before it is, retention of a continuation must clear the
  effort barrier, or every later input stays blocked.

A persistently failing close can keep only two of: no send before the old pane ended, a finite
end for the front, and delivery without a resend request. This change keeps the first two; the
#8077 rule that such a front is never retained is replaced. A pin whose pane was replaced never
closes, so every later input on that channel is retained after its budget until `/clear`.

## Journal lifecycle census

`git grep -n -E 'begin_handoff|finish_handoff|abandon_handoff|handoff_held|unjudged_held|handoff_start_failed|handoff_ended|handoff_fronts_left|handoff_input_left|unfinished_handoff|\.handoff\(|claude_handoff::(prepare|recover|retry_start)' -- src`
(tests excluded).

| Entry | Code | Journal effect |
|---|---|---|
| Start, a person's input | `provider_dispatch/owner_input.rs` handoff closure; `input_owner/claude_handoff.rs` `prepare` | opens or reuses the journal, closes the old pane, counts a refusal |
| Start, a routine, scheduled or HTTP front | same closure, `Start::Kept`; `headless_turn/entrypoints.rs` `owner_start_session_key` | never opens a journal; under one that stands it is judged by the session it runs under |
| Start, an effort continuation | same closure | not judged; the reserve refuses it while the journal is unended |
| Begin / reuse | `queued_head/v2/claude_handoff.rs` `begin_handoff` | created `ended=false, failures=0` for a Legacy or absent row on a mapped channel |
| Pin / old pane ended | `handoff_pin`, `handoff_ended` | exact pane, `ended=true` |
| Counted hold | `handoff_held` (`Left`, `Refused`, `Close`); `prepare` `spend`, `count` | the front's count and deadline (none for a failed close); the fifth retains the front |
| Unjudgeable front with no journal | `unjudged_held`; `prepare` `refuse` | no journal; held once, retained on the second look |
| Channel left Herdr | `abandon_handoff`; `prepare`, `recover` | dropped when ended or its original is gone |
| Launch failure before reserve | `handoff_start_failed`; `provider_dispatch/start_failure.rs` | the front's count and deadline, fifth retained |
| Retention | `queued_head/v2/start_failure.rs` `start_failed` → `handoff_fronts_left` | ended: dropped; unended: kept, budget emptied |
| Herdr reserve | `queued_head/v2.rs` `place` | consumed with the attempt of the journal's own session; refused otherwise |
| Reserve refused | `input_owner/pump.rs`; `provider_dispatch/start_failure.rs` `finish` | not counted, so `prepare` decides the session before any launch |
| Finish | `finish_handoff`; `prepare` (Bound row, front of another session), `recover` | dropped once its original is no longer queued; the caller reads whether it was |
| Boot | `pending_queue_persistence/queued_recovery.rs`; `provider_dispatch/owner_input.rs` `recover`; `runtime_bootstrap.rs` | a journal alone selects the actor; a timer is armed only for an ended journal or a pending deadline |
| Timer | `claude_handoff.rs` `schedule`, `retry_start` | one per journal; wakes `recover` and the owner, and is not re-armed for an empty custody |
| Cancel one input | `queued_head/v2/control.rs` `cancel` → `handoff_input_left` | the original or the spending front empties the budget; an ended journal goes once its original is gone |
| Cancel every waiting input | `clear_waiting` → `handoff_fronts_left` | ended: dropped; unended: kept, budget emptied |
| Session reset, idle cleanup | `control.rs` `clear` | dropped with the queue |
| Stop | no journal access; the turn boundary wakes the owner | the held front is judged again |
| RootReplaced | `recovery_engine/herdr_server_replaced.rs`, `herdr_reader.rs` deferred kickoff | no journal access; the row is Retired and the kickoff wakes the owner |
| Herdr mapping change | boot config only (`config/session_hosts.rs`) | no journal write; the next `prepare` or `recover` reads the new mapping |

## Model and bounds

`HerdrHandoffHeld.tla`: two or three inputs, `Cap = 5`. `Policy` selects the tree: `before`
(base), `held1` (the first fix), `held2` (custody without a per-front budget or session check)
and `fixed`. The second input may run under another session (`Target2`), the host may start
accepting (`Host = "recovers"`), the close may keep failing (`CloseFails`), and a person may
cancel (`Cancels`). Every modelled value except the hold counter survives a restart.

Outside the model, covered by tests only: a front with no journal that cannot be judged, a
Bound row, a kept start whose session cannot be resolved, the transition-lock and running-turn
holds, session reset, and the effort continuation.

| Rule | Meaning |
|---|---|
| `InputOnce`, `NoLoss` (safety) | an input is delivered or retained at most once, and never leaves unseen |
| `BoundedHolds` (safety) | one front is re-held at most `Cap` times |
| `OwnBudget` (safety) | an input is retained only after its own looks spent the budget |
| `NoSendBeforeEnd` (safety) | no input reaches a host before the old pane's end is confirmed |
| `CustodyKept` (safety) | a mapped channel's unconfirmed old pane stays under its journal |
| `QuietWhenEmpty` (safety) | no retry deadline stays armed once no input waits |
| `OnlyEndedLeftReleased`, `EndedLeftDelivers`, `InProgressDelivers` (safety) | only an ended journal of a channel that left Herdr is dropped with its input queued, and that never costs the input |
| `FrontProgress` (liveness) | every input is delivered, or retained and told, or cancelled by its person |

## TLC result

Plain TLC 1.7.4 through `scripts/ci/statecharts_tlc.py`; thirty configs, every expected
violation matched.

| Config | Policy and start | Result |
|---|---|---|
| Before, BeforeUnended, BeforeRefused | base | RED BoundedHolds, FrontProgress |
| Fixed, FixedUnended, FixedRefused | channel left Herdr or host refuses an ended journal | GREEN |
| InProgress, InProgressRecovers | mapped, Herdr host, unended | GREEN (closed, then delivered) |
| InProgressRefusedHeld1 | `held1`, host refuses, unended | RED BoundedHolds, FrontProgress |
| InProgressRefused | host refuses, unended | GREEN (retained and told, custody kept) |
| PersistentCloseHeld1 | `held1`, close keeps failing | RED BoundedHolds, FrontProgress |
| PersistentClose | close keeps failing | GREEN |
| OtherTargetHeld2 | `held2`, host recovers, second input of another session | RED FrontProgress |
| OtherTarget, OtherTargetRefused, OtherTargetClose | second input of another session | GREEN |
| CancelHeld2 | `held2`, three inputs, cancels | RED CustodyKept, OwnBudget, QuietWhenEmpty |
| Cancel, CancelRecovers | cancels | GREEN |
| RetryForever, CloseHolds, NoCount (mutants) | the budget does not end the holds | RED BoundedHolds, FrontProgress |
| SendBeforeEnd (mutant) | a front under custody is started | RED NoSendBeforeEnd |
| DropCustody, QueueCancelDropsCustody (mutants) | retention or a cancel drops the journal | RED CustodyKept |
| CustodyTimer (mutant) | an empty custody keeps a timer | RED QuietWhenEmpty |
| SharedBudget (mutant) | one count per journal | RED OwnBudget, QuietWhenEmpty |
| NoRelease (mutant) | a left channel's ended journal is kept | RED EndedLeftDelivers |
| AlwaysLeft, ReleaseUnended (mutants) | a journal is dropped too early | RED OnlyEndedLeftReleased |
