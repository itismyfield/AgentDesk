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
EXTENDS Naturals

CONSTANTS Policy, Scenario, Mutant

ASSUME Policy \in {"Before", "Fixed"}
ASSUME Scenario \in {"source_none", "fence", "unread", "healthy", "launching"}
ASSUME Mutant \in {"None", "SettleAnyNoSource"}

MaxTurns == 2

VARIABLES queued, wake, turns, source, fence, screen, cause

vars == <<queued, wake, turns, source, fence, screen, cause>>

launch == IF Scenario = "launching" THEN "this" ELSE "before"

Init ==
    /\ queued = "held"
    /\ wake = TRUE
    /\ turns = 0
    /\ source = IF Scenario \in {"source_none", "launching"} THEN "none" ELSE "ok"
    /\ fence = IF Scenario = "fence" THEN "open" ELSE "none"
    /\ screen = IF Scenario = "unread" THEN "unread" ELSE "empty"
    /\ cause = "unset"

\* The cause `expired` names, in its own branch order: no source, open fence, then the observed
\* proof whose screen never let the composer reserve.
CauseOf ==
    IF source = "none" THEN "NoSource"
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
    /\ UNCHANGED <<turns, source, fence, screen>>

\* A subagent task-notification turn ends; the owner wakes. It clears no source, fence or dialog.
InternalTurnEnd ==
    /\ turns < MaxTurns
    /\ turns' = turns + 1
    /\ wake' = (queued = "held")
    /\ UNCHANGED <<queued, source, fence, screen, cause>>

\* A launch of this run publishes its source late; the preparation watch wakes the owner.
SourceAppears ==
    /\ launch = "this"
    /\ source = "none"
    /\ source' = "ok"
    /\ wake' = (queued = "held")
    /\ UNCHANGED <<queued, turns, fence, screen, cause>>

\* Nothing left to do: the input is resolved, or held with no wake (the stuck case).
Quiet == ~ENABLED (Attempt \/ InternalTurnEnd \/ SourceAppears) /\ UNCHANGED vars

Next == Attempt \/ InternalTurnEnd \/ SourceAppears \/ Quiet

Spec == Init /\ [][Next]_vars /\ WF_vars(Attempt) /\ WF_vars(InternalTurnEnd) /\ WF_vars(SourceAppears)

TypeOK ==
    /\ queued \in {"held", "sent", "settled"}
    /\ wake \in BOOLEAN
    /\ turns \in 0..MaxTurns
    /\ source \in {"ok", "none"}
    /\ fence \in {"none", "open"}
    /\ screen \in {"empty", "unread"}
    /\ cause \in {"unset", "none", "NoSource", "Fenced", "Observed"}

\* No key reaches a pane without its source, with an open fence, or over an unread composer.
KeysNeedProof == queued = "sent" => (source = "ok" /\ fence = "none" /\ screen = "empty")

\* Settlement is never taken for a wait some later look can still clear.
SettleOnlyCauseless == queued = "settled" => cause = "NoSource"

\* Liveness: the input is eventually sent or settled with its notice.
Resolution == <>(queued \in {"sent", "settled"})

\* A launch of this run whose source arrives late still delivers instead of settling (launching configs only).
LaunchDelivers == <>(queued = "sent")

====
