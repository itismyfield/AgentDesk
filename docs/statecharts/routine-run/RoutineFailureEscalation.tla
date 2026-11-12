---------------------- MODULE RoutineFailureEscalation ----------------------
EXTENDS Naturals, Sequences, FiniteSets
CONSTANTS N, Transport, AllowedBot, Policy

Min(a, b) == IF a < b THEN a ELSE b
Program == [i \in 1..(N+2) |-> "failed"] \o <<"interrupted", "succeeded">>
           \o [i \in 1..(N+1) |-> "failed"] \o <<"skipped", "failed", "paused">>
RECURSIVE FailedBefore(_)
FailedBefore(i) == IF i = 0 THEN 0
                   ELSE IF Program[i] = "interrupted" THEN FailedBefore(i-1)
                   ELSE IF Program[i] = "failed" THEN Min(FailedBefore(i-1)+1, N+2)
                   ELSE 0
RECURSIVE FirstFailedBefore(_)
FirstFailedBefore(i) == IF i = 0 THEN 0
                       ELSE IF Program[i] = "interrupted" THEN FirstFailedBefore(i-1)
                       ELSE IF Program[i] # "failed" THEN 0
                       ELSE IF FailedBefore(i-1) = 0 THEN i ELSE FirstFailedBefore(i-1)
Keys(seq) == {seq[i] : i \in 1..Len(seq)}

VARIABLE s
vars == <<s>>
Init == s = [threshold |-> N, cursor |-> 1, count |-> 0, firstFailed |-> 0,
             requests |-> <<>>, requestCounts |-> <<>>, notices |-> 0,
             lastNotice |-> 0, noticeInjected |-> FALSE, badNoticeTurn |-> FALSE,
             duplicateAdded |-> FALSE, worker |-> 1, phase |-> "idle",
             row |-> "none", outcome |-> "none", messages |-> 0,
             postAttempts |-> 0, maxMessages |-> 0, blindRepost |-> FALSE,
             intakeAccepted |-> FALSE, nativeKeys |-> <<>>, outcomes |-> <<>>]

RecordRun ==
    /\ s.cursor <= Len(Program)
    /\ LET status == Program[s.cursor]
           failed == status = "failed"
           neutral == status = "interrupted"
           reset == ~failed /\ ~neutral /\ ~(Policy = "no_success_reset" /\ status = "succeeded")
           count == IF failed THEN Min(s.count+1, N+2) ELSE IF reset THEN 0 ELSE s.count
           first == IF failed /\ s.count = 0 THEN s.cursor ELSE IF reset THEN 0 ELSE s.firstFailed
           boundary == IF Policy = "off_by_one" THEN N ELSE N+1
           trigger == failed /\ count = boundary /\ first \notin Keys(s.requests)
           notice == failed /\ ~trigger
       IN s' = [s EXCEPT !.cursor = @ + 1, !.count = count, !.firstFailed = first,
                         !.requests = IF trigger THEN Append(@, first) ELSE @,
                         !.requestCounts = IF trigger THEN Append(@, count) ELSE @,
                         !.notices = @ + IF notice THEN 1 ELSE 0,
                         !.lastNotice = IF notice THEN s.cursor ELSE 0,
                         !.noticeInjected = FALSE]

NotifyIntakeStarts ==
    /\ Policy = "ordinary_notify_triggers" /\ s.lastNotice > 0 /\ ~s.noticeInjected
    /\ s' = [s EXCEPT !.badNoticeTurn = TRUE, !.noticeInjected = TRUE]

DuplicateTrigger ==
    /\ Policy = "duplicate_trigger" /\ Len(s.requests) > 0 /\ ~s.duplicateAdded
    /\ s' = [s EXCEPT !.requests = Append(@, s.requests[1]),
                      !.requestCounts = Append(@, N+1), !.duplicateAdded = TRUE]

LoadOutbox ==
    /\ s.phase = "idle" /\ s.worker <= Len(s.requests)
    /\ s' = [s EXCEPT !.phase = "pending", !.row = "pending", !.outcome = "none",
                      !.messages = 0, !.postAttempts = 0, !.intakeAccepted = FALSE]

PostOutbox ==
    /\ s.phase = "pending" /\ s.row = "pending"
    /\ IF Transport = "unknown" /\ s.postAttempts = 0
       THEN \E accepted \in {0, 1}:
              s' = [s EXCEPT !.phase = "posted", !.row = "held",
                            !.outcome = "completion_unconfirmed", !.messages = accepted,
                            !.postAttempts = 1, !.maxMessages = IF accepted > s.maxMessages THEN accepted ELSE @]
       ELSE s' = [s EXCEPT !.phase = "posted", !.row = "sent", !.outcome = "Delivered",
                          !.messages = @ + 1, !.postAttempts = @ + 1,
                          !.maxMessages = IF s.messages+1 > @ THEN s.messages+1 ELSE @]

BlindUnknownRepost ==
    /\ Policy = "blind_unknown_repost" /\ s.phase = "posted"
    /\ s.outcome = "completion_unconfirmed" /\ s.postAttempts = 1
    /\ s' = [s EXCEPT !.phase = "pending", !.row = "pending", !.blindRepost = TRUE]

ExistingBotIntake ==
    /\ s.phase = "posted" /\ s.messages > 0 /\ AllowedBot
    /\ s' = [s EXCEPT !.phase = "accepted", !.intakeAccepted = TRUE]

NoAcceptedBotInput ==
    /\ s.phase = "posted" /\ (s.messages = 0 \/ ~AllowedBot)
    /\ s' = [s EXCEPT !.phase = "closed"]

ExistingOwnerStarts ==
    /\ s.phase = "accepted" /\ s.intakeAccepted
    /\ s' = [s EXCEPT !.phase = "closed", !.nativeKeys = Append(@, s.requests[s.worker])]

RecordAttemptOutcome ==
    /\ s.phase = "closed"
    /\ s' = [s EXCEPT !.worker = @ + 1, !.phase = "idle", !.outcomes = Append(@, s.outcome)]

Finished == s.cursor > Len(Program) /\ s.worker > Len(s.requests) /\ s.phase = "idle"
Done == Finished /\ UNCHANGED s
Delivery == LoadOutbox \/ PostOutbox \/ ExistingBotIntake \/ NoAcceptedBotInput
            \/ ExistingOwnerStarts \/ RecordAttemptOutcome
Next == RecordRun \/ NotifyIntakeStarts \/ DuplicateTrigger \/ Delivery \/ BlindUnknownRepost \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(RecordRun) /\ WF_vars(Delivery)

CounterMatchesHistory == s.count = FailedBefore(s.cursor-1)
TriggerOnlyAtNPlusOne == \A i \in 1..Len(s.requestCounts): s.requestCounts[i] = N+1
AtMostOneTriggerPerStreak == Len(s.requests) = Cardinality(Keys(s.requests))
OrdinaryNotifyDoesNotStart == ~s.badNoticeTurn
NoBlindUnknownRepost == ~s.blindRepost
NoDuplicateDiscordMessage == s.maxMessages <= 1
AtMostOneRemediationPerIncident == Len(s.nativeKeys) = Cardinality(Keys(s.nativeKeys))
OnlyExistingAcceptedBotIntake == Len(s.nativeKeys) > 0 => AllowedBot
AllEligibleStreaksOffered == s.cursor > Len(Program) => Len(s.requests) = 2
EveryAttemptTerminates == <>Finished
ConfirmedIncidentsReachExistingOwner == <>(Finished /\ Len(s.nativeKeys) = Len(s.requests))
=============================================================================
