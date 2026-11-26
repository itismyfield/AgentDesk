# Herdr composer wait cause (#8161 S0)

Model: `HerdrComposerWaitCause.tla`. Code SHA: `a219f6c716` + S0.
Code path: `claude/herdr_turn/composer.rs` `when_idle` → `claude_tui/readiness/watched.rs` `expired`.

## Why
11-19 adk-dash-cc: after the 11:28 restart every Discord input was held as
`Claude composer held before first key: Refused(Unknown)`, 10.2 s after its first key would have gone.
That is the composer's 10 s budget ending on `Expiry::Waits`, which named no branch.
Internal turns ended twice and the hold stayed. The pane, read once (read-only), showed a background
agent's Bash permission dialog where the composer is (design W2).

## Values (from the design entry tables only)
| Variable | Values | Code |
|---|---|---|
| source | ok, none | `Source::current` at the wait's first look |
| fence | none, open | `unread_continuations`/`continuations`, `pending`, `compact_*` |
| screen | empty, unread | `composer_owner` |
| launch | before, this | binding context `created_at` vs daemon `STARTED` |

Actions: `Attempt` (the 10 s budget and its `expired` verdict), `InternalTurnEnd` (task-notification
turn; owner wake; clears no source, fence or dialog), `SourceAppears` (a launch of this run publishes
its source late), `Quiet`.

## Rules
- `KeysNeedProof`, `SettleOnlyCauseless` (invariants).
- `Resolution == <>(queued ∈ {sent, settled})` — the input is eventually sent or settled with its notice.
- `LaunchDelivers == <>(queued = sent)` (launching configs) — S0 never settles a launch of this run.

## Results (TLC 1.7.4, `scripts/ci/statecharts_tlc.py`, 11/11 as expected)
| cfg | broken | meaning |
|---|---|---|
| BeforeSourceNone | Resolution | current code holds a lost source for good |
| FixedSourceNone | - | S0 settles it with the retained notice |
| BeforeFence / FixedFence | Resolution | expected: an open fence still waits (S0 only names it) |
| BeforeUnread / FixedUnread | Resolution | expected: W2 (dialog over composer) still waits; follow-up |
| Before/FixedHealthy | - | |
| Before/FixedLaunching | - | a late source of this run still delivers |
| MutantSettleAnyLaunching | LaunchDelivers | dropping the launch guard settles an input that would deliver |

## S0b: a permission dialog over the composer
Code: `composer.rs` `when_idle` read `composer_owner` before its modal check, so a background agent's
permission prompt that replaces the composer read Unread and waited out the budget (`Waits(Observed)`).
The modal check now runs first and holds at once as `Waits(Modal)`. The held watch already existed:
`first_input.rs` records a probe (empty composer, no modal) that `held_watch.rs` reads every 2 s and that
wakes the drive once; 11-19 14:47 production delivered the 12:10 original that way after the prompt closed.

Added: `screen = dialog`, `DialogShown` (only while held), `DialogDismissed` (wakes unless mutant
`NoRewake`), invariant `DialogNamed` (a front held under a dialog says Modal).

| cfg | broken | meaning |
|---|---|---|
| BeforeDialog | DialogNamed | before: the dialog hold is named an observed wait |
| FixedDialog | - | named Modal; dismissal wakes and delivers |
| BeforeDialogStays | DialogNamed, Resolution | |
| FixedDialogStays | Resolution | expected: a prompt nobody answers keeps the input held |
| MutantNoRewakeDialog | Resolution | without the held-watch wake the dismissed dialog never delivers |

TLC 16/16 as expected.

## Fix 1: dialog structure, unproven dialogs and the tmux caller
Policy `S0b` is the S0b head; `Fixed` is this fix.
- `composer.rs` `dialog_open` holds only for a dialog's structure: a question, numbered choices from
  `1.` and its `Esc to cancel` footer at the bottom with no composer below. A footer alone (an effort
  overlay, an answer that says "Press Esc to cancel") keeps base's screen reading.
- `when_idle` reads the screen on its first look even without a proof, and once more before a budget
  settles; `held_watch` uses the same `dialog_open`. The probe still needs a proof to wake.
- `Watched::settling_lost_source` opts the Herdr first input into settling an older execution's
  missing source; `wait.rs` (tmux warm follow-up) keeps `Waits`.

Added: `screen = overlay`, `proved`, `ProofAppears`, `WatchLook` (the held watch; mutant
`NoRewake` now removes it), and an internal turn end proves the source.

Entry points and the model:

| code | model |
|---|---|
| `composer.rs` `when_idle` first look, `dialog_open` | `Attempt` / `CauseOf` "Modal" |
| `composer.rs` `when_idle` before `expired` | `Attempt` with `proved = FALSE` |
| `first_input.rs` probe (`dialog_open`, proof) | `WatchLook` |
| `held_watch.rs` `LOOK` / `Verdict::Wake` | `WatchLook` `wake' = TRUE` |
| `watched.rs` `expired` NoSource, `settle_lost` | `Settles` with `caller` |
| `wait.rs` `prepare` (tmux) | `caller = "tmux"` |
| `first_input.rs` `before_payload` `modal`, `ingress.rs` `Hold::Dialog` | not modelled: notice text only |

| cfg | broken | meaning |
|---|---|---|
| S0bDialogUnproven | DialogDelivers | S0b never read the screen without a proof; the front settled |
| FixedDialogUnproven | - | held as Modal, delivered once dismissed and proven |
| S0bOverlay | OverlayNotModal, Resolution | S0b took the footer for a dialog and the watch re-woke it forever |
| FixedOverlay | - | the overlay answer screen takes its keys |
| S0bTmuxLost | TmuxKeepsWaits | S0b settled a tmux warm follow-up's lost source |
| FixedTmuxLost | Resolution | expected: tmux keeps waiting as in base |

TLC 22/22 as expected.

## #8128 U: the session's own turn during the wait

11-21 adk-dash-cc: a front held in `Waits(Pending)` saw the session start its own turn (presence
idle→busy). The moved transcript ended `Unproven(Refused)` and the original was dropped with its
notice; the next input ended `Unproven(Retained)` with a resend ask and no retry.

Added: Policy `U`; scenarios `own_moved`, `own_screen`, `own_restart`, `own_stop`, `own_clear`,
`quiet_idle`; variables `own`, `moved`, `restarts`; actions `OwnTurnEnd`, `StopCommand`, `Restart`,
`ClearCommand`; invariant `NoSilentLoss`; properties `OwnDelivers`, `ClearResolves`.

| Transition | Code |
|---|---|
| `Attempt` cause `Refused` (moved, windows spent, screen shows no turn) | `claude_tui/readiness/watched.rs:170` |
| `Attempt` cause `Unproven`/`UnprovenTurn` (unmoved, screen not read) | `watched.rs:159` |
| `Attempt` cause `NoSource` settled for an older launch (`Unproven::Lost`) | `watched.rs:100` |
| U: `OwnTurn` from a moved transcript or a busy screen; held | `claude/herdr_turn/composer.rs:231` → `own_turn` `composer.rs:272` |
| held front keeps the preparation watch, no retained or refused record | `claude/herdr_turn/first_input.rs:232`, `:273`; `queue_io/input_owner/effect.rs:128` (`hold_not_sent`) |
| `settled` (resend ask) / `refused` | `effect.rs:204-210` → `queued_head/v2/retained.rs:201` / `v2/refused.rs:12` |
| `OwnTurnEnd` wake (turn boundary, held watch) | `queue_io/input_owner/drive.rs:113`, `held_watch.rs:65`; probe `first_input.rs:232-260` |
| `StopCommand` | `discord/commands/stop.rs:191` (pending queue drain) |
| `Restart` | durable owner queue; boot recovery `queue_io/input_owner/recovery.rs:8` |
| `ClearCommand` | `discord/commands/control.rs:546` |

| cfg | Before | U | meaning |
|---|---|---|---|
| BeforeOwnMoved | NoSilentLoss, OwnDelivers | - | the incident: the moved wait is refused for good |
| BeforeOwnScreen | NoSilentLoss, OwnDelivers | - | the incident's next input: retained with a resend ask |
| UOwnMoved / UOwnScreen / UOwnRestart / UOwnStop | - | all hold | held, then delivered once after the turn |
| UOwnClear | - | all hold | `ClearResolves`: delivered or disposed of by /clear |
| UQuietIdle / USourceNone | - | all hold | no own turn: the resend ask stays |
| UOwnMovedNoWake | - | OwnDelivers | without a turn-boundary or watch wake the held front never delivers |
| UOwnMovedMutRefuseMoved | - | NoSilentLoss, OwnDelivers | U without the moved check |
| UOwnScreenMutRetainScreen | - | NoSilentLoss, OwnDelivers | U without the screen check |

TLC 34/34 as expected (`scripts/ci/statecharts_tlc.py --base`).

## #8441: the inflight row of a start held before its first key

11-26: an AgentDesk start met a composer a person had just made Running (terminal input 0.24 s after
the start). The first input ended held before its first key, but its durable inflight row stayed
with a recorded Herdr turn start. No key followed that start, so the row's own end is never proven
and watcher and finalizer keep it. Every later idle gap restarted the same front and
`foreign_row::admit` refused it over its own earlier row (6 refusals in 13 min, never delivered).
Model code SHA: `c28732c52c06a73515c15a6895b331661dc62180`. A design contract check, not a test of
the code; the restart counterexample is not replayed against a restarted daemon.

Added: policies `Row` (U with the attempt's row modeled as main keeps it) and `Unwind` (the fix);
scenarios `racer`, `racer_restart`, `keyed_hold`; variables `row`, `sends`, `racers`, `blocked`,
`rewound`, `lost`; actions `RacerTurnStarts`, `OwnRowEnds`, `CrashMidAttempt`, and the
`FirstKeyHeld` rewind inside the held branch of `Attempt`; invariants `RetryNotRefusedByUntouched`,
`TouchedRowKept`, `NoAnswerLost`, `AtMostOnce`; property `HeldInputDelivers`; mutants `NoUnwind`,
`UnwindTouched`. The 34 earlier configs keep their results (`row` stays `none` there).

| Model | Code |
|---|---|
| `row = "untouched"`: the attempt ended Unsubmitted and FinishedUntouched, token still in this process | `provider/herdr_before_start.rs` `claude_observed_input`, `finish_input` |
| `row = "touched"`: Submitted or Unknown, or text typed without Enter | same; `claude/herdr_turn/first_input.rs` `herdr_input_hold_pending` |
| `row = "orphan"`: a row of an earlier daemon run, no token here | `recovery_engine/herdr_row_end.rs` `held_at_restore` |
| `Attempt` refused over a row (`blocked`) | `router/message_handler/foreign_row.rs` `admit` |
| `FirstKeyHeld` (`Rewinds("untouched")`) | `provider_dispatch/start_failure/herdr_unsent_unwind.rs` `unwind`, called from `Failure::finish` |
| an untouched row no path ends | `inflight/herdr_turn_evidence.rs` `own_terminal`; `turn_finalizer/cleanup/terminal_release.rs`, `tmux_watcher/terminal_commit_epilogue.rs` |
| `RacerTurnStarts` | a person's terminal input or a task-notification turn; composer Running at `claude/herdr_turn/composer.rs` |
| `OwnTurnEnd` wake for a racer | `queue_io/input_owner/drive.rs`, `held_watch.rs` |
| `OwnRowEnds` | the row's own terminal: watcher terminal commit, bridge epilogue |

| cfg | Row (main) | Unwind (fix) | meaning |
|---|---|---|---|
| RowRacer / UnwindRacer | RetryNotRefusedByUntouched, HeldInputDelivers | - | the incident: after one racer the front is refused for good; with the rewind it is delivered once in the next idle gap |
| RowKeyedHold / UnwindKeyedHold | - | - | a row whose key may have reached the pane stays and ends on its own terminal |
| RowRacerRestart / UnwindRacerRestart | RetryNotRefusedByUntouched, HeldInputDelivers | HeldInputDelivers | a daemon death mid-attempt leaves a row no token proves untouched; the fix does not cover it (hold cap backstop, #8430) |
| UnwindRacerMutNoUnwind | | RetryNotRefusedByUntouched, HeldInputDelivers | the fix without its rewind is main |
| UnwindKeyedHoldMutUnwindTouched | | TouchedRowKept, NoAnswerLost, AtMostOnce | a rewind that skips the Untouched check drops an answering row and lets the front send twice |

`AtMostOnce` in the last row shows the row's part only; the #8201 input hold is a second guard
outside this model.

TLC 42/42 as expected (`scripts/ci/statecharts_tlc.py --base`).
