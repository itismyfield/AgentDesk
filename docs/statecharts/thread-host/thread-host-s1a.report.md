# Thread host judgement, S1a (#8118)

Model `ThreadHostS1a.tla`, written against `94f32aeacf` plus the S1a change.
It covers only what S1a changes: who reads a thread's host, and from what.
Launch, placement, the first input and the activation of Herdr threads are not modelled; they belong to the later slice.

## Modes

- `Before`: main. Every reader asks the boot section for the channel's own entry.
- `Rev3`: design revision 3 taken literally. Every synchronous reader, the execution-absence read included, reads a process cache that is empty after a restart.
- `After`: the S1a code. A channel's own boot entry answers for it; a thread under a Herdr parent reads what the turn host last judged, and nothing judged is `Unknown`.

## Entries and transitions

Entries come from the design's entry table and nowhere else.

| Entry | Meaning |
|---|---|
| `OwnBootAbsent` | Own boot Herdr channel, row Missing or Retired, input after a restart |
| `HerdrThread` | Bound Herdr thread under a Herdr parent |
| `TmuxThread` | Live tmux thread under a Herdr parent |
| `Unresolved` | Thread whose parent the boot view holds unresolved |
| `LeftChannel` | Channel no boot policy routes to Herdr |

| Transition | Code |
|---|---|
| `Judge` | `services/turn_host.rs` `for_turn` → `resolve` → `is_live_thread_endpoint`; After: `session_host/thread_host.rs` `record_judged` |
| `Restart` | `session_host/thread_host.rs` `JUDGED` (process memory) |
| `BusyInput` | `discord/router/intake_gate/busy_inject.rs` `defer_owned_busy_view`, `discord/health/turn_deliver/inject.rs` `turn_mode_tmux`, `inject/herdr.rs` `bound_pane`, `turn_presence/activity/unbound/herdr_bound.rs` `admitted` |
| `JournalCheck` | `discord/queue_io/input_owner/claude_handoff.rs` `left_herdr` (entry and `recover`); After: `thread_host::left_herdr` |
| `StartAbsent` | `provider_dispatch/owner_input_start_wait.rs` → `turn_presence/activity/unbound.rs` `host_scope_result` (Missing → `HerdrFirstCreate`, own Retired → `HerdrRetired`) |
| `StartWatchAgain` | Rev3 only: the same read over an empty cache holds `host_unresolved`, and the start wait observes again |

## Rules

| Rule | Meaning |
|---|---|
| `JournalKept` | A journal is dropped only on a channel whose policy is tmux |
| `ConsumersAgree` | A reader answers what the turn host judged at the moment it read |
| `NoUnknownAsTmux` | With nothing judged, a Bound Herdr thread is never read as tmux |
| `BootStartResolves` (liveness) | An own boot Herdr channel with no execution starts its next input |
| `BoundThreadSeenAfterRestart` | A Bound Herdr thread is always read as Herdr (known gap, below) |

## Results

TLC 1.7.4 through `scripts/ci/statecharts_tlc.py`, 11 configs, none skipped.

| Config | Expected | Broken rules |
|---|---|---|
| `ThreadHostS1aBeforeAll` | RED | `JournalKept`, `ConsumersAgree`, `NoUnknownAsTmux` |
| `ThreadHostS1aRev3All` | RED | `BootStartResolves` |
| `ThreadHostS1aAfterAll` | GREEN | none |
| `ThreadHostS1aBeforeJournal` | RED | `JournalKept` |
| `ThreadHostS1aAfterJournal` | GREEN | none |
| `ThreadHostS1aBeforeBoot` | GREEN (control) | none |
| `ThreadHostS1aRev3Boot` | RED | `BootStartResolves` |
| `ThreadHostS1aAfterBoot` | GREEN | none |
| `ThreadHostS1aBeforeBusy` | RED | `ConsumersAgree`, `NoUnknownAsTmux` |
| `ThreadHostS1aAfterBusy` | GREEN | none |
| `ThreadHostS1aAfterRestartGap` | RED (known gap) | `BoundThreadSeenAfterRestart` |

## Before and after

| Behaviour | Before | After |
|---|---|---|
| Journal of a thread under a Herdr parent, or of an unresolved thread | Dropped as "left Herdr" | Kept |
| Journal of a channel that left Herdr | Dropped | Dropped |
| Busy and delivery readers of a judged Herdr thread | Read tmux while the turn host says Herdr | Read Herdr |
| The same readers with nothing judged | Read tmux | Read `Unknown`, never tmux |
| Own boot Herdr channel, no execution, input after restart | Starts | Starts (Rev3 would watch forever) |

## Known gap

`ThreadHostS1aAfterRestartGap` stays RED.
After a restart a Bound Herdr thread reads `Unknown` until its next turn judgement, because boot restore does not seed the judged record.
No Herdr thread can be created before the activation slice, so the gap has no production path yet.
The activation slice must seed the record from the boot restore of Bound rows, and this config then turns GREEN.

## Outside the model

- The readers one by one; the unit tests cover them.
- `covers` (`busy_inject.rs`), which S1a leaves on the caller's parent.
- The asynchronous row-based execution read, which the command slice introduces.
- Codex threads, launch, placement, the first input and tmux probe failure.
