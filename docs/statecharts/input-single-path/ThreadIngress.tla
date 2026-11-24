-------------------------- MODULE ThreadIngress --------------------------
\* API/thread join projection of InputPath custody and TmuxSession cold/warm launch.
\* Production baseline: ad04f6f3ecc33e86e7dc0968822ce2ce3f087e90.
EXTENDS Naturals
CONSTANTS Target, InitialSession, ActualHost, DesiredHost, Mutation, CanCrash
VARIABLES phase, accepted, retained, delivered, reply, visibleFailure,
          status, session, usedHost, launches, crashes
VARIABLES hphase, haccepted, hretained, hdelivered, hreply, hfailure,
          hstatus, hactivity, hprobe, hreserved, hepoch, hreadEpoch,
          hstarted, hstaleEffect, hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy
hvars == <<hphase, haccepted, hretained, hdelivered, hreply, hfailure,
           hstatus, hactivity, hprobe, hreserved, hepoch, hreadEpoch,
           hstarted, hstaleEffect, hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
vars == <<phase, accepted, retained, delivered, reply, visibleFailure,
          status, session, usedHost, launches, crashes>>
\* #8372: cold resume of a Claude thread child whose stored provider session has no verified binding.
VARIABLES rstored, rbinding, rpane, rfate, rposts, rnotice, rprompt, rshown,
          rfreshPending, rfreshOnBound, rcrashes, rruns, rlaunches
rvars == <<rstored, rbinding, rpane, rfate, rposts, rnotice, rprompt, rshown,
           rfreshPending, rfreshOnBound, rcrashes, rruns, rlaunches>>
Authorized == Target \in {"boundThread", "boundChannel"}
LiveLocalContract == Mutation \in {"liveLocal", "beforeLiveLocal", "skipLiveCheck"}
ThreadExecutionUnavailable ==
    Target = "remoteThread" \/ (Target = "boundThread" /\ InitialSession # "live")
HerdrFields ==
    /\ hphase = "request" /\ haccepted = FALSE /\ hretained = FALSE
    /\ hdelivered = 0 /\ hreply = 0 /\ hfailure = FALSE /\ hstatus = 0
    /\ hactivity = IF Mutation \in {"herdrBusy", "herdrMachineBusy", "herdrMachineJoinsBusy"} THEN "busy"
                    ELSE IF Mutation = "herdrBoot" THEN "booting" ELSE "idle"
    /\ hprobe = "none" /\ hreserved = FALSE /\ hepoch = 1 /\ hreadEpoch = 0
    /\ hstarted = FALSE /\ hstaleEffect = FALSE /\ hcrashes = 0
    /\ horigin = "externalAPI"
    /\ hnativeChanged = FALSE /\ hjoinedBusy = FALSE /\ hidleWhileBusy = FALSE
    /\ hmachine = (Mutation \in {"herdrMachineBusy", "herdrMachineJoinsBusy"})
RInputs == {1, 2}
RNoRoutineRun == Mutation \in {"resumeCurrentNoRun", "resumeFreshNoRun"}
RFix == CASE Mutation \in {"resumeFresh", "resumeFreshNoRun", "resumeFreshAll", "resumeFreshLate",
                             "resumeFreshNoRecent", "resumeFreshSilentRestart"} -> "fresh"
          [] Mutation = "resumeRecord" -> "record"
          [] Mutation = "resumeRecordLegacy" -> "recordLegacy"
          [] OTHER -> "current"
\* Values from the entry table: E1 routine process thread = unboundStored, E4/E6 TUI thread = boundStored,
\* E5 new thread or cleared session = noneStored.
RFields ==
    /\ rstored = CASE InitialSession = "unboundStored" -> "process"
                    [] InitialSession = "boundStored" -> "tui"
                    [] OTHER -> "none"
    /\ rbinding = CASE InitialSession = "boundStored" -> "hook"
                     [] InitialSession = "unboundStored" /\ RFix = "record" -> "forged"
                     [] OTHER -> "none"
    /\ rpane = "cold"
    /\ rfate = [k \in RInputs |-> "absent"]
    /\ rposts = [k \in RInputs |-> 0]
    /\ rnotice = [k \in RInputs |-> "none"]
    /\ rprompt = [k \in RInputs |-> "none"]
    /\ rshown = [k \in RInputs |-> FALSE]
    /\ rfreshPending = FALSE /\ rfreshOnBound = FALSE
    /\ rcrashes = 0 /\ rruns = 0 /\ rlaunches = 0
Init ==
    /\ HerdrFields
    /\ RFields
    /\ phase = "request" /\ accepted = FALSE /\ retained = FALSE
    /\ delivered = 0 /\ reply = 0 /\ visibleFailure = FALSE /\ status = 0
    /\ session = InitialSession /\ usedHost = "none" /\ launches = 0 /\ crashes = 0

\* Authorization precedes custody, bootstrap, mapping and host/pane effects.
Authorize ==
    /\ phase = "request"
    /\ IF Mutation = "beforeLiveLocal" /\ Target \in {"boundThread", "remoteThread"}
       THEN /\ phase' = "denied" /\ status' = 403
       ELSE IF LiveLocalContract /\ ThreadExecutionUnavailable /\ Mutation # "skipLiveCheck"
       THEN /\ phase' = "denied" /\ status' = 503
       ELSE IF (Authorized /\ ~(Mutation = "before" /\ Target = "boundThread"))
               \/ (Mutation = "parentOpen" /\ Target = "parentUnavailable")
               \/ (Mutation = "skipLiveCheck" /\ Target = "remoteThread")
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
Next == BaseNext /\ UNCHANGED hvars /\ UNCHANGED rvars
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
BoundThreadAdmitted ==
    Target = "boundThread" /\ ~(LiveLocalContract /\ ThreadExecutionUnavailable) => phase # "denied"
LiveLocalRefusal ==
    LiveLocalContract /\ ThreadExecutionUnavailable /\ phase # "request" =>
      status = 503 /\ phase = "denied" /\ ~accepted /\ ~retained
      /\ delivered = 0 /\ reply = 0 /\ launches = 0
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
                   hstarted, hstaleEffect, hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HAdmit ==
    /\ hphase = "checked"
    /\ IF Mutation = "herdrBefore"
       THEN /\ hphase' = "denied" /\ hstatus' = 422
            /\ UNCHANGED <<haccepted, hretained>>
       ELSE /\ haccepted' = TRUE /\ hretained' = TRUE /\ hstatus' = 200
            /\ hphase' = IF hactivity = "booting" THEN "held" ELSE "queued"
    /\ UNCHANGED <<hdelivered, hreply, hfailure, hactivity, hprobe, hreserved,
                   hepoch, hreadEpoch, hstarted, hstaleEffect, hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HObserve ==
    /\ hphase = "queued" /\ hactivity \in {"idle", "busy"}
    /\ hphase' = IF hmachine /\ Mutation # "herdrMachineJoinsBusy" /\ hactivity = "busy" THEN "held" ELSE "probed"
    /\ hprobe' = hactivity /\ hreadEpoch' = hepoch
    /\ hreserved' = (hactivity = "idle")
    /\ horigin' = IF Mutation = "herdrReclassify" THEN "discordPerson" ELSE horigin
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hactivity, hepoch, hstarted, hstaleEffect, hcrashes, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HReturnRead ==
    /\ hphase = "probed" /\ hprobe = "idle"
    /\ hphase' = "delegated"
    /\ hreserved' = (Mutation = "herdrKeepReservation")
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hactivity, hprobe, hepoch, hreadEpoch, hstarted,
                   hstaleEffect, hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
\* Native activity can change in the same incarnation after the read proof returns.
HNativeStart ==
    /\ hphase \in {"delegated", "driverReserved"} /\ hactivity = "idle" /\ ~hnativeChanged
    /\ hactivity' = "busy" /\ hnativeChanged' = TRUE /\ hreserved' = FALSE
    /\ UNCHANGED <<hphase, haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hprobe, hepoch, hreadEpoch, hstarted, hstaleEffect, hcrashes,
                   horigin, hmachine, hjoinedBusy, hidleWhileBusy>>
HDriverReserve ==
    /\ hphase = "delegated" /\ hactivity = "idle"
    /\ hphase' = "driverReserved" /\ hreserved' = TRUE /\ hreadEpoch' = hepoch
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hactivity, hprobe, hepoch, hstarted, hstaleEffect, hcrashes,
                   horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HReobserve ==
    /\ hphase \in {"delegated", "driverReserved"}
    /\ (hactivity # "idle" \/ hreadEpoch # hepoch \/ (hphase = "driverReserved" /\ ~hreserved))
    /\ hphase' = "queued" /\ hreserved' = FALSE /\ hprobe' = "none"
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hactivity, hepoch, hreadEpoch, hstarted, hstaleEffect, hcrashes,
                   horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
\* The driver reserves again and checks immediately before the first native effect.
HBegin ==
    /\ ((hphase = "driverReserved" /\ hprobe = "idle" /\ hactivity = "idle" /\ hreserved)
        \/ (Mutation = "herdrNoIdleRecheck" /\ hphase \in {"delegated", "driverReserved"})
        \/ (hphase = "probed" /\ hprobe = "busy")
        \/ (Mutation = "herdrStaleEpoch" /\ hphase = "delegated" /\ hactivity = "idle" /\ hreadEpoch # hepoch))
    /\ (hreadEpoch = hepoch \/ Mutation = "herdrStaleEpoch")
    /\ hphase' = "started" /\ hstarted' = TRUE /\ hactivity' = "busy"
    /\ hdelivered' = hdelivered + 1 /\ hreserved' = FALSE
    /\ hstaleEffect' = (hstaleEffect \/ (hreadEpoch # hepoch))
    /\ hjoinedBusy' = (hjoinedBusy \/ (hprobe = "busy" /\ hactivity = "busy"))
    /\ hidleWhileBusy' = (hidleWhileBusy \/ (hprobe = "idle" /\ hactivity = "busy"))
    /\ UNCHANGED <<haccepted, hretained, hreply, hfailure, hstatus, hprobe,
                   hepoch, hreadEpoch, hcrashes, horigin, hnativeChanged, hmachine>>
HReceipt ==
    /\ hphase = "started"
    /\ hphase' = IF Mutation = "herdrDoubleSubmit" /\ hdelivered = 1 THEN "queued" ELSE "submitted"
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus, hactivity,
                   hprobe, hreserved, hepoch, hreadEpoch, hstarted,
                   hstaleEffect, hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HPost ==
    /\ hphase = "submitted" /\ hphase' = "done"
    /\ hreply' = hreply + 1 /\ hretained' = FALSE
    /\ UNCHANGED <<haccepted, hdelivered, hfailure, hstatus, hactivity, hprobe,
                   hreserved, hepoch, hreadEpoch, hstarted, hstaleEffect,
                   hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HReady ==
    /\ hphase = "held" /\ hactivity = "booting"
    /\ hactivity' = "idle"
    /\ hphase' = IF Mutation = "herdrNoWake" THEN "held" ELSE "queued"
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hprobe, hreserved, hepoch, hreadEpoch, hstarted,
                   hstaleEffect, hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
\* Non-person HttpStart never borrows the person Busy injector.
HNativeEnd ==
    /\ hphase = "held" /\ hactivity = "busy" /\ hmachine
    /\ hactivity' = "idle" /\ hphase' = "queued"
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hprobe, hreserved, hepoch, hreadEpoch, hstarted,
                   hstaleEffect, hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HRestart ==
    /\ CanCrash /\ hcrashes = 0
    /\ hphase \in {"queued", "probed", "delegated", "driverReserved", "started", "held"}
    /\ hcrashes' = 1 /\ hepoch' = hepoch + 1 /\ hreserved' = FALSE
    /\ IF Mutation = "herdrStaleEpoch" /\ hphase = "delegated"
       THEN /\ hphase' = "delegated" /\ hactivity' = "idle"
            /\ UNCHANGED <<hprobe, hreadEpoch>>
       ELSE /\ hphase' = IF hphase = "started" THEN "uncertain" ELSE "held"
            /\ hactivity' = "booting"
            /\ hprobe' = "none" /\ hreadEpoch' = 0
    /\ UNCHANGED <<haccepted, hretained, hdelivered, hreply, hfailure, hstatus,
                   hstarted, hstaleEffect, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
\* A crash after a native effect is not permission to repeat the effect.
HUncertainFailure ==
    /\ hphase = "uncertain" /\ hphase' = "failed"
    /\ hfailure' = TRUE /\ hretained' = FALSE
    /\ UNCHANGED <<haccepted, hdelivered, hreply, hstatus, hactivity, hprobe,
                   hreserved, hepoch, hreadEpoch, hstarted, hstaleEffect,
                   hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HFail ==
    /\ Mutation = "herdrFailure" /\ hphase = "queued"
    /\ hphase' = "failed" /\ hfailure' = TRUE /\ hretained' = FALSE
    /\ UNCHANGED <<haccepted, hdelivered, hreply, hstatus, hactivity, hprobe,
                   hreserved, hepoch, hreadEpoch, hstarted, hstaleEffect,
                   hcrashes, horigin, hnativeChanged, hmachine, hjoinedBusy, hidleWhileBusy>>
HDone == /\ hphase \in {"done", "failed", "denied"} /\ UNCHANGED hvars
HNext == (HAuthorize \/ HAdmit \/ HObserve \/ HReturnRead \/ HDriverReserve \/ HReobserve \/ HNativeStart \/ HBegin \/ HReceipt
          \/ HPost \/ HReady \/ HNativeEnd \/ HRestart \/ HUncertainFailure \/ HFail \/ HDone) /\ UNCHANGED vars /\ UNCHANGED rvars
HerdrSpec == Init /\ [][HNext]_hvars
    /\ WF_hvars(HAuthorize) /\ WF_hvars(HAdmit) /\ WF_hvars(HObserve \/ HFail)
    /\ WF_hvars(HReturnRead) /\ WF_hvars(HDriverReserve) /\ WF_hvars(HReobserve) /\ WF_hvars(HBegin) /\ WF_hvars(HReceipt)
    /\ WF_hvars(HPost) /\ WF_hvars(HReady) /\ WF_hvars(HNativeEnd) /\ WF_hvars(HUncertainFailure)
HerdrExactlyOnce == hdelivered <= 1 /\ hreply <= 1
HerdrNoSilentLoss == haccepted => hretained \/ hdelivered = 1 \/ hfailure
HerdrRejectedNoEffects == hphase = "denied" =>
    hstatus \in {403, 422, 503} /\ ~haccepted /\ ~hretained /\ hdelivered = 0
    /\ hreply = 0 /\ ~hstarted /\ ~hreserved
HerdrExternalWired == Authorized => hphase # "denied"
HerdrReservationReturned == hphase \in {"delegated", "started", "submitted", "done"} => ~hreserved
HNoIdleStartWhileBusy == ~hidleWhileBusy
MachineNeverJoinsBusy == hmachine => ~hjoinedBusy
HerdrCurrentExecutor == ~hstaleEffect
HerdrSourcePreserved == horigin = "externalAPI"
HerdrAcceptedStarts == haccepted ~> hstarted
HerdrAcceptedCompletes == haccepted ~> (hdelivered = 1 \/ hfailure)
HerdrDeliveredGetsOutcome == hdelivered = 1 ~> (hreply = 1 \/ hfailure)
\* ---------------------------------------------------------------------------
\* #8372 resume projection. One thread child under a ClaudeTui parent, writer policy selected
\* (`claude_thread.rs:14-23`). Inputs arrive one at a time through the thread mailbox.
\* rstored: the provider session the thread's session row names
\*   (`session_preparation.rs:48-63` DB restore, or the runtime cache).
\* rbinding: binding_events evidence for rstored: hook (writer verified), none, forged (written
\*   by the headless end without a hook, repair b).
\* rfate: absent | queued | submitted | answered | retained (owner_start_failed, notice says resend)
\*   | uncertain (submitted when dcserver restarted, failure notice still owed)
\*   | failed (terminal, no automatic resend). rshown: the failure notice reached the thread.
\* rnotice "fresh": the input started a fresh session in place of the stored unverified one.
\* Scope: a late-adopt child still under the reservation policy (Deferred or parked,
\* `claude_thread.rs` launch_policy). A child whose adoption committed is not reserved and is
\* outside this projection.
RFrame == UNCHANGED vars /\ UNCHANGED hvars
RFresh(k, bound) ==
    /\ rpane' = "live" /\ rfreshPending' = TRUE /\ rlaunches' = rlaunches + 1
    /\ rfate' = [rfate EXCEPT ![k] = "submitted"]
    /\ rprompt' = [rprompt EXCEPT ![k] =
          IF rstored # "none" /\ Mutation = "resumeFreshNoRecent" THEN @
          ELSE IF rstored = "none" \/ Mutation # "resumeFreshLate" THEN "withRecent" ELSE "bare"]
    /\ rnotice' = [rnotice EXCEPT ![k] = IF rstored = "none" THEN "none" ELSE "fresh"]
    /\ rfreshOnBound' = (rfreshOnBound \/ bound)
    /\ UNCHANGED <<rstored, rbinding, rposts, rshown, rcrashes, rruns>>
RResume(k) ==
    /\ rpane' = "live" /\ rlaunches' = rlaunches + 1
    /\ rfate' = [rfate EXCEPT ![k] = "submitted"]
    \* The resumed pane's SessionStart hook records a real verified binding for its tmux.
    /\ rbinding' = "hook"
    /\ UNCHANGED <<rstored, rposts, rnotice, rprompt, rshown, rfreshPending, rfreshOnBound, rcrashes, rruns>>
RRefuse(k) ==
    \* `claude_fold.rs:595-601` -> `start_failure.rs:234` -> retained notice (`retained_notice.rs:40`).
    \* Mutant resumeSilentFail ends the input without telling anyone.
    /\ rfate' = [rfate EXCEPT ![k] = IF Mutation = "resumeSilentFail" THEN "failed" ELSE "retained"]
    /\ rnotice' = [rnotice EXCEPT ![k] = IF Mutation = "resumeSilentFail" THEN @ ELSE "resend"]
    /\ UNCHANGED <<rstored, rbinding, rpane, rposts, rprompt, rshown, rfreshPending, rfreshOnBound,
                   rcrashes, rruns, rlaunches>>

RPersonSend(k) ==
    /\ RFrame
    /\ rfate[k] = "absent"
    /\ IF k = 1 THEN TRUE ELSE rfate[k - 1] \in {"answered", "retained", "failed"}
    /\ rfate' = [rfate EXCEPT ![k] = "queued"]
    /\ UNCHANGED <<rstored, rbinding, rpane, rposts, rnotice, rprompt, rshown, rfreshPending,
                   rfreshOnBound, rcrashes, rruns, rlaunches>>
\* Cold launch of the thread's pane (`claude.rs:1799` -> `tui_session_launch.rs:78-86`).
RLaunch(k) ==
    /\ RFrame
    /\ rfate[k] = "queued" /\ rpane = "cold"
    /\ CASE rstored = "none" -> RFresh(k, FALSE)
         [] rbinding \in {"hook", "forged"} ->
              IF Mutation = "resumeFreshAll" /\ rbinding = "hook" THEN RFresh(k, TRUE) ELSE RResume(k)
         [] OTHER -> IF RFix = "fresh" THEN RFresh(k, FALSE) ELSE RRefuse(k)
RWarm(k) ==
    /\ RFrame
    /\ rfate[k] = "queued" /\ rpane = "live"
    /\ rfate' = [rfate EXCEPT ![k] = "submitted"]
    /\ UNCHANGED <<rstored, rbinding, rpane, rposts, rnotice, rprompt, rshown, rfreshPending,
                   rfreshOnBound, rcrashes, rruns, rlaunches>>
\* Completion posts once and persists the provider session (`completion_postlude.rs:389`).
RAnswer(k) ==
    /\ RFrame
    /\ rfate[k] = "submitted"
    /\ rfate' = [rfate EXCEPT ![k] = "answered"]
    /\ rposts' = [rposts EXCEPT ![k] = @ + 1]
    /\ IF rfreshPending
       THEN /\ rstored' = "tui" /\ rbinding' = "hook" /\ rfreshPending' = FALSE
       ELSE UNCHANGED <<rstored, rbinding, rfreshPending>>
    /\ UNCHANGED <<rpane, rnotice, rprompt, rshown, rfreshOnBound, rcrashes, rruns, rlaunches>>
RBusy == \E k \in RInputs : rfate[k] \in {"queued", "submitted", "uncertain"}
\* Idle reclaim / stop-kill of an idle pane keeps the stored session.
RIdleReap ==
    /\ RFrame
    /\ rpane = "live" /\ ~RBusy /\ ~rfreshPending
    /\ rpane' = "cold"
    /\ UNCHANGED <<rstored, rbinding, rfate, rposts, rnotice, rprompt, rshown, rfreshPending,
                   rfreshOnBound, rcrashes, rruns, rlaunches>>
\* /clear or a cleared provider session: the next start is a plain fresh start.
RClear ==
    /\ RFrame
    /\ ~RBusy /\ rstored # "none" /\ ~rfreshPending
    /\ rstored' = "none" /\ rbinding' = "none" /\ rpane' = "cold"
    /\ UNCHANGED <<rfate, rposts, rnotice, rprompt, rshown, rfreshPending, rfreshOnBound, rcrashes,
                   rruns, rlaunches>>
\* The routine's next process run in the same thread re-points the thread session at its own
\* process session (log: tmux=AgentDesk-claude-routine-process-<turn>-...).
RRoutineNext ==
    /\ RFrame
    /\ ~RNoRoutineRun /\ rruns = 0 /\ ~RBusy /\ ~rfreshPending
    /\ rruns' = 1 /\ rstored' = "process" /\ rpane' = "cold"
    /\ rbinding' = IF RFix \in {"record", "recordLegacy"} THEN "forged" ELSE "none"
    /\ UNCHANGED <<rfate, rposts, rnotice, rprompt, rshown, rfreshPending, rfreshOnBound, rcrashes, rlaunches>>
\* dcserver restart: pane state and the unpersisted fresh session id are lost; the DB row and
\* binding log survive; a queued input is re-driven; a submitted one is uncertain until its failure
\* notice is posted (RShowFailure).
RRestart ==
    /\ RFrame
    /\ CanCrash /\ rcrashes = 0
    /\ rcrashes' = 1 /\ rpane' = "cold" /\ rfreshPending' = FALSE
    /\ rfate' = [k \in RInputs |-> IF rfate[k] = "submitted" THEN "uncertain" ELSE rfate[k]]
    /\ UNCHANGED <<rstored, rbinding, rposts, rnotice, rprompt, rshown, rfreshOnBound, rruns, rlaunches>>
\* The owed failure notice of an uncertain input; mutant resumeFreshSilentRestart posts none.
RShowFailure(k) ==
    /\ RFrame
    /\ rfate[k] = "uncertain"
    /\ rfate' = [rfate EXCEPT ![k] = "failed"]
    /\ rshown' = [rshown EXCEPT ![k] = Mutation # "resumeFreshSilentRestart"]
    /\ UNCHANGED <<rstored, rbinding, rpane, rposts, rnotice, rprompt, rfreshPending,
                   rfreshOnBound, rcrashes, rruns, rlaunches>>
RDone ==
    /\ RFrame
    /\ \A k \in RInputs : rfate[k] \in {"answered", "retained", "failed"}
    /\ UNCHANGED rvars
RNext == \/ \E k \in RInputs : RPersonSend(k)
         \/ \E k \in RInputs : RLaunch(k)
         \/ \E k \in RInputs : RWarm(k)
         \/ \E k \in RInputs : RAnswer(k)
         \/ \E k \in RInputs : RShowFailure(k)
         \/ RIdleReap \/ RClear \/ RRoutineNext \/ RRestart \/ RDone
ResumeSpec == Init /\ [][RNext]_rvars
    /\ \A k \in RInputs : WF_rvars(RPersonSend(k)) /\ WF_rvars(RLaunch(k))
                         /\ WF_rvars(RWarm(k)) /\ WF_rvars(RAnswer(k))
                         /\ WF_rvars(RShowFailure(k))

RNoSilentLoss == \A k \in RInputs :
    /\ rfate[k] \in {"absent", "queued", "submitted", "answered", "retained", "uncertain", "failed"}
    /\ (rfate[k] = "retained" => rnotice[k] = "resend")
\* A person's input ends delivered to the session or with a failure notice the person can see.
RNoSilentFailure == \A k \in RInputs : rfate[k] = "failed" => rshown[k]
RAnswerOnce == \A k \in RInputs :
    rposts[k] <= 1 /\ (rfate[k] = "answered" <=> rposts[k] = 1)
RVerifiedResumeKept == ~rfreshOnBound
RVerifiedMeansObserved == rbinding # "forged"
\* A fresh start that replaces a stored session carries the thread's recent pairs in its first prompt.
RFreshCarriesRecent == \A k \in RInputs :
    rprompt[k] # "bare" /\ (rnotice[k] = "fresh" => rprompt[k] = "withRecent")
RPersonServed == \A k \in RInputs :
    (rfate[k] = "queued") ~> (rfate[k] = "answered" \/ (rfate[k] = "failed" /\ rshown[k]))
=============================================================================
