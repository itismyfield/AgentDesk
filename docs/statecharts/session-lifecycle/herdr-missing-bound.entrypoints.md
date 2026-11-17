# Missing Bound execution entrypoints

Frozen source: `1161162a18c87a0514f53dd7ae5dac3c830149cb`. Coordinates below are for that source, before any production edits.

The search covered the five hosted-record mutation APIs, their callers, direct SQL writes of `hosted_execution`, every reference to `held_watch`, and `BusyState::Held`. Raw `git grep -n` results are retained in the lane's evidence directory. Test-only SQL writers were inspected and excluded from the production table. `session_record.rs:97` is inside `#[cfg(test)]`; the two production SQL write families are the hosted-record CAS and the explicit legacy return.

`HerdrMissingBound` is deliberately smaller than `HerdrSession`, `HerdrRootRecovery` and `HerdrHandoffHeld`: none of those existing models covers this combination of Missing reconnect, the early human front and its permanently Unknown probe. Values are the exact row's `row`, `nonce`, `pane`, the owner's `input`, `active` and `watching`. Launch's Pending/location/evidence substeps are a projection to `Launch`; they remain covered by `HerdrSession`. No new kill or legacy-return behavior is proposed.

| # | Entry / frozen file:line | Model value / action | Scope and origin |
|---|---|---|---|
| 1 | `src/db/dispatched_sessions/hosted_execution.rs:408` install Pending | `row`, `nonce`; `Launch` | New incarnation only over Legacy/Retired; not over Bound |
| 2 | `src/db/dispatched_sessions/hosted_execution.rs:418` record location | `row = Pending`; projected `Launch` | Same Pending nonce |
| 3 | `src/db/dispatched_sessions/hosted_execution.rs:430` record evidence | `pane`, `nonce`; projected `Launch` | Location/root/provider evidence, not a new incarnation |
| 4 | `src/db/dispatched_sessions/hosted_execution.rs:449` bind | `row = Bound`; projected `Launch` | Same owner/nonce, complete Pending evidence |
| 5 | `src/db/dispatched_sessions/hosted_execution.rs:460` retire | `Retire` | Exact owner, nonce and observed row |
| 6 | `src/db/dispatched_sessions/hosted_execution.rs:573` compare-and-set | `captured`, `nonce`; `Proof` / `Retire` | Raw row and canonical identity predicates; successor cannot be retired |
| 7 | `src/services/herdr_launch.rs:503` install | `Launch` | Shared launch entry, all input origins |
| 8 | `src/services/herdr_launch.rs:575` location | Projected `Launch` | Create reply fills only its Pending |
| 9 | `src/services/herdr_launch.rs:717` evidence | Projected `Launch` | Same incarnation |
| 10 | `src/services/herdr_launch.rs:649` failed launch retirement | `LaunchFails`; projected `Retire` | Existing own-Pending failure cleanup, unchanged |
| 11 | `src/services/herdr_launch/unplaced.rs:457` recovered evidence | Projected `Launch` | Boot/next-input resolution of an unplaced Pending, unchanged |
| 12 | `src/services/claude/herdr_turn.rs:878` initial attach bind | Projected `Launch`; `row = Bound` | First source of the same launched execution |
| 13 | `src/services/discord/tui_prompt_relay/herdr_source.rs:197` source bind | `Reconnect`, `row = Bound` | Verified attach of Pending; not a replacement execution |
| 14 | `src/services/discord/tui_prompt_relay/herdr_source.rs:272` clear-chain bind | `Reconnect`, `row = Bound` | Same nonce after an authentic clear chain |
| 15 | `src/services/discord/recovery_engine/herdr_reader.rs:235` restart pass | `Reconnect` | Local rows; Claude and Codex passes, no pane creation |
| 16 | `src/services/discord/recovery_engine/herdr_reader.rs:456` reconnect candidate | `Reconnect` / `Read` / proposed `Retire` | Missing currently omitted; transition lease and current-row check already exist |
| 17 | `src/services/discord/recovery_engine/host_reconcile.rs:142` absent exact pane | `pane = Missing`; `Read` | Complete endpoint-matching snapshot only |
| 18 | `src/cli/herdr.rs:241` retire CLI | `Read` / `Retire` | Explicit local ended execution; existing authority, no pane write |
| 19 | `src/cli/herdr.rs:326` retirement CAS | `Retire` | Proposed common proof/CAS extraction; preserves the CLI's existing shell-ended cases |
| 20 | `src/services/discord/herdr_terminate.rs:224` shared confirmed retirement | `Retire` | Existing strict end authority, unchanged |
| 21 | `src/services/discord/herdr_terminate.rs:411` explicit terminate, `:606` retire | `ControlAction(terminate)` | Explicit request and existing close/end proof; no automatic close proposed |
| 22 | `src/services/discord/herdr_terminate/root_replaced.rs:536` recovery | `Retire` projection | Existing RootReplaced/MarkerLost strict proof; not broadened to Unknown |
| 23 | `src/services/discord/recovery_engine/herdr_row_end.rs:492` row end | `Boundary` / `Retire` projection | Existing terminal source proof |
| 24 | `src/services/discord/recovery_engine/herdr_row_end_live.rs:67` live row end | `Boundary` / `Retire` projection | Existing managed-turn settlement |
| 25 | `src/services/claude/herdr_return.rs:84` return retirement | `Retire` projection | Existing explicit return path, unchanged |
| 26 | `src/db/dispatched_sessions/hosted_execution/legacy_return.rs:41` remove retired record | Outside model: legacy return | Retired proof plus exact owner/nonce SQL; no Missing fix changes this path |
| 27 | `src/services/discord/runtime_bootstrap.rs:860` reconciliation latch | `Reconnect`, `Reboot` | Boot ordering, durable row survives process restart |
| 28 | `src/services/discord/herdr_terminate/root_replaced/preclaim.rs:19` preclaim | `Input` / `Read` projection | Existing remembered/replaced-server candidates only; ordinary Missing is not recovered here |
| 29 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:903` idle start | `Input` | Restored front and new front share this path |
| 30 | `src/services/discord/router/message_handler/intake_turn.rs:983` early call | `Input` / `Read` | Intake before mailbox turn claim |
| 31 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:53` early | `Read`, proposed `Retire`, `Launch` or `Reject` | Claude origin classified as person, no managed active turn required by the early path |
| 32 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:9` running | `active = TRUE`; `Boundary` | Existing working-turn temporary hold, retained until a real boundary |
| 33 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:194` late | `Read` / `Boundary` projection | Pre-submit recheck after claim; must not silently fall back to the old pane |
| 34 | `src/services/discord/queue_io/input_owner/claude_handoff.rs:566` observe | `Read` | Unknown record/foreign owner/unavailable target also yield Held; none proves Missing |
| 35 | `src/services/claude_tui/host_input/busy/herdr.rs:222` liveness read | `Read` / frozen `held` | False `execution_alive` is not itself Missing; transcript section is outside this lane |
| 36 | `src/services/claude_tui/host_input/busy.rs:192` observation probe | `HeldWatch` | No context, or absent execution, reads Unknown; immutable None context is also possible |
| 37 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:44` running fallback arm | `watching`; `Boundary` | Working-turn hold; preserve boundary wake, avoid inventing an idle retry |
| 38 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:263` late arm | `watching`; `Boundary` projection | `unwound` waits for this attempt's token/row to leave |
| 39 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:343` shared hold arm | Frozen `watching = TRUE` / `HeldWatch` | Early Held and failed prepare both reach this; proposed idle path must stop calling it |
| 40 | `src/services/discord/queue_io/input_owner/effect.rs:108` preparation watch | `watching`; post-reservation projection | A concrete preparation cause, not the early idle Missing decision |
| 41 | `src/services/discord/queue_io/input_owner/effect.rs:152` screen watch | `watching`; screen projection | Legacy tmux screen path, unchanged |
| 42 | `src/services/discord/queue_io/input_owner/effect.rs:166` cause watch | `watching`; cause projection | Lock/buffer/transcript cause; unchanged legacy path |
| 43 | `src/services/discord/queue_io/input_owner/effect/claude.rs:88` actual busy transaction | `HeldWatch` / `Boundary` projection | Working transaction's concrete returned probe; preserve transient holds |
| 44 | `src/services/discord/queue_io/input_owner/effect/codex.rs:71` Codex cause | Outside model: legacy screen | Codex path, unchanged |
| 45 | `src/services/discord/queue_io/input_owner/effect/codex.rs:182` Codex composer | Outside model: legacy screen | Codex path, unchanged |
| 46 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:187` writer boot hold | `Reconnect` / waiting projection | All origins; readiness probe, not a dead exact-pane probe |
| 47 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:936` synthetic native wait | `active`; `Boundary` projection | Internal/command start; native turn must clear before its own start |
| 48 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1032` writer refusal | `Reconnect` / waiting projection | Writer/bootstrap readiness probe |
| 49 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1047` turn-mode refusal | `Boundary` projection | Existing mode confirmation wake |
| 50 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1058` bootstrap refusal | `Read` projection | Reads turn-host readiness, not one dead pane |
| 51 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1185` synthetic late read | `active`; `Boundary` projection | Internal starts' native-screen probe; legacy path unchanged |
| 52 | `src/services/discord/queue_io/input_owner/drive.rs:109` start registered watch | `HeldWatch` | Drive stops if refused front is still held; removing a watch alone does not dispose the front |
| 53 | `src/services/discord/queue_io/input_owner/held_watch.rs:95` arm, `:66` verdict | `watching`; `HeldWatch` | Unknown keeps indefinitely; Ready wakes; Gone only ends watching, does not settle input |
| 54 | `src/services/discord/queue_io/input_owner/recovery.rs:15` queue recovery | `Restored`, `Input`, `Reboot` | Existing queue is adopted/requeued before the next owner judgement; no migration proposed |
| 55 | `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:83` failure commit | Proposed `Reject`; current retained failure projection | Calls `owner.start_failed`; normalized fresh-launch failure must use reason+disposal instead |
| 56 | `src/services/turn_orchestrator/queued_head/v2/start_failure.rs:60` retain failed start | Frozen retained failure projection | Current failure retention is an additional acceptance gap after early Missing repair succeeds |
| 57 | `src/services/discord/commands/control.rs:713` cancel queued | `ControlAction(cancel)` | Explicit user's removal acknowledgement; no hosted-row mutation |
| 58 | `src/services/discord/commands/control.rs:508`, `control/clear_teardown.rs:137` clear | `ControlAction(clear)` plus existing terminate projection | Explicit queue/session clear; separate existing terminate proof |
| 59 | `src/services/discord/commands/control.rs:619`, `src/services/discord/commands/stop.rs:84` stop | `ControlAction(stop)` / `Boundary` projection | Existing native stop gate, not automatic Missing cleanup authority |
| 60 | `src/services/discord/queue_io/input_owner/origin.rs:26` / `:30` origin | Human-input scope of `Input` | Discord/iMessage are person origins; HTTP/routine/effort continuation are internal origins |
| 61 | `src/services/turn_orchestrator/queued_head/v2.rs:61` person context | Human-input scope of `Input` | Missing context is Discord; command-bot author flag is not separately checked by `OwnerTurn::early` |

The adjacent-control cfgs project an already acknowledged explicit control to `ControlAction`; they do not verify the entire implementations of cancel/clear/stop/terminate. Launch Pending placement, return to tmux and legacy composer details likewise remain outside this small model. The entrypoint table explicitly distinguishes those projections from actions checked here.

The frozen early classifier operates on `InputOrigin`, not directly on the author bot flag. This lane should preserve that classification rather than introducing a separate bot policy. Real human test fixtures must carry ordinary Discord admission, and internal-origin regression cases must continue to use their preserved admission context.

The new path must remove early idle watch registration, not turn `Unknown` into a timeout or make `Screen::Gone` silently delete a front. A working-turn watch remains contingent on a real boundary; an empty-context probe is not evidence that a dead idle execution can recover. The actual tests must cover both paths and must not claim the projected running fairness assumption as a product proof.


## Implementation delta

The frozen 61 rows above remain the exhaustive pre-change census. These six rows locate the new connections to the same actions; input origin and adjacent controls keep their frozen policy.

| # | Entry / current file:line | Model value / action | Scope and origin |
|---|---|---|---|
| 62 | `src/services/herdr_launch/retire.rs:36` exact endpoint read | `Read`, `Proof`, `Retire` | CLI accepts its existing ended-shell proof; automatic callers require complete Missing; raw-row/owner/nonce CAS is shared |
| 63 | `src/services/discord/recovery_engine/herdr_reader.rs:487` Missing reconnect | `Reconnect`, `Retire` | Existing transition lease/current-row check precedes the common Missing-only proof |
| 64 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:172` idle repair | `Read`, `Retire`, `Launch`, `Reject` | Human origin and no active mailbox turn; same Retired row must be re-judged before normal launch |
| 65 | `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:87` exact claim disposal | `Reject`, `NoticeOnce` | Commit once for that claim and its captured merged originals; then one channel reason; a repeated failure cannot discard the next front |
| 66 | `src/services/herdr_launch/retire.rs:113` restart repair outcome | `Reconnect`, `Restored`, `LaunchFails` | Keeps only exact owner/retired nonce in memory; no input is stored; a new incarnation clears the old outcome |
| 67 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:461` working probe | `HeldWatch`, `Boundary` | No watch for an observation without context; complete Missing ends a working watch, preserving its front until the actual boundary |

`provider_dispatch/owner_input.rs:1001` passes pre-executor start failures to the scoped failure handler. The existing `start_failure::run` wrapper and `Failure::finish` cover executor/registration errors as well. Its cancellation and native-stop checks remain in place. Ordinary failures keep their existing retry/retained policy. The added marker contains no prompt or queue payload.
