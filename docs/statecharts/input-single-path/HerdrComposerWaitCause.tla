---- MODULE HerdrComposerWaitCause ----
\* #8161 S0: a herdr Claude first input whose 10s composer readiness budget ends.
\* Code: claude/herdr_turn/composer.rs `when_idle` -> readiness/watched.rs `expired`.
\* Values come only from the design entry tables (report-r2-design.md §3, §5):
\*   source: the wait's Source at its first look (`Source::current`), "ok" | "none"
\*   fence:  continuation / pending / compact fences (watched.rs open/fenced), "none" | "open"
\*   screen: composer read by `composer_owner`, "empty" | "unread" (W2: a background agent's
\*           permission dialog over the composer reads Unread)
\*   launch: whether the execution started before this daemon run ("before") or in it ("this")
\* Actions: restart already happened (ledger empty); internal turns end (task-notification) and
\* wake the owner; a launch of this run may still publish its source (SessionStart).
\* S0b: a permission dialog may cover the composer (DialogShown) and be dismissed by a person
\* (DialogDismissed); the held watch (first_input.rs probe, held_watch.rs) then wakes the owner.
\* Policy "S0b" is that head; "Fixed" adds: the dialog is read by its structure (`dialog_open`) even
\* without a readiness proof, a footer-only overlay stays a plain screen, and only the Herdr first
\* input settles an older execution's missing source (tmux warm follow-up keeps `Waits`).
\* #8128 U: the session's own turn (terminal input, background task-notification) runs while the
\* front waits on a restored source with no proof. It moves the transcript (own_moved) or shows on the
\* screen over unchanged bytes (own_screen). Before/S0b/Fixed end that wait Unproven: Refused, the body
\* dropped (moved), or Retained with a resend ask (screen). Policy "U" keeps such a front held; the
\* turn's end proves the source and wakes the owner (turn boundary, held watch), restart re-drives it,
\* /stop ends the turn and drains the queue, /clear disposes of it with its notice. A quiet idle source
\* with no turn in flight (quiet_idle) keeps the resend ask, as does a lost older source.
\* Code (U): composer.rs `when_idle` -> `own_turn`; first_input.rs keeps a Waits hold at the front.
\* #8441: the durable inflight row of a start attempt. Policy "Row" is U with that row modeled as main
\* keeps it, "Unwind" the fix. row: "none" | "untouched" (its attempt ended held before the first key
\* and this process still holds that token: Unsubmitted and FinishedUntouched,
\* provider/herdr_before_start.rs `claude_observed_input`) | "touched" (the key may have reached the
\* pane: Submitted or Unknown) | "orphan" (left by an earlier daemon run; no token evidence here).
\* Nothing proves the own end of an untouched row (inflight/herdr_turn_evidence.rs `own_terminal`
\* reads from a start no key followed), so watcher and finalizer keep it and every later start of the
\* same front is refused at foreign_row.rs `admit`. FirstKeyHeld (the held branch of Attempt under
\* Unwind) rewinds exactly the untouched row before the front may retry; a touched row is never
\* rewound and ends on its own terminal (OwnRowEnds). RacerTurnStarts: a turn AgentDesk did not
\* start (terminal input, task-notification) begins before the first key, up to MaxRacers times.
\* keyed_hold: the first Enter's result is unknown and the attempt ends held with the body in the
\* pane (first_input.rs `herdr_input_hold_pending`). racer_restart: the daemon dies mid-attempt.
\* A held attempt ends in two steps: its producer returns (Attempt), then the start-failure exit
\* rewinds its row and answers the owner (FinishUnwind); the owner retries only once answered.
\* racer_native: the native producer unwinds the attempt itself first, cancelling its token as
\* cleanup (owner_input/native_input.rs `Producer::unwind`). Mutants of the first fixed head:
\* CleanupAsStop reads that cleanup cancel as a stop and skips the rewind; EarlyReply lets the launch
\* hook answer the owner when its reserve spent the claim, before the rewind (start_failure.rs
\* `guard_reply`).
EXTENDS Naturals

CONSTANTS Policy, Scenario, Mutant

ASSUME Policy \in {"Before", "S0b", "Fixed", "U", "Row", "Unwind"}
ASSUME Scenario \in {"source_none", "fence", "unread", "healthy", "launching", "dialog",
                      "dialog_stays", "dialog_unproven", "overlay", "tmux_lost",
                      "own_moved", "own_screen", "own_restart", "own_stop", "own_clear",
                      "quiet_idle", "racer", "racer_restart", "keyed_hold", "racer_native"}
ASSUME Mutant \in {"None", "SettleAnyNoSource", "NoRewake", "NoWake", "RefuseMoved",
                    "RetainScreen", "NoUnwind", "UnwindTouched", "CleanupAsStop", "EarlyReply"}

MaxTurns == 2
MaxRacers == 2

VARIABLES queued, wake, turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts,
          row, sends, racers, blocked, rewound, lost, answer, unwinding

base == <<queued, wake, turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts>>
rowVars == <<row, sends, racers, blocked, rewound, lost, answer, unwinding>>
vars == <<base, rowVars>>

\* U's reads carry into the row policies; Fixed's into all three.
ULike == Policy \in {"U", "Row", "Unwind"}
FixedLike == Policy = "Fixed" \/ ULike

\* #8441: the attempt's durable row is modeled only under these policies and scenarios.
RowModel == Policy \in {"Row", "Unwind"}
RowScenario == Scenario \in {"racer", "racer_restart", "keyed_hold", "racer_native"}
ASSUME RowModel <=> RowScenario

\* #8128 U scenarios: the session's own turn runs while the front waits; quiet_idle has none.
Own == Scenario \in {"own_moved", "own_screen", "own_restart", "own_stop", "own_clear"}
Unproven8128 == Own \/ Scenario = "quiet_idle"

dialogScenario == Scenario \in {"dialog", "dialog_stays"}

launch == IF Scenario = "launching" THEN "this" ELSE "before"

\* The wait's caller: the tmux warm follow-up (wait.rs `prepare`) or the Herdr first input.
caller == IF Scenario = "tmux_lost" THEN "tmux" ELSE "herdr"

Init ==
    /\ queued = "held"
    /\ wake = TRUE
    /\ turns = 0
    /\ source = IF Scenario \in {"source_none", "launching", "tmux_lost"} THEN "none" ELSE "ok"
    /\ fence = IF Scenario = "fence" THEN "open" ELSE "none"
    /\ screen = CASE Scenario = "unread" -> "unread"
                  [] Scenario = "dialog_unproven" -> "dialog"
                  [] Scenario = "own_screen" -> "turn"
                  [] Scenario = "overlay" -> "overlay"
                  [] OTHER -> "empty"
    /\ cause = "unset"
    /\ dialogs = IF Scenario = "dialog_unproven" THEN 1 ELSE 0
    \* A restored source idle and unmoved but with no readiness proof (`observe_restored` None).
    /\ proved = (Scenario # "dialog_unproven" /\ ~Unproven8128)
    \* own: "running" while the session's own turn is in flight, then "ended".
    /\ own = IF Own THEN "running" ELSE "none"
    \* The own turn moved the transcript inside the wait (watched.rs `moved`, windows spent).
    /\ moved = (Own /\ Scenario # "own_screen")
    /\ restarts = 0
    /\ row = "none"
    \* sends: times the body reached the pane. racers: turns AgentDesk did not start.
    /\ sends = 0
    /\ racers = 0
    \* blocked: the row kind that last refused a start. rewound: the row kind last rewound.
    /\ blocked = "none"
    /\ rewound = "none"
    \* lost: a turn's answer ended with its row already rewound.
    /\ lost = FALSE
    \* answer: the owner's reply for its last attempt. unwinding: that attempt's exit still runs.
    /\ answer = "none"
    /\ unwinding = FALSE

\* watched.rs `expired`: a moved transcript whose windows end on a screen showing no turn is Refused;
\* an unmoved one is Retained, whatever its screen shows ("UnprovenTurn" names a turn on it).
\* U (composer.rs `own_turn`): either trace of the session's own turn waits as "OwnTurn".
UnprovenCause ==
    IF /\ ULike
       /\ \/ moved /\ Mutant # "RefuseMoved"
          \/ screen = "turn" /\ Mutant # "RetainScreen"
    THEN "OwnTurn"
    ELSE IF moved THEN "Refused"
    ELSE IF screen = "turn" THEN "UnprovenTurn"
    ELSE "Unproven"

\* The cause `expired` names, in its own branch order: no source, open fence, then the observed
\* proof whose screen never let the composer reserve.
\* Before S0b a dialog over the composer read Unread and ended as an observed proof's wait.
\* S0b read the screen only under a proof and took any `Esc to cancel` footer, an overlay's too,
\* for a dialog. With no proof an idle unmoved source ends Unproven (retained with a resend ask).
CauseOf ==
    IF screen = "dialog" /\ (FixedLike \/ (Policy = "S0b" /\ proved)) THEN "Modal"
    ELSE IF screen = "overlay" /\ Policy = "S0b" /\ proved THEN "Modal"
    ELSE IF source = "none" THEN "NoSource"
    ELSE IF fence = "open" THEN "Fenced"
    ELSE IF ~proved THEN UnprovenCause
    ELSE "Observed"

\* The composer takes keys: an empty one, or one under an answer that only names a footer.
Clear == screen \in {"empty", "overlay"}

\* Only a causeless wait settles: a lost source of an execution older than this run, for the Herdr
\* caller (S0b settled it for tmux too), or an unproven idle source (unchanged since base).
Settles(c) ==
    \/ /\ (Policy = "S0b" \/ FixedLike)
       /\ c = "NoSource"
       /\ (launch = "before" \/ Mutant = "SettleAnyNoSource")
       /\ (caller = "herdr" \/ Policy = "S0b")
    \/ c \in {"Unproven", "UnprovenTurn"}

\* #8441 herdr_unsent_unwind: only the row of an attempt whose token shows Unsubmitted and Untouched
\* is rewound, by its exact identity and without the Herdr own-end hold.
Rewinds(kind) ==
    /\ Policy = "Unwind"
    /\ Mutant # "NoUnwind"
    /\ (kind = "untouched" \/ Mutant = "UnwindTouched")
    /\ ~(Scenario = "racer_native" /\ Mutant = "CleanupAsStop")

\* The row a keyed attempt of `kind` leaves when it ends held.
LeftRow(kind) == IF ~RowModel \/ Rewinds(kind) THEN "none" ELSE kind
Rewound(kind) == IF RowModel /\ Rewinds(kind) THEN kind ELSE rewound

\* keyed_hold: the first attempt's Enter goes out but its result is unknown.
KeyedHold == Scenario = "keyed_hold" /\ sends = 0

Attempt ==
    /\ queued = "held"
    /\ wake
    /\ answer # "pending"
    /\ IF row # "none"
         \* foreign_row.rs `admit`: a durable row holds the channel; the front is requeued.
         THEN /\ blocked' = row
              /\ wake' = FALSE
              /\ UNCHANGED <<queued, cause, row, sends, rewound, answer, unwinding>>
         ELSE IF proved /\ source = "ok" /\ fence = "none" /\ Clear /\ CauseOf # "Modal"
         THEN IF KeyedHold
              THEN /\ sends' = sends + 1
                   /\ cause' = "HoldPending"
                   /\ row' = LeftRow("touched")
                   /\ rewound' = Rewound("touched")
                   /\ wake' = FALSE
                   /\ UNCHANGED <<queued, blocked, answer, unwinding>>
              ELSE /\ queued' = "sent"
                   /\ sends' = sends + 1
                   /\ cause' = "none"
                   \* A submitted turn's row answers until its own terminal.
                   /\ row' = IF RowModel THEN "touched" ELSE "none"
                   /\ wake' = FALSE
                   /\ UNCHANGED <<blocked, rewound, answer, unwinding>>
         ELSE /\ cause' = CauseOf
              \* "refused": dropped for good with its notice, no body kept (refused.rs).
              /\ queued' = CASE Settles(CauseOf) -> "settled"
                              [] CauseOf = "Refused" -> "refused"
                              [] OTHER -> "held"
              /\ wake' = FALSE
              \* No key went out; the row stays until the exit's FinishUnwind.
              /\ row' = IF RowModel THEN "untouched" ELSE row
              /\ unwinding' = RowModel
              /\ answer' = CASE ~RowModel -> answer
                               [] Mutant = "EarlyReply" -> "given"
                               [] OTHER -> "pending"
              /\ UNCHANGED <<sends, blocked, rewound>>
    /\ UNCHANGED <<turns, source, fence, screen, dialogs, proved, own, moved, restarts, racers, lost>>

\* FirstKeyHeld: the exit rewinds the untouched row, then answers the owner (one transition).
FinishUnwind ==
    /\ unwinding
    /\ row' = IF row = "untouched" /\ Rewinds("untouched") THEN "none" ELSE row
    /\ rewound' = IF row = "untouched" /\ Rewinds("untouched") THEN "untouched" ELSE rewound
    /\ answer' = "given"
    /\ unwinding' = FALSE
    /\ UNCHANGED <<base, sends, racers, blocked, lost>>

\* A subagent task-notification turn ends and proves the source; the owner wakes. It clears no
\* missing source, fence or dialog.
InternalTurnEnd ==
    /\ ~Unproven8128
    /\ turns < MaxTurns
    /\ turns' = turns + 1
    /\ wake' = (queued = "held")
    /\ proved' = TRUE
    /\ UNCHANGED <<queued, source, fence, screen, cause, dialogs, own, moved, restarts>>
    /\ UNCHANGED rowVars

\* A launch of this run publishes its source late; the preparation watch wakes the owner.
SourceAppears ==
    /\ launch = "this"
    /\ source = "none"
    /\ source' = "ok"
    /\ wake' = (queued = "held")
    /\ UNCHANGED <<queued, turns, fence, screen, cause, dialogs, proved, own, moved, restarts>>
    /\ UNCHANGED rowVars

\* A background agent's permission prompt covers the composer.
DialogShown ==
    /\ dialogScenario
    /\ queued = "held"
    /\ screen = "empty"
    /\ dialogs = 0
    /\ screen' = "dialog"
    /\ dialogs' = 1
    /\ UNCHANGED <<queued, wake, turns, source, fence, cause, proved, own, moved, restarts>>
    /\ UNCHANGED rowVars

\* A person answers the prompt after the front's first look saw it (a prompt gone before that look
\* leaves the unproven source of base).
DialogDismissed ==
    /\ Scenario \in {"dialog", "dialog_unproven"}
    /\ screen = "dialog"
    /\ cause # "unset"
    /\ screen' = "empty"
    /\ UNCHANGED <<queued, wake, turns, source, fence, cause, dialogs, proved, own, moved, restarts>>
    /\ UNCHANGED rowVars

\* A finished turn proves the restored source; nothing wakes the owner but the held watch.
ProofAppears ==
    /\ ~Unproven8128
    /\ ~proved
    /\ proved' = TRUE
    /\ UNCHANGED <<queued, wake, turns, source, fence, screen, cause, dialogs, own, moved, restarts>>
    /\ UNCHANGED rowVars

\* The held watch (first_input.rs probe, held_watch.rs `LOOK`): a proof, an empty composer and no
\* dialog by the same `dialog_open` (an overlay footer passes it) wake a held front once.
WatchLook ==
    /\ Mutant \notin {"NoRewake", "NoWake"}
    /\ queued = "held"
    /\ ~wake
    /\ proved /\ source = "ok" /\ fence = "none" /\ Clear
    /\ wake' = TRUE
    /\ UNCHANGED <<queued, turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts>>
    /\ UNCHANGED rowVars

\* The own turn ends with its Stop: the source is proven (readiness/mod.rs `evidence`), the screen idles
\* and the turn boundary wakes a held owner (drive.rs `run`). /stop is the only end of own_stop's turn.
OwnTurnEnd ==
    /\ own = "running"
    /\ Scenario # "own_stop"
    /\ own' = "ended"
    /\ proved' = TRUE
    /\ screen' = IF screen = "turn" THEN "empty" ELSE screen
    /\ wake' = (queued = "held" /\ Mutant # "NoWake")
    /\ UNCHANGED <<queued, turns, source, fence, cause, dialogs, moved, restarts>>
    /\ UNCHANGED rowVars

\* /stop interrupts the own turn (its Stop proves the source) and drains the pending queue
\* (commands/stop.rs `after_native_publish` -> `schedule_text_stop_pending_queue_drain`).
StopCommand ==
    /\ Scenario = "own_stop"
    /\ own = "running"
    /\ own' = "ended"
    /\ proved' = TRUE
    /\ screen' = IF screen = "turn" THEN "empty" ELSE screen
    /\ wake' = (queued = "held" /\ Mutant # "NoWake")
    /\ UNCHANGED <<queued, turns, source, fence, cause, dialogs, moved, restarts>>
    /\ UNCHANGED rowVars

\* A daemon restart keeps the held front in the durable queue; the boot drive grants it again.
Restart ==
    /\ Scenario = "own_restart"
    /\ queued = "held"
    /\ restarts = 0
    /\ restarts' = 1
    /\ wake' = (Mutant # "NoWake")
    /\ UNCHANGED <<queued, turns, source, fence, screen, cause, dialogs, proved, own, moved>>
    /\ UNCHANGED rowVars

\* /clear discards the channel's queue with its reply (commands/control.rs
\* `clear_channel_session_state_with_session_key`): the person disposed of the input.
ClearCommand ==
    /\ Scenario = "own_clear"
    /\ queued = "held"
    /\ queued' = "cleared"
    /\ UNCHANGED <<wake, turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts>>
    /\ UNCHANGED rowVars

\* #8441: a turn AgentDesk did not start (a person typing in the TUI, a task-notification) begins
\* while the front waits, so the next first key meets a Running composer. No fairness: it may never
\* come, and comes at most MaxRacers times.
RacerTurnStarts ==
    /\ Scenario \in {"racer", "racer_restart", "racer_native"}
    /\ queued = "held"
    /\ racers < MaxRacers
    /\ own # "running"
    /\ screen = "empty"
    /\ racers' = racers + 1
    /\ own' = "running"
    /\ screen' = "turn"
    /\ UNCHANGED <<queued, wake, turns, source, fence, cause, dialogs, proved, moved, restarts,
                   row, sends, blocked, rewound, lost, answer, unwinding>>

\* The turn of a row whose key reached the pane ends on its own terminal: the row is cleared and a
\* front still held for it is settled as delivered. With that row already rewound the answer has no
\* row to land on, and the turn boundary re-drives the front that nothing refuses any more.
OwnRowEnds ==
    /\ RowModel
    /\ sends > 0
    /\ \/ /\ row = "touched"
          /\ row' = "none"
          /\ queued' = IF queued = "held" THEN "sent" ELSE queued
          /\ UNCHANGED <<wake, lost>>
       \/ /\ row = "none"
          /\ queued = "held"
          /\ ~lost
          /\ lost' = TRUE
          /\ wake' = TRUE
          /\ UNCHANGED <<row, queued>>
    /\ UNCHANGED <<turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts,
                   sends, racers, blocked, rewound, answer, unwinding>>

\* The daemon dies while an attempt waits on a busy composer: its row stays and its token is gone.
\* Boot re-drives the front; nothing in the new process proves that row untouched.
CrashMidAttempt ==
    /\ Scenario = "racer_restart"
    /\ queued = "held"
    /\ wake
    /\ row = "none"
    /\ ~unwinding
    /\ screen = "turn"
    /\ restarts = 0
    /\ row' = "orphan"
    /\ restarts' = 1
    /\ UNCHANGED <<queued, wake, turns, source, fence, screen, cause, dialogs, proved, own, moved,
                   sends, racers, blocked, rewound, lost, answer, unwinding>>

Moves == Attempt \/ InternalTurnEnd \/ SourceAppears \/ DialogShown \/ DialogDismissed
         \/ ProofAppears \/ WatchLook \/ OwnTurnEnd \/ StopCommand \/ Restart \/ ClearCommand
         \/ RacerTurnStarts \/ OwnRowEnds \/ CrashMidAttempt \/ FinishUnwind

\* Nothing left to do: the input is resolved, or held with no wake (the stuck case).
Quiet == ~ENABLED Moves /\ UNCHANGED vars

Next == Moves \/ Quiet

Spec == Init /\ [][Next]_vars /\ WF_vars(Attempt) /\ WF_vars(InternalTurnEnd)
        /\ WF_vars(SourceAppears) /\ WF_vars(DialogShown) /\ WF_vars(DialogDismissed)
        /\ WF_vars(ProofAppears) /\ WF_vars(WatchLook) /\ WF_vars(OwnTurnEnd)
        /\ WF_vars(StopCommand) /\ WF_vars(Restart) /\ WF_vars(ClearCommand)
        /\ WF_vars(OwnRowEnds) /\ WF_vars(FinishUnwind)

TypeOK ==
    /\ queued \in {"held", "sent", "settled", "refused", "cleared"}
    /\ wake \in BOOLEAN
    /\ turns \in 0..MaxTurns
    /\ source \in {"ok", "none"}
    /\ fence \in {"none", "open"}
    /\ screen \in {"empty", "unread", "dialog", "overlay", "turn"}
    /\ cause \in {"unset", "none", "NoSource", "Fenced", "Observed", "Modal", "Unproven",
                  "UnprovenTurn", "Refused", "OwnTurn", "HoldPending"}
    /\ dialogs \in 0..1
    /\ proved \in BOOLEAN
    /\ own \in {"none", "running", "ended"}
    /\ moved \in BOOLEAN
    /\ restarts \in 0..1
    /\ row \in {"none", "untouched", "touched", "orphan"}
    /\ sends \in 0..2
    /\ racers \in 0..MaxRacers
    /\ blocked \in {"none", "untouched", "touched", "orphan"}
    /\ rewound \in {"none", "untouched", "touched"}
    /\ lost \in BOOLEAN
    /\ answer \in {"none", "pending", "given"}
    /\ unwinding \in BOOLEAN

\* No key reaches a pane without its source, with an open fence, or over an unread composer.
KeysNeedProof == queued = "sent" => (proved /\ source = "ok" /\ fence = "none" /\ Clear)

\* Settlement is never taken for a wait some later look can still clear.
SettleOnlyCauseless == queued = "settled" => cause \in {"NoSource", "Unproven"}

\* An answer that only names an `Esc to cancel` footer over an empty composer is no dialog.
OverlayNotModal == screen = "overlay" => cause # "Modal"

\* The tmux warm follow-up keeps waiting on a lost source; it is never settled (tmux_lost configs).
TmuxKeepsWaits == caller = "tmux" => queued # "settled"

\* A front held under a dialog says so (S0b dialog configs).
DialogNamed == (queued = "held" /\ screen = "dialog" /\ cause # "unset") => cause = "Modal"

\* #8128 U, the person's input is never lost silently: it leaves unsent only by a settlement that keeps
\* the original with its resend ask, and only when no own turn left a trace this wait saw (a lost
\* older source or a quiet idle one), or by the person's own /clear. Refused (dropped) never.
NoSilentLoss ==
    /\ queued # "refused"
    /\ queued = "settled" => cause \in {"NoSource", "Unproven"}

\* Liveness: the input is eventually sent or settled with its notice.
Resolution == <>(queued \in {"sent", "settled"})

\* A launch of this run whose source arrives late still delivers instead of settling (launching configs only).
LaunchDelivers == <>(queued = "sent")

\* A dialog over an unproven front delivers once dismissed and proven (dialog_unproven configs).
DialogDelivers == <>(queued = "sent")

\* #8128 U: a front held under the session's own turn is eventually delivered (own configs but clear).
OwnDelivers == <>(queued = "sent")

\* /clear during the own turn: delivered first, or disposed of by the person's command.
ClearResolves == <>(queued \in {"sent", "cleared"})

\* #8441: a retry of the same front is never refused by the row of an earlier attempt that this
\* process proved never reached its first key.
RetryNotRefusedByUntouched == blocked # "untouched"

\* A row whose first key may have reached the pane is never rewound, so its answer keeps its row
\* and the body reaches the pane at most once.
TouchedRowKept == rewound # "touched"

\* The owner hears an attempt's refusal only after that attempt's exit ran (its row rewound).
AnsweredAfterExit == unwinding => answer # "given"
NoAnswerLost == ~lost
AtMostOnce == sends <= 1

\* Once an idle gap comes the held input is delivered (with AtMostOnce: exactly once).
HeldInputDelivers == <>(queued = "sent")

====
