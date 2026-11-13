# Thread child store retirement model

`WriterThreadRetire.tla` models one recorded thread child from the `thread_gc` judgement of its session row to a retired store and the next input, written against `ed19fbbb54`. `WriterThreadActive.tla` keeps its coarse `GcDelete`/`Retire` steps; this model splits them into the steps the code takes.

Values come only from the entry table below. `Observe` is the pin taken before the row delete, `GcDelete` the conditional SQL delete, `Begin`/`Debt`/`Stop`/`Write`/`Confirm` the owner's transition, `Arrive` every input placement or claimed-row boundary, `Supersede` the mark replacement by that input, `Boot` the store read at process start.

## Entry table

| Step | Code |
|---|---|
| GC judgement, pin before delete | `src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:76` (`gc_stale_thread_sessions_retiring_pg`), `ChildStore::pin` `:15` |
| Row delete | `src/db/dispatched_sessions/thread_gc.rs:92` (`delete_stale_thread_cleanup_row_pg`) |
| Request after the delete, successor read | `thread_gc.rs:129` (`retire_child_store`), `src/db/dispatched_sessions/thread_gc.rs:82` |
| Production GC callers | `src/services/discord/runtime_bootstrap/session_gc.rs:10`, `src/services/dispatched_sessions.rs:159` |
| Handle kept instead of detached | `src/services/tui_o/writer/host/child_retirement.rs:24` (`keep_stoppable`), callers `writer/host.rs:425`, `runtime_bootstrap/o_writer_host.rs:812,878` |
| Transition, debt read, stop and join | `writer/host/child_retirement.rs:92` (`retire_child`), `writer/host.rs:492` (`stop_and_join`) |
| Generation and epoch compare, mark publish | `src/services/tui_o/channel_policy/children/lifecycle.rs:124,151` |
| Mark write and read | `src/services/tui_o/store/thread_children/lifecycle.rs:31,46` |
| Boot rehydration | `channel_policy/children/lifecycle.rs:53`, called from `children.rs` `seeded_from_store` |
| Input: placement, claimed row | `src/services/tui_o/cutover.rs:192`, `cutover/intake_route.rs:30`, `cutover/intake_route/input.rs:36`, `src/services/cluster/intake_worker/o_input.rs:53` |
| Supersede | `channel_policy/children/lifecycle.rs:182,205` |
| Writer and refresh candidacy | `cutover.rs:44`, `runtime_bootstrap/owner_execution.rs:80,159`, read-only hold `cutover/intake_route.rs:101` |

## Rules

| Rule | Meaning |
|---|---|
| `ExactlyOnce` | an input is posted once, never twice |
| `NoStalePost` | no writer posts for a retired store or under an older lifecycle generation |
| `RetiredNeedsMark` | a child is retired only with a durable mark of its current generation |
| `MarkHonored` | a durable retirement mark is read as retired by a running process |
| `NoLateMark` | no mark is written for a request an input has overtaken |
| `DebtKept` | no mark is written while captured output is unposted |
| `RequestAfterDelete` | only an exact row delete makes a request |
| `RetireResolves` (liveness) | a request ends retired or visibly refused; a crash loses it |
| `InputAnswered` (liveness) | an input after retirement is posted by the new generation's writer |

Fairness is weak fairness on `Supersede`, `StartWriter`, `Post`, `OldEnd`, `GcDelete`, `Begin`, `Debt`, `Stop`, `Write`, `Confirm` and `Boot`. No SYMMETRY. Bounds: two inputs, two GC judgements, one crash, one unit of owed output.

## Results

Plain TLC 1.7.4 through `scripts/ci/statecharts_tlc.py --base HEAD~1 --jobs 4`, 9 configs, 30.0 s, on 2026-11-13.

| cfg | distinct states | result |
|---|---:|---|
| WriterThreadRetireBefore | 814 | `RetireResolves` broken: no owner consumes a GC success |
| WriterThreadRetireFixed | 2400 | GREEN, both liveness properties included |
| WriterThreadRetireNoCas | 562 | `NoLateMark`, `DebtKept`, `MarkHonored`: no generation and epoch compare |
| WriterThreadRetireNoMark | 580 | `RetiredNeedsMark`: retired with no durable mark |
| WriterThreadRetireNoStop | 580 | `NoStalePost`, `ExactlyOnce`: stop without join |
| WriterThreadRetireNoSupersede | 2150 | `InputAnswered`: the next input leaves the mark |
| WriterThreadRetireNoDelete | 13 | `RequestAfterDelete`, `DebtKept`: a refused delete still requests |
| WriterThreadRetireNoRehydrate | 813 | `MarkHonored`: a boot ignores the mark |
| WriterThreadRetireNoDebt | 552 | `DebtKept`: retired over unposted output |

## Before and after

| | Before (`ed19fbbb54`) | After |
|---|---|---|
| Row deleted by `thread_gc` | nothing reaches O; the child stays a writer and refresh candidate | the exact writer generation is stopped and joined, then the store is marked retired |
| Input between the pin and the mark | not applicable | the request is refused or the mark is superseded; the input is held, never dropped |
| Restart over a retired store | not applicable | read back as retired; no writer candidate |
| Next input on a retired store | not applicable | a new generation over the same `init`, ledger and cursors |

## Findings carried into the code

- `NoDelete` and `NoCas` both break `DebtKept`: the gap between the debt read and the mark is safe only because the row is gone and no input arrived. The code therefore pins before the delete, compares the epoch at the transition and again around the mark write.
- `NoStop` breaks `ExactlyOnce` as well as `NoStalePost`: a stop that is not joined lets the old writer finish a POST beside the new generation.
- `Write` and `Confirm` are separate steps because the mark is written with no entry lock held; an input between them is held and then supersedes the mark.

## Scope

One child and one parent. Sibling and parent isolation are checked in the Rust tests. The single-writer-per-channel claim is taken from the existing readiness model, not re-modelled. A retirement request does not survive a crash, and no retry is modelled or implemented.

Logs: `~/.adk/release/lane-runs/hl-20260925/i8118/logs/ob2-tlc`.
