# Dormant delivery obligation schema

The obligation ledger belongs to the existing delivery record. This slice adds
types only: no production caller, writer, lease, owner, deduper or sender changes.
`LEDGER_PROTOCOL` stays at 0 until every required writer and consumer is wired.
The serde/read-only loader slice follows locally using the same fixture at
`tests/fixtures/delivery_obligation/ledger.json`.

## Publication and identity

The record's optional `obligation_ledger` contains one required `publication`
object: source epoch, protected `extent_end`, 16-byte prefix digest and `rev`.
The extent covers the frontier, every held whole commit, every open range and
every intent; it never shrinks within an epoch. Digest calculation and publication
must recheck the source/coord token under the existing record flock and coord
mutex. The digest checks continuity of `[0, extent_end)`, not delivery success.
`SourceObs` carries the publication captured before hashing with its measured
digest, allowing the loader to reject a changed epoch, extent, digest or revision.

Only the absence of both frontier `source_dev` and `source_ino` means legacy.
One missing field is `IdentityIncomplete`. Serialization omits absent identity
fields. Registration, receipt append, withdrawal and equal/lower-END commits
preserve that absence. Only a greater-END whole commit can bind the frontier,
using the admission token after rechecking it against the current source.

The loader must reject unknown schema/protocol versions and unknown ledger
variants without rewriting the record. Fence presence is a deny-only input;
its revision is diagnostic and need not equal the ledger revision. Missing fence
for an active ledger requires repair; empty ledger with a fence is IncompleteClear.
The read-only loader reports those states without repairing either file.

## Lease and settlement

`FlightHandoff` is owned by an existing lease guard. Its states distinguish
Admitted (including whether intent was written), Issuing, Settled and Released.
The eventual Issuing transition and removal from `fresh` share one synchronous
coord critical section. Release hands off before releasing the lease, ignores
stale tokens and performs no await, I/O or detached task. Settled and Released
never recreate Unknown work. Controller, sink and no-anchor recovery guards
must all use this contract when wired.

`u_rev` is a separate in-memory safety revision. Pending, coord generation,
unavailable and reset-pending changes increment it immediately under the coord
mutex, including failed publications. U changes increment it at successful
publication. Neither an unchanged revision nor a blocked state permits discard
without rechecking U and pending under that same mutex.

Protocol 1 conservatively keeps ambiguous errors, including HTTP 4xx, Unknown.
It enables neither redrive nor operator settlement. Its intent withdrawal is
limited to pre-POST cancellation and write-ahead failure. Protocol 2 typed
NotIssued/FirstRejected outcomes can prove non-delivery; MaybePosted cannot.
WholeProof requires all planned chunks for the same epoch/attempt, no cleanup,
and an unchanged epoch/attempt/revision at settlement. ChunkProof and Ambiguous
remain unresolved. No operator retry permit exists.

## Durable diagnostics

Attempt preparation time is `prepared_at_ms`, recorded at write-ahead; it does
not claim transport was issued. Unobserved timestamps and unknown chunk plans
stay absent. Receipts retain chunk numbers, message IDs and cleanup state.
`unresolved_since_ms` is recorded in the publication entering Unresolved.

**At the no-progress cap transition, emit `relay_obligation_redrive_capped`
immediately.** Persist `redrive_capped` on the open obligation in that transition:
its exact range, cap time, next rearm time and last rejection. The owning record
and publication epoch provide channel and source scope. This diagnostic grants
neither a retry nor settlement. Status must not create, normalize or repair it.
D1a and ON require cap -> immediate alert -> restart reconstruction -> rearm
event tests, including a row veto and preserved obligations. This type-only
slice does not emit alerts or implement that later runtime gate.

## Limits

The split is S0a-1 public dormant types, then S0a-2 serde, fence classification and
read-only load. No intermediate loader is activated. Record writer integration,
restart durability barriers, source digest I/O and all runtime transitions are
later slices. The fixture is a schema example, not permission to activate it.
