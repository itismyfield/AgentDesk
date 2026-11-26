-------------------------- MODULE HeadlessIdleReplay --------------------------
EXTENDS Naturals, FiniteSets

CONSTANT Mode
ASSUME Mode \in {"Current", "Target", "NoLink", "EofRange"}

VARIABLE s
vars == <<s>>
Turns == {"A", "B"}
Cases == {"AppendedTail", "LaterTurn", "ReplacedFile"}

\* A/B have identical text but name separate terminal occurrences.
\* Offsets are abstract record boundaries. In ReplacedFile the offsets and
\* wrapper generation repeat, but the inode identifies a different file.
Source(t) == [provider |-> "claude", channel |-> "worker", session |-> "wrapper",
              path |-> "wrapper.jsonl", device |-> 1,
              inode |-> IF t = "B" /\ s.case = "ReplacedFile" THEN 2 ELSE 1,
              generation |-> 1]
Start(t) == IF t = "A" \/ s.case = "ReplacedFile" THEN 0 ELSE 1
End(t) == Start(t) + 1
Headless(t) == t = "A" \/ s.case = "LaterTurn"
NoProof == [start |-> 0, end |-> 0, source |->
                [provider |-> "", channel |-> "", session |-> "", path |-> "",
                 device |-> 0, inode |-> 0, generation |-> 0]]

Init == \E c \in Cases:
    s = [case |-> c, available |-> {"A"}, inflight |-> "A", grace |-> TRUE,
         state |-> [t \in Turns |-> "None"], proof |-> [t \in Turns |-> NoProof],
         posts |-> [t \in Turns |-> 0], retired |-> {}]

\* The same wrapper can append B while the A delivery is still pending.
\* A separate headless reply B opens inflight after A releases its row.
Append == /\ "B" \notin s.available
          /\ s.case = "AppendedTail" \/ "A" \in s.retired
          /\ s' = [s EXCEPT !.available = @ \cup {"B"},
                            !.inflight = IF s.case = "LaterTurn" THEN "B" ELSE @,
                            !.grace = IF s.case = "LaterTurn" THEN TRUE ELSE @]

\* Capture the actual terminal span before worker visibility. EofRange instead
\* certifies bytes after A's terminal when B has already appended on this file.
Capture(t) ==
    /\ t \in s.available /\ s.inflight = t /\ Headless(t)
    /\ s.state[t] = "None"
    /\ s' = [s EXCEPT !.state[t] = "Captured",
                      !.proof[t] = IF Mode = "Current" THEN NoProof
                                  ELSE [source |-> Source(t), start |-> Start(t),
                                        end |-> IF Mode = "EofRange" /\ t = "A"
                                                   /\ s.case = "AppendedTail"
                                                   /\ "B" \in s.available
                                                THEN End("B") ELSE End(t)]]

Stage(t) == /\ s.state[t] = "Captured"
            /\ s' = [s EXCEPT !.state[t] = "Pending"]

WorkerPost(t) == /\ s.state[t] = "Pending"
                 /\ s' = [s EXCEPT !.state[t] = "Posted", !.posts[t] = @ + 1]

MarkSent(t) == /\ s.state[t] = "Posted"
               /\ s' = [s EXCEPT !.state[t] = "Sent"]

ClearInflight(t) == /\ s.inflight = t /\ s.state[t] = "Sent"
                    /\ s' = [s EXCEPT !.inflight = "None"]

ExpireGrace == /\ s.inflight = "None" /\ s.grace
               /\ s' = [s EXCEPT !.grace = FALSE]

\* Source proof suppresses only covered bytes in the matching file and
\* generation, and only if the corresponding outbox row is durably sent.
CoveredBySent(t) ==
    \E p \in Turns:
        /\ s.state[p] = "Sent" /\ s.proof[p].end > s.proof[p].start
        /\ s.proof[p].source = Source(t)
        /\ s.proof[p].start <= Start(t) /\ End(t) <= s.proof[p].end

CanReplay(t) == /\ s.inflight = "None" /\ ~s.grace
                /\ t \in s.available \ s.retired
                /\ t = "A" \/ "A" \in s.retired

ConsumeProof(t) == /\ CanReplay(t) /\ Mode # "NoLink" /\ CoveredBySent(t)
                   /\ s' = [s EXCEPT !.retired = @ \cup {t}]

IdlePost(t) == /\ CanReplay(t) /\ (Mode = "NoLink" \/ ~CoveredBySent(t))
               /\ s' = [s EXCEPT !.posts[t] = @ + 1, !.retired = @ \cup {t}]

Done == /\ s.retired = Turns /\ UNCHANGED vars

Next == Append \/ ExpireGrace \/ Done
        \/ (\E t \in Turns: Capture(t) \/ Stage(t) \/ WorkerPost(t) \/ MarkSent(t)
                           \/ ClearInflight(t) \/ ConsumeProof(t) \/ IdlePost(t))

Spec == Init /\ [][Next]_vars
        /\ WF_vars(Append) /\ WF_vars(ExpireGrace)
        /\ (\A t \in Turns: WF_vars(Capture(t)) /\ WF_vars(Stage(t))
                         /\ WF_vars(WorkerPost(t)) /\ WF_vars(MarkSent(t))
                         /\ WF_vars(ClearInflight(t)) /\ WF_vars(ConsumeProof(t))
                         /\ WF_vars(IdlePost(t)))

NoDuplicate == \A t \in Turns: s.posts[t] <= 1
NoQuietLoss == \A t \in s.retired: s.posts[t] >= 1
EveryAnswerPosts == \A t \in Turns: <> (t \in s.available /\ s.posts[t] >= 1)
=============================================================================
