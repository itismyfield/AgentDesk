------------------------- MODULE CampaignDeployPermit -------------------------
EXTENDS Naturals
CONSTANTS CheckApproval, CheckStop, CheckHead, CheckJournal, CheckWitness, CheckAcceptance,
          ExpireWaiting, ExpireStarted
VARIABLES job
vars == <<job>>

\* head: the main commit the existing deployment path would release now.
\* target: the commit of the server's intent. approved: the commit the Deploy gate was approved for.
Init == job = [stage |-> "waiting", head |-> 1, target |-> 1, approved |-> 0,
               stopped |-> FALSE, journal |-> "none", starts |-> 0, startHead |-> 0,
               approvedAtStart |-> 0, startedAfterStop |-> FALSE,
               witness |-> "none", accepted |-> FALSE]

Approve ==
    /\ job.stage = "waiting" /\ job.journal = "none"
    /\ job' = [job EXCEPT !.approved = job.target]

HeadMoves ==
    /\ job.stage = "waiting" /\ job.head = 1
    /\ job' = [job EXCEPT !.head = 2]

\* A moved head replaces the unstarted intent; the old approval names another commit.
Retarget ==
    /\ job.stage = "waiting" /\ job.journal = "none" /\ job.target # job.head
    /\ job' = [job EXCEPT !.target = job.head]

Stop ==
    /\ ~job.stopped
    /\ job' = [job EXCEPT !.stopped = TRUE]

Cancel ==
    /\ CheckStop /\ job.stage = "waiting" /\ job.journal = "none" /\ job.stopped
    /\ job' = [job EXCEPT !.stage = "failed"]

\* The final gate journals the intent and starts the executor under the campaign row lock.
Begin ==
    /\ job.stage = "waiting" /\ job.starts < 2
    /\ job.journal = "none" \/ ~CheckJournal
    /\ ~CheckApproval \/ job.approved = job.target
    /\ ~CheckHead \/ job.target = job.head
    /\ ~CheckStop \/ ~job.stopped
    /\ job' = [job EXCEPT !.journal = "initiating", !.starts = job.starts + 1,
               !.startHead = job.head, !.startedAfterStop = job.stopped,
               !.approvedAtStart = IF job.approved = job.target THEN job.approved ELSE 0]

\* The worker, possibly a restarted one, replays the journal: a receipt or a lost reply.
Record(lost) ==
    /\ job.stage = "waiting" /\ job.journal = "initiating"
    /\ job' = [job EXCEPT !.journal = "recorded",
               !.stage = IF lost THEN "unknown" ELSE "started"]

Witness(seen) ==
    /\ job.stage \in {"started", "unknown"}
    /\ job' = [job EXCEPT !.witness = seen]

Confirm ==
    /\ job.stage \in {"started", "unknown"}
    /\ ~CheckWitness \/ job.witness = "expected"
    /\ job' = [job EXCEPT !.stage = "confirmed"]

Accept ==
    /\ job.stage = "confirmed"
    /\ job' = [job EXCEPT !.accepted = TRUE]

Complete ==
    /\ job.stage = "confirmed"
    /\ ~CheckAcceptance \/ job.accepted
    /\ job' = [job EXCEPT !.stage = "complete"]

Expire ==
    /\ \/ ExpireWaiting /\ job.stage = "waiting" /\ job.journal = "none"
       \/ ExpireStarted /\ job.stage \in {"started", "unknown"}
    /\ job' = [job EXCEPT !.stage = "failed"]

Next == Approve \/ HeadMoves \/ Retarget \/ Stop \/ Cancel \/ Begin \/ Confirm \/ Accept
        \/ Complete \/ Expire \/ UNCHANGED vars
        \/ (\E lost \in BOOLEAN : Record(lost))
        \/ (\E seen \in {"expected", "unknown"} : Witness(seen))

Spec == Init /\ [][Next]_vars /\ WF_vars(Expire) /\ WF_vars(\E lost \in BOOLEAN : Record(lost))
NoStartWithoutApproval == job.starts > 0 => job.approvedAtStart # 0
NoStartAfterStop == ~job.startedAfterStop
ExactApprovedHead == job.approvedAtStart # 0 => job.startHead = job.approvedAtStart
NoDuplicateStart == job.starts <= 1
OnlyWitnessedConfirm == job.stage \in {"confirmed", "complete"} => job.witness = "expected"
CompleteNeedsAcceptance == job.stage = "complete" => job.accepted
ApprovedSettles == (job.stage = "waiting" /\ job.approved = job.target) ~> (job.stage # "waiting")
StartedSettles == (job.stage \in {"started", "unknown"})
               ~> (job.stage \in {"confirmed", "complete", "failed"})
=============================================================================
