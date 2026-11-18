------------------------ MODULE CampaignNodeLifecycle ------------------------
EXTENDS Naturals
CONSTANTS Required, MaxRepairs, MaxStops, MaxCrash,
          CheckGates, TargetBound, PositionGuard, BudgetStops, Recompute
VARIABLES control, stage, open, result, failed, cycle, rev, epoch, approved, facts,
          cmd, applied, stops, crashes, blind, badEntry, dup, retried
vars == <<control, stage, open, result, failed, cycle, rev, epoch, approved, facts,
          cmd, applied, stops, crashes, blind, badEntry, dup, retried>>
node == <<stage, open, result, failed, cycle>>
flags == <<badEntry, dup, retried>>
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

Init ==
    /\ control = "prepared" /\ stage = "requirements" /\ open = FALSE /\ result = "none"
    /\ failed = FALSE /\ cycle = 0 /\ rev = 0 /\ epoch = 0 /\ approved = {} /\ facts = FALSE
    /\ cmd = None /\ applied = {} /\ stops = 0 /\ crashes = 0 /\ blind = FALSE
    /\ badEntry = FALSE /\ dup = FALSE /\ retried = FALSE

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
    /\ UNCHANGED <<rev, approved, facts, cmd, crashes, blind, stops, dup>>

\* Control: without the position guard a retransmitted command takes effect a second time.
Replay ==
    /\ ~PositionGuard /\ control = "running" /\ cmd \in applied /\ ~dup /\ dup' = TRUE
    /\ UNCHANGED <<control, node, rev, epoch, approved, facts, driver, stops, badEntry, retried>>

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

\* Resume returns an interrupted Work or Review to a fresh Work of the same repair cycle.
Resume ==
    /\ control = "stopped" /\ control' = "running" /\ epoch' = epoch + 1
    /\ stage' = IF stage = "review" THEN "work" ELSE stage
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
SucceededOnlyWhenConfirmed == control = "succeeded" => facts /\ stage = "complete"

\* A running node whose boundary is approved must not rest there.
Movable ==
    /\ control \in {"prepared", "running"} /\ ~failed /\ stage # "complete"
    /\ \A g \in Boundary : Current(g)
ApprovedNodeMoves == []<>(~Movable)
=============================================================================
