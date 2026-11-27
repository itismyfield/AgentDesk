# Channel home — per-channel execution node (#8486 design)

Code SHA: `adeb172ff4e23e858601632b3a40123f9c10179c` (Before, As7336). The Design config describes the #8486 design, which is not implemented yet.
Model: [ChannelHome.tla](ChannelHome.tla). One O channel, two nodes: D is the node the setting names, G holds the Discord gateway lease and runs the channel when D cannot. [GatewayLease.tla](GatewayLease.tla) covers who holds the lease; this model takes exactly one lease holder as given (`LeaseMatchesRuntime`).

## Rules

- `SingleWriter` (INVARIANT): at most one node can put the channel's answer on Discord at any moment.
- `NoDuplicate` (INVARIANT): no input is answered twice, and none is answered after the user was told it was withheld.
- `NoSilentLoss` (INVARIANT): no input disappears without an answer or a notice.
- `NoRefusal` (INVARIANT): the user sees no refusal for an input that a non-home node received.
- `BusyTurnFinishes` (INVARIANT): a turn whose owner answered Busy is never cut by a move.
- `HomeFollowsDesignation` (PROPERTY): once restarts and edits stop, the home is the designated node while that node is up and the gateway otherwise, and it stays so.
- `Drains` (PROPERTY): every input leaves the gateway's queue.

Restarts of D (`MaxFlaps`) and config edits (`MaxEdits`) are the environment and carry no fairness. Every config uses `MaxFlaps = 1`, `MaxEdits = 2`, except LocalCfg (`0`, `3`): its bug needs three edits and no restart. Every tick, hand-over and started turn is weakly fair, and a next message always arrives.

## Results (TLC 1.7.4, 4 workers)

| cfg | Mode | states | safety | liveness |
|---|---|---:|---|---|
| ChannelHomeBefore | `before` — main | 2,440 | RED `NoRefusal`, `NoSilentLoss` | RED `HomeFollowsDesignation` |
| ChannelHomeAs7336 | `as7336` — the #7336 path switched on as it is | 275,565 | RED `NoRefusal`, `NoSilentLoss` | RED `HomeFollowsDesignation`, `Drains` |
| ChannelHomeDesign | `design` | 133,051 | GREEN | GREEN |
| ChannelHomeNoFence | mutation: post on the cached permit, no PG owner check | 153,097 | RED `SingleWriter`, `NoDuplicate` | holds |
| ChannelHomeLocalCfg | mutation: each node judges the home from its own config file | 83,942 | RED `SingleWriter`, `NoDuplicate`, `NoSilentLoss` | RED `HomeFollowsDesignation` |
| ChannelHomeNoWait | mutation: the old owner ends a running turn instead of answering Busy | 156,557 | RED `BusyTurnFinishes` | holds |

The Design state space has no deadlock (`CHECK_DEADLOCK TRUE` run, 133,051 states). The same Design with `MaxFlaps = 2`, `MaxEdits = 3` also holds every rule (1,519,702 states, 163 s); it is not kept as a config because of the CI budget. Design is over the 500-state chart budget, so it has a full result and no chart. The five RED configs each have a bug-path chart: [before](channel-home-before.chart.mmd), [as7336](channel-home-as7336.chart.mmd), [nofence](channel-home-nofence.chart.mmd), [localcfg](channel-home-localcfg.chart.mmd), [nowait](channel-home-nowait.chart.mmd).

## Counterexamples and the code

| cfg / rule | Path | Code check |
|---|---|---|
| Before / `NoRefusal`, `NoSilentLoss` | A node is set, no session lives on the gateway, a message arrives: the placement leaves the gateway and the O channel refuses it. | Confirmed. `intake_router_hook.rs:589-597` blocks with "O channel runs only on its gateway", `intake_dispatch.rs` posts the ⛔ notice and a live Blocked input is not retried. |
| Before / `HomeFollowsDesignation` | With a live session on the gateway every later input stays on it. | Confirmed. `intake_router_hook.rs:334-345` (`LiveSessionOwnerIsLocal`) returns before any preference is read. This is the 11-28 observation. |
| As7336 / `NoRefusal`, `Drains` | The designated node owns the channel; the next message reaches the gateway, whose O hold check sees the other node's permit and refuses before the routing hook runs. | Confirmed by reading. `intake_dispatch.rs:294-312` calls `for_input_placement` first; `cutover.rs:416-420` returns the permit error; `boot_custody.rs:28-33` cannot hold without a local permit, so the result is `WriterBlocked` and the ⛔ notice (`intake_dispatch.rs:727-729`). Not replayed as a test. |
| As7336 / `NoSilentLoss` | The first managed row is staged while an unmanaged turn still runs on the gateway; that turn's answer has no origin stamp and is withheld. | Partly confirmed. `owner_record/output.rs:118-125` withholds unstamped output of a managed channel and `owner_record/adoption.rs:1` states that adoption never stamps old artifacts. Whether a live message reaches staging while a turn is active was not traced; the design forbids it either way. |
| NoFence / `SingleWriter`, `NoDuplicate` | The old home misses the control deadline, the owner row moves, and the old home posts from its still-cached permit while the new home runs. | The fence exists in main: `owner_record/output.rs:106-117` joins the active owner generation under the channel custody lock. The mutation shows the design depends on it. |
| LocalCfg / `SingleWriter` | The two config files differ and both nodes judge themselves home. | Design premise, no code: the design reads the designation and the owner only from PG. |
| NoWait / `BusyTurnFinishes` | The old home ends its session while its turn runs. | The Busy answer exists in main: `clear_teardown/execution_cleanup.rs:63-81`. |

## Not modeled

- An unknown availability of the designated node (online heartbeat, stale readiness evidence). The design keeps such an input in custody and treats it as unavailable after the readiness lifetime.
- Lease movement between nodes (see GatewayLease.tla), more than one channel, thread children, and attachments.
- A machine power-off that also kills the native session; `DDown` keeps the session, which is the worse case for a duplicate answer.
- `Expire` may fire as soon as a request is pending. Real code waits 12 s, so the model allows more cut turns than production.

## Reproduce

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/gateway-lease
~/.claude/skills/statechart/scripts/statechart ChannelHome.tla --config ChannelHomeDesign.cfg --view channel-home.view.mjs --out /tmp/sc-channel-home
```
