# Antigravity durable custody across a restart (#8274 4a)

Model: `WriterAdoption.tla` (`AgyCustodySpec`, `AgyOwnerSpec`). Base `6ac2175a7f` + reader `d67613229f`.
TLC 1.7.4 through `scripts/ci/statecharts_tlc.py`. Before = the same model with the fix removed.

## What it checks

- The inflight `agy_custody` route tells an AGY admission from a Legacy one before any spool
  exists. `end_authority` reads it: absent is Legacy, present is AGY, unreadable or inconsistent is
  Held. Every generic end (boot reaper, generation invalidation, `settle_boot`, the sweeper) skips a
  row that is not Legacy.
- The producer stage is what the spool proves: no slot, no checkpoint, open only, launch recorded
  (the gate keeps input from running), started with a durable identity, or sealed. After a restart
  no writer is left, so a live orphan never seals: the owner kills its group. A probe error is held
  visibly and retried; abandon needs proof that input never ran or that its producer is gone.
- Boot runs in the real eight steps; the owner acts after observe and keeps re-judging.
- `AgyOwnerSpec`: new input waits until an existing turn's end is settled.

## Results

| cfg | before | after |
|---|---|---|
| WriterAgyCustody | RED `AgyCustodyResolves`, `HeldVisible`, `ProbeErrorVisible` (no end authority; or one-shot owner) | GREEN |
| WriterAgyOwnerCutover, WriterAgyOwnerShadow | RED `NoNewInputWithExistingDebt` | GREEN |
| WriterAgyCustodyLegacyAsAgy | — | RED `LegacyRecoveryKept` (+`LegacyResolves`) |
| WriterAgyCustodyGenericSettle | — | RED `AgyEndOnlyByOwner` (+liveness) |
| WriterAgyCustodyUnprovenLost | — | RED `NoAbandonUnproven` |
| WriterAgyCustodyNoLaunchGate | — | RED `NoAbandonUnproven` |
| WriterAgyCustodyLegacyFallback | — | RED `NoAbandonUnproven` |
| WriterAgyCustodyOneShot | — | RED `AgyCustodyResolves` (+`HeldVisible`, `ProbeErrorVisible`) |
| WriterAgyOwnerInputWithDebt | — | RED `NoNewInputWithExistingDebt` |

All 70 `WriterAdoption` configs match `expected.tsv`.

## Assumptions and limits

Strong fairness on a successful group kill and weak fairness on a retried probe: a kill that keeps
failing stays a visible hold. Suppression on live paths (normal end cleanup, stop, `/clear`) is not
modelled; slice 4b owns it.
