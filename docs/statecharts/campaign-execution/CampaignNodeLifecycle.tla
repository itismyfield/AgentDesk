------------------------ MODULE CampaignNodeLifecycle ------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS Required, MaxRepairs, MaxStops, MaxCrash,
          CheckGates, TargetBound, PositionGuard, BudgetStops, Recompute
VARIABLES control, stage, open, result, failed, cycle, rev, epoch, approved, facts,
          cmd, applied, stops, crashes, blind, badEntry, dup, retried, landingEpoch, worker
lifeVars == <<control, stage, open, result, failed, cycle, rev, epoch, approved, facts,
          cmd, applied, stops, crashes, blind, badEntry, dup, retried, landingEpoch>>
vars == <<lifeVars, worker>>
node == <<stage, open, result, failed, cycle>>
flags == <<badEntry, dup, retried, landingEpoch, worker>>
driver == <<cmd, applied, crashes, blind>>

None == [kind |-> "none", epoch |-> 0]

\* The truth: an approval of this gate for the current candidate and definition revision.
Current(g) == g \notin Required \/ <<g, cycle, rev>> \in approved
\* What the code checks; the controls weaken it.
Passes(g) ==
    \/ ~CheckGates \/ g \notin Required
    \/ IF TargetBound THEN <<g, cycle, rev>> \in approved
       ELSE \E c \in 0..MaxRepairs, r \in 0..1 : <<g, c, r>> \in approved

\* The approval slots of the boundary the node stands in front of.
Boundary ==
    CASE stage = "requirements" -> {"start", "implementation"}
      [] stage = "work" /\ ~open -> {"implementation"}
      [] stage = "work" /\ open /\ result = "pass" -> {"review"}
      [] stage = "landing" -> {"landing"}
      [] OTHER -> {}

\* The next action is a function of the saved state only.
Wanted ==
    CASE failed /\ (BudgetStops \/ retried) -> "none"
      [] stage \in {"requirements", "work"} /\ ~open -> "assign"
      [] stage = "work" /\ open /\ result = "pass" -> "review"
      [] stage \in {"work", "review"} /\ open /\ result # "none" -> "accept"
      [] stage = "landing" -> "land"
      [] stage = "confirm" /\ facts -> "complete"
      [] OTHER -> "none"

LifecycleInit ==
    /\ control = "prepared" /\ stage = "requirements" /\ open = FALSE /\ result = "none"
    /\ failed = FALSE /\ cycle = 0 /\ rev = 0 /\ epoch = 0 /\ approved = {} /\ facts = FALSE
    /\ cmd = None /\ applied = {} /\ stops = 0 /\ crashes = 0 /\ blind = FALSE
    /\ badEntry = FALSE /\ dup = FALSE /\ retried = FALSE /\ landingEpoch = 0

Init == LifecycleInit /\ worker = "inactive"

BeginRun ==
    /\ control = "prepared" /\ control' = "running"
    /\ UNCHANGED <<node, rev, epoch, approved, facts, driver, stops, flags>>

Approve(g) ==
    /\ control = "running" /\ g \in Required /\ g \in Boundary
    /\ <<g, cycle, rev>> \notin approved
    /\ approved' = approved \cup {<<g, cycle, rev>>}
    /\ UNCHANGED <<control, node, rev, epoch, facts, driver, stops, flags>>

Compute ==
    /\ control = "running" /\ cmd = None /\ ~blind /\ Wanted # "none"
    /\ \A g \in Boundary : Passes(g)
    /\ cmd' = [kind |-> Wanted, epoch |-> epoch]
    /\ UNCHANGED <<control, node, rev, epoch, approved, facts, applied, crashes, blind,
                    stops, flags>>

QualityFailure ==
    IF cycle < MaxRepairs
    THEN /\ cycle' = cycle + 1 /\ stage' = "work" /\ failed' = failed
    ELSE /\ cycle' = cycle /\ stage' = stage /\ failed' = TRUE

\* The server applies a command only at the position it was computed for.
Apply ==
    /\ control = "running" /\ cmd # None /\ cmd.epoch = epoch /\ cmd.kind = Wanted
    /\ \A g \in Boundary : Passes(g)
    /\ epoch' = epoch + 1 /\ applied' = applied \cup {cmd}
    /\ landingEpoch' = IF cmd.kind = "land" THEN epoch' ELSE landingEpoch
    /\ badEntry' = (badEntry \/ \E g \in Boundary : ~Current(g))
    /\ retried' = (retried \/ (cmd.kind = "assign" /\ failed))
    /\ CASE cmd.kind = "assign" ->
              /\ stage' = "work" /\ open' = TRUE /\ result' = "none"
              /\ UNCHANGED <<failed, cycle, control>>
         [] cmd.kind = "review" ->
              /\ stage' = "review" /\ open' = TRUE /\ result' = "none"
              /\ UNCHANGED <<failed, cycle, control>>
         [] cmd.kind = "accept" /\ result = "fail" ->
              /\ QualityFailure /\ open' = FALSE /\ result' = "none" /\ UNCHANGED control
         [] cmd.kind = "accept" /\ result = "pass" ->
              /\ stage' = "landing" /\ open' = FALSE /\ result' = "none"
              /\ UNCHANGED <<failed, cycle, control>>
         [] cmd.kind = "land" ->
              /\ stage' = "confirm" /\ UNCHANGED <<open, result, failed, cycle, control>>
         [] cmd.kind = "complete" ->
              /\ stage' = "complete" /\ control' = "succeeded"
              /\ UNCHANGED <<open, result, failed, cycle>>
    /\ UNCHANGED <<rev, approved, facts, cmd, crashes, blind, stops, dup, worker>>

\* Control: without the position guard a retransmitted command takes effect a second time.
Replay ==
    /\ ~PositionGuard /\ control = "running" /\ cmd \in applied /\ ~dup /\ dup' = TRUE
    /\ UNCHANGED <<control, node, rev, epoch, approved, facts, driver, stops, badEntry, retried, landingEpoch, worker>>

\* The driver drops a command once it is answered, stale or stopped.
Ack ==
    /\ cmd # None /\ (cmd \in applied \/ cmd.epoch # epoch \/ control # "running")
    /\ cmd' = None
    /\ UNCHANGED <<control, node, rev, epoch, approved, facts, applied, crashes, blind,
                    stops, flags>>

\* A restart loses the in-flight command; the driver recomputes it from the saved state.
Crash ==
    /\ crashes < MaxCrash /\ crashes' = crashes + 1 /\ cmd' = None
    /\ blind' = (blind \/ ~Recompute)
    /\ UNCHANGED <<control, node, rev, epoch, approved, facts, applied, stops, flags>>

Outcome(r) ==
    /\ control = "running" /\ open /\ result = "none" /\ result' = r
    /\ UNCHANGED <<control, stage, open, failed, cycle, rev, epoch, approved, facts, driver,
                    stops, flags>>

Confirmed ==
    /\ stage = "confirm" /\ ~facts /\ facts' = TRUE
    /\ UNCHANGED <<control, node, rev, epoch, approved, driver, stops, flags>>

\* A stop interrupts the open attempt and is never a quality failure.
Stop ==
    /\ control = "running" /\ stops < MaxStops /\ stops' = stops + 1
    /\ control' = "stopped" /\ open' = FALSE /\ result' = "none" /\ epoch' = epoch + 1
    /\ UNCHANGED <<stage, failed, cycle, rev, approved, facts, driver, flags>>

Edit ==
    /\ control = "stopped" /\ rev = 0 /\ rev' = 1
    /\ UNCHANGED <<control, node, epoch, approved, facts, driver, stops, flags>>

KeepLanding == IF stage = "review" THEN "work" ELSE stage
LandingAfterResume == IF stage = "confirm" /\ ~facts THEN "landing" ELSE KeepLanding

\* Resume rechecks an unused Landing admission and keeps the same Work repair cycle.
Resume ==
    /\ control = "stopped" /\ control' = "running" /\ epoch' = epoch + 1
    /\ stage' = LandingAfterResume
    /\ UNCHANGED <<open, result, failed, cycle, rev, approved, facts, driver, stops, flags>>

Done == UNCHANGED vars
External == Confirmed \/ \E r \in {"pass", "fail"} : Outcome(r)
Next == BeginRun \/ (\E g \in Required : Approve(g)) \/ Compute \/ Apply \/ Replay \/ Ack
        \/ Crash \/ External \/ Stop \/ Edit \/ Resume \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(BeginRun) /\ WF_vars(Compute) /\ WF_vars(Apply)
        /\ WF_vars(Ack) /\ WF_vars(External)

NoUnapprovedEntry == ~badEntry
NextActionAtMostOnce == ~dup
NoRetryAfterBudget == ~retried
LandingAdmissionCurrent ==
    control = "running" /\ stage = "confirm" /\ ~facts => landingEpoch = epoch
SucceededOnlyWhenConfirmed == control = "succeeded" => facts /\ stage = "complete"

\* A running node whose boundary is approved must not rest there.
Movable ==
    /\ control \in {"prepared", "running"} /\ ~failed /\ stage # "complete"
    /\ \A g \in Boundary : Current(g)
ApprovedNodeMoves == []<>(~Movable)

\* One scoped host journal and two contenders for its writer lock.
ExclusiveWorker == TRUE
FenceRequired == TRUE
ReplayJournal == TRUE
HeadroomRequired == TRUE
WithoutGuard == FALSE

HostInit ==
    /\ LifecycleInit
    /\ worker = [holders |-> {}, fence |-> FALSE, resource |-> "waiting",
                  journal |-> "empty", boot |-> 0, journalBoot |-> 0, resets |-> 0, lostHandle |-> FALSE,
                  starts |-> 0, outcome |-> "pending", unfenced |-> FALSE,
                  ignoredWait |-> FALSE]

HostAcquire(n) ==
    /\ n \notin worker.holders /\ (~ExclusiveWorker \/ worker.holders = {})
    /\ worker' = [worker EXCEPT !.holders = @ \cup {n}]
    /\ UNCHANGED lifeVars

HostFence ==
    /\ worker.holders # {} /\ ~worker.fence
    /\ worker' = [worker EXCEPT !.fence = TRUE]
    /\ UNCHANGED lifeVars

HostResource ==
    /\ worker.resource = "waiting"
    /\ \E res \in {"ready", "unavailable"} : worker' = [worker EXCEPT !.resource = res]
    /\ UNCHANGED lifeVars

HostStart(n) ==
    /\ n \in worker.holders /\ worker.outcome = "pending" /\ worker.starts < 2
    /\ (~FenceRequired \/ worker.fence)
    /\ (~HeadroomRequired \/ worker.resource = "ready")
    /\ worker.journal = "empty" \/ (~ReplayJournal /\ worker.journal = "unknown")
    /\ worker' = [worker EXCEPT !.journal = "intent", !.journalBoot = worker.boot,
         !.starts = @ + 1, !.unfenced = @ \/ ~worker.fence, !.lostHandle = FALSE,
         !.ignoredWait = @ \/ worker.resource # "ready"]
    /\ UNCHANGED lifeVars

\* A server crash drops the writer. Reboot can also invalidate the saved process identity.
HostCrash ==
    /\ worker.holders # {} /\ worker.resets < 1
    /\ \E reboot \in BOOLEAN : worker' = [worker EXCEPT !.holders = {}, !.fence = FALSE,
         !.boot = IF reboot THEN 1 ELSE @, !.resets = @ + 1,
         !.journal = IF @ = "intent" THEN "lost" ELSE @, !.lostHandle = TRUE]
    /\ UNCHANGED lifeVars

HostRecover ==
    /\ worker.holders # {} /\ worker.journal = "lost"
    /\ worker' = [worker EXCEPT !.journal = "unknown"]
    /\ UNCHANGED lifeVars

HostReceipt ==
    /\ worker.holders # {} /\ worker.journal = "intent"
    /\ worker' = [worker EXCEPT !.journal = "started"]
    /\ UNCHANGED lifeVars

HostFinished ==
    /\ worker.holders # {} /\ worker.journal = "started" /\ worker.boot = worker.journalBoot
    /\ ~worker.lostHandle
    /\ worker' = [worker EXCEPT !.journal = "finished"]
    /\ UNCHANGED lifeVars

\* Observation loss is not an unsent launch. The original action remains spent.
HostFailure ==
    /\ worker.holders # {} /\ worker.outcome = "pending"
    /\ \/ worker.resource = "unavailable" /\ worker.journal = "empty"
       \/ worker.journal = "started" /\ (worker.boot # worker.journalBoot \/ worker.lostHandle)
       \/ worker.journal = "unknown"
    /\ worker' = [worker EXCEPT !.outcome = "failed"]
    /\ UNCHANGED lifeVars

HostDeliver ==
    /\ worker.holders # {} /\ worker.outcome = "pending" /\ worker.journal = "finished"
    /\ worker' = [worker EXCEPT !.outcome = "advanced", !.journal = "delivered"]
    /\ UNCHANGED lifeVars

HostClaim == \E n \in {1, 2} : HostAcquire(n)
HostInitiate == \E n \in {1, 2} : HostStart(n)
HostProgress == HostInitiate \/ HostRecover \/ HostReceipt \/ HostFinished \/ HostFailure \/ HostDeliver
HostNext == HostClaim \/ HostFence \/ HostResource \/ HostProgress \/ HostCrash \/ UNCHANGED vars
HostSpec == HostInit /\ [][HostNext]_vars /\ WF_vars(HostClaim) /\ WF_vars(HostFence)
            /\ WF_vars(HostResource) /\ WF_vars(HostProgress)

SingleHostWorker == Cardinality(worker.holders) <= 1
HostWritesFenced == ~worker.unfenced
HostStartsOnce == worker.starts <= 1
HostRespectsHeadroom == ~worker.ignoredWait
HostProgresses == <>(worker.outcome # "pending")
 =============================================================================
