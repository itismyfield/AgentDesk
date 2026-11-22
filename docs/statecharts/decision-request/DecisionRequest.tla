------------------------- MODULE DecisionRequest -------------------------
EXTENDS Naturals, TLC
CONSTANT Mode
VARIABLES phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved
vars == <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Init == phase = "new"
    /\ choice = 0
    /\ first = 0
    /\ accepted = FALSE
    /\ payload = FALSE
    /\ runnable = FALSE
    /\ begun = FALSE
    /\ claims = 0
    /\ norms = 0
    /\ session = 1
    /\ resolved = 0
    /\ busy = TRUE
    /\ sent = 0
    /\ writes = 0
    /\ pending = 0
    /\ writeTarget = 0
    /\ dispatchSession = 0
    /\ receipt = FALSE
    /\ alive = TRUE
    /\ crashes = 1
    /\ sessionMoves = 1
    /\ archived = FALSE
    /\ locked = FALSE
    /\ archiveMoves = 1
    /\ view = "none"
    /\ closed = FALSE
    /\ atClose = 0
    /\ unauthorized = FALSE
    /\ queueFault = FALSE
    /\ preserved = FALSE
Desired == CASE phase \in {"new", "posting"} -> "none"
             [] phase = "open" -> "open"
             [] phase = "selected" -> "selected"
             [] phase = "queued" -> "waiting"
             [] phase = "delivered" -> "delivered"
             [] phase = "failed" -> "failed"
             [] phase = "unknown_closed" -> "unknown"
             [] phase = "post_failed" -> "none"
             [] OTHER -> "processing"
PostBegin == /\ alive /\ phase = "new"
    /\ phase' = ("posting")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
PostFinish == /\ alive /\ phase = "posting"
    /\ phase' = (IF Mode = "PostFailure" THEN "post_failed" ELSE "open")
    /\ view' = (IF Mode = "PostFailure" THEN "none" ELSE "open")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved>>
Click(actor, option) == /\ alive /\ phase = "open" /\ (actor = "operator" \/ Mode = "BadAuth")
    /\ choice' = (IF Mode = "OldLiveCheck" /\ session # 1 THEN 0 ELSE option)
    /\ first' = (option)
    /\ accepted' = (TRUE)
    /\ phase' = ("selected")
    /\ unauthorized' = (actor # "operator")
    /\ UNCHANGED <<payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, queueFault, preserved>>
AckReclickOtherUserStop == UNCHANGED vars
\* The not_started -> resolving update is the only start. NoClaimCas drops its state condition.
Claim == /\ alive /\ ~closed
    /\ \/ phase = "selected" /\ ~begun
       \/ Mode = "NoClaimCas" /\ phase \in {"claimed", "queued", "prepared"} /\ claims < 2
    /\ phase' = ("claimed")
    /\ begun' = (TRUE)
    /\ claims' = (claims + 1)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
ResolvePresent == /\ alive /\ phase = "claimed" /\ session # 0 /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = (IF busy THEN "queued" ELSE "prepared")
    /\ resolved' = (IF busy THEN 0 ELSE session)
    /\ payload' = (busy)
    /\ runnable' = (busy)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
NormalizeAbsent == /\ alive /\ phase = "claimed" /\ session = 0 /\ norms = 0 /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = ("normalizing")
    /\ norms' = (1)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
AbsentAfterNormalize == /\ alive /\ phase = "claimed" /\ session = 0 /\ norms = 1
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved>>
NormalizeSuccess == /\ alive /\ phase = "normalizing"
    /\ phase' = ("claimed")
    /\ session' = (IF session = 0 THEN 2 ELSE session)
    /\ busy' = (FALSE)
    /\ queueFault' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, resolved, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, preserved>>
NormalizeFailure == /\ alive /\ phase = "normalizing"
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved>>
RefusalNormalize == /\ alive /\ (phase = "prepared" \/ (phase = "queued" /\ queueFault)) /\ norms = 0 /\ writes = 0 /\ ~closed
    /\ phase' = ("normalizing")
    /\ norms' = (1)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ resolved' = (0)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
RefusalFinal == /\ alive /\ (phase = "prepared" \/ (phase = "queued" /\ queueFault)) /\ norms = 1 /\ writes = 0 /\ ~closed
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved>>
QueueRefused == /\ alive /\ phase = "queued" /\ ~queueFault
    /\ queueFault' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, preserved>>
TurnBoundary == /\ Mode # "NoTurnBoundary" /\ busy
    /\ busy' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
DrainCurrent == /\ alive /\ phase = "queued" /\ ~busy /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = ("claimed")
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ resolved' = (0)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
BeginSend == /\ alive /\ phase = "prepared" /\ (~closed \/ Mode = "RetryClosed") /\ (writes = 0 \/ Mode = "RetryClosed") /\ session # 0 /\ (resolved = session \/ Mode = "BadFence")
    /\ phase' = ("sending")
    /\ writes' = (writes + 1)
    /\ pending' = (pending + 1)
    /\ writeTarget' = (resolved)
    /\ dispatchSession' = (session)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
RemoteEffect == /\ pending > 0 /\ sent < 2
    /\ pending' = (pending - 1)
    /\ sent' = (sent + 1)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, writes, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Observe == /\ alive /\ phase \in {"sending", "unknown", "unknown_closed"} /\ sent > 0
    /\ phase' = ("delivered")
    /\ receipt' = (TRUE)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Uncertain == /\ alive /\ phase = "sending"
    /\ phase' = ("unknown")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
CloseUnknown == /\ Mode # "NoRecovery" /\ alive /\ phase = "unknown" /\ writes > 0
    /\ phase' = ("unknown_closed")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved>>
CloseAbandoned == /\ Mode # "NoRecovery" /\ alive /\ phase = "unknown" /\ writes = 0
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved>>
Finalize == /\ alive /\ phase = "closing"
    /\ phase' = ("failed")
    /\ payload' = (Mode = "NoDiscard")
    /\ runnable' = (Mode = "NoDiscard")
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
WakeFailed == /\ Mode = "RetryClosed" /\ alive /\ phase \in {"failed", "unknown_closed"}
    /\ phase' = ("queued")
    /\ payload' = (TRUE)
    /\ runnable' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
CancelClear == /\ alive /\ phase \in {"selected", "claimed", "normalizing", "queued", "prepared"}
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved>>
ChangeSession == /\ sessionMoves = 1
    /\ session' = (IF session = 1 THEN 2 ELSE 0)
    /\ sessionMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
EndSession == /\ sessionMoves = 1
    /\ session' = (0)
    /\ sessionMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Crash == /\ alive /\ crashes = 1
    /\ alive' = (FALSE)
    /\ crashes' = (0)
    /\ choice' = (IF Mode = "Volatile" THEN 0 ELSE choice)
    /\ phase' = (IF phase \in {"sending", "claimed", "normalizing", "prepared"} THEN "unknown" ELSE phase)
    /\ UNCHANGED <<first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Boot == /\ ~alive
    /\ alive' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Archive == /\ archiveMoves = 1 /\ view # "none"
    /\ archived' = (TRUE)
    /\ archiveMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, locked, view, closed, atClose, unauthorized, queueFault, preserved>>
ArchiveLocked == /\ archiveMoves = 1 /\ view # "none"
    /\ archived' = (TRUE)
    /\ locked' = (TRUE)
    /\ archiveMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Unlock == /\ Mode # "NoUnlock" /\ alive /\ locked /\ view # Desired
    /\ locked' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Unarchive == /\ Mode # "NoUnarchive" /\ alive /\ archived /\ ~locked /\ view # Desired
    /\ archived' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
Render == /\ alive /\ ~archived /\ ~locked /\ view # Desired
    /\ view' = (Desired)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved>>
EarlySuccess == /\ Mode = "EarlySuccess" /\ accepted /\ sent = 0
    /\ view' = ("delivered")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved>>
HandoffDecision == /\ Mode = "BadHandoff" /\ ~accepted /\ writes = 0 /\ session # 0
    /\ phase' = ("sending")
    /\ writes' = (1)
    /\ pending' = (1)
    /\ writeTarget' = (session)
    /\ dispatchSession' = (session)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved>>
\* A request that names source=decision itself has no approved event. Every entry refuses it;
\* BadProvenance lets it reach the byte-keeping planner that only an approved opinion may use.
SelfDeclared == /\ Mode = "BadProvenance" /\ ~accepted /\ ~preserved
    /\ preserved' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault>>
Progress == Claim \/ ResolvePresent \/ NormalizeAbsent \/ AbsentAfterNormalize \/ NormalizeSuccess \/ NormalizeFailure \/ RefusalNormalize \/ RefusalFinal \/ DrainCurrent \/ BeginSend \/ RemoteEffect \/ Observe \/ Uncertain \/ CloseUnknown \/ CloseAbandoned \/ Finalize
Repair == Unlock \/ Unarchive \/ Render
Next == PostBegin \/ PostFinish \/ (\E a \in {"operator", "stranger"}, o \in {1,2}: Click(a,o))
        \/ AckReclickOtherUserStop \/ QueueRefused \/ Progress \/ TurnBoundary \/ ChangeSession \/ EndSession
        \/ Crash \/ Boot \/ Archive \/ ArchiveLocked \/ Repair \/ WakeFailed \/ CancelClear \/ EarlySuccess \/ HandoffDecision \/ SelfDeclared
Spec == Init /\ [][Next]_vars /\ WF_vars(Boot) /\ WF_vars(Progress) /\ WF_vars(Repair) /\ WF_vars(TurnBoundary)
NoDuplicate == sent <= 1
ChoiceRecorded == accepted => choice # 0
FirstChoiceWins == accepted => choice = first
OperatorOnly == ~unauthorized
ExecutionAuthorized == writes > 0 => accepted /\ choice # 0 /\ ~unauthorized
NoFalseSuccess == view = "delivered" => receipt /\ sent = 1
CurrentAtDispatch == writes > 0 => writeTarget = dispatchSession
NoFailureQueue == phase \in {"failed", "unknown_closed"} => ~payload /\ ~runnable
NoRearmAfterClose == closed => writes <= atClose
AttemptOnce == claims <= 1
NormalizeOnce == norms <= 1
PreservedNeedsApproval == preserved => accepted
StrictDisplayEquality == view = "none" \/ view = Desired
EventuallySettled == accepted ~> ((phase = "delivered" /\ view = "delivered")
    \/ (phase = "failed" /\ view = "failed") \/ (phase = "unknown_closed" /\ view = "unknown"))
\* Contrast configs let writes repeat without end; this bound only stops exploring past two.
ContrastBound == sent <= 2 /\ writes <= 2
=============================================================================
