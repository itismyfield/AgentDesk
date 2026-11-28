# Channel home — per-channel execution node (#8486 design, revision 3)

Code SHA: `5f0f2803e8` (Before, As7336). Design, DesignClear and Thread describe the #8486 design, which is not implemented yet.
Model: [ChannelHome.tla](ChannelHome.tla). One O channel, two nodes: D is the node the setting names, G holds the Discord gateway lease and runs the channel when D cannot. [GatewayLease.tla](GatewayLease.tla) covers who holds the lease; this model takes exactly one lease holder as given.

## Rules

- `SingleWriter` (INVARIANT): at most one node can put the channel's answer on Discord at any moment.
- `NoDuplicate` (INVARIANT): no input is answered twice, and none is answered once it was withheld.
- `Conservation` (INVARIANT): every received input that is neither answered nor noticed is still held: in the gateway queue or custody, as the turn of a node, or as a notice reserved in the outbox. Received = answered + noticed + in progress.
- `NoRefusal` (INVARIANT): the user sees no refusal for an input that a non-home node received.
- `BusyTurnFinishes` (INVARIANT): a turn whose owner answered Busy is never cut by a move.
- `CurrentStore` (INVARIANT): a node that may post for a managed channel writes from a store of the current owner tenure.
- `FirstInputOnHome` (INVARIANT): an input that arrived under a setting that did not change before it started runs on the intended node (the designated node while its heartbeat is fresh, else the gateway). One misplaced first input is enough to break it.
- `NoSelfCleanup` (INVARIANT): a move's cleanup never ends the session of the node the plan now keeps.
- `Completes` (PROPERTY): every received input is eventually answered or delivered a notice, also when no further message comes. A withheld input counts only after the outbox sends its notice.
- `Drains` (PROPERTY): every input leaves the gateway's queue.
- `HeldResolves` (PROPERTY): no input stays held forever.
- `ServesOnExpected` (PROPERTY): once restarts and edits stop, every turn that runs, runs on the expected home.
- `OldSessionEnds` (PROPERTY): a session of an older generation ends while its node is up.

The designation is `none`, `G` or `D`; it starts as `none`. Only configs with `Removal = TRUE` (DesignRemoval, NoneHold) may edit it back to `none`, which keeps the other configs inside the CI budget. `none` is no designation: an unmanaged channel stays on today's path and a managed one falls back to the gateway. Restarts of D (`MaxFlaps`), config edits (`MaxEdits`), `/clear` on the home (`MaxClears`) and new messages are the environment and carry no fairness. Every tick, hand-over, started turn and notice delivery is weakly fair.

## Results (TLC 1.7.4, 4 workers)

| cfg | Mode, bounds (flaps/edits/clears) | states | safety | liveness |
|---|---|---:|---|---|
| ChannelHomeBefore | `before` — main, 1/2/1 | 6,134 | RED `NoRefusal`, `FirstInputOnHome` | RED `ServesOnExpected` |
| ChannelHomeAs7336 | `as7336` — the #7336 path switched on as it is, 0/2/0 | 9,204 | RED `Conservation`, `NoRefusal` | RED `Completes`, `Drains`, `OldSessionEnds` |
| ChannelHomeDesign | `design`, 1/1/0 | 59,601 | GREEN | GREEN |
| ChannelHomeDesignClear | `design`, 0/2/1 | 49,969 | GREEN | GREEN |
| ChannelHomeDesignRemoval | `design`, 0/2/0, edits may remove the designation | 28,668 | GREEN | GREEN |
| ChannelHomeThread | `design`, thread child, 1/1/0 | 66,447 | GREEN | GREEN |
| ChannelHomeNoFence | mutation: post on the cached permit, 0/2/0, invariants only | 18,505 | RED `SingleWriter`, `NoDuplicate`, `CurrentStore` | not checked |
| ChannelHomeLocalCfg | mutation: each node judges the home from its own file, 0/3/0, invariants only | 752,638 | RED `SingleWriter`, `NoDuplicate`, `Conservation`, `CurrentStore`, `FirstInputOnHome` | not checked |
| ChannelHomeNoWait | mutation: the old owner ends a running turn instead of answering Busy, 0/2/0, invariants only | 19,393 | RED `BusyTurnFinishes` | not checked |
| ChannelHomeDrop | mutation: an admitted input leaves custody with no turn and no notice, 0/1/0 | 3,349 | RED `Conservation` | RED `Completes` |
| ChannelHomeNoAdoptNative | mutation: cleanup and permit never see the adopted native, 0/2/0 | 8,673 | holds | RED `Completes`, `Drains`, `OldSessionEnds` |
| ChannelHomeNoRenew | mutation: no new generation after the owner's own session ended, 0/1/1 | 2,811 | holds | RED `Completes`, `Drains` |
| ChannelHomeReuseStore | mutation: a writer reuses a store of an older tenure, 0/2/0 | 18,273 | RED `CurrentStore` | holds |
| ChannelHomeRawOnly | mutation: a held input is re-planned only on a raw revision change, 0/2/0 | 15,405 | holds | RED `Completes`, `Drains`, `HeldResolves` |
| ChannelHomeThreadNoInherit | mutation: a thread reads its own designation, 0/2/0 | 875 | RED `FirstInputOnHome` | RED `ServesOnExpected` |
| ChannelHomeThreadNoChild | mutation: D never records the thread child, 0/2/0 | 4,952 | holds | RED `Completes`, `Drains` |
| ChannelHomeIndexLag | mutation: admission reads a designation index that lags the PG row, 0/1/0 | 2,625 | RED `FirstInputOnHome` | holds |
| ChannelHomeNoNotice | mutation: a started turn is withheld with no notice reserved, 0/2/0 | 13,855 | RED `Conservation` | RED `Completes` |
| ChannelHomeNoCancel | mutation: a plan that returns to the owner keeps the move's cleanup, 0/2/0, invariants only | 16,832 | RED `NoSelfCleanup` | not checked |
| ChannelHomeNoneHold | mutation: a held input gets no plan once the designation is removed, 0/2/0, edits may remove | 27,510 | holds | RED `Completes`, `Drains`, `HeldResolves` |
| ChannelHomeRenewRace | mutation: renew during a move's cleanup, plan judged without the owner generation, 0/1/0 | 1,653 | holds | RED `Completes`, `Drains`, `HeldResolves` |

LocalCfg has 752,638 states, so its properties would not fit the 300 s CI budget per config; it keeps the invariants it exists for. NoFence, NoWait and NoCancel are safety mutations and also run their invariants only. Every mutation uses the fewest edits that still break its rule; the CI check runs one TLC per rule on one worker. Its edits switch between the two nodes only, as `none` adds nothing to a disagreement between two files. Design, DesignClear, DesignRemoval and Thread are over the 500-state chart budget and have no chart. Each RED config has a bug-path chart named `channel-home-<mode>.chart.mmd`.

## Changes from revision 2

| cfg | revision 2 | revision 3 | why |
|---|---|---|---|
| Before | RED `NoRefusal` / `ServesOnExpected` | adds `FirstInputOnHome` | The sticky gateway session runs the first input after a designation; the new rule catches it once. |
| As7336 | 1/2/1, 237,015 states | 0/2/0, same rules RED | At 1/2/1 the bigger model took 325 s. The same five rules break without a restart or `/clear`. |
| Design, DesignClear, Thread | GREEN | GREEN with the new rules, plus DesignRemoval | Thread drops `/clear` (DesignClear has it): 1/1/1 took 131 s. DesignRemoval covers removing a designation and the held return G → D → G. |
| LocalCfg | RED four rules / `Completes` | RED five rules, invariants only | See above. |
| ReuseStore, RawOnly | 0/3/0 | 0/2/0 | Three designation values make G → D → G two edits. |
| Drop, NoRenew | 0/2/x | 0/1/x | One edit is enough for the drop and the clear. |
| NoFence, NoWait | properties hold | invariants only | Their rules are safety rules; the properties cost CI time and showed nothing. |
| RawOnly, ThreadNoInherit | as before | add `HeldResolves`, `FirstInputOnHome` | The new rules also catch these. |
| IndexLag, NoNotice, NoCancel, NoneHold, RenewRace | — | new, all RED | One per review finding: first-input misplacement, notice with no delivery, return during a move, removed designation, renew against a move. |

Model changes: the designation can be removed (`none`); a withheld input reserves its notice in the same transaction and only the outbox's `SendNotice` makes it noticed; `FirstInputOnHome` checks each started input; a renew by the same owner keeps its store (only a change of executor retires it); a renew waits while a move's cleanup owns the generation, and a plan is re-made when its owner generation is gone; a plan that returns to the current owner cancels the move's cleanup; with no designation a managed channel's held input is planned to the gateway.

## Counterexamples and the code

| cfg / rule | Path | Code check |
|---|---|---|
| Before / `NoRefusal`, `FirstInputOnHome`, `ServesOnExpected` | With a live gateway session every later input stays on it; with none the O channel refuses. | `intake_router_hook.rs:334-345`, `:589-597`. |
| As7336 | The gateway's O hold check refuses before routing; the adopted native is never ended. | `intake_dispatch.rs:294-312`, `cutover.rs:416-420`, `owner_record/cleanup.rs:74-75`. |
| IndexLag / `FirstInputOnHome` | The PG row says D; admission reads a stale index, routes the first input to the gateway's session. | The design reads the PG row at admission; a lagging index is the revision 1 design. |
| NoNotice / `Conservation`, `Completes` | The owner goes offline mid-turn; the move withholds the receipt and reserves no notice. | `db/replay_disposition/write.rs:209-219` changes the receipt only; `owner_record/plan.rs:191-218` reserves the outbox row in the same transaction. |
| NoCancel / `NoSelfCleanup` | A move G → D asks G to end; the setting returns to G; the re-plan keeps the cleanup; G's own session is ended. | `owner_record/plan.rs:118-139` re-plans held rows, nothing clears `routing_cleanup`; `owner_record/cleanup.rs:90-92` keeps the work due. |
| NoneHold / `HeldResolves` | An input held for D; the designation is removed; no plan is made, the input stays held. | `owner_record/plan.rs:26-36` (`build` returns no plan without a target and fails without a home), `:118-120`. |
| RenewRace / `HeldResolves` | The move's cleanup ends G's native; G's refresh renews before the transfer; the cleanup's generation is gone and the plan still looks valid. | `owner_record.rs:363-449` moves the generation without the cleanup; `owner_record/cleanup.rs:216` selects only rows waiting on a cleanup owner's generation. |
| Other mutations | As in revision 2. | See the design report. |

## Not modeled

- Unknown availability (no or stale readiness evidence), lease movement, Herdr consent order, provider selectors, the O store's cursor, thread child registration order, more than one channel and attachments. The design covers them with integration tests.
- A machine power-off that also kills the native session; `DDown` keeps the session, the worse case for a duplicate answer.
- `Expire` may fire as soon as a request is pending; real code waits 12 s.

## Reproduce

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/gateway-lease
~/.claude/skills/statechart/scripts/statechart ChannelHome.tla --config ChannelHomeNoNotice.cfg --view channel-home.view.mjs --out /tmp/sc-channel-home-nonotice
```
