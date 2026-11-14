----------------------------- MODULE TurnView -----------------------------
EXTENDS Naturals, FiniteSets
CONSTANT Entry, DeferActive, EarlyHandback, Fallback, QueueMembershipVisible
VARIABLES phase, eligible, markers, adds, queued, restarted
vars == <<phase, eligible, markers, adds, queued, restarted>>
Init == /\ phase = "new" /\ eligible \in BOOLEAN
        /\ markers = {} /\ adds = 0 /\ queued = FALSE /\ restarted = FALSE
\* Both entries use the same owner/provider/host/channel eligibility predicate.
Deferred == eligible /\ (Entry = "EmptyMailbox" \/ DeferActive)
Accept == /\ phase = "new" /\ phase' = "accepted" /\ queued' = TRUE
          /\ markers' = IF Deferred THEN {} ELSE {"mailbox"}
          /\ adds' = IF Deferred THEN 0 ELSE 1
          /\ UNCHANGED <<eligible, restarted>>
Restart == /\ phase = "accepted" /\ ~restarted /\ restarted' = TRUE
           /\ UNCHANGED <<phase, eligible, markers, adds, queued>>
Settle(result) ==
    /\ phase = "accepted" /\ phase' = "settled"
    /\ result \in {"plus", "mailbox", "question"}
    /\ markers' = {result}
    /\ adds' = IF result \in markers THEN adds ELSE adds + 1
    /\ queued' = (result = "mailbox")
    /\ UNCHANGED <<eligible, restarted>>
KeepQueued == /\ phase = "accepted" /\ ~eligible /\ phase' = "settled"
              /\ UNCHANGED <<eligible, markers, adds, queued, restarted>>
Finish == IF eligible THEN \E result \in {"plus", "mailbox", "question"} : Settle(result)
          ELSE KeepQueued
EarlyHold == /\ EarlyHandback /\ phase = "accepted" /\ eligible
             /\ phase' = "settled"
             /\ markers' = IF Fallback THEN {"mailbox"} ELSE markers
             /\ adds' = IF Fallback /\ "mailbox" \notin markers THEN adds + 1 ELSE adds
             /\ UNCHANGED <<eligible, queued, restarted>>
LegacySweep == /\ ~QueueMembershipVisible /\ phase = "accepted"
               /\ markers = {"mailbox"} /\ markers' = {}
               /\ UNCHANGED <<phase, eligible, adds, queued, restarted>>
Next == Accept \/ Restart \/ Finish \/ EarlyHold \/ LegacySweep
Spec == Init /\ [][Next]_vars /\ WF_vars(Accept) /\ WF_vars(Finish)
OneBusyResult == (eligible /\ phase = "settled") => adds = 1
NoInvisibleSettled == phase = "settled" => Cardinality(markers) = 1
ClosedGateKeepsPending == (~eligible /\ phase # "new") => markers = {"mailbox"}
AcceptedHasCustody == phase = "accepted" => queued
AcceptedEventuallyVisible == (phase = "accepted") ~> (phase = "settled" /\ markers # {})
=============================================================================
