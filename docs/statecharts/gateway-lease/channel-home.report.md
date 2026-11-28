# Channel home — per-channel execution node (#8486 design, revision 5)

Code SHA: `3d3299c517` (Before, As7336; their paths are unchanged since `5f0f2803e8`). The `design` configs describe the #8486 design, which is not implemented yet.
Model: [ChannelHome.tla](ChannelHome.tla). One O channel, two nodes: D is the node the setting names, G holds the Discord gateway lease and runs the channel when D cannot. [GatewayLease.tla](GatewayLease.tla) covers who holds the lease; this model takes exactly one lease holder as given.

## Rules

- `SingleWriter` (INVARIANT): at most one node can put the channel's answer on Discord at any moment.
- `NoDuplicate` (INVARIANT): no input is answered twice, and none is answered once it was withheld.
- `Conservation` (INVARIANT): every received input that is neither answered nor noticed is still held: in the gateway queue or custody, as the turn of a node, or as a notice reserved in the outbox. Received = answered + noticed + in progress.
- `EffectOnce` (INVARIANT): each input has at most one delivered effect (a posted answer or a notice the outbox sent), and an input marked answered or noticed has exactly one.
- `ClearHonored` (INVARIANT): an input that was waiting when the user ran `/clear` never begins, also when a worker had already claimed it, and the next input after a `/clear` never continues the cleared session.
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

The designation is `none`, `G` or `D`; it starts as `none`. `Toks` inputs can be in flight at once (ids are reused); `Debt = TRUE` lets an answer leave an announcement that only a later record of the live native seals; `Queue2 = TRUE` lets a second input wait in custody behind the held head. Only configs with `Removal = TRUE` (DesignRemoval, NoneHold) may edit it back to `none`. Restarts of D (`MaxFlaps`), config edits (`MaxEdits`), `/clear` (`MaxClears`) and new messages are the environment and carry no fairness. Every tick, hand-over, started turn and notice delivery is weakly fair.

## Results (TLC 1.7.4, 4 workers)

| cfg | Mode, bounds (flaps/edits/clears) | states | safety | liveness |
|---|---|---:|---|---|
| ChannelHomeBefore | `before` — main, 1/2/1 | 6,134 | RED `NoRefusal`, `FirstInputOnHome` | RED `ServesOnExpected` |
| ChannelHomeAs7336 | `as7336` — the #7336 path switched on as it is, 0/2/0 | 10,488 | RED `Conservation`, `NoRefusal` | RED `Completes`, `Drains`, `OldSessionEnds` |
| ChannelHomeDesign | `design`, 1/1/0 | 72,402 | GREEN | GREEN |
| ChannelHomeDesignClear | `design`, 0/1/2 | 27,041 | GREEN | GREEN |
| ChannelHomeDesignRemoval | `design`, 0/2/0, edits may remove the designation | 35,622 | GREEN | GREEN |
| ChannelHomeDesignDebt | `design`, `Debt`, 0/1/1 | 30,866 | GREEN | GREEN |
| ChannelHomeDesignCancel | `design`, `Queue2`, 0/1/1 | 28,591 | GREEN | GREEN |
| ChannelHomeDesignFresh | `design`, 1/1/1, `Toks = 2`, properties `Completes`, `Drains` | 93,174 | GREEN | GREEN |
| ChannelHomeDesignRemovalClear | `design`, 0/2/1, edits may remove the designation, `Toks = 2` | 49,717 | GREEN | GREEN |
| ChannelHomeThread | `design`, thread child, 1/1/0 | 79,842 | GREEN | GREEN |
| ChannelHomeNoFence | mutation: post on the cached permit, 0/2/0, invariants only | 23,587 | RED `SingleWriter`, `NoDuplicate`, `CurrentStore`, `EffectOnce` | not checked |
| ChannelHomeLocalCfg | mutation: each node judges the home from its own file, 0/3/0, invariants only | 983,407 | RED `SingleWriter`, `NoDuplicate`, `Conservation`, `CurrentStore`, `FirstInputOnHome`, `EffectOnce` | not checked |
| ChannelHomeNoWait | mutation: the old owner ends a running turn instead of answering Busy, 0/2/0, invariants only | 24,529 | RED `BusyTurnFinishes` | not checked |
| ChannelHomeDrop | mutation: an admitted input leaves custody with no turn and no notice, 0/1/0 | 3,956 | RED `Conservation` | RED `Completes` |
| ChannelHomeNoAdoptNative | mutation: cleanup and permit never see the adopted native, 0/2/0 | 10,269 | holds | RED `Completes`, `Drains`, `OldSessionEnds` |
| ChannelHomeNoRenew | mutation: no new generation after the owner's own session ended, 0/1/1 | 9,123 | holds | RED `Completes`, `Drains` |
| ChannelHomeReuseStore | mutation: a writer reuses a store of an older tenure, 0/2/0 | 23,067 | RED `CurrentStore` | holds |
| ChannelHomeRawOnly | mutation: a held input is re-planned only on a raw revision change, 0/2/0 | 19,845 | holds | RED `Completes`, `Drains`, `HeldResolves`, `CleanupResolves` |
| ChannelHomeThreadNoInherit | mutation: a thread reads its own designation, 0/2/0 | 875 | RED `FirstInputOnHome` | RED `ServesOnExpected` |
| ChannelHomeThreadNoChild | mutation: D never records the thread child, 0/2/0 | 5,708 | holds | RED `Completes`, `Drains` |
| ChannelHomeIndexLag | mutation: admission reads a designation index that lags the PG row, 0/1/0 | 3,069 | RED `FirstInputOnHome` | holds |
| ChannelHomeNoNotice | mutation: a started turn is withheld with no notice reserved, 0/2/0 | 17,617 | RED `Conservation` | RED `Completes` |
| ChannelHomeNoCancel | mutation: a plan that returns to the owner keeps the move's cleanup, 0/2/0, invariants only | 21,446 | RED `NoSelfCleanup` | not checked |
| ChannelHomeNoneHold | mutation: a held input gets no plan once the designation is removed, 0/2/0, edits may remove | 34,326 | holds | RED `Completes`, `Drains`, `HeldResolves`, `CleanupResolves` |
| ChannelHomeRenewRace | mutation: renew during a move's cleanup, plan judged without the owner generation, 0/1/0 | 1,953 | holds | RED `Completes`, `Drains`, `HeldResolves`, `CleanupResolves` |
| ChannelHomeNoDelivery | mutation: a withheld input is marked noticed with no notice sent, 1/1/0, invariants only | 54,564 | RED `EffectOnce` | not checked |
| ChannelHomeClearLeak | mutation: `/clear` drops the held input but keeps the move's cleanup, 0/1/1 | 12,703 | holds | RED `CleanupResolves` |
| ChannelHomeClearLocal | mutation: `/clear` at the gateway leaves a held input of a remote or absent owner, 0/1/1, invariants only | 9,793 | RED `ClearHonored` | not checked |
| ChannelHomeHeldGuard | mutation: an old generation's native ends only while the move's input is still held, 1/1/0 | 70,050 | holds | RED `Completes`, `Drains`, `OldSessionEnds` |
| ChannelHomeStoreOnly | mutation: a new tenure retires the store but keeps the binding log, era entry and thread candidate, 1/1/0 | 67,046 | holds | RED `Completes`, `Drains` |
| ChannelHomeClearClaimed | mutation: `/clear` skips an input a worker claimed but has not begun, 0/1/1, invariants only | 14,531 | RED `ClearHonored` | not checked |
| ChannelHomeNoFresh | mutation: `/clear` of an offline owner's idle session leaves no fresh-session mark, 1/1/1, `Toks = 2`, invariants only | 104,283 | RED `ClearHonored` | not checked |
| ChannelHomeNoResume | mutation: the input behind a cancelled head waits on the cancelled cleanup, `Queue2`, 0/1/1 | 27,967 | holds | RED `Completes` |
| ChannelHomeWaitDebt | mutation: renew waits for a seal the ended native can never write, `Debt`, 0/1/1 | 30,866 | holds | RED `Completes`, `Drains` |

DesignFresh and DesignRemovalClear run with two inputs in flight: with three they have 466,671 and 231,829 states and do not fit the 300 s CI budget per config; DesignFresh checks `Completes` and `Drains`, the rules its mark could break. LocalCfg's properties would not fit the 300 s CI budget per config; it keeps the invariants it exists for. NoFence, NoWait, NoCancel, NoDelivery, ClearLocal, ClearClaimed and NoFresh are safety mutations and run their invariants only. The CI check runs one TLC per rule on one worker. The eight `design` configs are over the 500-state chart budget and have no chart. Each RED config has a bug-path chart named `channel-home-<mode>.chart.mmd`.

## Changes from revision 4

| cfg | revision 4 | revision 5 | why |
|---|---|---|---|
| all 26 | as revision 4 | same verdicts, more states | A turn now starts in two steps, `Claim` and `Begin`. |
| DesignDebt, DesignCancel, DesignFresh, DesignRemovalClear | — | new, GREEN | The design with an unsealed tail, a second held input, `/clear` of an offline owner, and `/clear` after removing the designation. |
| ClearClaimed, NoFresh, NoResume, WaitDebt | — | new, RED | One per review finding: a claimed input `/clear` skipped, an offline owner's cleared session reused, the input behind a cancelled head stranded, a renew waiting for a seal that never comes. |

The four new mutations have no counterpart on the revision 4 model: it starts a turn in one step, has no second held input, no unsealed tail and no offline `/clear`.

Model changes:
- `Start` is `Claim` (the worker takes the input) and `Begin` (the input begins under the custody lock). `/clear` discards a claimed input that has not begun; `Begin` then fails its fence.
- `/clear` of an offline owner's idle session puts a fresh-session mark on the owner row (`fresh`); `FreshEnd` honours it before the owner's next input. `cleared` marks a session the user cleared; reusing it breaks `ClearHonored`.
- `Debt`: `Post` may leave `unsealed`; `Seal` needs the live native; `EndSettle` closes it with the recorded end after the native ended; `Renew` waits for it.
- `Queue2`: `Arrive2` holds a second input behind the head; `CancelHead` is `/cancel-queued` on the head; `Promote2` makes the next input the head and plans it.
- A claim of an offline owner that has not begun goes back to custody with `Replan`.

## Transitions and the code

| Action | Code today (`3d3299c517`) | Design |
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
| `Renew` | none today | S4b, after `native_end` is Ended and the writer owes nothing |
| `Withhold`, `SendNotice` | `db/replay_disposition/write.rs:209-219` (receipt only), `owner_record/plan.rs:191-219` (outbox in the same tx) | S4b settle |
| `Claim` | `intake_worker.rs:309-311`, `db/intake_outbox/worker_status.rs:164` | as today |
| `Begin` | `intake_worker.rs:441-477`, `owner_record/input_start.rs:50-73,114-127`, `tui_o/ownership/execution.rs:132-189` | S3, S6 |
| `Seal`, `EndSettle` | `tui_o/shadow/seal.rs:26-59`, `writer/restart_end.rs:381-469`, `writer/pieces.rs:184-201` | S4b |
| `FreshEnd` | none today (`session_preparation.rs:8,23` keeps `force_fresh` in memory) | S5a mark, S4b honour |
| `CancelHead`, `Promote2` | `v2/control.rs:197-272`, `owner_record/cleanup.rs:214-269` | S2, S5a |
| `Post`, `Withheld` | `owner_record/output.rs:105-125`, `tui_o/writer/deliver.rs:723-731` | as today |
| `Refresh` | `tui_o/ownership/execution.rs:132-189` | `native_of` |
| `StaleEnd`, `DUp` | `cleanup.rs:91` (ended work stays due only while its head is held) | post-transfer authority |
| `Clear` | `commands/control.rs:548-660`, `control/cancel_queued.rs:13-30` (local mailbox only) | S5a PG custody |
| `RecordChild` | `tui_o/cutover.rs:170-193` | S6 before claim |
| `res` retirement in `Begin` | `writer/activation.rs:342-381`, `writer/host/first_init.rs:299-326`, `channel_policy.rs:549-557`, `writer/host/recover.rs:56-77` | S4b tenure retirement |

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
| ClearClaimed / `ClearHonored` | D owns; its worker claims the input; `/clear` at the gateway leaves the claimed row; the input begins. | `db/intake_outbox/worker_status.rs:164` claims; the row stays `admitted` and `owner_record/input_start.rs:114-127` lets it begin. |
| NoFresh / `ClearHonored` | D answers and keeps its idle session; D goes down; `/clear` cannot end it; D comes back and the next input runs in that session. | No mark survives in PG: `force_fresh` is in memory (`session_preparation.rs:8,23`). |
| NoResume / `Completes` | G owns; an input is held for D with a cleanup request; a second input waits; `/cancel-queued` drops the head and its cleanup; nothing plans the second. | `owner_record/cleanup.rs:216` needs a terminal cleanup, `:230` needs owner = target, `:256` needs no active other owner; `request` is called only from `resume_head` (`:272-296`). |
| WaitDebt / `Completes`, `Drains` | G answers and leaves an unsealed announcement; `/clear` ends the native; the next input waits for a renew that waits for the seal. | `tui_o/shadow/identity.rs:265-270` announces without a seal; `seal.rs:26-59` drops it only on seal or `end_announced`. |
| IndexLag, NoNotice, NoCancel, NoneHold, RenewRace | As in revision 3. | `owner_record/plan.rs:26-36,118-139`, `owner_record.rs:363-449`, `cleanup.rs:216`. |
| Other mutations | As in revision 2. | See the design report. |

## Not modeled

- Batches of several inputs in one custody row (the model's input is one message), the binding log's sequence numbers and the store's era, Herdr's end reading: the design covers them with integration tests on the real queue, writer and host.
- Unknown availability, lease movement, Herdr consent order, provider selectors, the O store's cursor, thread child registration order, more than one channel and attachments.
- A machine power-off that also kills the native session; `DDown` keeps the session, the worse case for a duplicate answer.
- `Expire` may fire as soon as a request is pending; real code waits 12 s.

## Reproduce

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/gateway-lease
~/.claude/skills/statechart/scripts/statechart ChannelHome.tla --config ChannelHomeClearClaimed.cfg --view channel-home.view.mjs --out /tmp/sc-channel-home-clearclaimed
```
