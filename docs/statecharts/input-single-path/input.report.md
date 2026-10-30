# InputPath
<!-- doc-path-refs: proposed drive.rs -->

Model written against AgentDesk `origin/main` `0a9b3f99cf` (2026-10-30, landed #7272 S1~S4 4a) plus the unlanded 4f design `design-4f-r1` (base `6745f803d3`) and its review carry `carry-design-4f-r1` (P1 9, mac decisions mac-0954). It models #7272: every Discord input to a Claude TUI channel goes through the owner line and one drive task that reads the composer, parks a draft with C-s, pastes, presses Enter and settles. Every counterexample was replayed by reading the landed code and the 4f design (no build or test run); see the table at the end.

Two configs:

- `InputPath.cfg`: the landed code and the 4f design as written. Chart `input.chart.mmd` (bug paths only; the full graph is above the 500-state budget).
- `InputPathClean.cfg`: the four open points closed (an unreadable composer holds, a Held input stays first in line and is woken when its hold clears, the backstop also covers a MayHaveSent head, /stop does not put queued prompts back). All rules hold (22,704 states). The mutants run on this config.

## What the model holds

Source column: **code** = landed `0a9b3f99cf`, **4f** = design-4f-r1 section, **carry** = carry-design-4f-r1, **plan** = plan-design-r2.

| Model action | Source |
|---|---|
| `Arrive`: Discord input admitted, drive woken | 4f §2.2 wake table (admit Accepted → `drive::wake`) |
| `DriveTakesFront`: single-flight drive takes the first item and reserves it | 4f §2.2 (`drive.rs`, front only when no head_injection); code `v2.rs:86-115` (reserve refuses a second active head, takes `items.first()`) |
| `DriveFindsNothing` | 4f §2.2 step 1 (front is None while an attempt is active) |
| `Prepare`: one read before any key; modal → Held; empty → paste; readable draft → C-s; draft while a stash is shown → paste behind; unreadable → paste (`UnreadProceeds`) | code `busy_inject.rs:357-386` (`before_paste`, `park_draft`; capture fails → no C-s), `screen.rs:114-123` (`parkable` false on Unknown layout or a shown stash); 4f §3 Herdr steps 2-3 ("unreadable composer는 그대로 진행"); carry P1-7 (owner A: visible draft → C-s 1, empty or unreadable → C-s 0) |
| Held → NotSent → `retain_not_sent` (design config) | 4f §3 step 3 (modal → NotSent); code `retained.rs:85-114` is the only landed V2 NotSent settlement. 4f does not say which settlement the drive uses (see C1) |
| `ArmAndKey`: MayHaveSent, deadline now+30 s, then C-s | code `attempt.rs:91-119`, `stash.rs:50-58` |
| `Paste`, `Enter`: Enter applied → Submitted, no receipt wait | code `stash.rs:76-93`; 4f §3 table (`confirm` replaced by `collect_receipt`, `busy_inject.rs:482`) |
| `WriteUncertain`: paste or Enter fails or is indeterminate (a failed Enter may still have landed) → Unknown | code `stash.rs:88-93`, `retained.rs:36-83` (`settle(false)` → Unknown) |
| `Backstop`: at the deadline the head leaves the line as unconfirmed with a notice, never resent | code `retained.rs:4-30` (`tick`); 4f §7 (backstop armed for the earliest **Unknown** deadline, ticks only if still Unknown) |
| `Restart`, `Boot`: drive tasks and wakes are lost; adopt → tick → wake; Reserved is prepared again | code `v2.rs:25-60` (adopt turns only a Bootstrap MayHaveSent into Unknown); 4f §2.5 B1, §7 boot row, §12.1 |
| `Absorb`, `TurnEnds`: Claude takes a busy-queued prompt into this turn or the next; completion wakes the drive | 4f §2.3 (D1~D3 completion → wake) |
| `Stop`: /stop sends Escape to the Claude pane; Claude puts queued prompts back in the composer (`StopReturnsQueued`, provider behaviour not verified) | code `claude_stop_delivery.rs:517,696`; 4f §12.2 (stop: AI turn only) |
| `HumanTypes`, `ModalOpens/Closes` | environment |
| `LegacyStarts`: a non-owner start of a source the owner holds (only with the mutant) | 4f §5 typed claim (OwnerHeld refused) |

Left out: Codex (4f §4 leaf has the same read-before-key shape), cold BootstrapAndFirstInput, a provider incarnation change (4f §12.1: ready Target refused before keys → NotSent, the same exit as Held), the human typing between our read and our Enter (no pane is read after a key, `stash.rs:1`; an inherent race), /clear and cancel (4f §12.2: only unattempted items), HTTP/routine/scheduled adapters, history receipts, more than one channel.

## Rules

1. `NoDuplicate`: no input reaches Claude twice.
2. `NoSilentLoss`: once the channel is quiet, each input reached Claude or sits retained with a notice. (Plan r2 (b): an unconfirmed input is retained with a notice, never resent; that is the designed exception to "exactly once".)
3. `DraftSafe`: the human's draft is never sent together with an input and never overwritten.
4. `OneExecutor`: at most one drive task executes an input on the channel.
5. `Fifo`: inputs reach Claude in arrival order.
6. `HeldNotDropped`: an input held for a passing reason (modal, unreadable) is not dropped from the line.
7. `BusyInputLands`: an input sent while Claude was busy reached this turn or the next one.
8. `NoDeadlock`: a waiting input can always move. The only end state is a quiet channel with every input admitted.

## Results, `InputPath.cfg`

TLC reached 40,828 states, more than the chart budget of 500, so the chart draws only the bug paths: 12 states and 17 arrows. The view hides `Done` (end-state step) only.

## HeldNotDropped breaks
The shortest path has 4 steps:
0. No input waiting (Claude idle)
1. a Discord input is admitted: Inputs wait, no drive running (Claude idle)
2. the drive takes the first input: Input taken, composer being read (Claude idle)
3. a modal opens: Input taken, composer being read (Claude idle)
4. the drive is held [modal, or unread when that holds]: No input waiting (Claude idle)

## NoSilentLoss breaks
The shortest path has 7 steps:
0. No input waiting (Claude busy)
1. a Discord input is admitted: Inputs wait, no drive running (Claude busy)
2. the drive takes the first input: Input taken, composer being read (Claude busy)
3. the composer reads empty or cannot be read: Ready to paste without C-s
4. the attempt is armed (C-s if parking): Keys started, pasting
5. the input is pasted: Pasted, Enter next
6. Enter is applied, settled as sent: No input waiting (Claude busy)
7. you /stop the turn: No input waiting (Claude idle)

## DraftSafe breaks
The shortest path has 7 steps:
0. No input waiting (Claude idle)
1. a Discord input is admitted: Inputs wait, no drive running (Claude idle)
2. the drive takes the first input: Input taken, composer being read (Claude idle)
3. you type a draft: Input taken, composer being read (Claude idle)
4. the drive pastes behind text in the composer [unread, or no C-s allowed]: Ready to paste without C-s
5. the attempt is armed (C-s if parking): Keys started, pasting
6. the input is pasted: Pasted, Enter next
7. the paste or Enter is uncertain: Write uncertain, waits for the deadline, draft sent with an input

## BusyInputLands breaks
The shortest path has 7 steps:
0. No input waiting (Claude busy)
1. a Discord input is admitted: Inputs wait, no drive running (Claude busy)
2. the drive takes the first input: Input taken, composer being read (Claude busy)
3. the composer reads empty or cannot be read: Ready to paste without C-s
4. the attempt is armed (C-s if parking): Keys started, pasting
5. the input is pasted: Pasted, Enter next
6. Enter is applied, settled as sent: No input waiting (Claude busy)
7. you /stop the turn: No input waiting (Claude idle)

## NoDeadlock breaks
The shortest path has 12 steps:
0. No input waiting (Claude idle)
1. a Discord input is admitted: Inputs wait, no drive running (Claude idle)
2. a Discord input is admitted: Inputs wait, no drive running (Claude idle)
3. the drive takes the first input: Input taken, composer being read (Claude idle)
4. the composer reads empty or cannot be read: Ready to paste without C-s
5. the attempt is armed (C-s if parking): Keys started, pasting
6. the server restarts: Server restarting, line kept on disk
7. boot recovery runs: Maybe sent, nobody watching it
8. the drive wakes and finds nothing it may take: Maybe sent, nobody watching it
9. 30 s pass: Maybe sent, nobody watching it
10. you type a draft: Maybe sent, nobody watching it
11. a modal opens: Maybe sent, nobody watching it
12. the modal closes: Maybe sent, nobody watching it

## Rules that hold
NoDuplicate, OneExecutor, Fifo

In the NoDeadlock path, steps 10-12 only use up the bounded environment moves; the channel is stuck from step 9.

## Results, `InputPathClean.cfg`

All rules hold: NoDuplicate, NoSilentLoss, DraftSafe, OneExecutor, Fifo, HeldNotDropped, BusyInputLands, NoDeadlock.

## Mutants (each removes one guard; run on `InputPathClean.cfg`)

| Mutant | Guard removed | Rules that break |
|---|---|---|
| `Mut = "noArrivalWake"` | 4f §2.2: no wake after admission (`wake_removed_after_accept`) | NoDeadlock |
| `Mut = "reserveIgnoresHead"` | single-flight and `v2.rs:91-93` (reserve refuses an active head) | OneExecutor, DraftSafe, NoDuplicate, Fifo (others not checked: 100,000-state bound) |
| `Mut = "noSingleFlight"` | 4f §2.2 single-flight map alone | none: the owner's one head slot (`v2.rs:91-93`) already keeps a second drive out |
| `Mut = "frontNotHead"` | `v2.rs:94` (`items.first()`) | Fifo |
| `Mut = "noCtrlS"` | carry P1-7 (visible draft → C-s 1); also main today for a channel not in `ADK_BUSY_INJECT_STASH_CHANNELS` (`busy_inject.rs:384-386`) | DraftSafe |
| `HerdrNoCtrlS = TRUE` | 4f r1 D-4f-5 (Herdr visible draft pasted behind, C-s 0), reversed by carry P1-7 | DraftSafe |
| `Mut = "stashShownIgnored"` | `screen.rs:121-123` (no C-s while `› stashed` shows) | DraftSafe (the old stash is overwritten) |
| `Mut = "resendAfterUnknown"` | `retained.rs:4-30` (an expired head is retained, not put back in line) | NoDuplicate, Fifo, NoSilentLoss, BusyInputLands, DraftSafe |
| `Mut = "sourceCheckRemoved"` | 4f §5 OwnerHeld (`source_check_removed`) | NoDuplicate, Fifo, NoSilentLoss, BusyInputLands |
| `Mut = "noCompletionWake"` | 4f §2.3 completion wake (`completion_wake_removed`) | none: the owner injects while Claude is busy and every settle wakes the drive, so no input waits on a completion in this model |
| `UnreadProceeds = TRUE` | Held for an unreadable composer | DraftSafe |
| `StopReturnsQueued = TRUE` | (provider behaviour, not a guard) | NoSilentLoss, BusyInputLands |
| `HeldWakes = FALSE` | plan A19 (a Held input is delivered after the hold clears, with no new message) | HeldNotDropped |
| `BackstopMayHaveSent = FALSE` | backstop for a MayHaveSent head | NoDeadlock |

Command for a mutant: copy `InputPathClean.cfg`, change the constant, and run the command below with that config.

## Counterexamples replayed against the code and the 4f design

| ID | Rule, path | Replay | Verdict |
|---|---|---|---|
| C1 | DraftSafe, steps 0-7 (step 7 is an Enter that landed but reported failure; a plain Enter ends the same) | A human draft is in the composer and the read does not recognise it: `before_paste` returns `Ok(false)` when the capture fails (`busy_inject.rs:373-375`, comment `:357-358` "An unread composer deliberately proceeds without C-s"), and `parkable` is false for an Unknown layout (`screen.rs:114-118`). `stash::submit` with `stash=false` pastes and presses Enter (`stash.rs:76-93`): Claude sends the draft and the input as one prompt. 4f §3 Herdr leaf: "unreadable composer는 그대로 진행한다"; carry P1-7 fixes "판독 불가 → C-s 0" as the accepted behaviour. | Confirmed by code and design. The 4f-b test list (carry P1-7) asserts the opposite of the rule (unreadable → paste). Decision needed: Held, or accept the risk |
| C2 | NoDeadlock, steps 3-9 | Armed head MayHaveSent with deadline now+30 s (`attempt.rs:111-112`), server restarts between C-s/paste and Enter. Adopt turns only a Bootstrap MayHaveSent into Unknown (`v2.rs:51-59`); boot tick retains only after the deadline (`retained.rs:4-15`), which a fast restart has not reached. Afterwards: front is None while the head is active (4f §2.2 step 1), so arrivals and completions find nothing; the 4f §7 backstop is armed for the earliest **Unknown** deadline and ticks only "아직 그 Unknown일 때", and §2.2/§7 add a tick only after an effect returns. Nothing ticks the MayHaveSent head; the channel waits until the next restart after 30 s. | Confirmed by design (§7 names only Unknown; the code steps are landed). Same area as carry R1-P2-2 (backstop moved to the V2 Unknown deadline); plan A06 (cut points) |
| C3 | DraftSafe, mutant `noCtrlS` / `HerdrNoCtrlS` | Main today: `park_draft(stash_allowed, parkable)` (`busy_inject.rs:384-386`) pastes behind a visible draft on a channel not listed in `ADK_BUSY_INJECT_STASH_CHANNELS`. 4f r1 D-4f-5 did the same for Herdr. | Confirmed by code (main, unlisted channels) and design r1. Already carry R1-P1-7 (mac decision (a): visible draft → C-s 1 on tmux and Herdr, typed CtrlS; mutants `owner_stash_env_gate_restored`, `herdr_ctrl_s_removed`) |
| C4 | HeldNotDropped, steps 0-4 | A modal is open when the drive reads: 4f §3 step 3 → NotSent(Modal), content 0. The only landed V2 NotSent settlement is `retain_not_sent` (`retained.rs:85-114`): the item leaves the line as KnownNotSent with a "retained_retry_required" notice and is never sent. 4f does not name the NotSent settlement of the drive; if it instead releases the head, the wake table (§2.2 "준비 재시도 → 7절 backstop") has no wake for a cleared hold, because §7 arms only for an Unknown deadline. Plan A19 requires delivery after the hold clears with no new message. | Not confirmed: the 4f drive's NotSent settlement is not written. Either reading breaks a rule (dropped, or stuck until the next arrival/completion). 4f-b acceptance test candidate (plan A19); no carry P1 matches (nearest R1-P1-2 drive wiring) |
| C5 | NoSilentLoss and BusyInputLands, steps 0-7 | Claude busy; the owner pastes and presses Enter; 4f §3 settles Submitted on the Enter with no receipt wait (`collect_receipt`, `busy_inject.rs:482`, only logs). Claude queues the prompt. /stop sends Escape to the pane (`claude_stop_delivery.rs:517,696`). If Claude puts queued prompts back in the composer on Escape, the input is settled as sent but sits unsent in the composer, and the next input parks it with C-s as a "draft". | Not confirmed: Claude's Escape behaviour for queued prompts was not checked. If it holds: High (silent loss with a Submitted record). 4f-b acceptance test candidate (stop while a busy-injected input is queued); no carry P1 |

## Run

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/input-single-path
~/.claude/skills/statechart/scripts/statechart InputPath.tla --config InputPath.cfg --view input.view.mjs --out /tmp/sc-input
~/.claude/skills/statechart/scripts/statechart InputPath.tla --config InputPathClean.cfg --view input.view.mjs --out /tmp/sc-input-clean
```

The first command exits 1 (rules break, as documented). The clean config has more than 500 states, so it writes no chart.
