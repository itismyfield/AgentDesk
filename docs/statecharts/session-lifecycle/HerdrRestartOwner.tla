---- MODULE HerdrRestartOwner ----
\* Design r2 port model over main 33d1f4b5a9dce7023e2d0014e45a05ed6c530bba.
\* The restart command commits one seed original at the owner's queue head, closes the
\* exact old execution and wakes the owner; the owner's ordinary Herdr turn launches the
\* successor with that seed as its first input. Not a model of a new Core or host trait.
EXTENDS Naturals, FiniteSets
CONSTANTS Policy, Mutant, N, StartKind, Dispatch, CloseReply, LaunchReply, Busy,
          ClearOn, CrashOn, Rollback
VARIABLE s
vars == <<s>>
Ids == 1..N
SeedId == N + 1
NeedsClose == StartKind \in {"Bound", "PendingPlaced"}
\* Refused before any effect: unreadable rows, an unplaced Pending and an active dispatch.
Refusable == StartKind \in {"Unknown", "PendingUnplaced"}
             \/ (Dispatch /\ NeedsClose /\ Mutant # "IgnoreDispatch")
Healthy == ~Refusable /\ ~Dispatch /\ CloseReply = "confirmed" /\ LaunchReply = "Created"
           /\ ClearOn = "off" /\ ~CrashOn
Init == s = [cmd |-> "idle", seed |-> "none", gate |-> FALSE, excl |-> FALSE,
             row |-> StartKind, oldLive |-> NeedsClose, selector |-> "kept",
             inflight |-> Busy /\ NeedsClose, settled |-> ~(Busy /\ NeedsClose),
             warned |-> FALSE, awake |-> FALSE, panes |-> 0, created |-> "no",
             creates |-> 0, launcher |-> 0, seedEffects |-> 0, oldSeed |-> FALSE,
             oldInputs |-> 0, inherit |-> FALSE, ready |-> FALSE, visible |-> FALSE,
             closedByRestart |-> FALSE, clear |-> "none", clearSaw |-> "none",
             clearExcl |-> FALSE, clearEffective |-> FALSE, clearHeld |-> FALSE,
             resumedPreClear |-> FALSE, seedRevived |-> FALSE, crashed |-> FALSE,
             binary |-> "new", accepted |-> {1}, receipts |-> {},
             q |-> [i \in Ids |-> IF i = 1 THEN "queued" ELSE "incoming"],
             effects |-> [i \in Ids |-> 0]]

SeedPending == s.seed \in {"queued", "claimed"}
FrontInput(i) == /\ ~SeedPending /\ s.launcher = 0 /\ s.q[i] = "queued"
                 /\ \A j \in 1..(i-1) : s.q[j] # "queued"
OwnerFree == s.awake /\ ~s.excl /\ ~s.clearExcl
\* The in-memory gate keeps the seed unstarted until the command released its exclusion.
SeedHeld == s.gate /\ Mutant # "NoGate"
\* The old execution the seed names is still the row's unretired one.
OldTarget == s.row \in {"Bound", "PendingPlaced"}
Vacant == s.row \in {"Retired", "Missing", "Legacy"}
FrontFails(l) == [s EXCEPT !.launcher = 0,
                           !.seed = IF l = SeedId THEN "dropped" ELSE @,
                           !.visible = @ \/ (l = SeedId /\ Mutant # "NoNotice"),
                           !.q = [i \in Ids |-> IF i = l THEN "retained" ELSE s.q[i]]]

\* restart.rs:219 refuses today. The design commits the seed first: that commit is the
\* acceptance point, and the seed is the only durable record of the request.
Request == /\ s.cmd = "idle"
           /\ s' = IF Policy = "before" \/ Refusable
                   THEN [s EXCEPT !.cmd = "notice", !.awake = TRUE]
                   ELSE [s EXCEPT !.seed = "queued", !.gate = NeedsClose, !.awake = TRUE,
                                  !.warned = Busy /\ NeedsClose,
                                  !.cmd = IF NeedsClose THEN "seeded" ELSE "done"]
Admit(i) == /\ s.cmd # "idle" /\ s.q[i] = "incoming"
            /\ s' = [s EXCEPT !.accepted = @ \cup {i}, !.q[i] = "queued", !.awake = TRUE]
\* A drive that finds the transition taken or the seed gated parks until the next wake.
OwnerParks == /\ s.awake /\ (s.excl \/ s.clearExcl \/ (s.seed = "queued" /\ SeedHeld))
              /\ s' = [s EXCEPT !.awake = FALSE]
\* terminate_scoped: transition, fence, exact close, end read, retire CAS, settlement.
TakeExclusion == /\ s.cmd = "seeded" /\ ~s.clearExcl
                 /\ s' = [s EXCEPT !.excl = TRUE, !.cmd = "closing"]
\* The terminate is bound to the old nonce: a row that already records a successor refuses.
Successor == s.row \in {"NewPending", "NewBound"}
CloseExact == /\ s.cmd = "closing" /\ CloseReply = "confirmed" /\ ~Successor
              /\ s' = [s EXCEPT !.oldLive = IF Mutant = "SkipClose" THEN @ ELSE FALSE,
                               !.closedByRestart = TRUE, !.cmd = "retiring"]
CloseFails == /\ s.cmd = "closing" /\ (CloseReply # "confirmed" \/ Successor)
              /\ s' = [s EXCEPT !.cmd = "failing"]
Retire == /\ s.cmd = "retiring"
          /\ s' = [s EXCEPT !.row = "Retired", !.cmd = "postlude",
                           !.inflight = IF Mutant = "NoSettle" THEN @ ELSE FALSE,
                           !.settled = IF Mutant = "NoSettle" THEN @ ELSE TRUE,
                           !.q = IF Mutant = "DropQueue"
                                 THEN [i \in Ids |-> IF s.q[i] = "queued" THEN "lost" ELSE s.q[i]]
                                 ELSE @]
\* after_retire, still inside the exclusion: a seed a clear discarded means the clear
\* overtook this restart, so the selector is forgotten as that clear would have done.
Superseded == s.seed = "cancelled"
Postlude ==
  /\ s.cmd = "postlude"
  /\ s' = IF Mutant = "R1Launch"
          THEN [s EXCEPT !.excl = FALSE, !.gate = FALSE, !.awake = TRUE, !.row = "NewBound",
                         !.panes = 1, !.resumedPreClear = s.clearEffective,
                         !.seed = IF Superseded THEN @ ELSE "delivered",
                         !.seedEffects = IF Superseded THEN @ ELSE @ + 1,
                         !.ready = ~Superseded, !.cmd = IF Superseded THEN "notice" ELSE "done"]
          ELSE [s EXCEPT !.excl = FALSE, !.gate = FALSE,
                         !.awake = @ \/ Mutant # "NoWake",
                         !.selector = IF Superseded /\ Mutant \notin {"NoSupersedeCheck", "ReloadSeed"}
                                      THEN "forgotten" ELSE @,
                         !.seed = IF Superseded /\ Mutant = "ReloadSeed" THEN "queued" ELSE @,
                         !.seedRevived = @ \/ (Superseded /\ Mutant = "ReloadSeed"),
                         !.cmd = IF Superseded /\ Mutant # "ReloadSeed" THEN "notice" ELSE "done"]
\* A failed close answers first and withdraws the seed after: a crash in between leaves
\* the seed to the permit's refusal, never a silent end.
Fail == /\ s.cmd = "failing"
        /\ s' = IF Mutant = "WithdrawFirst"
                THEN [s EXCEPT !.excl = FALSE, !.gate = FALSE, !.awake = TRUE, !.cmd = "notice",
                               !.seed = IF @ = "queued" THEN "withdrawn" ELSE @]
                ELSE [s EXCEPT !.excl = FALSE, !.gate = FALSE, !.awake = TRUE,
                               !.visible = TRUE, !.cmd = "withdrawing"]
Withdraw == /\ s.cmd = "withdrawing"
            /\ s' = [s EXCEPT !.cmd = "done",
                             !.seed = IF @ = "queued" /\ Mutant # "NoWithdraw" THEN "withdrawn" ELSE @]
NotifyFailure == /\ s.cmd = "notice" /\ Mutant # "NoNotice"
                 /\ s' = [s EXCEPT !.visible = TRUE, !.cmd = "done"]

\* The seed's permit at its start: the execution it was queued against must be retired.
\* An older binary reads the seed as a headless start and refuses it as executor_not_wired.
SeedRefused ==
  /\ s.seed = "queued" /\ OwnerFree /\ ~SeedHeld
  /\ s.binary = "old" \/ (OldTarget /\ Mutant # "NoPermit")
  /\ LET silent == s.binary = "old" /\ Mutant = "IntentField"
     IN s' = [s EXCEPT !.seed = IF silent THEN "stripped" ELSE "dropped",
                       !.visible = @ \/ (~silent /\ Mutant # "NoNotice")]
SeedToOld == /\ Mutant = "NoPermit" /\ s.seed = "queued" /\ OwnerFree /\ ~SeedHeld
             /\ s.binary = "new" /\ OldTarget /\ s.oldLive
             /\ s' = [s EXCEPT !.seed = "delivered", !.oldSeed = TRUE, !.seedEffects = @ + 1]
Launching(l) == [s EXCEPT !.launcher = l, !.row = "NewPending", !.created = "no", !.creates = 0,
                          !.seed = IF l = SeedId THEN "claimed" ELSE @,
                          !.inherit = @ \/ (Dispatch /\ s.closedByRestart),
                          !.resumedPreClear = @ \/ (s.clearEffective /\ s.selector = "kept")]
\* herdr_launch.rs:378: the owner's turn installs Pending over a vacant row, then creates.
SeedLaunch == /\ s.seed = "queued" /\ OwnerFree /\ ~SeedHeld /\ s.binary = "new"
              /\ Vacant /\ s.launcher = 0
              /\ s' = Launching(SeedId)
SeedJoin == /\ s.seed = "queued" /\ OwnerFree /\ ~SeedHeld /\ s.binary = "new"
            /\ s.row = "NewBound"
            /\ s' = [s EXCEPT !.seed = "delivered", !.seedEffects = @ + 1, !.ready = TRUE]
InputLaunch(i) == /\ FrontInput(i) /\ OwnerFree /\ Vacant
                  /\ s' = Launching(i)
Create == /\ s.row = "NewPending" /\ s.created = "no" /\ s.launcher # 0
          /\ s' = [s EXCEPT !.creates = @ + 1,
                           !.panes = @ + (IF LaunchReply = "NotSent" THEN 0 ELSE 1),
                           !.created = IF LaunchReply = "Created" THEN "yes"
                                       ELSE IF LaunchReply = "NotSent" THEN "notsent" ELSE "unknown"]
LaunchNotSent == /\ s.row = "NewPending" /\ s.created = "notsent"
                 /\ s' = [FrontFails(s.launcher) EXCEPT !.row = "Retired", !.created = "no"]
\* An unknown create acknowledgement keeps Pending, fails its input visibly and is only
\* probed again; it is never created a second time.
LaunchUncertain == /\ s.row = "NewPending" /\ s.created = "unknown" /\ s.launcher # 0
                   /\ s' = FrontFails(s.launcher)
Adopt == /\ s.row = "NewPending" /\ s.created = "unknown" /\ s.launcher = 0
         /\ Mutant # "ReplayUnknown"
         /\ s' = [s EXCEPT !.created = "yes", !.row = "NewBound"]
ReplayCreate == /\ Mutant = "ReplayUnknown" /\ s.row = "NewPending" /\ s.created = "unknown"
                /\ s.creates < 2
                /\ s' = [s EXCEPT !.creates = @ + 1, !.panes = @ + 1]
Submit == /\ s.row = "NewPending" /\ s.created = "yes" /\ s.launcher # 0
          /\ s' = IF s.launcher = SeedId
                  THEN [s EXCEPT !.row = "NewBound", !.launcher = 0, !.seed = "delivered",
                                 !.seedEffects = @ + 1, !.ready = TRUE]
                  ELSE [s EXCEPT !.row = "NewBound", !.launcher = 0,
                                 !.q[s.launcher] = "delivered", !.effects[s.launcher] = @ + 1]
Deliver(i) == /\ FrontInput(i) /\ OwnerFree /\ s.row = "NewBound" /\ s.effects[i] < 2
              /\ s' = [s EXCEPT !.q[i] = "delivered", !.effects[i] = @ + 1]
\* With no restart holding the channel the old execution is still the session's only one.
DeliverOld(i) == /\ FrontInput(i) /\ OwnerFree /\ OldTarget /\ s.oldLive /\ s.effects[i] < 2
                 /\ s' = [s EXCEPT !.q[i] = "delivered", !.effects[i] = @ + 1]
RefuseUnknown(i) == /\ FrontInput(i) /\ OwnerFree /\ s.row = "Unknown"
                    /\ s' = [s EXCEPT !.q[i] = "retained"]
StaleOldDelivery(i) == /\ Mutant = "OldInput" /\ s.excl /\ s.oldLive /\ s.q[i] = "queued"
                       /\ s.oldInputs = 0
                       /\ s' = [s EXCEPT !.oldInputs = 1, !.effects[i] = @ + 1, !.q[i] = "delivered"]
ReplayInput(i) == /\ Mutant = "ReplayInput" /\ s.q[i] = "delivered" /\ s.effects[i] = 1
                  /\ s' = [s EXCEPT !.q[i] = "queued"]
ReplaySeed == /\ Mutant = "ReplaySeed" /\ s.crashed /\ s.seed = "delivered" /\ s.seedEffects = 1
              /\ s' = [s EXCEPT !.seedEffects = 2]
\* Existing recovery outside this design: a Bound row whose pane is gone is retired, and
\* an unplaced Pending is resolved by the turn's own resolver.
RowEndRecovery == /\ OldTarget /\ ~s.oldLive /\ ~s.excl
                  /\ s' = [s EXCEPT !.row = "Retired", !.inflight = FALSE, !.settled = TRUE]
ResolveUnplaced == /\ s.row = "PendingUnplaced"
                   /\ s' = [s EXCEPT !.row = "Retired"]

\* The daemon dies: gate, exclusion and the command are gone; the queue, the row and the
\* panes stay. Boot drives the queue; a create that was never sent retires its Pending.
Crash == /\ CrashOn /\ ~s.crashed /\ s.cmd # "idle"
         /\ LET unsent == s.row = "NewPending" /\ s.created = "no"
            IN s' = [s EXCEPT !.crashed = TRUE, !.cmd = "dead", !.gate = FALSE, !.excl = FALSE,
                              !.awake = TRUE, !.binary = IF Rollback THEN "old" ELSE "new",
                              !.row = IF unsent THEN "Retired" ELSE @,
                              !.launcher = IF unsent THEN 0 ELSE @,
                              !.seed = IF unsent /\ @ = "claimed" THEN "queued" ELSE @]

\* !clear, control.rs:510. `serial` holds the transition; `overtake` arrived while the
\* restart held it and proceeds without it after the 3 second wait.
ClearRead == /\ ClearOn # "off" /\ s.clear = "none" /\ s.cmd # "idle"
             /\ (ClearOn = "serial" => ~s.excl) /\ (ClearOn = "overtake" => s.excl)
             /\ s' = [s EXCEPT !.clear = "read", !.clearSaw = s.row, !.clearExcl = ClearOn = "serial"]
Discarded == [s EXCEPT !.clear = "pending", !.clearEffective = s.launcher = 0,
                       !.clearHeld = s.launcher # 0,
                       !.seed = IF @ = "queued" THEN "cancelled" ELSE @,
                       !.receipts = @ \cup {i \in Ids : s.q[i] = "queued" /\ i # s.launcher},
                       !.q = [i \in Ids |-> IF s.q[i] = "queued" /\ i # s.launcher
                                            THEN "cancelled" ELSE s.q[i]]]
\* A clear that found a turn launching reports a hold and ends nothing (existing behaviour).
Ended(t) ==
  IF t.clearHeld THEN [t EXCEPT !.clear = "done", !.clearExcl = FALSE, !.awake = TRUE]
  ELSE IF t.row \in {"Bound", "PendingPlaced"}
  THEN [t EXCEPT !.clear = "done", !.clearExcl = FALSE, !.awake = TRUE, !.oldLive = FALSE,
                 !.row = "Retired", !.selector = "forgotten", !.inflight = FALSE, !.settled = TRUE]
  ELSE IF t.row \in {"NewBound", "NewPending"}
  \* An input that started launching after the discard loses its pane to this teardown.
  THEN [t EXCEPT !.clear = "done", !.clearExcl = FALSE, !.awake = TRUE, !.panes = 0,
                 !.row = "Retired", !.selector = "forgotten", !.created = "no",
                 !.launcher = 0, !.seed = IF t.launcher = SeedId THEN "dropped" ELSE @,
                 !.visible = @ \/ t.launcher = SeedId,
                 !.q = [i \in Ids |-> IF i = t.launcher THEN "retained" ELSE t.q[i]]]
  ELSE [t EXCEPT !.clear = "done", !.clearExcl = FALSE, !.awake = TRUE,
                 !.selector = IF Mutant = "ClearSkipsRetired" /\ t.row = "Retired"
                                 /\ t.clearSaw \in {"Bound", "PendingPlaced"}
                              THEN @ ELSE "forgotten"]
ClearDiscard == /\ s.clear = "read"
                /\ s' = IF s.excl THEN Discarded ELSE Ended(Discarded)
ClearEnd == /\ s.clear = "pending" /\ ~s.excl
            /\ s' = Ended(s)
\* Its own terminate waited out the transition: the clear ends with a warning only.
ClearGivesUp == /\ s.clear = "pending" /\ s.excl
                /\ s' = [s EXCEPT !.clear = "gaveup"]
Wait == UNCHANGED s

Next == \/ Request \/ OwnerParks \/ TakeExclusion \/ CloseExact \/ CloseFails \/ Retire
        \/ Postlude \/ Fail \/ Withdraw \/ NotifyFailure \/ SeedRefused \/ SeedToOld
        \/ SeedLaunch \/ SeedJoin \/ Create \/ LaunchNotSent \/ LaunchUncertain \/ Adopt
        \/ ReplayCreate \/ Submit \/ ReplaySeed \/ RowEndRecovery \/ ResolveUnplaced \/ Crash
        \/ ClearRead \/ ClearDiscard \/ ClearEnd \/ ClearGivesUp \/ Wait
        \/ \E i \in Ids : \/ Admit(i) \/ InputLaunch(i) \/ Deliver(i) \/ DeliverOld(i)
                          \/ RefuseUnknown(i) \/ StaleOldDelivery(i) \/ ReplayInput(i)
\* Each step of the command, the owner and a started clear eventually runs while it stays
\* enabled. Crash, a clear's arrival and a parked drive are not fair: they may never happen.
Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Request) /\ WF_vars(TakeExclusion) /\ WF_vars(CloseExact)
        /\ WF_vars(CloseFails) /\ WF_vars(Retire) /\ WF_vars(Postlude) /\ WF_vars(Fail)
        /\ WF_vars(Withdraw) /\ WF_vars(NotifyFailure) /\ WF_vars(SeedRefused)
        /\ WF_vars(SeedToOld)
        /\ WF_vars(SeedLaunch) /\ WF_vars(SeedJoin) /\ WF_vars(Create)
        /\ WF_vars(LaunchNotSent) /\ WF_vars(LaunchUncertain) /\ WF_vars(Adopt)
        /\ WF_vars(Submit) /\ WF_vars(RowEndRecovery) /\ WF_vars(ResolveUnplaced)
        /\ WF_vars(ClearDiscard) /\ WF_vars(ClearEnd) /\ WF_vars(ClearGivesUp)
        /\ \A i \in Ids : /\ WF_vars(Admit(i)) /\ WF_vars(InputLaunch(i))
                          /\ WF_vars(Deliver(i)) /\ WF_vars(DeliverOld(i))
                          /\ WF_vars(RefuseUnknown(i))

Settled(i) == \/ s.q[i] \in {"delivered", "retained"}
              \/ (s.q[i] = "cancelled" /\ i \in s.receipts)
InputCustody == \A i \in s.accepted : s.q[i] = "queued" \/ Settled(i)
InputAtMostOnce == \A i \in Ids : s.effects[i] <= 1
NoOldPaneInput == s.oldInputs = 0
\* The greeting never reaches the execution the restart was meant to end.
NoFalseReady == ~s.oldSeed
SeedAtMostOnce == s.seedEffects <= 1
NoBlindCreate == s.creates <= 1
SingleSession == (IF s.oldLive THEN 1 ELSE 0) + s.panes <= 1
InflightSettledBeforeReady == s.ready => s.settled /\ ~s.inflight /\ (~(Busy /\ NeedsClose) \/ s.warned)
NoCloseUnderDispatch == (Dispatch /\ NeedsClose) => ~s.closedByRestart
NoInheritedDispatch == ~s.inherit
\* After a clear that discarded the whole queue, no launch resumes the conversation it ended.
NoResumeAfterClear == ~s.resumedPreClear
NoSeedRevival == ~s.seedRevived
\* Checked only where the pre-existing clear-versus-launch gap is on display.
ClearNeverHeld == ~s.clearHeld
Resolved == s.ready \/ s.visible \/ s.seed = "cancelled"
RequestResolves == (s.seed # "none") ~> Resolved
RefusalVisible == (s.cmd = "notice") ~> (s.visible \/ s.crashed)
RestartAvailable == <> (s.ready \/ (~Healthy /\ s.cmd \in {"done", "dead"}))
AcceptedInputResolves == \A i \in Ids : (i \in s.accepted) ~> Settled(i)
ClearForgets == s.clearEffective ~> (s.selector = "forgotten")
====
