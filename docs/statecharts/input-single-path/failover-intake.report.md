# Failover intake across a gateway handback

Code baseline: `6aa89c408cd365220f843e908820ecf95daf14f5`, fixed at the commit that adds this report.
`InputOwnershipFailoverIntake` follows one Discord message across gateway handbacks between a
standby and the preferred node. Each node keeps its own V2 queue file (the lease handback keeps
it, `runtime_bootstrap/gateway_lease.rs` self-fence), and each node's catch-up dedupes only on its
own queue file and ledgers. The only cross-node evidence is the `turns` receipt that a node's turn
records at its terminal (`discord:{channel}:{message}`, `db/turns.rs::record_turn_receipt`).
A delivery is one provider run of the front; its terminal is modelled in the same step.

| Configuration | Constants | TLC result |
|---|---|---|
| `InputOwnershipFailoverIntakeBefore.cfg` | Fix=FALSE, StandbyRuns=TRUE, Order=measured | Expected exit 12, `NoCrossNodeRedelivery`: 58 generated / 25 distinct states |
| `InputOwnershipFailoverIntakeBeforeReverse.cfg` | Fix=FALSE, StandbyRuns=TRUE, Order=any | Expected exit 12, `NoCrossNodeRedelivery`: 79 generated / 30 distinct states |
| `InputOwnershipFailoverIntake.cfg` | Fix=TRUE, StandbyRuns=TRUE, Order=any | PASS, exit 0: 105 generated / 36 distinct states |
| `InputOwnershipFailoverIntakeIncident.cfg` | Fix=FALSE, StandbyRuns=FALSE, Order=measured | PASS, exit 0: 55 generated / 24 distinct states |

`StandbyRuns=FALSE` is the 11-09 setting: the standby is not the O home, so
`tui_o/cutover.rs::claim_for_placement` holds its start (`Site::Foreign`) and the kept input never
runs there; that is why the measured handback answered once. `StandbyRuns=TRUE` is the same
handback once that hold no longer applies (the O home moved, or a channel outside the O
selection), with no completed-turn check in between.

The measured counterexample (`Before`) is checked against code, step by step:

1. `Arrive`, `CatchUp(standby)`: the standby acquired the lease after the message posted;
   `runtime_bootstrap/recovery_flush.rs` runs catch-up first, `queue_io/input_owner/ingress.rs::enqueue`
   admits it into the standby's V2 file.
2. `Handback`: operator release self-fences the standby; the V2 file stays on disk.
3. `CatchUp(preferred)`, `Deliver(preferred)`: the preferred node's catch-up does not see the
   standby's file and queues the message; its turn answers and records the `completed` receipt.
4. `Handback`, `Deliver(standby)`: the standby's next lease re-adopts the file
   (`runtime_bootstrap/queued_recovery.rs` -> `owner_input.rs::recover`) and, before the fix,
   `router/intake_dispatch/queued.rs::start_owner_front` admits it with no completed-turn check.

`BeforeReverse` is the same gap in the other order: the standby answers first, then the preferred
node's catch-up, whose ledgers are local, queues and runs it again.

The fix is `RetireSettledElsewhere`: `start_owner_front` reads `db/turns.rs::turn_completed` for
the front's `discord_turn_id` before admission and, on a `completed` receipt, removes the front
through `OwnerTurn::complete_locally` without a turn. A front carrying merged followers, a
cancelled/failed/empty receipt, no receipt, or a failed read starts as before; the model does not
retire without a receipt (`RetiredOnlyAfterCompletion`). The model abstracts a turn still running
on another node at handback (its receipt is not yet written; out of scope here), merged
followers, and the queue notice count. Tests:
`router/message_handler/provider_dispatch/owner_input_settled_front_tests.rs`.

Run from this directory with TLC 1.7.4:

```sh
java -Xmx512m -XX:+UseParallelGC -jar ~/.local/share/tla2tools/tla2tools-1.7.4.jar -config InputOwnershipFailoverIntakeBefore.cfg InputOwnershipFailoverIntake.tla
java -Xmx512m -XX:+UseParallelGC -jar ~/.local/share/tla2tools/tla2tools-1.7.4.jar -config InputOwnershipFailoverIntake.cfg InputOwnershipFailoverIntake.tla
```

The Mermaid chart is not committed: the `statechart` renderer was not available to this lane.
