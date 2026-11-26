--------------------- MODULE WriterAbsorbedNoticeReuse ---------------------
EXTENDS Naturals, FiniteSets

CONSTANT Mode
VARIABLES first, consumedEnd, kind, next, restarted, replaying
vars == <<first, consumedEnd, kind, next, restarted, replaying>>

\* Both rows have the same frame and source but distinct logical identities.
FirstRange == <<0, 2>>
NextRange == <<4, 6>>
OwesNotice == kind = "direct"
Uncertain == kind \in {"contested", "unreadable"}
NativeRegistrationsAt(at) ==
    {"A"} \cup IF at < NextRange[1] THEN {} ELSE
        CASE kind = "registered" -> {"B"}
          [] kind = "contested" -> {"B", "rival"}
          [] OTHER -> {}
NoticeRegistrationsAt(at) ==
    {r \in NativeRegistrationsAt(at) :
        r # "A" \/ Mode \in {"w3", "drop-consumed-range"} \/ consumedEnd = 0 \/ at < consumedEnd}
NextRegistrations == NoticeRegistrationsAt(NextRange[1])
Attribution ==
    CASE kind = "unreadable" -> "pending"
      [] NextRegistrations = {"B"} -> "excluded"
      [] NextRegistrations = {} /\ consumedEnd = FirstRange[2] /\ Mode # "drop-unowned"
            -> "send"
      [] OTHER -> "pending"

Init ==
    /\ first = "enqueued" /\ consumedEnd = 0
    /\ kind \in {"registered", "direct", "contested", "unreadable"}
    /\ next = [row |-> "absent", closed |-> FALSE, verdict |-> "waiting",
               prepared |-> FALSE, effects |-> 0, posted |-> FALSE]
    /\ restarted = FALSE /\ replaying = FALSE

\* The initial registration remains active to native execution after its notice is excluded.
FirstCapture ==
    /\ first = "enqueued" /\ ~replaying /\ first' = "materialized"
    /\ UNCHANGED <<consumedEnd, kind, next, restarted, replaying>>
FirstClose ==
    /\ first = "materialized" /\ ~replaying /\ first' = "closed"
    /\ UNCHANGED <<consumedEnd, kind, next, restarted, replaying>>
FirstSettle ==
    /\ first = "closed" /\ ~replaying /\ first' = "excluded"
    /\ consumedEnd' = FirstRange[2]
    /\ UNCHANGED <<kind, next, restarted, replaying>>

NextEnqueue ==
    /\ first = "excluded" /\ next.row = "absent" /\ ~replaying
    /\ next' = [next EXCEPT !.row = "enqueued"]
    /\ UNCHANGED <<first, consumedEnd, kind, restarted, replaying>>
NextCapture ==
    /\ next.row = "enqueued" /\ ~replaying
    /\ next' = [next EXCEPT !.row = "materialized"]
    /\ UNCHANGED <<first, consumedEnd, kind, restarted, replaying>>
NextClose ==
    /\ next.row = "materialized" /\ ~next.closed /\ ~replaying
    /\ next' = [next EXCEPT !.closed = TRUE]
    /\ UNCHANGED <<first, consumedEnd, kind, restarted, replaying>>

ReviewNext ==
    /\ next.row = "materialized" /\ next.verdict = "waiting" /\ ~replaying
    /\ (Attribution = "excluded" => next.closed)
    /\ next' = [next EXCEPT !.verdict = Attribution]
    /\ UNCHANGED <<first, consumedEnd, kind, restarted, replaying>>
PrepareNext ==
    /\ next.verdict = "send" /\ ~next.prepared /\ ~replaying
    /\ next' = [next EXCEPT !.prepared = TRUE]
    /\ UNCHANGED <<first, consumedEnd, kind, restarted, replaying>>
PostNext ==
    /\ next.prepared /\ next.effects = 0 /\ ~replaying
    /\ next' = [next EXCEPT !.effects = 1]
    /\ UNCHANGED <<first, consumedEnd, kind, restarted, replaying>>
ConfirmNext ==
    /\ next.effects = 1 /\ ~next.posted /\ ~replaying
    /\ next' = [next EXCEPT !.posted = TRUE]
    /\ UNCHANGED <<first, consumedEnd, kind, restarted, replaying>>

Restart ==
    /\ first # "enqueued" /\ ~restarted
    /\ restarted' = TRUE /\ replaying' = TRUE
    /\ UNCHANGED <<first, consumedEnd, kind, next>>
Replay ==
    /\ replaying /\ replaying' = FALSE
    /\ UNCHANGED <<first, consumedEnd, kind, next, restarted>>

Debt == IF next.row = "materialized" /\ next.verdict # "excluded" /\ ~next.posted THEN 1 ELSE 0
Reviewed == next.verdict # "waiting"
Ready == restarted /\ ~replaying /\ next.closed /\ Reviewed
Done == Ready /\ (next.verdict # "send" \/ next.posted) /\ UNCHANGED vars
Next == FirstCapture \/ FirstClose \/ FirstSettle
    \/ NextEnqueue \/ NextCapture \/ NextClose \/ ReviewNext
    \/ PrepareNext \/ PostNext \/ ConfirmNext \/ Restart \/ Replay \/ Done
Spec == Init /\ [][Next]_vars
    /\ WF_vars(FirstCapture) /\ WF_vars(FirstClose) /\ WF_vars(FirstSettle)
    /\ WF_vars(NextEnqueue) /\ WF_vars(NextCapture) /\ WF_vars(NextClose) /\ WF_vars(ReviewNext)
    /\ WF_vars(PrepareNext) /\ WF_vars(PostNext) /\ WF_vars(ConfirmNext)
    /\ WF_vars(Restart) /\ WF_vars(Replay)

RegisteredSecondSettles == kind = "registered" /\ Reviewed => next.verdict = "excluded" /\ Debt = 0
SeparateInputCanPost == OwesNotice /\ Reviewed => next.verdict = "send"
UncertainStaysPending == Uncertain /\ Reviewed => next.verdict = "pending" /\ Debt = 1
NoExtraPost == next.effects <= 1 /\ (~OwesNotice => next.effects = 0)
NoEarlyOwnedSettlement == next.verdict = "excluded" => next.closed
RetainsPriorMembership == "A" \in NoticeRegistrationsAt(FirstRange[1])
EventuallyHandled == <> (Ready /\ IF Uncertain THEN Debt = 1 ELSE Debt = 0)
=============================================================================
