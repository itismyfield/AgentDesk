------------------------- MODULE GatewayBootShutdown -------------------------
\* #8351 round 3: one SIGTERM against a process whose Claude runtime C is still in the
\* native boot gate (runtime_bootstrap.rs:299-301) while another provider O already
\* runs its SIGTERM handler (:332/:408/:453). Both share one shutdown barrier of 2 slots
\* (shared_state.rs:617-621); each runtime has its own shutting_down flag (shared_data.rs:163-180).
\* Mode = "before":       53de918 - the gate reads only C's own flag, which nothing sets
\*                        before C installs its handler; that handler opens a fresh stream.
\* Mode = "after":        the gate waits on the process SIGTERM latch armed before :299; a
\*                        signal observed by the gate stops C and takes its slot once, and the
\*                        handler installed after the gate reads the same latch.
\* Mode = "noreadycheck": mutation - a Ready attempt does not look at the latch.
\* Mode = "nolast":       mutation - the gate takes the last slot without exiting.
\* Mode = "freshstream":  mutation - C's handler opens its own stream (no handoff).
\* Refusable: preparation may refuse; the refused runtime hands its slot to the latch.
EXTENDS Naturals
CONSTANTS Mode, Refusable
VARIABLES term, busy, c, cHears, cCounted, oCounted, exited, termAtGate
vars == <<term, busy, c, cHears, cCounted, oCounted, exited, termAtGate>>

CStates == {"attempt", "wait", "ready", "started", "stopped", "refused"}
TypeOK == /\ term \in BOOLEAN /\ busy \in BOOLEAN /\ c \in CStates /\ cHears \in BOOLEAN
          /\ cCounted \in BOOLEAN /\ oCounted \in BOOLEAN /\ exited \in BOOLEAN
          /\ termAtGate \in BOOLEAN

Init == /\ term = FALSE /\ busy \in BOOLEAN /\ c = "attempt" /\ cHears = FALSE
        /\ cCounted = FALSE /\ oCounted = FALSE /\ exited = FALSE /\ termAtGate = FALSE

GateSees == Mode # "before"
HandlerSees == IF Mode \in {"before", "freshstream"} THEN cHears ELSE term
\* shutdown.rs:34-54: the CAS takes a slot once; the holder of the last slot exits.
Take(lastExits) == /\ cCounted' = TRUE
                   /\ exited' = (exited \/ (oCounted /\ lastExits))

\* The process gets SIGTERM once. A handler stream that exists at that moment hears it.
Term == /\ ~term /\ ~exited /\ term' = TRUE /\ cHears' = (c = "started")
        /\ UNCHANGED <<busy, c, cCounted, oCounted, exited, termAtGate>>
\* O is past its gate: its handler persists and takes its slot (shutdown.rs:14-54).
OHandle == /\ term /\ ~oCounted /\ ~exited /\ oCounted' = TRUE /\ exited' = cCounted
           /\ UNCHANGED <<term, busy, c, cHears, cCounted, termAtGate>>
\* The history lock holder finishes (environment progress).
ClearBusy == /\ busy /\ busy' = FALSE
             /\ UNCHANGED <<term, c, cHears, cCounted, oCounted, exited, termAtGate>>

\* An attempt runs to completion; only then is the latch looked at (native_input.rs).
CAttempt ==
    /\ c = "attempt" /\ ~exited
    /\ \/ /\ GateSees /\ term /\ Mode # "noreadycheck"
          /\ c' = "stopped" /\ Take(Mode # "nolast") /\ UNCHANGED termAtGate
       \/ /\ ~(GateSees /\ term /\ Mode # "noreadycheck")
          /\ \/ /\ busy /\ c' = "wait" /\ UNCHANGED <<cCounted, exited, termAtGate>>
             \/ /\ ~busy /\ c' = "ready" /\ termAtGate' = term
                /\ UNCHANGED <<cCounted, exited>>
             \/ /\ Refusable /\ c' = "refused" /\ UNCHANGED <<cCounted, exited, termAtGate>>
    /\ UNCHANGED <<term, busy, cHears, oCounted>>
\* The 5 s retry sleep, raced against the latch in "after".
CWaitEnd == /\ c = "wait" /\ ~exited
            /\ IF GateSees /\ term
               THEN c' = "stopped" /\ Take(Mode # "nolast")
               ELSE c' = "attempt" /\ UNCHANGED <<cCounted, exited>>
            /\ UNCHANGED <<term, busy, cHears, oCounted, termAtGate>>
\* Ready: lease, intake and the runtime's own SIGTERM handler (runtime_bootstrap.rs:332/:408/:453).
CStart == /\ c = "ready" /\ ~exited /\ c' = "started"
          /\ UNCHANGED <<term, busy, cHears, cCounted, oCounted, exited, termAtGate>>
CHandle == /\ c = "started" /\ HandlerSees /\ ~cCounted /\ ~exited /\ Take(TRUE)
           /\ UNCHANGED <<term, busy, c, cHears, oCounted, termAtGate>>
\* A refused runtime returns from run_bot; in "before" its slot is never taken.
CRefusedSlot == /\ c = "refused" /\ Mode # "before" /\ term /\ ~cCounted /\ ~exited
                /\ Take(Mode # "nolast")
                /\ UNCHANGED <<term, busy, c, cHears, oCounted, termAtGate>>

Next == Term \/ OHandle \/ ClearBusy \/ CAttempt \/ CWaitEnd \/ CStart \/ CHandle \/ CRefusedSlot

Spec == Init /\ [][Next]_vars
        /\ WF_vars(OHandle) /\ WF_vars(ClearBusy) /\ WF_vars(CAttempt) /\ WF_vars(CWaitEnd)
        /\ WF_vars(CStart) /\ WF_vars(CHandle) /\ WF_vars(CRefusedSlot)

\* ---- rules ----
\* A gate that decided after the shutdown request starts no runtime.
NoStartAfterTerm == ~(c \in {"ready", "started"} /\ termAtGate)
\* The shutdown barrier ends: the last slot holder exits.
BarrierEnds == term ~> exited
=============================================================================
