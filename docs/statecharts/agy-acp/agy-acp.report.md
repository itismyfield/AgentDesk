# agy ACP channel session (#8339 design)

Code reference: `5021d25794` (origin/main). The ACP client does not exist yet, so this is a design model: the counterexamples are checked against the design and the T3 Code reference behaviour, not replayed against current Rust code. The output side (spool → writer → Discord receipt) stays the contract of [WriterAdoption AgySpec](../tui-o-writer/agy-adoption.report.md); here it is one receipt-guarded `Settle` step.

Design: [`docs/design/i8339/agy-acp.design.md`](../../design/i8339/agy-acp.design.md). Model SHA256 `dc338577da65c6bee81dd46a8868859f781d883af7ef71909f7585c5ff7c4431`.

## Scope

One person channel, two inputs, at most one AgentDesk restart, one agy exit, one `!stop`, one `/clear` and one refused `session/resume`. agy itself is not fair: it may never answer a prompt or a cancel. Only AgentDesk's own steps are fair: spawn, attach, prompt, cancel, watchdog, boot and settle. The child is AgentDesk's stdio child, so a restart kills it. A boot sees only durable state: the stored session id, the per-input sent mark (the spool `open` record), the terminal record and the receipts.

## Rules

| Rule | Meaning |
|---|---|
| `AtMostOnePost` | A person input never gets a second terminal post. |
| `NoDuplicateExecution` | A `session/prompt` is written at most once per input. |
| `NoForeignResponse` | An input is settled only by the response to its own prompt. |
| `NoPromptToClearedSession` | After `/clear`, no prompt is written to the retired session. |
| `PostOnlyWithDisposition` | A post always has a durable terminal record behind it. |
| `EveryInputSettles` (PROPERTY) | Every accepted input eventually gets its one terminal post: an answer, a failure notice or a cancel notice. |

## Results

All 11 configs ran with `scripts/ci/statecharts_tlc.py` (rc=0, 22.0 s in total). Each config's broken set matches its `expected.tsv` row.

| Config | What it fixes | Distinct states | Broken |
|---|---|---:|---|
| Design | Resident child, `session/resume`, FIFO queue between prompts, watchdog, incarnation fence | 18,634 | none |
| DesignSteer | Design plus a second person input sent as `session/prompt` while one runs | 19,442 | none |
| DesignLoad | Fallback `session/load` with replay dropped until the load response | 19,450 | none |
| PerTurn | Alternative: one `agy acp` child per turn, resumed each time | 24,942 | none |
| LoadLeak | Mutant: load replay is forwarded to the channel | 25,512 | `AtMostOnePost` |
| RerunSent | Mutant: boot re-queues an input whose prompt was already written | 32,824 | `NoDuplicateExecution` |
| SteerDrop | Mutant: the steered first prompt's `end_turn` is discarded | 20,294 | `EveryInputSettles` |
| NoWatchdog | Mutant: no inactivity or cancel deadline | 7,936 | `EveryInputSettles` |
| ClearKeeps | Mutant: `/clear` forgets the id but the live child keeps the old session | 17,058 | `NoPromptToClearedSession` |
| NoFence | Mutant: responses matched by JSON-RPC id alone across incarnations | 21,662 | `NoForeignResponse` |
| Mutants | The four safety mutants together (source of the chart) | 47,689 | all four safety rules above |

## Counterexamples

The [chart](agy-acp.chart.mmd) draws the four safety bug paths of `AgyAcpMutants`. Shortest paths, in the chart's words:

- **ClearKeeps:** a person sends → spawn → `session/new` → `/clear` → `session/prompt` goes to the retired session. The design closes the child (or detaches it) inside the clear path.
- **RerunSent:** prompt open → AgentDesk restarts → boot re-queues → spawn → `session/prompt` is written a second time. agy may already have run its tools. The design fails the orphan with a notice, as in #8274 §3.2.
- **LoadLeak:** an answer is posted → agy exits → spawn → `session/load` replays that answer as `session/update` → a second post. The design uses `session/resume`. Load is a fallback only, and it drops every update until the load response with no prompt open, as in T3 `AcpSessionRuntime.ts:1838-1863`.
- **NoFence:** prompt open → watchdog kills the child → respawn → resume → new prompt open → the dead child's buffered response settles the new input. JSON-RPC ids restart at 1 in every child. The design tags each reader and pending map with the incarnation and drops the old reader's lines.
- **SteerDrop (liveness):** two prompts open (steer) → agy answers the first with `end_turn` → the adapter keeps only the newest request → "Input sent, nothing will answer it" forever. The first input's inflight row and lease stay held, the same symptom as #8160. The design keeps every pending request id in the map until its own response arrives.
- **NoWatchdog (liveness):** `!stop` → `session/cancel` → agy never answers → "Prompt open, cancel sent" forever. The design kills the process group after the cancel deadline (T3 uses 15 s), or after the inactivity deadline (T3 uses 15 minutes). That kill fails or cancels the open prompts, and the next input resumes in a new child.

## Not covered

- Repeated failures are bounded to one each. Repeated refused resumes and unbounded restarts are not enumerated.
- The `/clear` model abstracts the queue drop (#8274 §3.6 and the clear path drop queued inputs with their own notice).
- Unsolicited output after a turn (background commands, `schedule` wakes) has no input to own it. The design logs it and does not post it, and the model does not include it.
- Model change (`session/set_config_option`) has the same shape as attach: it runs before `session/prompt` on an idle session, and it is not modelled.
- Multi-node owner moves: the stored id is node-local in the design. A move starts a new session, so context is lost but no input is.

Raw TLC output: lane-runs `hl-20260925/i8339/tlc/` (`run3.log`, `sc3-*`). These are model results, not Rust test results.
