-------------------------- MODULE HeadlessIdleReplay --------------------------
EXTENDS Naturals, FiniteSets

CONSTANT Mode
ASSUME Mode \in {"Current", "Target", "NoLink", "EofRange",
                 "CursorCurrent", "CursorTarget", "CursorNoAdvance", "CursorUnsafePrefix"}

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

\* Refine the already-Sent, post-grace AppendedTail path with the actual
\* scanner cursor and the ordered sink's independently parsed read window.
WindowBytes == 1048576
PrefixEnd == 75
SentAnswerEnd == 706908
LaterAnswerEnd == 1312781
FirstFrameEnd == 1040066
PrefixKinds == {"Sentinel", "Visible", "Failed", "Pending"}

CursorInit == \E kind \in PrefixKinds:
    s = [prefix |-> kind, cursor |-> 0, committed |-> 0, phase |-> "Scan",
         frameStart |-> 0, frameEnd |-> 0, laterAvailable |-> FALSE,
         prefixChecked |-> FALSE, outboxPosts |-> 1, idlePostsA |-> 0,
         visiblePrefixPosts |-> 0, idlePostsB |-> 0, skippedA |-> FALSE]

CursorAppend == /\ ~s.laterAvailable
                /\ s' = [s EXCEPT !.laterAvailable = TRUE]

\* An earlier owned prefix is first Pending, then Sent and actually visible.
\* A failed headless row is no longer an owner; its terminal needs idle delivery.
CursorConfirmPrefix == /\ s.prefix = "Pending"
                       /\ s' = [s EXCEPT !.prefix = "Sent", !.visiblePrefixPosts = 1]

CursorHasPrefixProof == s.cursor = 0 /\ s.prefix = "Sent"
CursorInSentAnswer == PrefixEnd <= s.cursor /\ s.cursor < SentAnswerEnd
CursorCanSkipGap == /\ s.cursor = 0 /\ s.prefix \notin {"Pending", "Sent"}
                    /\ s.prefix = "Sentinel" \/ s.committed >= PrefixEnd
                         \/ Mode = "CursorUnsafePrefix"

CursorPrefixProof == /\ s.phase = "Scan" /\ CursorHasPrefixProof
                     /\ s' = [s EXCEPT !.cursor = PrefixEnd]

CursorGapAdvance == /\ s.phase = "Scan" /\ CursorCanSkipGap
                    /\ Mode \in {"CursorTarget", "CursorUnsafePrefix"}
                    /\ s' = [s EXCEPT !.cursor = PrefixEnd]

\* The mutation recognizes the prefix but drops the local cursor write.
CursorObserveWithoutAdvance == /\ s.phase = "Scan" /\ CursorCanSkipGap
                               /\ Mode = "CursorNoAdvance" /\ ~s.prefixChecked
                               /\ s' = [s EXCEPT !.prefixChecked = TRUE]

CursorConsumeSent == /\ s.phase = "Scan" /\ CursorInSentAnswer
                     /\ s' = [s EXCEPT !.cursor = SentAnswerEnd, !.skippedA = TRUE]

CursorWidth == s.cursor + WindowBytes
CursorLength == IF s.laterAvailable THEN LaterAnswerEnd ELSE SentAnswerEnd
CursorWindowEnd ==
    LET newlines == {0, PrefixEnd, SentAnswerEnd, FirstFrameEnd, LaterAnswerEnd}
        complete == {end \in newlines: end <= CursorWidth /\ end <= CursorLength}
    IN CHOOSE end \in complete: \A other \in complete: other <= end

CursorOpenWindow ==
    /\ s.phase = "Scan" /\ s.cursor < CursorLength
    /\ ~CursorInSentAnswer /\ ~CursorHasPrefixProof /\ s.prefix # "Pending"
    /\ ~(CursorCanSkipGap /\ Mode \in {"CursorTarget", "CursorUnsafePrefix"})
    /\ s' = [s EXCEPT !.phase = "Frame", !.frameEnd = CursorWindowEnd,
                      !.frameStart = IF s.committed > s.cursor THEN s.committed ELSE s.cursor]

CursorPostVisiblePrefix ==
    /\ s.phase = "Frame" /\ s.frameStart < PrefixEnd /\ PrefixEnd <= s.frameEnd
    /\ s.prefix \in {"Visible", "Failed"} /\ s.visiblePrefixPosts = 0
    /\ s' = [s EXCEPT !.visiblePrefixPosts = 1, !.committed = PrefixEnd]

CursorOwesVisiblePrefix == /\ s.prefix \in {"Visible", "Failed"}
                           /\ s.frameStart < PrefixEnd /\ s.visiblePrefixPosts = 0

CursorSinkSkip == /\ s.phase = "Frame" /\ ~CursorOwesVisiblePrefix
                  /\ s.frameStart < SentAnswerEnd /\ SentAnswerEnd <= s.frameEnd
                  /\ s.frameEnd < LaterAnswerEnd
                  /\ s' = [s EXCEPT !.phase = "Scan", !.skippedA = TRUE]

CursorPostB == /\ s.phase = "Frame" /\ s.frameEnd = LaterAnswerEnd
               /\ SentAnswerEnd <= s.frameStart /\ s.frameStart < LaterAnswerEnd
               /\ s' = [s EXCEPT !.phase = "Scan", !.idlePostsB = @ + 1,
                                 !.committed = LaterAnswerEnd]

CursorRetireCommitted ==
    /\ s.phase = "Frame" /\ s.frameEnd <= s.frameStart
    /\ s' = [s EXCEPT !.phase = "Scan", !.cursor = s.frameEnd]

CursorDone == s.cursor = LaterAnswerEnd /\ UNCHANGED vars

CursorNext == CursorAppend \/ CursorConfirmPrefix \/ CursorPrefixProof \/ CursorGapAdvance
              \/ CursorObserveWithoutAdvance \/ CursorConsumeSent \/ CursorOpenWindow
              \/ CursorPostVisiblePrefix \/ CursorSinkSkip \/ CursorPostB
              \/ CursorRetireCommitted \/ CursorDone

CursorSpec == CursorInit /\ [][CursorNext]_vars
              /\ WF_vars(CursorAppend) /\ WF_vars(CursorConfirmPrefix)
              /\ WF_vars(CursorPrefixProof) /\ WF_vars(CursorGapAdvance)
              /\ WF_vars(CursorObserveWithoutAdvance) /\ WF_vars(CursorConsumeSent)
              /\ WF_vars(CursorOpenWindow) /\ WF_vars(CursorPostVisiblePrefix)
              /\ WF_vars(CursorSinkSkip) /\ WF_vars(CursorPostB)
              /\ WF_vars(CursorRetireCommitted)

ASkippedBExactlyOnce == /\ s.outboxPosts = 1 /\ s.idlePostsA = 0 /\ s.idlePostsB <= 1
                        /\ (s.cursor = LaterAnswerEnd => s.skippedA /\ s.idlePostsB = 1)
NoUnpostedPrefixSkipped == s.cursor >= PrefixEnd /\ s.prefix # "Sentinel" => s.visiblePrefixPosts = 1
ReadStartAdvances == s.laterAvailable ~> (s.cursor >= SentAnswerEnd)
LaterAnswerPosts == s.laterAvailable ~> (s.idlePostsB = 1)

=============================================================================
