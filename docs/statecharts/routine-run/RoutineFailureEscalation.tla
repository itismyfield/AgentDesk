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
Init == s = [threshold |-> N, allowedBot |-> AllowedBot, cursor |-> 1, count |-> 0, firstFailed |-> 0,
             requests |-> <<>>, requestCounts |-> <<>>, notices |-> 0,
             lastNotice |-> 0, noticeInjected |-> FALSE, badNoticeTurn |-> FALSE,
             duplicateAdded |-> FALSE, relogged |-> FALSE, worker |-> 1, phase |-> "idle",
             row |-> "none", outcome |-> "none", messages |-> 0, wire |-> "none", retried |-> FALSE,
             postAttempts |-> 0, maxMessages |-> 0, blindRepost |-> FALSE, reopened |-> FALSE,
             intakeAccepted |-> FALSE, nativeKeys |-> <<>>, outcomes |-> <<>>]

UnseenIncident(first) == Policy = "drop_event_key" \/ first \notin Keys(s.requests)

RecordRun ==
    /\ s.cursor <= Len(Program)
    /\ LET status == Program[s.cursor]
           failed == status = "failed"
           neutral == status = "interrupted"
           reset == ~failed /\ ~neutral /\ ~(Policy = "no_success_reset" /\ status = "succeeded")
           count == IF failed THEN Min(s.count+1, N+2) ELSE IF reset THEN 0 ELSE s.count
           first == IF failed /\ s.count = 0 THEN s.cursor ELSE IF reset THEN 0 ELSE s.firstFailed
           boundary == IF Policy = "off_by_one" THEN N ELSE N+1
           trigger == failed /\ count = boundary /\ UnseenIncident(first)
           notice == failed /\ ~trigger
       IN s' = [s EXCEPT !.cursor = @ + 1, !.count = count, !.firstFailed = first,
                         !.requests = IF trigger THEN Append(@, first) ELSE @,
                         !.requestCounts = IF trigger THEN Append(@, count) ELSE @,
                         !.notices = @ + IF notice THEN 1 ELSE 0,
                         !.lastNotice = IF notice THEN s.cursor ELSE 0,
                         !.noticeInjected = FALSE, !.relogged = FALSE]

RelogRun ==
    /\ s.cursor > 1 /\ Program[s.cursor-1] = "failed" /\ ~s.relogged
    /\ LET boundary == IF Policy = "off_by_one" THEN N ELSE N+1
           trigger == s.count = boundary /\ UnseenIncident(s.firstFailed)
       IN s' = [s EXCEPT !.relogged = TRUE,
                         !.requests = IF trigger THEN Append(@, s.firstFailed) ELSE @,
                         !.requestCounts = IF trigger THEN Append(@, s.count) ELSE @]

NotifyIntakeStarts ==
    /\ Policy = "ordinary_notify_triggers" /\ s.lastNotice > 0 /\ ~s.noticeInjected
    /\ s' = [s EXCEPT !.badNoticeTurn = TRUE, !.noticeInjected = TRUE]

DuplicateTrigger ==
    /\ Policy = "duplicate_trigger" /\ Len(s.requests) > 0 /\ ~s.duplicateAdded
    /\ s' = [s EXCEPT !.requests = Append(@, s.requests[1]),
                      !.requestCounts = Append(@, N+1), !.duplicateAdded = TRUE]

LoadOutbox ==
    /\ s.phase = "idle" /\ s.worker <= Len(s.requests)
    /\ s' = [s EXCEPT !.phase = "pending", !.row = "pending", !.outcome = "none", !.wire = "none",
                      !.messages = 0, !.postAttempts = 0, !.retried = FALSE, !.reopened = FALSE,
                      !.intakeAccepted = FALSE]

\* The same row commits posting before the Discord request; its nonce is the row id.
BeginPosting ==
    /\ s.phase = "pending" /\ s.row = "pending"
    /\ s' = [s EXCEPT !.phase = "posting", !.row = "posting"]

WireResults == IF Transport = "unknown"
               THEN {"ack", "may_have_sent", "ack_lost", "crash"} \cup (IF s.retried THEN {} ELSE {"not_sent"})
               ELSE {"ack"}

DiscordAttempt ==
    /\ s.phase = "posting"
    /\ \E r \in WireResults, maybe \in {0, 1}:
         LET accepted == IF r \in {"ack", "ack_lost"} THEN 1 ELSE IF r = "not_sent" THEN 0 ELSE maybe
         IN s' = [s EXCEPT !.phase = "wire", !.wire = r, !.messages = @ + accepted,
                           !.postAttempts = @ + 1,
                           !.maxMessages = IF s.messages + accepted > @ THEN s.messages + accepted ELSE @]

Uncertain == [s EXCEPT !.phase = "posted", !.row = IF Policy = "unknown_to_failed" THEN "failed" ELSE "unknown",
                       !.outcome = "completion_unconfirmed"]

SettleAttempt ==
    /\ s.phase = "wire"
    /\ CASE s.wire = "ack" /\ Policy = "drop_ack_retry" /\ s.postAttempts = 1 ->
              s' = [s EXCEPT !.phase = "pending", !.row = "pending", !.wire = "none", !.reopened = TRUE]
          [] s.wire = "ack" ->
              s' = [s EXCEPT !.phase = "posted", !.row = "sent",
                             !.outcome = IF Policy = "ack_turn_linked" THEN "turn_linked" ELSE "posted_turn_unconfirmed"]
          [] s.wire = "not_sent" ->
              s' = [s EXCEPT !.phase = "pending", !.row = "pending", !.wire = "none", !.retried = TRUE]
          [] s.wire = "crash" -> s' = [s EXCEPT !.phase = "abandoned"]
          [] OTHER -> s' = Uncertain

\* Restart after the post: the posting row is swept to unknown, never reposted.
SweepAbandoned ==
    /\ s.phase = "abandoned" /\ s.row = "posting"
    /\ s' = Uncertain

BlindUnknownRepost ==
    /\ Policy = "blind_unknown_repost" /\ s.phase = "posted"
    /\ s.outcome = "completion_unconfirmed" /\ s.postAttempts = 1
    /\ s' = [s EXCEPT !.phase = "pending", !.row = "pending", !.wire = "none", !.blindRepost = TRUE]

HeldActivation ==
    /\ Policy = "held_activation" /\ s.phase = "posted" /\ s.row = "unknown" /\ ~s.reopened
    /\ s' = [s EXCEPT !.phase = "pending", !.row = "pending", !.wire = "none", !.reopened = TRUE]

ExistingBotIntake ==
    /\ s.phase = "posted" /\ s.messages > 0 /\ (AllowedBot \/ Policy = "bypass_intake_gate")
    /\ s' = [s EXCEPT !.phase = "accepted", !.intakeAccepted = TRUE]

NoAcceptedBotInput ==
    /\ s.phase = "posted" /\ (s.messages = 0 \/ ~AllowedBot)
    /\ s' = [s EXCEPT !.phase = "closed"]

ExistingOwnerStarts ==
    /\ s.phase = "accepted" /\ s.intakeAccepted
    /\ s' = [s EXCEPT !.phase = "closed", !.outcome = "turn_linked",
                      !.nativeKeys = Append(@, s.requests[s.worker])]

RecordAttemptOutcome ==
    /\ s.phase = "closed"
    /\ s' = [s EXCEPT !.worker = @ + 1, !.phase = "idle", !.outcomes = Append(@, s.outcome)]

Finished == s.cursor > Len(Program) /\ s.worker > Len(s.requests) /\ s.phase = "idle"
Done == Finished /\ UNCHANGED s
Delivery == LoadOutbox \/ BeginPosting \/ DiscordAttempt \/ SettleAttempt \/ SweepAbandoned
            \/ ExistingBotIntake \/ NoAcceptedBotInput \/ ExistingOwnerStarts \/ RecordAttemptOutcome
Next == RecordRun \/ RelogRun \/ NotifyIntakeStarts \/ DuplicateTrigger \/ Delivery
        \/ BlindUnknownRepost \/ HeldActivation \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(RecordRun) /\ WF_vars(Delivery)

CounterMatchesHistory == s.count = FailedBefore(s.cursor-1)
TriggerOnlyAtNPlusOne == \A i \in 1..Len(s.requestCounts): s.requestCounts[i] = N+1
AtMostOneTriggerPerStreak == Len(s.requests) = Cardinality(Keys(s.requests))
OrdinaryNotifyDoesNotStart == ~s.badNoticeTurn
NoBlindUnknownRepost == ~s.blindRepost
NoDuplicateDiscordMessage == s.maxMessages <= 1
MayHaveSentKeepsFence == s.row # "failed"
TurnLinkedOnlyAfterOwnerStart == s.outcome = "turn_linked" => s.intakeAccepted
AtMostOneRemediationPerIncident == Len(s.nativeKeys) = Cardinality(Keys(s.nativeKeys))
OnlyExistingAcceptedBotIntake == Len(s.nativeKeys) > 0 => AllowedBot
AllEligibleStreaksOffered == s.cursor > Len(Program) => Len(s.requests) = 2
EveryAttemptTerminates == <>Finished
ConfirmedIncidentsReachExistingOwner == <>(Finished /\ Len(s.nativeKeys) = Len(s.requests))
=============================================================================
