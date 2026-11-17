------------------------ MODULE DecisionDiscussion ------------------------
EXTENDS Naturals, FiniteSets, TLC
CONSTANT Mode, Scenario, Actors
Items == {1,2,3}
Messages == {1,2}
BaseKind(m) == IF m = 3 \/ (m = 2 /\ Scenario = "Mixed") THEN "choice" ELSE "opinion"
IsOwner(m) == ~(Actors = "ForeignSecond" /\ m = 2)
Original(m) == IF m = 1 THEN "  raw opinion\n " ELSE "second raw opinion"
Terminal(s) == s \in {"delivered","failed","unknown_closed","ignored"}
VARIABLES gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes
vars == <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
Init == gate = "open"
    /\ choice = 0
    /\ first = 0
    /\ ledger = {}
    /\ kind = [m \in Items |-> "none"]
    /\ stage = [m \in Items |-> "unseen"]
    /\ raw = [m \in Items |-> ""]
    /\ forwarded = [m \in Items |-> ""]
    /\ claims = [m \in Items |-> 0]
    /\ writes = [m \in Items |-> 0]
    /\ pending = [m \in Items |-> 0]
    /\ applied = [m \in Items |-> 0]
    /\ cursor = 0
    /\ scanned = {}
    /\ notes = {}
    /\ atClose = [m \in Items |-> 0]
    /\ replyPending = FALSE
    /\ closePending = FALSE
    /\ replyCount = 0
    /\ explicitClose = FALSE
    /\ archived = FALSE
    /\ locked = FALSE
    /\ visible = "open"
    /\ alive = TRUE
    /\ crashes = 1
    /\ busy = TRUE
    /\ badScope = FALSE
    /\ badReply = FALSE
    /\ afterCloseAccepted = FALSE
    /\ failureNotes = {}
Accept(m) == IsOwner(m) /\ (gate = "open" \/ Mode = "BadClosedGate")
Candidate(m) == IF Mode = "BadClassify" /\ m = 1 THEN "choice" ELSE BaseKind(m)
CanObserve(m) == m \notin ledger \/ (Mode = "NoDedup" /\ Terminal(stage[m]) /\ writes[m] < 2 /\ gate = "open")
ObserveMessage(m, via) == /\ alive /\ CanObserve(m) /\ (m \in Messages \/ via = "button")
    /\ ledger' = (ledger \cup {m})
    /\ kind' = ([kind EXCEPT ![m] = IF Accept(m) THEN Candidate(m) ELSE "ignore"])
    /\ stage' = ([stage EXCEPT ![m] = IF Accept(m) THEN "not_started" ELSE "ignored"])
    /\ raw' = ([raw EXCEPT ![m] = IF Accept(m) THEN IF Mode = "TrimRaw" THEN "trimmed" ELSE Original(m) ELSE ""])
    /\ choice' = (IF Accept(m) /\ Candidate(m) = "choice" /\ choice = 0 THEN m ELSE choice)
    /\ first' = (IF Accept(m) /\ Candidate(m) = "choice" /\ first = 0 THEN m ELSE first)
    /\ gate' = (IF Accept(m) /\ Candidate(m) = "choice" THEN "selecting" ELSE gate)
    /\ cursor' = (IF Mode = "BadCursor" /\ via = "gateway" THEN m ELSE cursor)
    /\ afterCloseAccepted' = (afterCloseAccepted \/ (Accept(m) /\ gate = "closed"))
    /\ UNCHANGED <<forwarded, claims, writes, pending, applied, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, failureNotes>>
ScanSeen(m) == /\ alive /\ m \in Messages /\ m \in ledger /\ m \notin scanned
    /\ scanned' = (scanned \cup {m})
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
AdvanceCursor(n) == /\ alive /\ n \in Messages /\ n > cursor /\ (\A i \in 1..n: i \in scanned /\ i \in ledger)
    /\ cursor' = (n)
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
Begin(m) == /\ alive /\ stage[m] = "not_started" /\ gate # "closed" /\ ~\E n \in Items: stage[n] \in {"busy","writing"}
    /\ stage' = ([stage EXCEPT ![m] = "busy"])
    /\ claims' = ([claims EXCEPT ![m] = @ + 1])
    /\ UNCHANGED <<gate, choice, first, ledger, kind, raw, forwarded, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
TurnBoundary == /\ busy
    /\ busy' = (FALSE)
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, badScope, badReply, afterCloseAccepted, failureNotes>>
Write(m) == /\ alive /\ stage[m] = "busy" /\ (gate # "closed" \/ Mode = "BadClosedWrite") /\ (writes[m] = 0 \/ Mode = "NoDedup") /\ ~busy
    /\ stage' = ([stage EXCEPT ![m] = "writing"])
    /\ writes' = ([writes EXCEPT ![m] = @ + 1])
    /\ pending' = ([pending EXCEPT ![m] = @ + 1])
    /\ forwarded' = ([forwarded EXCEPT ![m] = raw[m]])
    /\ UNCHANGED <<gate, choice, first, ledger, kind, raw, claims, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
RemoteEffect(m) == /\ pending[m] > 0 /\ applied[m] < 2
    /\ pending' = ([pending EXCEPT ![m] = @ - 1])
    /\ applied' = ([applied EXCEPT ![m] = @ + 1])
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
Confirm(m) == /\ alive /\ stage[m] \in {"writing","unknown_closed"} /\ applied[m] > 0
    /\ stage' = ([stage EXCEPT ![m] = "delivered"])
    /\ failureNotes' = (failureNotes \ {m})
    /\ UNCHANGED <<gate, choice, first, ledger, kind, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted>>
NormalizeFailureDiscard(m) == /\ alive /\ stage[m] = "busy"
    /\ stage' = ([stage EXCEPT ![m] = "failed"])
    /\ raw' = ([raw EXCEPT ![m] = ""])
    /\ UNCHANGED <<gate, choice, first, ledger, kind, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
OpinionNotice(m) == /\ alive /\ kind[m] = "opinion" /\ m \notin notes /\ (stage[m] = "delivered" \/ Mode = "EarlyNotice")
    /\ notes' = (notes \cup {m})
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
FailureNotice(m) == /\ alive /\ stage[m] \in {"failed","unknown_closed"} /\ m \notin failureNotes
    /\ failureNotes' = (failureNotes \cup {m})
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted>>
ChoiceCloses == /\ alive /\ choice # 0 /\ Terminal(stage[choice]) /\ gate # "closed"
    /\ gate' = ("closed")
    /\ atClose' = (writes)
    /\ stage' = ([m \in Items |-> IF stage[m] \in {"not_started","busy"} /\ Mode # "BadClosedWrite" THEN "failed" ELSE stage[m]])
    /\ raw' = ([m \in Items |-> IF stage[m] \in {"not_started","busy"} THEN "" ELSE raw[m]])
    /\ UNCHANGED <<choice, first, ledger, kind, forwarded, claims, writes, pending, applied, cursor, scanned, notes, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
OpinionCloses == /\ Mode = "OpinionCloses" /\ notes # {} /\ gate = "open"
    /\ gate' = ("closed")
    /\ atClose' = (writes)
    /\ UNCHANGED <<choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
AgentReply(actor) == /\ alive /\ replyCount = 0 /\ (actor = "requester" \/ Mode = "BadScope")
    /\ replyCount' = (1)
    /\ replyPending' = (TRUE)
    /\ badScope' = (actor # "requester")
    /\ badReply' = (Mode = "ReplyReopens" /\ gate = "closed")
    /\ gate' = (IF Mode = "ReplyReopens" THEN "open" ELSE gate)
    /\ UNCHANGED <<choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, closePending, explicitClose, archived, locked, visible, alive, crashes, busy, afterCloseAccepted, failureNotes>>
AgentClose(actor) == /\ alive /\ ~explicitClose /\ (actor = "requester" \/ Mode = "BadScope")
    /\ gate' = ("closed")
    /\ atClose' = (writes)
    /\ stage' = ([m \in Items |-> IF stage[m] \in {"not_started","busy"} /\ Mode # "BadClosedWrite" THEN "failed" ELSE stage[m]])
    /\ raw' = ([m \in Items |-> IF stage[m] \in {"not_started","busy"} THEN "" ELSE raw[m]])
    /\ explicitClose' = (TRUE)
    /\ closePending' = (TRUE)
    /\ badScope' = (actor # "requester")
    /\ UNCHANGED <<choice, first, ledger, kind, forwarded, claims, writes, pending, applied, cursor, scanned, notes, replyPending, replyCount, archived, locked, visible, alive, crashes, busy, badReply, afterCloseAccepted, failureNotes>>
MakeEditable == /\ alive /\ (replyPending \/ closePending) /\ (archived \/ locked)
    /\ archived' = (FALSE)
    /\ locked' = (FALSE)
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
PostReply == /\ alive /\ replyPending /\ ~archived /\ ~locked
    /\ replyPending' = (FALSE)
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, closePending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
PostConclusion == /\ alive /\ closePending /\ ~archived /\ ~locked
    /\ closePending' = (FALSE)
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, replyCount, explicitClose, archived, locked, visible, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
RepairLifecycle == /\ Mode # "NoArchive" /\ alive /\ gate = "closed" /\ ~replyPending /\ ~closePending /\ (~archived \/ ~locked \/ visible # "closed")
    /\ archived' = (TRUE)
    /\ locked' = (TRUE)
    /\ visible' = ("closed")
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, alive, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
Crash == /\ alive /\ crashes = 1
    /\ alive' = (FALSE)
    /\ crashes' = (0)
    /\ stage' = ([m \in Items |-> IF stage[m] = "writing" THEN "unknown_closed" ELSE stage[m]])
    /\ UNCHANGED <<gate, choice, first, ledger, kind, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
Boot == /\ ~alive
    /\ alive' = (TRUE)
    /\ UNCHANGED <<gate, choice, first, ledger, kind, stage, raw, forwarded, claims, writes, pending, applied, cursor, scanned, notes, atClose, replyPending, closePending, replyCount, explicitClose, archived, locked, visible, crashes, busy, badScope, badReply, afterCloseAccepted, failureNotes>>
Progress == (\E m \in Items: Begin(m) \/ Write(m) \/ RemoteEffect(m) \/ Confirm(m) \/ NormalizeFailureDiscard(m) \/ OpinionNotice(m) \/ FailureNotice(m)) \/ ChoiceCloses
Repair == MakeEditable \/ PostReply \/ PostConclusion \/ RepairLifecycle
Next == (\E m \in Messages, v \in {"gateway","scan"}: ObserveMessage(m,v)) \/ ObserveMessage(3,"button")
    \/ (\E m \in Messages: ScanSeen(m) \/ AdvanceCursor(m)) \/ Progress \/ Repair \/ TurnBoundary
    \/ (\E actor \in {"requester","other_agent"}: AgentReply(actor) \/ AgentClose(actor))
    \/ OpinionCloses \/ Crash \/ Boot
Spec == Init /\ [][Next]_vars /\ WF_vars(Boot) /\ WF_vars(TurnBoundary) /\ WF_vars(Progress) /\ WF_vars(Repair)
NoDuplicateOpinion == \A m \in Items: BaseKind(m) = "opinion" => applied[m] <= 1
OperatorOnly == \A m \in Items: ~IsOwner(m) => claims[m] = 0 /\ writes[m] = 0 /\ choice # m
ChoiceOne == choice = first
NoClosedAdmission == ~afterCloseAccepted
NoClosedWrites == gate = "closed" => \A m \in Items: writes[m] <= atClose[m]
OpinionRawPreserved == \A m \in Items: kind[m] = "opinion" /\ writes[m] > 0 => forwarded[m] = Original(m)
OpinionKeepsOpen == ~explicitClose /\ choice = 0 => gate = "open"
OpinionNoticeTruth == \A m \in notes: applied[m] = 1 /\ kind[m] = "opinion"
RequesterOnly == ~badScope
ReplyNoLifecycleChange == ~badReply
CursorCoversAll == \A m \in 1..cursor: m \in ledger
ClassMatches == \A m \in ledger: kind[m] # "ignore" => kind[m] = BaseKind(m)
ViewTruth == visible = "closed" => gate = "closed"
LiteralDisplayEquality == visible = gate
ClosedEventuallyArchived == (gate = "closed") ~> (archived /\ locked /\ visible = "closed")
OpinionEventuallyAccounted == \A m \in Items: (kind[m] = "opinion" /\ stage[m] # "ignored") ~> ((stage[m] \in {"failed","unknown_closed"} /\ m \in failureNotes) \/ (stage[m] = "delivered" /\ m \in notes))
\* Without dedup a message can be claimed again without end; the contrast needs two claims at most.
ContrastBound == \A m \in Items: claims[m] <= 2
=============================================================================
