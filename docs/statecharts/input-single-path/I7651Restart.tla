--------------------------- MODULE I7651Restart ---------------------------
EXTENDS InputOwnership, FiniteSets

(* Retain the exact owner-drive and sweep transitions from the first design leg.
   Each bound interrupted episode has a durable notice identity. The queue's
   progress is independent of Discord recovering a notice receipt. *)
VARIABLE restartNotices, boots
restartVars == <<vars, restartNotices, boots>>
Episodes == 1..2

CBSweepInit == CompletionWakeInit /\ CB.op = "staleCancel" /\ CB.wake
CBSweepClear == CBCloseRow \/ CBLifecycleClears
CBSweepRecover == IF Mut = "cbNoSweep" THEN FALSE ELSE CBSweepClear \/ CBCloseBarrier
CBSweepNext == CBLook \/ CBNativeEnds \/ CBPaste \/ CBSettle \/ CBNothingLeft \/ CBSweepRecover

RestartInit == /\ CBSweepInit /\ boots = 0
        /\ \E interrupted \in SUBSET Episodes :
             restartNotices = [e \in Episodes |-> [interrupted |-> e \in interrupted,
                 phase |-> "unseen", copies |-> 0, mayMiss |-> TRUE]]

RecoverNotice(e) ==
    /\ restartNotices[e].phase = "unseen"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase =
         IF restartNotices[e].interrupted /\ Mut # "noNotice" THEN "owed" ELSE "closed"]
    /\ UNCHANGED <<vars, boots>>

Prepare(e) ==
    /\ restartNotices[e].phase = "owed"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase = "prepared"]
    /\ UNCHANGED <<vars, boots>>

Create(e) ==
    /\ restartNotices[e].phase = "prepared"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase = "uncertain", ![e].copies = @ + 1]
    /\ UNCHANGED <<vars, boots>>

MissBeforeCreate(e) ==
    /\ restartNotices[e].phase = "prepared" /\ restartNotices[e].mayMiss
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase = "uncertain", ![e].mayMiss = FALSE]
    /\ UNCHANGED <<vars, boots>>

Resolve(e) ==
    /\ restartNotices[e].phase = "uncertain"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase =
         IF restartNotices[e].copies = 0 THEN "owed" ELSE "closed"]
    /\ UNCHANGED <<vars, boots>>

(* The model's delivery port resolves an uncertain request before permitting
   any new create. One repeat boot may lose the prepared send permission while
   retaining its receipt or the fact that delivery is uncertain. *)
Reboot ==
    /\ boots = 0
    /\ boots' = 1
    /\ restartNotices' = [e \in Episodes |->
        IF restartNotices[e].phase = "uncertain" /\ Mut = "blindRetry"
        THEN [restartNotices[e] EXCEPT !.phase = "prepared"]
        ELSE IF restartNotices[e].phase = "prepared"
             THEN [restartNotices[e] EXCEPT !.phase = "owed"]
             ELSE restartNotices[e]]
    /\ UNCHANGED vars

Queue == CBSweepNext /\ UNCHANGED <<restartNotices, boots>>
DropInput ==
    /\ Mut = "dropInput" /\ CB.pending /\ CB.pastes = 0
    /\ observer' = [CB EXCEPT !.pending = FALSE]
    /\ UNCHANGED <<ObserverLegacy, restartNotices, boots>>

RestartNext == Queue \/ Reboot \/ DropInput
        \/ \E e \in Episodes : RecoverNotice(e) \/ Prepare(e) \/ Create(e) \/ MissBeforeCreate(e) \/ Resolve(e)
RestartSpec == RestartInit /\ [][RestartNext]_restartVars
    /\ WF_restartVars(CBLook /\ UNCHANGED <<restartNotices, boots>>)
    /\ WF_restartVars(CBNativeEnds /\ UNCHANGED <<restartNotices, boots>>)
    /\ WF_restartVars(CBPaste /\ UNCHANGED <<restartNotices, boots>>)
    /\ WF_restartVars(CBSettle /\ UNCHANGED <<restartNotices, boots>>)
    /\ WF_restartVars(CBSweepRecover /\ UNCHANGED <<restartNotices, boots>>)
    /\ \A e \in Episodes : WF_restartVars(RecoverNotice(e))
                          /\ WF_restartVars(Prepare(e))
                          /\ WF_restartVars(Create(e))
                          /\ WF_restartVars(Resolve(e))

NoticeExactlyOnce == \A e \in Episodes :
    /\ restartNotices[e].copies <= (IF restartNotices[e].interrupted THEN 1 ELSE 0)
    /\ restartNotices[e].phase = "closed" /\ restartNotices[e].interrupted => restartNotices[e].copies = 1
NoInputLoss == CB.pending \/ CB.pastes = 1
NextInputEventuallyProcessed == <>(CB.pastes = 1 /\ ~CB.pending)
NoticesEventuallyClosed == <>(\A e \in Episodes : restartNotices[e].phase = "closed")
=============================================================================
