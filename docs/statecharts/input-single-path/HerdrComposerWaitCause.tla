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
EXTENDS Naturals

CONSTANTS Policy, Scenario, Mutant

ASSUME Policy \in {"Before", "S0b", "Fixed", "U"}
ASSUME Scenario \in {"source_none", "fence", "unread", "healthy", "launching", "dialog",
                      "dialog_stays", "dialog_unproven", "overlay", "tmux_lost",
                      "own_moved", "own_screen", "own_restart", "own_stop", "own_clear",
                      "quiet_idle"}
ASSUME Mutant \in {"None", "SettleAnyNoSource", "NoRewake", "NoWake", "RefuseMoved",
                    "RetainScreen"}

MaxTurns == 2

VARIABLES queued, wake, turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts

vars == <<queued, wake, turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts>>

\* Fixed's reads carry into U.
FixedLike == Policy \in {"Fixed", "U"}

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

\* watched.rs `expired`: a moved transcript whose windows end on a screen showing no turn is Refused;
\* an unmoved one is Retained, whatever its screen shows ("UnprovenTurn" names a turn on it).
\* U (composer.rs `own_turn`): either trace of the session's own turn waits as "OwnTurn".
UnprovenCause ==
    IF /\ Policy = "U"
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
    \/ /\ Policy \in {"S0b", "Fixed", "U"}
       /\ c = "NoSource"
       /\ (launch = "before" \/ Mutant = "SettleAnyNoSource")
       /\ (caller = "herdr" \/ Policy = "S0b")
    \/ c \in {"Unproven", "UnprovenTurn"}

Attempt ==
    /\ queued = "held"
    /\ wake
    /\ IF proved /\ source = "ok" /\ fence = "none" /\ Clear /\ CauseOf # "Modal"
         THEN /\ queued' = "sent"
              /\ cause' = "none"
              /\ wake' = FALSE
         ELSE /\ cause' = CauseOf
              \* "refused": dropped for good with its notice, no body kept (refused.rs).
              /\ queued' = CASE Settles(CauseOf) -> "settled"
                              [] CauseOf = "Refused" -> "refused"
                              [] OTHER -> "held"
              /\ wake' = FALSE
    /\ UNCHANGED <<turns, source, fence, screen, dialogs, proved, own, moved, restarts>>

\* A subagent task-notification turn ends and proves the source; the owner wakes. It clears no
\* missing source, fence or dialog.
InternalTurnEnd ==
    /\ ~Unproven8128
    /\ turns < MaxTurns
    /\ turns' = turns + 1
    /\ wake' = (queued = "held")
    /\ proved' = TRUE
    /\ UNCHANGED <<queued, source, fence, screen, cause, dialogs, own, moved, restarts>>

\* A launch of this run publishes its source late; the preparation watch wakes the owner.
SourceAppears ==
    /\ launch = "this"
    /\ source = "none"
    /\ source' = "ok"
    /\ wake' = (queued = "held")
    /\ UNCHANGED <<queued, turns, fence, screen, cause, dialogs, proved, own, moved, restarts>>

\* A background agent's permission prompt covers the composer.
DialogShown ==
    /\ dialogScenario
    /\ queued = "held"
    /\ screen = "empty"
    /\ dialogs = 0
    /\ screen' = "dialog"
    /\ dialogs' = 1
    /\ UNCHANGED <<queued, wake, turns, source, fence, cause, proved, own, moved, restarts>>

\* A person answers the prompt after the front's first look saw it (a prompt gone before that look
\* leaves the unproven source of base).
DialogDismissed ==
    /\ Scenario \in {"dialog", "dialog_unproven"}
    /\ screen = "dialog"
    /\ cause # "unset"
    /\ screen' = "empty"
    /\ UNCHANGED <<queued, wake, turns, source, fence, cause, dialogs, proved, own, moved, restarts>>

\* A finished turn proves the restored source; nothing wakes the owner but the held watch.
ProofAppears ==
    /\ ~Unproven8128
    /\ ~proved
    /\ proved' = TRUE
    /\ UNCHANGED <<queued, wake, turns, source, fence, screen, cause, dialogs, own, moved, restarts>>

\* The held watch (first_input.rs probe, held_watch.rs `LOOK`): a proof, an empty composer and no
\* dialog by the same `dialog_open` (an overlay footer passes it) wake a held front once.
WatchLook ==
    /\ Mutant \notin {"NoRewake", "NoWake"}
    /\ queued = "held"
    /\ ~wake
    /\ proved /\ source = "ok" /\ fence = "none" /\ Clear
    /\ wake' = TRUE
    /\ UNCHANGED <<queued, turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts>>

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

\* A daemon restart keeps the held front in the durable queue; the boot drive grants it again.
Restart ==
    /\ Scenario = "own_restart"
    /\ queued = "held"
    /\ restarts = 0
    /\ restarts' = 1
    /\ wake' = (Mutant # "NoWake")
    /\ UNCHANGED <<queued, turns, source, fence, screen, cause, dialogs, proved, own, moved>>

\* /clear discards the channel's queue with its reply (commands/control.rs
\* `clear_channel_session_state_with_session_key`): the person disposed of the input.
ClearCommand ==
    /\ Scenario = "own_clear"
    /\ queued = "held"
    /\ queued' = "cleared"
    /\ UNCHANGED <<wake, turns, source, fence, screen, cause, dialogs, proved, own, moved, restarts>>

Moves == Attempt \/ InternalTurnEnd \/ SourceAppears \/ DialogShown \/ DialogDismissed
         \/ ProofAppears \/ WatchLook \/ OwnTurnEnd \/ StopCommand \/ Restart \/ ClearCommand

\* Nothing left to do: the input is resolved, or held with no wake (the stuck case).
Quiet == ~ENABLED Moves /\ UNCHANGED vars

Next == Moves \/ Quiet

Spec == Init /\ [][Next]_vars /\ WF_vars(Attempt) /\ WF_vars(InternalTurnEnd)
        /\ WF_vars(SourceAppears) /\ WF_vars(DialogShown) /\ WF_vars(DialogDismissed)
        /\ WF_vars(ProofAppears) /\ WF_vars(WatchLook) /\ WF_vars(OwnTurnEnd)
        /\ WF_vars(StopCommand) /\ WF_vars(Restart) /\ WF_vars(ClearCommand)

TypeOK ==
    /\ queued \in {"held", "sent", "settled", "refused", "cleared"}
    /\ wake \in BOOLEAN
    /\ turns \in 0..MaxTurns
    /\ source \in {"ok", "none"}
    /\ fence \in {"none", "open"}
    /\ screen \in {"empty", "unread", "dialog", "overlay", "turn"}
    /\ cause \in {"unset", "none", "NoSource", "Fenced", "Observed", "Modal", "Unproven",
                  "UnprovenTurn", "Refused", "OwnTurn"}
    /\ dialogs \in 0..1
    /\ proved \in BOOLEAN
    /\ own \in {"none", "running", "ended"}
    /\ moved \in BOOLEAN
    /\ restarts \in 0..1

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

====
