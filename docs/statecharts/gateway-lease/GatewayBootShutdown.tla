------------------------- MODULE GatewayBootShutdown -------------------------
\* #8351 rounds 3-4: one SIGTERM against a process whose Claude runtime C has no SIGTERM
\* handler yet: it is in the native boot gate (runtime_bootstrap.rs:301-303), in the gateway
\* lease phase after a Ready gate, or out of run_bot after a failed lease or a refusal.
\* With Providers = 2 another provider O already runs its handler (shutdown.rs:14-54).
\* All runtimes share one shutdown barrier (shared_state.rs:617-621); each has its own
\* shutting_down flag (shared_data.rs:163-180).
\* While no tokio SIGTERM listener exists, SIGTERM keeps its default action and kills the process.
\* Mode = "r2":          53de918 - the gate reads only C's own flag, which nothing sets.
\* Mode = "r3":          2b93d60 - the gate registers a process SIGTERM latch before its first
\*                       attempt (so the default action is gone); a failed lease still does a
\*                       raw fetch_sub.
\* Mode = "b":           round 4 - nothing registers early; O's handler sets a process mark, the
\*                       gate stops on it, a slot the gate gives up exits when it was the last
\*                       after SIGTERM, and a later handler also takes the mark.
\* Mutations of "b": "b_nomark" (gate ignores the mark), "b_nohandoff" (C's handler hears
\* only its own stream), "b_noexit" (a slot the gate gives up never exits).
\* A failed lease keeps run_bot's raw fetch_sub in every mode (its body is census-pinned).
\* LeaseEnds: preferred-poll eventually ends (acquired, standby or failed). FALSE models a
\* peer whose handback stays suppressed (gateway_handback_breaker.rs:23-25).
EXTENDS Naturals
CONSTANTS Mode, Providers, LeaseEnds, LeaseCanFail, Refusable
VARIABLES term, done, busy, c, cHeard, cCounted, oCounted, mark, termAtGate
vars == <<term, done, busy, c, cHeard, cCounted, oCounted, mark, termAtGate>>

CStates == {"gate", "wait", "ready", "lease", "started", "stopped", "failed", "refused"}
TypeOK == /\ term \in BOOLEAN /\ done \in {"no", "exit", "killed"} /\ busy \in BOOLEAN
          /\ c \in CStates /\ cHeard \in BOOLEAN /\ cCounted \in BOOLEAN
          /\ oCounted \in BOOLEAN /\ mark \in BOOLEAN /\ termAtGate \in BOOLEAN

Init == /\ term = FALSE /\ done = "no" /\ busy \in BOOLEAN /\ c = "gate" /\ cHeard = FALSE
        /\ cCounted = FALSE /\ oCounted = FALSE /\ mark = FALSE /\ termAtGate = FALSE

B == Mode \in {"b", "b_nomark", "b_nohandoff", "b_noexit"}
\* Some tokio SIGTERM listener exists: a live handler, or the r3 latch from gate entry on.
Registered == Providers = 2 \/ c = "started" \/ Mode = "r3"
GateSees == mark /\ Mode \in {"r3", "b", "b_nohandoff", "b_noexit"}
Live == done = "no"
\* C gives up its slot once; with `exits` it ends the process when that was the last slot.
Take(exits) == /\ cCounted' = TRUE
               /\ done' = IF exits /\ (Providers = 1 \/ oCounted) THEN "exit" ELSE done

\* One SIGTERM. A listener that exists hears it (and O's handler or the r3 latch sets the mark);
\* with none, the default action ends the process.
Term == /\ ~term /\ Live /\ term' = TRUE
        /\ IF Registered
           THEN /\ mark' = TRUE /\ cHeard' = (c = "started") /\ UNCHANGED done
           ELSE /\ done' = "killed" /\ UNCHANGED <<mark, cHeard>>
        /\ UNCHANGED <<busy, c, cCounted, oCounted, termAtGate>>
\* O's handler persists and takes its slot; the last slot exits (shutdown.rs:34-54).
OHandle == /\ Providers = 2 /\ term /\ ~oCounted /\ Live /\ oCounted' = TRUE
           /\ done' = IF cCounted THEN "exit" ELSE done
           /\ UNCHANGED <<term, busy, c, cHeard, cCounted, mark, termAtGate>>
ClearBusy == /\ busy /\ busy' = FALSE
             /\ UNCHANGED <<term, done, c, cHeard, cCounted, oCounted, mark, termAtGate>>

\* An attempt runs to completion, then the gate looks at the mark (native_input.rs:99-104).
CAttempt ==
    /\ c = "gate" /\ Live /\ UNCHANGED <<term, busy, cHeard, oCounted, mark>>
    /\ IF GateSees
       THEN c' = "stopped" /\ Take(Mode # "b_noexit") /\ UNCHANGED termAtGate
       ELSE \/ /\ busy /\ c' = "wait" /\ UNCHANGED <<cCounted, done, termAtGate>>
            \/ /\ ~busy /\ c' = "ready" /\ termAtGate' = term
               /\ UNCHANGED <<cCounted, done>>
            \/ /\ Refusable /\ c' = "refused" /\ UNCHANGED termAtGate
               /\ IF B THEN Take(Mode # "b_noexit" /\ mark) ELSE UNCHANGED <<cCounted, done>>
\* The 5 s retry sleep raced against the mark (native_input.rs:137-144).
CWaitEnd == /\ c = "wait" /\ Live /\ GateSees /\ c' = "stopped" /\ Take(Mode # "b_noexit")
            /\ UNCHANGED <<term, busy, cHeard, oCounted, mark, termAtGate>>
CRetry == /\ c = "wait" /\ Live /\ ~GateSees /\ c' = "gate"
          /\ UNCHANGED <<term, done, busy, cHeard, cCounted, oCounted, mark, termAtGate>>
\* Ready: run_bot_acquire_gateway_lease (preferred-poll gateway_lease.rs:229-319).
CReady == /\ c = "ready" /\ Live /\ c' = "lease"
          /\ UNCHANGED <<term, done, busy, cHeard, cCounted, oCounted, mark, termAtGate>>
\* Lease acquired or standby: the runtime and its handler start (runtime_bootstrap.rs:334/:410/:455).
CLeaseStart == /\ c = "lease" /\ Live /\ c' = "started"
               /\ UNCHANGED <<term, done, busy, cHeard, cCounted, oCounted, mark, termAtGate>>
\* Lease failed: run_bot gives up the slot and returns (runtime_bootstrap.rs:368-372).
CLeaseFail == /\ LeaseCanFail /\ c = "lease" /\ Live /\ c' = "failed" /\ Take(FALSE)
              /\ UNCHANGED <<term, busy, cHeard, oCounted, mark, termAtGate>>
CLeaseEnd == CLeaseStart \/ CLeaseFail
\* C's handler: its own stream, plus the mark when it takes over (shutdown.rs:94-104).
CHandle == /\ c = "started" /\ Live /\ ~cCounted
           /\ IF Mode = "r2" \/ Mode = "b_nohandoff" THEN cHeard ELSE mark
           /\ Take(TRUE)
           /\ UNCHANGED <<term, busy, c, cHeard, oCounted, mark, termAtGate>>
\* r3 only: the refused runtime waits on the latch for its slot.
CRefusedSlot == /\ Mode = "r3" /\ c = "refused" /\ mark /\ ~cCounted /\ Live /\ Take(TRUE)
                /\ UNCHANGED <<term, busy, c, cHeard, oCounted, mark, termAtGate>>

Next == Term \/ OHandle \/ ClearBusy \/ CAttempt \/ CWaitEnd \/ CRetry \/ CReady \/ CLeaseEnd
        \/ CHandle \/ CRefusedSlot

Spec == Init /\ [][Next]_vars
        /\ WF_vars(OHandle) /\ WF_vars(ClearBusy) /\ WF_vars(CAttempt) /\ WF_vars(CWaitEnd)
        /\ WF_vars(CRetry) /\ WF_vars(CReady) /\ WF_vars(CHandle) /\ WF_vars(CRefusedSlot)
        /\ IF LeaseEnds THEN WF_vars(CLeaseEnd) ELSE TRUE

\* ---- rules ----
\* A gate that decided after the shutdown request starts no runtime.
NoStartAfterTerm == ~(c \in {"ready", "lease", "started"} /\ termAtGate)
\* The process ends after SIGTERM: the last slot holder exits or the default action kills it.
BarrierEnds == term ~> (done # "no")
=============================================================================
