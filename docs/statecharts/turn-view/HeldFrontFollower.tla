------------------------- MODULE HeldFrontFollower -------------------------
EXTENDS Naturals, FiniteSets
CONSTANT FirstOnly
VARIABLES phase, FrontHeld, markers, queued, retried
vars == <<phase, FrontHeld, markers, queued, retried>>
Init == /\ phase = "new" /\ FrontHeld = FALSE
        /\ markers = [i \in {"A", "B"} |-> {}]
        /\ queued = {} /\ retried = FALSE
EnqueueA == /\ phase = "new" /\ phase' = "Aaccepted"
            /\ queued' = {"A"} /\ UNCHANGED <<FrontHeld, markers, retried>>
HoldA == /\ phase = "Aaccepted" /\ phase' = "Aheld" /\ FrontHeld' = TRUE
         /\ markers' = [markers EXCEPT !["A"] = {"mailbox"}]
         /\ UNCHANGED <<queued, retried>>
EnqueueB == /\ phase = "Aheld" /\ phase' = "Bwaiting"
            /\ queued' = {"A", "B"}
            /\ markers' = [markers EXCEPT !["B"] = IF FirstOnly THEN {"mailbox"} ELSE {}]
            /\ UNCHANGED <<FrontHeld, retried>>
RetryA == /\ phase = "Bwaiting" /\ FrontHeld /\ retried' = ~retried
          /\ UNCHANGED <<phase, FrontHeld, markers, queued>>
Next == EnqueueA \/ HoldA \/ EnqueueB \/ RetryA
Spec == Init /\ [][Next]_vars /\ WF_vars(EnqueueA) /\ WF_vars(HoldA)
        /\ WF_vars(EnqueueB) /\ WF_vars(RetryA)
HeldFollowerVisible == phase = "Bwaiting" => markers["B"] = {"mailbox"}
OneInputMarker == \A i \in {"A", "B"} : Cardinality(markers[i]) <= 1
AcceptedCustody == phase = "Bwaiting" => queued = {"A", "B"}
FollowerEventuallyVisible == (phase = "Bwaiting") ~> (markers["B"] = {"mailbox"})
=============================================================================
