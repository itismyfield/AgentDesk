# Channel home — per-channel execution node (#8486 design, revision 4)

Code SHA: `826a01da0e` (Before, As7336; their paths are unchanged since `5f0f2803e8`). Design, DesignClear, DesignRemoval and Thread describe the #8486 design, which is not implemented yet.
Model: [ChannelHome.tla](ChannelHome.tla). One O channel, two nodes: D is the node the setting names, G holds the Discord gateway lease and runs the channel when D cannot. [GatewayLease.tla](GatewayLease.tla) covers who holds the lease; this model takes exactly one lease holder as given.

## Rules

- `SingleWriter` (INVARIANT): at most one node can put the channel's answer on Discord at any moment.
- `NoDuplicate` (INVARIANT): no input is answered twice, and none is answered once it was withheld.
- `Conservation` (INVARIANT): every received input that is neither answered nor noticed is still held: in the gateway queue or custody, as the turn of a node, or as a notice reserved in the outbox. Received = answered + noticed + in progress.
- `EffectOnce` (INVARIANT): each input has at most one delivered effect (a posted answer or a notice the outbox sent), and an input marked answered or noticed has exactly one.
- `ClearHonored` (INVARIANT): an input that was waiting when the user ran `/clear` never starts.
- `NoRefusal` (INVARIANT): the user sees no refusal for an input that a non-home node received.
- `BusyTurnFinishes` (INVARIANT): a turn whose owner answered Busy is never cut by a move.
- `CurrentStore` (INVARIANT): a node that may post for a managed channel writes from a store of the current owner tenure.
- `FirstInputOnHome` (INVARIANT): an input that arrived under a setting that did not change before it started runs on the intended node (the designated node while its heartbeat is fresh, else the gateway).
- `NoSelfCleanup` (INVARIANT): a move's cleanup never ends the session of the node that the plan or the current setting keeps.
- `Completes` (PROPERTY): every received input is eventually answered or delivered a notice, also when no further message comes.
- `Drains` (PROPERTY): every input leaves the gateway's queue.
- `HeldResolves` (PROPERTY): no input stays held forever.
- `CleanupResolves` (PROPERTY): no move's cleanup stays forever.
- `ServesOnExpected` (PROPERTY): once restarts and edits stop, every turn that runs, runs on the expected home.
- `OldSessionEnds` (PROPERTY): a session of an older generation ends while its node is up.

The designation is `none`, `G` or `D`; it starts as `none`. Only configs with `Removal = TRUE` (DesignRemoval, NoneHold) may edit it back to `none`. Restarts of D (`MaxFlaps`), config edits (`MaxEdits`), `/clear` (`MaxClears`) and new messages are the environment and carry no fairness. Every tick, hand-over, started turn and notice delivery is weakly fair.

## Results (TLC 1.7.4, 4 workers)

| cfg | Mode, bounds (flaps/edits/clears) | states | safety | liveness |
|---|---|---:|---|---|
| ChannelHomeBefore | `before` — main, 1/2/1 | 6,134 | RED `NoRefusal`, `FirstInputOnHome` | RED `ServesOnExpected` |
| ChannelHomeAs7336 | `as7336` — the #7336 path switched on as it is, 0/2/0 | 9,204 | RED `Conservation`, `NoRefusal` | RED `Completes`, `Drains`, `OldSessionEnds` |
| ChannelHomeDesign | `design`, 1/1/0 | 63,993 | GREEN | GREEN |
| ChannelHomeDesignClear | `design`, 0/1/2 | 24,749 | GREEN | GREEN |
| ChannelHomeDesignRemoval | `design`, 0/2/0, edits may remove the designation | 29,790 | GREEN | GREEN |
| ChannelHomeThread | `design`, thread child, 1/1/0 | 71,127 | GREEN | GREEN |
| ChannelHomeNoFence | mutation: post on the cached permit, 0/2/0, invariants only | 19,555 | RED `SingleWriter`, `NoDuplicate`, `CurrentStore`, `EffectOnce` | not checked |
| ChannelHomeLocalCfg | mutation: each node judges the home from its own file, 0/3/0, invariants only | 850,288 | RED `SingleWriter`, `NoDuplicate`, `Conservation`, `CurrentStore`, `FirstInputOnHome`, `EffectOnce` | not checked |
| ChannelHomeNoWait | mutation: the old owner ends a running turn instead of answering Busy, 0/2/0, invariants only | 20,443 | RED `BusyTurnFinishes` | not checked |
| ChannelHomeDrop | mutation: an admitted input leaves custody with no turn and no notice, 0/1/0 | 3,437 | RED `Conservation` | RED `Completes` |
| ChannelHomeNoAdoptNative | mutation: cleanup and permit never see the adopted native, 0/2/0 | 8,673 | holds | RED `Completes`, `Drains`, `OldSessionEnds` |
| ChannelHomeNoRenew | mutation: no new generation after the owner's own session ended, 0/1/1 | 8,313 | holds | RED `Completes`, `Drains` |
| ChannelHomeReuseStore | mutation: a writer reuses a store of an older tenure, 0/2/0 | 19,323 | RED `CurrentStore` | holds |
| ChannelHomeRawOnly | mutation: a held input is re-planned only on a raw revision change, 0/2/0 | 16,455 | holds | RED `Completes`, `Drains`, `HeldResolves`, `CleanupResolves` |
| ChannelHomeThreadNoInherit | mutation: a thread reads its own designation, 0/2/0 | 875 | RED `FirstInputOnHome` | RED `ServesOnExpected` |
| ChannelHomeThreadNoChild | mutation: D never records the thread child, 0/2/0 | 5,456 | holds | RED `Completes`, `Drains` |
| ChannelHomeIndexLag | mutation: admission reads a designation index that lags the PG row, 0/1/0 | 2,649 | RED `FirstInputOnHome` | holds |
| ChannelHomeNoNotice | mutation: a started turn is withheld with no notice reserved, 0/2/0 | 14,905 | RED `Conservation` | RED `Completes` |
| ChannelHomeNoCancel | mutation: a plan that returns to the owner keeps the move's cleanup, 0/2/0, invariants only | 17,906 | RED `NoSelfCleanup` | not checked |
| ChannelHomeNoneHold | mutation: a held input gets no plan once the designation is removed, 0/2/0, edits may remove | 28,608 | holds | RED `Completes`, `Drains`, `HeldResolves`, `CleanupResolves` |
| ChannelHomeRenewRace | mutation: renew during a move's cleanup, plan judged without the owner generation, 0/1/0 | 1,677 | holds | RED `Completes`, `Drains`, `HeldResolves`, `CleanupResolves` |
| ChannelHomeNoDelivery | mutation: a withheld input is marked noticed with no notice sent, 1/1/0, invariants only | 48,411 | RED `EffectOnce` | not checked |
| ChannelHomeClearLeak | mutation: `/clear` drops the held input but keeps the move's cleanup, 0/1/1 | 11,569 | holds | RED `CleanupResolves` |
| ChannelHomeClearLocal | mutation: `/clear` at the gateway leaves a held input of a remote or absent owner, 0/1/1, invariants only | 7,881 | RED `ClearHonored` | not checked |
| ChannelHomeHeldGuard | mutation: an old generation's native ends only while the move's input is still held, 1/1/0 | 62,721 | holds | RED `Completes`, `Drains`, `OldSessionEnds` |
| ChannelHomeStoreOnly | mutation: a new tenure retires the store but keeps the binding log, era entry and thread candidate, 1/1/0 | 59,357 | holds | RED `Completes`, `Drains` |

LocalCfg's properties would not fit the 300 s CI budget per config; it keeps the invariants it exists for. NoFence, NoWait, NoCancel, NoDelivery and ClearLocal are safety mutations and run their invariants only. The CI check runs one TLC per rule on one worker. Design, DesignClear, DesignRemoval and Thread are over the 500-state chart budget and have no chart. Each RED config has a bug-path chart named `channel-home-<mode>.chart.mmd`.

## Changes from revision 3

| cfg | revision 3 | revision 4 | why |
|---|---|---|---|
| Design, DesignRemoval, Thread | GREEN | GREEN with `EffectOnce`, `ClearHonored`, `CleanupResolves` | The new rules hold for the design. |
| DesignClear | 0/2/1, 49,969 states | 0/1/2, 24,749 states, GREEN | `/clear` now acts on a held input, which made 0/2/1 124,369 states. Two clears cover a clear of the idle home and a clear of a held input in one run. |
| NoFence, LocalCfg | as before | add `EffectOnce` | Two posts for one input are also two effects. |
| RawOnly, NoneHold, RenewRace | RED `Completes`, `Drains`, `HeldResolves` | add `CleanupResolves` | The stuck input keeps its move's cleanup too. |
| NoDelivery, ClearLeak, ClearLocal, HeldGuard, StoreOnly | — | new, all RED | One per review finding: a notice with no delivery, a clear that leaves the move's cleanup, a clear that cannot reach a remote hold, a cleanup tied to a held input, a tenure change that retires only the store. |

The same five mutations on the revision 3 model: NoDelivery is GREEN there (45,045 states; every rule holds, which is the review's finding), and HeldGuard is RED `Completes`, `Drains`, `OldSessionEnds` (58,857 states). ClearLeak, ClearLocal and StoreOnly have no counterpart: revision 3's `Clear` ignored held inputs and cleanup, and its store had no binding-log residue.

Model changes:
- `ndel` counts notices the outbox sent; `SendNotice` is the only step that raises it.
- `/clear` is one gateway action (`Clear`). Under the custody lock it discards a held or unclaimed input with a notice and cancels that generation's move cleanup, whoever owns the channel; the owner's idle session ends where the owner is up.
- A move's cleanup that has not transferred cancels just before it ends a session if the current setting keeps the owner (`Respond`); `NoSelfCleanup` judges by the current setting too.
- After the hand-over, ending an old generation's native needs only the recorded generation (`StaleEnd`, `DUp`), not a still-held input.
- `res` is the writer's binding-log, era and thread-candidate residue of a tenure; a new tenure retires it with the store.

## Transitions and the code

| Action | Code today (`826a01da0e`) | Design |
|---|---|---|
| `EditYaml`, `Sync` | `config/agent_memory.rs`, `db/postgres/agent_sync.rs` | S1, unchanged |
| `Arrive`, `RouteLocal` | `router/intake_dispatch.rs:235-297` | unmanaged path unchanged |
| `Stage` | `owner_record/custody.rs:117` (held) | S3 reads the PG row |
| `Replan` | `owner_record/plan.rs:62,110-139`, `custody.rs:438` | S2 plan validity |
| `PromoteSame` | `owner_record/custody.rs:244` (admitted) | as today |
| `RequestCleanup` | `owner_record/cleanup.rs:56-88` (`:84` needs `routing_cleanup IS NULL`) | pre-transfer work |
| `Respond`, `Expire` | `cleanup.rs:110-131`, `runtime_bootstrap/owner_cleanup.rs:102-115`, `clear_teardown/execution_cleanup.rs` | cancel branch: S2 |
| `Transfer` | `owner_record.rs:363-449`, `cleanup.rs:133-200` (`:153` needs a held head) | post-transfer work on the superseded row |
| `OrphanMove` | none today | S4b, existing cleanup storage |
| `Renew` | none today | S4a, after `native_end` is Ended and the writer owes nothing |
| `Withhold`, `SendNotice` | `db/replay_disposition/write.rs:209-219` (receipt only), `owner_record/plan.rs:191-219` (outbox in the same tx) | S4b settle |
| `Start` | `intake_worker.rs:304-311,422-485`, `tui_o/ownership/execution.rs:132-189` | S3, S6 |
| `Post`, `Withheld` | `owner_record/output.rs:105-125`, `tui_o/writer/deliver.rs:723-731` | as today |
| `Refresh` | `tui_o/ownership/execution.rs:132-189` | `native_of` |
| `StaleEnd`, `DUp` | `cleanup.rs:91` (ended work stays due only while its head is held) | post-transfer authority |
| `Clear` | `commands/control.rs:548-660`, `control/cancel_queued.rs:13-30` (local mailbox only) | S5a PG custody |
| `RecordChild` | `tui_o/cutover.rs:170-193` | S6 before claim |
| `res` retirement in `Start` | `writer/activation.rs:342-381`, `writer/host/first_init.rs:299-326`, `channel_policy.rs:549-557`, `writer/host/recover.rs:56-77` | S4b tenure retirement |

## Counterexamples and the code

| cfg / rule | Path | Code check |
|---|---|---|
| Before / `NoRefusal`, `FirstInputOnHome`, `ServesOnExpected` | With a live gateway session every later input stays on it; with none the O channel refuses. | `intake_router_hook.rs:334-345`, `:589-597`. |
| As7336 | The gateway's O hold check refuses before routing; the adopted native is never ended. | `intake_dispatch.rs:294-312`, `cutover.rs:416-420`, `owner_record/cleanup.rs:74-75`. |
| NoDelivery / `EffectOnce` | The owner goes offline mid-turn; the move marks the input noticed; no notice is sent. | Only an outbox row sent by the existing outbox is a notice: `owner_record/plan.rs:191-219`. |
| ClearLeak / `CleanupResolves` | G owns, an input is held for D, G is asked to end; `/clear` drops the input; the request expires and nothing finishes it. | `cleanup.rs:153-156` returns `Ok(false)` with no held head; `:84` refuses a new request while `routing_cleanup` is set. |
| ClearLocal / `ClearHonored` | No owner row yet; an input is held for D; `/clear` at the gateway sees only its local queue; D starts the input. | `control/cancel_queued.rs:13-30` and `/clear` read the local mailbox only. |
| HeldGuard / `Completes`, `OldSessionEnds` | G→D moves while D is down; the input is admitted, so G's old idle session never ends; back on G, the permit refuses the unbound live session. | `cleanup.rs:91` keeps ended work due only while its pending row is held; `:153` likewise. |
| StoreOnly / `Completes` | D→G→D: D's first tenure left its binding log; on return activation refuses the old sources and the writer holds. | `writer/activation.rs:347` reads the binding log from seq 0, `:368-381` refuses a source with bytes. |
| IndexLag, NoNotice, NoCancel, NoneHold, RenewRace | As in revision 3. | `owner_record/plan.rs:26-36,118-139`, `owner_record.rs:363-449`, `cleanup.rs:216`. |
| Other mutations | As in revision 2. | See the design report. |

## Not modeled

- The output debt of a same-owner Renew, Herdr's end reading and V2 follower merges: the design covers them with integration tests on the real store, host and queue.
- Unknown availability, lease movement, Herdr consent order, provider selectors, the O store's cursor, thread child registration order, more than one channel and attachments.
- A machine power-off that also kills the native session; `DDown` keeps the session, the worse case for a duplicate answer.
- `Expire` may fire as soon as a request is pending; real code waits 12 s.

## Reproduce

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/gateway-lease
~/.claude/skills/statechart/scripts/statechart ChannelHome.tla --config ChannelHomeClearLeak.cfg --view channel-home.view.mjs --out /tmp/sc-channel-home-clearleak
```
