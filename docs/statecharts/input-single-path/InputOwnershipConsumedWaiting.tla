-------------------- MODULE InputOwnershipConsumedWaiting --------------------
\* The F ownership model, narrowed to consumed A, its restart, later native N and waiting B.
\* A's input-notice debt has its own ledger; it never proves unfinished A output.
EXTENDS InputOwnership
CONSTANTS RepairConsumedWaiting, ReadoptOutcomes, OutputOwners,
          RepairBusyPinSupply, RepairOrphanOutcome, PinSources, NativeEnds, AStamps
VARIABLE cw

cwvars == <<vars, cw>>
CWFixed == <<mode, src, natOwner, pend, reader, obsOk, route, plain, sentInto, win, rcv,
             userRow, twin, claim, lp, screen, draft, notices, timer, byTimer, timed,
             heldBy, watch, aged, twinPend, envLeft, stale, old, acct, leak, outside, observer>>

CWInit ==
    /\ Init
    /\ mode = "turn" /\ src = "start" /\ mbx = "idle" /\ nat = "busy"
    /\ old = "none" /\ obsOk /\ ~twin /\ ~twinPend /\ screen = "ok"
    /\ draft = "none" /\ ~stale /\ ~acct /\ row = "none" /\ ~ep /\ ~leak
    /\ \E owner \in OutputOwners, source \in PinSources,
          ending \in NativeEnds, stamp \in AStamps :
         cw = [phase |-> "initial", outputOwner |-> owner,
              generation |-> 0, marker |-> FALSE, markerGeneration |-> 0,
              readopt |-> "pending", token |-> "none", aStarts |-> 0, consumed |-> FALSE,
              terminal |-> FALSE, captured |-> FALSE, nStarted |-> FALSE,
              pendingB |-> FALSE, refusedB |-> FALSE, lateClose |-> FALSE,
              lock |-> "free", close |-> "idle", receipt |-> FALSE,
              receiptOutcome |-> "none", ownStamp |-> stamp,
              nativeSource |-> source, nativeReceipt |-> FALSE, writeSettled |-> FALSE,
              effectReturned |-> FALSE, historySettled |-> FALSE,
              pinSupplier |-> RepairBusyPinSupply, pinAttempted |-> FALSE, pinVerified |-> FALSE,
              nextNativeEnd |-> ending, nAborted |-> FALSE,
              unconfirmed |-> FALSE, attemptsB |-> 0,
              odebt |-> [aOutput |-> 1, inputNotice |-> 20]]

\* execute_owned's busy paste creates A's row; its settled owner identity is not a completion.
OwnerPastesA ==
    /\ cw.phase = "initial"
    /\ row' = "carried" /\ mbx' = "active"
    /\ cw' = [cw EXCEPT !.phase = "pasted", !.token = "A", !.aStarts = 1]
    /\ UNCHANGED <<ep, nat, inp, wake, pastes, op, CWFixed>>
\* The real busy effect returns before history settlement and the existing answer observer.
BusyEffectReturns ==
    /\ cw.phase = "pasted"
    /\ cw' = [cw EXCEPT !.phase = "returned", !.effectReturned = TRUE,
                         !.writeSettled = (cw.nativeSource # "unsettled")]
    /\ UNCHANGED <<row, ep, mbx, nat, inp, wake, pastes, op, CWFixed>>
SettleBusySubmission ==
    /\ cw.phase = "returned"
    /\ cw' = [cw EXCEPT !.phase = "consumed", !.historySettled = TRUE,
                         !.consumed = cw.writeSettled, !.token = "none"]
    /\ mbx' = "idle"
    /\ UNCHANGED <<row, ep, nat, inp, wake, pastes, op, CWFixed>>
ObserveBusyReceipt ==
    /\ cw.phase = "consumed"
    /\ cw' = [cw EXCEPT !.phase = "observed", !.nativeReceipt = TRUE]
    /\ UNCHANGED <<row, ep, mbx, nat, inp, wake, pastes, op, CWFixed>>
\* The observer keeps watching even after an uncertain write; only an armed matching input pins.
SupplyBusyPin ==
    /\ cw.phase = "observed"
    /\ cw' = [cw EXCEPT !.phase = "settled", !.pinAttempted = TRUE,
              !.pinVerified = (RepairBusyPinSupply /\ cw.nativeSource = "matching" /\ cw.writeSettled)]
    /\ UNCHANGED <<row, ep, mbx, nat, inp, wake, pastes, op, CWFixed>>

\* Every shutdown refreshes the marker, including rows outside O's ownership.
ShutdownRestamps ==
    /\ cw.phase \in {"settled", "running"} /\ row = "carried" /\ cw.generation < 2
    /\ cw.close = "idle" /\ cw.token # "BClaim"
    /\ cw' = [cw EXCEPT !.phase = "down", !.marker = TRUE,
              !.markerGeneration = cw.generation]
    /\ UNCHANGED <<row, ep, mbx, nat, inp, wake, pastes, op, CWFixed>>
SettleBoot ==
    /\ cw.phase = "down"
    /\ cw' = [cw EXCEPT !.phase = "restoring", !.generation = @ + 1,
                        !.readopt = "pending", !.token = "none"]
    /\ ep' = FALSE /\ mbx' = "idle"
    /\ UNCHANGED <<row, nat, inp, wake, pastes, op, CWFixed>>
\* pane-alive restore consumes a successful adoption's marker as before. The new refusal hook
\* consumes only O-owned refused markers; it never calls the row readopted.
RestoreAlive ==
    /\ cw.phase = "restoring"
    /\ \E result \in ReadoptOutcomes :
         /\ cw' = [cw EXCEPT !.phase = "running", !.readopt = result,
                    !.token = IF result = "ok" THEN "A" ELSE "none",
                    !.marker = IF result = "ok" \/
                                  (RepairConsumedWaiting /\ cw.outputOwner = "owned")
                               THEN FALSE ELSE @]
         /\ ep' = (result = "ok")
         /\ mbx' = IF result = "ok" THEN "active" ELSE "idle"
    /\ UNCHANGED <<row, nat, inp, wake, pastes, op, CWFixed>>

\* A's first canonical terminator is historical proof even when N later runs in the same pane.
EndA ==
    /\ cw.phase = "running" /\ ~cw.terminal
    /\ cw' = [cw EXCEPT !.terminal = TRUE]
    /\ nat' = "idle"
    /\ UNCHANGED <<row, ep, mbx, inp, wake, pastes, op, CWFixed>>
NativeStartsN ==
    /\ cw.phase = "running" /\ cw.terminal /\ ~cw.nStarted
    /\ cw' = [cw EXCEPT !.nStarted = TRUE]
    /\ nat' = "busy"
    /\ UNCHANGED <<row, ep, mbx, inp, wake, pastes, op, CWFixed>>
CaptureAndDeliverA ==
    /\ cw.phase = "running" /\ cw.outputOwner = "owned" /\ cw.terminal /\ ~cw.captured
    /\ cw' = [cw EXCEPT !.captured = TRUE, !.odebt.aOutput = 0]
    /\ op' = 1
    /\ UNCHANGED <<row, ep, mbx, nat, inp, wake, pastes, CWFixed>>

\* A successfully re-adopted live execution ends only through its existing normal completion.
FinishReadoptedA ==
    /\ cw.phase = "running" /\ row = "carried" /\ ep /\ cw.readopt = "ok"
    /\ cw.terminal /\ cw.captured /\ nat = "idle"
    /\ row' = "none" /\ ep' = FALSE /\ mbx' = "idle" /\ wake' = TRUE
    /\ cw' = [cw EXCEPT !.token = "none", !.receipt = TRUE,
                         !.receiptOutcome = IF cw.ownStamp = "none" THEN "completed" ELSE cw.ownStamp]
    /\ UNCHANGED <<nat, inp, pastes, op, CWFixed>>

CloseProof ==
    /\ cw.phase = "running" /\ cw.outputOwner = "owned"
    /\ row = "carried" /\ ~ep /\ cw.token = "none"
    /\ cw.nStarted /\ cw.consumed /\ cw.terminal /\ cw.captured /\ nat = "idle"
    /\ (~RepairConsumedWaiting \/ cw.pinVerified)
    /\ cw.refusedB /\ cw.lock = "free" /\ ~cw.unconfirmed
    /\ IF RepairConsumedWaiting
       THEN cw.odebt.aOutput = 0
       ELSE cw.odebt.aOutput + cw.odebt.inputNotice = 0
\* The health pass, never foreign admission, takes the existing transition lock. It snapshots
\* A's row before awaiting PG; archive rechecks that same identity and episode under the lock.
CloseATick ==
    /\ CloseProof /\ cw.close = "idle"
    /\ cw' = [cw EXCEPT !.close = "locked", !.lock = "close"]
    /\ UNCHANGED <<row, ep, mbx, nat, inp, wake, pastes, op, CWFixed>>
CommitReceiptA ==
    /\ cw.close = "locked" /\ row = "carried" /\ cw.token = "none"
    /\ cw' = [cw EXCEPT !.receipt = TRUE, !.close = "committed",
              !.receiptOutcome = IF cw.ownStamp # "none" THEN cw.ownStamp
                                 ELSE IF RepairOrphanOutcome THEN "completed"
                                 ELSE IF cw.nAborted THEN "cancelled" ELSE "completed"]
    /\ UNCHANGED <<row, ep, mbx, nat, inp, wake, pastes, op, CWFixed>>
ArchiveA ==
    /\ cw.close = "committed" /\ cw.receipt /\ row = "carried"
    /\ cw.token = "none" /\ ~ep /\ nat = "idle" /\ ~cw.unconfirmed
    /\ row' = "none"
    /\ cw' = [cw EXCEPT !.close = "archived", !.lock = "free"]
    /\ UNCHANGED <<ep, mbx, nat, inp, wake, pastes, op, CWFixed>>
KickoffB ==
    /\ cw.close = "archived"
    /\ cw' = [cw EXCEPT !.close = "done"]
    /\ wake' = TRUE
    /\ UNCHANGED <<row, ep, mbx, nat, inp, pastes, op, CWFixed>>

AdmitB ==
    /\ cw.phase = "running" /\ cw.nStarted /\ nat = "busy" /\ ~cw.pendingB /\ pastes = 0
    /\ cw' = [cw EXCEPT !.pendingB = TRUE]
    /\ wake' = TRUE
    /\ UNCHANGED <<row, ep, mbx, nat, inp, pastes, op, CWFixed>>
\* B claims its own token before create. A's closer must not retire anything while that token
\* holds the mailbox. Arrival and N's one idle edge provide at most two pre-close attempts.
ClaimB ==
    /\ cw.phase = "running" /\ cw.pendingB /\ row = "carried"
    /\ cw.token = "none" /\ cw.lock = "free" /\ wake /\ cw.attemptsB < 2
    /\ cw' = [cw EXCEPT !.token = "BClaim", !.attemptsB = @ + 1]
    /\ mbx' = "active" /\ wake' = FALSE
    /\ UNCHANGED <<row, ep, nat, inp, pastes, op, CWFixed>>
\* A collision releases B's claim and preserves its front. There is no closer call here.
ForeignAdmission ==
    /\ cw.phase = "running" /\ cw.pendingB /\ row = "carried" /\ cw.token = "BClaim"
    /\ cw' = [cw EXCEPT !.refusedB = TRUE, !.token = "none"]
    /\ mbx' = "idle"
    /\ UNCHANGED <<row, ep, nat, inp, wake, pastes, op, CWFixed>>
NativeIdle ==
    /\ cw.phase = "running" /\ cw.nStarted /\ nat = "busy"
    /\ cw.pendingB /\ (cw.refusedB \/ cw.readopt = "ok")
    /\ nat' = "idle" /\ wake' = (wake \/ cw.pendingB)
    /\ cw' = [cw EXCEPT !.nAborted = (cw.nextNativeEnd = "aborted")]
    /\ UNCHANGED <<row, ep, mbx, inp, pastes, op, CWFixed>>
StartB ==
    /\ cw.phase = "running" /\ cw.pendingB /\ row = "none" /\ mbx = "idle"
    /\ nat = "idle" /\ wake /\ pastes = 0 /\ cw.lock = "free"
    /\ row' = "B" /\ mbx' = "active" /\ pastes' = pastes + 1 /\ inp' = "sent"
    /\ wake' = FALSE
    /\ cw' = [cw EXCEPT !.pendingB = FALSE, !.token = "B", !.phase = "done"]
    /\ UNCHANGED <<ep, nat, op, CWFixed>>
\* An old result arriving after B's row and token can no longer name A's durable episode.
LateCloseA ==
    /\ cw.phase = "done" /\ ~cw.lateClose
    /\ cw' = [cw EXCEPT !.lateClose = TRUE]
    /\ UNCHANGED vars
\* A normal no-op health tick keeps a row whose output gate does not pass. It makes the
\* permanent wait a temporal lasso rather than mistaking an idle process for a deadlock.
HealthKeepsA ==
    /\ cw.phase = "running" /\ row = "carried" /\ nat = "idle"
    /\ cw.pendingB /\ cw.token = "none" /\ ~CloseProof
    /\ UNCHANGED cwvars
CWDone == cw.phase = "done" /\ UNCHANGED cwvars

CWNext == OwnerPastesA \/ BusyEffectReturns \/ SettleBusySubmission
          \/ ObserveBusyReceipt \/ SupplyBusyPin
          \/ ShutdownRestamps \/ SettleBoot \/ RestoreAlive
          \/ EndA \/ NativeStartsN \/ CaptureAndDeliverA \/ FinishReadoptedA
          \/ CloseATick \/ CommitReceiptA \/ ArchiveA \/ KickoffB
          \/ AdmitB \/ ClaimB \/ ForeignAdmission \/ NativeIdle
          \/ StartB \/ LateCloseA \/ HealthKeepsA \/ CWDone
CWSpec == CWInit /\ [][CWNext]_cwvars
          /\ WF_cwvars(OwnerPastesA) /\ WF_cwvars(BusyEffectReturns)
          /\ WF_cwvars(SettleBusySubmission) /\ WF_cwvars(ObserveBusyReceipt)
          /\ WF_cwvars(SupplyBusyPin) /\ WF_cwvars(ShutdownRestamps)
          /\ WF_cwvars(SettleBoot) /\ WF_cwvars(RestoreAlive) /\ WF_cwvars(EndA)
          /\ WF_cwvars(NativeStartsN) /\ WF_cwvars(CaptureAndDeliverA)
          /\ WF_cwvars(FinishReadoptedA) /\ WF_cwvars(CloseATick) /\ WF_cwvars(AdmitB)
          /\ WF_cwvars(CommitReceiptA) /\ WF_cwvars(ArchiveA) /\ WF_cwvars(KickoffB)
          /\ WF_cwvars(ClaimB) /\ WF_cwvars(ForeignAdmission)
          /\ WF_cwvars(NativeIdle) /\ WF_cwvars(LateCloseA) /\ WF_cwvars(StartB)

PendingB == cw.pendingB
BStarted == pastes = 1
PendingBEventuallyStarts == PendingB ~> BStarted
NoDuplicateStart == pastes <= 1
NoAReplay == cw.aStarts <= 1
SuccessorUntouched == pastes = 1 => row = "B" /\ cw.token = "B" /\ mbx = "active"
NoDuplicateAnswer == op <= 1 /\ lp = 0
InputNoticeDebtIndependent == cw.odebt.inputNotice = 20
RefusedMarkerConsumed == (RepairConsumedWaiting /\ cw.outputOwner = "owned"
                          /\ cw.phase = "running"
                          /\ cw.readopt = "refused") => ~cw.marker
RefusedMarkerPreservedOutsideO == (cw.outputOwner # "owned" /\ cw.phase = "running"
                                   /\ cw.readopt = "refused") => cw.marker
FreshShutdownMarker == cw.phase = "down" => cw.marker /\ cw.markerGeneration = cw.generation
ReceiptBeforeArchive == cw.close \in {"archived", "done"} => cw.receipt
NoCloseUnderBClaim == cw.token = "BClaim" => row = "carried" /\ cw.close = "idle"
PinEvidenceVerified == cw.pinVerified => cw.nativeReceipt /\ cw.writeSettled /\ cw.consumed
                                        /\ cw.nativeSource = "matching"
ObserverFollowsEffect == cw.nativeReceipt => cw.effectReturned /\ cw.historySettled
NoCloseWithoutPin == (RepairConsumedWaiting /\ cw.close # "idle") => cw.pinVerified
OrphanAReceiptCompleted == (cw.receipt /\ cw.readopt = "refused" /\ cw.ownStamp = "none")
                           => cw.receiptOutcome = "completed"
OwnStampRespected == (cw.receipt /\ cw.ownStamp # "none") => cw.receiptOutcome = cw.ownStamp
=============================================================================
