# agy ACP channel session with steering (#8339 design)

<!-- doc-path-refs: external AcpSessionRuntime.ts -->

Code reference: `5021d25794` (origin/main). The ACP client does not exist yet, so this is a design model. Its counterexamples are checked against the design and against the T3 Code reference behaviour. They are not replayed against current Rust code. The output side (spool → writer → Discord receipt) stays the contract of [WriterAdoption AgySpec](../tui-o-writer/agy-adoption.report.md); in this model it is one receipt-guarded `Settle` step.

Design: [`docs/design/i8339/agy-acp.design.md`](../../design/i8339/agy-acp.design.md). Model SHA256 `8ab7aa6484542fb98d6ba62a831adf3cbf35683f489f5fbb0a4afa342f6e27cc`.

## Scope

The model covers one person-driven antigravity destination with two inputs. A person input that arrives while the turn has an open prompt is **steered**: it is written as another `session/prompt`, joins that turn, and is covered by the turn's single terminal post. The turn ends only when none of its prompts is still open.

Each fault happens at most once: one AgentDesk restart, one agy exit, one `!stop`, one `/clear`, one refused `session/resume` and one failed steer prompt. A failed steer prompt is one of two kinds, and they share that one budget:

- `SteerRejected`: an error that proves the prompt was rejected before execution (design §6.4.1 ①). The input leaves the turn through a durable Returning record, and `ReturnToQueue` puts it back in the queue.
- `SteerErrored`: an error with an unknown outcome (§6.4.1 ②). The input stays a member of the turn, is never sent again, and is covered by the turn's one post with a notice.

agy is not fair. It may never answer a prompt or a cancel. It may answer the older prompt first (steer) or finish it normally before the newer one (queueing agy 1.4). Only AgentDesk's own steps are fair: spawn, attach, prompt, cancel, watchdog, boot and settle.

The child runs in its own process group, so an AgentDesk restart may leave it running (`orphan`). Boot must sweep that group (`SweepOrphan`) before it spawns a new child. A boot sees only durable state:

- the stored session id
- the turn membership written before each prompt (spool `open` and the turn's origin list)
- the terminal record
- the receipts
- the Returning records of rejected steers

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
| `DurableOwner` | An input that was sent is always either a member of a turn or held by a durable Returning record. It is never held only in memory. |
| `EveryInputSettles` (PROPERTY) | Every accepted input, steered inputs included, is eventually covered by one terminal post: an answer, a failure notice or a cancel notice. |

## Results

All 14 configs ran with `scripts/ci/statecharts_tlc.py`, rc=0 for each. Each config's broken set matches its `expected.tsv` row.

| Config | What it fixes | Distinct states | Broken |
|---|---|---:|---|
| Design | Resident child in its own group, `session/resume`, steering, steer error split, durable Returning, orphan sweep, watchdog, incarnation fence | 28,186 | none |
| DesignLoad | Fallback `session/load` with replay dropped until the load response | 29,176 | none |
| PerTurn | Alternative: one `agy acp` child per turn | 36,562 | none |
| LoadLeak | Mutant: load replay is forwarded to the channel | 36,828 | `AtMostOnePost` |
| RerunSent | Mutant: boot re-queues the inputs of a turn whose prompts were written | 59,922 | `NoDuplicateExecution` |
| ClearKeeps | Mutant: `/clear` forgets the id but the live child keeps the old session | 32,682 | `NoPromptToClearedSession` |
| NoFence | Mutant: responses matched by JSON-RPC id alone across incarnations | 33,450 | `NoForeignResponse` |
| NoWatchdog | Mutant: no inactivity or cancel deadline | 14,230 | `EveryInputSettles` |
| SteerEarlyClose | Mutant: the steered first prompt's `end_turn` closes the turn | 30,435 | `AnswerCoversWholeSteer`, `PostOnlyWithDisposition` |
| SteerAfterClose | Mutant: an input is steered into a turn whose last prompt already answered | 31,913 | `AnswerCoversWholeSteer`, `PostOnlyWithDisposition` |
| SteerRefuseDrop | Mutant: a rejected steer stays in the turn instead of going back to the queue | 16,650 | `NoAnswerForRefusedSteer` |
| SteerErrorRequeue | Mutant: a steer error with an unknown outcome is put back in the queue | 38,534 | `NoDuplicateExecution` |
| SteerReturnVolatile | Mutant: a rejected steer is handed back from memory only, with no durable Returning record | 28,312 | `DurableOwner`, `EveryInputSettles` |
| Mutants | Session safety mutants together (source of `agy-acp.chart.mmd`) | n/a | the four session safety rules |

Mutants stops at its first break, and its later runs stop at 100,000 distinct states, so its rows do not give one total.

### Reachability

Steering is not vacuous in Design. Three witness invariants were checked alone against the Design constants, and each breaks (TLC exit 12):

- `NoSteeredAnswer`: a steered input is answered by its turn.
- `NoRefusedThenAnswered`: a rejected steer is answered later by a turn of its own.
- `NoUncertainCovered`: a steer with an unknown outcome is covered by its turn's post.

The witnesses are in the module and are not part of any committed config.

## Counterexamples

Charts:

- [Session chart](agy-acp.chart.mmd) draws the four session safety bug paths (`AgyAcpMutants`).
- [Steering chart](agy-acp-steer.chart.mmd) draws `AgyAcpSteerEarlyClose`.
- [Steer return chart](agy-acp-steer-return.chart.mmd) draws `AgyAcpSteerReturnVolatile`.

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
  5. the steer is then rejected and goes back to the queue, so the earlier post had no disposition behind it.

  In the design, only the last open prompt's response ends the turn.
- **SteerAfterClose:** prompt answered ("Turn answered, post pending") → the next input joins that closing turn → the turn posts while the joined prompt is open. In the design, an input arriving with no open prompt goes to the queue as today (BusyActiveTurn).
- **SteerRefuseDrop:** two prompts open → agy answers the first → the steer prompt is rejected → the turn posts and counts the rejected input as answered. In the design, the rejected input leaves the turn as Returning and goes back to the queue front. It is never re-steered into the same turn.
- **SteerErrorRequeue:** two prompts open → agy answers the first → "steer prompt fails with unknown outcome" → the input goes back to the queue → a second `session/prompt` for it. In the design, an unknown outcome keeps the input in the turn and is never sent again.
- **SteerReturnVolatile, safety:** two prompts open → "steer prompt rejected before execution: Rejected steer held in memory only" (7 steps). `DurableOwner` breaks at once.
- **SteerReturnVolatile, liveness:** two prompts open → agy answers the first → `!stop` → `session/cancel` → the steer is rejected and held in memory only → the turn posts → agy exits → AgentDesk restarts → boot settles the orphaned turn → "Input sent, nothing will answer it" forever. In the design, the same write that removes the input from the turn records Returning, and boot finishes any pending Returning record (§6.4.2).
- **NoWatchdog (liveness):** `!stop` → `session/cancel` → agy never answers → "Prompt open, cancel sent" forever. The design kills the child's own group after the cancel deadline (15 s) or the inactivity deadline (15 min).

## Not covered

- Repeated failures are bounded to one each. Both kinds of steer failure share one budget.
- Signals reaching other process groups are not modelled. Group isolation is a slice 1 process test (design §16), not a model property. The model only fixes the order: no new spawn while an orphan of the old child may run.
- Enqueueing a Returning input is one step. Its idempotence by message id is a design rule (§6.4.2), not modelled.
- The queue drop done by `/clear` is abstracted.
- Unsolicited output after a turn is dropped by design and not modelled.
- Model change (`session/set_config_option`) runs before a turn's first prompt on an idle session and is not modelled.
- Steering of HTTP iMessage inputs is out of scope: they stay queued.
- Whether agy reflects a steer in its answer cannot be observed at the protocol level.

Raw TLC output is in the lane folder `hl-20260925/i8339/tlc1c/`. These are model results, not Rust test results.
