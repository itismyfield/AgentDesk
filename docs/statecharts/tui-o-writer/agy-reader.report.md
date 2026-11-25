# Antigravity reader: typed slots, evidence and recovery custody (#8274 reader)

Model: `WriterAdoption.tla` (`AgySpec`, `AgyOwnerSpec`). Base `6ac2175a7f`. TLC 1.7.4, run with
`scripts/ci/statecharts_tlc.py`. Before = the same model with the fix guards removed.

## What the reader adds

- `AgySpec`: an accepted iMessage spool is read only with durable evidence that names its
  destination, parent, id, nonce and digest (`AgyHttpReadable`). Without it the reader holds the
  slot (`readHeld`, spool kept). A PG row alone (`pgOnly`) proves the original, not the execution;
  slice 4 publishes the nonce binding (`AgyHttpBindingPublish`) and the reader re-reads
  (`AgyHttpReaderRecheck`). `HttpNoBinding` shows that End depends on that publication.
- `AgyOwnerSpec`: a damaged slot holds the destination, a deselected destination with an existing
  spool gets recovery-only custody that never admits new input, a producer lost before Done is ended
  once without a rerun, shadow takes no visible effect, and retirement waits for new input to end.

## Results

| cfg | before | after |
|---|---|---|
| WriterAgyHttpOn | RED `AgyHttpReadVerified`, `AgyHttpUnreadableHeld` | GREEN |
| WriterAgyOwnerShadow | RED `AgyDamagedNeverReady`, `AgyDamagedVisible`, `NoRetireWithNewSpool`, `ShadowNoVisible` | GREEN |
| WriterAgyOwnerCutover | RED `AgyDamagedNeverReady`, `AgyDamagedVisible`, `AgyExistingDebtResolves`, `NoRetireWithNewSpool` | GREEN |
| WriterAgyHttpReaderBlind | — | RED `AgyHttpReadVerified` (+`AgyHttpUnreadableHeld`) |
| WriterAgyHttpReaderDiscordOnly | — | RED `AgyHttpReadableEnds` (+`AgyAcceptedEnds`) |
| WriterAgyHttpReaderNoHold | — | RED `AgyHttpUnreadableHeld` (+`AgyAcceptedEnds`) |
| WriterAgyHttpNoBinding | — | RED `AgyAcceptedEnds` |
| WriterAgyOwnerSelectedOnly | — | RED `AgyExistingDebtResolves` |
| WriterAgyOwnerRecoveryAdmits | — | RED `AgyRecoveryNoNewSpool` |
| WriterAgyOwnerNoAbandon | — | RED `AgyExistingDebtResolves` |
| WriterAgyOwnerRerun | — | RED `AgyNoRerun` |
| WriterAgyOwnerSlotBlind | — | RED `AgyDamagedNeverReady` (+`AgyDamagedVisible`) |
| WriterAgyOwnerShadowEffect | — | RED `ShadowNoVisible` |
| WriterAgyOwnerAbandonLive | — | RED `AgyNoAbandonLive` |
| WriterAgyOwnerRetireWithInput | — | RED `NoRetireWithNewSpool` |

All 62 `WriterAdoption` configs match `expected.tsv`; the 50 earlier configs are unchanged.

## Limits

The model does not separate a spool-less admission, an open-only turn and a started producer
without a durable identity; slice 4a owns those states, the durable AGY custody predicate shared
by boot reaping and receipt settlement, and the nonce binding the reader waits for.
