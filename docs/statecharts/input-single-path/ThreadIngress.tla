-------------------------- MODULE ThreadIngress --------------------------
\* API/thread join projection of InputPath custody and TmuxSession cold/warm launch.
\* Production baseline: ad04f6f3ecc33e86e7dc0968822ce2ce3f087e90.
EXTENDS Naturals
CONSTANTS Target, InitialSession, ActualHost, DesiredHost, Mutation, CanCrash
VARIABLES phase, accepted, retained, delivered, reply, visibleFailure,
          status, session, usedHost, launches, crashes
VARIABLES hphase, haccepted, hretained, hdelivered, hreply, hfailure,
          hstatus, hactivity, hprobe, hreserved, hepoch, hreadEpoch,
          hstarted, hstaleEffect, hcrashes, horigin
hvars == <<hphase, haccepted, hretained, hdelivered, hreply, hfailure,
           hstatus, hactivity, hprobe, hreserved, hepoch, hreadEpoch,
           hstarted, hstaleEffect, hcrashes, horigin>>
vars == <<phase, accepted, retained, delivered, reply, visibleFailure,
          status, session, usedHost, launches, crashes>>
Authorized == Target \in {"boundThread", "boundChannel"}
HerdrFields ==
    /\ hphase = "request" /\ haccepted = FALSE /\ hretained = FALSE
    /\ hdelivered = 0 /\ hreply = 0 /\ hfailure = FALSE /\ hstatus = 0
    /\ hactivity = IF Mutation \in {"herdrBusy", "herdrMachineBusy"} THEN "busy"
                    ELSE IF Mutation = "herdrBoot" THEN "booting" ELSE "idle"
    /\ hprobe = "none" /\ hreserved = FALSE /\ hepoch = 1 /\ hreadEpoch = 0
    /\ hstarted = FALSE /\ hstaleEffect = FALSE /\ hcrashes = 0
    /\ horigin = "externalAPI"
Init ==
    /\ HerdrFields
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
BaseNext == Authorize \/ Admit \/ Ready \/ BootFails \/ ColdStart \/ WarmSubmit \/ PublishReply
        \/ StartOrSubmitFails \/ ShowFailure \/ Restart \/ Done
Next == BaseNext /\ UNCHANGED hvars
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

\* Shared owner port, entered only after external API admission is wired.
\* Idle read reservation is returned before the existing Idle driver rechecks.
HAuthorize ==
    /\ hphase = "request"
    /\ hphase' = IF Authorized THEN "checked" ELSE "denied"
    /\ hstatus' = IF Authorized THEN 0 ELSE IF Target = "runtimeUnavailable" THEN 503 ELSE 403
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure,
                   hactivity, hprobe, hreserved, hepoch, hreadEpoch,
                   hstarted, hstaleEffect, hcrashes, horigin>>
HAdmit ==
    /\ hphase = "checked"
    /\ IF Mutation = "herdrBefore"
       THEN /\ hphase' = "denied" /\ hstatus' = 422
            /\ UNCHANGED <<haccepted, hretained>>
       ELSE /\ haccepted' = TRUE /\ hretained' = TRUE /\ hstatus' = 200
            /\ hphase' = IF hactivity = "booting" THEN "held" ELSE "queued"
    /\ UNCHANGED <<hdelivered, hreply, hfailure, hactivity, hprobe, hreserved,
                   hepoch, hreadEpoch, hstarted, hstaleEffect, hcrashes, horigin>>
HObserve ==
    /\ hphase = "queued" /\ hactivity \in {"idle", "busy"}
    /\ hphase' = IF Mutation = "herdrMachineBusy" /\ hactivity = "busy" THEN "held" ELSE "probed"
    /\ hprobe' = hactivity /\ hreadEpoch' = hepoch
    /\ hreserved' = (hactivity = "idle")
    /\ horigin' = IF Mutation = "herdrReclassify" THEN "discordPerson" ELSE horigin
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hactivity, hepoch, hstarted, hstaleEffect, hcrashes>>
HReturnRead ==
    /\ hphase = "probed" /\ hprobe = "idle"
    /\ hphase' = "delegated"
    /\ hreserved' = (Mutation = "herdrKeepReservation")
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hactivity, hprobe, hepoch, hreadEpoch, hstarted,
                   hstaleEffect, hcrashes, horigin>>
\* Idle begins its own turn; Busy joins the already started native turn.
\* The first native effect is counted here; the later receipt is only an ACK.
HBegin ==
    /\ ((hphase = "delegated" /\ hprobe = "idle")
        \/ (hphase = "probed" /\ hprobe = "busy"))
    /\ (hreadEpoch = hepoch \/ Mutation = "herdrStaleEpoch")
    /\ hphase' = "started" /\ hstarted' = TRUE /\ hactivity' = "busy"
    /\ hdelivered' = hdelivered + 1
    /\ hstaleEffect' = (hstaleEffect \/ (hreadEpoch # hepoch))
    /\ UNCHANGED <<haccepted, hretained, hreply, hfailure, hstatus,
                   hprobe, hreserved, hepoch, hreadEpoch, hcrashes, horigin>>
HReceipt ==
    /\ hphase = "started"
    /\ hphase' = IF Mutation = "herdrDoubleSubmit" /\ hdelivered = 1 THEN "queued" ELSE "submitted"
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus, hactivity,
                   hprobe, hreserved, hepoch, hreadEpoch, hstarted,
                   hstaleEffect, hcrashes, horigin>>
HPost ==
    /\ hphase = "submitted" /\ hphase' = "done"
    /\ hreply' = hreply + 1 /\ hretained' = FALSE
    /\ UNCHANGED <<haccepted, hdelivered, hfailure, hstatus, hactivity, hprobe,
                   hreserved, hepoch, hreadEpoch, hstarted, hstaleEffect,
                   hcrashes, horigin>>
HReady ==
    /\ hphase = "held" /\ hactivity = "booting"
    /\ hactivity' = "idle"
    /\ hphase' = IF Mutation = "herdrNoWake" THEN "held" ELSE "queued"
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hprobe, hreserved, hepoch, hreadEpoch, hstarted,
                   hstaleEffect, hcrashes, horigin>>
\* Non-person HttpStart never borrows the person Busy injector.
HNativeEnd ==
    /\ hphase = "held" /\ hactivity = "busy" /\ Mutation = "herdrMachineBusy"
    /\ hactivity' = "idle" /\ hphase' = "queued"
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hprobe, hreserved, hepoch, hreadEpoch, hstarted,
                   hstaleEffect, hcrashes, horigin>>
HRestart ==
    /\ CanCrash /\ hcrashes = 0
    /\ hphase \in {"queued", "probed", "delegated", "started", "held"}
    /\ hcrashes' = 1 /\ hepoch' = hepoch + 1 /\ hreserved' = FALSE
    /\ IF Mutation = "herdrStaleEpoch" /\ hphase = "delegated"
       THEN /\ hphase' = "delegated" /\ hactivity' = "idle"
            /\ UNCHANGED <<hprobe, hreadEpoch>>
       ELSE /\ hphase' = IF hphase = "started" THEN "uncertain" ELSE "held"
            /\ hactivity' = "booting"
            /\ hprobe' = "none" /\ hreadEpoch' = 0
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hstarted, hstaleEffect, horigin>>
\* A crash after a native effect is not permission to repeat the effect.
HUncertainFailure ==
    /\ hphase = "uncertain" /\ hphase' = "failed"
    /\ hfailure' = TRUE /\ hretained' = FALSE
    /\ UNCHANGED <<haccepted, hdelivered, hreply, hstatus, hactivity, hprobe,
                   hreserved, hepoch, hreadEpoch, hstarted, hstaleEffect,
                   hcrashes, horigin>>
HFail ==
    /\ Mutation = "herdrFailure" /\ hphase = "queued"
    /\ hphase' = "failed" /\ hfailure' = TRUE /\ hretained' = FALSE
    /\ UNCHANGED <<haccepted, hdelivered, hreply, hstatus, hactivity, hprobe,
                   hreserved, hepoch, hreadEpoch, hstarted, hstaleEffect,
                   hcrashes, horigin>>
HDone == /\ hphase \in {"done", "failed", "denied"} /\ UNCHANGED hvars
HNext == (HAuthorize \/ HAdmit \/ HObserve \/ HReturnRead \/ HBegin \/ HReceipt
          \/ HPost \/ HReady \/ HNativeEnd \/ HRestart \/ HUncertainFailure \/ HFail \/ HDone) /\ UNCHANGED vars
HerdrSpec == Init /\ [][HNext]_hvars
    /\ WF_hvars(HAuthorize) /\ WF_hvars(HAdmit) /\ WF_hvars(HObserve \/ HFail)
    /\ WF_hvars(HReturnRead) /\ WF_hvars(HBegin) /\ WF_hvars(HReceipt)
    /\ WF_hvars(HPost) /\ WF_hvars(HReady) /\ WF_hvars(HNativeEnd) /\ WF_hvars(HUncertainFailure)
HerdrExactlyOnce == hdelivered <= 1 /\ hreply <= 1
HerdrNoSilentLoss == haccepted => hretained \/ hdelivered = 1 \/ hfailure
HerdrRejectedNoEffects == hphase = "denied" =>
    hstatus \in {403, 422, 503} /\ ~haccepted /\ ~hretained /\ hdelivered = 0
    /\ hreply = 0 /\ ~hstarted /\ ~hreserved
HerdrExternalWired == Authorized => hphase # "denied"
HerdrReservationReturned == hphase \in {"started", "submitted", "done"} => ~hreserved
HerdrCurrentExecutor == ~hstaleEffect
HerdrSourcePreserved == horigin = "externalAPI"
HerdrAcceptedStarts == haccepted ~> hstarted
HerdrAcceptedCompletes == haccepted ~> (hdelivered = 1 \/ hfailure)
HerdrDeliveredGetsOutcome == hdelivered = 1 ~> (hreply = 1 \/ hfailure)
=============================================================================
