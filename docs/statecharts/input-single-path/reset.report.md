# Queue reset and owner admission

`InputOwnershipReset` extends the existing admission model. Its scope is an input kept pending
for retry, outside an intentional server purge. The owner has adopted a V2 snapshot at boot.
The fixed reset uses the serialized owner API, or preserves a Herdr session after proving its
queues empty. A reset cannot update the disk snapshot without the owner observing that write.
`StartPending` keeps the live admission unconfirmed before reset, with the same stored ticket
and original. Only catch-up can first commit that input; later catch-ups keep its committed pair.

| Configuration | TLC result |
|---|---|
| `InputOwnershipReset.cfg` | PASS, exit 0: 11 generated / 7 distinct states; live first commit and catch-up retry |
| `InputOwnershipResetPending.cfg` | PASS, exit 0: 7 generated / 4 distinct states; Pending → reset preservation → first catch-up commit → retry |
| `InputOwnershipResetPendingCatchUpRejected.cfg` | Expected exit 13: always-rejected catch-up leaves the same Pending input uncommitted; 3 distinct states |
| `InputOwnershipResetRawWriter.cfg` | Expected exit 12: third-party write invariant violated by `[]`; 2 distinct states |
| `InputOwnershipResetCatchUpRejected.cfg` | Expected exit 13: live CAS rejection → catch-up CAS rejection → stuttering; 4 distinct states |

The liveness property assumes reset, live intake and catch-up are weakly fair. It promises
admission, not provider execution or a Discord answer. The model retains the snapshot CAS guard.
`AtMostOneAdmission` checks one ticket and one original; `PendingEventuallyAdmittedOnce` checks
that a pending input eventually has both, instead of treating repeated CAS rejection as progress.
`PendingOriginalKept` and `AtMostOneAdmission` preserve the stored pair and bind its committed
copy. Rejecting every catch-up was a false green in the earlier live-only config; the Pending
config now makes that mutation a liveness counterexample.

Run each configuration from this directory with:

```sh
java -jar <tla2tools.jar> -config InputOwnershipReset.cfg InputOwnershipReset.tla
java -jar <tla2tools.jar> -config InputOwnershipResetPending.cfg InputOwnershipReset.tla
java -jar <tla2tools.jar> -config InputOwnershipResetPendingCatchUpRejected.cfg InputOwnershipReset.tla
java -jar <tla2tools.jar> -config InputOwnershipResetRawWriter.cfg InputOwnershipReset.tla
java -jar <tla2tools.jar> -config InputOwnershipResetCatchUpRejected.cfg InputOwnershipReset.tla
```

Checked on WSL with Java 21 and `/home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar`
(TLC 2.19). All five outcomes match their contracts and are registered in `../expected.tsv`;
the negative runs reach their intended
invariant/liveness counterexamples. The raw logs are in `lane-runs/i7835/logs/tlc-*.log`.
