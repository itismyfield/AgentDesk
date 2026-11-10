---------------------------- MODULE WriterWake ----------------------------
EXTENDS Naturals
CONSTANTS ParkRule, Case
VARIABLES presence, loss, drain, unsealed, successor, writer, parked, alarm,
          boots, restarted, input, newSource, posted, oldDebt
vars == <<presence, loss, drain, unsealed, successor, writer, parked, alarm,
          boots, restarted, input, newSource, posted, oldDebt>>

\* Source baseline 7a0c819e12; park-and-run contract, issue 7845.
\* One unretired missing cursor, no live capture; retained spool debt may remain.
\* drain is unread capture debt; unsealed is unfinished derivation, not posted debt.
\* family has no EOF witness; superseded models durable successor/link seq.
BaseInit ==
        /\ presence \in IF Case = "active" THEN {"present", "unknown"}
                        ELSE IF Case = "superseded" THEN {"present"}
                        ELSE {"absent"}
        /\ loss \in IF Case = "unsafe" THEN {"missing", "replaced", "denied", "shrunk"}
                    ELSE {"missing"}
        /\ drain \in IF Case = "unsafe" THEN {TRUE, FALSE} ELSE {FALSE}
        /\ unsealed \in IF Case = "unsafe" THEN {TRUE, FALSE} ELSE {FALSE}
        /\ successor = (Case = "superseded")
        /\ writer = "booting" /\ parked = FALSE /\ alarm \in {FALSE, TRUE} /\ boots = 1
        /\ restarted = FALSE
        /\ input = (IF Case = "superseded" THEN "launched" ELSE "waiting")
        /\ newSource = (Case = "superseded") /\ posted = 0 /\ oldDebt = 1

CanPark == loss = "missing" /\ ~drain
           /\ (successor \/ presence = "absent")
           /\ (successor \/ ~unsealed)
Min2(n) == IF n > 2 THEN 2 ELSE n

\* Every gateway, execution/standby and owner_cleanup entry reaches this resume.
ResumePark == /\ writer = "booting" /\ ParkRule /\ CanPark
              /\ writer' = "running" /\ parked' = TRUE /\ alarm' = FALSE
              /\ UNCHANGED <<presence,loss,drain,unsealed,successor,boots,
                              restarted,input,newSource,posted,oldDebt>>
ResumeHalt == /\ writer = "booting" /\ ~(ParkRule /\ CanPark)
              /\ writer' = "halted" /\ alarm' = TRUE
              /\ UNCHANGED <<presence,loss,drain,unsealed,successor,parked,boots,
                              restarted,input,newSource,posted,oldDebt>>
HostEnds == /\ writer = "halted" /\ writer' = "ended"
            /\ UNCHANGED <<presence,loss,drain,unsealed,successor,parked,alarm,
                            boots,restarted,input,newSource,posted,oldDebt>>
PeriodicBoot == /\ writer = "ended" /\ writer' = "booting"
                /\ boots' = Min2(boots + 1)
                /\ UNCHANGED <<presence,loss,drain,unsealed,successor,parked,alarm,
                                restarted,input,newSource,posted,oldDebt>>

\* Pending input cannot create a session until the unchanged readiness gate admits it.
InputArrives == /\ input = "waiting" /\ input' = "held"
                /\ UNCHANGED <<presence,loss,drain,unsealed,successor,writer,
                                parked,alarm,boots,restarted,newSource,posted,oldDebt>>
AdmitInput == /\ input = "held" /\ writer = "running"
              /\ input' = "accepted"
              /\ UNCHANGED <<presence,loss,drain,unsealed,successor,writer,
                              parked,alarm,boots,restarted,newSource,posted,oldDebt>>
\* FreshSubstitution chooses a fresh UUID/resume=false; startup creates Owed{0}.
NewSessionSource == /\ input = "accepted" /\ writer = "running"
                    /\ input' = "launched" /\ presence' = "present"
                    /\ newSource' = TRUE /\ successor' = TRUE
                    /\ UNCHANGED <<loss,drain,unsealed,writer,parked,alarm,
                                    boots,restarted,posted,oldDebt>>
PostFirstReply == /\ input \in {"launched", "done"} /\ newSource /\ writer = "running"
                  /\ posted = 0 /\ posted' = Min2(posted + 1) /\ input' = "done"
                  /\ UNCHANGED <<presence,loss,drain,unsealed,successor,writer,
                                  parked,alarm,boots,restarted,newSource,oldDebt>>
\* Redundant bind and periodic refresh cannot post a second durable reply.
RepeatNotification == /\ writer = "running" /\ input = "done" /\ UNCHANGED vars
\* One bounded process restart resets memory, retaining the new binding/source.
Restart == /\ ~restarted /\ writer = "running"
           /\ restarted' = TRUE /\ writer' = "booting" /\ parked' = FALSE /\ boots' = 1
           /\ UNCHANGED <<presence,loss,drain,unsealed,successor,alarm,input,
                           newSource,posted,oldDebt>>
\* Resume replays all retained records; parked source debt is still delivered.
DeliverRetainedSpool == /\ writer = "running" /\ oldDebt = 1 /\ oldDebt' = 0
                       /\ UNCHANGED <<presence,loss,drain,unsealed,successor,writer,
                                       parked,alarm,boots,restarted,input,newSource,posted>>
Rest == UNCHANGED vars
BaseNext == ResumePark \/ ResumeHalt \/ HostEnds \/ PeriodicBoot \/ InputArrives \/
        AdmitInput \/ NewSessionSource \/ PostFirstReply \/ RepeatNotification \/
        Restart \/ DeliverRetainedSpool \/ Rest
\* Boot ingress uses one stable (provider, channel, source-id) across gateway and catch-up.
\* ParkRule selects old FALSE / custody design TRUE only in BootSpec; old cfgs retain Spec.
VARIABLES bootHandler, bootCustody, bootNotice, bootCheckpoint, bootExecutions,
          bootWake, bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
          bootCatchup, bootRestarted, bootHeld, bootReason
bootVars == <<bootHandler, bootCustody, bootNotice, bootCheckpoint, bootExecutions,
              bootWake, bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
              bootCatchup, bootRestarted, bootHeld, bootReason>>
VARIABLES routeGateway, routeCheck, routeBadWake
routeVars == <<routeGateway, routeCheck, routeBadWake>>
RouteDormant == /\ routeGateway = TRUE /\ routeCheck = FALSE /\ routeBadWake = FALSE
bootAll == <<vars, bootVars, routeVars>>
BootDormant ==
    /\ bootHandler = "dormant" /\ bootCustody = FALSE /\ bootNotice = FALSE
    /\ bootCheckpoint = FALSE /\ bootExecutions = 0 /\ bootWake = FALSE
    /\ bootReady = FALSE /\ bootPermanent = FALSE /\ bootPersistFails = FALSE
    /\ bootNoticeFails = FALSE /\ bootCatchup = FALSE /\ bootRestarted = FALSE
    /\ bootHeld = FALSE /\ bootReason = "none"

Init == BaseInit /\ BootDormant /\ RouteDormant
Next == BaseNext /\ UNCHANGED <<bootVars, routeVars>>
Spec == Init /\ [][Next]_vars
        /\ WF_vars(ResumePark) /\ WF_vars(ResumeHalt) /\ WF_vars(HostEnds)
        /\ WF_vars(PeriodicBoot) /\ WF_vars(InputArrives) /\ WF_vars(AdmitInput)
        /\ WF_vars(NewSessionSource) /\ WF_vars(PostFirstReply)
        /\ WF_vars(DeliverRetainedSpool)

NoQuietReboot == (presence = "absent" /\ loss = "missing" /\ ~drain /\ ~unsealed)
                 => boots <= 1
ParkOnlySafe == parked => CanPark
ActiveLossVisible == (~CanPark /\ writer \in {"halted", "ended"}) => alarm
ReadyHasNoStaleHalt == writer = "running" => ~alarm
FirstReplyOnce == posted <= 1
FirstReplyNeedsAdmission == posted = 1 => newSource /\ input = "done"
ParkedKeepsHost == (writer = "running" /\ parked) => boots = 1
NewSourceGetsFirstReply == (input = "launched") ~> (posted = 1)
RetainedSpoolDelivered == (writer = "running" /\ oldDebt = 1) ~> (oldDebt = 0)
WaitingInputGetsFirstReply == (input = "held") ~> (posted = 1)

BootInit ==
    /\ BaseInit
    /\ bootHandler = "unseen" /\ bootCustody = FALSE /\ bootNotice = FALSE
    /\ bootCheckpoint = FALSE /\ bootExecutions = 0 /\ bootWake = FALSE
    /\ bootReady = FALSE /\ bootCatchup = FALSE /\ bootRestarted = FALSE
    /\ bootPermanent \in BOOLEAN /\ bootPersistFails \in BOOLEAN
    /\ bootNoticeFails \in BOOLEAN /\ bootHeld = FALSE /\ bootReason = "none"
    /\ RouteDormant

\* intake_gate -> admit_text_intake: handler work is not yet a completed intake.
GatewayArrivalRaw ==
    /\ bootHandler = "unseen" /\ bootHandler' = "classify"
    /\ UNCHANGED <<bootCustody, bootNotice, bootCheckpoint, bootExecutions, bootWake,
                   bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
                   bootCatchup, bootRestarted, bootHeld, bootReason>>
\* Route Hold guards are distinct from a permanent destination/provider refusal.
GatewayClassifyRaw ==
    /\ bootHandler = "classify"
    /\ IF bootPermanent
       THEN /\ bootHandler' = "notice" /\ bootReason' = "permanent"
            /\ bootHeld' = FALSE
       ELSE /\ bootHeld' = FALSE
            /\ IF ~bootReady /\ ~ParkRule
               THEN /\ bootHandler' = "notice" /\ bootReason' = "transient"
               ELSE /\ bootHandler' = "custody" /\ bootReason' = "none"
    /\ UNCHANGED <<bootCustody, bootNotice, bootCheckpoint, bootExecutions, bootWake,
                   bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
                   bootCatchup, bootRestarted>>
\* defer_live_submission: enqueue/merge/source-id duplicate is custody only without IO error.
GatewayCustodyRaw ==
    /\ bootHandler = "custody"
    /\ (~bootPersistFails \/ bootCustody)
    /\ bootCustody' = TRUE /\ bootHandler' = "complete"
    /\ bootWake' = (bootWake \/ bootReady)
    /\ bootHeld' = (bootHeld \/ (~bootReady /\ ParkRule))
    /\ UNCHANGED <<bootNotice, bootCheckpoint, bootExecutions, bootReady, bootPermanent,
                   bootPersistFails, bootNoticeFails, bootCatchup, bootRestarted, bootReason>>
GatewayCustodyFailureRaw ==
    /\ bootHandler = "custody" /\ bootPersistFails /\ ~bootCustody
    /\ bootHandler' = "notice" /\ bootReason' = "custody_failed" /\ bootHeld' = FALSE
    /\ UNCHANGED <<bootCustody, bootNotice, bootCheckpoint, bootExecutions, bootWake,
                   bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
                   bootCatchup, bootRestarted>>
\* A failed notice is explicitly unfinished/retry-required; never reported as completed.
NoticeFailureRaw ==
    /\ bootHandler = "notice" /\ bootNoticeFails
    /\ bootHandler' = (IF ParkRule THEN "retry_required" ELSE "complete")
    /\ UNCHANGED <<bootCustody, bootNotice, bootCheckpoint, bootExecutions, bootWake,
                   bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
                   bootCatchup, bootRestarted, bootHeld, bootReason>>
NoticeDeliveredRaw ==
    /\ bootHandler = "notice" /\ ~bootNoticeFails
    /\ bootNotice' = TRUE /\ bootHandler' = "complete"
    /\ UNCHANGED <<bootCustody, bootCheckpoint, bootExecutions, bootWake, bootReady,
                   bootPermanent, bootPersistFails, bootNoticeFails, bootCatchup,
                   bootRestarted, bootHeld, bootReason>>
\* Explicit retry is an environment action, not a promised production retry loop.
NoticeRetryRaw ==
    /\ bootHandler = "retry_required" /\ bootHandler' = "notice"
    /\ bootNoticeFails' = FALSE
    /\ UNCHANGED <<bootCustody, bootNotice, bootCheckpoint, bootExecutions, bootWake,
                   bootReady, bootPermanent, bootPersistFails, bootCatchup,
                   bootRestarted, bootHeld, bootReason>>
GatewayCompleteRaw ==
    /\ bootHandler = "complete" /\ bootHandler' = "done"
    /\ bootCheckpoint' = (IF ParkRule THEN (bootCustody \/ bootExecutions > 0) ELSE TRUE)
    /\ UNCHANGED <<bootCustody, bootNotice, bootExecutions, bootWake, bootReady,
                   bootPermanent, bootPersistFails, bootNoticeFails, bootCatchup,
                   bootRestarted, bootHeld, bootReason>>
\* Writer readiness summarizes ready/live/Owned/not-stopping/resumed; readiness eventually returns.
WriterReadyRaw ==
    /\ ~bootReady /\ bootReady' = TRUE
    /\ UNCHANGED <<bootHandler, bootCustody, bootNotice, bootCheckpoint, bootExecutions,
                   bootWake, bootPermanent, bootPersistFails, bootNoticeFails,
                   bootCatchup, bootRestarted, bootHeld, bootReason>>
\* Existing owner/start/reconcile wakes are preserved in both behaviors.
\* boot_no_wake alone removes the proposed readiness notification for the liveness mutant.
ReadyWakeRaw ==
    /\ (Case # "boot_no_wake" \/ ~bootHeld) /\ bootReady /\ bootCustody /\ bootExecutions = 0 /\ ~bootWake
    /\ bootWake' = TRUE
    /\ UNCHANGED <<bootHandler, bootCustody, bootNotice, bootCheckpoint, bootExecutions,
                   bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
                   bootCatchup, bootRestarted, bootHeld, bootReason>>
ExecuteSourceRaw ==
    /\ bootReady /\ bootCustody /\ bootWake /\ bootExecutions = 0
    /\ bootExecutions' = bootExecutions + 1 /\ bootWake' = FALSE
    /\ UNCHANGED <<bootHandler, bootCustody, bootNotice, bootCheckpoint, bootReady,
                   bootPermanent, bootPersistFails, bootNoticeFails, bootCatchup,
                   bootRestarted, bootHeld, bootReason>>
\* Catch-up overlaps gateway custody/finalization and reuses its source-id dedupe result.
CatchupSameSourceRaw ==
    /\ bootHandler \notin {"unseen", "classify"} /\ ~bootCatchup /\ ~bootPermanent
    /\ bootCatchup' = TRUE
    /\ IF bootCheckpoint \/ bootReason = "custody_failed" \/ bootPersistFails
       THEN UNCHANGED <<bootCustody, bootCheckpoint, bootWake>>
       ELSE /\ bootCustody' = TRUE /\ bootCheckpoint' = TRUE
            /\ bootWake' = (bootWake \/ bootReady)
    /\ UNCHANGED <<bootHandler, bootNotice, bootExecutions, bootReady, bootPermanent,
                   bootPersistFails, bootNoticeFails, bootRestarted, bootHeld, bootReason>>
\* Only a confirmed durable row is promised to survive restart; memory wakes disappear.
BootRestartRaw ==
    /\ bootCustody /\ ~bootRestarted
    /\ bootRestarted' = TRUE /\ bootReady' = FALSE /\ bootWake' = FALSE
    /\ UNCHANGED <<bootHandler, bootCustody, bootNotice, bootCheckpoint, bootExecutions,
                   bootPermanent, bootPersistFails, bootNoticeFails, bootCatchup,
                   bootHeld, bootReason>>
GatewayArrival == GatewayArrivalRaw /\ UNCHANGED <<vars, routeVars>>
GatewayClassify == GatewayClassifyRaw /\ UNCHANGED <<vars, routeVars>>
GatewayCustody == GatewayCustodyRaw /\ UNCHANGED <<vars, routeVars>>
GatewayCustodyFailure == GatewayCustodyFailureRaw /\ UNCHANGED <<vars, routeVars>>
NoticeFailure == NoticeFailureRaw /\ UNCHANGED <<vars, routeVars>>
NoticeDelivered == NoticeDeliveredRaw /\ UNCHANGED <<vars, routeVars>>
NoticeRetry == NoticeRetryRaw /\ UNCHANGED <<vars, routeVars>>
GatewayComplete == GatewayCompleteRaw /\ UNCHANGED <<vars, routeVars>>
WriterReady == WriterReadyRaw /\ UNCHANGED <<vars, routeVars>>
ReadyWake == ReadyWakeRaw /\ UNCHANGED <<vars, routeVars>>
ExecuteSource == ExecuteSourceRaw /\ UNCHANGED <<vars, routeVars>>
CatchupSameSource == CatchupSameSourceRaw /\ UNCHANGED <<vars, routeVars>>
BootRestart == BootRestartRaw /\ UNCHANGED <<vars, routeVars>>
BootDone == UNCHANGED bootAll
BootNext ==
    /\ (GatewayArrival \/ GatewayClassify \/ GatewayCustody \/ GatewayCustodyFailure \/ NoticeFailure \/ NoticeDelivered \/ NoticeRetry \/ GatewayComplete \/ WriterReady \/ ReadyWake \/ ExecuteSource \/ CatchupSameSource \/ BootRestart \/ BootDone)
    /\ UNCHANGED routeVars
BootSpec == BootInit /\ [][BootNext]_bootAll
            /\ WF_bootAll(GatewayArrival) /\ WF_bootAll(GatewayClassify)
            /\ WF_bootAll(GatewayCustody) /\ WF_bootAll(GatewayCustodyFailure)
            /\ WF_bootAll(NoticeFailure) /\ WF_bootAll(NoticeDelivered)
            /\ WF_bootAll(GatewayComplete) /\ WF_bootAll(WriterReady)
            /\ WF_bootAll(ReadyWake) /\ WF_bootAll(ExecuteSource)
\* Completed processing needs durable custody, execution, or a refusal visible to the user.
BootNoSilentLoss == bootHandler = "done" =>
    bootCustody \/ bootExecutions > 0 \/ bootNotice
BootExecuteOnce == bootExecutions <= 1
BootCheckpointOnlyAccepted == bootCheckpoint => bootCustody \/ bootExecutions > 0
BootPermanentNeverExecutes == bootPermanent => bootExecutions = 0 /\ ~bootCustody
BootHeldEventuallyExecutes == bootHeld ~> (bootExecutions = 1)

\* RouteSpec starts after one real ready callback or post-commit kick reached a held front.
\* host readiness and input placement are independent: successor_unproved precedes accepts.
\* No clock or periodic wake is modeled. Gateway progress and a later existing owner event
\* are explicit environment obligations, not promises of the two self-wake checks.
RouteInit ==
    /\ BaseInit
    /\ bootHandler = "done" /\ bootCustody = TRUE /\ bootNotice = FALSE
    /\ bootCheckpoint = TRUE /\ bootExecutions = 0 /\ bootWake = TRUE
    /\ bootReady = TRUE /\ bootPermanent = FALSE /\ bootPersistFails = FALSE
    /\ bootNoticeFails = FALSE /\ bootCatchup = FALSE /\ bootRestarted = FALSE
    /\ bootHeld = TRUE /\ bootReason = "none"
    /\ routeGateway = FALSE /\ routeCheck = FALSE /\ routeBadWake = FALSE

\* Both common preclaim and late claimed-front holds commit before their level recheck.
RouteOwnerHold ==
    /\ bootCustody /\ bootWake /\ bootExecutions = 0 /\ ~routeGateway /\ ~routeCheck
    /\ bootWake' = FALSE /\ routeCheck' = TRUE
    /\ UNCHANGED <<vars, bootHandler, bootCustody, bootNotice, bootCheckpoint,
                   bootExecutions, bootReady, bootPermanent, bootPersistFails,
                   bootNoticeFails, bootCatchup, bootRestarted, bootHeld, bootReason,
                   routeGateway, routeBadWake>>
RouteSelfWakeRecheck ==
    /\ routeCheck /\ bootExecutions = 0
    /\ LET current == Case = "boot_route_current"
           wantsWake == bootReady /\ (current \/ routeGateway)
       IN /\ bootWake' = (bootWake \/ wantsWake)
          /\ routeBadWake' = (routeBadWake \/ (wantsWake /\ ~routeGateway))
    /\ routeCheck' = FALSE
    /\ UNCHANGED <<vars, bootHandler, bootCustody, bootNotice, bootCheckpoint,
                   bootExecutions, bootReady, bootPermanent, bootPersistFails,
                   bootNoticeFails, bootCatchup, bootRestarted, bootHeld, bootReason,
                   routeGateway>>
\* A source/owner fact becoming eligible is not itself this code's notification.
RouteGatewayOpens ==
    /\ ~routeGateway /\ routeGateway' = TRUE
    /\ UNCHANGED <<vars, bootVars, routeCheck, routeBadWake>>
\* Existing owner/start/reconcile notifications remain an external event, not a self-poll.
RouteExistingOwnerWake ==
    /\ bootReady /\ routeGateway /\ bootCustody /\ ~routeCheck
    /\ ~bootWake /\ bootExecutions = 0 /\ bootWake' = TRUE
    /\ UNCHANGED <<vars, bootHandler, bootCustody, bootNotice, bootCheckpoint,
                   bootExecutions, bootReady, bootPermanent, bootPersistFails,
                   bootNoticeFails, bootCatchup, bootRestarted, bootHeld, bootReason,
                   routeVars>>
RouteExecute ==
    /\ bootReady /\ routeGateway /\ bootCustody /\ bootWake
    /\ ~routeCheck /\ bootExecutions = 0
    /\ bootExecutions' = 1 /\ bootWake' = FALSE
    /\ UNCHANGED <<vars, bootHandler, bootCustody, bootNotice, bootCheckpoint,
                   bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
                   bootCatchup, bootRestarted, bootHeld, bootReason, routeVars>>
RouteDone == UNCHANGED bootAll
RouteNext == RouteOwnerHold \/ RouteSelfWakeRecheck \/ RouteGatewayOpens \/
             RouteExistingOwnerWake \/ RouteExecute \/ RouteDone
RouteSpec == RouteInit /\ [][RouteNext]_bootAll
             /\ WF_bootAll(RouteOwnerHold) /\ WF_bootAll(RouteSelfWakeRecheck)
             /\ WF_bootAll(RouteGatewayOpens) /\ WF_bootAll(RouteExistingOwnerWake)
             /\ WF_bootAll(RouteExecute)
BootRouteNoBlockedSelfWake == ~routeBadWake
BootRouteEligibleHeldExecutes ==
    (bootHeld /\ bootReady /\ routeGateway) ~> (bootExecutions = 1)
=============================================================================
