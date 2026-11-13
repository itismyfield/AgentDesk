---- MODULE HerdrRestartOwner ----
\* B1 effect permissions and fixed owner claims over the earlier B1/B2 port model.
\* The restart command commits one seed original behind the owner's in-flight unit, closes
\* the exact old execution and wakes the owner; the owner's ordinary Herdr turn launches the
\* successor with that seed as its first input. A user clear bumps the channel's clear epoch
\* and wins over a launching turn. Not a model of a new Core or host trait.
EXTENDS Naturals, FiniteSets, Sequences
CONSTANTS Policy, Mutant, N, StartKind, Dispatch, CloseReply, LaunchReply, Busy,
          ClearOn, CrashOn, Rollback, SeedMode, HeadBusy,
          RecoveryPolicy, ReplayMode, OpPolicy, EffectPolicy
VARIABLE s
vars == <<s>>
Ids == 1..N
SeedId == N + 1
\* One pre-existing owner claim: its head and first follower precede the late third input.
ClaimMembers == IF HeadBusy THEN <<1, 2>> ELSE <<>>
ClaimSet == {ClaimMembers[i] : i \in DOMAIN ClaimMembers}
ActiveClaim == s.claimPhase \in {"claimed", "prepared", "reserved"}
NeedsClose == StartKind \in {"Bound", "PendingPlaced"}
\* Refused before any effect: unreadable rows and an unplaced Pending only.
Refusable == StartKind \in {"Unknown", "PendingUnplaced"}
\* The seed could not be admitted (queue full, V2 not adopted, effort pending): restart anyway.
Seeded == SeedMode = "ok"
Healthy == ~Refusable /\ CloseReply = "confirmed" /\ LaunchReply = "Created"
           /\ ClearOn = "off" /\ ~CrashOn /\ Seeded
Init == s = [cmd |-> "idle", seed |-> "none", gate |-> FALSE, excl |-> FALSE,
             row |-> StartKind, oldLive |-> NeedsClose, selector |-> "kept",
             rowDispatch |-> Dispatch /\ NeedsClose, retries |-> 0, inherit |-> FALSE,
             inflight |-> Busy /\ NeedsClose, settled |-> ~(Busy /\ NeedsClose),
             warned |-> FALSE, awake |-> FALSE, panes |-> 0, created |-> "no",
             creates |-> 0, launcher |-> 0, lphase |-> "none", lEpoch |-> 0,
             seedEffects |-> 0, oldSeed |-> FALSE, oldInputs |-> 0,
             opSeen |-> FALSE, opSeedless |-> FALSE, opDuplicate |-> FALSE,
             checked |-> FALSE, permission |-> FALSE, effectKind |-> "none", grantEpoch |-> 0, grantValid |-> FALSE,
             effectBegan |-> FALSE, unsafeEffect |-> FALSE, clearRequested |-> FALSE,
             undrainedClear |-> FALSE, knownUnknownClear |-> FALSE,
             ready |-> FALSE, falseReady |-> FALSE, visible |-> FALSE, answered |-> FALSE,
             falseSuperseded |-> FALSE, epoch |-> 0, clearing |-> FALSE, clear |-> "none",
             clearExcl |-> FALSE, clearEffective |-> FALSE, staleCreate |-> FALSE,
             seedAfterClear |-> FALSE, stalePane |-> FALSE, resumedPreClear |-> FALSE,
             seedRevived |-> FALSE, crashed |-> FALSE, binary |-> "new",
             claimPhase |-> IF HeadBusy THEN "claimed" ELSE "none",
             claimObserved |-> <<>>, claimPayload |-> <<>>, claimReserved |-> <<>>, badOffered |-> FALSE,
             accepted |-> IF HeadBusy THEN ClaimSet ELSE {1}, receipts |-> {},
             q |-> [i \in Ids |-> IF HeadBusy /\ i \in ClaimSet THEN "claimed"
                                  ELSE IF ~HeadBusy /\ i = 1 THEN "queued" ELSE "incoming"],
             effects |-> [i \in Ids |-> 0]]

SeedPending == s.seed \in {"queued", "claimed"}
\* An input whose delivery to the old execution began before the restart was accepted.
UnitPending == \E i \in Ids : s.q[i] \in {"claimed", "inflight"}
FrontInput(i) == /\ (~SeedPending \/ Mutant = "SeedBehind") /\ s.launcher = 0
                 /\ s.q[i] = "queued"
                 /\ \A j \in 1..(i-1) : s.q[j] \notin {"queued", "claimed", "inflight"}
OwnerFree == s.awake /\ ~s.excl /\ ~s.clearExcl /\ ~s.clearing
\* The in-memory gate keeps the seed unstarted until the command released its exclusion.
SeedHeld == s.gate /\ Mutant # "NoGate"
OldTarget == s.row \in {"Bound", "PendingPlaced"}
Successor == s.row \in {"NewPending", "NewBound"}
Vacant == s.row \in {"Retired", "Missing", "Legacy"}
\* From acceptance until the restart ended or failed, no delivery may start on the old pane.
RestartOpen == s.excl \/ (Seeded /\ s.cmd \in {"seeded", "closing", "retiring", "postlude"})
Superseded == s.epoch # 0
Early == s.lphase \in {"begun", "pending", "created"}
FrontFails(l) == [s EXCEPT !.launcher = 0, !.lphase = "none",
                           !.checked = FALSE, !.permission = FALSE, !.effectKind = "none", !.effectBegan = FALSE,
                           !.seed = IF l = SeedId /\ @ = "claimed" THEN "dropped" ELSE @,
                           !.visible = @ \/ (l = SeedId /\ s.seed = "claimed" /\ Mutant # "NoNotice"),
                           !.q = [i \in Ids |-> IF i = l /\ s.q[i] = "queued" THEN "retained" ELSE s.q[i]]]

\* The B2 restart port remains future work. Its seed commit is the acceptance
\* point when it is admitted; without it the terminate's fence is.
Request == /\ s.cmd = "idle"
           /\ s' = IF Policy = "before" \/ Refusable
                   THEN [s EXCEPT !.cmd = "notice", !.awake = TRUE]
                   ELSE [s EXCEPT !.seed = IF Seeded THEN "queued" ELSE "none",
                                  !.gate = NeedsClose, !.awake = TRUE,
                                  !.warned = Busy /\ NeedsClose,
                                  !.cmd = IF NeedsClose THEN "seeded" ELSE "answering"]
Admit(i) == /\ s.cmd # "idle" /\ s.q[i] = "incoming"
            /\ s' = [s EXCEPT !.accepted = @ \cup {i}, !.q[i] = "queued", !.awake = TRUE]
\* A drive that finds the transition taken or the seed gated parks until the next wake.
OwnerParks == /\ s.awake
              /\ s.excl \/ s.clearExcl \/ s.clearing \/ (s.seed = "queued" /\ SeedHeld)
              /\ s' = [s EXCEPT !.awake = FALSE]
\* The front claim freezes its ordered originals. A later arrival is not its follower.
ClaimRead == /\ s.claimPhase = "claimed"
             /\ s' = [s EXCEPT !.claimObserved = ClaimMembers, !.claimPayload = ClaimMembers,
                               !.claimPhase = "prepared"]
\* A caller can offer a mismatching payload once; reserve must reject it, then retry normally.
ClaimBadPayload == /\ s.claimPhase = "claimed" /\ ~s.badOffered /\ 3 \in s.accepted
                   /\ s' = [s EXCEPT !.claimObserved = ClaimMembers,
                                     !.claimPayload = Append(ClaimMembers, 3),
                                     !.badOffered = TRUE, !.claimPhase = "prepared"]
ClaimReserve == /\ s.claimPhase = "prepared" /\ s.oldLive
                /\ s.claimPayload = ClaimMembers
                /\ s' = [s EXCEPT !.claimReserved = s.claimPayload, !.claimPhase = "reserved",
                                  !.q = [i \in Ids |-> IF i \in {s.claimPayload[j] : j \in DOMAIN s.claimPayload}
                                                        THEN "inflight" ELSE s.q[i]]]
ClaimReject == /\ s.claimPhase = "prepared" /\ s.claimPayload # ClaimMembers
               /\ s' = [s EXCEPT !.claimPayload = <<>>, !.claimPhase = "claimed"]
ClaimDone == /\ s.claimPhase = "reserved" /\ \A i \in ClaimSet : s.q[i] # "inflight"
             /\ s' = [s EXCEPT !.claimPhase = "done"]
\* The unit's own receipt settles it; the seed sits behind the fixed head and followers.
InflightSettle(i) == /\ s.q[i] = "inflight" /\ s.oldLive /\ Mutant # "PrependOverHead"
                     /\ s' = [s EXCEPT !.q[i] = "delivered", !.effects[i] = @ + 1]
\* terminate_scoped bound to the row's owner and dispatch: transition, fence, exact close,
\* end read, retire CAS, settlement.
TakeExclusion == /\ s.cmd = "seeded" /\ ~s.clearExcl
                 /\ s' = [s EXCEPT !.excl = TRUE, !.cmd = "closing"]
\* The terminate names the old nonce: a row that moved on refuses.
CloseExact == /\ s.cmd = "closing" /\ CloseReply = "confirmed" /\ OldTarget
              /\ s' = [s EXCEPT !.oldLive = IF Mutant = "SkipClose" THEN @ ELSE FALSE,
                               !.cmd = "retiring"]
CloseFails == /\ s.cmd = "closing" /\ (CloseReply # "confirmed" \/ ~OldTarget)
              /\ s' = [s EXCEPT !.cmd = "failing"]
Retire == /\ s.cmd = "retiring"
          /\ s' = [s EXCEPT !.row = "Retired", !.cmd = "postlude",
                           !.inflight = IF Mutant = "NoSettle" THEN @ ELSE FALSE,
                           !.settled = IF Mutant = "NoSettle" THEN @ ELSE TRUE,
                           !.claimPhase = IF ActiveClaim /\ Mutant # "PrependOverHead" THEN "revoked" ELSE @,
                           !.q = [i \in Ids |->
                                    IF s.q[i] = "queued" /\ Mutant = "DropQueue" THEN "lost"
                                    ELSE IF s.q[i] \in {"claimed", "inflight"} /\ Mutant # "PrependOverHead"
                                    THEN "retained" ELSE s.q[i]]]
\* after_retire, still inside the exclusion. A clear epoch that moved since acceptance means
\* a clear overtook this restart: the selector is forgotten as that clear would have done.
Postlude ==
  /\ s.cmd = "postlude"
  /\ s' = IF Mutant = "R1Launch"
          THEN [s EXCEPT !.excl = FALSE, !.gate = FALSE, !.awake = TRUE, !.row = "NewBound",
                         !.panes = 1, !.stalePane = Superseded, !.rowDispatch = FALSE,
                         !.seed = IF @ = "queued" THEN "delivered" ELSE @,
                         !.seedEffects = IF s.seed = "queued" THEN @ + 1 ELSE @,
                         !.seedAfterClear = Superseded /\ s.seed = "queued",
                         !.ready = s.seed = "queued", !.cmd = "answering", !.unsafeEffect = TRUE]
          ELSE [s EXCEPT !.excl = FALSE, !.gate = FALSE,
                         !.awake = @ \/ Mutant # "NoWake",
                         !.rowDispatch = IF Mutant = "UnboundDispatch" THEN @ ELSE FALSE,
                         !.retries = IF Mutant = "RetryDispatch" /\ s.rowDispatch THEN @ + 1 ELSE @,
                         !.selector = IF Superseded /\ Mutant \notin {"NoSupersedeCheck", "ReloadSeed"}
                                      THEN "forgotten" ELSE @,
                         !.seed = IF ~Superseded THEN @
                                  ELSE IF Mutant = "ReloadSeed" /\ Seeded THEN "queued"
                                  ELSE IF @ = "queued" /\ Mutant # "NoSupersedeCheck" THEN "cancelled"
                                  ELSE @,
                         !.seedRevived = Superseded /\ Mutant = "ReloadSeed" /\ Seeded,
                         !.cmd = "answering"]
\* The reply reads the clear epoch, never the seed's absence: a seed already delivered or
\* failed is not a clear.
Answer ==
  /\ s.cmd = "answering"
  /\ LET guess == Mutant = "SeedAbsentIsClear" /\ Seeded /\ ~SeedPending /\ ~Superseded
     IN s' = [s EXCEPT !.cmd = "done", !.answered = TRUE,
                       !.visible = @ \/ (Superseded /\ Mutant # "NoNotice"),
                       !.falseSuperseded = guess,
                       !.selector = IF guess THEN "forgotten" ELSE @]
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

SeedStartable == /\ s.seed = "queued" /\ OwnerFree /\ ~SeedHeld /\ s.launcher = 0
                 /\ ~UnitPending
\* The seed's permit at its start: an execution it was queued against that still runs refuses
\* it; one that ended but is not retired yet makes it wait for the row's recovery.
\* An older binary reads the seed as a headless start and refuses it as executor_not_wired.
SeedRefused ==
  /\ SeedStartable
  /\ s.binary = "old" \/ (OldTarget /\ s.oldLive /\ Mutant # "NoPermit")
  /\ LET silent == s.binary = "old" /\ Mutant = "IntentField"
     IN s' = [s EXCEPT !.seed = IF silent THEN "stripped" ELSE "dropped",
                       !.visible = @ \/ (~silent /\ Mutant # "NoNotice")]
SeedToOld == /\ Mutant = "NoPermit" /\ SeedStartable /\ s.binary = "new" /\ OldTarget /\ s.oldLive
             /\ s' = [s EXCEPT !.seed = "delivered", !.oldSeed = TRUE, !.seedEffects = @ + 1, !.unsafeEffect = TRUE]
SeedJoin == /\ SeedStartable /\ s.binary = "new" /\ s.row = "NewBound"
            /\ s' = [s EXCEPT !.seed = "claimed", !.launcher = SeedId,
                             !.lphase = "created", !.lEpoch = s.epoch,
                             !.checked = FALSE, !.effectBegan = FALSE,
                             !.permission = FALSE, !.effectKind = "none"]
\* A launching turn registers with the clear epoch it saw. The seed's permit first finishes
\* the restart's own disconnect, so a crash before the postlude leaves no dispatch behind.
Detaches(l) == l = SeedId /\ Mutant # "NoPermitDetach"
LaunchBegin(l) ==
  /\ IF l = SeedId THEN SeedStartable /\ s.binary = "new" ELSE FrontInput(l) /\ OwnerFree
  /\ Vacant
  /\ s' = [s EXCEPT !.launcher = l, !.lphase = "begun", !.lEpoch = s.epoch,
                    !.seed = IF l = SeedId THEN "claimed" ELSE @,
                    !.rowDispatch = IF Detaches(l) THEN FALSE ELSE @,
                    !.inherit = @ \/ (s.rowDispatch /\ ~Detaches(l)),
                    !.ready = @ \/ (Mutant = "ReadyOnAdmit" /\ l = SeedId),
                    !.falseReady = @ \/ (Mutant = "ReadyOnAdmit" /\ l = SeedId),
                    !.resumedPreClear = @ \/ (Superseded /\ s.selector = "kept")]
\* Pending is installed over a vacant row before create; B1 report maps the actual leaf ports.
InstallPending == /\ s.lphase = "begun" /\ Vacant
                  /\ s' = [s EXCEPT !.row = "NewPending", !.lphase = "pending",
                                   !.created = "no", !.creates = 0]
Stale == s.epoch # s.lEpoch
ChecksLaunch == Mutant \notin {"NoLaunchCheck", "MainClear"}
ChecksSubmit == Mutant \notin {"NoSubmitCheck", "MainClear"}
Unclaim == [s EXCEPT !.launcher = IF Mutant = "LeakLaunching" THEN @ ELSE 0, !.lphase = "none",
                     !.checked = FALSE, !.permission = FALSE, !.effectKind = "none", !.effectBegan = FALSE,
                     !.seed = IF @ = "claimed" THEN "queued" ELSE @]
\* A launch whose epoch moved retires its own unsent Pending instead of creating.
AbortUnsent == /\ s.lphase = "pending" /\ (Stale \/ s.clearing) /\ ChecksLaunch /\ ~s.effectBegan
               /\ s' = [Unclaim EXCEPT !.row = "Retired"]
\* CheckEpoch observes only. Grant issuance and clear epoch updates share a mutex.
\* A granted effect keeps its counter, not that mutex, through physical completion.
CheckCreateEpoch == /\ s.lphase = "pending" /\ ~s.checked /\ ~(Stale /\ ChecksLaunch)
                    /\ s' = [s EXCEPT !.checked = TRUE]
BeginCreateEffect == /\ s.lphase = "pending" /\ s.checked /\ ~s.effectBegan
                     /\ ~((Stale \/ s.clearing) /\ ChecksLaunch) \/ EffectPolicy = "check_only_create"
                     /\ s' = [s EXCEPT !.effectBegan = TRUE, !.effectKind = "create",
                                      !.permission = EffectPolicy # "check_only_create",
                                      !.grantEpoch = s.epoch, !.grantValid = ~Stale /\ ~s.clearing,
                                      !.staleCreate = @ \/ Stale \/ s.clearing]
Create == /\ s.lphase = "pending" /\ s.effectBegan /\ s.effectKind = "create"
          /\ s.permission \/ EffectPolicy = "check_only_create"
          /\ LET pane == LaunchReply # "NotSent"
             IN s' = [s EXCEPT !.creates = @ + 1, !.lphase = "created",
                              !.unsafeEffect = @ \/ ~s.permission \/ ~s.grantValid, !.permission = FALSE,
                              !.effectBegan = FALSE, !.effectKind = "none", !.checked = FALSE,
                              !.panes = @ + (IF pane THEN 1 ELSE 0),
                                                            !.stalePane = @ \/ (Stale /\ pane),
                              !.created = IF LaunchReply = "Created" THEN "yes"
                                          ELSE IF LaunchReply = "NotSent" THEN "notsent" ELSE "unknown"]
LaunchNotSent == /\ s.lphase = "created" /\ s.created = "notsent"
                 /\ s' = [FrontFails(s.launcher) EXCEPT !.row = "Retired", !.created = "no"]
\* An unknown create acknowledgement keeps Pending, fails its input visibly and is only
\* probed again; it is never created a second time.
LaunchUncertain == /\ s.lphase = "created" /\ s.created = "unknown"
                   /\ s' = FrontFails(s.launcher)
ReplayCreate == /\ Mutant = "ReplayUnknown" /\ s.row = "NewPending" /\ s.created = "unknown"
                /\ s.creates < 2
                /\ s' = [s EXCEPT !.creates = @ + 1, !.panes = @ + 1, !.unsafeEffect = TRUE]
\* The first input's three steps: the physical submit, the owner's receipt, the source bind.
AbortSubmit == /\ s.lphase = "created" /\ s.created = "yes" /\ (Stale \/ s.clearing) /\ ChecksSubmit /\ ~s.effectBegan
               /\ s' = Unclaim
CheckSubmitEpoch == /\ s.lphase = "created" /\ s.created = "yes" /\ ~s.checked /\ ~(Stale /\ ChecksSubmit)
                    /\ s' = [s EXCEPT !.checked = TRUE]
BeginSubmitEffect == /\ s.lphase = "created" /\ s.created = "yes" /\ s.checked /\ ~s.effectBegan
                     /\ ~((Stale \/ s.clearing) /\ ChecksSubmit)
                     /\ s' = [s EXCEPT !.effectBegan = TRUE, !.effectKind = "submit",
                                      !.permission = EffectPolicy # "early_release_submit",
                                      !.grantEpoch = s.epoch, !.grantValid = ~Stale /\ ~s.clearing,
                                      !.seedAfterClear = @ \/ (s.launcher = SeedId /\ (Superseded \/ s.clearing))]
SubmitEffect == /\ s.lphase = "created" /\ s.created = "yes" /\ s.effectBegan /\ s.effectKind = "submit"
                /\ s.permission \/ EffectPolicy = "early_release_submit"
                /\ s' = IF s.launcher = SeedId
                        THEN [s EXCEPT !.lphase = "sent", !.seedEffects = @ + 1,
                                       !.unsafeEffect = @ \/ ~s.permission \/ ~s.grantValid, !.permission = FALSE,
                                       !.effectBegan = FALSE, !.effectKind = "none", !.checked = FALSE,
                                       !.seedAfterClear = @]
                        ELSE [s EXCEPT !.lphase = "sent", !.effects[s.launcher] = @ + 1,
                                       !.unsafeEffect = @ \/ ~s.permission \/ ~s.grantValid, !.permission = FALSE,
                                       !.effectBegan = FALSE, !.effectKind = "none", !.checked = FALSE]
SettleReceipt == /\ s.lphase = "sent"
                 /\ s' = IF s.launcher = SeedId
                         THEN [s EXCEPT !.lphase = "settled", !.seed = "delivered"]
                         ELSE [s EXCEPT !.lphase = "settled", !.q[s.launcher] = "delivered"]
BindSource == /\ s.lphase = "settled"
              /\ s' = [s EXCEPT !.row = "NewBound", !.launcher = 0, !.lphase = "none",
                               !.ready = @ \/ (s.launcher = SeedId)]
\* The probe adopts a Pending whose pane exists and nobody is launching.
Adopt == /\ s.row = "NewPending" /\ s.created \in {"unknown", "yes"} /\ s.launcher = 0
         /\ Mutant # "ReplayUnknown"
         /\ s' = [s EXCEPT !.created = "yes", !.row = "NewBound",
                          !.ready = @ \/ (s.seed = "delivered")]
Deliver(i) == /\ FrontInput(i) /\ OwnerFree /\ s.row = "NewBound" /\ s.effects[i] < 2
              /\ s' = [s EXCEPT !.q[i] = "delivered", !.effects[i] = @ + 1]
\* Every delivery to the old execution is observed: one that starts while the restart is
\* open breaks the rule, one before acceptance or after a confirmed failure does not.
DeliverOld(i) == /\ FrontInput(i) /\ OwnerFree /\ OldTarget /\ s.oldLive /\ s.effects[i] < 2
                 /\ s' = [s EXCEPT !.q[i] = "delivered", !.effects[i] = @ + 1,
                                  !.oldInputs = @ + (IF RestartOpen THEN 1 ELSE 0)]
RefuseUnknown(i) == /\ FrontInput(i) /\ OwnerFree /\ s.row = "Unknown"
                    /\ s' = [s EXCEPT !.q[i] = "retained"]
StaleOldDelivery(i) == /\ Mutant = "OldInput" /\ s.excl /\ s.oldLive /\ s.q[i] = "queued"
                       /\ s.oldInputs = 0
                       /\ s' = [s EXCEPT !.oldInputs = 1, !.effects[i] = @ + 1, !.q[i] = "delivered"]
ReplayInput(i) == /\ Mutant = "ReplayInput" /\ s.q[i] = "delivered" /\ s.effects[i] = 1
                  /\ s' = [s EXCEPT !.q[i] = "queued"]
\* Existing recovery outside this design: a Bound row whose pane is gone is retired, and
\* an unplaced Pending is resolved by the turn's own resolver.
RowEndRecovery == /\ OldTarget /\ ~s.oldLive /\ ~s.excl
                  /\ s.inflight \/ (SeedPending /\ OwnerFree /\ RecoveryPolicy = "owned")
                  /\ s' = [s EXCEPT !.row = "Retired", !.inflight = FALSE, !.settled = TRUE,
                                   !.awake = TRUE]
ResolveUnplaced == /\ s.row = "PendingUnplaced"
                   /\ s' = [s EXCEPT !.row = "Retired"]

\* The daemon dies: gate, exclusion, epoch and the command are gone; the queue, the row and
\* the panes stay. A submit whose receipt was not committed is uncertain and never re-sent.
Crash ==
  /\ CrashOn /\ ~s.crashed /\ s.cmd # "idle" /\ ~s.clearing /\ ~s.clearRequested
  /\ LET l == s.launcher
         sent == s.lphase = "sent"
         requeue == Mutant = "RequeueUncertain"
     IN s' = [s EXCEPT !.crashed = TRUE, !.cmd = "dead", !.gate = FALSE, !.excl = FALSE,
                       !.awake = TRUE, !.binary = IF Rollback THEN "old" ELSE "new",
                       !.claimPhase = IF ActiveClaim THEN "revoked" ELSE @,
                       !.launcher = 0, !.lphase = "none", !.lEpoch = s.epoch,
                       !.checked = FALSE, !.permission = FALSE, !.effectKind = "none", !.effectBegan = FALSE,
                       !.row = IF s.lphase = "pending" THEN "Retired" ELSE @,
                       !.seed = IF @ # "claimed" THEN @
                                ELSE IF sent /\ ~requeue THEN "uncertain" ELSE "queued",
                       !.visible = @ \/ (l = SeedId /\ sent /\ ~requeue),
                       !.q = [i \in Ids |->
                                IF s.q[i] = "inflight" THEN "retained"
                                ELSE IF s.q[i] = "claimed" THEN "queued"
                                ELSE IF i = l /\ sent /\ ~requeue THEN "retained"
                                ELSE s.q[i]]]

\* The control clear entry first advances its epoch; it holds the transition when
\* that is free and proceeds without it after the 3 second wait otherwise.
ClearRequest == /\ ClearOn # "off" /\ s.clear = "none" /\ ~s.clearRequested /\ s.cmd # "idle"
              /\ (ClearOn = "serial" => ~s.excl) /\ (ClearOn = "overtake" => s.excl)
              /\ (ClearOn = "launch" => s.launcher # 0)
              /\ s' = [s EXCEPT !.clearRequested = TRUE]
ClearBegin == /\ s.clearRequested /\ s.clear = "none"
              /\ s' = [s EXCEPT !.clear = "begun", !.epoch = 1, !.clearing = TRUE,
                               !.clearExcl = ~s.excl, !.stalePane = @ \/ s.panes > 0]
\* The queue is discarded with receipts and a launching turn's unsubmitted input with it.
ClearDiscard ==
  /\ s.clear = "begun"
  /\ LET cancels == Early /\ ~(s.effectBegan /\ s.effectKind = "submit")
                  /\ Mutant \notin {"ClearKeepsClaim", "MainClear"}
         gone(i) == s.q[i] \in {"claimed", "inflight"} \/ (s.q[i] = "queued" /\ (i # s.launcher \/ cancels))
     IN s' = [s EXCEPT !.clear = "discarded", !.clearEffective = TRUE,
                       !.claimPhase = IF ActiveClaim THEN "revoked" ELSE @,
                       !.seed = IF @ = "queued" \/ (@ = "claimed" /\ cancels) THEN "cancelled" ELSE @,
                       !.receipts = @ \cup {i \in Ids : gone(i)},
                       !.q = [i \in Ids |-> IF gone(i) THEN "cancelled" ELSE s.q[i]]]
Finished(t, how) == [t EXCEPT !.clear = how, !.clearing = FALSE, !.clearExcl = FALSE, !.awake = TRUE,
  !.undrainedClear = @ \/ (s.launcher # 0 /\ s.lEpoch < s.epoch)
                        \/ (s.effectBegan /\ s.grantEpoch < s.epoch),
  !.knownUnknownClear = how = "unconfirmed" /\ s.created = "unknown"
                        /\ s.launcher = 0 /\ ~s.permission /\ ~s.effectBegan]
\* The teardown waits for a launching turn to end, then acts on the row it reads.
ClearEnd ==
  /\ s.clear = "discarded"
  /\ (s.launcher = 0 /\ ~s.permission) \/ Mutant \in {"ClearNoWait", "MainClear"}
  /\ s' = IF s.launcher # 0 /\ Mutant = "MainClear" THEN Finished(s, "held")
          ELSE IF OldTarget /\ s.excl THEN Finished(s, "gaveup")
          ELSE IF OldTarget
          THEN [Finished(s, "done") EXCEPT !.oldLive = FALSE, !.row = "Retired",
                       !.selector = "forgotten", !.inflight = FALSE, !.settled = TRUE]
          ELSE IF Successor /\ (s.created # "yes" \/ s.launcher # 0)
          THEN Finished(s, "unconfirmed")
          ELSE IF Successor
          THEN [Finished(s, "done") EXCEPT !.panes = 0, !.row = "Retired", !.created = "no",
                       !.selector = "forgotten", !.stalePane = FALSE]
          ELSE IF Vacant
          THEN [Finished(s, "done") EXCEPT
                       !.selector = IF Mutant = "ClearSkipsRetired" /\ s.row = "Retired"
                                    THEN @ ELSE "forgotten"]
          ELSE Finished(s, "unconfirmed")
\* A known operation replay/conflict is an existing-ticket result, not an
\* admission capacity failure. It must not take the legitimate seedless route.
ReplayRequest == /\ ReplayMode # "off" /\ ~s.opSeen /\ s.cmd # "idle" /\ s.seed # "none"
                 /\ s' = [s EXCEPT !.opSeen = TRUE,
                                  !.opSeedless = OpPolicy = "seedless",
                                  !.opDuplicate = OpPolicy = "duplicate"]
Wait == UNCHANGED s

Next == \/ ClaimRead \/ ClaimBadPayload \/ ClaimReserve \/ ClaimReject \/ ClaimDone
        \/ Request \/ OwnerParks \/ TakeExclusion \/ CloseExact \/ CloseFails \/ Retire
        \/ Postlude \/ Answer \/ Fail \/ Withdraw \/ NotifyFailure \/ SeedRefused \/ SeedToOld
        \/ SeedJoin \/ InstallPending \/ AbortUnsent \/ CheckCreateEpoch \/ BeginCreateEffect \/ Create \/ LaunchNotSent
        \/ LaunchUncertain \/ ReplayCreate \/ AbortSubmit \/ CheckSubmitEpoch \/ BeginSubmitEffect \/ SubmitEffect \/ SettleReceipt
        \/ BindSource \/ Adopt \/ RowEndRecovery \/ ResolveUnplaced \/ Crash
        \/ ClearRequest \/ ClearBegin \/ ClearDiscard \/ ClearEnd \/ ReplayRequest \/ Wait
        \/ \E l \in 1..SeedId : LaunchBegin(l)
        \/ \E i \in Ids : \/ Admit(i) \/ InflightSettle(i) \/ Deliver(i) \/ DeliverOld(i)
                          \/ RefuseUnknown(i) \/ StaleOldDelivery(i) \/ ReplayInput(i)
\* Each step of the command, the owner and a started clear eventually runs while it stays
\* enabled. Crash, a clear's arrival and a parked drive are not fair: they may never happen.
Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(ClaimRead) /\ WF_vars(ClaimReserve) /\ WF_vars(ClaimReject) /\ WF_vars(ClaimDone)
        /\ WF_vars(Request) /\ WF_vars(TakeExclusion) /\ WF_vars(CloseExact)
        /\ WF_vars(CloseFails) /\ WF_vars(Retire) /\ WF_vars(Postlude) /\ WF_vars(Answer)
        /\ WF_vars(Fail) /\ WF_vars(Withdraw) /\ WF_vars(NotifyFailure)
        /\ WF_vars(SeedRefused) /\ WF_vars(SeedToOld) /\ WF_vars(SeedJoin)
        /\ WF_vars(InstallPending) /\ WF_vars(AbortUnsent) /\ WF_vars(CheckCreateEpoch) /\ WF_vars(BeginCreateEffect) /\ WF_vars(Create)
        /\ WF_vars(LaunchNotSent) /\ WF_vars(LaunchUncertain) /\ WF_vars(AbortSubmit)
        /\ WF_vars(CheckSubmitEpoch) /\ WF_vars(BeginSubmitEffect) /\ WF_vars(SubmitEffect) /\ WF_vars(SettleReceipt) /\ WF_vars(BindSource)
        /\ WF_vars(Adopt) /\ WF_vars(RowEndRecovery) /\ WF_vars(ResolveUnplaced)
        /\ WF_vars(ClearBegin) /\ WF_vars(ClearDiscard) /\ WF_vars(ClearEnd)
        /\ \A l \in 1..SeedId : WF_vars(LaunchBegin(l))
        /\ \A i \in Ids : /\ WF_vars(Admit(i)) /\ WF_vars(InflightSettle(i))
                          /\ WF_vars(Deliver(i)) /\ WF_vars(DeliverOld(i))
                          /\ WF_vars(RefuseUnknown(i))

Settled(i) == \/ s.q[i] \in {"delivered", "retained"}
              \/ (s.q[i] = "cancelled" /\ i \in s.receipts)
InputCustody == \A i \in s.accepted : s.q[i] \in {"queued", "claimed", "inflight"} \/ Settled(i)
InputAtMostOnce == \A i \in Ids : s.effects[i] <= 1
NoOldPaneInput == s.oldInputs = 0
\* The greeting never reaches the execution the restart was meant to end.
NoFalseReady == ~s.oldSeed
SeedAtMostOnce == s.seedEffects <= 1
NoBlindCreate == s.creates <= 1
SingleSession == (IF s.oldLive THEN 1 ELSE 0) + s.panes <= 1
InflightSettledBeforeReady == s.ready => s.settled /\ ~s.inflight /\ (~(Busy /\ NeedsClose) \/ s.warned)
\* Ready is only ever said on the seed's receipt, never on its admission.
ReadyEvidence == ~s.falseReady
NoInheritedDispatch == ~s.inherit
\* The restart writes no dispatch outcome and creates no retry dispatch.
NoDispatchRepost == s.retries = 0
\* After clear advances its epoch, no new grant may authorize an old launch/seed.
\* Effects already granted before that advance may finish while clear drains them.
NoSeedAfterClear == ~s.seedAfterClear
NoStaleCreate == ~s.staleCreate
\* A clear that ended leaves no session a turn older than it launched.
NoSessionAfterClear == (s.clear \in {"done", "gaveup", "held", "unconfirmed"} /\ ~s.excl)
                       => ~s.undrainedClear /\ (s.knownUnknownClear \/ ~s.stalePane)
\* No launch after a clear began resumes the conversation that clear ended.
NoResumeAfterClear == ~s.resumedPreClear
NoSeedRevival == ~s.seedRevived
NoFalseSuperseded == ~s.falseSuperseded
RequestResolves == (s.seed # "none") ~> (s.ready \/ s.visible \/ s.seed = "cancelled")
CommandAnswers == (s.cmd # "idle") ~> (s.answered \/ s.visible \/ s.crashed)
RestartAvailable == <> (s.ready \/ (~Healthy /\ s.cmd \in {"done", "dead"}))
AcceptedInputResolves == \A i \in Ids : (i \in s.accepted) ~> Settled(i)
ClearForgets == s.clearEffective ~> (s.selector = "forgotten")
ClearCompletes == (s.clear = "begun") ~> (s.clear \in {"done", "gaveup", "held", "unconfirmed"})

NoOperationSeedless == ~s.opSeedless
NoOperationDuplicateSeed == ~s.opDuplicate
EffectHasLivePermission == ~s.unsafeEffect
EndedSeedRecovers == (s.crashed /\ SeedPending /\ OldTarget /\ ~s.oldLive /\ ~s.excl /\ s.binary = "new")
                     ~> (s.row = "Retired" \/ Successor \/ (~SeedPending /\ s.visible))
ClearRequestCompletes == s.clearRequested ~> (s.clear \in {"done", "gaveup", "held", "unconfirmed"})

\* B1's in-memory ordered snapshot and reserve's exact payload comparison.
ClaimedMembersFixed == s.claimObserved = <<>> \/ s.claimObserved = ClaimMembers
ReservationUsesClaim == s.claimReserved = <<>> \/ s.claimReserved = ClaimMembers
ClaimedUnitResolves == ActiveClaim ~> s.claimPhase \in {"done", "revoked"}

====
