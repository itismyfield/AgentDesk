------------------------- MODULE DecisionRequest -------------------------
EXTENDS Naturals, TLC
CONSTANT Mode
VARIABLES phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound
vars == <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
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
    /\ preserved = "none"
    /\ journal = FALSE
    /\ holds = 0
    /\ tookLive = FALSE
    /\ kept = FALSE
    /\ noticed = FALSE
    /\ otherKept = TRUE
    /\ otherNoticed = FALSE
    /\ posts = 0
    /\ early = FALSE
    /\ leaked = FALSE
    /\ bootstrapped = FALSE
    /\ tempHeld = FALSE
    /\ listed = "online"
    /\ gone = FALSE
    /\ bound = TRUE
Desired == CASE phase \in {"new", "posting"} -> "none"
             [] phase = "disabled" -> "not_yet"
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
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* The starter is posted once under its journal, with its buttons disabled and the notice that no
\* selection is taken yet. A lost answer is settled by finding that same post; RePost sends another.
PostFinish == /\ alive /\ phase = "posting" /\ posts = 0 /\ Mode # "PostFailure"
    /\ posts' = (1)
    /\ \E heard \in BOOLEAN:
        /\ phase' = (IF heard THEN "disabled" ELSE "posting")
        /\ view' = (IF heard THEN "not_yet" ELSE view)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
PostRejected == /\ Mode = "PostFailure" /\ alive /\ phase = "posting"
    /\ phase' = ("post_failed")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
Reconcile == /\ alive /\ phase = "posting" /\ posts > 0
    /\ posts' = (IF Mode = "RePost" THEN posts + 1 ELSE posts)
    /\ phase' = ("disabled")
    /\ view' = ("not_yet")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* The buttons are enabled only once selection handling is wired (a later slice).
Activate == /\ alive /\ phase = "disabled"
    /\ phase' = ("open")
    /\ view' = ("open")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* A message in the Decision thread never reaches the agent's ordinary intake. The guard knows the
\* forum, so it also holds before the thread id is stored; GuardByThread only knows stored threads.
ThreadMessage == /\ posts > 0 /\ ~leaked
    /\ (Mode = "NoIntakeGuard" \/ (Mode = "GuardByThread" /\ phase = "posting"))
    /\ leaked' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, bootstrapped, tempHeld, listed, gone, bound>>
Click(actor, option) == /\ alive /\ (phase = "open" \/ (Mode = "EarlyButtons" /\ phase = "disabled")) /\ (actor = "operator" \/ Mode = "BadAuth")
    /\ choice' = (IF Mode = "OldLiveCheck" /\ session # 1 THEN 0 ELSE option)
    /\ first' = (option)
    /\ accepted' = (TRUE)
    /\ phase' = ("selected")
    /\ unauthorized' = (actor # "operator")
    /\ early' = (phase = "disabled")
    /\ UNCHANGED <<payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, leaked, bootstrapped, tempHeld, listed, gone, bound>>
AckReclickOtherUserStop == UNCHANGED vars
\* The not_started -> resolving update is the only start. NoClaimCas drops its state condition.
Claim == /\ alive /\ ~closed /\ (bootstrapped \/ Mode = "TempHolderId")
    /\ \/ phase = "selected" /\ ~begun
       \/ Mode = "NoClaimCas" /\ phase \in {"claimed", "queued", "prepared"} /\ claims < 2
    /\ phase' = ("claimed")
    /\ begun' = (TRUE)
    /\ claims' = (claims + 1)
    /\ tempHeld' = (~bootstrapped)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, listed, gone, bound>>
ResolvePresent == /\ alive /\ phase = "claimed" /\ session # 0 /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = (IF busy THEN "queued" ELSE "prepared")
    /\ resolved' = (IF busy THEN 0 ELSE session)
    /\ payload' = (busy)
    /\ runnable' = (busy)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
NormalizeAbsent == /\ alive /\ phase = "claimed" /\ session = 0 /\ norms = 0 /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = ("normalizing")
    /\ norms' = (1)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
AbsentAfterNormalize == /\ alive /\ phase = "claimed" /\ session = 0 /\ norms = 1
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
NormalizeSuccess == /\ alive /\ phase = "normalizing"
    /\ phase' = ("claimed")
    /\ session' = (IF session = 0 THEN 2 ELSE session)
    /\ busy' = (FALSE)
    /\ queueFault' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, resolved, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
NormalizeFailure == /\ alive /\ phase = "normalizing"
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
RefusalNormalize == /\ alive /\ (phase = "prepared" \/ (phase = "queued" /\ queueFault)) /\ norms = 0 /\ writes = 0 /\ ~closed
    /\ phase' = ("normalizing")
    /\ norms' = (1)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ resolved' = (0)
    /\ journal' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
RefusalFinal == /\ alive /\ (phase = "prepared" \/ (phase = "queued" /\ queueFault)) /\ norms = 1 /\ writes = 0 /\ ~closed
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ journal' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
QueueRefused == /\ alive /\ phase = "queued" /\ ~queueFault
    /\ queueFault' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* The owner's admission answered pending: the original may sit in its queue. The fence closes the
\* event first and Finalize removes that exact original; PendingLeft is the close without removal.
OwnerPending == /\ alive /\ phase = "claimed" /\ session # 0 /\ busy /\ ~closed
    /\ phase' = (IF Mode = "PendingLeft" THEN "failed" ELSE "closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ payload' = (TRUE)
    /\ runnable' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
TurnBoundary == /\ Mode # "NoTurnBoundary" /\ busy
    /\ busy' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
DrainCurrent == /\ alive /\ phase = "queued" /\ ~busy /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = ("claimed")
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ resolved' = (0)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* The may_have_sent journal: one PG update before any key. A closed or written event never gets it.
Journal == /\ alive /\ phase = "prepared" /\ ~journal /\ (~closed \/ Mode = "RetryClosed") /\ (writes = 0 \/ Mode = "RetryClosed")
    /\ journal' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* The input box was in use and the owner proved no key was sent: the same original waits again.
ReleaseHeld == /\ alive /\ phase = "prepared" /\ journal /\ holds = 0 /\ writes = 0 /\ ~closed
    /\ phase' = ("queued")
    /\ journal' = (FALSE)
    /\ holds' = (1)
    /\ payload' = (TRUE)
    /\ runnable' = (TRUE)
    /\ resolved' = (0)
    /\ busy' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
BeginSend == /\ alive /\ phase = "prepared" /\ (journal \/ Mode = "NoJournal") /\ (~closed \/ Mode = "RetryClosed") /\ (writes = 0 \/ Mode = "RetryClosed") /\ session # 0 /\ (resolved = session \/ Mode = "BadFence")
    /\ phase' = ("sending")
    /\ writes' = (writes + 1)
    /\ pending' = (pending + 1)
    /\ writeTarget' = (resolved)
    /\ dispatchSession' = (session)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
RemoteEffect == /\ pending > 0 /\ sent < 2
    /\ pending' = (pending - 1)
    /\ sent' = (sent + 1)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, writes, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
Observe == /\ alive /\ phase \in {"sending", "unknown", "unknown_closed"} /\ sent > 0
    /\ phase' = ("delivered")
    /\ receipt' = (TRUE)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
Uncertain == /\ alive /\ phase = "sending"
    /\ phase' = ("unknown")
    /\ kept' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
CloseUnknown == /\ Mode # "NoRecovery" /\ alive /\ phase = "unknown" /\ (writes > 0 \/ journal)
    /\ phase' = ("unknown_closed")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ \E done \in BOOLEAN: kept' = (kept /\ ~done)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
CloseAbandoned == /\ Mode # "NoRecovery" /\ alive /\ phase = "unknown" /\ writes = 0 /\ ~journal
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
Finalize == /\ alive /\ phase = "closing"
    /\ phase' = ("failed")
    /\ payload' = (Mode = "NoDiscard")
    /\ runnable' = (Mode = "NoDiscard")
    /\ \E done \in BOOLEAN: kept' = (kept \/ (payload /\ ~done))
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
WakeFailed == /\ Mode = "RetryClosed" /\ alive /\ phase \in {"failed", "unknown_closed"}
    /\ phase' = ("queued")
    /\ payload' = (TRUE)
    /\ runnable' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
CancelClear == /\ alive /\ ~journal /\ phase \in {"selected", "claimed", "normalizing", "queued", "prepared"}
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
ChangeSession == /\ sessionMoves = 1
    /\ session' = (IF session = 1 THEN 2 ELSE 0)
    /\ sessionMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
EndSession == /\ sessionMoves = 1
    /\ session' = (0)
    /\ sessionMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
Crash == /\ alive /\ crashes = 1
    /\ alive' = (FALSE)
    /\ crashes' = (0)
    /\ choice' = (IF Mode = "Volatile" THEN 0 ELSE choice)
    /\ phase' = (IF phase \in {"sending", "claimed", "normalizing", "prepared"} THEN "unknown" ELSE phase)
    /\ kept' = (kept \/ phase = "sending")
    /\ \E forever \in BOOLEAN: gone' = (forever)
    /\ UNCHANGED <<first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, bound>>
Boot == /\ ~alive /\ ~gone
    /\ alive' = (TRUE)
    /\ listed' = ("online")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, gone, bound>>
Archive == /\ archiveMoves = 1 /\ view # "none"
    /\ archived' = (TRUE)
    /\ archiveMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, locked, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
ArchiveLocked == /\ archiveMoves = 1 /\ view # "none"
    /\ archived' = (TRUE)
    /\ locked' = (TRUE)
    /\ archiveMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
Unlock == /\ Mode # "NoUnlock" /\ alive /\ locked /\ view # Desired
    /\ locked' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
Unarchive == /\ Mode # "NoUnarchive" /\ alive /\ archived /\ ~locked /\ view # Desired
    /\ archived' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
Render == /\ alive /\ ~archived /\ ~locked /\ view # Desired
    /\ view' = (Desired)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
EarlySuccess == /\ Mode = "EarlySuccess" /\ accepted /\ sent = 0
    /\ view' = ("delivered")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
HandoffDecision == /\ Mode = "BadHandoff" /\ ~accepted /\ writes = 0 /\ session # 0
    /\ phase' = ("sending")
    /\ writes' = (1)
    /\ pending' = (1)
    /\ writeTarget' = (session)
    /\ dispatchSession' = (session)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* A request that names source=decision itself has no approved event. Every entry refuses it;
\* BadProvenance lets it reach the byte-keeping planner that only an approved opinion may use.
SelfDeclared == /\ Mode = "BadProvenance" /\ ~accepted /\ preserved = "none"
    /\ preserved' = ("unapproved")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* The normal byte-keeping path: an approved opinion whose bytes match the approved hash gets its
\* frame. BadPreserveCheck builds one for an approved event of another kind or with other bytes.
PreserveApproved == /\ accepted /\ ~unauthorized /\ preserved = "none"
    /\ preserved' = (IF Mode = "BadPreserveCheck" THEN "altered" ELSE "approved")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* The close is recorded in PG first; whether the owner's kept original left with it is a second
\* step that an unadopted owner or a failed removal skips (CloseUnknown and Finalize above).
\* Before its requeue and notice drain the owner checks each Decision original it still holds
\* against the events PG closed and removes it. NoSweep trusts the close to have removed it;
\* WideSweep clears the channel instead of that one original.
Sweep == /\ Mode # "NoSweep" /\ alive /\ kept /\ (bound \/ Mode # "BindOnlySweep") /\ phase \in {"failed", "unknown_closed", "delivered"}
    /\ kept' = (FALSE)
    /\ otherKept' = (Mode # "WideSweep")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* The resend notice of a kept original. A closed Decision's is never posted: its original leaves
\* first, and one whose removal failed keeps its notice back.
Notice == /\ (Mode = "NoSweep" \/ (Mode = "BindOnlySweep" /\ ~bound)) /\ alive /\ kept /\ ~noticed /\ phase \in {"failed", "unknown_closed", "delivered"}
    /\ noticed' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* Another user's kept original on the same channel still gets its own notice.
OtherNotice == /\ alive /\ otherKept /\ ~otherNoticed
    /\ otherNoticed' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
\* Another node's boot recovery over the shared rows. It ends an attempt only when its holder's
\* run is over; TimeOnlyRecovery is the age-only rule that also takes a live holder's attempt.
PeerClose == /\ phase' = (IF writes > 0 \/ journal THEN "unknown_closed" ELSE "closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ tookLive' = (alive)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound>>
PeerRecover == /\ Mode # "NoRecovery"
    /\ \/ listed = "offline" /\ phase = "unknown"
       \/ (Mode = "TimeOnlyRecovery" \/ tempHeld) /\ alive /\ phase \in {"claimed", "normalizing", "sending"}
    /\ PeerClose
\* The registry lists a dead node offline, and that event settles what it still held: without it a
\* row a peer kept at its own boot, while the node was still listed online, is never looked at again.
MarkOffline == /\ ~alive /\ listed = "online"
    /\ listed' = ("offline")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, gone, bound>>
OfflineSettle == /\ Mode \notin {"NoRecovery", "NoOfflineSettle"} /\ listed = "offline" /\ phase = "unknown"
    /\ PeerClose
\* The node's registered identity is known only after its cluster bootstrap; an attempt moved before
\* that carries a temporary name no peer finds online (TempHolderId).
Bootstrap == /\ alive /\ ~bootstrapped /\ phase = "selected"
    /\ bootstrapped' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, tempHeld, listed, gone, bound>>
\* The owner refuses a kept original whose wait ran out: its bind goes, its refusal notice stays.
RefuseKept == /\ alive /\ kept /\ bound /\ phase \in {"failed", "unknown_closed"}
    /\ bound' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone>>
Progress == Claim \/ ResolvePresent \/ NormalizeAbsent \/ AbsentAfterNormalize \/ NormalizeSuccess \/ NormalizeFailure \/ RefusalNormalize \/ RefusalFinal \/ DrainCurrent \/ Journal \/ BeginSend \/ RemoteEffect \/ Observe \/ Uncertain \/ CloseUnknown \/ CloseAbandoned \/ Finalize \/ Sweep \/ OtherNotice \/ Bootstrap \/ Reconcile
Repair == Unlock \/ Unarchive \/ Render
Next == PostBegin \/ PostFinish \/ PostRejected \/ Activate \/ ThreadMessage \/ MarkOffline \/ OfflineSettle \/ RefuseKept \/ (\E a \in {"operator", "stranger"}, o \in {1,2}: Click(a,o))
        \/ AckReclickOtherUserStop \/ QueueRefused \/ OwnerPending \/ ReleaseHeld \/ PreserveApproved \/ Progress \/ TurnBoundary \/ ChangeSession \/ EndSession
        \/ Crash \/ Boot \/ Archive \/ ArchiveLocked \/ Repair \/ WakeFailed \/ CancelClear \/ EarlySuccess \/ HandoffDecision \/ SelfDeclared \/ PeerRecover \/ Notice
Spec == Init /\ [][Next]_vars /\ WF_vars(Boot) /\ WF_vars(Progress) /\ WF_vars(Repair) /\ WF_vars(TurnBoundary) /\ WF_vars(Sweep) /\ WF_vars(OtherNotice) /\ WF_vars(MarkOffline) /\ WF_vars(OfflineSettle)
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
PreservedNeedsApproval == preserved # "none" => accepted /\ preserved = "approved"
\* Holds only where the normal path is missing: PreservedReach expects it to break.
NeverPreserved == preserved = "none"
JournalBeforeWrite == writes > 0 => journal
LiveHolderKeeps == ~tookLive
NoDecisionNotice == ~noticed
OthersKept == otherKept
\* A node that never returns keeps its own queue file and screen; only the shared rows still settle.
ClosedLeavesNothing == (phase \in {"failed", "unknown_closed"}) ~> (~kept \/ gone)
OthersTold == <>(otherNoticed \/ gone)
AbandonedEnds == (gone /\ phase = "unknown") ~> (phase # "unknown")
OnePost == posts <= 1
NoEarlyChoice == ~early
NoIntakeLeak == ~leaked
StrictDisplayEquality == view = "none" \/ view = Desired
EventuallySettled == accepted ~> ((phase = "delivered" /\ view = "delivered")
    \/ (phase = "failed" /\ view = "failed") \/ (phase = "unknown_closed" /\ view = "unknown") \/ gone)
\* Contrast configs let writes repeat without end; this bound only stops exploring past two.
ContrastBound == sent <= 2 /\ writes <= 2
=============================================================================
