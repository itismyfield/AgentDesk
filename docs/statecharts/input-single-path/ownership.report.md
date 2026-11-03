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
- A screen that may refuse keys (modal, unread capture, copy mode), or a cause that refuses before any key (a lock, a buffer load, a missing transcript), and later clears with no event of its own.
- A read of that held pane, or of the queue, that fails once.
- An earlier held front's watch still running when the input is held.
- A person's draft that C-s cannot be proven to keep.
- An earlier input with the same author and body; its own caller may have declared a reader, and it may have been absorbed.
- A long running turn that outlives the 10 s pending text.
- The receipt thread's 3 s window.
- The relay reading the input's row once.
- In `c`, an input an older binary retained, with or without its resend ask posted, and the boot that finds it.
- In `c`, a cause that clears before its watch's first read, a pane that goes away while its cause stands, and a live idle pane under another account than the channel's.
- In `c`, a busy turn-mode pane's frame ledger refusal: a `LoadFailed` no read sees, told apart from a buffer that failed to load.

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
| HeldResumes | An input held by its screen or a cause is not left waiting once that clears. | holds (timer) | holds (timer) | breaks (4) | breaks (4) | breaks (4) | holds |
| NoSilentLoss | Once nothing else happens, the input was answered or an older binary already asked to resend it. | breaks (3) | breaks (3) | breaks (3) | breaks (3) | breaks (3) | holds |
| NoSilentLossObserved | On a turn-mode channel with an idle observer, once all is quiet the input was answered. | breaks (3) | holds (timer) | breaks (4) | breaks (4) | breaks (4) | holds |
| ToldStays | An input whose resend ask an older binary posted is never pasted again. | holds | holds | holds | holds | holds | holds |
| ForeignUntouched | Nothing is pasted into a pane under another account than the channel's. | — | — | — | — | — | holds |
| GoneWatchEnds | A watch whose pane is gone ends once nothing else is left to happen. | — | — | — | — | — | holds |

Each break shows the step count of its shortest path. `—`: the version has no such state, and its config does not check the rule.

- Quiet now also needs the pane's refusal cleared and no watch left to read it, so a screen that never clears is not counted as a loss.
- a2d4 breaks HeldResumes once a lock, buffer load or missing transcript can hold the front, because it watched only screens.

States reached: main 38,304; a2d1 47,184; a2d2 24,976; a2d3 69,568; a2d4 68,608; c 48,830. `ownership.chart.mmd` is the main chart, drawing the bug paths only.

### What `c` adds, and the test that drives each step through the code

Only a step a test drives through its real entry point is in the `c` model; a step read in the code but not tested is left nondeterministic.

| `c` step | Code | Test |
|---|---|---|
| A turn-mode turn ends: the supervisor reads the V2 front and wakes the owner once (`TurnEnds`, `RC`) | `turn_presence/supervisor.rs` `waiting_input`, `wake`, `Action::WakeOwner` | `supervisor_tests::an_owned_front_wakes_its_owner_once_at_the_idle_transition_and_never_by_time` |
| No timer wakes an owned queue (no `TimerFires` in `c`) | supervisor `wake` (`due` needs `!owned`), `queue_io/backstop.rs` `owned` | the same supervisor test; `owned_backstop_tests::no_backstop_schedules_or_fires_a_kick_at_an_owned_queue` |
| A turn-mode turn's owner wake reaches the drive, which grants the front to the idle start | `supervisor.rs` `LiveEffects::wake_owner` → `owner_input::wake` | `supervisor_tests::the_live_owner_wake_hands_the_queued_front_to_the_owner_drive` |
| An idle pane's front is submitted with the provider's observer attached (`DriveIdle`, `obsOk` in `c`), its transcript found as the live path finds it | `owner_input.rs` `execute_owned` → `observer`, `claude_tui/hosting/warm_followup.rs` `owner_observer` → `claude.rs` `resolve_live_claude_tui_session` | `owner_input_tests::idle::an_idle_pane_takes_each_queued_front_once_through_its_own_observer` |
| A held pane's recovery wakes the drive (`WatchSees`) for Claude and Codex modals | `effect.rs` `arm_screen`, `effect/codex.rs` `composer`, `held_watch.rs` | `owner_input_tests::a_closing_modal_wakes_the_drive_to_deliver_its_held_front_once`, `owner_input_codex_tests::a_codex_modal_that_closes_wakes_the_drive_to_deliver_once` |
| A lock, buffer load or missing transcript that clears wakes the drive (`screen = "cause"`, `WatchSees`) | `effect.rs` `arm_cause`, `loads`; `effect/codex.rs` `idle_codex` | `owner_input_tests::a_failed_load_or_a_missing_transcript_holds_the_front_until_it_clears`, `owner_input_codex_tests::an_idle_codex_lock_or_load_refusal_holds_the_front_until_it_clears` |
| A cause that cleared before the watch's first read still wakes the drive once (`CauseClearsBeforeRead`) | `effect.rs` `arm_cause` (no check at the refusal) | `owner_input_codex_tests::an_idle_codex_load_that_fails_once_is_pasted_once_without_another_wake` |
| A busy Claude pane's buffer that failed to load is watched, even if it loads again before the first read; a frame ledger refusal leaves no watch and waits for the turn's end (`screen = "ledger"`) | `effect.rs` `BusyClaude::leaf` (`LoadFailed` with `owner_input::buffer_failed`), `claude_tui/owner_input.rs` `buffer_failed` | `owner_input_tests::a_busy_buffer_that_fails_to_load_once_is_pasted_once_without_another_wake`, `owner_input_tests::a_busy_ledger_refusal_holds_the_front_without_a_watch` |
| A cause read through a pane that is gone ends its watch (`PaneGoes`, `WatchLosesPane`) | `effect.rs` `arm_cause` → `Pane::lost`, `held_watch.rs` `verdict` | `owner_input_codex_tests::a_cause_watch_ends_when_its_pane_is_gone` |
| A live idle pane under another account is no execution of the channel: the front takes the launch route, and the owner writes the replacement's first prompt (`DriveIdle`, `acct`) | `owner_input.rs` `other_account` in `Registration::observed` and `execute_owned` → `Route::Cold` → `Taken::Launch` | `owner_input_tests::idle::a_pane_under_another_account_is_replaced_before_any_front_is_written` |
| A boot requeues an input retained before its ask went out, and keeps a told one (`BootRequeues`) | `v2/retained.rs` `requeueable`, `recovery.rs` | `notice_tests::told_retained_inputs_stay_and_an_untold_one_is_handed_on_once_pg`, `requeue_tests::only_an_untold_proven_unsent_input_requeues_and_a_told_one_stays` |
| A boot's re-adoption wakes the drive, which grants the kept front to the idle start | `owner_input.rs` `recover` → `wake` | `owner_input_tests::idle::a_boot_recovery_hands_the_kept_front_to_the_owner_drive` |
| A legacy channel's turn end: not assumed. The watcher epilogue's schedule becomes one owner wake in code (`backstop.rs`), but the epilogue itself is not driven by a test | — | — |

Assumed, not driven by a test:

- The idle start's Discord turn (`start_idle` → `start_owner_front` → `finish_admitted_local` → `provider_dispatch::execute`). The tests drive both sides of it: the drive's grant before it, and `execute_owned` with the claimed front after it.
- An idle live pane has a transcript the live path's resolve finds (`obsOk` in `c`). A pane with no transcript anywhere still ends `idle_unobserved`.
- The launch route's driver. On a profile mismatch the Claude driver (`claude.rs` `execute_streaming_local_tui_tmux`) cleans the stale pane up and creates one under the channel's account, then hands its first prompt to the owner's hook. Launching Claude is outside the tests, so the test stands in for that driver at `CLAUDE_LEGACY_PROBE`: it replaces the pane and runs the owner's hook.
- A ledger refusal is checked only on a busy turn-mode pane, whose turn end wakes the owner (`RC`). On a legacy channel that wake is not assumed, as for every legacy turn end.

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
   - Mutant `noBoundaryWake` breaks it again: a front held by a frame ledger refusal has no watch, so the turn-end wake is its only one. The code tests also catch the wake's removal: deletion mutants `noliveowner` and `norecoverwake` fail `the_live_owner_wake_hands_the_queued_front_to_the_owner_drive` and `a_boot_recovery_hands_the_kept_front_to_the_owner_drive`.
11. **NoSilentLoss breaks in every version before `c`.**
   - Path: an idle pane whose observer cannot be built holds the front, and a native turn AgentDesk did not start may end without waking the drive.
   - In production (11-03), every owned idle submit on one channel ended `idle_unobserved`. The drive's observer looked for the transcript only in the default home under the turn's working dir, while the live path uses the account's home and the pane's runtime binding.
   - `c` resolves it the way the live path does. Mutant `noObserver` breaks NoSilentLoss again.
12. **HeldResumes breaks on a2d4 for a cause.**
   - Path: a lock, buffer load or missing transcript holds the front, and the cause later clears with no event.
   - `c` arms the watch with a read of that cause. The effect is never retried while the cause stays; the first read that finds it cleared wakes the drive once, even when it cleared before that read.
   - Mutant `noCauseWatch` breaks it again. Mutant `clearedDrops`, the arm that dropped a cause it already saw cleared, breaks NoSilentLoss and HeldResumes.
   - A busy Claude `LoadFailed` is a buffer load or a frame ledger refusal. The owner scope's `load_failed` mark tells them apart at the refusal: a buffer is watched, a ledger refusal is not (`watchLedger` breaks NoTimedRetry).
13. **NoSilentLoss breaks in `c` without the relaunch.**
   - Path: the channel's account changed while its idle pane stayed live. The observer would not read another account's transcript, so the owner refused `idle_unobserved` on every wake.
   - `c` reads the pane as no execution of the channel and takes the launch route, as the live path does. Mutant `noRelaunch` breaks NoSilentLoss and NoSilentLossObserved.
14. **GoneWatchEnds breaks in `c` without the pane read.**
   - Path: a cause holds the front, then its pane goes away while the cause stands. A read of the cause alone keeps the watch forever.
   - `c` reads the pane after a cause that still stands, and tmux reporting it gone ends the watch. Mutant `causeBlind` breaks GoneWatchEnds and leaves the run with no end (NoDeadlock).

## Mutants of the c model

| Mutant | What it removes | Breaks |
|---|---|---|
| noObserver | the drive's observer found as the live path finds it | NoSilentLoss |
| noCauseWatch | the watch of a refusing cause | NoSilentLoss, HeldResumes |
| requeueTold | the boot keeps a told input retained | ToldStays |
| unknownEnds | a failed read keeps the watch | NoSilentLoss, HeldResumes |
| noGeneration | a watch ends only its own hold | NoSilentLoss, HeldResumes |
| noWatch | the held pane's watch | NoSilentLoss, HeldResumes |
| timer | no fixed recheck | NoTimedRetry |
| noBoundaryWake | the supervisor's V2 front read and owner wake | NoSilentLoss, HeldResumes, NoSilentLossObserved |
| watchLedger | a ledger refusal leaves no load read | NoTimedRetry |
| clearedDrops | a cause that cleared before the first read still wakes | NoSilentLoss, HeldResumes |
| noRelaunch | another account's pane takes the launch route | NoSilentLoss, NoSilentLossObserved |
| causeBlind | a cause's watch also reads its pane | GoneWatchEnds, NoDeadlock |

`noBoundaryWake` breaks a rule in `c` only through the ledger refusal: every other handoff pastes or leaves a watch. `watchLedger` arms a load read for that refusal; the read sees a working load while the ledger still refuses, so the front is handed again on every read.

## Mutants of the a2d4 model

a2d4 breaks NoSilentLoss, HeldResumes and NoSilentLossObserved as written:

- Each mutant below breaks a further rule.
- noWatch, unknownEnds and noGeneration add nothing over a2d4's own cause-hold break, so they are checked on `c`.

| Mutant | What it removes | Breaks |
|---|---|---|
| watchBlind | the watch wakes on an unchanged screen | NoTimedRetry |
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

- Codex and Herdr panes: the model's pane is Claude tmux; slice C arms the same watch on their modals, and on an idle Codex pane's lock and buffer load.
- Restart replay of the relay's claim ledger.
- Two inputs racing one paste.
- A screen or cause that never clears (Quiet waits for it).
- A gone pane before `c`: it has no screen that would submit. In `c` its front waits, unwatched, for a new event.
