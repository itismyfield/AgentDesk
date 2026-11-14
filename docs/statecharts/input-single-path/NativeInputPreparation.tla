-------------------- MODULE NativeInputPreparation --------------------
EXTENDS Naturals
CONSTANT Mode
VARIABLES phase, role, pin, prepared, anyKey, original, result, source, restored, admitted, review
BaseVars == <<phase, role, pin, prepared, anyKey, original, result, source, restored, admitted>>
vars == <<BaseVars, review>>
Init == /\ phase = "Intent" /\ role \in {"Primary", "Follower"}
        /\ pin = "None" /\ prepared = FALSE /\ anyKey = FALSE /\ original = TRUE
        /\ result = "None" /\ source = "PreSource" /\ restored = FALSE /\ admitted = FALSE
        /\ review \in [pc:{0}, kind:{"Taken","Attached"}, member:{"None"}, firstProof:{0}, proofs:{{}}, fifo:{TRUE}, taken:{FALSE}, pending:{TRUE}, receipts:{IF Mode \in {"StampBefore","StampFixed"} THEN {} ELSE {"two"}}, version:{0}, readVersion:{0}, stored:{"None"}]
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
=====================================================================
