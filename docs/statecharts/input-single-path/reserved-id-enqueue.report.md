# Reserved id enqueue and owner return

Model base and review base: `0b9af5833990e6ee41d3428fa2d37ba5289c22e9`, including
#8077 `08d5ac88be45e5e1af7d46696c69b2ee02def1b6`. Native execution evidence is
recorded in `/home/adk/adk/lane-runs/i8091/report-4.md` for the containing commit.
Held custody, existing restore calls and wake policy retain their existing behavior.

## Contract and model

An ordinary receipt cannot enqueue a primary or absorbed id the actor has already
reserved. The pre-hydrate actor guard returns `SourceIdPendingOrActive` without
changing the queue. A receipt refusal does not abandon another caller's reservation.
A legacy queued start that loses its claim instead hands its payload back to
`QueuedRestoreOwner`, which already holds the original and the dispatch lease.

The owner uses the existing actor restore command with marker preservation enabled.
The actor checks the current lease with `Arc::ptr_eq`, persists the queue before
clearing the reservation, and keeps the marker. A failed write leaves the old
reservation and marker. Existing callers use the old restore mode. Return wakeup
uses the existing race-loss idle recheck; no timer or operator control was added.

See [chart](reserved-id-enqueue.chart.mmd) and [model](ReservedIdEnqueue.tla).
One original id is sufficient for the counterexample. `Take` combines dequeue,
durable marker and reservation in one actor step. `Replay` represents an ordinary
receipt, the former return enqueue, or the authorized atomic return in Fixed.
`Release` is either a claim (which purges same-id copies) or the old abandon.
`AfterRelease` optionally supplies a fresh external receipt, which is a control
and is not assumed to repair a lost return.

`NoReservationQueueOverlap`, `AtMostOneExecution` and `NoLostReturn` are invariants.
`EventuallyExecuted` plus `AtMostOneExecution` requires exactly one execution.
Weak fairness applies to every transition. `OwnerReturn` represents possession of
the current lease; the model does not simulate forged leases. Other ids, merged
payloads, crashes, cancellation, Held and wake scheduling are outside this small
model. Native tests cover real lease checks, merged source identity and receipts
interleaving with return; this model is not a runtime reproduction.

| Config | Expected and observed failures | Meaning |
|---|---|---|
| Before | `NoReservationQueueOverlap` | Old return enqueue leaves the same id in both places. |
| GuardOnly | `NoLostReturn`, `EventuallyExecuted` | A guard alone refuses return, after which the old caller abandons custody. |
| Fixed | none | The guard and authorized atomic return preserve custody and execute once. |
| Live | none | A duplicate receipt is refused while the original proceeds to claim. |
| Released | none | A fresh receipt after release is accepted and executes once. |

TLC 1.7.4: 5 configs, 12 TLC runs and 5 fairness prechecks; expected violation sets
match 5/5. Logs: `logs/tlc-summary-2.json`, `logs/tlc-run2-*.log` under the lane run.
Modeling finished within the 20-minute stop line. GuardOnly remains an expected
RED configuration rather than describing the fixed production path.

## Entry-point inventory

Paths below are relative to `src/services/`. Raw `git grep` output is retained in
`/home/adk/adk/lane-runs/i8091/logs/entrypoints-3.txt`. Calls inside test modules
are omitted: `foreign_row.rs:90`, `catch_up.rs:631`, `queue_effects.rs:320` and
`discord/mod.rs:2273` begin relevant test modules.

| Production code coordinate | Model action / role |
|---|---|
| `discord/catch_up/phase1_sweep.rs:689`, `phase2_sweep.rs:458` | Replay through the catch-up API. |
| `discord/catch_up/api.rs:95` | Replay with the classifying snapshot's claim observation. |
| `discord/health/turn_deliver.rs:373` | Ordinary receipt through the shared adapter. |
| `discord/mod.rs:2071` | Replay of the same original id for internal history recovery. |
| `discord/router/intake_dispatch.rs:930` | Deferred live input, including absorbed sources; refusal leaves checkpoint unread. |
| `discord/router/intake_gate/queue_effects.rs:121` | Replay of a soft intake input. |
| `discord/router/message_handler/intake_turn/race_loss.rs:51,62` | Ordinary receipt; refusal no longer abandons the pending owner. |
| `discord/router/message_handler/intake_turn.rs:1081` | A legacy queued claim loser hands uploads to its existing restore owner. |
| `discord/router/intake_dispatch/queued.rs:361`, `queued/race_loss.rs:30` | Authorized owner return; reuse original and lease, then existing race-loss wake. |
| `discord/mod.rs:1635`, `queue_io/turn_admission.rs:312` | Shared adapters preserving the refusal reason. |
| `turn_orchestrator/registry_purge.rs:260,270` | Enqueue through resolved actor; retry only a closed actor. |
| `turn_orchestrator/claim_observation.rs:214` | Send the actor Enqueue command. |
| `turn_orchestrator.rs:2081,2092`, `claim_observation.rs:176` | Replay guard before hydrate; refuse a reserved primary or absorbed id. |
| `turn_orchestrator/queue_enqueue.rs:25` | Existing queued-source duplicate check. |
| `turn_orchestrator/queued_head/v2/control.rs:69,86` | Separate V2 enqueue; legacy Take is withheld, outside this reservation model. |
| `turn_orchestrator.rs:2279`, `dispatch_reservation.rs:38,49` | Take: the only production setter of `pending_user_dispatch`. |
| `turn_orchestrator.rs:2002`, `dispatch_reservation.rs:90,112` | Release by matching claim; absorbed ids become active. Completion follows claim, with no remaining reservation. |
| `turn_orchestrator/front_requeue.rs:69,101,106,129,144,152,186` | Authorized return: lease checked, queue persisted, reservation cleared. New return preserves marker; old restore keeps its prior settlement. |
| `discord/queue_dispatch.rs:487,501` | Existing authorized restore adapter for admission defer, transition busy, promoted busy and dispatch errors. |
| `turn_orchestrator.rs:2320`, `dispatch_reservation.rs:234,241` | Release by explicit abandon or reservation-only clear. |
| `turn_orchestrator/dispatch_cleanup.rs:8,17,24,34,41,50` | Abandon APIs choose marker consumption; ordinary abandon has no lease requirement. |
| `turn_orchestrator.rs:2182`, `dispatch_reservation.rs:256,274` | Release orphan reservation before marker self-heal. |
| `turn_orchestrator/inbound_order.rs:105,114,160` | Existing stalled-reservation valves. |
| `turn_orchestrator/clear_channel.rs:210`, `checked_purge.rs:55` | Explicit clear/purge, excluded from eventual-execution assumptions. |

## Confirmed counterexample and preserved boundaries

At the base, `intake_turn/race_loss.rs:51` enqueues a returned head, but `:79`
also abandons its reservation and marker after a non-persistence refusal.
Adding only the actor guard would leave no queued copy or owned execution.
`race_loss.rs` at that base is byte-identical to main
`4f30c67d9a48eacad96a99faff9fd442d0d4dd3c` (SHA256
`84d42052b99b0345b15881ac14f61de7e7981bc46c79f6ca66128a8949c81b52`).
This is a code-confirmed return contract, not a claim of an observed production loss.

The existing catch-up known-id union (`recovery_known_ids.rs:98`), queue backstop
(`queue_io/backstop.rs:152`) and injection race check
(`health/turn_deliver/inject/catch_up_race.rs:24`) remain. The latter two control
wakeup/injection rather than only enqueue admission, so they are not redundant
with the new guard. No cleanup authority, retry timer, format or Held transition
was added or removed.
