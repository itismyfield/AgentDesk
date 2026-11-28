-------------------------- MODULE SessionRotationD1 --------------------------
EXTENDS Naturals, FiniteSets

CONSTANTS Ordinary, Request, Seed, Native, MaxCrashes
ASSUME /\ IsFiniteSet(Ordinary) /\ Ordinary # {}
       /\ Cardinality({Request, Seed, Native}) = 3
       /\ Ordinary \cap {Request, Seed, Native} = {} /\ MaxCrashes \in Nat

\* Fixed-SHA entry-table mapping (the design supplies full paths):
\* Q01,Q05-Q13: original custody, attempt pin, MayHaveSent/Unknown and receipts.
\* Q16-Q18: deterministic machine tickets, typed lifecycle intent and permits.
\* Q19-Q22: explicit controls retain evidence and revoke the operation revision.
\* Q02-Q04,Q23-Q26: crash/adopt, durable candidate recovery, boot before wake.
\* O rows: prepared piece -> external HTTP accept -> durable receipt; fresh debt.
\* N rows (new): reservation/body/barrier/stop/launch/seed and terminal tombstone.
\* Old/New denote two fixed native UUIDs; Live/Revoked denote one op's revisions.
\* Input categories are table rows, not inferred from untrusted source strings.

Inputs == Ordinary \cup {Request, Seed, Native}
Sessions == {"Old", "New"}
None == "None"
Terminal == {"Done", "Abandoned"}
Phases == {"Idle", "Requesting", "Staged", "Barrier", "StopIntent",
           "OldEnded", "LaunchIntent", "Bound", "SeedPending"} \cup Terminal
Risky == {"Barrier", "StopIntent", "OldEnded", "LaunchIntent", "Bound", "SeedPending"}
InputStates == {"Absent", "Queued", "Pinned", "Armed", "Unknown", "Accepted", "Cancelled"}
Blocks == {"Background", "Monitor", "Draft", "Lifecycle", "Grant"}
Debt == {"Prepared", "Unsealed", "Uncaptured", "BindingPending", "VerifiedWork",
         "Posting", "Owed", "NoticeOwed"}
Controls == {"Cancel", "Terminate", "Clear", "Stop", "EffortRestart", "Compact"}
EmptyInput == [state |-> "Absent", pin |-> "Unbound", pins |-> {}, effects |-> 0,
               delivered |-> {}, flight |-> FALSE, receipt |-> FALSE, completed |-> FALSE]

VARIABLE s
vars == <<s>>
Active == s.phase \notin Terminal \cup {"Idle"}
Unsettled == {"Queued", "Pinned", "Armed", "Unknown"}
EmptyQueue == \A i \in Ordinary \cup {Native}:
               s.input[i].state \notin Unsettled /\ ~s.input[i].flight
NoInputFlight == \A i \in Inputs: ~s.input[i].flight
NoOldWork == s.turn = "Idle" /\ s.blockers = {} /\ s.permit = None /\ NoInputFlight
CleanOutput == s.out.fresh /\ s.out.debt = {} /\ s.out.state \in {"Empty", "Posted"}
CutGate == NoOldWork /\ EmptyQueue /\ CleanOutput /\ s.capability
           /\ s.input[Request].state = "Accepted" /\ s.requestEnded /\ s.body
Current == IF s.proc.bound THEN "New" ELSE "Old"
Alive(g) == IF g = "Old" THEN s.proc.oldAlive ELSE s.proc.newAlive
Pinned(x, g) == [x EXCEPT !.state = "Pinned", !.pin = g, !.pins = @ \cup {g}]
CancelledAttempt(x) ==
  IF x.state = "Pinned" THEN [x EXCEPT !.state = "Cancelled"]
  ELSE IF x.state = "Armed" THEN [x EXCEPT !.state = "Unknown"] ELSE x
Abandoned(held) ==
  [s EXCEPT !.phase = "Abandoned", !.fence = "Revoked", !.barrier = held,
    !.abandonedEver = TRUE,
    !.capability = IF held THEN @ ELSE FALSE, !.permit = None,
    !.proc.stopPermit = None, !.proc.launchPermit = None,
    !.input[Request] = CancelledAttempt(@), !.input[Seed] = CancelledAttempt(@)]

Init == s = [phase |-> "Idle", latch |-> [g \in Sessions |-> "Fresh"],
  online |-> TRUE, incarnation |-> 0, crashes |-> 0, expired |-> FALSE,
  capability |-> FALSE, fence |-> "Live", barrier |-> FALSE,
  barrierEver |-> FALSE, abandonedEver |-> FALSE,
  turn |-> "Idle", turnInput |-> None, blockers |-> {}, control |-> None,
  input |-> [i \in Inputs |-> EmptyInput], custody |-> {}, permit |-> None,
  permitFence |-> "Live", requestPlans |-> 0, reservedEver |-> FALSE, body |-> FALSE,
  requestEnded |-> FALSE, seedAccepted |-> FALSE, newEligible |-> FALSE, nextRequestPlans |-> 0,
  proc |-> [oldAlive |-> TRUE, newAlive |-> FALSE, stopPermit |-> None,
    stopFlight |-> FALSE, stopProof |-> FALSE, launchPermit |-> None,
    launchFlight |-> FALSE, bound |-> FALSE,
    starts |-> 0, captured |-> FALSE, retired |-> FALSE],
  out |-> [state |-> "Empty", permit |-> FALSE, flight |-> FALSE,
          count |-> 0, debt |-> {}, fresh |-> TRUE],
  badCut |-> FALSE, badDrain |-> FALSE]

AcquireCapability == /\ s.online /\ s.phase \in {"Idle", "Abandoned"}
                     /\ s.latch["Old"] = "Fresh" /\ ~s.barrier /\ ~s.capability
                     /\ s' = [s EXCEPT !.capability = TRUE]
\* This is a backend exclusion lease, persistent across a dcserver crash.
\* A backend without it can reserve only the explicit unsupported outcome.
ReserveUnsupported == /\ s.online /\ s.phase \in {"Idle", "Abandoned"}
  /\ ~s.barrier /\ s.proc.oldAlive /\ ~s.proc.newAlive /\ ~s.capability
  /\ s.latch["Old"] = "Fresh"
  /\ s' = [Abandoned(FALSE) EXCEPT !.latch["Old"] = "Spent", !.reservedEver = TRUE]
Reserve == /\ s.online /\ s.phase \in {"Idle", "Abandoned"} /\ ~s.barrier
  /\ s.proc.oldAlive /\ ~s.proc.newAlive /\ s.latch["Old"] = "Fresh"
  /\ NoOldWork /\ EmptyQueue /\ CleanOutput /\ s.capability
  /\ s' = [s EXCEPT !.phase = "Requesting", !.latch["Old"] = "Spent", !.fence = "Live",
      !.reservedEver = TRUE,
      !.requestPlans = @ + 1, !.input[Request] = Pinned(@, "Old"),
      !.custody = @ \cup {Request}]

Arrive(i) == /\ s.online /\ i \in Ordinary /\ s.input[i].state = "Absent"
  /\ LET t == IF s.phase \in {"Requesting", "Staged"} THEN Abandoned(FALSE) ELSE s
     IN s' = [t EXCEPT !.input[i].state = "Queued", !.custody = @ \cup {i}]
PinOrdinary(i) == /\ s.online /\ i \in Ordinary /\ s.input[i].state = "Queued"
  /\ ~s.barrier /\ Alive(Current)
  /\ s' = [s EXCEPT !.input[i] = Pinned(@, Current)]
Arm(i) == /\ s.online /\ i \in Inputs \ {Native} /\ s.input[i].state = "Pinned"
  /\ s.permit = None /\ s.turn = "Idle" /\ NoInputFlight /\ Alive(s.input[i].pin)
  /\ (CASE i = Request -> s.phase = "Requesting" /\ s.capability /\ s.fence = "Live"
       [] i = Seed -> s.phase = "Bound" /\ s.capability /\ s.fence = "Live"
       [] OTHER -> ~s.barrier)
  /\ s' = [s EXCEPT !.input[i].state = "Armed", !.permit = i,
      !.permitFence = s.fence, !.phase = IF i = Seed THEN "SeedPending" ELSE @]
\* Last authorization, actual provider acceptance, and native receipt differ.
DispatchInput(i) == /\ s.online /\ i \in Inputs \ {Native} /\ s.permit = i
  /\ s.input[i].state = "Armed" /\ Alive(s.input[i].pin)
  /\ (IF i \in {Request, Seed}
        THEN s.fence = "Live" /\ s.permitFence = s.fence /\ s.capability
             /\ s.phase = (IF i = Request THEN "Requesting" ELSE "SeedPending")
        ELSE ~s.barrier)
  /\ s' = [s EXCEPT !.input[i].flight = TRUE, !.permit = None]
\* A dispatched write may arrive after crash/control; its original pin stays.
InputAccept(i) == /\ i \in Inputs \ {Native} /\ s.input[i].flight
  /\ Alive(s.input[i].pin)
  /\ s' = [s EXCEPT !.input[i].effects = @ + 1, !.input[i].flight = FALSE,
      !.input[i].delivered = @ \cup {s.input[i].pin},
      !.turn = "Open", !.turnInput = i,
      !.out.fresh = IF s.input[i].pin = "Old" THEN FALSE ELSE @]
NativeReceipt(i) == /\ i \in Inputs /\ s.input[i].effects = 1 /\ ~s.input[i].receipt
  /\ s' = [s EXCEPT !.input[i].receipt = TRUE]
SettleInput(i) == /\ s.online /\ i \in Inputs /\ s.input[i].receipt
  /\ s.input[i].state \in {"Armed", "Unknown"}
  /\ LET done == i = Seed /\ s.phase = "SeedPending" /\ s.fence = "Live"
     IN s' = [s EXCEPT !.input[i].state = "Accepted",
       !.seedAccepted = IF done THEN TRUE ELSE @,
       !.phase = IF done THEN "Done" ELSE @,
       !.barrier = IF done THEN FALSE ELSE @,
       !.capability = IF done THEN FALSE ELSE @]
LoseInputAck(i) == /\ s.online /\ i \in Inputs /\ s.input[i].state = "Armed"
  /\ ~s.input[i].receipt
  /\ s' = [s EXCEPT !.input[i].state = "Unknown",
      !.permit = IF @ = i THEN None ELSE @]
EndTurn == /\ s.turn = "Open"
  /\ LET i == s.turnInput IN s' = [s EXCEPT !.turn = "Idle", !.turnInput = None,
      !.input[i].completed = TRUE,
      !.requestEnded = IF i = Request THEN TRUE ELSE @,
      !.newEligible = @ \/ (i \in Ordinary /\ s.input[i].pin = "New"),
      !.out.fresh = IF s.input[i].pin = "Old" THEN FALSE ELSE @]
\* Stage models only exact op/source/request-nonce capability matches; every
\* other API tuple is rejected without a state change. No process effect here.
Stage == /\ s.online /\ s.phase = "Requesting" /\ s.fence = "Live"
  /\ s.turn = "Open" /\ s.turnInput = Request /\ s.input[Request].pin = "Old"
  /\ s.input[Request].effects = 1
  /\ s' = [s EXCEPT !.phase = "Staged", !.body = TRUE]
MissingBody == /\ s.online /\ s.phase = "Requesting" /\ s.requestEnded /\ ~s.body
               /\ s' = Abandoned(FALSE)

\* The durable barrier commit is the cutover linearization point. Subsequent
\* arrivals remain Unbound; they are never reissued copies of old attempts.
Barrier == /\ s.online /\ s.phase = "Staged" /\ CutGate
           /\ s' = [s EXCEPT !.phase = "Barrier", !.barrier = TRUE, !.barrierEver = TRUE]
StopIntent == /\ s.online /\ s.phase = "Barrier" /\ NoOldWork /\ CleanOutput
  /\ s.capability /\ s.fence = "Live"
  /\ s' = [s EXCEPT !.phase = "StopIntent", !.proc.stopPermit = s.fence]
\* The last revision check and irreversible external call dispatch are atomic.
\* Its already-started completion can arrive after a crash or revocation.
DispatchStop == /\ s.online /\ s.phase = "StopIntent" /\ s.fence = "Live"
  /\ s.proc.stopPermit = s.fence /\ s.capability /\ s.proc.oldAlive
  /\ NoOldWork /\ CleanOutput
  /\ s' = [s EXCEPT !.proc.stopFlight = TRUE, !.proc.stopPermit = None,
      !.badDrain = @ \/ (s.out.debt # {}) \/ ~s.out.fresh
          \/ (s.out.state \notin {"Empty", "Posted"})]
StopCompletes == /\ s.proc.stopFlight
  /\ s' = [s EXCEPT !.proc.oldAlive = FALSE, !.proc.stopFlight = FALSE,
      !.badCut = @ \/ (s.turn # "Idle") \/ (s.blockers # {}),
      !.out.fresh = FALSE, !.out.debt = @ \cup {"Uncaptured"}]
ProveStopped == /\ s.online /\ s.phase = "StopIntent" /\ ~s.proc.oldAlive
  /\ s' = [s EXCEPT !.phase = "OldEnded", !.proc.stopProof = TRUE]
\* Exit tail is additional byte debt, not a second representative HTTP piece.
LaunchIntent == /\ s.online /\ s.phase = "OldEnded" /\ s.proc.stopProof /\ CleanOutput
  /\ s' = [s EXCEPT !.phase = "LaunchIntent", !.proc.launchPermit = s.fence,
      !.badDrain = @ \/ (s.out.debt # {}) \/ ~s.out.fresh
          \/ (s.out.state \notin {"Empty", "Posted"})]
DispatchLaunch == /\ s.online /\ s.phase = "LaunchIntent" /\ s.fence = "Live"
  /\ s.proc.launchPermit = s.fence /\ s.capability /\ s.proc.stopProof
  /\ ~s.proc.oldAlive /\ ~s.proc.newAlive
  /\ s' = [s EXCEPT !.proc.launchFlight = TRUE, !.proc.launchPermit = None]
LaunchCompletes == /\ s.proc.launchFlight
  /\ s' = [s EXCEPT !.proc.newAlive = TRUE, !.proc.launchFlight = FALSE,
      !.proc.starts = @ + 1]
BindSuccessor == /\ s.online /\ s.phase = "LaunchIntent" /\ s.proc.newAlive
  /\ s.proc.stopProof /\ s.fence = "Live"
  /\ s' = [s EXCEPT !.phase = "Bound", !.proc.bound = TRUE,
      !.input[Seed] = Pinned(@, "New"), !.custody = @ \cup {Seed}]
CaptureSuccessor == /\ s.proc.bound /\ s.input[Seed].effects = 1 /\ ~s.proc.captured
  /\ s' = [s EXCEPT !.proc.captured = TRUE]
RetireOld == /\ s.online /\ s.proc.stopProof /\ s.proc.captured /\ CleanOutput
  /\ ~s.proc.retired /\ s' = [s EXCEPT !.proc.retired = TRUE]

\* One representative request-or-seed output piece. count is an external-world
\* counter, never inferred from the application's durable Posted state.
PrepareOutput == /\ s.online /\ s.turn = "Open" /\ s.turnInput \in {Request, Seed}
  /\ s.out.state = "Empty"
  /\ s' = [s EXCEPT !.out.state = "Prepared", !.out.fresh = FALSE, !.out.debt = Debt]
ArmPost == /\ s.online /\ s.out.state = "Prepared"
  /\ s' = [s EXCEPT !.out.state = "MayHaveSent", !.out.permit = TRUE]
SendHttp == /\ s.online /\ s.out.state = "MayHaveSent" /\ s.out.permit
  /\ s' = [s EXCEPT !.out.flight = TRUE, !.out.permit = FALSE]
\* Already-dispatched HTTP may accept after dcserver crashed or gave up.
HttpAccept == /\ s.out.flight
  /\ s' = [s EXCEPT !.out.count = @ + 1, !.out.flight = FALSE]
HttpReject == /\ s.out.flight /\ s' = [s EXCEPT !.out.flight = FALSE]
PostReceipt == /\ s.online /\ s.out.state = "MayHaveSent" /\ s.out.count = 1
  /\ s' = [s EXCEPT !.out.state = "Posted", !.out.permit = FALSE,
      !.out.debt = @ \ {"Prepared", "Posting"}]
\* This action denotes positive external no-send evidence, not a guess from
\* a missing ACK. None of the three reconciliation outcomes has fairness.
PostProvedNotSent == /\ s.online /\ s.out.state = "MayHaveSent"
  /\ ~s.out.permit /\ ~s.out.flight /\ s.out.count = 0
  /\ s' = [s EXCEPT !.out.state = "Prepared"]
PostUnknown == /\ s.online /\ s.out.state = "MayHaveSent" /\ ~s.out.permit
  /\ s' = [s EXCEPT !.out.state = "Held"]
\* Independent discharge makes owed=0 with unsealed>0 (and each singleton debt)
\* reachable. A zero of one component never substitutes for the conjunction.
DrainDebt(d) == /\ s.online /\ s.turn = "Idle" /\ s.out.state \in {"Empty", "Posted"}
  /\ d \in s.out.debt /\ s' = [s EXCEPT !.out.debt = @ \ {d}, !.out.fresh = FALSE]
RefreshOutput == /\ s.online /\ s.turn = "Idle" /\ s.out.state \in {"Empty", "Posted"}
  /\ s.out.debt = {} /\ ~s.out.fresh
  /\ s' = [s EXCEPT !.out.fresh = TRUE]

LoseCapability == /\ s.online /\ s.capability
  /\ s.phase \in {"Idle", "Requesting", "Staged"}
  /\ s' = IF s.phase = "Idle" THEN [s EXCEPT !.capability = FALSE] ELSE Abandoned(FALSE)
NativeInput == /\ ~s.capability /\ ~s.barrier /\ Alive(Current)
  /\ s.turn = "Idle" /\ s.permit = None /\ NoInputFlight
  /\ s.input[Native].state = "Absent"
  /\ s' = [s EXCEPT !.input[Native] = [Pinned(@, Current) EXCEPT
       !.state = "Accepted", !.effects = 1, !.delivered = {Current}, !.receipt = TRUE],
      !.custody = @ \cup {Native}, !.turn = "Open", !.turnInput = Native,
      !.out.fresh = IF Current = "Old" THEN FALSE ELSE @]
AddBlocker(b) == /\ b \in Blocks /\ b \notin s.blockers /\ s.online
  /\ s.phase \in {"Idle", "Requesting", "Staged", "Barrier", "StopIntent"}
  /\ ~s.proc.stopFlight
  /\ LET t == IF Active THEN Abandoned(s.phase \in Risky) ELSE s
     IN s' = [t EXCEPT !.blockers = @ \cup {b}]
RemoveBlocker(b) == /\ b \in s.blockers /\ s' = [s EXCEPT !.blockers = @ \ {b}]
UserControl(c) == /\ s.online /\ Active /\ c \in Controls
  /\ LET t == Abandoned(s.phase \in Risky)
     IN s' = [t EXCEPT !.control = c,
      !.turn = IF c = "Compact" THEN @ ELSE "Idle",
      !.turnInput = IF c = "Compact" THEN @ ELSE None,
      !.input = [i \in Inputs |->
        IF c = "Clear" /\ i \in Ordinary
           /\ t.input[i].state \in {"Queued", "Pinned"}
        THEN [t.input[i] EXCEPT !.state = "Cancelled"] ELSE t.input[i]]]
CancelInput(i) == /\ s.online /\ i \in Ordinary /\ i \in s.custody
  /\ s.input[i].state = "Queued"
  /\ s' = [s EXCEPT !.input[i].state = "Cancelled",
      !.permit = IF @ = i THEN None ELSE @]
\* Boundary observation only: no third generation's execution is expanded.
ReserveNext == /\ s.online /\ s.phase = "Done" /\ s.newEligible
  /\ s.latch["New"] = "Fresh" /\ NoOldWork /\ EmptyQueue /\ CleanOutput
  /\ s' = [s EXCEPT !.nextRequestPlans = @ + 1, !.latch["New"] = "Spent"]

Crash == /\ s.online /\ s.crashes < MaxCrashes
  /\ s' = [s EXCEPT !.online = FALSE, !.crashes = @ + 1, !.permit = None,
      !.input = [i \in Inputs |-> IF s.input[i].state = "Armed"
         THEN [s.input[i] EXCEPT !.state = "Unknown"] ELSE s.input[i]],
      !.proc.stopPermit = None, !.proc.launchPermit = None,
      !.out.permit = FALSE, !.out.fresh = FALSE]
Boot == /\ ~s.online
        /\ s' = [s EXCEPT !.online = TRUE, !.incarnation = @ + 1]
\* An absolute persisted deadline: neither boot nor any progress postpones it.
Tick == /\ Active /\ ~s.expired /\ s' = [s EXCEPT !.expired = TRUE]
Expire == /\ s.online /\ Active /\ s.expired
          /\ s' = Abandoned(s.phase \in Risky)

Next == AcquireCapability \/ ReserveUnsupported \/ Reserve \/ Stage \/ MissingBody
  \/ Barrier \/ StopIntent \/ DispatchStop \/ StopCompletes \/ ProveStopped
  \/ LaunchIntent \/ DispatchLaunch \/ LaunchCompletes \/ BindSuccessor
  \/ CaptureSuccessor \/ RetireOld \/ PrepareOutput \/ ArmPost \/ SendHttp \/ HttpAccept \/ HttpReject
  \/ PostReceipt \/ PostProvedNotSent \/ PostUnknown \/ RefreshOutput \/ ReserveNext
  \/ LoseCapability \/ NativeInput \/ EndTurn \/ Crash \/ Boot \/ Tick \/ Expire
  \/ (\E i \in Ordinary: Arrive(i) \/ PinOrdinary(i) \/ CancelInput(i))
  \/ (\E i \in Inputs: Arm(i) \/ DispatchInput(i) \/ InputAccept(i)
                           \/ NativeReceipt(i) \/ SettleInput(i) \/ LoseInputAck(i))
  \/ (\E d \in Debt: DrainDebt(d))
  \/ (\E b \in Blocks: AddBlocker(b) \/ RemoveBlocker(b))
  \/ (\E c \in Controls: UserControl(c))
Spec == Init /\ [][Next]_vars /\ WF_vars(Boot) /\ WF_vars(Tick) /\ WF_vars(Expire)

TypeOK == /\ s.phase \in Phases /\ s.latch \in [Sessions -> {"Fresh", "Spent"}]
  /\ s.online \in BOOLEAN /\ s.incarnation \in 0..MaxCrashes /\ s.crashes \in 0..MaxCrashes
  /\ s.expired \in BOOLEAN /\ s.capability \in BOOLEAN /\ s.barrier \in BOOLEAN
  /\ s.barrierEver \in BOOLEAN /\ s.abandonedEver \in BOOLEAN /\ s.badDrain \in BOOLEAN
  /\ s.fence \in {"Live", "Revoked"} /\ s.permitFence \in {"Live", "Revoked"}
  /\ s.turn \in {"Idle", "Open"} /\ s.turnInput \in Inputs \cup {None}
  /\ s.blockers \subseteq Blocks /\ s.control \in Controls \cup {None}
  /\ s.custody \subseteq Inputs /\ s.permit \in Inputs \cup {None}
  /\ s.requestPlans \in 0..2 /\ s.reservedEver \in BOOLEAN /\ s.body \in BOOLEAN /\ s.requestEnded \in BOOLEAN
  /\ s.seedAccepted \in BOOLEAN /\ s.newEligible \in BOOLEAN /\ s.badCut \in BOOLEAN
  /\ s.nextRequestPlans \in 0..2
  /\ s.input \in [Inputs -> [state : InputStates, pin : Sessions \cup {"Unbound"},
      pins : SUBSET Sessions, effects : 0..2, delivered : SUBSET Sessions,
      flight : BOOLEAN, receipt : BOOLEAN, completed : BOOLEAN]]
  /\ s.proc \in [oldAlive : BOOLEAN, newAlive : BOOLEAN, stopPermit : {None, "Live", "Revoked"},
      stopFlight : BOOLEAN, stopProof : BOOLEAN, launchPermit : {None, "Live", "Revoked"},
      launchFlight : BOOLEAN, bound : BOOLEAN,
      starts : 0..2, captured : BOOLEAN, retired : BOOLEAN]
  /\ s.out \in [state : {"Empty", "Prepared", "MayHaveSent", "Posted", "Held"},
      permit : BOOLEAN, flight : BOOLEAN, count : 0..2, debt : SUBSET Debt, fresh : BOOLEAN]
NoTurnCut == ~s.badCut
NoUnflushedCut == ~s.badDrain
NoDuplicatePost == s.out.count <= 1
NoRepeatedRequest == s.requestPlans <= 1 /\ s.input[Request].effects <= 1 /\ s.nextRequestPlans <= 1
LatchDurable == /\ (s.reservedEver => s.latch["Old"] = "Spent")
                /\ (s.nextRequestPlans > 0 => s.latch["New"] = "Spent")
OperationClosed == /\ (s.abandonedEver => s.phase = "Abandoned")
  /\ ((s.phase = "Abandoned" /\ s.barrierEver) => s.barrier /\ s.capability)
InputExactlyOneSession == \A i \in Inputs:
  /\ Cardinality(s.input[i].pins) <= 1 /\ s.input[i].effects <= 1
  /\ (s.input[i].state \in {"Pinned", "Armed", "Unknown", "Accepted"}
       => s.input[i].pin \in Sessions /\ s.input[i].pins = {s.input[i].pin})
  /\ s.input[i].delivered \subseteq s.input[i].pins
  /\ (s.input[i].effects = 1 => s.input[i].delivered = {s.input[i].pin})
  /\ (s.input[i].state = "Accepted" => s.input[i].effects = 1)
NoLostCustody == /\ \A i \in s.custody: s.input[i].state # "Absent"
                /\ \A i \in Inputs: s.input[i].state # "Absent" => i \in s.custody
SeedFirst == \A i \in Ordinary \cup {Native}: "New" \in s.input[i].delivered => s.seedAccepted
NoAutonomousCascade == /\ (s.nextRequestPlans > 0 => s.newEligible)
  /\ (s.newEligible => \E i \in Ordinary: s.input[i].completed /\ s.input[i].pin = "New")
LifecycleOrder == /\ (s.proc.newAlive => s.proc.stopProof /\ ~s.proc.oldAlive)
                  /\ s.proc.starts <= 1
                  /\ (s.proc.retired => s.proc.captured /\ s.proc.stopProof)
ReservationResolved == (s.latch["Old"] = "Spent") ~> (s.phase \in Terminal)
=============================================================================
