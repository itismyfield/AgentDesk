------------------------- MODULE DecisionRequest -------------------------
EXTENDS Naturals, TLC
CONSTANT Mode
VARIABLES phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened
vars == <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
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
    /\ known = FALSE
    /\ inflight = FALSE
    /\ stored = 0
    /\ late = FALSE
    /\ wired \in BOOLEAN
    /\ fresh = FALSE
    /\ ready = TRUE
    /\ burned = FALSE
    /\ reopened = FALSE
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
\* The journal: this run alone may POST, and its row names the forum in PG. `known` is a receiver's
\* memory of the forum; SnapshotRegister and OverrideUnshared may never learn it.
PostBegin == /\ alive /\ phase = "new"
    /\ phase' = ("posting")
    /\ inflight' = (TRUE)
    /\ \E reached \in BOOLEAN: known' = (IF Mode \in {"SnapshotRegister", "OverrideUnshared"} THEN reached ELSE known)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, stored, late, wired, fresh, ready, burned, reopened>>
\* Only the run that wrote the journal posts the starter, once, buttons disabled. A lost answer is
\* settled by finding that post (RePost sends another); a boot never posts (BootRePost does).
PostFinish == /\ alive /\ phase = "posting" /\ Mode # "PostFailure"
    /\ \/ inflight /\ posts = 0
       \/ Mode = "BootRePost" /\ ~inflight /\ posts < 2
    /\ posts' = (posts + 1)
    /\ inflight' = (FALSE)
    /\ \E heard \in BOOLEAN:
        /\ phase' = (IF heard THEN "disabled" ELSE "posting")
        /\ view' = (IF heard THEN "not_yet" ELSE view)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, stored, late, wired, fresh, ready, burned, reopened>>
PostRejected == /\ Mode = "PostFailure" /\ alive /\ phase = "posting"
    /\ phase' = ("post_failed")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Reconcile == /\ alive /\ phase = "posting" /\ posts > 0
    /\ posts' = (IF Mode = "RePost" THEN posts + 1 ELSE posts)
    /\ phase' = ("disabled")
    /\ view' = ("not_yet")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The buttons are enabled only for a target kind whose selection handling and normalization are
\* wired; any other keeps the disabled post. ActivateUnwired enables them regardless.
Activate == /\ alive /\ phase = "disabled" /\ (wired \/ Mode = "ActivateUnwired")
    /\ phase' = ("open")
    /\ view' = ("open")
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* A Decision thread's message never reaches ordinary intake. The guard matches the forum (GuardByThread
\* only stored threads); on a miss Fixed asks PG, whose journal row names it. OverrideUnshared asks only
\* the forum cache, which a configured forum never enters; the others trust memory.
ThreadMessage == /\ posts > 0 /\ ~leaked
    /\ \/ Mode = "NoIntakeGuard"
       \/ Mode = "GuardByThread" /\ phase = "posting"
       \/ Mode \in {"TtlGuard", "SnapshotRegister", "OverrideUnshared"} /\ ~known
    /\ leaked' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The periodic full read of ready forums. An older read that lands late replaces the set; when the
\* registration lives in that set it is lost (SnapshotRegister). Fixed keeps registrations apart.
Refresh == /\ Mode \in {"TtlGuard", "SnapshotRegister"} /\ phase # "new" /\ ~known
    /\ known' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, inflight, stored, late, wired, fresh, ready, burned, reopened>>
StaleRefresh == /\ Mode = "SnapshotRegister" /\ known
    /\ known' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Click(actor, option) == /\ alive /\ (phase = "open" \/ (Mode = "EarlyButtons" /\ phase = "disabled")) /\ (actor = "operator" \/ Mode = "BadAuth")
    /\ choice' = (IF (Mode = "OldLiveCheck" /\ session # 1) \/ Mode = "RunBeforeStore" THEN 0 ELSE option)
    /\ stored' = (IF Mode = "RunBeforeStore" THEN stored ELSE stored + 1)
    /\ fresh' = (TRUE)
    /\ first' = (option)
    /\ accepted' = (TRUE)
    /\ phase' = ("selected")
    /\ unauthorized' = (actor # "operator")
    /\ early' = (phase = "disabled")
    /\ UNCHANGED <<payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, late, wired, ready, burned, reopened>>
AckReclickOtherUserStop == UNCHANGED vars
\* The choice row is one per request: a second click of the same option only reads it back and
\* another option is refused (AckReclickOtherUserStop). NoChoiceCas stores the later click too;
\* LateChoice still stores one after the selection's delivery ended and the thread was closed.
Reclick(option) == /\ alive /\ accepted /\ stored < 2
    /\ \/ Mode = "NoChoiceCas" /\ phase \notin {"delivered", "failed", "unknown_closed"}
       \/ Mode = "LateChoice" /\ phase \in {"delivered", "failed", "unknown_closed"}
    /\ choice' = (option)
    /\ stored' = (stored + 1)
    /\ late' = (phase \in {"delivered", "failed", "unknown_closed"})
    /\ UNCHANGED <<phase, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, wired, fresh, ready, burned, reopened>>
\* RunBeforeStore answers the click and lets the delivery start before the choice is durable.
StoreLate == /\ Mode = "RunBeforeStore" /\ alive /\ accepted /\ choice = 0
    /\ choice' = (first)
    /\ stored' = (stored + 1)
    /\ UNCHANGED <<phase, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, late, wired, fresh, ready, burned, reopened>>
\* A boot settles a write whose result is unknown from its evidence; BootResend sends it again.
BootResend == /\ Mode = "BootResend" /\ alive /\ crashes = 0 /\ phase = "unknown" /\ writes = 1 /\ session # 0
    /\ phase' = ("sending")
    /\ writes' = (writes + 1)
    /\ pending' = (pending + 1)
    /\ writeTarget' = (session)
    /\ dispatchSession' = (session)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* After a boot the process can start a stored selection only once its gateway is ready. A pass that
\* runs earlier leaves the event unstarted; EarlyBootStart claims it, finds no runtime and closes it.
GatewayReady == /\ alive /\ ~ready
    /\ ready' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, burned, reopened>>
EarlyStart == /\ Mode = "EarlyBootStart" /\ alive /\ ~ready /\ ~closed /\ bootstrapped /\ phase = "selected" /\ ~begun
    /\ phase' = ("closing")
    /\ begun' = (TRUE)
    /\ claims' = (claims + 1)
    /\ norms' = (1)
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ fresh' = (FALSE)
    /\ burned' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, payload, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, ready, reopened>>
\* Every card edit of a request is ordered by its lifecycle read under the request's lock. StaleActivate
\* lets a second ask's older "open" edit land after the card showed the selection's end.
StaleShow == /\ Mode = "StaleActivate" /\ alive /\ ~reopened /\ ~(archived /\ locked)
    /\ phase \in {"delivered", "failed", "unknown_closed"} /\ view = Desired
    /\ view' = ("open")
    /\ reopened' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned>>
\* Once the selection's delivery ended and its result is shown, the thread is archived and locked.
Seal == /\ Mode # "NoSeal" /\ alive /\ phase \in {"delivered", "failed", "unknown_closed"} /\ view = Desired /\ ~(archived /\ locked)
    /\ archived' = (TRUE)
    /\ locked' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The not_started -> resolving update is the only start. NoClaimCas drops its state condition.
Claim == /\ alive /\ ready /\ ~closed /\ (bootstrapped \/ Mode = "TempHolderId") /\ (fresh \/ Mode # "NoBootStart")
    /\ \/ phase = "selected" /\ ~begun
       \/ Mode = "NoClaimCas" /\ phase \in {"claimed", "queued", "prepared"} /\ claims < 2
    /\ phase' = ("claimed")
    /\ begun' = (TRUE)
    /\ claims' = (claims + 1)
    /\ tempHeld' = (~bootstrapped)
    /\ fresh' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, listed, gone, bound, known, inflight, stored, late, wired, ready, burned, reopened>>
ResolvePresent == /\ alive /\ phase = "claimed" /\ session # 0 /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = (IF busy THEN "queued" ELSE "prepared")
    /\ resolved' = (IF busy THEN 0 ELSE session)
    /\ payload' = (busy)
    /\ runnable' = (busy)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
NormalizeAbsent == /\ alive /\ phase = "claimed" /\ session = 0 /\ norms = 0 /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = ("normalizing")
    /\ norms' = (1)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
AbsentAfterNormalize == /\ alive /\ phase = "claimed" /\ session = 0 /\ norms = 1
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
NormalizeSuccess == /\ alive /\ phase = "normalizing"
    /\ phase' = ("claimed")
    /\ session' = (IF session = 0 THEN 2 ELSE session)
    /\ busy' = (FALSE)
    /\ queueFault' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, resolved, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
NormalizeFailure == /\ alive /\ phase = "normalizing"
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
RefusalNormalize == /\ alive /\ (phase = "prepared" \/ (phase = "queued" /\ queueFault)) /\ norms = 0 /\ writes = 0 /\ ~closed
    /\ phase' = ("normalizing")
    /\ norms' = (1)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ resolved' = (0)
    /\ journal' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
RefusalFinal == /\ alive /\ (phase = "prepared" \/ (phase = "queued" /\ queueFault)) /\ norms = 1 /\ writes = 0 /\ ~closed
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ journal' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
QueueRefused == /\ alive /\ phase = "queued" /\ ~queueFault
    /\ queueFault' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The owner's admission answered pending: the original may sit in its queue. The fence closes the
\* event first and Finalize removes that exact original; PendingLeft is the close without removal.
OwnerPending == /\ alive /\ phase = "claimed" /\ session # 0 /\ busy /\ ~closed
    /\ phase' = (IF Mode = "PendingLeft" THEN "failed" ELSE "closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ payload' = (TRUE)
    /\ runnable' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
TurnBoundary == /\ Mode # "NoTurnBoundary" /\ busy
    /\ busy' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
DrainCurrent == /\ alive /\ phase = "queued" /\ ~busy /\ (~closed \/ Mode = "RetryClosed")
    /\ phase' = ("claimed")
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ resolved' = (0)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The may_have_sent journal: one PG update before any key. A closed or written event never gets it.
Journal == /\ alive /\ phase = "prepared" /\ ~journal /\ (~closed \/ Mode = "RetryClosed") /\ (writes = 0 \/ Mode = "RetryClosed")
    /\ journal' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The input box was in use and the owner proved no key was sent: the same original waits again.
ReleaseHeld == /\ alive /\ phase = "prepared" /\ journal /\ holds = 0 /\ writes = 0 /\ ~closed
    /\ phase' = ("queued")
    /\ journal' = (FALSE)
    /\ holds' = (1)
    /\ payload' = (TRUE)
    /\ runnable' = (TRUE)
    /\ resolved' = (0)
    /\ busy' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
BeginSend == /\ alive /\ phase = "prepared" /\ (journal \/ Mode = "NoJournal") /\ (~closed \/ Mode = "RetryClosed") /\ (writes = 0 \/ Mode = "RetryClosed") /\ session # 0 /\ (resolved = session \/ Mode = "BadFence")
    /\ phase' = ("sending")
    /\ writes' = (writes + 1)
    /\ pending' = (pending + 1)
    /\ writeTarget' = (resolved)
    /\ dispatchSession' = (session)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
RemoteEffect == /\ pending > 0 /\ sent < 2
    /\ pending' = (pending - 1)
    /\ sent' = (sent + 1)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, writes, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Observe == /\ alive /\ phase \in {"sending", "unknown", "unknown_closed"} /\ sent > 0
    /\ phase' = ("delivered")
    /\ receipt' = (TRUE)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Uncertain == /\ alive /\ phase = "sending"
    /\ phase' = ("unknown")
    /\ kept' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
CloseUnknown == /\ Mode # "NoRecovery" /\ alive /\ phase = "unknown" /\ (writes > 0 \/ journal)
    /\ phase' = ("unknown_closed")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ payload' = (FALSE)
    /\ runnable' = (FALSE)
    /\ \E done \in BOOLEAN: kept' = (kept /\ ~done)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
CloseAbandoned == /\ Mode # "NoRecovery" /\ alive /\ phase = "unknown" /\ writes = 0 /\ ~journal
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Finalize == /\ alive /\ phase = "closing"
    /\ phase' = ("failed")
    /\ payload' = (Mode = "NoDiscard")
    /\ runnable' = (Mode = "NoDiscard")
    /\ \E done \in BOOLEAN: kept' = (kept \/ (payload /\ ~done))
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
WakeFailed == /\ Mode = "RetryClosed" /\ alive /\ phase \in {"failed", "unknown_closed"}
    /\ phase' = ("queued")
    /\ payload' = (TRUE)
    /\ runnable' = (TRUE)
    /\ UNCHANGED <<choice, first, accepted, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
CancelClear == /\ alive /\ ~journal /\ phase \in {"selected", "claimed", "normalizing", "queued", "prepared"}
    /\ phase' = ("closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
ChangeSession == /\ sessionMoves = 1
    /\ session' = (IF session = 1 THEN 2 ELSE 0)
    /\ sessionMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
EndSession == /\ sessionMoves = 1
    /\ session' = (0)
    /\ sessionMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Crash == /\ alive /\ crashes = 1
    /\ alive' = (FALSE)
    /\ crashes' = (0)
    /\ choice' = (IF Mode = "Volatile" THEN 0 ELSE choice)
    /\ phase' = (IF phase \in {"sending", "claimed", "normalizing", "prepared"} THEN "unknown" ELSE phase)
    /\ kept' = (kept \/ phase = "sending")
    /\ gone' = (Mode \in {"Gone", "NoOfflineSettle"})
    /\ known' = (FALSE)
    /\ inflight' = (FALSE)
    /\ fresh' = (FALSE)
    /\ UNCHANGED <<first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, bound, stored, late, wired, ready, burned, reopened>>
Boot == /\ ~alive /\ ~gone
    /\ alive' = (TRUE)
    /\ listed' = ("online")
    /\ ready' = (~(phase = "selected" /\ ~begun))
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, gone, bound, known, inflight, stored, late, wired, fresh, burned, reopened>>
Archive == /\ archiveMoves = 1 /\ view # "none" /\ phase # "disabled"
    /\ archived' = (TRUE)
    /\ archiveMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, locked, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
ArchiveLocked == /\ archiveMoves = 1 /\ view # "none" /\ phase # "disabled"
    /\ archived' = (TRUE)
    /\ locked' = (TRUE)
    /\ archiveMoves' = (0)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Unlock == /\ Mode # "NoUnlock" /\ alive /\ locked /\ view # Desired
    /\ locked' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Unarchive == /\ Mode # "NoUnarchive" /\ alive /\ archived /\ ~locked /\ view # Desired
    /\ archived' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Render == /\ alive /\ ~archived /\ ~locked /\ view # Desired
    /\ view' = (Desired)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
EarlySuccess == /\ Mode = "EarlySuccess" /\ accepted /\ sent = 0
    /\ view' = ("delivered")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
HandoffDecision == /\ Mode = "BadHandoff" /\ ~accepted /\ writes = 0 /\ session # 0
    /\ phase' = ("sending")
    /\ writes' = (1)
    /\ pending' = (1)
    /\ writeTarget' = (session)
    /\ dispatchSession' = (session)
    /\ UNCHANGED <<choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* A request that names source=decision itself has no approved event. Every entry refuses it;
\* BadProvenance lets it reach the byte-keeping planner that only an approved opinion may use.
SelfDeclared == /\ Mode = "BadProvenance" /\ ~accepted /\ preserved = "none"
    /\ preserved' = ("unapproved")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The normal byte-keeping path: an approved opinion whose bytes match the approved hash gets its
\* frame. BadPreserveCheck builds one for an approved event of another kind or with other bytes.
PreserveApproved == /\ accepted /\ ~unauthorized /\ preserved = "none"
    /\ preserved' = (IF Mode = "BadPreserveCheck" THEN "altered" ELSE "approved")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The close is recorded in PG first; whether the owner's kept original left with it is a second
\* step that an unadopted owner or a failed removal skips (CloseUnknown and Finalize above).
\* Before its requeue and notice drain the owner checks each Decision original it still holds
\* against the events PG closed and removes it. NoSweep trusts the close to have removed it;
\* WideSweep clears the channel instead of that one original.
Sweep == /\ Mode # "NoSweep" /\ alive /\ kept /\ (bound \/ Mode # "BindOnlySweep") /\ phase \in {"failed", "unknown_closed", "delivered"}
    /\ kept' = (FALSE)
    /\ otherKept' = (Mode # "WideSweep")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, noticed, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The resend notice of a kept original. A closed Decision's is never posted: its original leaves
\* first, and one whose removal failed keeps its notice back.
Notice == /\ (Mode = "NoSweep" \/ (Mode = "BindOnlySweep" /\ ~bound)) /\ alive /\ kept /\ ~noticed /\ phase \in {"failed", "unknown_closed", "delivered"}
    /\ noticed' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* Another user's kept original on the same channel still gets its own notice.
OtherNotice == /\ alive /\ otherKept /\ ~otherNoticed
    /\ otherNoticed' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* Another node's boot recovery over the shared rows. It ends an attempt only when its holder's
\* run is over; TimeOnlyRecovery is the age-only rule that also takes a live holder's attempt.
PeerClose == /\ phase' = (IF writes > 0 \/ journal THEN "unknown_closed" ELSE "closing")
    /\ closed' = (TRUE)
    /\ atClose' = (writes)
    /\ runnable' = (FALSE)
    /\ tookLive' = (alive)
    /\ UNCHANGED <<choice, first, accepted, payload, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, unauthorized, queueFault, preserved, journal, holds, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
PeerRecover == /\ Mode # "NoRecovery"
    /\ \/ listed = "offline" /\ phase = "unknown"
       \/ (Mode = "TimeOnlyRecovery" \/ tempHeld) /\ alive /\ phase \in {"claimed", "normalizing", "sending"}
    /\ PeerClose
\* The registry lists a dead node offline, and that event settles what it still held: without it a
\* row a peer kept at its own boot, while the node was still listed online, is never looked at again.
MarkOffline == /\ ~alive /\ listed = "online" /\ phase = "unknown"
    /\ listed' = ("offline")
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
OfflineSettle == /\ Mode \notin {"NoRecovery", "NoOfflineSettle"} /\ listed = "offline" /\ phase = "unknown"
    /\ PeerClose
\* The node's registered identity is known only after its cluster bootstrap; an attempt moved before
\* that carries a temporary name no peer finds online (TempHolderId).
Bootstrap == /\ alive /\ ~bootstrapped /\ phase = "selected"
    /\ bootstrapped' = (TRUE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, tempHeld, listed, gone, bound, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
\* The owner refuses a kept original whose wait ran out: its bind goes, its refusal notice stays.
RefuseKept == /\ Mode = "BindOnlySweep" /\ alive /\ kept /\ bound /\ phase \in {"failed", "unknown_closed"}
    /\ bound' = (FALSE)
    /\ UNCHANGED <<phase, choice, first, accepted, payload, runnable, begun, claims, norms, session, resolved, busy, sent, writes, pending, writeTarget, dispatchSession, receipt, alive, crashes, sessionMoves, archived, locked, archiveMoves, view, closed, atClose, unauthorized, queueFault, preserved, journal, holds, tookLive, kept, noticed, otherKept, otherNoticed, posts, early, leaked, bootstrapped, tempHeld, listed, gone, known, inflight, stored, late, wired, fresh, ready, burned, reopened>>
Progress == Claim \/ ResolvePresent \/ NormalizeAbsent \/ AbsentAfterNormalize \/ NormalizeSuccess \/ NormalizeFailure \/ RefusalNormalize \/ RefusalFinal \/ DrainCurrent \/ Journal \/ BeginSend \/ RemoteEffect \/ Observe \/ Uncertain \/ CloseUnknown \/ CloseAbandoned \/ Finalize \/ Sweep \/ OtherNotice \/ Bootstrap \/ Reconcile \/ GatewayReady
Repair == Unlock \/ Unarchive \/ Render \/ Seal
Next == PostBegin \/ PostFinish \/ PostRejected \/ Refresh \/ StaleRefresh \/ Activate \/ ThreadMessage \/ MarkOffline \/ OfflineSettle \/ RefuseKept \/ (\E a \in {"operator", "stranger"}, o \in {1,2}: Click(a,o))
        \/ AckReclickOtherUserStop \/ QueueRefused \/ OwnerPending \/ ReleaseHeld \/ PreserveApproved \/ Progress \/ TurnBoundary \/ ChangeSession \/ EndSession
        \/ Crash \/ Boot \/ Archive \/ ArchiveLocked \/ Repair \/ WakeFailed \/ CancelClear \/ EarlySuccess \/ HandoffDecision \/ SelfDeclared \/ PeerRecover \/ Notice \/ StoreLate \/ BootResend \/ (\E o \in {1,2}: Reclick(o)) \/ EarlyStart \/ StaleShow
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
\* A node that never returns (Gone) keeps its own queue file and screen; only the shared rows settle.
ClosedLeavesNothing == (phase \in {"failed", "unknown_closed"}) ~> (~kept \/ gone)
OthersTold == <>(otherNoticed \/ gone)
AbandonedEnds == (gone /\ phase = "unknown") ~> (phase # "unknown")
OnePost == posts <= 1
NoEarlyChoice == ~early
NoIntakeLeak == ~leaked
OneStoredChoice == stored <= 1
StoredBeforeRun == (begun \/ writes > 0) => (stored = 1 /\ choice # 0)
NoChoiceAfterEnd == ~late
NoUnwiredButtons == (phase = "open" \/ view = "open" \/ accepted) => wired
EndedSealed == accepted ~> ((phase \in {"delivered", "failed", "unknown_closed"} /\ archived /\ locked) \/ gone)
ReadyBeforeStart == ~burned
EndedCardStaysEnded == ~reopened
StoredStarts == (accepted /\ ~begun) ~> (begun \/ closed \/ gone)
StrictDisplayEquality == view = "none" \/ view = Desired
EventuallySettled == accepted ~> ((phase = "delivered" /\ view = "delivered")
    \/ (phase = "failed" /\ view = "failed") \/ (phase = "unknown_closed" /\ view = "unknown") \/ gone)
\* Contrast configs let writes repeat without end; this bound only stops exploring past two.
ContrastBound == sent <= 2 /\ writes <= 2
=============================================================================
