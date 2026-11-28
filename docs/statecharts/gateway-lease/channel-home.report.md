# Channel home — per-channel execution node (#8486 design, revision 2)

Code SHA: `5f0f2803e8` (Before, As7336; `src/` is the same as `adeb172ff4`). Design, DesignClear and Thread describe the #8486 design, which is not implemented yet.
Model: [ChannelHome.tla](ChannelHome.tla). One O channel, two nodes: D is the node the setting names, G holds the Discord gateway lease and runs the channel when D cannot. [GatewayLease.tla](GatewayLease.tla) covers who holds the lease; this model takes exactly one lease holder as given.

## Rules

- `SingleWriter` (INVARIANT): at most one node can put the channel's answer on Discord at any moment.
- `NoDuplicate` (INVARIANT): no input is answered twice, and none is answered after the user was told it was withheld.
- `Conservation` (INVARIANT): every received input that is neither answered nor notified is still held — in the gateway queue or custody, or as the turn of a node. Received = answered + notified + in progress.
- `NoRefusal` (INVARIANT): the user sees no refusal for an input that a non-home node received.
- `BusyTurnFinishes` (INVARIANT): a turn whose owner answered Busy is never cut by a move.
- `CurrentStore` (INVARIANT): a node that may post for a managed channel writes from a store of the current owner generation.
- `Completes` (PROPERTY): every received input is eventually answered or notified, also when no further message comes.
- `Drains` (PROPERTY): every input leaves the gateway's queue.
- `ServesOnExpected` (PROPERTY): once restarts and edits stop, every turn that runs, runs on the expected home (D while D is set and up, G otherwise). A home moves with the next message, so the rule says nothing while no message comes.
- `OldSessionEnds` (PROPERTY): a session of an older generation ends while its node is up.

Restarts of D (`MaxFlaps`), config edits (`MaxEdits`), `/clear` on the home (`MaxClears`) and new messages are the environment and carry no fairness. Every tick, hand-over and started turn is weakly fair. `Thread = TRUE` makes the channel a thread child: its designation is its parent's and D must record the child before its writer starts.

## Results (TLC 1.7.4, 4 workers)

| cfg | Mode, bounds (flaps/edits/clears) | states | safety | liveness | time |
|---|---|---:|---|---|---:|
| ChannelHomeBefore | `before` — main, 1/2/1 | 2,715 | RED `NoRefusal` | RED `ServesOnExpected` | 9 s |
| ChannelHomeAs7336 | `as7336` — the #7336 path switched on as it is, 1/2/1 | 237,015 | RED `Conservation`, `NoRefusal` | RED `Completes`, `Drains`, `OldSessionEnds` | 54 s |
| ChannelHomeDesign | `design`, 1/1/0 | 32,241 | GREEN | GREEN | 18 s |
| ChannelHomeDesignClear | `design`, 0/2/1 | 28,189 | GREEN | GREEN | 19 s |
| ChannelHomeThread | `design`, thread child, 1/1/1 | 106,238 | GREEN | GREEN | 56 s |
| ChannelHomeNoFence | mutation: post on the cached permit, no PG owner check, 0/2/0 | 9,510 | RED `SingleWriter`, `NoDuplicate`, `CurrentStore` | holds | 14 s |
| ChannelHomeLocalCfg | mutation: each node judges the home from its own config file, 0/3/0 | 106,066 | RED `SingleWriter`, `NoDuplicate`, `Conservation`, `CurrentStore` | RED `Completes` | 41 s |
| ChannelHomeNoWait | mutation: the old owner ends a running turn instead of answering Busy, 0/2/0 | 9,751 | RED `BusyTurnFinishes` | holds | 12 s |
| ChannelHomeDrop | mutation: an admitted input leaves custody with no turn and no notice, 0/2/0 | 24,144 | RED `Conservation` | RED `Completes` | 19 s |
| ChannelHomeNoAdoptNative | mutation: cleanup and permit never see the adopted session's native binding, 0/2/0 | 3,407 | holds | RED `Completes`, `Drains`, `OldSessionEnds` | 9 s |
| ChannelHomeNoRenew | mutation: no new generation after the owner's own session ended, 0/2/1 | 19,582 | holds | RED `Completes`, `Drains` | 16 s |
| ChannelHomeReuseStore | mutation: a Fresh writer reuses a store of an older generation, 0/3/0 | 48,754 | RED `CurrentStore` | holds | 28 s |
| ChannelHomeRawOnly | mutation: a held input is re-planned only on a raw revision change, 0/3/0 | 29,154 | holds | RED `Completes`, `Drains` | 21 s |
| ChannelHomeThreadNoInherit | mutation: a thread reads its own designation, not its parent's, 0/2/0 | 376 | holds | RED `ServesOnExpected` | 5 s |
| ChannelHomeThreadNoChild | mutation: D never records the thread child, 0/2/0 | 2,591 | holds | RED `Completes`, `Drains` | 6 s |

Design also holds every rule at 1/2/0 (174,997 states, 83 s) and at 1/2/1 (212 s). Those runs are not kept as configs: the CI check runs each property on its own, and 1/2/0 took 208 s of the 300 s budget with four configs in parallel. Design (1/1/0) keeps a restart with an edit, DesignClear (0/2/1) keeps two edits with a `/clear`. Design, DesignClear and Thread are over the 500-state chart budget, so they have a full result and no chart. Each RED config has a bug-path chart: [before](channel-home-before.chart.mmd), [as7336](channel-home-as7336.chart.mmd), [nofence](channel-home-nofence.chart.mmd), [localcfg](channel-home-localcfg.chart.mmd), [nowait](channel-home-nowait.chart.mmd), [drop](channel-home-drop.chart.mmd), [noadoptnative](channel-home-noadoptnative.chart.mmd), [norenew](channel-home-norenew.chart.mmd), [reusestore](channel-home-reusestore.chart.mmd), [rawonly](channel-home-rawonly.chart.mmd), [threadnoinherit](channel-home-threadnoinherit.chart.mmd), [threadnochild](channel-home-threadnochild.chart.mmd).

## Changes from revision 1

| cfg | revision 1 | revision 2 | why |
|---|---|---|---|
| Before | RED `NoRefusal`, `NoSilentLoss` / `HomeFollowsDesignation` | RED `NoRefusal` / `ServesOnExpected` | The refusal posts a ⛔ notice, so the input is notified, not lost. `NoSilentLoss` counted a notice as a loss. |
| As7336 | RED `NoRefusal`, `NoSilentLoss` / `HomeFollowsDesignation`, `Drains` | RED `Conservation`, `NoRefusal` / `Completes`, `Drains`, `OldSessionEnds` | Same two paths; the adopted native binding is now modeled, so the old gateway session is never ended (`OldSessionEnds`). |
| Design | GREEN, 133,051 states (1/2) | GREEN, 32,241 states at 1/1/0 and 174,997 at 1/2/0 (+ DesignClear, Thread) | Now includes `/clear` renew, store generations, held re-plan on proof change, thread children and the offline-owner move. |
| NoFence | RED `SingleWriter`, `NoDuplicate` | adds `CurrentStore` | The new store rule also catches the cached-permit post. |
| LocalCfg | RED `SingleWriter`, `NoDuplicate`, `NoSilentLoss` / `HomeFollowsDesignation` | RED `SingleWriter`, `NoDuplicate`, `Conservation`, `CurrentStore` / `Completes` | Loss is now judged by conservation. |
| NoWait | RED `BusyTurnFinishes` | same | NoWait and NoFence now run without a restart (`MaxFlaps = 0`): the same rules break in about 9,700 states instead of about 200,000, which keeps them far inside the CI budget. |
| Drop, NoAdoptNative, NoRenew, ReuseStore, RawOnly, ThreadNoInherit, ThreadNoChild | — | new, all RED | One mutation per review finding (see below). |

Rule changes: `NoSilentLoss` (an input's fate becomes "lost") is replaced by `Conservation`, which looks at where every unanswered input is held. Revision 1 marked loss only on paths that set "lost", so an action that drops an admitted input without setting a fate passed it; the Drop config is that action and is RED now. `HomeFollowsDesignation` asked the home to follow the setting even with no messages; it is replaced by `ServesOnExpected`, which judges only turns that run. Revision 1 also gave the arrival of a message weak fairness, which made "the next message moves the home" true by assumption; arrivals now carry no fairness and `Completes` must hold without them.

## Counterexamples and the code

| cfg / rule | Path | Code check |
|---|---|---|
| Before / `NoRefusal` | A node is set, no session lives on the gateway, a message arrives: the placement leaves the gateway and the O channel refuses it. | Confirmed. `intake_router_hook.rs:589-597` blocks with "O channel runs only on its gateway"; `intake_dispatch.rs` posts the ⛔ notice. |
| Before / `ServesOnExpected` | With a live session on the gateway every later input stays on it. | Confirmed. `intake_router_hook.rs:334-345` (`LiveSessionOwnerIsLocal`) returns before any preference is read. |
| As7336 / `NoRefusal`, `Drains` | The designated node owns the channel; the gateway's O hold check sees the other node's permit and refuses before routing. | Confirmed by reading. `intake_dispatch.rs:294-312` calls `for_input_placement` first; `cutover.rs:416-420` returns the permit error; `boot_custody.rs:28-33` cannot hold without a local permit. |
| As7336 / `Conservation` | The first managed row is staged while an unmanaged turn still runs on the gateway; that turn's answer has no origin stamp and is withheld with no notice. | `owner_record/output.rs:118-125` withholds unstamped output of a managed channel. The design stages the first row only with no live turn. |
| As7336, NoAdoptNative / `Completes`, `Drains`, `OldSessionEnds` | The first designation adopts the gateway's idle session; cleanup has no routed receipt, so it cannot name the native session; the request never ends and the input stays held. | Confirmed by reading. `owner_record/adoption.rs:46-54` stores the binding; `cleanup.rs:69-84` reads the native only from the routed receipt; `clear_teardown/execution_cleanup.rs:22-28` returns NeedsOperator without it; `ownership/execution.rs:163-178` ignores the adoption binding. |
| NoRenew / `Completes`, `Drains` | `/clear` ends the owner's own session in the same generation; the permit stays bound to the dead native, so the next input is admitted and never starts. | Confirmed by reading. `ownership/execution.rs:163-178` binds the permit to the receipt's native; `/clear` (`control.rs:548-667`) keeps the owner row and generation. |
| ReuseStore / `CurrentStore` | G → D → G: the gateway becomes owner again in a new generation and its writer recovers the store of its earlier generation. | Confirmed by reading. `recover.rs:47-79` recovers whatever store is on disk; `host.rs:620-627` seeds the last posted id from it; nothing compares the store's generation with the permit. |
| RawOnly / `Completes`, `Drains` | A held input whose plan was made for D; the setting moves back to G with no raw revision change on the held row; the tick never re-plans and the input waits forever. | Confirmed by reading. `cleanup.rs:221-228` re-plans only on a raw revision change; `cleanup.rs:153-164` advances on that revision only. |
| ThreadNoInherit / `ServesOnExpected` | The parent is set to D; the thread has no setting of its own and is served on G forever. | Confirmed by reading. `agent_execution_node.rs:59-65` (`has_channel_policy`) misses home preferences and execution-node rows, so `policy_channel.rs:21-27` does not resolve the parent. |
| ThreadNoChild / `Completes`, `Drains` | The thread is admitted to D, but D has no child record, so its writer never starts. | Confirmed by reading. `cutover.rs:170-194` (`record_thread_child_by_id`) records the child in the process's own boot snapshot, and its callers run where the message arrives (`router/intake_gate.rs:241`, `session_runtime.rs:506`, `routes/discord.rs:125`); D's owner view and writer start walk only known channels (`owner_execution.rs:159-168`, `o_writer_host.rs:874-884`). |
| Drop / `Conservation`, `Completes` | An admitted input leaves custody with no turn and no notice. | Synthetic mutation; it shows the rules catch the drop the revision 1 rules passed. |
| NoFence, LocalCfg, NoWait | As in revision 1. | `owner_record/output.rs:106-117` (fence), design premise (PG only), `clear_teardown/execution_cleanup.rs:63-81` (Busy). |

## Not modeled

- An unknown availability of the designated node (stale readiness evidence). The design keeps such an input in custody and treats D as unavailable 120 s after D's last readiness evidence.
- The 2 s owner-index lag at admission; the design reads the designation and owner from PG at admission, so the model reads them directly.
- Herdr consent order (the gateway's consent check before routing); covered by unit tests in the design.
- Lease movement between nodes (see GatewayLease.tla), more than one channel, and attachments.
- A machine power-off that also kills the native session; `DDown` keeps the session, which is the worse case for a duplicate answer.
- `Expire` may fire as soon as a request is pending. Real code waits 12 s, so the model allows more cut turns than production.

## Reproduce

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/gateway-lease
~/.claude/skills/statechart/scripts/statechart ChannelHome.tla --config ChannelHomeDrop.cfg --view channel-home.view.mjs --out /tmp/sc-channel-home-drop
```
