# Queue reset and owner admission

`InputOwnershipReset` extends the existing admission model. Its scope is an input kept pending
for retry, outside an intentional server purge. The owner has adopted a V2 snapshot at boot.
The fixed reset uses the serialized owner API, or preserves a Herdr session after proving its
queues empty. A reset cannot update the disk snapshot without the owner observing that write.

| Configuration | TLC result |
|---|---|
| `InputOwnershipReset.cfg` | PASS, exit 0: 9 generated / 7 distinct states; both invariants and liveness hold |
| `InputOwnershipResetRawWriter.cfg` | Expected exit 12: third-party write invariant violated by `[]`; 2 distinct states |
| `InputOwnershipResetCatchUpRejected.cfg` | Expected exit 13: live CAS rejection → catch-up CAS rejection → stuttering; 4 distinct states |

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

Checked on WSL with Java 21 and `/home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar`
(TLC 2.19). All three outcomes match their contracts; the negative runs reach their intended
invariant/liveness counterexamples. The raw logs are in `lane-runs/i7835/logs/tlc-*.log`.
