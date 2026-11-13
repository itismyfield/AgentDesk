-------------------- MODULE NativeInputAttribution --------------------
EXTENDS Naturals, FiniteSets, Sequences
\* Pre-key intent/AnyKey/NotSent are refined in NativeInputPreparation; finalization stays B2.
CONSTANTS Mode, Scenario, Legacy
Ids == {1, 2}
VARIABLES pc, native, members, nativeQueue, stage, lease, rows, pending,
          result, count, proof, closed, ran, injected, wake, booted,
          generation, known, restarted, foreignBody, staleReleased, warned,
          incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review
vars == <<pc, native, members, nativeQueue, stage, lease, rows, pending,
          result, count, proof, closed, ran, injected, wake, booted,
          generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>

ReviewModes == {"FollowerBefore", "FollowerFixed", "BootBefore", "BootFixed",
                "BootFailureFixed", "BootIgnoreFailure"}
ReviewBoot == Mode \in {"BootBefore", "BootFixed", "BootFailureFixed", "BootIgnoreFailure"}

Init == /\ pc = 0 /\ native = "Idle" /\ members = {} /\ nativeQueue = <<>>
        /\ stage = [i \in Ids |-> "Pending"] /\ lease = 0 /\ rows = {}
        /\ pending = Ids /\ result = [i \in Ids |-> "None"]
        /\ count = [i \in Ids |-> 0] /\ proof = [i \in Ids |-> "None"]
        /\ closed = {} /\ ran = {} /\ injected = {} /\ wake = FALSE
        /\ booted = TRUE /\ generation = 1 /\ known = TRUE
        /\ restarted = FALSE /\ foreignBody = FALSE /\ staleReleased = FALSE
        /\ warned = {} /\ incarnation = 1 /\ inputInc = [i \in Ids |-> 0]
        /\ proofInc = [i \in Ids |-> 0] /\ replaced = FALSE /\ staleSeen = FALSE /\ outsideQueued = FALSE
        /\ review \in [phase : {IF ReviewBoot THEN "Prepare" ELSE IF Mode \in ReviewModes THEN "Registered" ELSE "Dormant"},
             outcome : {"Executed", "Withdrawn"}, commits : {0, 1}, result : {"None", "Executed"},
             row : {TRUE}, preserved : {FALSE}, prepared : {FALSE}, head : {FALSE},
             failed : {Mode \in {"BootFailureFixed", "BootIgnoreFailure"}}, notice : {0},
             parentLease : {1}, parentRow : {TRUE}, parentActive : {1}]
        /\ review.commits = IF review.result = "Executed" THEN 1 ELSE 0
        /\ ~ReviewBoot => review.result = "None"
        /\ Mode \notin ReviewModes => review.outcome = "Executed"

ExternalEnqueue == /\ booted /\ pc = 0 /\ ~outsideQueued
                   /\ outsideQueued' = TRUE /\ nativeQueue' = <<"External">>
                   /\ UNCHANGED <<pc, native, members, stage, lease, rows, pending,
                       result, count, proof, closed, ran, injected, wake, booted,
                       generation, known, restarted, foreignBody, staleReleased, warned,
                       incarnation, inputInc, proofInc, replaced, staleSeen, review>>
ExternalStart == /\ booted /\ pc = 0 /\ outsideQueued
                 /\ native' = "External" /\ pc' = 1 /\ nativeQueue' = <<>>
                 /\ UNCHANGED <<members, stage, lease, rows, pending,
                     result, count, proof, closed, ran, injected, wake, booted,
                     generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
Inject == /\ booted /\ pc = 1 /\ lease = 0
          /\ lease' = 1 /\ rows' = rows \cup {1} /\ injected' = injected \cup {1}
          /\ stage' = [stage EXCEPT ![1] = "Injected"] /\ pc' = 2
          /\ inputInc' = [inputInc EXCEPT ![1] = incarnation]
    /\ UNCHANGED <<native, members, nativeQueue, pending, result, count, proof,
              closed, ran, wake, booted, generation, known, restarted, foreignBody,
              staleReleased, warned, incarnation, proofInc, replaced, staleSeen, outsideQueued, review>>
Enqueue == /\ booted /\ pc = 2
           /\ nativeQueue' = <<1>> /\ stage' = [stage EXCEPT ![1] = "Waiting"]
           /\ pc' = 3
           /\ UNCHANGED <<native, members, lease, rows, pending, result, count,
               proof, closed, ran, injected, wake, booted, generation, known,
               restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
MidTurnJoin == /\ booted /\ Scenario = "MidTurn" /\ pc = 3
               /\ nativeQueue' = <<>> /\ members' = {1} /\ ran' = ran \cup {1}
               /\ stage' = [stage EXCEPT ![1] = "Running"] /\ pc' = 4
               /\ UNCHANGED <<native, lease, rows, pending, result, count, proof,
                   closed, injected, wake, booted, generation, known, restarted,
                   foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
Withdraw == /\ booted /\ Scenario = "Withdraw" /\ pc = 3
            /\ nativeQueue' = <<>> /\ pc' = 4
            /\ stage' = [stage EXCEPT ![1] = "Withdrawn"]
            /\ proof' = [proof EXCEPT ![1] = IF Mode \in {"Current", "NoWithdraw", "DropWithdraw"}
                                                   THEN "None" ELSE "Withdrawn"]
            /\ pending' = IF Mode = "DropWithdraw" THEN pending \ {1} ELSE pending
            /\ rows' = IF Mode = "DropWithdraw" THEN rows \ {1} ELSE rows
            /\ UNCHANGED <<native, members, lease, result, count, closed, ran,
                injected, wake, booted, generation, known, restarted, foreignBody,
                staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
ExternalAssistantEnd ==
    /\ booted /\ ((Scenario = "Queue" /\ pc = 3) \/ (Scenario # "Queue" /\ pc = 4))
    /\ pc' = 5
    /\ foreignBody' = (foreignBody \/ (Mode = "Current" /\ Scenario = "Queue"))
    /\ UNCHANGED <<native, members, nativeQueue, stage, lease, rows, pending,
        result, count, proof, closed, ran, injected, wake, booted, generation,
        known, restarted, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
ExternalDuration ==
    /\ booted /\ pc = 5 /\ pc' = 6 /\ native' = "Idle"
    /\ closed' = closed \cup members /\ members' = {}
    /\ proof' = IF Mode = "ForeignEnd" THEN [proof EXCEPT ![1] = "Complete"]
                ELSE IF 1 \in members /\ Mode # "Current"
                     THEN [proof EXCEPT ![1] = "Complete"] ELSE proof
    /\ proofInc' = IF Mode = "ForeignEnd" \/ (1 \in members /\ Mode # "Current")
                         THEN [proofInc EXCEPT ![1] = inputInc[1]] ELSE proofInc
    /\ UNCHANGED <<nativeQueue, stage, lease, rows, pending, result, count, ran,
        injected, wake, booted, generation, known, restarted, foreignBody,
        staleReleased, warned, incarnation, inputInc, replaced, staleSeen, outsideQueued, review>>
Dequeue == /\ booted /\ Scenario = "Queue" /\ pc = 6
           /\ nativeQueue' = <<>> /\ pc' = 7
           /\ stage' = [stage EXCEPT ![1] = "Dequeued"]
           /\ UNCHANGED <<native, members, lease, rows, pending, result, count,
               proof, closed, ran, injected, wake, booted, generation, known,
               restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
OwnStart == /\ booted /\ Scenario = "Queue" /\ pc = 7
            /\ native' = "Own" /\ members' = {1} /\ ran' = ran \cup {1}
            /\ stage' = [stage EXCEPT ![1] = "Running"] /\ pc' = 8
            /\ UNCHANGED <<nativeQueue, lease, rows, pending, result, count, proof,
                closed, injected, wake, booted, generation, known, restarted,
                foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
OwnAssistantEnd == /\ booted /\ Scenario = "Queue" /\ pc = 8 /\ pc' = 9
                   /\ UNCHANGED <<native, members, nativeQueue, stage, lease, rows,
                       pending, result, count, proof, closed, ran, injected, wake,
                       booted, generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
OwnDuration == /\ booted /\ Scenario = "Queue" /\ pc = 9 /\ pc' = 10
               /\ native' = "Idle" /\ closed' = closed \cup members /\ members' = {}
               /\ proof' = IF Mode = "Current" THEN proof
                           ELSE [proof EXCEPT ![1] = "Complete"]
               /\ proofInc' = IF Mode = "Current" THEN proofInc ELSE [proofInc EXCEPT ![1] = inputInc[1]]
    /\ UNCHANGED <<nativeQueue, stage, lease, rows, pending, result,
                   count, ran, injected, wake, booted, generation, known, restarted,
                   foreignBody, staleReleased, warned, incarnation, inputInc, replaced, staleSeen, outsideQueued, review>>

\* Outcome commit is durable before releasing the mailbox; replay is idempotent.
ObserveProof(i) == /\ booted /\ known /\ proof[i] # "None" /\ result[i] = "None"
                   /\ result' = [result EXCEPT ![i] = proof[i]]
                   /\ count' = [count EXCEPT ![i] = @ + 1]
                   /\ pending' = pending \ {i}
                   /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease,
                       rows, proof, closed, ran, injected, wake, booted, generation,
                       known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
Settle(i) == /\ booted /\ result[i] # "None" /\ lease = i
             /\ lease' = 0
             /\ UNCHANGED <<pc, native, members, nativeQueue, stage, rows, pending,
                 result, count, proof, closed, ran, injected, wake, booted, generation,
                 known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
ProjectionClear(i) == /\ booted /\ result[i] # "None" /\ lease # i /\ i \in rows
                      /\ rows' = rows \ {i}
                      /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease,
                          pending, result, count, proof, closed, ran, injected, wake,
                          booted, generation, known, restarted, foreignBody,
                          staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
Kick == /\ booted /\ Mode # "NoWake" /\ lease = 0 /\ rows = {}
        /\ result[1] # "None" /\ ~wake /\ 2 \notin injected
        /\ wake' = TRUE
        /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows, pending,
            result, count, proof, closed, ran, injected, booted, generation, known,
            restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
InjectHead == /\ booted /\ wake /\ lease = 0 /\ rows = {} /\ native = "Idle"
              /\ nativeQueue = <<>> /\ (Scenario # "Queue" \/ pc = 10) /\ 2 \notin injected
              /\ lease' = 2 /\ rows' = {2} /\ injected' = injected \cup {2}
              /\ native' = "Own" /\ members' = {2} /\ ran' = ran \cup {2}
              /\ stage' = [stage EXCEPT ![2] = "Running"] /\ wake' = FALSE
              /\ inputInc' = [inputInc EXCEPT ![2] = incarnation]
    /\ UNCHANGED <<pc, nativeQueue, pending, result, count, proof, closed,
                  booted, generation, known, restarted, foreignBody, staleReleased, warned, incarnation, proofInc, replaced, staleSeen, outsideQueued, review>>
HeadAssistantEnd == /\ booted /\ 2 \in members /\ stage[2] = "Running"
                    /\ stage' = [stage EXCEPT ![2] = "AssistantEnd"]
                    /\ UNCHANGED <<pc, native, members, nativeQueue, lease, rows,
                        pending, result, count, proof, closed, ran, injected, wake,
                        booted, generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
HeadDuration == /\ booted /\ 2 \in members /\ stage[2] = "AssistantEnd"
                /\ native' = "Idle" /\ members' = {} /\ closed' = closed \cup {2}
                /\ proof' = [proof EXCEPT ![2] = "Complete"]
                /\ proofInc' = [proofInc EXCEPT ![2] = inputInc[2]]
    /\ UNCHANGED <<pc, nativeQueue, stage, lease, rows, pending, result,
                    count, ran, injected, wake, booted, generation, known, restarted,
                    foreignBody, staleReleased, warned, incarnation, inputInc, replaced, staleSeen, outsideQueued, review>>

\* Existing operator/adjacent actions record a non-success result, never provider completion.
OperatorRelease == /\ booted /\ lease \in Ids /\ result[lease] = "None"
                   /\ result' = [result EXCEPT ![lease] = "OperatorReleased"]
                   /\ count' = [count EXCEPT ![lease] = @ + 1]
                   /\ pending' = pending \ {lease}
                   /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease,
                       rows, proof, closed, ran, injected, wake, booted, generation,
                       known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
AdjacentEnd == /\ booted /\ lease \in Ids /\ result[lease] = "None"
               /\ result' = [result EXCEPT ![lease] = "Cancelled"]
               /\ count' = [count EXCEPT ![lease] = @ + 1]
               /\ pending' = pending \ {lease}
               /\ native' = "Idle" /\ nativeQueue' = <<>> /\ members' = {}
               /\ pc' = IF lease = 1 THEN (IF Scenario = "Queue" THEN 10 ELSE 6) ELSE pc
               /\ UNCHANGED <<stage, lease, rows, proof, closed, ran, injected, wake,
                   booted, generation, known, restarted, foreignBody, staleReleased, warned,
                   incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
Restart == /\ booted /\ ~restarted /\ restarted' = TRUE /\ booted' = FALSE
           /\ generation' = 2 /\ lease' = 0 /\ wake' = FALSE
           /\ known' = ~Legacy
           /\ UNCHANGED <<pc, native, members, nativeQueue, stage, rows, pending,
               result, count, proof, closed, ran, injected, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
BootRestore == /\ ~booted /\ booted' = TRUE
               /\ lease' = IF rows = {} THEN 0 ELSE CHOOSE i \in rows : TRUE
               /\ UNCHANGED <<pc, native, members, nativeQueue, stage, rows, pending,
                   result, count, proof, closed, ran, injected, wake, generation,
                   known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
\* Legacy reconstruction replays the authenticated source; this is a port assumption.
Reconstruct == /\ booted /\ ~known /\ Mode # "Unavailable" /\ known' = TRUE
               /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows,
                   pending, result, count, proof, closed, ran, injected, wake, booted,
                   generation, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
WarnIdle == /\ booted /\ native = "Idle" /\ lease \in Ids /\ lease \notin warned
            /\ warned' = warned \cup {lease}
            /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows,
                pending, result, count, proof, closed, ran, injected, wake, booted,
                generation, known, restarted, foreignBody, staleReleased, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>
\* Late proof from the old input/execution must not borrow the successor's lease.
StaleEvidence == /\ booted /\ lease = 2 /\ result[1] # "None" /\ ~staleSeen
                 /\ staleSeen' = TRUE
                 /\ IF Mode = "NoFence" /\ result[2] = "None"
                     THEN /\ result' = [result EXCEPT ![2] = "Complete"]
                          /\ proofInc' = [proofInc EXCEPT ![2] = proofInc[1]]
                          /\ count' = [count EXCEPT ![2] = @ + 1]
                          /\ pending' = pending \ {2} /\ staleReleased' = TRUE
                     ELSE UNCHANGED <<result, proofInc, count, pending, staleReleased, outsideQueued, review>>
                 /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows,
                     proof, closed, ran, injected, wake, booted, generation, known,
                     restarted, foreignBody, warned, incarnation, inputInc, replaced, outsideQueued, review>>
ExecutionReplaced == /\ booted /\ ~replaced /\ lease = 0 /\ rows = {} /\ native = "Idle"
                     /\ result[1] # "None" /\ 2 \notin injected
                     /\ nativeQueue = <<>> /\ (Scenario # "Queue" \/ pc = 10)
                     /\ incarnation' = 2 /\ replaced' = TRUE
                     /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows,
                         pending, result, count, proof, closed, ran, injected, wake,
                         booted, generation, known, restarted, foreignBody, staleReleased,
                         warned, inputInc, proofInc, staleSeen, outsideQueued, review>>
DuplicateSettlement == /\ booted /\ Mode = "DoubleSettle"
                       /\ \E i \in Ids : /\ result[i] # "None" /\ count[i] = 1
                            /\ count' = [count EXCEPT ![i] = 2]
                       /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease,
                           rows, pending, result, proof, closed, ran, injected, wake,
                           booted, generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued, review>>

ReviewFollowerCommit == /\ Mode = "FollowerFixed" /\ review.phase = "Registered"
                        /\ review' = [review EXCEPT !.phase = "Done", !.result = review.outcome,
                             !.commits = 1, !.notice = IF review.outcome = "Withdrawn" THEN 1 ELSE 0]
                        /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows, pending, result, count, proof, closed, ran, injected, wake, booted, generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued>>
ReviewPrepare == /\ ReviewBoot /\ review.phase = "Prepare" /\ ~review.prepared
                 /\ review' = [review EXCEPT !.prepared = TRUE, !.preserved = ~review.failed]
                 /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows, pending, result, count, proof, closed, ran, injected, wake, booted, generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued>>
ReviewReap == /\ ReviewBoot /\ review.phase = "Prepare"
              /\ (Mode \in {"BootBefore", "BootIgnoreFailure"} \/ (review.prepared /\ review.preserved))
              /\ review' = [review EXCEPT !.phase = "Reaped", !.row = FALSE]
              /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows, pending, result, count, proof, closed, ran, injected, wake, booted, generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued>>
ReviewRestore == /\ ReviewBoot /\ review.phase = "Reaped" /\ review.preserved
                 /\ review' = [review EXCEPT !.phase = "Done", !.head = TRUE,
                      !.result = "Executed", !.commits = 1]
                 /\ UNCHANGED <<pc, native, members, nativeQueue, stage, lease, rows, pending, result, count, proof, closed, ran, injected, wake, booted, generation, known, restarted, foreignBody, staleReleased, warned, incarnation, inputInc, proofInc, replaced, staleSeen, outsideQueued>>
Next == ReviewFollowerCommit \/ ReviewPrepare \/ ReviewReap \/ ReviewRestore \/ ExternalEnqueue \/ ExternalStart \/ Inject \/ Enqueue \/ MidTurnJoin \/ Withdraw
        \/ ExternalAssistantEnd \/ ExternalDuration \/ Dequeue \/ OwnStart
        \/ OwnAssistantEnd \/ OwnDuration \/ (\E i \in Ids : ObserveProof(i) \/ Settle(i) \/ ProjectionClear(i))
        \/ Kick \/ InjectHead \/ HeadAssistantEnd \/ HeadDuration \/ OperatorRelease
        \/ AdjacentEnd \/ Restart \/ BootRestore \/ Reconstruct \/ WarnIdle
        \/ StaleEvidence \/ ExecutionReplaced \/ DuplicateSettlement
Spec == Init /\ [][Next]_vars
        /\ WF_vars(ReviewFollowerCommit) /\ WF_vars(ReviewPrepare) /\ WF_vars(ReviewReap) /\ WF_vars(ReviewRestore)
        /\ WF_vars(ExternalEnqueue) /\ WF_vars(ExternalStart) /\ WF_vars(Inject) /\ WF_vars(Enqueue)
        /\ WF_vars(MidTurnJoin) /\ WF_vars(Withdraw) /\ WF_vars(ExternalAssistantEnd)
        /\ WF_vars(ExternalDuration) /\ WF_vars(Dequeue) /\ WF_vars(OwnStart)
        /\ WF_vars(OwnAssistantEnd) /\ WF_vars(OwnDuration)
        /\ (\A i \in Ids : WF_vars(ObserveProof(i)) /\ WF_vars(Settle(i)) /\ WF_vars(ProjectionClear(i)))
        /\ WF_vars(Kick) /\ WF_vars(InjectHead) /\ WF_vars(HeadAssistantEnd)
        /\ WF_vars(HeadDuration) /\ WF_vars(BootRestore) /\ WF_vars(Reconstruct)

CompletionExactlyOnce == \A i \in Ids : count[i] = IF result[i] = "None" THEN 0 ELSE 1
NoForeignEnd == \A i \in Ids : result[i] = "Complete" => i \in closed /\ i \in ran
NoSilentLoss == \A i \in Ids : i \in pending \/ result[i] # "None"
NoForeignResponse == ~foreignBody
NoSuccessorRelease == ~staleReleased
NoSourceBorrow == \A i \in Ids : result[i] = "Complete" => proofInc[i] = inputInc[i] /\ inputInc[i] > 0
IdleEventuallyReleased == (booted /\ native = "Idle") ~> (booted /\ lease = 0 /\ rows = {})
HeadEventuallyInjected == <>(2 \in injected)
EveryInputEventuallySettled == \A i \in Ids : <>(result[i] # "None")
FollowerKeepsParent == review.parentLease = 1 /\ review.parentRow /\ review.parentActive = 1
ReviewResultOnce == review.commits <= 1
NoBootTicketLoss == ~ReviewBoot \/ review.row \/ review.preserved
FailureKeepsRow == ~review.failed \/ review.row
FollowerEventuallySettles == (Mode \in {"FollowerBefore", "FollowerFixed"}) => <>(review.phase = "Done")
BootEventuallyRestores == (Mode \in {"BootBefore", "BootFixed"}) => <>review.head
=============================================================================
