# InputOwnership — who answers an input pasted into a Claude pane (#7676 A2+D)

Code: a1c `0b0fbcbfbe` (`Fix = "main"`, `InputOwnershipMain.cfg`), the first A2+D change
`524adb6190` (`Fix = "a2d1"`, `InputOwnershipA2d1.cfg`) and its repair (`Fix = "a2d2"`,
`InputOwnership.cfg`). One input from Discord or from `turn/start` (an iMessage reply), one
Claude tmux channel in turn mode or legacy mode, mailbox idle/active and transcript idle/busy in
every combination, a screen that may refuse keys (modal, unread capture, copy mode) and clear with
no event, a person's draft C-s cannot be proven to keep, an earlier input with the same author and
body, the receipt thread's 3 s window, and the relay reading the input's row once.

Run (exit 1 means a rule breaks):

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart InputOwnership.tla --config InputOwnershipMain.cfg --view ownership.view.mjs --out /tmp/sc-own-main
~/.claude/skills/statechart/scripts/statechart InputOwnership.tla --config InputOwnershipA2d1.cfg --view ownership.view.mjs --out /tmp/sc-own-a2d1
~/.claude/skills/statechart/scripts/statechart InputOwnership.tla --config InputOwnership.cfg --view ownership.view.mjs --out /tmp/sc-own-a2d2
```

## Rules

| Rule | Reader's sentence | main | a2d1 | a2d2 |
|---|---|---|---|---|
| NoDuplicate | The input is pasted at most once. | holds | holds | holds |
| NoRetryRequired | No input ends asking the person to send it again. | breaks (1) | holds | holds |
| AnswerOnce | An answered input was posted once: O writer only on turn mode, the relay or the reader the input's turn set up on legacy. | breaks (5) | breaks (4) | holds |
| HandedHasRow | Time alone never makes the input Handed. | holds | holds | holds |
| StartNotDropped | A turn/start into a busy pane is never dropped for want of an idle observer. | breaks (1) | breaks (1) | holds |
| NoTimedRetry | A held input is handed again only by an event, never by a fixed timer alone. | breaks (3) | breaks (3) | holds |
| DraftToldOnce | An owned paste beside a draft C-s could not park tells its original once. | breaks (1) | breaks (1) | holds |
| NoSilentLoss | Once nothing else happens, the input was answered. | breaks (3) | breaks (3) | breaks (3, same path) |

`ownership.chart.mmd` is the main chart (bug paths only; 12,416 states). a2d1 reached 15,472
states, a2d2 7,456.

## Counterexamples, replayed against the code

1. **NoRetryRequired, main** — mailbox active, transcript idle: `hand_busy` → `inject_owned` was
   `BusyOnly`, `NotBusy` is not a held reason, so `effect::run` retains the original and asks for a
   resend. Fixed in a2d1 (the owned entry is `AnyTurn`).
2. **StartNotDropped, main and a2d1** — a `turn/start` on a pane busy with a turn AgentDesk did not
   start: `start_idle` → `start_owner_headless` → `execute_owned` registered `busy=false` and
   `InputOrigin::Discord`, so with no idle observer `take_owned` returned
   `NotSent("idle_unobserved")` and the turn ended with a failure (the 11-02 11:57 iMessage case).
   a2d2 re-reads the pane (`running_transcript`) and keeps the front's origin: a busy pane takes the
   owned busy entry, with a reader of the row the input takes (`busy_owner_observer`).
3. **NoTimedRetry, main and a2d1** — a held refusal (`Modal`, and in a2d1 also `ScreenUnread`,
   `CopyMode`) spawned `sleep(recheck)` → `wake` in `drive.rs`, so an unchanged screen rebuilt the
   prompt every 5 s. a2d2 removes the timer; the next turn boundary, idle kickoff or new input wakes
   the drive.
4. **DraftToldOnce, main and a2d1** — a draft beside an image chip reads `Composer::Unknown`, so
   C-s is not sent and the frame is pasted behind it with no word to the person. a2d2 pastes and
   posts one outbox notice keyed by the original's sources.
5. **AnswerOnce, main** — every recorded Claude frame was `Joined { first: false }`, so a queued
   input that opened its own turn had no relay. Fixed in a2d1 by the row-based claim.
6. **AnswerOnce, a2d1** — with an earlier input of the same author and body, both attempts carry
   the second user row, `user_row_attempt` sees two owners and the row is Joined: no answer. a2d2
   pairs same-frame rows with attempts in registration order (`history::row_owner`).
7. **NoSilentLoss, every version** — an idle pane whose observer cannot be built holds the front,
   and a native turn AgentDesk did not start may end without waking the drive. The Claude observer
   is absent only when the transcript cannot be resolved, so nothing proves where a paste would
   land. The remaining event-wake gap of the A1 review (P2-1), owned by slice C.

## Mutants of the a2d2 model (each breaks a rule besides the known NoSilentLoss gap)

| Mut | What it removes | Breaks |
|---|---|---|
| pendOnBusy | the owned busy entry declares an Answer nobody reads | AnswerOnce |
| blanketJoin | Claude frames joined whatever the row | AnswerOnce |
| receiptKind | ownership read from the 3 s receipt kind | AnswerOnce |
| timeHanded | Handed after the window with only an enqueue | HandedHasRow, AnswerOnce |
| turnLegacy | the legacy relay also answers on turn mode | AnswerOnce |
| startIdle | the turn/start's turn read as idle (`busy=false`) | StartNotDropped |
| timer | the drive's fixed recheck restored | NoTimedRetry |
| noNotice | the draft notice skipped | DraftToldOnce |
| twoCandidates | a row with two possible owners joined | AnswerOnce |

Left out: Codex and Herdr panes (A2 keeps their hold), restart replay of the relay's claim ledger,
two inputs racing one paste, a screen that never clears.
