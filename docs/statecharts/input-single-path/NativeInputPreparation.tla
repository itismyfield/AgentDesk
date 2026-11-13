-------------------- MODULE NativeInputPreparation --------------------
EXTENDS Naturals
CONSTANT Mode
VARIABLES phase, role, pin, prepared, anyKey, original, result, source, restored, review
BaseVars == <<phase, role, pin, prepared, anyKey, original, result, source, restored>>
vars == <<BaseVars, review>>
Init == /\ phase = "Intent" /\ role \in {"Primary", "Follower"}
        /\ pin = "None" /\ prepared = FALSE /\ anyKey = FALSE /\ original = TRUE
        /\ result = "None" /\ source = "PreSource" /\ restored = FALSE
        /\ review \in [pc:{0}, kind:{"Taken","Attached"}, member:{"None"}, firstProof:{0}, proofs:{{}}, fifo:{TRUE}, taken:{FALSE}]
Prepare == /\ phase = "Intent"
           /\ phase' = "Prepared" /\ prepared' = TRUE
           /\ pin' = IF role = "Primary" \/ Mode = "BorrowBusy" THEN "Primary" ELSE "None"
           /\ UNCHANGED <<role, anyKey, original, result, source, restored, review>>
AnyKey == /\ (phase = "Prepared" \/ (Mode = "KeyBeforePrepare" /\ phase = "Intent"))
          /\ phase' = "Unknown" /\ anyKey' = TRUE
          /\ UNCHANGED <<role, pin, prepared, original, result, source, restored, review>>
NotSent == /\ phase \in {"Intent", "Prepared"} /\ ~anyKey
           /\ phase' = "NotSent" /\ original' = (Mode # "DropNotSent")
           /\ UNCHANGED <<role, pin, prepared, anyKey, result, source, restored, review>>
RuntimeReady == /\ source = "PreSource" /\ source' = "Bound"
                /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, restored, review>>
Restart == /\ phase \in {"Prepared", "Unknown", "NotSent"} /\ ~restored
           /\ restored' = TRUE
           /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, source, review>>
Done == /\ phase \in {"Unknown", "NotSent"} /\ source = "Bound" /\ restored
        /\ UNCHANGED vars
ReceiptOne == /\ Mode \in {"ReceiptBefore","ReceiptFixed"} /\ review.pc = 0
              /\ review' = [review EXCEPT !.pc = 1, !.member = IF Mode = "ReceiptBefore" THEN "one" ELSE "None"]
              /\ UNCHANGED BaseVars
ReceiptCloseOne == /\ Mode \in {"ReceiptBefore","ReceiptFixed"} /\ review.pc = 1
                   /\ review' = [review EXCEPT !.pc = 2, !.firstProof = IF review.member = "one" THEN 1 ELSE 0,
                        !.proofs = IF review.member = "one" THEN {"one"} ELSE {}]
                   /\ UNCHANGED BaseVars
ReceiptTwo == /\ Mode \in {"ReceiptBefore","ReceiptFixed"} /\ review.pc = 2
              /\ review' = [review EXCEPT !.pc = 3, !.member = "two",
                   !.proofs = IF Mode = "ReceiptFixed" THEN {"two"} ELSE {}]
              /\ UNCHANGED BaseVars
PopAll == /\ Mode \in {"PopAllBefore","PopAllFixed"} /\ review.pc = 0
          /\ review' = [review EXCEPT !.pc = 1, !.fifo = (Mode = "PopAllBefore")]
          /\ UNCHANGED BaseVars
SecondDequeue == /\ Mode \in {"PopAllBefore","PopAllFixed"} /\ review.pc = 1
                 /\ review' = [review EXCEPT !.pc = 2, !.taken = ~review.fifo]
                 /\ UNCHANGED BaseVars
SecondClose == /\ Mode \in {"PopAllBefore","PopAllFixed"} /\ review.pc = 2
               /\ review' = [review EXCEPT !.pc = 3, !.proofs = IF review.taken THEN {"two"} ELSE {}]
               /\ UNCHANGED BaseVars
PendingOpen == /\ Mode \in {"OpenBefore","OpenFixed"} /\ review.pc = 0
               /\ review' = [review EXCEPT !.pc = 1, !.member = IF Mode = "OpenFixed" THEN "two" ELSE "None"]
               /\ UNCHANGED BaseVars
PendingClose == /\ Mode \in {"OpenBefore","OpenFixed"} /\ review.pc = 1
                /\ review' = [review EXCEPT !.pc = 2, !.proofs = IF review.member = "two" THEN {"two"} ELSE {}]
                /\ UNCHANGED BaseVars
Next == Prepare \/ AnyKey \/ NotSent \/ RuntimeReady \/ Restart \/ Done
        \/ ReceiptOne \/ ReceiptCloseOne \/ ReceiptTwo \/ PopAll \/ SecondDequeue \/ SecondClose \/ PendingOpen \/ PendingClose
Spec == Init /\ [][Next]_vars /\ WF_vars(Prepare) /\ WF_vars(AnyKey)
        /\ WF_vars(NotSent) /\ WF_vars(RuntimeReady) /\ WF_vars(Restart)
        /\ WF_vars(ReceiptOne) /\ WF_vars(ReceiptCloseOne) /\ WF_vars(ReceiptTwo)
        /\ WF_vars(PopAll) /\ WF_vars(SecondDequeue) /\ WF_vars(SecondClose)
        /\ WF_vars(PendingOpen) /\ WF_vars(PendingClose)
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
=====================================================================
