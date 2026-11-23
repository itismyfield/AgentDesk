# agy ACP channel session with steering (#8339 design)

<!-- doc-path-refs: external AcpSessionRuntime.ts -->

Code reference: `5021d25794` (origin/main). The ACP client does not exist yet, so this is a design model. Its counterexamples are checked against the design and against the T3 Code reference behaviour. They are not replayed against current Rust code. The output side (spool → writer → Discord receipt) stays the contract of [WriterAdoption AgySpec](../tui-o-writer/agy-adoption.report.md); in this model it is one receipt-guarded `Settle` step.

Design: [`docs/design/i8339/agy-acp.design.md`](../../design/i8339/agy-acp.design.md). Model SHA256 `ebbc330fd7ca5cbe35f9119256fc1571d35d23b6d00159ce20e3fbf79ce02fb3`.

## Scope

The model covers one person-driven antigravity destination with two inputs. A person input that arrives while the turn has an open prompt is **steered**: it is written as another `session/prompt`, joins that turn, and is covered by the turn's single terminal post. The turn ends only when none of its prompts is still open.

Each fault happens at most once: one AgentDesk restart, one agy exit, one `!stop`, one `/clear`, one refused `session/resume`, one failed steer prompt and, in the design configs and MarkNoRetry (`ReadFails = 1`), one receipt lookup that cannot read the row.

A failed steer prompt (`SteerErrored`) is either kind from design §6.4.1: rejected before execution, or an unknown outcome. Both are handled the same way, because the scope cut B-noreturn takes out the return to the queue. The input stays a member of the turn and is never sent again. The turn's one post covers it with a notice line, recorded as `noticed`. The kind only picks the wording, so the model does not split it.

Every input, the turn's first one included, goes through three actor steps (design §6.2-3): `Accept` (the input joins a turn, or leads a new one), `Save` (its origin is written ahead into the turn's durable row) and `Wire` (its `session/prompt` is written, or dropped as a failed steer when its turn is stopping or sealed). The actor handles one input at a time, so agy's responses wait while an input is accepted or saved. A restart after `Accept` returns the input to the queue; a restart after `Save` leaves it in its turn unsent, and boot fails that turn.

The bridge's in-memory copy predates these writes, so the completion receipt merges the turn's latest durable origins under the same identity and nonce (§6.2-3b). On `!stop` the bridge leaves its loop at once and looks up the receipt before agy answers the cancel (`EarlyLookup`); otherwise `Settle` looks it up. Either lookup first seals the turn's steer acceptance and waits for an accepted input's save (§6.2-3a). A lookup that cannot read the row records nothing and keeps the row (`held`), marked or not (`marked`); the next admission marks it again and records it (`AdmitSettle`, §6.2-3c). After a restart, the boot settler reads the durable row itself. The stream tick (`StreamTick`) only matters without the merge, so only the memory mutant explores it.

agy is not fair. It may never answer a prompt or a cancel. It may answer the older prompt first (steer) or finish it normally before the newer one (queueing agy 1.4). Only AgentDesk's own steps are fair: spawn, attach, prompt, cancel, watchdog, boot and settle.

The child runs in its own process group, so on Unix an AgentDesk restart may leave it running (`orphan`). Boot must sweep that group (`SweepOrphan`) before it spawns a new child. On Windows the dcserver job object closes the child tree when dcserver ends, so no orphan exists there. The orphan path is a superset of that, so one model covers both platforms. The platform kill calls themselves (Unix group signals, Windows `taskkill /T`) are outside the model (design §5.2-13). A boot sees only durable state:

- the stored session id
- the turn membership written before each prompt (spool `open` and the turn's origin list, the first and steered inputs included)
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
| `DurableOwner` | An input that was written ahead is always a member of a turn whose origin list holds it. |
| `NoReviveAfterClear` | An input that had been sent when `/clear` ran is never queued again, also when its steer fails after the clear. |
| `NoMissingOrigin` | Every input a post covered, the first and steered ones included, is named by that turn's receipt (`consumed_origin_ids`), or its turn's row is still kept for the receipt. |
| `EveryInputSettles` (PROPERTY) | Every accepted input, steered inputs included, is eventually covered by one terminal post: an answer, a failure notice or a cancel notice. |

## Results

All 17 configs ran with `scripts/ci/statecharts_tlc.py`. Each config's broken set matches its `expected.tsv` row. State counts come from the `statechart` command on the same model.

| Config | What it fixes | Distinct states | Broken |
|---|---|---:|---|
| Design | Resident child in its own group, `session/resume`, steering, a failed steer stays in the turn with a notice, every origin written ahead, steer acceptance sealed before the receipt lookup, receipt merges durable origins, unmarked kept row re-marked at admission, orphan sweep, watchdog, incarnation fence | 216,278 | none |
| DesignLoad | Fallback `session/load` with replay dropped until the load response | 219,529 | none |
| PerTurn | Alternative: one `agy acp` child per turn | 281,803 | none |
| LoadLeak | Mutant: load replay is forwarded to the channel | 79,337 | `AtMostOnePost` |
| RerunSent | Mutant: boot re-queues the inputs of a turn whose prompts were written | first break | `NoDuplicateExecution`, `NoReviveAfterClear` |
| ClearKeeps | Mutant: `/clear` forgets the id but the live child keeps the old session | 68,165 | `NoPromptToClearedSession` |
| NoFence | Mutant: responses matched by JSON-RPC id alone across incarnations | 74,046 | `NoForeignResponse` |
| NoWatchdog | Mutant: no inactivity or cancel deadline | 35,860 | `EveryInputSettles` |
| SteerEarlyClose | Mutant: the steered first prompt's `end_turn` closes the turn | 66,936 | `AnswerCoversWholeSteer`, `NoAnswerForRefusedSteer` |
| SteerAfterClose | Mutant: an input is steered into a turn whose last prompt already answered | 58,704 | `AnswerCoversWholeSteer`, `NoAnswerForRefusedSteer` |
| SteerErrorRequeue | Mutant: a failed steer is put back in the queue | first break | `NoDuplicateExecution`, `NoReviveAfterClear` |
| SteerErrorSilent | Mutant: the turn post covers a failed steer without its notice | 65,608 | `NoAnswerForRefusedSteer` |
| ReceiptFromMemory | Mutant: the completion receipt uses only the bridge's in-memory origins | first break | `NoMissingOrigin` |
| SealOff | Mutant: the receipt is looked up without sealing steer acceptance | 67,441 | `NoMissingOrigin` |
| LeaderOriginOff | Mutant: the first input's origin is never written ahead | 49,388 | `NoMissingOrigin` |
| MarkNoRetry | Mutant: a kept row whose mark was not written is never re-marked | 216,278 | `EveryInputSettles` |
| Mutants | Session safety mutants together (source of `agy-acp.chart.mmd`) | first break | the four session safety rules and `NoReviveAfterClear` |

"First break" rows stop at their first broken rule, and their later runs stop at 100,000 distinct states, so they give no one total.

SteerEarlyClose and SteerAfterClose break `NoAnswerForRefusedSteer` too: the turn is posted before the steer prompt fails, so that post has no notice. In Design, `AnswerCoversWholeSteer` rules out the early post. RerunSent and Mutants also break `NoReviveAfterClear`, because boot re-queues a written input that `/clear` had already met.

### Reachability

Steering is not vacuous in Design. Seven witness invariants were checked alone against the Design constants, and each breaks (TLC exit 12):

- `NoSteeredAnswer`: a steered input is answered by its turn.
- `NoUncertainCovered`: a failed steer is covered by its turn's post.
- `NoMergeOnCancel`: a cancelled turn's receipt names a steered input only through the durable merge.
- `NoSteerFailAfterClear`: a steer that fails after `/clear` is covered by its turn's post. This is the path of the 1d delta review's counterexample, and in Design it ends in one post with no requeue.
- `NoCrashAfterSave`: AgentDesk restarts after an input's origin is written ahead and before its prompt is written.
- `NoUnsentSteer`: a steer accepted before `!stop` is saved, not written, and covered by the turn's post with its failure notice.
- `NoRetrySettle`: a kept row without its mark waits on an admission, which marks it again.

The witnesses are in the module and are not part of any committed config.

## Counterexamples

Charts:

- [Session chart](agy-acp.chart.mmd) draws the four session safety bug paths (`AgyAcpMutants`).
- [Steering chart](agy-acp-steer.chart.mmd) draws `AgyAcpSteerEarlyClose`.
- [Steer requeue chart](agy-acp-steer-requeue.chart.mmd) draws `AgyAcpSteerErrorRequeue`.
- [Receipt chart](agy-acp-receipt.chart.mmd) draws `AgyAcpReceiptFromMemory`.
- [Seal chart](agy-acp-seal.chart.mmd) draws `AgyAcpSealOff`.

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
- **ReceiptFromMemory (`NoMissingOrigin`, 8 steps):** one input accepted, saved and written → agy answers → the turn posts and commits a receipt without it, because the bridge's copy predates the write-ahead. In the design, the receipt merges the durable row under the same identity and nonce (§6.2-3b).
- **SealOff (`NoMissingOrigin`, 14 steps):** the first prompt open → the second input accepted → `!stop` → the bridge looks up the receipt → the second input's origin is saved → agy answers → the post covers both, the receipt names only the first. This is the 1f review's order. In the design, the lookup waits for the accepted input's save, and an input after the seal waits for the next turn (§6.2-3a).
- **LeaderOriginOff (`NoMissingOrigin`, 8 steps):** one input → prompt → answer → the receipt has no origin for it, because only its Discord source ids were kept (§6.2-3).
- **MarkNoRetry (liveness):** the receipt lookup cannot read the row → the post goes out and the row stays without its mark → "Row kept for its receipt" forever, so the next input never starts. In the design, the next admission marks the row again and records it (§6.2-3c).
- **NoWatchdog (liveness):** `!stop` → `session/cancel` → agy never answers → "Prompt open, cancel sent" forever. The design kills the child's own group after the cancel deadline (15 s) or the inactivity deadline (15 min).

## Not covered

- Repeated failures are bounded to one each. Both kinds of steer failure share one budget.
- Signals reaching other process groups are not modelled. Group isolation is a slice 1 process test (design §16), not a model property. The model only fixes the order: no new spawn while an orphan of the old child may run.
- The two kinds of steer failure are one action. Their only difference is the notice wording (§6.4.1).
- The Unix sweep's hold branch (probe error: no spawn, failure notice) is a slice 1 process test, not modelled.
- The row belonging to another turn in the receipt merge is a slice 2b test, not modelled.
- The write-ahead save failing is a slice 2b test. In that case the prompt is not written: a steer goes to the queue unsent, a first input fails its turn.
- A `!stop` between the first input's save and its prompt is a slice 2b test: `UserStop` needs an open prompt.
- The automatic retry path and an abnormal bridge `Drop` (design §6.2-3c table) are slice 2b tests, not modelled.
- The queue drop done by `/clear` is abstracted.
- Unsolicited output after a turn is dropped by design and not modelled.
- Model change (`session/set_config_option`) runs before a turn's first prompt on an idle session and is not modelled.
- Steering of HTTP iMessage inputs is out of scope: they stay queued.
- Whether agy reflects a steer in its answer cannot be observed at the protocol level.

Raw TLC output is in the lane folder `hl-20260925/i8339/tlc1g/`. These are model results, not Rust test results.
