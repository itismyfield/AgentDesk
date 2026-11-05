# Queue reset and owner admission

`InputOwnershipReset` extends the existing admission model. Its scope is an input kept pending
for retry, outside an intentional server purge. The owner has adopted a V2 snapshot at boot.
The fixed reset uses the serialized owner API, or preserves a Herdr session after proving its
queues empty. A reset cannot update the disk snapshot without the owner observing that write.

| Configuration | Expected result |
|---|---|
| `InputOwnershipReset.cfg` | No third-party queue writes; the pending input commits exactly once |
| `InputOwnershipResetRawWriter.cfg` | Invariant violation: external reset writes `[]` |
| `InputOwnershipResetCatchUpRejected.cfg` | Liveness violation: external reset → live CAS rejection → catch-up CAS rejection → stuttering |

The liveness property assumes reset, live intake and catch-up are weakly fair. It promises
admission, not provider execution or a Discord answer. The model retains the snapshot CAS guard.
`AtMostOneAdmission` checks one ticket and one original; `PendingEventuallyAdmittedOnce` checks
that a pending input eventually has both, instead of treating repeated CAS rejection as progress.

Run each configuration from this directory with:

```sh
java -jar <tla2tools.jar> -config InputOwnershipReset.cfg InputOwnershipReset.tla
java -jar <tla2tools.jar> -config InputOwnershipResetRawWriter.cfg InputOwnershipReset.tla
java -jar <tla2tools.jar> -config InputOwnershipResetCatchUpRejected.cfg InputOwnershipReset.tla
```

SC_NOT_RUN(WSL 도구 없음): Java is available, but no tla2tools jar is installed in the lane.
