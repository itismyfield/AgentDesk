# agy ACP channel session with steering (#8339 design)

Code reference: `5021d25794` (origin/main). The ACP client does not exist yet, so this is a design model. Its counterexamples are checked against the design and against the T3 Code reference behaviour. They are not replayed against current Rust code. The output side (spool → writer → Discord receipt) stays the contract of [WriterAdoption AgySpec](../tui-o-writer/agy-adoption.report.md); in this model it is one receipt-guarded `Settle` step.

Design: [`docs/design/i8339/agy-acp.design.md`](../../design/i8339/agy-acp.design.md). Model SHA256 `0b841da987fc427a6bda6b0ffdff504c030279e1cadb090b8d4531f0846fa5d7`.

## Scope

The model covers one person-driven antigravity destination with two inputs. A person input that arrives while the turn has an open prompt is **steered**: it is written as another `session/prompt`, joins that turn, and is covered by the turn's single terminal post. The turn ends only when none of its prompts is still open.

Each fault happens at most once: one AgentDesk restart, one agy exit, one `!stop`, one `/clear`, one refused `session/resume` and one refused steer prompt (JSON-RPC error).

agy is not fair. It may never answer a prompt or a cancel. It may answer the older prompt first (steer) or finish it normally before the newer one (queueing agy 1.4). Only AgentDesk's own steps are fair: spawn, attach, prompt, cancel, watchdog, boot and settle.

The child is AgentDesk's stdio child, so a restart kills it. A boot sees only durable state:

- the stored session id
- the turn membership written before each prompt (spool `open` and the turn's origin list)
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
| `NoAnswerForRefusedSteer` | A refused steer is not counted as answered by the turn that never received it. |
| `EveryInputSettles` (PROPERTY) | Every accepted input, steered inputs included, is eventually covered by one terminal post: an answer, a failure notice or a cancel notice. |

## Results

All 12 configs ran with `scripts/ci/statecharts_tlc.py`, rc=0 for each. Each config's broken set matches its `expected.tsv` row.

| Config | What it fixes | Distinct states | Broken |
|---|---|---:|---|
| Design | Resident child, `session/resume`, steering, watchdog, incarnation fence | 22,004 | none |
| DesignLoad | Fallback `session/load` with replay dropped until the load response | 22,994 | none |
| PerTurn | Alternative: one `agy acp` child per turn | 29,316 | none |
| LoadLeak | Mutant: load replay is forwarded to the channel | 29,802 | `AtMostOnePost` |
| RerunSent | Mutant: boot re-queues the inputs of a turn whose prompts were written | 47,568 | `NoDuplicateExecution` |
| ClearKeeps | Mutant: `/clear` forgets the id but the live child keeps the old session | 26,176 | `NoPromptToClearedSession` |
| NoFence | Mutant: responses matched by JSON-RPC id alone across incarnations | 26,132 | `NoForeignResponse` |
| NoWatchdog | Mutant: no inactivity or cancel deadline | 10,224 | `EveryInputSettles` |
| SteerEarlyClose | Mutant: the steered first prompt's `end_turn` closes the turn | 23,707 | `AnswerCoversWholeSteer`, `PostOnlyWithDisposition` |
| SteerAfterClose | Mutant: an input is steered into a turn whose last prompt already answered | 23,883 | `AnswerCoversWholeSteer`, `PostOnlyWithDisposition` |
| SteerRefuseDrop | Mutant: a refused steer stays in the turn instead of going back to the queue | 13,144 | `NoAnswerForRefusedSteer` |
| Mutants | Session safety mutants together (source of `agy-acp.chart.mmd`) | 91,566 | the four session safety rules |

### Reachability

Steering is not vacuous in Design. Two witness invariants were checked alone against the Design constants, and each breaks (TLC exit 12):

- `NoSteeredAnswer`: a steered input is answered by its turn.
- `NoRefusedThenAnswered`: a refused steer is answered later by a turn of its own.

The witnesses are in the module and are not part of any committed config.

## Counterexamples

Charts:

- [Session chart](agy-acp.chart.mmd) draws the four session safety bug paths (`AgyAcpMutants`).
- [Steering chart](agy-acp-steer.chart.mmd) draws `AgyAcpSteerEarlyClose`.

Shortest paths, in the charts' words:

- **ClearKeeps:** spawn → `session/new` → `/clear` → `session/prompt` goes to the retired session. In the design, `/clear` closes or detaches the child.
- **RerunSent:** prompt open → AgentDesk restarts → boot re-queues → a second `session/prompt`. In the design, boot fails the turn with a notice covering all of its inputs (#8274 §3.2).
- **LoadLeak:** answer posted → agy exits → respawn → `session/load` replays it → second post. The design uses `session/resume`, and the load fallback drops updates until its response (T3 `AcpSessionRuntime.ts:1838-1863`).
- **NoFence:** prompt open → watchdog kills the child → respawn → new prompt → the dead child's buffered response ends the new turn. The design tags readers and pending entries with the child incarnation.
- **SteerEarlyClose:**
  1. prompt open
  2. second input steered ("Turn steered: two prompts open")
  3. agy answers the first prompt `end_turn`
  4. the turn is posted while the steer prompt is still open
  5. the steer is then refused and goes back to the queue, so the earlier post had no disposition behind it.

  In the design, only the last open prompt's response ends the turn.
- **SteerAfterClose:** prompt answered ("Turn answered, post pending") → the next input joins that closing turn → the turn posts while the joined prompt is open. In the design, an input arriving with no open prompt goes to the queue as today (BusyActiveTurn).
- **SteerRefuseDrop:** two prompts open → agy answers the first → the steer prompt is refused → the turn posts and counts the refused input as answered. In the design, the refused input is removed from the turn and handed back to the queue front (existing `HandedBack`). It is never re-steered into the same turn.
- **NoWatchdog (liveness):** `!stop` → `session/cancel` → agy never answers → "Prompt open, cancel sent" forever. The design kills the group after the cancel deadline (15 s) or the inactivity deadline (15 min).

## Not covered

- Repeated failures are bounded to one each.
- The queue drop done by `/clear` is abstracted.
- Unsolicited output after a turn is dropped by design and not modelled.
- Model change (`session/set_config_option`) runs before a turn's first prompt on an idle session and is not modelled.
- Steering of HTTP iMessage inputs is out of scope: they stay queued.
- Whether agy reflects a steer in its answer cannot be observed at the protocol level.

Raw TLC output is in the lane folder `hl-20260925/i8339/tlc1b/`. These are model results, not Rust test results.
