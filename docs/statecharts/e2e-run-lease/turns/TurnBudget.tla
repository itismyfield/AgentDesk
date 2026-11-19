---- MODULE TurnBudget ----
\* One driver run on the release host. Every input passes the turn gate, which first settles the
\* previous turn by its measured usage; unknown or over-limit usage stops all later input. The
\* harness runs inside the driver process, so its input ends when the lease holder dies.
EXTENDS Naturals
CONSTANTS Mut, Turns
VARIABLES sent, open, stopped, unknown, driver, reported, early, afterStop, orphan

vars == <<sent, open, stopped, unknown, driver, reported, early, afterStop, orphan>>

Init == /\ sent = 0 /\ open = FALSE /\ stopped = FALSE /\ unknown = FALSE /\ driver = "alive"
        /\ reported = "none" /\ early = FALSE /\ afterStop = FALSE /\ orphan = FALSE

\* The gate closes the open turn with what the transcripts show: measured, unmeasured or over its limit.
Settle(u) == /\ driver = "alive" /\ open /\ reported = "none"
             /\ open' = FALSE /\ unknown' = (unknown \/ u = "unknown") /\ stopped' = (stopped \/ u # "known")
             /\ UNCHANGED <<sent, driver, reported, early, afterStop, orphan>>

\* An input needs a live driver, a settled previous turn and a gate that has not stopped.
Send == /\ sent < Turns /\ reported = "none"
        /\ (driver = "alive" \/ Mut = "detached_writer")
        /\ (~open \/ Mut = "presettle")
        /\ (~stopped \/ Mut = "ignore_stop")
        /\ sent' = sent + 1 /\ open' = TRUE
        /\ early' = (early \/ open) /\ afterStop' = (afterStop \/ stopped) /\ orphan' = (orphan \/ driver = "dead")
        /\ UNCHANGED <<stopped, unknown, driver, reported>>

\* The report carries measured tokens only when every sent turn was measured.
Report == /\ driver = "alive" /\ ~open /\ reported = "none"
          /\ reported' = IF unknown /\ Mut # "partial_usage" THEN "unknown" ELSE "tokens"
          /\ UNCHANGED <<sent, open, stopped, unknown, driver, early, afterStop, orphan>>

\* The driver can die at any point; the channel lease is released with it.
Die == /\ driver = "alive" /\ driver' = "dead"
       /\ UNCHANGED <<sent, open, stopped, unknown, reported, early, afterStop, orphan>>

Next == (\E u \in {"known", "unknown", "over"}: Settle(u)) \/ Send \/ Report \/ Die \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars /\ WF_vars(Settle("known")) /\ WF_vars(Report)

NoInputBeforeSettlement == ~early
NoInputAfterStop == ~afterStop
NoInputAfterDriverDeath == ~orphan
UnmeasuredTurnIsNotReportedAsMeasured == reported = "tokens" => ~unknown
RunEnds == <>(reported # "none" \/ driver = "dead" \/ open)
====
