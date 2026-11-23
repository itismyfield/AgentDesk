# agy ACP channel session with steering (#8339 design)

<!-- doc-path-refs: external AcpSessionRuntime.ts -->

Code reference: `5021d25794` (origin/main). The ACP client does not exist yet, so this is a design model. Its counterexamples are checked against the design and against the T3 Code reference behaviour. They are not replayed against current Rust code. The output side (spool → writer → Discord receipt) stays the contract of [WriterAdoption AgySpec](../tui-o-writer/agy-adoption.report.md); in this model it is one receipt-guarded `Settle` step.

Design: [`docs/design/i8339/agy-acp.design.md`](../../design/i8339/agy-acp.design.md). Model SHA256 `dcd67b738ded6b5f787d0ac67a9182955032a4a2154706c91b37af95837da986`.

## Scope

The model covers one person-driven antigravity destination with two inputs. A person input that arrives while the turn has an open prompt is **steered**: it is written as another `session/prompt`, joins that turn, and is covered by the turn's single terminal post. The turn ends only when none of its prompts is still open.

Each fault happens at most once: one AgentDesk restart, one agy exit, one `!stop`, one `/clear`, one refused `session/resume` and one failed steer prompt.

A failed steer prompt (`SteerErrored`) is either kind from design §6.4.1: rejected before execution, or an unknown outcome. Both are handled the same way, because the scope cut B-noreturn takes out the return to the queue. The input stays a member of the turn and is never sent again. The turn's one post covers it with a notice line, recorded as `noticed`. The kind only picks the wording, so the model does not split it.

agy is not fair. It may never answer a prompt or a cancel. It may answer the older prompt first (steer) or finish it normally before the newer one (queueing agy 1.4). Only AgentDesk's own steps are fair: spawn, attach, prompt, cancel, watchdog, boot and settle.

The child runs in its own process group, so on Unix an AgentDesk restart may leave it running (`orphan`). Boot must sweep that group (`SweepOrphan`) before it spawns a new child. On Windows the dcserver job object closes the child tree when dcserver ends, so no orphan exists there. The orphan path is a superset of that, so one model covers both platforms. The platform kill calls themselves (Unix group signals, Windows `taskkill /T`) are outside the model (design §5.2-13). A boot sees only durable state:

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
| `EveryInputSettles` (PROPERTY) | Every accepted input, steered inputs included, is eventually covered by one terminal post: an answer, a failure notice or a cancel notice. |

## Results

All 13 configs ran with `scripts/ci/statecharts_tlc.py`. Each config's broken set matches its `expected.tsv` row. State counts come from the `statechart` command on the same model.

| Config | What it fixes | Distinct states | Broken |
|---|---|---:|---|
| Design | Resident child in its own group, `session/resume`, steering, a failed steer stays in the turn with a notice, orphan sweep, watchdog, incarnation fence | 24,375 | none |
| DesignLoad | Fallback `session/load` with replay dropped until the load response | 25,071 | none |
| PerTurn | Alternative: one `agy acp` child per turn | 31,933 | none |
| LoadLeak | Mutant: load replay is forwarded to the channel | 30,832 | `AtMostOnePost` |
| RerunSent | Mutant: boot re-queues the inputs of a turn whose prompts were written | 57,511 | `NoDuplicateExecution`, `NoReviveAfterClear` |
| ClearKeeps | Mutant: `/clear` forgets the id but the live child keeps the old session | 22,884 | `NoPromptToClearedSession` |
| NoFence | Mutant: responses matched by JSON-RPC id alone across incarnations | 29,113 | `NoForeignResponse` |
| NoWatchdog | Mutant: no inactivity or cancel deadline | 12,307 | `EveryInputSettles` |
| SteerEarlyClose | Mutant: the steered first prompt's `end_turn` closes the turn | 25,322 | `AnswerCoversWholeSteer`, `NoAnswerForRefusedSteer` |
| SteerAfterClose | Mutant: an input is steered into a turn whose last prompt already answered | 21,365 | `AnswerCoversWholeSteer`, `NoAnswerForRefusedSteer` |
| SteerErrorRequeue | Mutant: a failed steer is put back in the queue | 45,052 | `NoDuplicateExecution`, `NoReviveAfterClear` |
| SteerErrorSilent | Mutant: the turn post covers a failed steer without its notice | 24,375 | `NoAnswerForRefusedSteer` |
| Mutants | Session safety mutants together (source of `agy-acp.chart.mmd`) | 87,957 | the four session safety rules and `NoReviveAfterClear` |

Mutants stops at its first break, and its later runs stop at 100,000 distinct states, so its rows do not give one total.

SteerEarlyClose and SteerAfterClose break `NoAnswerForRefusedSteer` too: the turn is posted before the steer prompt fails, so that post has no notice. In Design, `AnswerCoversWholeSteer` rules out the early post. RerunSent and Mutants also break `NoReviveAfterClear`, because boot re-queues a written input that `/clear` had already met.

### Reachability

Steering is not vacuous in Design. Three witness invariants were checked alone against the Design constants, and each breaks (TLC exit 12):

- `NoSteeredAnswer`: a steered input is answered by its turn.
- `NoUncertainCovered`: a failed steer is covered by its turn's post.
- `NoSteerFailAfterClear`: a steer that fails after `/clear` is covered by its turn's post. This is the path of the 1d delta review's counterexample, and in Design it ends in one post with no requeue.

The witnesses are in the module and are not part of any committed config.

## Counterexamples

Charts:

- [Session chart](agy-acp.chart.mmd) draws the four session safety bug paths (`AgyAcpMutants`).
- [Steering chart](agy-acp-steer.chart.mmd) draws `AgyAcpSteerEarlyClose`.
- [Steer requeue chart](agy-acp-steer-requeue.chart.mmd) draws `AgyAcpSteerErrorRequeue`.

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
- **NoWatchdog (liveness):** `!stop` → `session/cancel` → agy never answers → "Prompt open, cancel sent" forever. The design kills the child's own group after the cancel deadline (15 s) or the inactivity deadline (15 min).

## Not covered

- Repeated failures are bounded to one each. Both kinds of steer failure share one budget.
- Signals reaching other process groups are not modelled. Group isolation is a slice 1 process test (design §16), not a model property. The model only fixes the order: no new spawn while an orphan of the old child may run.
- The two kinds of steer failure are one action. Their only difference is the notice wording (§6.4.1).
- The Unix sweep's hold branch (probe error: no spawn, failure notice) is a slice 1 process test, not modelled.
- The write-ahead save of a steered input's origin failing is a slice 2b test. In that case the prompt is not written and the input goes to the queue unsent.
- The queue drop done by `/clear` is abstracted.
- Unsolicited output after a turn is dropped by design and not modelled.
- Model change (`session/set_config_option`) runs before a turn's first prompt on an idle session and is not modelled.
- Steering of HTTP iMessage inputs is out of scope: they stay queued.
- Whether agy reflects a steer in its answer cannot be observed at the protocol level.

Raw TLC output is in the lane folder `hl-20260925/i8339/tlc1e/`. These are model results, not Rust test results.
