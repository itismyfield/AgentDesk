-------------------------- MODULE CatchUpFrontier --------------------------
\* One V2-queue channel's inputs in message order and the durable catch-up cursor.
\* Merge picks how the scan reads the V2 file: "main" holds every V2 id as queued,
\* "fixed" lets a delivered (terminal) id pass, "pendingpass" also passes queued ids.
EXTENDS Naturals
CONSTANTS N, Merge
VARIABLES st, cursor
vars == <<st, cursor>>
Inputs == 1..N
\* queued: V2 items; attempt: V2 head injection (dispatched, not settled);
\* retained: V2 retained input (owed); settled: V2 terminal identity;
\* lost: dropped before dispatch, so no source holds it and the scan must recover it.
States == {"queued", "attempt", "retained", "settled", "lost"}

\* May the frontier pass this id? Retained and attempted ids stay V2-held as queued in every mode.
Pass(i) == CASE Merge = "main" -> FALSE
           [] Merge = "fixed" -> st[i] = "settled"
           [] Merge = "pendingpass" -> st[i] \in {"settled", "queued"}

\* Longest run of passing ids right after the cursor; the scan stops at the first Open id.
RECURSIVE Frontier(_)
Frontier(k) == IF k < N /\ Pass(k + 1) THEN Frontier(k + 1) ELSE k

Init == st = [i \in Inputs |-> "queued"] /\ cursor = 0

Dispatch(i) == st[i] = "queued" /\ st' = [st EXCEPT ![i] = "attempt"] /\ UNCHANGED cursor
Deliver(i) == st[i] \in {"attempt", "retained"} /\ st' = [st EXCEPT ![i] = "settled"] /\ UNCHANGED cursor
Unconfirm(i) == st[i] = "attempt" /\ st' = [st EXCEPT ![i] = "retained"] /\ UNCHANGED cursor
Requeue(i) == st[i] = "retained" /\ st' = [st EXCEPT ![i] = "queued"] /\ UNCHANGED cursor
Drop(i) == st[i] = "queued" /\ st' = [st EXCEPT ![i] = "lost"] /\ UNCHANGED cursor

\* Phase 1 scan: a lost id after the cursor is recovered (queued again), and the cursor moves
\* to the end of the contiguous passing run.
Scan ==
    /\ cursor' = Frontier(cursor)
    /\ st' = [i \in Inputs |-> IF i > cursor /\ st[i] = "lost" THEN "queued" ELSE st[i]]
    /\ vars' # vars

Done == (\A i \in Inputs : st[i] = "settled") /\ UNCHANGED vars

Input == \E i \in Inputs : Dispatch(i) \/ Deliver(i) \/ Unconfirm(i) \/ Requeue(i) \/ Drop(i)
Next == Input \/ Scan \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Scan)

\* The cursor never sits past an input that is not delivered.
NoUndeliveredBehindCursor == \A i \in 1..cursor : st[i] = "settled"
\* Once every input is delivered the cursor reaches the newest one.
DeliveredInputsAdvanceCursor == (\A i \in Inputs : st[i] = "settled") ~> (cursor = N)
=============================================================================
