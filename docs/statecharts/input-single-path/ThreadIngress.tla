-------------------------- MODULE ThreadIngress --------------------------
\* API/thread join projection of InputPath custody and TmuxSession cold/warm launch.
\* Production baseline: 33d1f4b5a9dce7023e2d0014e45a05ed6c530bba.
EXTENDS Naturals
CONSTANTS Target, InitialSession, ActualHost, DesiredHost, Mutation, CanCrash
VARIABLES phase, accepted, retained, delivered, reply, visibleFailure,
          status, session, usedHost, launches, crashes
vars == <<phase, accepted, retained, delivered, reply, visibleFailure,
          status, session, usedHost, launches, crashes>>
Authorized == Target \in {"boundThread", "boundChannel"}
Init ==
    /\ phase = "request" /\ accepted = FALSE /\ retained = FALSE
    /\ delivered = 0 /\ reply = 0 /\ visibleFailure = FALSE /\ status = 0
    /\ session = InitialSession /\ usedHost = "none" /\ launches = 0 /\ crashes = 0

\* Authorization precedes custody, bootstrap, mapping and host/pane effects.
Authorize ==
    /\ phase = "request"
    /\ IF (Authorized /\ ~(Mutation = "before" /\ Target = "boundThread"))
          \/ (Mutation = "parentOpen" /\ Target = "parentUnavailable")
       THEN /\ phase' = "authorized" /\ status' = 0
       ELSE /\ phase' = "denied"
            /\ status' = IF Target \in {"parentUnavailable", "parentMalformed"} THEN 503 ELSE 403
    /\ UNCHANGED <<accepted, retained, delivered, reply, visibleFailure,
                   session, usedHost, launches, crashes>>
Admit ==
    /\ phase = "authorized"
    /\ accepted' = TRUE /\ retained' = TRUE /\ status' = 200
    /\ phase' = IF session = "booting" THEN "held" ELSE "queued"
    /\ UNCHANGED <<delivered, reply, visibleFailure, session, usedHost, launches, crashes>>

\* Ready wake and retained custody are inherited ports, not new polling loops.
Ready ==
    /\ session = "booting" /\ phase = "held"
    /\ session' = "live"
    /\ phase' = IF Mutation = "noReadyWake" THEN "held" ELSE "queued"
    /\ UNCHANGED <<accepted, retained, delivered, reply, visibleFailure,
                   status, usedHost, launches, crashes>>


BootFails ==
    /\ session = "booting" /\ phase = "held"
    /\ phase' = "failurePending"
    /\ UNCHANGED <<accepted, retained, delivered, reply, visibleFailure,
                   status, session, usedHost, launches, crashes>>

\* Startup consumes the source prompt once; a warm route only injects once.
ColdStart ==
    /\ phase = "queued" /\ session = "absent"
    /\ session' = "live" /\ launches' = launches + 1 /\ usedHost' = DesiredHost
    /\ delivered' = delivered + 1
    /\ phase' = IF Mutation = "doubleSubmit" THEN "queued" ELSE "submitted"
    /\ UNCHANGED <<accepted, retained, reply, visibleFailure, status, crashes>>
WarmSubmit ==
    /\ phase = "queued" /\ session = "live"
    /\ usedHost' = IF Mutation = "policyOverridesLive" THEN DesiredHost ELSE ActualHost
    /\ delivered' = delivered + 1 /\ phase' = "submitted"
    /\ UNCHANGED <<accepted, retained, reply, visibleFailure, status, session, launches, crashes>>
PublishReply ==
    /\ phase = "submitted"
    /\ reply' = reply + 1 /\ phase' = "done" /\ retained' = FALSE
    /\ UNCHANGED <<accepted, delivered, visibleFailure, status, session, usedHost, launches, crashes>>

\* Definite failure remains owned until a failure is actually visible.
StartOrSubmitFails ==
    /\ phase = "queued" /\ phase' = "failurePending"
    /\ UNCHANGED <<accepted, retained, delivered, reply, visibleFailure,
                   status, session, usedHost, launches, crashes>>
ShowFailure ==
    /\ phase \in {"failurePending", "uncertain"}
    /\ phase' = "failed" /\ visibleFailure' = TRUE /\ retained' = FALSE
    /\ UNCHANGED <<accepted, delivered, reply, status, session, usedHost, launches, crashes>>

\* Crash after host effect is uncertain, so recovery never blindly resubmits it.
Restart ==
    /\ CanCrash /\ crashes = 0
    /\ phase \in {"held", "queued", "submitted", "failurePending"}
    /\ crashes' = 1
    /\ IF Mutation = "dropRestart"
       THEN /\ phase' = "dropped" /\ retained' = FALSE
       ELSE /\ phase' = IF delivered = 1 THEN "uncertain" ELSE "failurePending"
            /\ UNCHANGED retained
    /\ UNCHANGED <<accepted, delivered, reply, visibleFailure, status, session, usedHost, launches>>
Done == /\ phase \in {"done", "failed", "denied", "dropped"}
        /\ UNCHANGED vars
Next == Authorize \/ Admit \/ Ready \/ BootFails \/ ColdStart \/ WarmSubmit \/ PublishReply
        \/ StartOrSubmitFails \/ ShowFailure \/ Restart \/ Done
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Authorize) /\ WF_vars(Admit) /\ WF_vars(Ready \/ BootFails)
        /\ WF_vars(ColdStart \/ WarmSubmit \/ StartOrSubmitFails)
        /\ WF_vars(PublishReply) /\ WF_vars(ShowFailure)

NoDuplicate == delivered <= 1 /\ reply <= 1 /\ launches <= 1
NoSilentLoss == accepted => retained \/ delivered = 1 \/ visibleFailure
RejectedNoEffects == phase = "denied" =>
    ~accepted /\ ~retained /\ delivered = 0 /\ reply = 0 /\ launches = 0 /\ usedHost = "none"
ForeignForbidden == Target \in {"foreignThread", "foreignChannel"} /\ phase # "request" => status = 403
ParentFailClosed == Target \in {"parentUnavailable", "parentMalformed"} /\ phase # "request" =>
    status = 503 /\ ~accepted /\ ~retained /\ delivered = 0 /\ reply = 0 /\ launches = 0
BoundThreadAdmitted == Target = "boundThread" => phase # "denied"
SuccessBacked == phase = "done" => delivered = 1 /\ reply = 1
HttpAcceptanceBacked == status = 200 => accepted
LiveActualHostPreserved == InitialSession = "live" /\ delivered > 0 => usedHost = ActualHost
AcceptedCompletes == accepted ~> (delivered = 1 \/ visibleFailure)
DeliveredGetsOutcome == (delivered = 1) ~> (reply = 1 \/ visibleFailure)
=============================================================================
