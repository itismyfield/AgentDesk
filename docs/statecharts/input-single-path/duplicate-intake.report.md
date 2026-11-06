# Duplicate Discord intake

Code baseline: `0bf73d0e2f6a83f7501fe9f8d555282e0e5d1c6a`.
`InputOwnershipDuplicateIntake` extends the indexed admission model with two distinct messages,
independent live/catch-up arrivals in either order, owner delivery, and one restart. Actor commits
are serialized; restart preserves the existing queue snapshot and receipts. No new storage or key
is assumed. Delivery is the owner's confirmed local submission, not a completed AI response.

The earlier admission/reset models require live admission before catch-up and keep one original;
they do not explore a retry after that original was delivered and removed. This model adds that
boundary. The paths rebuild different canonical originals (live carries a raw reply-context
envelope, catch-up does not), but derive the same existing Discord ticket.

| Configuration | TLC result |
|---|---|
| `InputOwnershipDuplicateIntakeBefore.cfg` | Expected exit 12, `NoFalseWarning`: 76 generated / 40 distinct states |
| `InputOwnershipDuplicateIntake.cfg` | PASS, exit 0: 400 generated / 98 distinct states; all safety and delivery liveness rules hold |

The counterexample is checked against code, step by step:

1. `Receive(catchUp, A)` derives a ticket in `queue_io/input_owner/ingress.rs::discord_ticket`,
   then `Original::admit_first` commits through `v2_resume_ticket` and `Custody::advance_ticket`.
2. `Deliver(A)` settles the owner's input. `Owner::ticket_original` scans queued/head/retained
   originals only; the admission receipt remains in `SnapshotV2::admission_receipts`.
3. `Receive(live, A)` cannot recover a stored original. `v2_resume_ticket` uses the newly built
   original and `advance_ticket` rejects its different canonical digest before checking sources.
   The queue seam exposes it as persistence failure; the live queue transaction posts the
   queue-storage warning. Catch-up itself only logs/retries failures, so its receive action does
   not increment channel warnings. This is the only counterexample promoted to tests.

The repair checks committed status under the same derived ticket after failed resumption. Pending
is never accepted by this check. The model's `CommittedRetry` represents that existing receipt
lookup, not a new receipt or dedup identity. Both messages must be delivered once under weak
fairness of each Receive and Deliver action; restart cannot erase custody. The model abstracts
failed filesystem operations, AI output and intentional cancellation; entry tests cover real
persistence failure separately.

Run from this directory with Java 21 and TLC 1.7.4:

```sh
java -Xmx512m -XX:+UseParallelGC -jar /home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar -config InputOwnershipDuplicateIntakeBefore.cfg InputOwnershipDuplicateIntake.tla
java -Xmx512m -XX:+UseParallelGC -jar /home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar -config InputOwnershipDuplicateIntake.cfg InputOwnershipDuplicateIntake.tla
```

Raw logs: `/home/adk/adk/lane-runs/i7857/logs/tlc-before.log` and `tlc-fixed.log`.
