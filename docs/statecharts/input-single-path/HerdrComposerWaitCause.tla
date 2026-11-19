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
EXTENDS Naturals

CONSTANTS Policy, Scenario, Mutant

ASSUME Policy \in {"Before", "Fixed"}
ASSUME Scenario \in {"source_none", "fence", "unread", "healthy", "launching", "dialog",
                      "dialog_stays"}
ASSUME Mutant \in {"None", "SettleAnyNoSource", "NoRewake"}

MaxTurns == 2

VARIABLES queued, wake, turns, source, fence, screen, cause, dialogs

vars == <<queued, wake, turns, source, fence, screen, cause, dialogs>>

dialogScenario == Scenario \in {"dialog", "dialog_stays"}

launch == IF Scenario = "launching" THEN "this" ELSE "before"

Init ==
    /\ queued = "held"
    /\ wake = TRUE
    /\ turns = 0
    /\ source = IF Scenario \in {"source_none", "launching"} THEN "none" ELSE "ok"
    /\ fence = IF Scenario = "fence" THEN "open" ELSE "none"
    /\ screen = IF Scenario = "unread" THEN "unread" ELSE "empty"
    /\ cause = "unset"
    /\ dialogs = 0

\* The cause `expired` names, in its own branch order: no source, open fence, then the observed
\* proof whose screen never let the composer reserve.
\* Before S0b a dialog over the composer read Unread and ended as an observed proof's wait.
CauseOf ==
    IF screen = "dialog" /\ Policy = "Fixed" THEN "Modal"
    ELSE IF source = "none" THEN "NoSource"
    ELSE IF fence = "open" THEN "Fenced"
    ELSE "Observed"

\* Only a causeless wait settles: a lost source of an execution older than this run.
Settles(c) ==
    /\ Policy = "Fixed"
    /\ c = "NoSource"
    /\ (launch = "before" \/ Mutant = "SettleAnyNoSource")

Attempt ==
    /\ queued = "held"
    /\ wake
    /\ IF source = "ok" /\ fence = "none" /\ screen = "empty"
         THEN /\ queued' = "sent"
              /\ cause' = "none"
              /\ wake' = FALSE
         ELSE /\ cause' = CauseOf
              /\ queued' = IF Settles(CauseOf) THEN "settled" ELSE "held"
              /\ wake' = FALSE
    /\ UNCHANGED <<turns, source, fence, screen, dialogs>>

\* A subagent task-notification turn ends; the owner wakes. It clears no source, fence or dialog.
InternalTurnEnd ==
    /\ turns < MaxTurns
    /\ turns' = turns + 1
    /\ wake' = (queued = "held")
    /\ UNCHANGED <<queued, source, fence, screen, cause, dialogs>>

\* A launch of this run publishes its source late; the preparation watch wakes the owner.
SourceAppears ==
    /\ launch = "this"
    /\ source = "none"
    /\ source' = "ok"
    /\ wake' = (queued = "held")
    /\ UNCHANGED <<queued, turns, fence, screen, cause, dialogs>>

\* A background agent's permission prompt covers the composer.
DialogShown ==
    /\ dialogScenario
    /\ queued = "held"
    /\ screen = "empty"
    /\ dialogs = 0
    /\ screen' = "dialog"
    /\ dialogs' = 1
    /\ UNCHANGED <<queued, wake, turns, source, fence, cause>>

\* A person answers the prompt; the held watch's next look sees the empty composer and wakes.
DialogDismissed ==
    /\ Scenario = "dialog"
    /\ screen = "dialog"
    /\ screen' = "empty"
    /\ wake' = (queued = "held" /\ Mutant # "NoRewake")
    /\ UNCHANGED <<queued, turns, source, fence, cause, dialogs>>

Moves == Attempt \/ InternalTurnEnd \/ SourceAppears \/ DialogShown \/ DialogDismissed

\* Nothing left to do: the input is resolved, or held with no wake (the stuck case).
Quiet == ~ENABLED Moves /\ UNCHANGED vars

Next == Moves \/ Quiet

Spec == Init /\ [][Next]_vars /\ WF_vars(Attempt) /\ WF_vars(InternalTurnEnd)
        /\ WF_vars(SourceAppears) /\ WF_vars(DialogShown) /\ WF_vars(DialogDismissed)

TypeOK ==
    /\ queued \in {"held", "sent", "settled"}
    /\ wake \in BOOLEAN
    /\ turns \in 0..MaxTurns
    /\ source \in {"ok", "none"}
    /\ fence \in {"none", "open"}
    /\ screen \in {"empty", "unread", "dialog"}
    /\ cause \in {"unset", "none", "NoSource", "Fenced", "Observed", "Modal"}
    /\ dialogs \in 0..1

\* No key reaches a pane without its source, with an open fence, or over an unread composer.
KeysNeedProof == queued = "sent" => (source = "ok" /\ fence = "none" /\ screen = "empty")

\* Settlement is never taken for a wait some later look can still clear.
SettleOnlyCauseless == queued = "settled" => cause = "NoSource"

\* A front held under a dialog says so (S0b dialog configs).
DialogNamed == (queued = "held" /\ screen = "dialog" /\ cause # "unset") => cause = "Modal"

\* Liveness: the input is eventually sent or settled with its notice.
Resolution == <>(queued \in {"sent", "settled"})

\* A launch of this run whose source arrives late still delivers instead of settling (launching configs only).
LaunchDelivers == <>(queued = "sent")

====
