-------------------------- MODULE RoutineSettlement --------------------------
\* Refines RoutineRun.Submit and RecoverWithdraw at the owner actor boundary.
\* One stored routine intent and one owner original. Recovery covers all four
\* starting points, including a Herdr channel start still waiting unclaimed.
EXTENDS Naturals
CONSTANTS Scenario, Policy
VARIABLE s
vars == <<s>>

Starts == {"queued", "claimed", "approved", "herdr_idle"}
Recovery == Scenario = "recovery"
Fixed == Policy = "fixed"
Unsent == {"queued", "claimed", "herdr_idle"}
Init == \E start \in (IF Recovery THEN Starts ELSE {Scenario}):
    s = [start |-> start, owner |-> start, boundary |-> "open",
         row |-> "running", outcome |-> "none", keys |-> 0]

\* The run clock or an expired lease brings the same stored ticket to the actor.
Expire == /\ s.boundary = "open"
          /\ s' = [s EXCEPT !.boundary = IF Recovery THEN "recovering" ELSE "expired"]

\* The pre-key claim exists before the delivery deadline. The actor must
\* arbitrate the first approval against that deadline, even before a poll.
ApproveFirstKey == /\ s.boundary # "open" /\ s.owner = "claimed" /\ ~Fixed
                   /\ s' = [s EXCEPT !.owner = "approved"]

\* Once approved, the writer may still commit after the boundary and row close.
PayloadCommit == /\ s.boundary # "open" /\ s.owner = "approved"
                 /\ s' = [s EXCEPT !.owner = "possible", !.keys = 1]

\* An exact withdrawal commits before we can close the row. Failure to commit
\* or a lost ACK can stutter with the row running; the next tick tries again.
Withdraw == /\ Fixed /\ s.boundary # "open" /\ s.owner \in Unsent
            /\ s' = [s EXCEPT !.owner = "withdrawn"]

Close == /\ s.row = "running" /\ s.boundary # "open"
         /\ IF Fixed THEN s.owner \in {"withdrawn", "approved", "possible"}
                       ELSE Recovery
         /\ s' = [s EXCEPT !.row = IF Recovery THEN "interrupted" ELSE "failed",
                           !.outcome = CASE s.owner = "withdrawn" -> "withdrawn"
                                         [] Fixed -> "unconfirmed"
                                         [] OTHER -> "generic"]

Wait == UNCHANGED s
Next == Expire \/ ApproveFirstKey \/ PayloadCommit \/ Withdraw \/ Close \/ Wait
Spec == Init /\ [][Next]_vars /\ WF_vars(Expire) /\ WF_vars(Withdraw)
        /\ WF_vars(PayloadCommit) /\ WF_vars(Close)

WithdrawsBeforeKey == s.start \in Unsent => s.keys = 0
CloseAfterOwner == s.row # "running" =>
    IF s.start \in Unsent
    THEN s.owner = "withdrawn" /\ s.outcome = "withdrawn"
    ELSE s.owner \in {"approved", "possible"} /\ s.outcome = "unconfirmed"
ApprovedCanCommit == s.start = "approved" ~> (s.keys = 1)
EveryRunCloses == <>(s.row = IF Recovery THEN "interrupted" ELSE "failed")
=============================================================================
