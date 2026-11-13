----------------------- MODULE ReservedIdEnqueue -----------------------
EXTENDS Naturals
CONSTANTS GuardReserved, CallerReturns, RetryAfterRelease, OwnerReturn
VARIABLES queue, reserved, marker, executions, pc, refused
vars == <<queue, reserved, marker, executions, pc, refused>>

\* One admitted id; queue counts its copies. No crash, cancellation or Held path.
Init == /\ queue = 1 /\ reserved = FALSE /\ marker = FALSE
        /\ executions = 0 /\ pc = "take" /\ refused = FALSE
Take == /\ pc = "take" /\ queue = 1
        /\ queue' = 0 /\ reserved' = TRUE /\ marker' = TRUE
        /\ pc' = "replay" /\ UNCHANGED <<executions, refused>>
Replay == /\ pc = "replay"
          /\ refused' = (GuardReserved /\ ~OwnerReturn) /\ pc' = "release"
          /\ queue' = IF OwnerReturn \/ ~GuardReserved THEN queue + 1 ELSE queue
          /\ reserved' = IF OwnerReturn THEN FALSE ELSE reserved
          /\ UNCHANGED <<marker, executions>>
\* A successful claim executes; the current race-loss caller instead abandons
\* its reservation and durable marker even after a non-persistence refusal.
Release == /\ pc = "release"
           /\ reserved' = FALSE /\ marker' = IF OwnerReturn THEN marker ELSE FALSE
           /\ pc' = "after"
           /\ executions' = IF CallerReturns THEN executions ELSE executions + 1
           /\ queue' = IF CallerReturns THEN queue ELSE 0
           /\ UNCHANGED refused
\* Separate control: a fresh receipt after release can enqueue the same id.
AfterRelease == /\ pc = "after" /\ pc' = "drain"
                /\ queue' = IF RetryAfterRelease /\ executions = 0 /\ queue = 0
                            THEN 1 ELSE queue
                /\ UNCHANGED <<reserved, marker, executions, refused>>
Execute == /\ pc = "drain" /\ queue > 0
           /\ queue' = queue - 1 /\ executions' = executions + 1 /\ marker' = FALSE
           /\ UNCHANGED <<reserved, pc, refused>>
Finish == /\ pc = "drain" /\ queue = 0 /\ pc' = "done"
          /\ UNCHANGED <<queue, reserved, marker, executions, refused>>
Next == Take \/ Replay \/ Release \/ AfterRelease \/ Execute \/ Finish
        \/ (pc = "done" /\ UNCHANGED vars)
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Take) /\ WF_vars(Replay) /\ WF_vars(Release)
        /\ WF_vars(AfterRelease) /\ WF_vars(Execute) /\ WF_vars(Finish)
NoReservationQueueOverlap == ~(reserved /\ queue > 0)
AtMostOneExecution == executions <= 1
NoLostReturn == pc = "done" => executions = 1
EventuallyExecuted == <>(pc = "done" /\ executions = 1)
======================================================================
