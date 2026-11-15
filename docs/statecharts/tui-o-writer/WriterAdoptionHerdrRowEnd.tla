-------------------- MODULE WriterAdoptionHerdrRowEnd --------------------
EXTENDS Naturals
CONSTANTS Mapping, Owners, BoundAtRestart
VARIABLES bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition
vars == <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Current actions: herdr_row_end.rs:238-279; completion_delivery.rs:411-416.
\* C separates the retired execution from its durable body; NExisting has no pre-adoption copy.
CMode == Mapping \in {"C", "CRepair", "CDecision", "CNoDurable", "CDuplicate", "COverwrite", "CEarlyOutput", "CSilentRefusal", "COwnedShortcut", "CNoSource"}
NStored == Mapping = "NStored"
NMode == Mapping = "NExisting" \/ NStored
DetachedMode == CMode \/ NMode
AdoptionHeld == owner \in {"Deferred", "Refused", "NoCandidate"}
Owned == owner = "Committed"
OldResolved == ~noticeBody \/ oldAck \/ lp = 1 \/ (disposition = "rejected" /\ refusalVisible)
OldAvailable == row \/ copyId = "old" \/ storeDuty \/ op >= 1 \/ lp >= 1

Init == /\ bound = BoundAtRestart /\ owner \in Owners
        /\ phase = "observe" /\ row = TRUE /\ inputHold = TRUE
        /\ noticeBody \in BOOLEAN /\ noticeSent = FALSE /\ lp = 0
        /\ gatewayHeld = TRUE /\ nextInput = "queued"
        /\ copyId = "none" /\ op = 0 /\ oldAck = FALSE /\ attempt = "none"
        /\ restarted = FALSE /\ storeDuty = Owned /\ newPosted = 0
        /\ earlyOutput = FALSE /\ refusalVisible = FALSE /\ disposition = "held"

\* C/N are proposals; true identity errors still cannot pass Observe.
Observe == /\ phase = "observe" /\ (Owned \/ (AdoptionHeld /\ Mapping # "before"))
    /\ phase' = (IF Owned /\ (~DetachedMode \/ storeDuty \/ Mapping = "COwnedShortcut") THEN "clear" ELSE IF CMode \/ NStored THEN "archive" ELSE IF NMode THEN "neutral" ELSE "commit")
    /\ UNCHANGED <<bound, owner, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* PROPOSED: identity-keyed fsynced copy before unlink. Existing o_pending cannot capture Deferred/Refused.
Archive == /\ phase = "archive"
    /\ copyId' = (IF oldAck \/ lp = 1 \/ Mapping = "CNoDurable" THEN "none" ELSE "old") /\ phase' = (IF NStored THEN "neutral" ELSE "detach")
    /\ UNCHANGED <<bound, owner, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Retire only the old execution/row guard. The old body stays in its copy.
Detach == /\ phase = "detach" /\ (copyId = "old" \/ oldAck \/ Mapping = "CNoDurable")
    /\ row' = (FALSE) /\ inputHold' = (FALSE) /\ gatewayHeld' = (FALSE) /\ phase' = ("done")
    /\ UNCHANGED <<bound, owner, noticeBody, noticeSent, lp, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Current commit releases the execution hold before Notice, not the row.
Commit == /\ phase = "commit"
    /\ inputHold' = (FALSE) /\ phase' = ("notice")
    /\ UNCHANGED <<bound, owner, row, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Current inner gate. Main is a negative body-delivery control, not an adopted proposal.
Notice == /\ phase = "notice" /\ (Owned \/ Mapping = "main")
    /\ noticeSent' = (TRUE) /\ phase' = ("clear") /\ lp' = (IF noticeBody /\ ~Owned THEN lp + 1 ELSE lp)
    /\ UNCHANGED <<bound, owner, row, inputHold, noticeBody, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* PROPOSED N: body-free notice; it does not count as a delivered answer.
Neutral == /\ phase = "neutral"
    /\ noticeSent' = (TRUE) /\ phase' = (IF NStored THEN "detach" ELSE "clear")
    /\ UNCHANGED <<bound, owner, row, inputHold, noticeBody, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Current guarded clear; without an independent copy NExisting can silently lose the body.
Clear == /\ phase = "clear" /\ (Owned \/ noticeSent)
    /\ row' = (FALSE) /\ inputHold' = (FALSE) /\ phase' = ("closed")
    /\ UNCHANGED <<bound, owner, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

Closed == /\ phase = "closed" /\ ~row
    /\ gatewayHeld' = (FALSE) /\ phase' = ("done")
    /\ UNCHANGED <<bound, owner, row, inputHold, noticeBody, noticeSent, lp, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Passing the old-row guard is not bypassing the separate O-ready admission.
Start == /\ ~gatewayHeld /\ ~row /\ nextInput = "queued"
    /\ nextInput' = ("pastRowGuard") /\ copyId' = (IF Mapping = "COverwrite" /\ copyId = "old" THEN "new" ELSE copyId)
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Optional environment source bind; it may never arrive and carries no fairness.
Bind == /\ Mapping # "CNoSource" /\ nextInput = "pastRowGuard" /\ ~bound
    /\ bound' = (TRUE)
    /\ UNCHANGED <<owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Existing adoption with source/row-only custody; no automatic recovery of Refused is assumed.
Adopt == /\ bound /\ owner = "Deferred"
    /\ owner' = ("Committed") /\ storeDuty' = (IF DetachedMode THEN storeDuty ELSE TRUE)
    /\ UNCHANGED <<bound, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, newPosted, earlyOutput, refusalVisible, disposition>>

\* Explicit assumption from the brief: home returns with O authority and access to the retained debt.
HomeReturns == /\ DetachedMode /\ owner = "NoCandidate"
    /\ owner' = ("Committed")
    /\ UNCHANGED <<bound, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Conditional control only: an external verified store recovery (adoption.rs:369), not an existing fair service.
Repair == /\ Mapping = "CRepair" /\ owner = "Refused"
    /\ owner' = ("Committed")
    /\ UNCHANGED <<bound, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* O effect with stable old identity. Uncertain receipt reconciliation is separate from creating a message.
OCreate == /\ Owned /\ noticeBody /\ (copyId = "old" \/ storeDuty) /\ ~oldAck /\ lp = 0 /\ (op = 0 \/ (Mapping = "CDuplicate" /\ op = 1))
    /\ op' = (op + 1) /\ attempt' = ("uncertain")
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, oldAck, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

\* Confirm the old effect durably before dropping its old-body copy. No alarm counts as O delivery.
OConfirm == /\ op >= 1 /\ ~oldAck
    /\ oldAck' = (TRUE) /\ attempt' = ("none") /\ copyId' = ("none") /\ storeDuty' = (FALSE) /\ disposition' = ("o_delivered")
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, op, restarted, newPosted, earlyOutput, refusalVisible>>

\* USER-DECISION control: per-input visible failure, body retained. Current b2 has no such body disposition.
RefusalAccount == /\ Mapping \in {"CDecision", "CSilentRefusal", "COwnedShortcut", "CNoSource"} /\ owner = "Refused" /\ copyId = "old" /\ disposition = "held"
    /\ refusalVisible' = (Mapping # "CSilentRefusal") /\ disposition' = ("rejected")
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput>>

\* New work may run; its output cannot overtake unresolved old-body debt.
NewOutput == /\ DetachedMode /\ Owned /\ nextInput = "pastRowGuard" /\ newPosted = 0 /\ (OldResolved \/ Mapping = "CEarlyOutput")
    /\ newPosted' = (1) /\ earlyOutput' = (~OldResolved)
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, refusalVisible, disposition>>

\* One bounded crash/restart; copy, effect evidence and receipt remain durable and keep the old identity.
Restart == /\ ~restarted
    /\ restarted' = (TRUE) /\ attempt' = ("none") /\ phase' = (IF row THEN "observe" ELSE phase)
    /\ UNCHANGED <<bound, owner, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, storeDuty, newPosted, earlyOutput, refusalVisible, disposition>>

Rest == UNCHANGED vars
Next == Observe \/ Archive \/ Detach \/ Commit \/ Notice \/ Neutral \/ Clear \/ Closed \/ Start \/ Bind \/ Adopt \/ HomeReturns \/ Repair \/ OCreate \/ OConfirm \/ RefusalAccount \/ NewOutput \/ Restart \/ Rest
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Observe) /\ WF_vars(Archive) /\ WF_vars(Detach) /\ WF_vars(Commit) /\ WF_vars(Notice) /\ WF_vars(Neutral) /\ WF_vars(Clear) /\ WF_vars(Closed) /\ WF_vars(Start) /\ WF_vars(Adopt) /\ WF_vars(HomeReturns) /\ WF_vars(Repair) /\ WF_vars(OCreate) /\ WF_vars(OConfirm) /\ WF_vars(RefusalAccount) /\ WF_vars(NewOutput)

NoLegacyBodyOnTui == lp = 0
UnadoptedNotOwned == owner # "Committed" => ~Owned
UnknownOwnerKept == owner = "UnknownKind" => (row /\ inputHold /\ gatewayHeld)
ClearHasEvidence == ~row => (Owned \/ noticeSent \/ copyId = "old" \/ oldAck \/ disposition = "rejected")
NoBodyLoss == noticeBody => OldAvailable
BodyExactlyOnce == op + lp <= 1 /\ newPosted <= 1
BodyIdentityKept == copyId # "new"
OldBodyBeforeNew == ~earlyOutput
NoSilentRefusal == disposition = "rejected" => refusalVisible
HerdrRowEventuallySettled == [](row => <>~row)
NewStartEventuallyPassesRow == [](nextInput = "queued" => <>(nextInput = "pastRowGuard"))
BodyAfterAdoption == [](Owned /\ noticeBody => <>(oldAck \/ lp = 1))
RefusedExplicitlySettled == [](owner = "Refused" /\ noticeBody /\ ~row
                              => <>(oldAck \/ (disposition = "rejected" /\ refusalVisible)))
=============================================================================
