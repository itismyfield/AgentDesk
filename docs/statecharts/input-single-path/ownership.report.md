# InputOwnership — who answers an input pasted into a Claude pane (#7676 A2+D)

Code versions modelled:

| `Fix` | Commit | Config |
|---|---|---|
| `main` | a1c `0b0fbcbfbe` | `InputOwnershipMain.cfg` |
| `a2d1` | first A2+D change `524adb6190` | `InputOwnershipA2d1.cfg` |
| `a2d2` | its repair `e6d97a192f` | `InputOwnershipA2d2.cfg` |
| `a2d3` | the second repair `4def1c6289` | `InputOwnershipA2d3.cfg` |
| `a2d4` | the third repair | `InputOwnership.cfg` |
| `c` | slice C, its tested transitions only | `InputOwnershipC.cfg` |

What the model covers:

- One input, from Discord or from `turn/start` (an iMessage reply).
- One Claude tmux channel, in turn mode or legacy mode.
- Every combination of mailbox idle/active and transcript idle/busy.
- A screen that may refuse keys (modal, unread capture, copy mode) and later clear with no event of its own.
- A read of that held pane, or of the queue, that fails once.
- An earlier held front's watch still running when the input is held.
- A person's draft that C-s cannot be proven to keep.
- An earlier input with the same author and body; its own caller may have declared a reader, and it may have been absorbed.
- A long running turn that outlives the 10 s pending text.
- The receipt thread's 3 s window.
- The relay reading the input's row once.
- In `c`, an input an older binary retained, with or without its resend ask posted, and the boot that finds it.

Run it like this (exit 1 means a rule breaks):

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
for c in Main A2d1 A2d2 A2d3 "" C; do
  ~/.claude/skills/statechart/scripts/statechart InputOwnership.tla --config InputOwnership$c.cfg \
    --view ownership.view.mjs --out /tmp/sc-own-${c:-A2d4}
done
```

## Rules

| Rule | Reader's sentence | main | a2d1 | a2d2 | a2d3 | a2d4 | c |
|---|---|---|---|---|---|---|---|
| NoDuplicate | The input is pasted at most once. | holds | holds | holds | holds | holds | holds |
| NoRetryRequired | No input ends asking the person to send it again, besides one an older binary already asked. | breaks (1) | holds | holds | holds | holds | holds |
| AnswerOnce | An answered input was posted once. In turn mode only the O writer posts it; in legacy, the relay or the reader the input's turn set up posts it. | breaks (5) | breaks (4) | breaks (4) | holds | holds | holds |
| HandedHasRow | Time alone never makes the input Handed. | holds | holds | holds | holds | holds | holds |
| StartNotDropped | A turn/start into a busy pane is never dropped for want of an idle observer. | breaks (1) | breaks (1) | holds | holds | holds | holds |
| NoTimedRetry | A held input is handed again only by an event, never by a fixed timer alone. | breaks (3) | breaks (3) | holds | holds | holds | holds |
| DraftToldOnce | An owned paste beside a draft C-s could not park tells its original once. | breaks (1) | breaks (1) | holds | holds | holds | holds |
| HeldResumes | An input held by its screen is not left waiting once that screen would submit. | holds (timer) | holds (timer) | breaks (4) | breaks (5) | holds | holds |
| NoSilentLoss | Once nothing else happens, the input was answered or an older binary already asked to resend it. | breaks (3) | breaks (3) | breaks (3) | breaks (3) | breaks (3) | breaks (3, same path) |
| NoSilentLossObserved | On a turn-mode channel with an idle observer, once all is quiet the input was answered. | breaks | holds (timer) | breaks | breaks | breaks | holds |
| ToldStays | An input whose resend ask an older binary posted is never pasted again. | holds | holds | holds | holds | holds | holds |

NoSilentLoss still breaks in `c` (3 steps): with no idle observer, the idle submit holds the front (`idle_unobserved`), which slice A owns.

Each break shows the step count of its shortest path.

States reached: main 30,272; a2d1 39,328; a2d2 22,832; a2d3 65,280; a2d4 63,744; c 65,924. `ownership.chart.mmd` is the main chart, drawing the bug paths only.

### What `c` adds, and the test that drives each step through the code

Only a step a test drives through its real entry point is in the `c` model; a step read in the code but not tested is left nondeterministic.

| `c` step | Code | Test |
|---|---|---|
| A turn-mode turn ends: the supervisor reads the V2 front and wakes the owner once (`TurnEnds`, `RC`) | `turn_presence/supervisor.rs` `waiting_input`, `wake`, `Action::WakeOwner` | `supervisor_tests::an_owned_front_wakes_its_owner_once_at_the_idle_transition_and_never_by_time` |
| No timer wakes an owned queue (no `TimerFires` in `c`) | supervisor `wake` (`due` needs `!owned`), `queue_io/backstop.rs` `owned` | the same supervisor test; `owned_backstop_tests::no_backstop_schedules_or_fires_a_kick_at_an_owned_queue` |
| A held pane's recovery wakes the drive (`WatchSees`) for Claude and Codex modals | `effect.rs` `arm_screen`, `effect/codex.rs` `composer`, `held_watch.rs` | `owner_input_tests::a_closing_modal_wakes_the_drive_to_deliver_its_held_front_once`, `owner_input_codex_tests::a_codex_modal_that_closes_wakes_the_drive_to_deliver_once` |
| A boot requeues an input retained before its ask went out, and keeps a told one (`BootRequeues`) | `v2/retained.rs` `requeueable`, `recovery.rs` | `notice_tests::told_retained_inputs_stay_and_an_untold_one_is_handed_on_once_pg`, `requeue_tests::only_an_untold_proven_unsent_input_requeues_and_a_told_one_stays` |
| A legacy channel's turn end: not assumed. The watcher epilogue's schedule becomes one owner wake in code (`backstop.rs`), but the epilogue itself is not driven by a test | — | — |

## Counterexamples, replayed against the code

1. **NoRetryRequired breaks on main.**
   - Path: mailbox active, transcript idle. `hand_busy` → `inject_owned` was `BusyOnly`.
   - `NotBusy` is not a held reason, so `effect::run` retains the original and asks the person to resend.
   - Fixed in a2d1: the owned entry is `AnyTurn`.
2. **StartNotDropped breaks on main and a2d1.**
   - Path: a `turn/start` on a pane busy with a turn AgentDesk did not start. `execute_owned` registered `busy=false` and `Discord`, so with no idle observer the turn ended with a failure.
   - Fixed in a2d2. It re-reads the pane and keeps the front's origin. A busy pane takes the owned busy entry, with a reader of the row the input takes.
3. **NoTimedRetry breaks on main and a2d1.**
   - Path: a held refusal spawned `sleep(recheck)` → `wake`, so an unchanged screen rebuilt the prompt every 5 s.
   - Fixed in a2d2: the timer is gone.
4. **HeldResumes breaks on a2d2.**
   - Path: the front is held by copy mode (or an unread capture) while a person's long turn runs, and that turn ends without a wake. The copy mode then closes, and nothing hands the front again.
   - Fixed in a2d3:
     - A held refusal leaves a probe of that pane (`held_watch::arm`).
     - The drive watches it, every 2 s with no handoff.
     - The first read that would submit wakes the drive once.
     - The watch ends when that front no longer leads or the pane is gone.
5. **DraftToldOnce breaks on main and a2d1.**
   - Path: a draft beside an image chip reads `Composer::Unknown`.
   - Fixed in a2d2: the frame is pasted, and the original gets one outbox notice.
6. **AnswerOnce breaks on main.** Every recorded Claude frame was `Joined { first: false }`, so a queued input that opened its own turn had no relay.
7. **AnswerOnce breaks on a2d1.** An earlier input with the same body made two candidate attempts for the row, so the row was Joined and nobody answered.
8. **AnswerOnce breaks on a2d2, in three ways.**
   - **Same-body twin:** an absorbed twin's caller declared a reader. Its pending text, matched by body, still marked this input's later user row as declared, so nobody answered it.
   - **Aged declaration:** the input's own declaration was pending text kept 10 s. After a long turn the row was read as undeclared, so the relay and the caller's reader both answered.
   - **Absorbed input:** an input taken into a person's running turn as a queued command still had its caller's reader post that turn's tail, a second body beside the relay's.
   - Fixed in a2d3:
     - The relay reads the declaration of the attempt that took the row (`history::has_reader`) and opens no turn for it (`Claim::Declared`).
     - A busy reader whose input became a queued command posts no body (`busy_owner_observer`).
9. **HeldResumes breaks on a2d3.**
   - Path: the front is held by copy mode, the turn ends, copy mode closes, and one read of the pane (or of the queue) fails. a2d3 read the failure as a gone pane and dropped the watch.
   - A second path: an earlier front's watch ends because that front was cancelled, and drops the watch this input's hold had just armed.
   - Fixed in a2d4:
     - A read is Ready, Blocked, Gone or Unknown. Only tmux reporting the pane dead or its session missing is Gone.
     - A failed pane or queue read keeps the probe for the next look.
     - Each hold the drive names gets a generation, and a watch ends only its own.
10. **NoSilentLossObserved breaks on a2d4.**
   - Path: a person's turn holds the front, then ends, and nothing wakes the drive.
   - In `c`, on a turn-mode channel the presence supervisor reads the owner's V2 front and wakes the owner once at the idle transition.
   - Mutant `noBoundaryWake` breaks it again.
11. **NoSilentLoss breaks in every version.**
   - Path: an idle pane whose observer cannot be built holds the front, and a native turn AgentDesk did not start may end without waking the drive.
   - This is the remaining event-wake gap of the A1 review (P2-1), owned by slice C.

## Mutants of the c model

| Mutant | What it removes | Breaks besides NoSilentLoss |
|---|---|---|
| noBoundaryWake | the supervisor's V2 front read and owner wake | NoSilentLossObserved |
| requeueTold | the boot keeps a told input retained | ToldStays |
| unknownEnds | a failed read keeps the watch | HeldResumes |
| noGeneration | a watch ends only its own hold | HeldResumes |
| noWatch | the held pane's watch | HeldResumes |
| timer | no fixed recheck | NoTimedRetry |

## Mutants of the a2d4 model

Each mutant breaks a rule besides the known NoSilentLoss gap.

| Mutant | What it removes | Breaks |
|---|---|---|
| noWatch | the held pane's watch | HeldResumes |
| watchBlind | the watch wakes on an unchanged screen | NoTimedRetry |
| unknownEnds | a failed read ends the watch | HeldResumes |
| noGeneration | an old watch's end drops a newer hold | HeldResumes |
| ttlDecl | the declaration read from 10 s pending text | AnswerOnce |
| bodyDecl | the declaration matched by body to any attempt | AnswerOnce |
| commandReads | a reader posts a queued command's turn | AnswerOnce |
| pendOnBusy | the owned busy entry declares an Answer nobody reads | AnswerOnce |
| blanketJoin | Claude frames joined whatever the row | AnswerOnce |
| receiptKind | ownership read from the 3 s receipt kind | AnswerOnce |
| timeHanded | Handed after the window with only an enqueue | HandedHasRow, AnswerOnce |
| turnLegacy | the legacy relay also answers on turn mode | AnswerOnce |
| startIdle | the turn/start's turn read as idle (`busy=false`) | StartNotDropped |
| timer | the drive's fixed recheck restored | NoTimedRetry |
| noNotice | the draft notice skipped | DraftToldOnce |
| twoCandidates | a row with two possible owners joined | AnswerOnce |

Left out of the model:

- Codex and Herdr panes: the model's pane is Claude tmux; slice C arms the same watch on their modals.
- Restart replay of the relay's claim ledger.
- Two inputs racing one paste.
- A screen that never clears.
- A gone pane: it has no screen that would submit.
