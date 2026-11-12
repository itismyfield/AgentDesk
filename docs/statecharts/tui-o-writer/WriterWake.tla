---------------------------- MODULE WriterWake ----------------------------
EXTENDS Naturals
CONSTANTS ParkRule, Case
VARIABLE restart
RestartDormant == restart = [mode |-> "dormant"]
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
bootAll == <<vars, bootVars, routeVars, restart>>
BootDormant ==
    /\ bootHandler = "dormant" /\ bootCustody = FALSE /\ bootNotice = FALSE
    /\ bootCheckpoint = FALSE /\ bootExecutions = 0 /\ bootWake = FALSE
    /\ bootReady = FALSE /\ bootPermanent = FALSE /\ bootPersistFails = FALSE
    /\ bootNoticeFails = FALSE /\ bootCatchup = FALSE /\ bootRestarted = FALSE
    /\ bootHeld = FALSE /\ bootReason = "none"

Init == BaseInit /\ BootDormant /\ RouteDormant /\ RestartDormant
Next == BaseNext /\ UNCHANGED <<bootVars, routeVars, restart>>
Spec == Init /\ [][Next]_bootAll
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
    /\ RestartDormant
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
GatewayArrival == GatewayArrivalRaw /\ UNCHANGED <<vars, routeVars, restart>>
GatewayClassify == GatewayClassifyRaw /\ UNCHANGED <<vars, routeVars, restart>>
GatewayCustody == GatewayCustodyRaw /\ UNCHANGED <<vars, routeVars, restart>>
GatewayCustodyFailure == GatewayCustodyFailureRaw /\ UNCHANGED <<vars, routeVars, restart>>
NoticeFailure == NoticeFailureRaw /\ UNCHANGED <<vars, routeVars, restart>>
NoticeDelivered == NoticeDeliveredRaw /\ UNCHANGED <<vars, routeVars, restart>>
NoticeRetry == NoticeRetryRaw /\ UNCHANGED <<vars, routeVars, restart>>
GatewayComplete == GatewayCompleteRaw /\ UNCHANGED <<vars, routeVars, restart>>
WriterReady == WriterReadyRaw /\ UNCHANGED <<vars, routeVars, restart>>
ReadyWake == ReadyWakeRaw /\ UNCHANGED <<vars, routeVars, restart>>
ExecuteSource == ExecuteSourceRaw /\ UNCHANGED <<vars, routeVars, restart>>
CatchupSameSource == CatchupSameSourceRaw /\ UNCHANGED <<vars, routeVars, restart>>
BootRestart == BootRestartRaw /\ UNCHANGED <<vars, routeVars, restart>>
BootDone == UNCHANGED bootAll
BootNext ==
    /\ (GatewayArrival \/ GatewayClassify \/ GatewayCustody \/ GatewayCustodyFailure \/ NoticeFailure \/ NoticeDelivered \/ NoticeRetry \/ GatewayComplete \/ WriterReady \/ ReadyWake \/ ExecuteSource \/ CatchupSameSource \/ BootRestart \/ BootDone)
    /\ UNCHANGED <<routeVars, restart>>
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
    /\ RestartDormant
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
    /\ UNCHANGED restart
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
    /\ UNCHANGED restart
\* A source/owner fact becoming eligible is not itself this code's notification.
RouteGatewayOpens ==
    /\ ~routeGateway /\ routeGateway' = TRUE
    /\ UNCHANGED <<vars, bootVars, routeCheck, routeBadWake>>
    /\ UNCHANGED restart
\* Existing owner/start/reconcile notifications remain an external event, not a self-poll.
RouteExistingOwnerWake ==
    /\ bootReady /\ routeGateway /\ bootCustody /\ ~routeCheck
    /\ ~bootWake /\ bootExecutions = 0 /\ bootWake' = TRUE
    /\ UNCHANGED <<vars, bootHandler, bootCustody, bootNotice, bootCheckpoint,
                   bootExecutions, bootReady, bootPermanent, bootPersistFails,
                   bootNoticeFails, bootCatchup, bootRestarted, bootHeld, bootReason,
                   routeVars>>
    /\ UNCHANGED restart
RouteExecute ==
    /\ bootReady /\ routeGateway /\ bootCustody /\ bootWake
    /\ ~routeCheck /\ bootExecutions = 0
    /\ bootExecutions' = 1 /\ bootWake' = FALSE
    /\ UNCHANGED <<vars, bootHandler, bootCustody, bootNotice, bootCheckpoint,
                   bootReady, bootPermanent, bootPersistFails, bootNoticeFails,
                   bootCatchup, bootRestarted, bootHeld, bootReason, routeVars>>
    /\ UNCHANGED restart
RouteDone == UNCHANGED bootAll
RouteNext == (RouteOwnerHold \/ RouteSelfWakeRecheck \/ RouteGatewayOpens \/
             RouteExistingOwnerWake \/ RouteExecute \/ RouteDone) /\ UNCHANGED restart
RouteSpec == RouteInit /\ [][RouteNext]_bootAll
             /\ WF_bootAll(RouteOwnerHold) /\ WF_bootAll(RouteSelfWakeRecheck)
             /\ WF_bootAll(RouteGatewayOpens) /\ WF_bootAll(RouteExistingOwnerWake)
             /\ WF_bootAll(RouteExecute)
BootRouteNoBlockedSelfWake == ~routeBadWake
BootRouteEligibleHeldExecutes ==
    (bootHeld /\ bootReady /\ routeGateway) ~> (bootExecutions = 1)

\* One source, one old opener, a later prompt, and one bounded writer process restart.
RestartExactScope == restart.scope = "exact"
RestartCheckedScope == RestartExactScope \/
    (Case = "restart-unscoped" /\ restart.scope \in {"nonce", "cutoff"})
RestartFrame == UNCHANGED <<vars, bootVars, routeVars>>
RestartInit ==
    /\ BaseInit /\ BootDormant /\ RouteDormant
    /\ \E scope \in (IF Case = "restart-guarded"
                     THEN {"nonce", "cutoff", "source", "prefix", "opener"}
                     ELSE IF Case = "restart-unscoped" THEN {"nonce", "cutoff"}
                     ELSE {"exact"}) :
       \E seen \in (IF Case = "restart-fixed" THEN {0, 1} ELSE {0}) :
       restart = [scope |-> scope, stage |-> "start", physicalEnd |-> FALSE,
           witness |-> FALSE, witnessEver |-> FALSE, installed |-> FALSE,
           accepted |-> FALSE, applied |-> FALSE, oldIdle |-> FALSE,
           open |-> 0, opener |-> 0, nativeRead |-> FALSE, nativeWrites |-> 0,
           newPrompt |-> (Case \in {"restart-fixed", "restart-late", "restart-new-open",
                                   "restart-current-nonce", "restart-repost", "restart-drop-prepared"}),
           currentNonce |-> (IF Case \in {"restart-fixed", "restart-late", "restart-new-open",
                                        "restart-current-nonce", "restart-repost", "restart-drop-prepared"}
                            THEN 2 ELSE 1),
           promptRead |-> FALSE, newOpened |-> FALSE, absorbed |-> FALSE,
           sealTried |-> FALSE, installedAtSeal |-> FALSE,
           prepared |-> (Case \in {"restart-fixed", "restart-repost", "restart-drop-prepared"}),
           preparedEver |-> (Case \in {"restart-fixed", "restart-repost", "restart-drop-prepared"}),
           effects |-> seen, acked |-> FALSE, restarted |-> FALSE, effectBeforeCrash |-> FALSE,
           freshPrepared |-> FALSE, confirmed |-> "none", response |-> "none", receipt |-> "none",
           live |-> TRUE, replayed |-> FALSE]

\* Authority is a historical binding, old nonce, source identity and validated prefix cut.
RestartPhysicalEnd ==
    /\ RestartFrame /\ restart.stage = "start"
    /\ restart' = [restart EXCEPT !.physicalEnd = TRUE, !.stage = "install",
        !.witness = Case \notin {"restart-current", "restart-native"},
        !.witnessEver = Case \notin {"restart-current", "restart-native"}]
RestartInstall ==
    /\ RestartFrame /\ restart.stage = "install"
    /\ LET scopeOK == RestartCheckedScope /\
              (Case # "restart-current-nonce" \/ restart.currentNonce = 1)
       IN restart' = [restart EXCEPT
          !.installed = restart.witness /\ Case # "restart-late",
          !.accepted = scopeOK /\ Case # "restart-late",
          !.live = scopeOK,
          !.stage = IF scopeOK THEN "old" ELSE "held"]
RestartReadOld ==
    /\ RestartFrame /\ restart.stage = "old"
    /\ restart' = [restart EXCEPT !.open = 1, !.opener = 1, !.stage = "boundary"]
\* This runs before the next record whose start is at/after cutoff, or at cutoff EOF.
RestartBoundary ==
    /\ RestartFrame /\ restart.stage = "boundary"
    /\ LET native == Case = "restart-native"
           apply == restart.installed /\ restart.accepted /\ restart.open = 1 /\ restart.opener = 1
       IN restart' = [restart EXCEPT !.stage = "new",
          !.applied = @ \/ apply,
          !.oldIdle = @ \/ apply \/ native,
          !.nativeRead = @ \/ native,
          !.open = IF apply \/ native THEN 0 ELSE @]
\* Prompt inside Open is ignored by TurnTracker; only the reset lets the new prompt open.
RestartReadNew ==
    /\ RestartFrame /\ restart.stage = "new"
    /\ restart' = [restart EXCEPT !.stage = "seal",
        !.promptRead = restart.newPrompt,
        !.absorbed = @ \/ (restart.newPrompt /\ restart.open # 0),
        !.newOpened = restart.newPrompt /\ restart.open = 0,
        !.open = IF restart.newPrompt /\ restart.open = 0 THEN 2 ELSE @,
        !.opener = IF restart.newPrompt /\ restart.open = 0 THEN 2 ELSE @]
RestartSeal ==
    /\ RestartFrame /\ restart.stage = "seal"
    /\ restart' = [restart EXCEPT !.stage = "ready", !.sealTried = TRUE,
        !.installedAtSeal = restart.installed,
        !.prepared = @ \/ restart.oldIdle,
        !.preparedEver = @ \/ restart.oldIdle,
        !.freshPrepared = @ \/ (restart.oldIdle /\ ~restart.preparedEver)]
RestartLateInstall ==
    /\ RestartFrame /\ Case = "restart-late" /\ restart.stage = "ready" /\ ~restart.installed
    /\ restart' = [restart EXCEPT !.installed = TRUE, !.accepted = TRUE,
        !.applied = TRUE, !.oldIdle = TRUE,
        !.open = IF restart.open = 1 THEN 0 ELSE @]
RestartReplayOld ==
    /\ RestartFrame /\ restart.stage = "ready" /\ restart.newOpened /\ ~restart.replayed
    /\ restart' = [restart EXCEPT !.replayed = TRUE,
        !.open = IF Case = "restart-new-open" THEN 0 ELSE @]
\* Prepared and Posted identities survive; network effect may precede its ledger ack.
RestartPost ==
    /\ RestartFrame /\ restart.live /\ restart.prepared /\ ~restart.acked
    /\ (~restart.restarted \/ restart.stage = "ready")
    /\ (restart.freshPrepared \/
        (Case = "restart-repost" /\ restart.restarted /\ restart.effectBeforeCrash))
    /\ \E outcome \in {"created", "uncertain"} :
       restart' = [restart EXCEPT !.effects = Min2(@ + 1), !.freshPrepared = FALSE,
           !.confirmed = "none", !.response = outcome]
\* A readable exact-history verdict is observed before a restored Prepared may be retried.
RestartConfirm ==
    /\ RestartFrame /\ restart.live /\ restart.prepared /\ ~restart.acked
    /\ ~restart.freshPrepared /\ restart.response # "created"
    /\ restart.confirmed \in {"none", "unknown"}
    /\ (~restart.restarted \/ restart.stage = "ready")
    /\ restart' = [restart EXCEPT
        !.confirmed = IF restart.effects > 0 THEN "posted" ELSE "missing"]
RestartHistoryUnavailable ==
    /\ RestartFrame /\ restart.live /\ restart.prepared /\ ~restart.acked
    /\ ~restart.freshPrepared /\ restart.response # "created"
    /\ restart.confirmed \in {"none", "unknown"}
    /\ restart' = [restart EXCEPT !.confirmed = "unknown"]
RestartRecordMissing ==
    /\ RestartFrame /\ restart.live /\ restart.confirmed = "missing" /\ ~restart.acked
    /\ restart' = [restart EXCEPT !.receipt = "missing", !.confirmed = "none"]
RestartPrepareRetry ==
    /\ RestartFrame /\ restart.live /\ restart.receipt = "missing" /\ ~restart.acked
    /\ restart' = [restart EXCEPT !.receipt = "none", !.freshPrepared = TRUE,
        !.confirmed = "none", !.response = "none"]
RestartRecordPosted ==
    /\ RestartFrame /\ restart.live /\ restart.prepared /\ ~restart.acked
    /\ (restart.response = "created" \/ restart.confirmed = "posted")
    /\ restart' = [restart EXCEPT !.acked = TRUE, !.receipt = "posted",
        !.confirmed = "none", !.response = "none", !.freshPrepared = FALSE]
RestartCrash ==
    /\ RestartFrame /\ restart.physicalEnd /\ ~restart.restarted
    /\ restart' = [restart EXCEPT !.restarted = TRUE, !.stage = "boot", !.live = FALSE,
        !.effectBeforeCrash = restart.effects > 0,
        !.freshPrepared = FALSE, !.confirmed = "none", !.response = "none",
        !.witness = IF Case = "restart-witness-lost" THEN FALSE ELSE @,
        !.installed = FALSE, !.accepted = FALSE, !.applied = FALSE, !.oldIdle = FALSE,
        !.open = 0, !.opener = 0, !.nativeRead = FALSE, !.promptRead = FALSE,
        !.newOpened = FALSE, !.replayed = FALSE, !.sealTried = FALSE, !.installedAtSeal = FALSE,
        !.prepared = IF Case = "restart-drop-prepared" /\ ~restart.acked THEN FALSE ELSE @]
RestartBoot ==
    /\ RestartFrame /\ restart.stage = "boot"
    /\ restart' = [restart EXCEPT !.stage = "install"]
RestartRest == UNCHANGED bootAll
RestartNext == RestartPhysicalEnd \/ RestartInstall \/ RestartReadOld \/ RestartBoundary \/
    RestartReadNew \/ RestartSeal \/ RestartLateInstall \/ RestartReplayOld \/
    RestartPost \/ RestartConfirm \/ RestartHistoryUnavailable \/ RestartRecordMissing \/
    RestartPrepareRetry \/ RestartRecordPosted \/ RestartCrash \/ RestartBoot \/ RestartRest
RestartSpec == RestartInit /\ [][RestartNext]_bootAll
    /\ WF_bootAll(RestartPhysicalEnd) /\ WF_bootAll(RestartInstall)
    /\ WF_bootAll(RestartReadOld) /\ WF_bootAll(RestartBoundary)
    /\ WF_bootAll(RestartReadNew) /\ WF_bootAll(RestartSeal)
    /\ WF_bootAll(RestartLateInstall) /\ WF_bootAll(RestartPost)
    /\ WF_bootAll(RestartConfirm) /\ WF_bootAll(RestartRecordMissing)
    /\ WF_bootAll(RestartPrepareRetry) /\ WF_bootAll(RestartRecordPosted)
    /\ WF_bootAll(RestartBoot)
RestartReplyOnce == restart.effects <= 1
RestartExactEndOnly == restart.applied => RestartExactScope
RestartNewPromptNotAbsorbed == ~restart.absorbed
RestartOldEndCannotCloseNewOpen == restart.newOpened => restart.open = 2
RestartWitnessBeforeSeal == (restart.sealTried /\ restart.witnessEver) => restart.installedAtSeal
RestartPreparedNotLost == (restart.preparedEver /\ ~restart.acked) => restart.prepared
RestartWitnessSurvivesBoot == restart.witnessEver => restart.witness
RestartNoNativeForgery == restart.nativeWrites = 0 /\ (restart.nativeRead => Case = "restart-native")
RestartExactEndEventuallyCloses == (restart.physicalEnd /\ RestartExactScope) ~> restart.oldIdle
RestartPreparedEventuallyPosted == restart.prepared ~> restart.acked

=============================================================================
