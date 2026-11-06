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
| `cf` | main after C `a9d59259e5`, with slice F's paths | `InputOwnershipCF.cfg` |
| `f` | main after slice F `40c556ae6c` | `InputOwnershipF.cfg`, `InputOwnershipFOutside.cfg` |
| `f4` | `f` plus the carried close's queue wake `501692a3d7` | `InputOwnershipF4.cfg` |
| `f5` | `f4` plus an intake start's create retried once past a marked row it removed | `InputOwnershipF5.cfg` |
| `a2b` | `f5` plus #7340 S2-A2b: a start waits out a native busy turn | `InputOwnershipA2b.cfg`; r2b's read failure (Mut `nativeReadGone`): `InputOwnershipA2bReadGone.cfg` |

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

## Slice F: a carried row that never closes (`cf`, `f`, `f4`, `f5`)

Code: `a9d59259e5`. Paths added only for `cf` and `f`; `c` and the older columns keep their state space (C: 48,830 states, unchanged).

- `row = carried`: an earlier generation's durable inflight row on a turn-mode channel. Restore keeps it and reattaches a watcher to the live pane (`recovery_engine/restore_inflight.rs:1769-1780`); it re-intakes nothing.
- `DriveMeetsRow`: every owner start on an idle mailbox creates a row, meets the carried one and is refused (`router/message_handler/foreign_row.rs:63-81`); the front keeps its place (`input_owner/drive.rs:100-112`).
- `CarriedCloses`: the carried-turn close archives the row once the transcript reads the native turn closed (`health/recovery/carried_turn_close.rs:197-288`). With a restored episode (`ep`) its finalizer releases the mailbox and the QueueEligible listener wakes the owner (`queue_io/backstop.rs:636`, `queue_dispatch/kickoff.rs:120`). Without one, `f` runs no finalizer and nothing wakes the front `foreign_row` refused; `f4` schedules one kickoff after the archive either way.
- `leak`: the transcript holds an input Claude absorbed mid-turn with no `queued_command` row. `tui_o/writer/input_facts/queued.rs:81` keeps that body owed and gives a dequeue credit no dequeue repays, so `closed_boundary.rs:47` answers `pending_receipt` for ever.
- `row = marked` (from `f4`): a finished turn's row kept, stamped, because its origin receipt failed to record (`receipt_settle.rs:94-143`); checked with the receipt store healthy again. The next start's admission records and removes it (`settle_marked_row`). A headless start then retries its create (`foreign_row.rs` `admit_headless`); up to `f4` an intake start drops itself anyway (`admit`: requeue refused by the V2 owner, no new wake), `f5` retries the create once. The ordinary Discord owner busy shortcut creates no row, so it neither holds nor settles a marked row. A headless start, including an owner front with preserved admission context, still clears it before provider input even when the native pane is busy. `CarriedRowEnds` reads only carried rows; `HeadlessSettlesMarked` separately checks that headless admission cleared a marked row.
- `HandedOutside`: a person types the text at the terminal, or another session sends it over its own socket.

| Config | Result |
|---|---|
| `CF` | `NoSilentLoss`, `NoSilentLossObserved`, `CarriedRowEnds` break: a person's terminal turn ends, the owner is woken, its start meets the carried row, and nothing closes it. Code-confirmed on adk-dash-cc 11-03: `pending_receipt:owed_inputs=75..78` from 04:51Z to 18:59Z, front refused at every wake. |
| `F` | `NoSilentLoss` and `NoSilentLossObserved` break: the boot wake meets the carried row, the close archives it with no episode, and the front is never woken again. Seen on adk-dash-cc 11-04 00:57Z: close with `episode_released=false`, then no owner wake for 4 queued inputs. Mutant `leakKept` (the reader still counts the absorbed input) also breaks `CarriedRowEnds`. |
| `F4` | `NoSilentLoss` and `NoSilentLossObserved` break on a marked row: the wake's start removes the row, drops itself, and nothing wakes the front again. Without a marked row (the F4 change itself) every rule held, 195,288 states; mutant `closeNoKick` breaks `NoSilentLoss`. |
| `F5` | every original rule and `NoDeadlock` hold. The merged F5/F6 model also checks `HeadlessSettlesMarked`; its busy headless path uses `RowClear` and clears the previous row, while the ordinary owner busy shortcut leaves it. The targeted probes and current safety/temporal matrix are in [admission.report.md](admission.report.md). Mutant `noRetry` models the intake start dropping itself after the settle. |
| `FOutside` | `SeenOnce` breaks in `f` too: AgentDesk records no identity for an outside copy, so its own single paste is the second time the session sees the text. Kept as a documented gap; `NoDuplicate` (AgentDesk pastes at most once) holds. |

The model folds the transcript reader into `leak`: which order an input's command and absorption rows come in, and how same-body inputs pair, are checked by the Rust reader tests (`tui_o/writer/input_facts/receipt_tests.rs`), not here.

## #7340 S2-A2b: a start waits out a native busy turn (`a2b`)

Code: `8b28712958` (A2b on main `6d7c1851e5`). Only `DriveStartWaits` and three rules are new; every other config keeps its state space and result.

- `DriveStartWaits`: a start whose idle-mailbox drive meets a native busy turn pastes nothing and is held (`heldBy = native`) before any claim (`router/message_handler/provider_dispatch/owner_input.rs:530-582`, `609-620`). In `a2b` it replaces `DriveStartBusy`, which pasted the start into the running turn.
- The wake: on a turn-mode channel the supervisor's idle transition wakes the owner (`RC`, `turn_presence/supervisor.rs:628`). On legacy the hold leaves a read of the same transcript with the held watch, and the owner marks the front held (`queued_head/v2.rs:357`), so the drive watches it (`input_owner/drive.rs:105`) and the turn's end wakes it once (`input_owner/held_watch.rs:58`).
- `WatchSees` gains one conjunct: a watch of a native hold wakes only once the turn reads idle.
- Rules: `NativeHeldResumes` (a held start is not left once all is quiet), `StartOwnTurn` (a start is never pasted into a running turn) and `StartOnce` (an answered start was pasted once).

| Config | Result |
|---|---|
| `A2b` | every rule holds (17 rules and NoDeadlock, 129,692 states). Mutant `nativeNoWatch` (no read of the turn's end) breaks `NativeHeldResumes` and `NoSilentLoss` on legacy; `noBoundaryWake` breaks them on turn mode; `startPastes` (the f4 paste) breaks `StartOwnTurn`. |

### r3: a failed read of the native-held pane (`NativeWatchMisreads`)

Code: r3, kept in r5 (`owner_input.rs:559-581`). The watch's probe of a native hold reads the pane's liveness first. A dead or absent pane is `Gone` and ends the watch (`held_watch.rs:38-44`). A tmux read that fails (`liveness.rs:59-77`, `ProbeError`) is now `Unknown`, which keeps the watch for the next 2 s look. r2b mapped every non-`Live` answer to `Gone`, so one failed read ended the watch, and nothing woke the held start after its turn ended.

- `NativeWatchMisreads` (Fix `a2b`): a read of a native-held pane fails. The watch is kept; mutant `nativeReadGone` (r2b) ends it.
- `NativeHeldRuns` (liveness, under `LiveSpec == Spec /\ WF_vars(Next)`): a native-held start is handed again unless its pane is gone. `WF_vars(Next)` is weak fairness of `Next` as a whole, not of each action: a run whose reads keep failing or whose native turn never ends is outside what the property proves. `A2bLive` and `A2bReadGoneLive` now check it in CI (#7772).
- Only a failed pane read is projected. A Claude transcript that is resolved but unreadable reads as not busy (in `native_turn`, `observe_claude_jsonl_turn_state(...).is_busy()` is false, so the probe returns `Screen::Ready`, `owner_input.rs:566-575`), so its watch wakes the drive rather than keeping the hold; the model leaves that read out.
- Out of scope: the model has one Claude tmux channel; Herdr panes are not modelled.
- Out of scope: Codex. This PR keeps Codex on main's path, so a Codex start into a native busy turn is the existing defect; its native-busy gate and turn-end read moved to #7772.

| Config | Invariants | `NativeHeldRuns` |
|---|---|---|
| `A2b` before r3 (no read failure) | every rule holds, 129,692 states | not defined |
| `A2b` (r3) | every rule holds, 129,692 states | holds, 129,692 states |
| `A2bReadGone` (r2b) | `NativeHeldResumes`, `NoSilentLoss` break | violated, 130,860 states |

The r2b counterexample: a person's turn runs, the start is held with a watch (`DriveStartWaits`), the turn ends, one read fails and ends the watch (`NativeWatchMisreads`), and the start is left held once all is quiet.

### On `f5` (after #7797)

`a2b` is now `f5` plus the start wait: every `f5` transition (the marked row, its retry, `HeadlessSettlesMarked`) applies to it, and both A2b configs also check `HeadlessSettlesMarked`.

- A start held by `DriveStartWaits` runs no admission, so a marked row stays (`start_waits` reads the row without settling it, `owner_input.rs:511-525`); the start's later idle admission records and removes it (`settle_marked_row`, `foreign_row.rs:45,72`), as `DriveIdle` does on `f5`.
- The F6 observer, F7 busy-arrival, F7 recovery/transition and automatic-hold slices keep their own `Init`/`Next` and constants; `a2b` adds nothing to them.

| Config | Invariants | `NativeHeldRuns` |
|---|---|---|
| `A2b` | every rule holds (18 rules and NoDeadlock), 130,292 states | holds, 130,292 states |
| `A2bReadGone` | `NativeHeldResumes`, `NoSilentLoss` break | violated, 131,460 states |

Mutants on `f5`: `nativeNoWatch` breaks `NoSilentLoss` and `NativeHeldResumes`; `noBoundaryWake` breaks `NoSilentLoss`, `NoSilentLossObserved`, `NativeHeldResumes` and `HeldResumes`; `startPastes` breaks `StartOwnTurn`.

## #7772: an unreadable pane holds a start, and a start held after its claim is watched (`a7772`)

Code: this PR on main `4eb8a84ff`. Only `a7772` reaches the new transitions; every other config keeps its state space and result (`A2b`: 130,292 states, as before).

| Code entry (file:line) | Model value range |
|---|---|
| `owner_input.rs:627-657` `native_read`: a dead, absent or other-account pane is `Gone`; a failed liveness read or an unresolved transcript is `Unknown`; Claude `Idle` is `Ready`, `Streaming`/`UserSubmitted` is `Blocked`; a classifier `Unknown` is `Ready` only if the composer passes the final send check | `nr \in {"ok","unknown","spent"}` (`ReadBreaks`, `ReadRecovers`); `View` is `"unknown"` while `nr = "unknown"`, else `nat` |
| `owner_input.rs:553-579` `start_waits`, `:608-615` `native_wait`, `:689-694` the early hold (`held_watch::arm` + `hold_front`) | `DriveStartWaits`: legacy holds on `View \in {"busy","unknown"}`; turn mode holds on `nat = "busy"` (the activity read, `:563-567`); mutant `earlyUnknownGoes` lets `unknown` through |
| `entrypoints.rs:100-138` the pane the early read takes: the session's channel name, else the start's hint, else Discord's name, else the registered fallback, as its headless start names it; `headless_turn.rs:201` keeps the resolved name as a new start's hint | not modelled: the model has one pane, which every read and the start share (see the out-of-scope lines) |
| `owner_input.rs:698-700` the claim after `Go` | `DriveStartClaims`: `inp' = "claimed"`, `seen' = View` (legacy) or `nat` (turn mode); rule `EarlyClaimHasIdleProof` |
| `owner_input.rs:808-832` the late read before `Registration`; `:839-865` registers its idle or gone read as read (`Registration::read_as`, `:67-89`) with no second liveness read | `DriveStartRuns` on `View = "idle"` (mutant `lateUnknownIdle` also on `unknown`); `DriveStartLateHolds` otherwise: `heldBy' = "native"`, `watch' = TRUE` (mutant `lateNoWatch`: no watch, as main), `prov' = "mailbox"` |
| `owner_input.rs:750-755`, `:763-774` the producer side of the late hold (`hold_start` after `NotSent("synthetic_waits_idle")`) | the same `DriveStartLateHolds` step: the owner marks the front held, so the drive watches it |
| `owner_input.rs:879-898` `late_probe`: `Ready` only with no active mailbox turn and no inflight row carrying the attempt's own nonce; a lookup error is `Unknown` | `prov \in {"mailbox","row","none"}` (`AttemptReleases`, `AttemptRowClears`); `WatchSees` wakes a native hold only once `prov = "none"` (mutant `lateNoUnwindWait`: before); `DriveMeetsUnwind` spends a wake that meets the unwind |
| `headless_turn.rs:527-533`, `:548-553` the fresh and snapshot binding clear, deferred by `:554-565` to after the late read | not modelled: it only decides which transcript the late read resolves (see the out-of-scope lines) |

| Config | Before (main) | After |
|---|---|---|
| `A2b` | every rule holds, 130,292 states | unchanged |
| `A2bReadGone` | `NativeHeldResumes`, `NoSilentLoss` break | unchanged |
| `A2bLive` | not a repository cfg (run by hand: holds) | `NativeHeldRuns` holds, 130,292 states |
| `A2bReadGoneLive` | not a repository cfg (run by hand: violated) | `NativeHeldRuns` violated |
| `A7772` | — | every rule holds (19 rules and NoDeadlock), 471,844 states |
| `A7772Live` | — | `NativeHeldRuns` holds, 471,844 states |
| `A7772EarlyUnknownGoes` | — | `EarlyClaimHasIdleProof` breaks |
| `A7772LateUnknownIdle` | — | `StartOwnTurn` breaks |
| `A7772LateNoWatch` / `A7772LateNoWatchLive` | — | `NativeHeldResumes`, `NoSilentLoss`, `NoSilentLossObserved` break / `NativeHeldRuns` violated |
| `A7772LateNoUnwindWait` / `A7772LateNoUnwindWaitLive` | — | `NativeHeldResumes`, `NoSilentLoss`, `NoSilentLossObserved` break / `NativeHeldRuns` violated |

- `InputOwnershipConsumedWaiting` extends this model and now keeps `nr`, `prov` and `seen` unchanged in every step (`CWFixed`); its seven configs reach the same states and results as on main.
- Out of scope: Codex. `start_target` reads only Claude panes, and the late read runs only for Claude (`owner_input.rs:574-576`, `:795-797`); Codex keeps main's path until slice C adds its reader.
- Out of scope: Herdr panes; `start_waits` sends a Herdr rest to the event path (`owner_input.rs:544-549`) and the model has one Claude tmux channel.
- Out of scope: InputPath (#7795) and the native stop (#7755); neither changes when a start is read or held.
- Out of scope: which transcript a read resolves (binding, session path, cwd); the model sees only its result, `nr`. The deferred fresh/snapshot clear is checked by `an_unsent_start_cleared_later_leaves_only_its_removal_receipt_pg`.
- Out of scope: a read that never recovers (`Unknown` for ever); `Quiet` waits for `nr # "unknown"`, as it waits for a screen that never clears.
