-------------------- MODULE WriterAdoptionHerdrRowEnd --------------------
EXTENDS Naturals
CONSTANTS Mapping, Owners, BoundAtRestart
VARIABLES cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy
vars == <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Current actions: herdr_row_end.rs:238-279; completion_delivery.rs:411-416.
\* C separates the retired execution from its durable body; NExisting has no pre-adoption copy.
CapModes == {"CapBefore", "CapFixed", "CapDuplicate", "CapNativeLeak", "CapWrongChannel", "CapWrongOwner", "CapWrongEpisode", "CapWrongSnapshot", "CapWrongEffect", "CapStaleGeneration", "CapRevive", "CapTransferRace", "CapReceiptFalse", "CapGcEarly", "CapEarlyOutput", "CapUnknownPlan", "CapReceiptUnproved", "CapWrongInput", "CapWrongPayload", "CapNoReserve", "CapCrashReservedNoProof", "CapCrashPreparedNoProof", "CapCrashReservedProof", "CapCrashPreparedProof", "CapProofUnchecked", "CapRunReset"}
CapMode == Mapping \in CapModes
CMode == CapMode \/ Mapping \in {"CLate", "CDiscard", "CDiscardLate", "CDiscardRetain", "CDiscardNoRecheck", "CDiscardRepost", "CDiscardHoldBody", "CDiscardOwnKey", "CDiscardUncertainDelivered", "CDiscardSilent", "CDiscardNoGate", "CDiscardRowBound", "CDiscardRestart", "CDiscardRestartNoGate", "C", "CRepair", "CDecision", "CNoDurable", "CDuplicate", "COverwrite", "CEarlyOutput", "CSilentRefusal", "COwnedShortcut", "CNoSource", "CRefusedRevoked", "CNoCandidateRevoked"}
NStored == Mapping = "NStored"
\* #7651 D1b design: a Refused body is never archived; it gets one visible reason and is dropped.
D1bDiscard == Mapping \in {"CDiscard", "CDiscardLate", "CDiscardRetain", "CDiscardNoRecheck", "CDiscardRepost", "CDiscardHoldBody", "CDiscardOwnKey", "CDiscardUncertainDelivered", "CDiscardSilent", "CDiscardNoGate", "CDiscardRowBound", "CDiscardRestart", "CDiscardRestartNoGate"}
LateRefusal == Mapping \in {"CLate", "CDiscardLate", "CDiscardNoRecheck", "CDiscardHoldBody", "CDiscardNoGate"}
\* The archive step re-reads ownership under the row lock and copies nothing for a Refused owner.
\* Archive is one step: the candidate guard is held from that re-read through the copy publish.
ArchiveRechecks == D1bDiscard /\ Mapping \notin {"CDiscardRetain", "CDiscardNoRecheck"}
\* An episode that already holds a reason (intent or receipt) is terminal for its body: it is
\* discarded reusing that reason and never copied or posted again. New admissions are not gated.
EpisodeClosed == D1bDiscard /\ Mapping \notin {"CDiscardNoGate", "CDiscardRestartNoGate"} /\ reasonState # "none"
\* The process may stop after the reason intent is durable and before the row is released; the
\* next boot restores no refusal, so the channel waits on a late adoption again (Deferred).
RestartCut == Mapping \in {"CDiscardRestart", "CDiscardRestartNoGate"}
NMode == Mapping = "NExisting" \/ NStored
DetachedMode == CMode \/ NMode
AdoptionHeld == owner \in {"Deferred", "Refused", "NoCandidate"}
Owned == owner = "Committed"
OldResolved == ~noticeBody \/ oldAck \/ lp = 1 \/ (disposition \in {"rejected", "discarded"} /\ refusalVisible)
OldAvailable == row \/ copyId = "old" \/ storeDuty \/ op >= 1 \/ lp >= 1

CapInit == [gen |-> 0, custodian |-> "home", run |-> 0, live |-> FALSE,
    grant |-> "none", grantGen |-> 0, grantOwner |-> "none", grantRun |-> 0,
    channel |-> "old", episode |-> "old", input |-> "old", snapshot |-> "old", effect |-> "old", payload |-> "old",
    locked |-> FALSE, reserved |-> FALSE, reservationGen |-> 0, reservationRun |-> 0, reservationSafe |-> TRUE, attemptId |-> 0, noEffectWitness |-> 0, resetSafe |-> TRUE, quiesced |-> FALSE, forcedCrash |-> FALSE, httpStarted |-> FALSE, prepared |-> FALSE, ended |-> FALSE,
    transferred |-> FALSE, stoppedOnce |-> FALSE, refusedOnce |-> FALSE,
    nativeTried |-> FALSE, nativePosts |-> 0, gc |-> FALSE,
    authoritySafe |-> TRUE, targetSafe |-> TRUE, freshSafe |-> TRUE]

Init == /\ cap = CapInit /\ bound = BoundAtRestart /\ owner \in Owners
        /\ phase = "observe" /\ row = TRUE /\ inputHold = TRUE
        /\ noticeBody \in BOOLEAN /\ noticeSent = FALSE /\ lp = 0
        /\ gatewayHeld = TRUE /\ nextInput = "queued"
        /\ copyId = "none" /\ op = 0 /\ oldAck = FALSE /\ attempt = "none"
        /\ restarted = FALSE /\ storeDuty = (Owned /\ ~CapMode) /\ newPosted = 0
        /\ earlyOutput = FALSE /\ refusalVisible = FALSE /\ disposition = "held"
        /\ refusedArchive = FALSE
        \* Another consumer (fresh output or foreign custody) may already hold this episode's reason;
        \* the restart mutant starts without one, so only this consumer's own intent can break it.
        /\ priorFresh \in (IF D1bDiscard /\ Mapping # "CDiscardRestartNoGate" THEN BOOLEAN ELSE {FALSE})
        /\ reasonState = (IF priorFresh THEN "posted" ELSE "none")
        /\ reasons = (IF priorFresh THEN 1 ELSE 0)
        /\ reasonKind = (IF priorFresh THEN "fresh" ELSE "none")
        /\ reasonedCopy = FALSE

\* C/N are proposals; true identity errors still cannot pass Observe.
Observe == /\ phase = "observe" /\ (Owned \/ (AdoptionHeld /\ Mapping # "before"))
    /\ phase' = (IF EpisodeClosed \/ (D1bDiscard /\ owner = "Refused" /\ Mapping # "CDiscardRetain") THEN "discard" ELSE IF CapMode THEN "archive" ELSE IF Owned /\ (~DetachedMode \/ storeDuty \/ Mapping = "COwnedShortcut") THEN "clear" ELSE IF CMode \/ NStored THEN "archive" ELSE IF NMode THEN "neutral" ELSE "commit")
    /\ UNCHANGED <<cap, bound, owner, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* PROPOSED: identity-keyed fsynced copy before unlink. Existing o_pending cannot capture Deferred/Refused.
Archive == /\ phase = "archive"
    /\ (IF EpisodeClosed \/ (ArchiveRechecks /\ owner = "Refused")
        THEN /\ phase' = "discard"
             /\ UNCHANGED <<copyId, refusedArchive, reasonedCopy>>
        ELSE /\ copyId' = (IF oldAck \/ lp = 1 \/ Mapping = "CNoDurable" THEN "none" ELSE "old")
             /\ phase' = (IF NStored THEN "neutral" ELSE "detach")
             /\ refusedArchive' = (refusedArchive \/ (owner = "Refused" /\ copyId' = "old"))
             /\ reasonedCopy' = (reasonedCopy \/ (reasonState # "none" /\ copyId' = "old")))
    /\ UNCHANGED <<cap, bound, owner, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, reasons, reasonState, reasonKind, priorFresh>>

\* Retire only the old execution/row guard. The old body stays in its copy.
Detach == /\ phase = "detach" /\ (copyId = "old" \/ oldAck \/ Mapping = "CNoDurable" \/ disposition = "discarded")
    /\ row' = (FALSE) /\ inputHold' = (FALSE) /\ gatewayHeld' = (FALSE) /\ phase' = ("done")
    /\ UNCHANGED <<cap, bound, owner, noticeBody, noticeSent, lp, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Current commit releases the execution hold before Notice, not the row.
Commit == /\ phase = "commit"
    /\ inputHold' = (FALSE) /\ phase' = ("notice")
    /\ UNCHANGED <<cap, bound, owner, row, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Current inner gate. Main is a negative body-delivery control, not an adopted proposal.
Notice == /\ phase = "notice" /\ (Owned \/ Mapping = "main")
    /\ noticeSent' = (TRUE) /\ phase' = ("clear") /\ lp' = (IF noticeBody /\ ~Owned THEN lp + 1 ELSE lp)
    /\ UNCHANGED <<cap, bound, owner, row, inputHold, noticeBody, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* PROPOSED N: body-free notice; it does not count as a delivered answer.
Neutral == /\ phase = "neutral"
    /\ noticeSent' = (TRUE) /\ phase' = (IF NStored THEN "detach" ELSE "clear")
    /\ UNCHANGED <<cap, bound, owner, row, inputHold, noticeBody, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Current guarded clear; without an independent copy NExisting can silently lose the body.
Clear == /\ phase = "clear" /\ (Owned \/ noticeSent)
    /\ row' = (FALSE) /\ inputHold' = (FALSE) /\ phase' = ("closed")
    /\ UNCHANGED <<cap, bound, owner, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

Closed == /\ phase = "closed" /\ ~row
    /\ gatewayHeld' = (FALSE) /\ phase' = ("done")
    /\ UNCHANGED <<cap, bound, owner, row, inputHold, noticeBody, noticeSent, lp, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Passing the old-row guard is not bypassing the separate O-ready admission.
Start == /\ ~gatewayHeld /\ ~row /\ nextInput = "queued"
    /\ nextInput' = ("pastRowGuard") /\ copyId' = (IF Mapping = "COverwrite" /\ copyId = "old" THEN "new" ELSE copyId)
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Optional environment source bind; it may never arrive and carries no fairness.
Bind == /\ Mapping # "CNoSource" /\ nextInput = "pastRowGuard" /\ ~bound
    /\ bound' = (TRUE)
    /\ UNCHANGED <<cap, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Existing adoption with source/row-only custody; no automatic recovery of Refused is assumed.
Adopt == /\ bound /\ owner = "Deferred"
    /\ owner' = ("Committed") /\ storeDuty' = (IF DetachedMode THEN storeDuty ELSE TRUE)
    /\ UNCHANGED <<cap, bound, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Explicit assumption from the brief: home returns with O authority and access to the retained debt.
HomeReturns == /\ DetachedMode /\ owner = "NoCandidate"
    /\ owner' = ("Committed")
    /\ UNCHANGED <<cap, bound, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Conditional control only: an external verified store recovery (adoption.rs:369), not an existing fair service.
Repair == /\ Mapping \in {"CRepair", "CRefusedRevoked"} /\ owner = "Refused"
    /\ owner' = ("Committed")
    /\ UNCHANGED <<cap, bound, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* O effect with stable old identity. Uncertain receipt reconciliation is separate from creating a message.
\* output::Scope::acquire restores the old artifact but refuses it after owner transfer.
\* A valid current owner/store is distinct from this old original admission remaining valid.
OldOriginalAdmitted == ~CapMode /\ Mapping \notin {"CRefusedRevoked", "CNoCandidateRevoked"}
OCreate == /\ OldOriginalAdmitted /\ ~EpisodeClosed /\ Owned /\ noticeBody /\ (copyId = "old" \/ storeDuty) /\ ~oldAck /\ lp = 0 /\ (op = 0 \/ (Mapping = "CDuplicate" /\ op = 1))
    /\ op' = (op + 1) /\ attempt' = ("uncertain")
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, oldAck, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Confirm the old effect durably before dropping its old-body copy. No alarm counts as O delivery.
OConfirm == /\ ~CapMode /\ op >= 1 /\ ~oldAck
    /\ oldAck' = (TRUE) /\ attempt' = ("none") /\ copyId' = ("none") /\ storeDuty' = (FALSE) /\ disposition' = ("o_delivered")
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, op, restarted, newPosted, earlyOutput, refusalVisible, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* USER-DECISION control: per-input visible failure, body retained. Current b2 has no such body disposition.
RefusalAccount == /\ Mapping \in {"CDecision", "CSilentRefusal", "COwnedShortcut", "CNoSource"} /\ owner = "Refused" /\ copyId = "old" /\ disposition = "held"
    /\ refusalVisible' = (Mapping # "CSilentRefusal") /\ disposition' = ("rejected")
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* New work may run; its output cannot overtake unresolved old-body debt.
NewOutput == /\ DetachedMode /\ Owned /\ nextInput = "pastRowGuard" /\ newPosted = 0 /\ (OldResolved \/ Mapping \in {"CEarlyOutput", "CapEarlyOutput"})
    /\ newPosted' = (1) /\ earlyOutput' = (~OldResolved)
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* One bounded crash/restart; copy, effect evidence and receipt remain durable and keep the old identity.
Restart == /\ ~restarted
    /\ cap' = (IF CapMode THEN [cap EXCEPT !.run = @ + 1, !.live = FALSE, !.grant = "revoked", !.quiesced = ~cap.httpStarted, !.locked = (IF ~cap.httpStarted THEN FALSE ELSE @)] ELSE cap)
    /\ restarted' = (TRUE) /\ attempt' = ("none") /\ phase' = (IF row THEN "observe" ELSE phase)
    /\ owner' = (IF RestartCut /\ owner = "Refused" THEN "Deferred" ELSE owner)
    /\ UNCHANGED <<bound, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Planned recovery port: validated original archive plus current OwnerPermit/store, not a new native stamp.
CapIdentity == cap.channel = "old" /\ cap.episode = "old" /\ cap.snapshot = "old" /\ cap.effect = "old" /\ cap.input = "old" /\ cap.payload = "old"
CapAuthority == cap.grantGen = cap.gen /\ cap.grantOwner = cap.custodian
CapFresh == cap.grantRun = cap.run /\ cap.grant = "issued" /\ cap.live
CapValid == (CapIdentity \/ Mapping \in {"CapWrongChannel", "CapWrongEpisode", "CapWrongSnapshot", "CapWrongEffect", "CapWrongInput", "CapWrongPayload"})
    /\ (CapAuthority \/ Mapping \in {"CapWrongOwner", "CapStaleGeneration"})
    /\ (CapFresh \/ Mapping = "CapRevive")

CapRecovery == /\ CapMode /\ cap.gen = 0 /\ copyId = "old" /\ owner \in {"Deferred", "Refused", "NoCandidate", "Committed"}
    /\ cap' = [cap EXCEPT !.gen = 1, !.live = TRUE]
    /\ owner' = "Committed"
    /\ UNCHANGED <<bound, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapIssue == /\ CapMode /\ noticeBody /\ Mapping \notin {"CapBefore", "CapUnknownPlan"} /\ Owned /\ cap.gen > 0 /\ cap.live /\ copyId = "old" /\ ~oldAck /\ ~cap.locked /\ ~cap.prepared /\ cap.grant \in {"none", "revoked"}
    /\ cap' = [cap EXCEPT !.grant = "issued", !.grantGen = (IF Mapping = "CapStaleGeneration" THEN 1 ELSE cap.gen),
        !.grantOwner = (IF Mapping = "CapWrongOwner" THEN "foreign" ELSE cap.custodian),
        !.grantRun = (IF Mapping = "CapRevive" THEN 0 ELSE cap.run),
        !.channel = (IF Mapping = "CapWrongChannel" THEN "other" ELSE "old"),
        !.episode = (IF Mapping = "CapWrongEpisode" THEN "other" ELSE "old"),
        !.snapshot = (IF Mapping = "CapWrongSnapshot" THEN "other" ELSE "old"),
        !.effect = (IF Mapping = "CapWrongEffect" THEN "other" ELSE "old"),
        !.input = (IF Mapping = "CapWrongInput" THEN "other" ELSE "old"),
        !.payload = (IF Mapping = "CapWrongPayload" THEN "other" ELSE "old")]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* The shared reservation commits before acquiring the effect guard/local Prepared.
CapReserve == /\ CapMode /\ cap.grant = "issued" /\ CapValid /\ ~cap.reserved /\ ~cap.prepared /\ ~cap.locked /\ ~oldAck /\ op = 0
    /\ cap' = [cap EXCEPT !.reserved = TRUE, !.attemptId = @ + 1, !.noEffectWitness = 0, !.quiesced = FALSE, !.reservationGen = cap.gen, !.reservationRun = cap.run]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Witness availability is independent evidence, not a result of gen/run/live mismatch.
CapProofAvailable == Mapping \notin {"CapReceiptUnproved", "CapCrashReservedNoProof", "CapCrashPreparedNoProof", "CapProofUnchecked", "CapRunReset"}
CapWitnessMatches == cap.attemptId > 0 /\ cap.noEffectWitness = cap.attemptId
CapProvideNoEffect == /\ CapMode /\ CapProofAvailable /\ cap.reserved /\ cap.quiesced /\ ~cap.locked /\ ~cap.httpStarted /\ op = 0 /\ ~CapWitnessMatches
    /\ cap' = [cap EXCEPT !.noEffectWitness = cap.attemptId]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapCertainNotSent == /\ CapMode /\ cap.reserved /\ cap.quiesced /\ ~cap.locked /\ ~cap.httpStarted /\ op = 0
    /\ (CapWitnessMatches \/ Mapping = "CapProofUnchecked" \/ (Mapping = "CapRunReset" /\ cap.reservationRun # cap.run))
    /\ cap' = [cap EXCEPT !.reserved = FALSE, !.prepared = FALSE, !.resetSafe = @ /\ CapWitnessMatches, !.quiesced = FALSE, !.grant = "revoked"]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Dedicated cuts force either reservation-only or durable Prepared before a crash.
CapCrashCut == /\ CapMode /\ ~cap.forcedCrash /\ cap.reserved /\ ~cap.httpStarted /\ op = 0
    /\ ((Mapping \in {"CapCrashReservedNoProof", "CapCrashReservedProof", "CapProofUnchecked", "CapRunReset"} /\ ~cap.prepared)
         \/ (Mapping \in {"CapCrashPreparedNoProof", "CapCrashPreparedProof"} /\ cap.prepared))
    /\ cap' = [cap EXCEPT !.forcedCrash = TRUE, !.run = @ + 1, !.live = FALSE, !.grant = "revoked", !.locked = FALSE, !.quiesced = TRUE]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapAcquire == /\ CapMode /\ (cap.forcedCrash \/ Mapping \notin {"CapCrashReservedNoProof", "CapCrashReservedProof", "CapProofUnchecked", "CapRunReset"}) /\ (Mapping = "CapNoReserve" \/ (cap.reserved /\ cap.reservationGen = cap.gen /\ cap.reservationRun = cap.run) \/ (Mapping = "CapDuplicate" /\ cap.prepared)) /\ cap.grant = "issued" /\ CapValid /\ ~cap.locked /\ ~oldAck /\ (op = 0 \/ (Mapping = "CapDuplicate" /\ op = 1)) /\ (~cap.prepared \/ Mapping = "CapDuplicate")
    /\ cap' = [cap EXCEPT !.locked = TRUE, !.freshSafe = @ /\ CapFresh]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Local Prepared precedes the first HTTP effect; both sides have a crash cut.
CapPrepare == /\ CapMode /\ cap.locked /\ cap.live /\ cap.grant = "issued" /\ ~oldAck /\ (~cap.prepared \/ Mapping = "CapDuplicate")
    /\ cap' = [cap EXCEPT !.prepared = TRUE, !.httpStarted = FALSE, !.ended = FALSE]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapPost == /\ CapMode /\ cap.locked /\ cap.prepared /\ ~cap.httpStarted /\ cap.live /\ cap.grant = "issued" /\ ~oldAck
    /\ (cap.forcedCrash \/ Mapping \notin {"CapCrashPreparedNoProof", "CapCrashPreparedProof"})
    /\ (op = 0 \/ (Mapping = "CapDuplicate" /\ op = 1))
    /\ cap' = [cap EXCEPT !.httpStarted = TRUE, !.ended = FALSE, !.reservationSafe = @ /\ cap.reserved, !.authoritySafe = @ /\ CapAuthority, !.targetSafe = @ /\ CapIdentity]
    /\ op' = op + 1 /\ attempt' = "uncertain"
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, oldAck, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapEnd == /\ CapMode /\ cap.locked /\ cap.httpStarted
    /\ cap' = [cap EXCEPT !.locked = FALSE, !.ended = TRUE]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapConfirm == /\ CapMode /\ ~oldAck /\ ((op = 1 /\ cap.ended /\ cap.targetSafe) \/ (~noticeBody /\ copyId = "old") \/ (Mapping = "CapReceiptFalse" /\ copyId = "old"))
    /\ cap' = [cap EXCEPT !.grant = "consumed"]
    /\ oldAck' = TRUE
    /\ attempt' = "confirmed"
    /\ disposition' = "o_delivered"
    /\ storeDuty' = FALSE
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, restarted, newPosted, earlyOutput, refusalVisible, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapGc == /\ CapMode /\ copyId = "old" /\ ~row /\ (oldAck \/ Mapping = "CapGcEarly")
    /\ cap' = [cap EXCEPT !.gc = TRUE]
    /\ copyId' = "none"
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapTransfer == /\ CapMode /\ cap.gen = 1 /\ ~cap.transferred /\ (~cap.locked \/ Mapping = "CapTransferRace")
    /\ cap' = [cap EXCEPT !.gen = 2, !.custodian = "next", !.transferred = TRUE, !.run = @ + 1, !.live = FALSE, !.quiesced = ~cap.httpStarted, !.grant = (IF Mapping = "CapTransferRace" THEN @ ELSE "revoked")]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapStop == /\ CapMode /\ cap.gen > 0 /\ ~cap.stoppedOnce /\ cap.live
    /\ cap' = [cap EXCEPT !.stoppedOnce = TRUE, !.live = FALSE, !.run = @ + 1, !.grant = "revoked", !.quiesced = ~cap.httpStarted]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapResume == /\ CapMode /\ cap.gen > 0 /\ ~cap.live /\ ~cap.locked
    /\ cap' = [cap EXCEPT !.live = TRUE]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapAbort == /\ CapMode /\ cap.locked /\ ~cap.httpStarted /\ ~cap.live
    /\ cap' = [cap EXCEPT !.locked = FALSE, !.quiesced = TRUE]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapRefuse == /\ CapMode /\ cap.grant = "issued" /\ ~cap.locked /\ ~cap.prepared /\ ~cap.refusedOnce
    /\ cap' = [cap EXCEPT !.refusedOnce = TRUE, !.live = FALSE, !.run = @ + 1, !.grant = "revoked", !.quiesced = TRUE]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapNative == /\ CapMode /\ cap.gen > 0 /\ ~cap.nativeTried
    /\ cap' = [cap EXCEPT !.nativeTried = TRUE, !.nativePosts = (IF Mapping = "CapNativeLeak" THEN 1 ELSE 0)]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

CapRaceResume == /\ CapMode /\ Mapping = "CapTransferRace" /\ cap.locked /\ ~cap.live
    /\ cap' = [cap EXCEPT !.live = TRUE]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Sensitivity: a Prepared without conclusive remote evidence remains held, never a false receipt.
CapIndeterminate == /\ Mapping = "CapReceiptUnproved" /\ cap.grant = "issued" /\ ~cap.prepared /\ ~cap.locked
    /\ cap' = [cap EXCEPT !.reserved = TRUE, !.prepared = TRUE, !.ended = TRUE, !.attemptId = (IF @ = 0 THEN 1 ELSE @), !.quiesced = TRUE]
    /\ UNCHANGED <<bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

NoResetWithoutProof == ~CapMode \/ cap.resetSafe
PendingUnprovedCrash == ~CapMode \/ (Mapping \in {"CapCrashReservedNoProof", "CapCrashPreparedNoProof"} /\ cap.forcedCrash /\ noticeBody => cap.reserved /\ op = 0 /\ ~oldAck)
CapPostHadReservation == ~CapMode \/ cap.reservationSafe
CapReceiptHasEffect == ~CapMode \/ (oldAck => (~noticeBody \/ (op = 1 /\ cap.targetSafe)))
CapGcHasReceipt == ~CapMode \/ (cap.gc => oldAck)
NoOldNativePost == ~CapMode \/ cap.nativePosts = 0
NoForeignDebtEffect == ~CapMode \/ (cap.authoritySafe /\ cap.targetSafe)
NoRevivedCapability == ~CapMode \/ cap.freshSafe
CapOldBeforeNew == ~CapMode \/ newPosted = 0 \/ op = 1 \/ ~noticeBody
CapDebtEventuallyGc == [](CapMode /\ oldAck /\ ~row /\ copyId = "old" => <>(copyId = "none"))

\* #7651 D1b: a waiting adoption may still be refused later (channel_gate.rs:464 Refused/Released).
LateRefuse == /\ LateRefusal /\ owner = "Deferred"
    /\ owner' = "Refused"
    /\ UNCHANGED <<cap, bound, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* D1b: discarding records one durable, body-free reason intent under the episode key and releases
\* the body at once; the post runs after it. A key that already holds a reason is reused.
OwnKey == Mapping = "CDiscardOwnKey"
IntentNext == IF Mapping = "CDiscardSilent" THEN reasonState
              ELSE IF OwnKey THEN "intent"
              ELSE IF reasonState = "none" THEN "intent" ELSE reasonState
KindNext == IF Mapping = "CDiscardSilent" THEN reasonKind
            ELSE IF OwnKey \/ reasonKind = "none" THEN "refused" ELSE reasonKind

\* D1b design: a Refused restarted row, or one whose episode already holds a reason, gets the
\* reason intent (or reuses it) and is released with no body copy.
DiscardRow == /\ phase = "discard"
    /\ reasonState' = IntentNext /\ reasonKind' = KindNext
    /\ refusalVisible' = (Mapping # "CDiscardSilent") /\ disposition' = "discarded"
    /\ (IF Mapping = "CDiscardHoldBody"
        THEN /\ phase' = "reasonwait" /\ UNCHANGED <<row, inputHold, gatewayHeld>>
        ELSE IF RestartCut
        THEN /\ phase' = "release" /\ UNCHANGED <<row, inputHold, gatewayHeld>>
        ELSE /\ row' = FALSE /\ inputHold' = FALSE /\ gatewayHeld' = FALSE /\ phase' = "done")
    /\ storeDuty' = FALSE
    /\ UNCHANGED <<cap, bound, owner, noticeBody, noticeSent, lp, nextInput, copyId, op, oldAck, attempt, restarted, newPosted, earlyOutput, refusedArchive, reasons, priorFresh, reasonedCopy>>

\* The exact row is cleared after its reason intent is durable; a restart may land between them.
ReleaseRow == /\ phase = "release"
    /\ row' = FALSE /\ inputHold' = FALSE /\ gatewayHeld' = FALSE /\ phase' = "done"
    /\ UNCHANGED <<cap, bound, owner, noticeBody, noticeSent, lp, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* Mutant control: the row is released only after a confirmed reason receipt.
ReleaseAfterReceipt == /\ phase = "reasonwait" /\ reasonState = "posted"
    /\ row' = FALSE /\ inputHold' = FALSE /\ gatewayHeld' = FALSE /\ phase' = "done"
    /\ UNCHANGED <<cap, bound, owner, noticeBody, noticeSent, lp, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonState, reasonKind, priorFresh, reasonedCopy>>

\* D1b design: a body kept while adoption waited is dropped with the reason intent once refused.
DiscardCopy == /\ D1bDiscard /\ owner = "Refused" /\ copyId = "old" /\ ~oldAck /\ op = 0
    /\ copyId' = "none" /\ storeDuty' = FALSE
    /\ reasonState' = IntentNext /\ reasonKind' = KindNext
    /\ refusalVisible' = (Mapping # "CDiscardSilent") /\ disposition' = "discarded"
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, op, oldAck, attempt, restarted, newPosted, earlyOutput, refusedArchive, reasons, priorFresh, reasonedCopy>>

\* The reason post: a verified created message settles it; an uncertain one keeps its attempt and is
\* never posted again (no reconciliation is installed). The intent resumes without row, body or debt.
ReasonWorkable == row \/ Mapping # "CDiscardRowBound"
ReasonCreated == /\ reasonState = "intent" /\ ReasonWorkable
    /\ reasonState' = "posted" /\ reasons' = reasons + 1
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasonKind, priorFresh, reasonedCopy>>
ReasonUncertain == /\ reasonState = "intent" /\ ReasonWorkable
    /\ reasonState' = "uncertain" /\ reasons' = reasons + 1
    /\ oldAck' = (Mapping = "CDiscardUncertainDelivered" \/ oldAck)
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasonKind, priorFresh, reasonedCopy>>
\* Mutant control: an uncertain reason is posted again.
RepostUncertain == /\ Mapping = "CDiscardRepost" /\ reasonState = "uncertain" /\ reasons < 2
    /\ reasonState' = "intent"
    /\ UNCHANGED <<cap, bound, owner, phase, row, inputHold, noticeBody, noticeSent, lp, gatewayHeld, nextInput, copyId, op, oldAck, attempt, restarted, storeDuty, newPosted, earlyOutput, refusalVisible, disposition, refusedArchive, reasons, reasonKind, priorFresh, reasonedCopy>>

Rest == UNCHANGED vars
Next == LateRefuse \/ DiscardRow \/ ReleaseRow \/ ReleaseAfterReceipt \/ DiscardCopy \/ ReasonCreated \/ ReasonUncertain \/ RepostUncertain \/ CapProvideNoEffect \/ CapCrashCut \/ CapPrepare \/ CapReserve \/ CapCertainNotSent \/ CapIndeterminate \/ CapRecovery \/ CapIssue \/ CapAcquire \/ CapPost \/ CapEnd \/ CapConfirm \/ CapGc \/ CapTransfer \/ CapStop \/ CapResume \/ CapAbort \/ CapRefuse \/ CapNative \/ CapRaceResume \/ Observe \/ Archive \/ Detach \/ Commit \/ Notice \/ Neutral \/ Clear \/ Closed \/ Start \/ Bind \/ Adopt \/ HomeReturns \/ Repair \/ OCreate \/ OConfirm \/ RefusalAccount \/ NewOutput \/ Restart \/ Rest
Spec == Init /\ [][Next]_vars
        /\ WF_vars(DiscardRow) /\ WF_vars(ReleaseRow) /\ WF_vars(DiscardCopy) /\ WF_vars(ReasonCreated) /\ WF_vars(ReleaseAfterReceipt) /\ WF_vars(Observe) /\ WF_vars(Archive) /\ WF_vars(Detach) /\ WF_vars(Commit) /\ WF_vars(Notice) /\ WF_vars(Neutral) /\ WF_vars(Clear) /\ WF_vars(Closed) /\ WF_vars(Start) /\ WF_vars(Adopt) /\ WF_vars(HomeReturns) /\ WF_vars(Repair) /\ WF_vars(OCreate) /\ WF_vars(OConfirm) /\ WF_vars(RefusalAccount) /\ WF_vars(NewOutput)

        /\ WF_vars(CapProvideNoEffect) /\ WF_vars(CapCrashCut) /\ WF_vars(CapPrepare) /\ WF_vars(CapReserve) /\ WF_vars(CapCertainNotSent) /\ WF_vars(CapRecovery) /\ WF_vars(CapIssue) /\ WF_vars(CapAcquire) /\ WF_vars(CapPost) /\ WF_vars(CapEnd) /\ WF_vars(CapConfirm) /\ WF_vars(CapGc) /\ WF_vars(CapResume) /\ WF_vars(CapAbort) /\ WF_vars(CapNative) /\ WF_vars(CapRaceResume)

NoLegacyBodyOnTui == lp = 0
UnadoptedNotOwned == owner # "Committed" => ~Owned
UnknownOwnerKept == owner = "UnknownKind" => (row /\ inputHold /\ gatewayHeld)
ClearHasEvidence == ~row => (Owned \/ noticeSent \/ copyId = "old" \/ oldAck \/ disposition \in {"rejected", "discarded"})
NoBodyLoss == noticeBody => (OldAvailable \/ (disposition = "discarded" /\ refusalVisible))
BodyExactlyOnce == op + lp <= 1 /\ newPosted <= 1
BodyIdentityKept == copyId # "new"
OldBodyBeforeNew == ~earlyOutput
NoSilentRefusal == disposition = "rejected" => refusalVisible
HerdrRowEventuallySettled == [](row => <>~row)
NewStartEventuallyPassesRow == [](nextInput = "queued" => <>(nextInput = "pastRowGuard"))
\* An explicitly discarded old body is no longer owed.
BodyAfterAdoption == [](Owned /\ noticeBody => <>(oldAck \/ lp = 1 \/ disposition = "discarded"))
RefusedExplicitlySettled == [](owner = "Refused" /\ noticeBody /\ ~row
                              => <>(oldAck \/ (disposition \in {"rejected", "discarded"} /\ refusalVisible)))
\* #7651 D1b rules: no Refused archive, one visible reason per discarded body, waiting copies end.
NoRefusedArchive == ~refusedArchive
ReasonAtMostOnce == reasons <= 1
DiscardVisible == disposition = "discarded" => reasonState # "none"
FirstReasonKept == priorFresh => reasonKind = "fresh"
ReasonNotBodyReceipt == disposition = "discarded" => ~oldAck
\* A body whose episode already held a reason when the copy step ran is never copied.
ReasonedBodyNotCopied == ~reasonedCopy
ReasonedBodyNotPosted == reasonState # "none" => op = 0
RefusedCopyDiscarded == [](owner = "Refused" /\ copyId = "old" => <>(copyId # "old"))
ReasonEventuallySettles == [](reasonState = "intent" => <>(reasonState \in {"posted", "uncertain"}))
=============================================================================
