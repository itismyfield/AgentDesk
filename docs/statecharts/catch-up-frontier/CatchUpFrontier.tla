-------------------------- MODULE CatchUpFrontier --------------------------
\* Source ids in one V2 snapshot in Discord message order.
EXTENDS Naturals
CONSTANTS N, Merge
VARIABLES st, cursor, reinjected
vars == <<st, cursor, reinjected>>
Inputs == 1..N

\* A cleared unsent record still reads as owed. A cleared unconfirmed record does not.
ClosedRetention == {"toldUnsent", "toldUnconfirmed", "clearedUnconfirmed"}
OwedRetention == {"unsent", "unconfirmed", "clearedUnsent"}
States == {"queued", "attempt", "settled", "lost"} \cup ClosedRetention \cup OwedRetention
Finished(i) == st[i] = "settled" \/ st[i] \in ClosedRetention

Arm(i) ==
    CASE st[i] = "lost" -> "Unknown"
      [] Merge = "main" -> "Queued"
      [] st[i] = "settled" -> "V2Delivered"
      [] st[i] \in ClosedRetention ->
          CASE Merge = "before" -> "Queued"
            [] Merge = "filteronly" -> "Unknown"
            [] OTHER -> "V2Delivered"
      [] Merge = "pendingpass" /\ st[i] = "queued" -> "V2Delivered"
      [] OTHER -> "Queued"

\* An unknown eligible original within the age window is recoverable.
Pass(i) == Arm(i) = "V2Delivered"
RECURSIVE Frontier(_)
Frontier(k) == IF k < N /\ Pass(k + 1) THEN Frontier(k + 1) ELSE k

Init == st = [i \in Inputs |-> "queued"] /\ cursor = 0 /\ reinjected = {}
Change(i, state) == st' = [st EXCEPT ![i] = state] /\ UNCHANGED <<cursor, reinjected>>
Dispatch(i) == st[i] = "queued" /\ Change(i, "attempt")
Deliver(i) == st[i] \in ({"attempt"} \cup OwedRetention \cup ClosedRetention)
              /\ Change(i, "settled")
Unconfirm(i) == st[i] = "attempt" /\ Change(i, "unconfirmed")
RetainUnsent(i) == st[i] \in {"queued", "attempt"} /\ Change(i, "unsent")
Requeue(i) == st[i] = "unsent" /\ Change(i, "queued")
Drop(i) == st[i] = "queued" /\ Change(i, "lost")
Tell(i) == \/ st[i] = "unconfirmed" /\ Change(i, "toldUnconfirmed")
           \/ st[i] = "unsent" /\ Change(i, "toldUnsent")
Clear(i) == \/ st[i] \in {"unconfirmed", "toldUnconfirmed"} /\ Change(i, "clearedUnconfirmed")
            \/ st[i] \in {"unsent", "toldUnsent"} /\ Change(i, "clearedUnsent")

Scan ==
    /\ cursor' = Frontier(cursor)
    /\ st' = [i \in Inputs |-> IF i > cursor /\ st[i] = "lost" THEN "queued" ELSE st[i]]
    /\ reinjected' = reinjected \cup {i \in Inputs : i > cursor /\ st[i] \in ClosedRetention
                                                        /\ Arm(i) = "Unknown"}
    /\ vars' # vars

Done == (\A i \in Inputs : Finished(i) \/ st[i] = "clearedUnsent") /\ UNCHANGED vars
Input == \E i \in Inputs : Dispatch(i) \/ Deliver(i) \/ Unconfirm(i) \/ RetainUnsent(i)
                           \/ Requeue(i) \/ Drop(i) \/ Tell(i) \/ Clear(i)
Next == Input \/ Scan \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Scan)

\* Clear can change a previously passed told-unsent record into a cleared one.
NoUndeliveredBehindCursor == \A i \in 1..cursor : Finished(i) \/ st[i] = "clearedUnsent"
ClosedRetentionDoesNotSeal == \A i \in Inputs : st[i] \in ClosedRetention => Pass(i)
NoClosedRetentionReinjection == reinjected = {}
OwedRetentionStaysOpen == \A i \in Inputs : st[i] \in OwedRetention => Arm(i) = "Queued"

DeliveredInputsAdvanceCursor ==
    \A k \in Inputs : ([] (\A i \in 1..k : Finished(i))) ~> (cursor >= k)
=============================================================================
