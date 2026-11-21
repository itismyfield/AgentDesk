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
                 phase |-> "unseen", copies |-> 0, mayMiss |-> TRUE, fresh |-> TRUE]]

RecoverNotice(e) ==
    /\ restartNotices[e].phase = "unseen"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase =
         IF restartNotices[e].interrupted /\ Mut # "noNotice" THEN "owed" ELSE "closed"]
    /\ UNCHANGED <<vars, boots>>

Prepare(e) ==
    /\ restartNotices[e].phase = "owed"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase = "prepared"]
    /\ UNCHANGED <<vars, boots>>

(* The POST lands while the durable record still says Prepared; its answer comes later.
   Two copies already break the rule; the bound keeps a mutant's state space finite. *)
Create(e) ==
    /\ restartNotices[e].phase = "prepared" /\ restartNotices[e].copies < 2
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase = "inflight", ![e].copies = @ + 1]
    /\ UNCHANGED <<vars, boots>>

Answer(e) ==
    /\ restartNotices[e].phase = "inflight"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase = "uncertain"]
    /\ UNCHANGED <<vars, boots>>

MissBeforeCreate(e) ==
    /\ restartNotices[e].phase = "prepared" /\ restartNotices[e].mayMiss
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase = "uncertain", ![e].mayMiss = FALSE]
    /\ UNCHANGED <<vars, boots>>

(* A decisive answer: the exact POST response or a certain non-send. It may never come. A
   non-send is certain only to the process that held the request, or to a decisive port. *)
Resolve(e) ==
    /\ Mut # "unknownForever"
    /\ restartNotices[e].phase = "uncertain"
    /\ restartNotices[e].copies > 0 \/ restartNotices[e].fresh \/ Mut = "decisive"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase =
         IF restartNotices[e].copies = 0 THEN "owed" ELSE "closed"]
    /\ UNCHANGED <<vars, boots>>

(* B0 keeps a history miss (NotFound, Ambiguous, Unresolved) as a terminal unknown: it never
   authorizes another CREATE, so the notice may stay unseen. The mutant re-owes it instead. *)
GiveUp(e) ==
    /\ Mut # "decisive"
    /\ restartNotices[e].phase = "uncertain"
    /\ restartNotices' = [restartNotices EXCEPT ![e].phase =
         IF Mut = "notFoundRetry" THEN "owed" ELSE "unknown"]
    /\ UNCHANGED <<vars, boots>>

(* As the writer's settle_open: a durable Prepared, whether or not its POST landed, is only
   settled from history after one repeat boot, never prepared again. *)
Reboot ==
    /\ boots = 0
    /\ boots' = 1
    /\ restartNotices' = [e \in Episodes |->
        IF restartNotices[e].phase = "uncertain" /\ Mut = "blindRetry"
        THEN [restartNotices[e] EXCEPT !.phase = "prepared"]
        ELSE IF restartNotices[e].phase \in {"prepared", "inflight", "uncertain"}
             THEN [restartNotices[e] EXCEPT !.phase =
                      IF Mut = "rebootReowe" /\ @ # "uncertain" THEN "owed" ELSE "uncertain",
                  !.fresh = FALSE]
             ELSE restartNotices[e]]
    /\ UNCHANGED vars

Queue == CBSweepNext /\ UNCHANGED <<restartNotices, boots>>
DropInput ==
    /\ Mut = "dropInput" /\ CB.pending /\ CB.pastes = 0
    /\ observer' = [CB EXCEPT !.pending = FALSE]
    /\ UNCHANGED <<ObserverLegacy, restartNotices, boots>>

RestartNext == Queue \/ Reboot \/ DropInput
        \/ \E e \in Episodes : RecoverNotice(e) \/ Prepare(e) \/ Create(e) \/ Answer(e)
            \/ MissBeforeCreate(e) \/ Resolve(e) \/ GiveUp(e)
RestartSpec == RestartInit /\ [][RestartNext]_restartVars
    /\ WF_restartVars(CBLook /\ UNCHANGED <<restartNotices, boots>>)
    /\ WF_restartVars(CBNativeEnds /\ UNCHANGED <<restartNotices, boots>>)
    /\ WF_restartVars(CBPaste /\ UNCHANGED <<restartNotices, boots>>)
    /\ WF_restartVars(CBSettle /\ UNCHANGED <<restartNotices, boots>>)
    /\ WF_restartVars(CBSweepRecover /\ UNCHANGED <<restartNotices, boots>>)
    /\ \A e \in Episodes : WF_restartVars(RecoverNotice(e))
                          /\ WF_restartVars(Prepare(e))
                          /\ WF_restartVars(Create(e))
                          /\ WF_restartVars(Answer(e))
                          /\ WF_restartVars(Resolve(e) \/ GiveUp(e))

(* Duplicate suppression holds in every state; delivery is claimed only when closed. *)
NoticeAtMostOnce == \A e \in Episodes :
    restartNotices[e].copies <= (IF restartNotices[e].interrupted THEN 1 ELSE 0)
ClosedNoticeWasDelivered == \A e \in Episodes :
    restartNotices[e].phase = "closed" /\ restartNotices[e].interrupted => restartNotices[e].copies = 1
NoInputLoss == CB.pending \/ CB.pastes = 1
NextInputEventuallyProcessed == <>(CB.pastes = 1 /\ ~CB.pending)
NoticesEventuallyClosed == <>(\A e \in Episodes : restartNotices[e].phase = "closed")
(* Every intent ends delivered or as a kept unknown; only a decisive port promises closed. *)
NoticesEventuallySettled ==
    <>(\A e \in Episodes : restartNotices[e].phase \in {"closed", "unknown"})
=============================================================================
