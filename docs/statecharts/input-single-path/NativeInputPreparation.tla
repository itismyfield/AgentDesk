-------------------- MODULE NativeInputPreparation --------------------
EXTENDS Naturals
CONSTANT Mode
VARIABLES phase, role, pin, prepared, anyKey, original, result, source, restored, admitted, review
BaseVars == <<phase, role, pin, prepared, anyKey, original, result, source, restored, admitted>>
vars == <<BaseVars, review>>
Init == /\ phase = "Intent" /\ role \in {"Primary", "Follower"}
        /\ pin = "None" /\ prepared = FALSE /\ anyKey = FALSE /\ original = TRUE
        /\ result = "None" /\ source = "PreSource" /\ restored = FALSE /\ admitted = FALSE
        /\ review \in [pc:{0}, kind:{"Taken","Attached"}, member:{"None"}, firstProof:{0}, proofs:{{}}, fifo:{TRUE}, taken:{FALSE}, pending:{TRUE}, receipts:{IF Mode \in {"StampBefore","StampFixed"} THEN {} ELSE {"two"}}, version:{0}, readVersion:{0}, stored:{"None"}, prefixStep : {0}, prefixReads : {0}, seed : {"Absent"}, prefixHash : {Mode \notin {"PrefixHashBefore", "PrefixHashFixed", "SupplyPrefixBefore", "SupplyPrefixFixed"}}, prefixFifo : {Mode \notin {"PrefixFifoBefore", "PrefixFifoFixed"}}, prefixLease : {1}, clearStep : {0}, clearOwner : (IF Mode \in {"ClearOwnerBefore", "ClearOwnerFixed"} THEN {FALSE, TRUE} ELSE {Mode \in {"ClearRouteBefore", "ClearRouteFixed"}}), clearDenied : {Mode \notin {"ClearRouteBefore", "ClearRouteFixed"}}, clearReadOnly : {Mode \in {"ClearReadOnlyBefore", "ClearReadOnlyFixed", "ClearRouteBefore", "ClearRouteFixed"}}, clearLease : {1}, clearRows : {1}, clearSignal : {FALSE}, clearIntents : {0}, clearHead : {FALSE}, supplyTicket : {Mode \notin {"SupplyTicketBefore", "SupplyTicketFixed"}}, supplySource : {Mode \notin {"SupplySourceBefore", "SupplySourceFixed"}}, supplyKey : {Mode \notin {"SupplyKeyBefore", "SupplyKeyFixed"}}, supplyStamp : {Mode \notin {"SupplyStampBefore", "SupplyStampFixed"}}]
Prepare == /\ phase = "Intent"
           /\ phase' = "Prepared" /\ prepared' = TRUE
           /\ pin' = IF role = "Primary" \/ Mode = "BorrowBusy" THEN "Primary" ELSE "None"
           /\ UNCHANGED <<role, anyKey, original, result, source, restored, admitted, review>>
AnyKey == /\ (phase = "Prepared" \/ (Mode = "KeyBeforePrepare" /\ phase = "Intent"))
          /\ phase' = "Unknown" /\ anyKey' = TRUE
          /\ UNCHANGED <<role, pin, prepared, original, result, source, restored, admitted, review>>
NotSent == /\ phase \in {"Intent", "Prepared"} /\ ~anyKey
           /\ phase' = "NotSent" /\ original' = (Mode # "DropNotSent")
           /\ UNCHANGED <<role, pin, prepared, anyKey, result, source, restored, admitted, review>>
RuntimeReady == /\ source = "PreSource" /\ source' = "Bound"
                /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, restored, admitted, review>>
Restart == /\ phase \in {"Prepared", "Unknown", "NotSent"} /\ ~restored
           /\ restored' = TRUE
           /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, source, admitted, review>>
Done == /\ phase \in {"Unknown", "NotSent"} /\ source = "Bound" /\ restored
        /\ UNCHANGED vars
AcceptRow(row) == Mode = "ReceiptBefore" \/ row \in review.receipts
JoinPending == Mode # "OpenBefore" /\ review.pending /\ review.kind \in {"Taken","Attached"}
ReceiptOne == /\ Mode \in {"ReceiptBefore","ReceiptFixed"} /\ review.pc = 0
              /\ review' = [review EXCEPT !.pc = 1, !.member = IF AcceptRow("one") THEN "one" ELSE review.member]
              /\ UNCHANGED BaseVars
ReceiptCloseOne == /\ Mode \in {"ReceiptBefore","ReceiptFixed"} /\ review.pc = 1
                   /\ review' = [review EXCEPT !.pc = 2, !.firstProof = IF review.member = "one" THEN 1 ELSE 0,
                        !.proofs = IF review.member = "one" THEN {"one"} ELSE {}]
                   /\ UNCHANGED BaseVars
ReceiptTwo == /\ Mode \in {"ReceiptBefore","ReceiptFixed"} /\ review.pc = 2
              /\ review' = [review EXCEPT !.pc = 3,
                   !.member = IF review.member = "None" /\ AcceptRow("two") THEN "two" ELSE review.member,
                   !.proofs = IF review.member = "None" /\ AcceptRow("two") THEN {"two"} ELSE {}]
              /\ UNCHANGED BaseVars
PopAll == /\ Mode \in {"PopAllBefore","PopAllFixed"} /\ review.pc = 0
          /\ review' = [review EXCEPT !.pc = 1, !.fifo = IF Mode # "PopAllBefore" THEN FALSE ELSE review.fifo]
          /\ UNCHANGED BaseVars
SecondDequeue == /\ Mode \in {"PopAllBefore","PopAllFixed"} /\ review.pc = 1
                 /\ review' = [review EXCEPT !.pc = 2, !.taken = (IF review.fifo THEN "one" ELSE "two") = "two"]
                 /\ UNCHANGED BaseVars
SecondClose == /\ Mode \in {"PopAllBefore","PopAllFixed"} /\ review.pc = 2
               /\ review' = [review EXCEPT !.pc = 3, !.proofs = IF review.taken THEN {"two"} ELSE {}]
               /\ UNCHANGED BaseVars
PendingOpen == /\ Mode \in {"OpenBefore","OpenFixed"} /\ review.pc = 0
               /\ review' = [review EXCEPT !.pc = 1, !.member = IF JoinPending /\ AcceptRow("two") THEN "two" ELSE review.member]
               /\ UNCHANGED BaseVars
PendingClose == /\ Mode \in {"OpenBefore","OpenFixed"} /\ review.pc = 1
                /\ review' = [review EXCEPT !.pc = 2, !.proofs = IF review.member = "two" THEN {"two"} ELSE {}]
                /\ UNCHANGED BaseVars
NativeTypedAdmission == /\ ~admitted
                        /\ (Mode = "UntypedAdmission" \/ (prepared /\ anyKey /\ source = "Bound"))
                        /\ admitted' = TRUE
                        /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, source, restored, review>>
StampRead == /\ Mode \in {"StampBefore","StampFixed"} /\ review.pc = 0
             /\ review' = [review EXCEPT !.pc = 1, !.readVersion = review.version, !.member = "one"]
             /\ UNCHANGED BaseVars
StampReceipt == /\ Mode \in {"StampBefore","StampFixed"} /\ review.pc = 1
                /\ review' = [review EXCEPT !.pc = 2, !.version = 1, !.receipts = {"two"}]
                /\ UNCHANGED BaseVars
StampCommit == /\ Mode \in {"StampBefore","StampFixed"} /\ review.pc = 2
               /\ review' = [review EXCEPT !.pc = 3,
                    !.stored = IF Mode = "StampBefore" \/ review.readVersion = review.version THEN review.member ELSE review.stored]
               /\ UNCHANGED BaseVars
StampRetry == /\ Mode \in {"StampBefore","StampFixed"} /\ review.pc = 3 /\ review.stored = "None"
              /\ review' = [review EXCEPT !.pc = 4, !.readVersion = review.version, !.member = "two"]
              /\ UNCHANGED BaseVars
StampOwn == /\ Mode \in {"StampBefore","StampFixed"} /\ review.pc = 4
            /\ review.member \in review.receipts /\ review.readVersion = review.version
            /\ review' = [review EXCEPT !.pc = 5, !.stored = review.member]
            /\ UNCHANGED BaseVars
Next == StampRead \/ StampReceipt \/ StampCommit \/ StampRetry \/ StampOwn \/ NativeTypedAdmission \/ Prepare \/ AnyKey \/ NotSent \/ RuntimeReady \/ Restart \/ Done
        \/ ReceiptOne \/ ReceiptCloseOne \/ ReceiptTwo \/ PopAll \/ SecondDequeue \/ SecondClose \/ PendingOpen \/ PendingClose
Spec == Init /\ [][Next]_vars /\ WF_vars(Prepare) /\ WF_vars(AnyKey)
        /\ WF_vars(StampRead) /\ WF_vars(StampReceipt) /\ WF_vars(StampCommit) /\ WF_vars(StampRetry) /\ WF_vars(StampOwn)
        /\ WF_vars(NotSent) /\ WF_vars(RuntimeReady) /\ WF_vars(Restart)
        /\ WF_vars(ReceiptOne) /\ WF_vars(ReceiptCloseOne) /\ WF_vars(ReceiptTwo)
        /\ WF_vars(PopAll) /\ WF_vars(SecondDequeue) /\ WF_vars(SecondClose)
        /\ WF_vars(PendingOpen) /\ WF_vars(PendingClose) /\ WF_vars(NativeTypedAdmission)
NoKeyBeforePrepare == anyKey => prepared
FollowerNoAuthority == role = "Follower" => pin = "None"
NoSilentLoss == original
NoNativeSettlement == result = "None"
PrimaryPinPreserved == (role = "Primary" /\ prepared) => pin = "Primary"
PreparationEventuallyLeavesIntent == <>(phase # "Intent")
SourceEventuallyBound == <>(source = "Bound")
NoForeignReceiptProof == review.firstProof = 0 /\ review.proofs \subseteq {"two"}
PopAllNoDebt == (Mode \in {"PopAllBefore","PopAllFixed"} /\ review.pc > 0) => ~review.fifo
EvidenceEventuallyOwn == (Mode \in {"ReceiptBefore","ReceiptFixed","PopAllBefore","PopAllFixed","OpenBefore","OpenFixed"}) => <>(review.proofs = {"two"})

NoUntypedAdmission == admitted => (prepared /\ anyKey /\ source = "Bound" /\ (role = "Primary" => pin = "Primary"))
TypedAdmissionEventuallyAvailable == (phase = "Unknown" /\ source = "Bound") ~> admitted
FreshRegistrationResult == review.stored = "None" \/ review.readVersion = review.version
ReceiptEventuallyCommits == <>(review.stored = "two")

\* The reopen pass owns the seed; a pre-key caller closes its unstarted scope on return.
PrefixHashPass == /\ review.prefixStep = 0
                  /\ review' = [review EXCEPT !.prefixStep = 1, !.prefixReads = 1,
                       !.seed = IF Mode = "PrefixGapBefore" THEN "Absent"
                          ELSE IF (review.prefixHash /\ review.prefixFifo) \/ Mode \in {"PrefixHashBefore", "PrefixFifoBefore"}
                               THEN "Ready" ELSE "Unavailable"]
                  /\ UNCHANGED BaseVars
PrefixTicketChange == /\ review.prefixStep = 1
                      /\ review' = [review EXCEPT !.prefixStep = 2,
                           !.prefixReads = IF Mode = "PrefixReadBefore" THEN 2 ELSE review.prefixReads]
                      /\ UNCHANGED BaseVars
PrefixScopeClose == /\ review.prefixStep = 2 /\ review.seed # "Absent"
                    /\ review' = [review EXCEPT !.prefixStep = 3, !.prefixLease = 0]
                    /\ UNCHANGED BaseVars
PrefixNext == PrefixHashPass \/ PrefixTicketChange \/ PrefixScopeClose
PrefixSpec == Init /\ [][PrefixNext]_vars /\ WF_vars(PrefixHashPass)
              /\ WF_vars(PrefixTicketChange) /\ WF_vars(PrefixScopeClose)
PrefixAuthenticated == review.seed = "Ready" => review.prefixHash /\ review.prefixFifo
PrefixNoReread == review.prefixReads <= 1
PrefixNoFabrication == result = "None" /\ original
PrefixSeedEventuallyAvailable == <>(review.seed # "Absent")
PrefixLeaseEventuallyFree == <>(review.prefixLease = 0)


\* Clear first refuses unsupported snapshots, then native admission, then delegates to V2.
ClearAttempt == /\ review.clearStep = 0
                /\ LET bypass == Mode = "ClearOwnerBefore" /\ review.clearOwner
                       readonlyBypass == Mode = "ClearReadOnlyBefore"
                       clears == bypass \/ readonlyBypass
                   IN review' = [review EXCEPT !.clearStep = 1,
                        !.clearLease = IF clears THEN 0 ELSE @,
                        !.clearRows = IF clears THEN 0 ELSE @,
                        !.clearSignal = clears,
                        !.clearIntents = IF readonlyBypass \/ (Mode = "ClearRouteBefore" /\ review.clearOwner /\ ~review.clearDenied) THEN 1 ELSE @]
                /\ UNCHANGED BaseVars
ClearRepair == /\ review.clearStep = 1
               /\ review' = [review EXCEPT !.clearStep = 2,
                    !.clearDenied = FALSE, !.clearReadOnly = FALSE]
               /\ UNCHANGED BaseVars
ClearDelegate == /\ review.clearStep = 2 /\ ~review.clearDenied /\ ~review.clearReadOnly
                 /\ review' = [review EXCEPT !.clearStep = 3, !.clearOwner = TRUE,
                      !.clearLease = 0, !.clearRows = 0, !.clearSignal = TRUE, !.clearHead = TRUE]
                 /\ UNCHANGED BaseVars
ClearSpec == Init /\ [][ClearAttempt \/ ClearRepair \/ ClearDelegate]_vars
             /\ WF_vars(ClearAttempt) /\ WF_vars(ClearRepair) /\ WF_vars(ClearDelegate)
ClearRefusalKeepsOwnership == review.clearStep = 1 =>
    review.clearLease = 1 /\ review.clearRows = 1 /\ ~review.clearSignal
ClearReadOnlyKeepsIntent == (review.clearStep = 1 /\ review.clearReadOnly) => review.clearIntents = 0
ClearEventuallyProgresses == <>(review.clearLease = 0 /\ review.clearRows = 0 /\ review.clearHead)

\* Registration followed by a Watcher Start may re-arm a generic deadline.
BackstopRegister == /\ review.pc = 0
                    /\ prepared' = TRUE /\ phase' = "Prepared"
                    /\ review' = [review EXCEPT !.pc = 1]
                    /\ UNCHANGED <<role, pin, anyKey, original, result, source, restored, admitted>>
BackstopKey == /\ review.pc = 1 /\ anyKey' = TRUE /\ phase' = "Unknown"
               /\ review' = [review EXCEPT !.pc = 2]
               /\ UNCHANGED <<role, pin, prepared, original, result, source, restored, admitted>>
BackstopWatcher == /\ review.pc = 2 /\ review' = [review EXCEPT !.pc = 3]
                   /\ UNCHANGED BaseVars
BackstopFinalize == /\ review.pc = 3
                    /\ review' = [review EXCEPT !.pc = 4,
                         !.stored = IF Mode = "BackstopBefore" THEN "Cancelled" ELSE @,
                         !.clearLease = IF Mode = "BackstopBefore" THEN 0 ELSE @,
                         !.clearRows = IF Mode = "BackstopBefore" THEN 0 ELSE @]
                    /\ UNCHANGED BaseVars
BackstopSpec == Init /\ [][BackstopRegister \/ BackstopKey \/ BackstopWatcher \/ BackstopFinalize]_vars
                /\ WF_vars(BackstopRegister) /\ WF_vars(BackstopKey) /\ WF_vars(BackstopWatcher) /\ WF_vars(BackstopFinalize)
NativeBackstopKeepsInput == review.stored = "None" /\ review.clearLease = 1 /\ review.clearRows = 1
BackstopWasAttempted == <>(review.pc = 4)

SupplyCaptured == /\ review.pc = 0
                  /\ prepared' = TRUE /\ phase' = "Unknown"
                  /\ anyKey' = review.supplyKey
                  /\ source' = IF review.supplySource THEN "Bound" ELSE "PreSource"
                  /\ pin' = IF role = "Primary" THEN "Primary" ELSE "None"
                  /\ review' = [review EXCEPT !.pc = 1, !.prefixReads = 1, !.seed = IF review.prefixHash /\ review.prefixFifo THEN "Ready" ELSE "Unavailable"]
                  /\ UNCHANGED <<role, original, result, restored, admitted>>
SupplyRegistration == /\ review.pc = 1
                      /\ review' = [review EXCEPT !.pc = 2, !.version = 1,
                           !.readVersion = IF review.supplyStamp THEN 1 ELSE 0, !.receipts = {"two"}]
                      /\ UNCHANGED BaseVars
OOffer == /\ review.pc = 2 /\ Mode \notin {"SupplyQueueBefore", "SupplyWithdrawBefore"}
          /\ (review.seed = "Ready" \/ Mode = "SupplyPrefixBefore")
          /\ review' = [review EXCEPT !.pc = 3]
          /\ UNCHANGED BaseVars
SupplyValid == prepared /\ anyKey /\ source = "Bound" /\ review.supplyTicket /\ review.readVersion = review.version
               /\ review.seed = "Ready" /\ review.prefixHash /\ review.prefixFifo
SupplyBypass == Mode \in {"SupplyTicketBefore", "SupplySourceBefore", "SupplyKeyBefore", "SupplyStampBefore", "SupplyPrefixBefore"}
NativePreparedAdmit == /\ review.pc = 3 /\ (SupplyValid \/ SupplyBypass)
                       /\ admitted' = TRUE /\ review' = [review EXCEPT !.pc = 4]
                       /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, source, restored>>
SupplyRejected == /\ ((review.pc = 3 /\ ~SupplyValid /\ ~SupplyBypass)
                       \/ (review.pc = 2 /\ review.seed # "Ready" /\ Mode # "SupplyPrefixBefore"))
                   /\ review' = [review EXCEPT !.pc = 11]
                   /\ UNCHANGED BaseVars
SupplyResult == /\ review.pc = 4 /\ admitted
                /\ result' = IF Mode = "SupplyWithdrawFixed" THEN "WithdrawnNotExecuted" ELSE "Executed"
                /\ review' = [review EXCEPT !.pc = 5, !.firstProof = 1, !.proofs = {"two"}]
                /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, source, restored, admitted>>
SupplyActorCAS == /\ review.pc = 5 /\ result # "None"
                  /\ review' = [review EXCEPT !.pc = 6, !.clearLease = IF role = "Primary" THEN 0 ELSE @]
                  /\ UNCHANGED BaseVars
SupplyRowCAS == /\ review.pc = 6 /\ (role = "Follower" \/ review.clearLease = 0)
                /\ review' = [review EXCEPT !.pc = 7, !.clearRows = IF role = "Primary" THEN 0 ELSE @]
                /\ UNCHANGED BaseVars
SupplyWake == /\ review.pc = 7 /\ (role = "Follower" \/ review.clearRows = 0)
              /\ review' = [review EXCEPT !.pc = 8, !.clearHead = IF role = "Primary" THEN TRUE ELSE @]
              /\ UNCHANGED BaseVars
ForeignTypedRun == /\ review.pc >= 8 /\ review.pc < 11
                   /\ review' = [review EXCEPT !.pc = @ + 1,
                        !.firstProof = IF Mode = "ForeignTypedRunsBefore" THEN 2 ELSE @,
                        !.proofs = IF Mode = "ForeignTypedRunsBefore" THEN {"two", "foreign"} ELSE @]
                   /\ UNCHANGED BaseVars
SupplySpec == Init /\ [][SupplyCaptured \/ SupplyRegistration \/ OOffer \/ NativePreparedAdmit \/ SupplyRejected
              \/ SupplyResult \/ SupplyActorCAS \/ SupplyRowCAS \/ SupplyWake \/ ForeignTypedRun]_vars
              /\ WF_vars(SupplyCaptured) /\ WF_vars(SupplyRegistration) /\ WF_vars(OOffer)
              /\ WF_vars(NativePreparedAdmit) /\ WF_vars(SupplyRejected) /\ WF_vars(SupplyResult)
              /\ WF_vars(SupplyActorCAS) /\ WF_vars(SupplyRowCAS) /\ WF_vars(SupplyWake) /\ WF_vars(ForeignTypedRun)
SupplyFollowerNoParent == role = "Follower" => review.clearLease = 1 /\ review.clearRows = 1 /\ ~review.clearHead
SupplyTyped == admitted => SupplyValid
SupplyOrder == /\ (review.clearLease = 0 => result # "None")
               /\ (review.clearRows = 0 => review.clearLease = 0)
               /\ (review.clearHead => review.clearRows = 0)
SupplyResultEventually == <>(result # "None")
SupplyLeaseEventuallyFree == role = "Primary" => <>(review.clearLease = 0)
SupplyHeadEventuallyInjected == <>review.clearHead
SupplyRejectedEventually == <>(review.pc = 11)
NoForeignResponse == review.firstProof <= 1 /\ review.proofs \subseteq {"two"}
\* Restored custody accepts an authentic result without minting a current live actor.
SupplyBootCapture == /\ review.pc = 0 /\ restored' = TRUE
                     /\ result' = IF Mode \in {"SupplySavedBefore", "SupplySavedFixed"} THEN "Executed" ELSE "None"
                     /\ review' = [review EXCEPT !.pc = 1, !.clearLease = 0,
                          !.firstProof = IF Mode \in {"SupplySavedBefore", "SupplySavedFixed"} THEN 1 ELSE 0,
                          !.prefixReads = 1]
                     /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, source, admitted>>
SupplyBootOffer == /\ review.pc = 1
                   /\ admitted' = (Mode = "SupplyBootBefore")
                   /\ review' = [review EXCEPT !.pc = 2]
                   /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, source, restored>>
SupplyBootResult == /\ review.pc = 2 /\ result' = "Executed"
                    /\ review' = [review EXCEPT !.pc = 3,
                         !.firstProof = IF result = "None" \/ Mode = "SupplySavedBefore" THEN @ + 1 ELSE @]
                    /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, source, restored, admitted>>
SupplyBootRow == /\ review.pc = 3 /\ result # "None"
                 /\ review' = [review EXCEPT !.pc = 4, !.clearRows = 0]
                 /\ UNCHANGED BaseVars
SupplyBootWake == /\ review.pc = 4 /\ review.clearRows = 0
                  /\ review' = [review EXCEPT !.pc = 5, !.clearHead = TRUE]
                  /\ UNCHANGED BaseVars
SupplyBootSpec == Init /\ [][SupplyBootCapture \/ SupplyBootOffer \/ SupplyBootResult \/ SupplyBootRow \/ SupplyBootWake]_vars
                  /\ WF_vars(SupplyBootCapture) /\ WF_vars(SupplyBootOffer) /\ WF_vars(SupplyBootResult)
                  /\ WF_vars(SupplyBootRow) /\ WF_vars(SupplyBootWake)
SupplyBootNoRemint == ~admitted
SupplySavedExactlyOnce == review.firstProof <= 1
SupplyBootEventuallyFree == <>(review.clearRows = 0 /\ review.clearHead)

====
