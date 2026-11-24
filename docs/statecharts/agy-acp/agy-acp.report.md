# agy ACP channel session with steering (#8339 design)

<!-- doc-path-refs: external AcpSessionRuntime.ts -->

Code reference: `5021d25794` (origin/main). The ACP client does not exist yet, so this is a design model. Its counterexamples are checked against the design and against the T3 Code reference behaviour. They are not replayed against current Rust code. The output side (spool → writer → Discord receipt) stays the contract of [WriterAdoption AgySpec](../tui-o-writer/agy-adoption.report.md); in this model it is one receipt-guarded `Settle` step.

Design: [`docs/design/i8339/agy-acp.design.md`](../../design/i8339/agy-acp.design.md). Model SHA256 `97c6b7b749e22fdadcf84c5cc8fd5a1a33361235a76f8e6dbda71e6cf7837e1b`. The slice 1 code coordinate of each transition is in [Transitions and code](#transitions-and-code).

## Scope

The model covers one person-driven antigravity destination with two inputs. A person input that arrives while the turn has an open prompt is **steered**: it is written as another `session/prompt`, joins that turn, and is covered by the turn's single terminal post. The turn ends only when none of its prompts is still open.

Each fault happens at most once: one AgentDesk restart, one agy exit, one `!stop`, one `/clear`, one refused `session/resume` and one failed steer prompt.

A failed steer prompt (`SteerErrored`) is either kind from design §6.4.1: rejected before execution, or an unknown outcome. Both are handled the same way, because the scope cut B-noreturn takes out the return to the queue. The input stays a member of the turn and is never sent again. The turn's one post covers it with a notice line, recorded as `noticed`. The kind only picks the wording, so the model does not split it.

A steered input's origin is written ahead into the turn's durable row before its prompt (design §6.2-3). The bridge's in-memory copy learns that origin only at its next stream tick (`StreamTick`, not fair). A stop, `/clear`, child exit or restart may come before the tick. The completion receipt therefore merges the turn's latest durable origins under the same identity and nonce before it commits (§6.2-3a). After a restart, the boot settler reads the durable row itself.

agy is not fair. It may never answer a prompt or a cancel. It may answer the older prompt first (steer) or finish it normally before the newer one (queueing agy 1.4). Only AgentDesk's own steps are fair: spawn, attach, prompt, cancel, watchdog, boot and settle.

The child runs in its own process group, so on Unix an AgentDesk restart may leave it running (`orphan`). Boot must sweep that group (`SweepOrphan`) before it spawns a new child. On Windows the dcserver job object closes the child tree when dcserver ends, so no orphan exists there. The orphan path is a superset of that, so one model covers both platforms. The platform kill calls themselves (Unix group signals, Windows `taskkill /T`) are outside the model (design §5.2-13).

A forced stop (`ForceStop`: a force-kill or session cleanup) can come after a person's `!stop`, also after its `session/cancel` went out. It records its intent before it publishes its cancel. The turn then sends Terminate (`SendTerminate`, fair): the child's own group is killed and its open turns end cancelled. Terminate also goes out when agy's answer closed the turn after the intent was recorded; the turn's own drive sends it on that answer. The mutant `ForceOrder = "cancel_first"` publishes the cancel first and records the intent later (`ForceIntent`); a turn that agy's answer closed in between has no drive left to read it. A child that exits while a cancel is out ends its turn as cancelled too. Each child writes its own record at spawn. A stopped child's exit report (`ExitReported`) can reach AgentDesk after the next child has written its record. Only the report of the child that wrote the current record removes it. The session safety mutant charts (ClearKeeps, LoadLeak, Mutants, NoFence, RerunSent, ReceiptFromMemory and the four Steer charts) set `ForceMode` and `RecordMode` to `"off"`, so their state spaces stay near the old size. The ACP path is picked at boot (`Boot` sets `pathAcp` from the switch; `PathPick = "boot"`). A config reload (`Reload`) turns the switch off in the live snapshot only, and an input not on the ACP path runs as its own `agy --print` turn (`PrintTurn`). The mutant `PathPick = "turn"` reads the live switch at every input, as the code did before. The other charts set `PathPick = "off"` (no reload), so their state spaces stay as they were. A boot sees only durable state:

- the stored session id
- the turn membership written before each prompt (spool `open` and the turn's origin list, steered inputs included)
- the terminal record
- the receipts

## Rules

| Rule | Meaning |
|---|---|
| `AtMostOnePost` | No person input, steered or not, is covered by a second terminal post. |
| `NoDuplicateExecution` | An accepted `session/prompt` is written at most once per input. |
| `NoForeignResponse` | A turn is ended only by responses to its own prompts. |
| `NoPromptToClearedSession` | After `/clear`, no prompt is written to the retired session. |
| `PostOnlyWithDisposition` | A post always has the durable terminal record of the input's turn behind it. |
| `AnswerCoversWholeSteer` | A turn's post is made only after every prompt in it (steered ones included) has been answered. |
| `NoAnswerForRefusedSteer` | A failed steer is never covered as a plain answer: the post that covers it carries the failure notice. |
| `DurableOwner` | An input that was sent is always a member of a turn whose origin list holds it. |
| `NoReviveAfterClear` | An input that had been sent when `/clear` ran is never queued again, also when its steer fails after the clear. |
| `NoMissingOrigin` | Every input a post covered, steered ones included, is named by that turn's receipt (`consumed_origin_ids`). |
| `ChildRecorded` | A running child, or one a restart left running, always has its own record on disk for the sweep. |
| `SessionKeepsPath` | No input goes to `agy --print` while the channel's ACP child is up: the path fixed at boot holds until the next boot. |
| `ForceEndsTurn` (PROPERTY) | A forced stop ends the open turn even when agy never answers its cancel. |
| `ForceEndsChild` (PROPERTY) | A forced stop ends its child, also when agy's answer closed the turn first. |
| `EveryInputSettles` (PROPERTY) | Every accepted input, steered inputs included, is eventually covered by one terminal post: an answer, a failure notice or a cancel notice. |

## Results

All 20 configs ran with `scripts/ci/statecharts_tlc.py`. Each config's broken set matches its `expected.tsv` row. State counts are the full state space of each config with every rule removed (plain TLC, the same method gives the earlier counts for the earlier model).

| Config | What it fixes | Distinct states | Broken |
|---|---|---:|---|
| Design | Resident child in its own group, `session/resume`, steering, a failed steer stays in the turn with a notice, receipt merges durable origins, orphan sweep, watchdog, incarnation fence, forced stop recorded before its cancel and sent after a sent cancel or a closing answer, record removed only by its own child's exit | 197,984 | none |
| DesignLoad | Fallback `session/load` with replay dropped until the load response | 202,398 | none |
| PerTurn | Alternative: one `agy acp` child per turn | 283,265 | none |
| ForceNoWatchdog | No deadline at all: the forced stop alone ends a turn whose cancel agy never answers | 101,588 | none |
| ForceFirstOnly | Mutant: a forced stop after a sent `session/cancel` is not sent as Terminate (no deadline) | 101,588 | `ForceEndsTurn` |
| ForceCancelFirst | Mutant: a forced stop publishes its cancel before it records its intent | 125,751 | `ForceEndsChild` |
| PathBoot | The switch is read at boot; one reload turns it off while the child is up (forced stops and records off) | 69,638 | none |
| PathPerTurn | Mutant: the live switch is read at every input (forced stops and records off) | 86,768 | `SessionKeepsPath` |
| RecordWaiter | Mutant: a stopped child's late exit report removes whatever record is on disk | 91,388 | `ChildRecorded` |
| LoadLeak | Mutant: load replay is forwarded to the channel | 36,339 | `AtMostOnePost` |
| RerunSent | Mutant: boot re-queues the inputs of a turn whose prompts were written | 64,890 | `NoDuplicateExecution`, `NoReviveAfterClear` |
| ClearKeeps | Mutant: `/clear` forgets the id but the live child keeps the old session | 27,294 | `NoPromptToClearedSession` |
| NoFence | Mutant: responses matched by JSON-RPC id alone across incarnations | 34,538 | `NoForeignResponse` |
| NoWatchdog | Mutant: no inactivity or cancel deadline | 101,588 | `EveryInputSettles` |
| SteerEarlyClose | Mutant: the steered first prompt's `end_turn` closes the turn | 31,496 | `AnswerCoversWholeSteer`, `NoAnswerForRefusedSteer` |
| SteerAfterClose | Mutant: an input is steered into a turn whose last prompt already answered | 31,994 | `AnswerCoversWholeSteer`, `NoAnswerForRefusedSteer` |
| SteerErrorRequeue | Mutant: a failed steer is put back in the queue | 54,167 | `NoDuplicateExecution`, `NoReviveAfterClear` |
| SteerErrorSilent | Mutant: the turn post covers a failed steer without its notice | 29,448 | `NoAnswerForRefusedSteer` |
| ReceiptFromMemory | Mutant: the completion receipt uses only the bridge's in-memory origins | 29,448 | `NoMissingOrigin` |
| Mutants | Session safety mutants together (source of `agy-acp.chart.mmd`) | 95,511 | the four session safety rules and `NoReviveAfterClear` |

ForceCancelFirst checks only `ForceEndsChild` with record bookkeeping off, and RecordWaiter only its invariants with forced stops off, so each stays well under the runner's time limit.

ForceFirstOnly and NoWatchdog have the Design state space without the watchdog. ForceFirstOnly keeps `EveryInputSettles` out, because without a deadline it breaks for the `!stop`-only path as in NoWatchdog. With the watchdog the mutant's turn still ends at the cancel deadline (15 s); liveness cannot tell that delay apart, so the process test `a_force_kill_ends_the_acp_child_at_once_and_a_preserving_stop_keeps_it` bounds it.

SteerEarlyClose and SteerAfterClose break `NoAnswerForRefusedSteer` too: the turn is posted before the steer prompt fails, so that post has no notice. In Design, `AnswerCoversWholeSteer` rules out the early post. RerunSent and Mutants also break `NoReviveAfterClear`, because boot re-queues a written input that `/clear` had already met.

### Reachability

Steering, promotion and late exit reports are not vacuous in Design. Six witness invariants were checked alone against the Design constants, and each breaks (TLC exit 12):

- `NoSteeredAnswer`: a steered input is answered by its turn.
- `NoUncertainCovered`: a failed steer is covered by its turn's post.
- `NoMergeOnCancel`: a cancelled turn's receipt names a steered input only through the durable merge.
- `NoPromotion`: a forced stop is pending while the child still runs after `session/cancel` went out.
- `NoLateExit`: an earlier child's exit report is still due while the next child runs with its record.
- `NoSteerFailAfterClear`: a steer that fails after `/clear` is covered by its turn's post. This is the path of the 1d delta review's counterexample, and in Design it ends in one post with no requeue.

The witnesses are in the module and are not part of any committed config.

## Counterexamples

Charts:

- [Session chart](agy-acp.chart.mmd) draws the four session safety bug paths (`AgyAcpMutants`).
- [Steering chart](agy-acp-steer.chart.mmd) draws `AgyAcpSteerEarlyClose`.
- [Steer requeue chart](agy-acp-steer-requeue.chart.mmd) draws `AgyAcpSteerErrorRequeue`.
- [Receipt chart](agy-acp-receipt.chart.mmd) draws `AgyAcpReceiptFromMemory`.

Shortest paths, in the charts' words:

- **ClearKeeps:** spawn → `session/new` → `/clear` → `session/prompt` goes to the retired session. In the design, `/clear` closes or detaches the child.
- **RerunSent:** prompt open → AgentDesk restarts → boot re-queues → a second `session/prompt`. In the design, boot fails the turn with a notice covering all of its member inputs (#8274 §3.2).
- **LoadLeak:** answer posted → agy exits → respawn → `session/load` replays it → second post. The design uses `session/resume`, and the load fallback drops updates until its response (T3 `AcpSessionRuntime.ts:1838-1863`).
- **NoFence:** prompt open → watchdog kills the child's own group → respawn → new prompt → the dead child's buffered response ends the new turn. The design tags readers and pending entries with the child incarnation.
- **SteerEarlyClose:**
  1. prompt open
  2. second input steered ("Turn steered: two prompts open")
  3. agy answers the first prompt `end_turn`
  4. the turn is posted while the steer prompt is still open
  5. the steer prompt then fails, so the earlier post covered it without the failure notice.

  In the design, only the last open prompt's response ends the turn.
- **SteerAfterClose:** prompt answered ("Turn answered, post pending") → the next input joins that closing turn → the turn posts while the joined prompt is open. In the design, an input arriving with no open prompt goes to the queue as today (BusyActiveTurn).
- **SteerErrorRequeue (`NoReviveAfterClear`, 8 steps):** two prompts open → `/clear` → "steer prompt fails; the input is put back in the queue". This is the 1d delta review's counterexample: an input that was a member when `/clear` ran comes back after a late steer error. In the design, a failed steer stays in its turn, and the cancel post that `/clear` causes covers it.
- **SteerErrorRequeue (`NoDuplicateExecution`):** the re-queued steer is written as a second `session/prompt`.
- **SteerErrorSilent:** two prompts open → agy answers the first → the steer prompt fails → the turn posts without the notice line. In the design, the post carries one notice line per failed steer: "not delivered, please resend" or "outcome unknown, not run again".
- **ReceiptFromMemory (`NoMissingOrigin`, 8 steps):** two prompts open (the steer written ahead, no stream tick yet) → agy exits → the turn posts and commits a receipt without the steered input. The `!stop` variant was checked separately with a scratch invariant: two prompts open → `!stop` → `session/cancel` → cancel deadline → the receipt has no steered input (TLC exit 12, not committed). In the design, the receipt merges the durable row under the same identity and nonce (§6.2-3a).
- **ForceFirstOnly (liveness):** prompt open → `!stop` → force-kill → `session/cancel` → the Terminate is never sent and agy never answers. In the design, the turn sends Terminate whenever a forced stop is pending, also after the cancel.
- **ForceCancelFirst (liveness):** prompt open → force-kill publishes its cancel → agy's answer closes the turn → the intent lands with no drive left to read it → the child keeps running. In the design, the intent is recorded before the cancel, and the turn sends Terminate on that answer.
- **RecordWaiter (`ChildRecorded`, 6 states):** spawn → the child exits → respawn writes the new record → the first child's late exit report removes it while the new child runs. In the design, an exit removes only the record its own child wrote.
- **PathPerTurn (`SessionKeepsPath`, 5 states):** an input arrives → spawn → a reload turns the switch off → the next input goes to `agy --print` while the ACP child is still up. In the design, the switch is read once at boot (`bootstrap.rs:86`), so the reload changes nothing until the next boot.
- **NoWatchdog (liveness):** `!stop` → `session/cancel` → agy never answers → "Prompt open, cancel sent" forever. The design kills the child's own group after the cancel deadline (15 s) or the inactivity deadline (15 min).

## Transitions and code

Coordinates at this slice's head. "Not in slice 1" marks a design step that a later slice builds.

| Transition | Code |
|---|---|
| `Arrive` | `src/services/discord/router/message_handler/provider_dispatch/stream_json_turn.rs:188` `execute_request` |
| `Boot` (path pick) | `src/bootstrap.rs:86` `agy_acp::install` → `src/services/agy_acp/mod.rs:43`; read at `stream_json_turn.rs:226` |
| `Boot` (open turn settled) | `src/services/discord/recovery_engine/restore_inflight.rs:120` `restore_inflight_states` (existing boot restore) |
| `Reload` | `src/config_live_reload.rs:146` `reload_from_path`; restart warning `:115`; row `src/config/reload_classes/table.rs:187` |
| `PrintTurn` | `stream_json_turn.rs:215-221` (`execute_recorded` / `execute_streaming`) |
| `Spawn` | `src/services/agy_acp/mod.rs:110` `actor_for` → `actor.rs:229` `start` → `child.rs:53` `spawn`, record `child.rs:107` |
| `SweepOrphan` | `actor.rs:251` → `child.rs:133` `sweep_recorded` |
| `NewSession` | `actor.rs:353` (`session/new`), `:360` |
| `Reattach`, `ReattachRefused`, `LoadAnswered`, `ReplayOld` | Not in slice 1 (`session/resume`, `session/load`) |
| `SendPrompt` | `actor.rs:391` `prompt` (`session/prompt` `:415`) |
| `AgyResponds` | `actor.rs:372` → `turn.rs:151` `finish` |
| `SteerErrored`, `StreamTick` | Not in slice 1 (steering, slice 2b) |
| `AgyDies` | `actor.rs:208-212` (`Eof`/`Exited` → `lost`) |
| `WatchdogKill` | `actor.rs:145`, `:160` `expire` |
| `ZombieResponse` | incarnation fence `actor.rs:181`, `:223` `owns` |
| `UserStop`, `SendCancel` | `src/services/discord/health/recovery/runtime_stop.rs:120` → `turn.rs:26` `drive` → `actor.rs:421` (`session/cancel` `:431`) |
| `ForceStop` | `runtime_stop.rs:118`, `turn_bridge/tmux_runtime/judged_stop.rs:172`, `provider/cancel_token_cleanup/executor.rs:88` |
| `ForceIntent` | Mutant only (`ForceOrder = "cancel_first"`) |
| `SendTerminate` | `turn.rs:45`, `:62` → `actor.rs:192`, `:196` → `actor.rs:440` `stop_child` → `child.rs:31` |
| `ExitReported` | `actor.rs:181` → `child.rs:122` `forget` |
| `Clear`, `CloseAfterTurn` | Not in slice 1 (`/clear` handling; `per_turn` is the rejected alternative) |
| `Crash` | Process exit (no code) |
| `Settle` | `turn.rs:151` → spool `turn.rs:87` → turn_bridge |

## Not covered

- Repeated failures are bounded to one each. Both kinds of steer failure share one budget.
- Signals reaching other process groups are not modelled. Group isolation is a slice 1 process test (design §16), not a model property. The model only fixes the order: no new spawn while an orphan of the old child may run.
- The two kinds of steer failure are one action. Their only difference is the notice wording (§6.4.1).
- The Unix sweep's hold branch (probe error: no spawn, failure notice) is a slice 1 process test, not modelled.
- The durable-row read in the receipt merge failing (receipt held, row marked) and the row belonging to another turn are slice 2b tests, not modelled.
- The write-ahead save of a steered input's origin failing is a slice 2b test. In that case the prompt is not written and the input goes to the queue unsent.
- The queue drop done by `/clear` is abstracted.
- Unsolicited output after a turn is dropped by design and not modelled.
- Model change (`session/set_config_option`) runs before a turn's first prompt on an idle session and is not modelled.
- Steering of HTTP iMessage inputs is out of scope: they stay queued.
- Whether agy reflects a steer in its answer cannot be observed at the protocol level.

Raw TLC output is in the lane folder `hl-20260925/i8339/tlc1f/` and, for the forced-stop and record rules, `hl-20260925/i8339/s1-2/` and `s1-3/`; the path rule's in `s1-4-tlc/`. These are model results, not Rust test results.
