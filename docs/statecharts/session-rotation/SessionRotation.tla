--------------------------- MODULE SessionRotation ---------------------------
\* Design-2 transcript-size rotation of one Claude channel: Old -> New once.
\* Each Fix* constant switches one design-2 repair on; all FALSE is design-1 behaviour.
EXTENDS Naturals, FiniteSets

CONSTANTS Ordinary, Req, Seed, Direct, Fire, MaxCrashes, MaxKills,
          FixRelease,   \* every post-barrier abort releases the barrier by phase
          FixLatch,     \* the latch is spent when the request is dispatched, not reserved
          FixRecreate,  \* reserved origins refuse the warm follow-up recreate branch
          FixDirect,    \* /tui/send and slash passthrough refuse while the barrier holds
          FixSched,     \* a pending CLI schedule makes the generation ineligible
          FixOwner      \* only an O-owned, Undelivered-published channel is eligible

ASSUME Ordinary # {} /\ Ordinary \cap {Req, Seed, Direct, Fire} = {}
       /\ Cardinality({Req, Seed, Direct, Fire}) = 4 /\ {MaxCrashes, MaxKills} \subseteq Nat

Inputs == Ordinary \cup {Req, Seed, Direct, Fire}
Gens == {"Old", "New", "Other"}
None == "None"
PreBarrier == {"Requesting", "Asked", "Staged"}
PostBarrier == {"Barrier", "Stopping", "OldEnded", "Launching", "Bound", "SeedPending"}
Active == PreBarrier \cup PostBarrier
Settled == {"Idle", "Released", "Done", "Held"}
States == {"Absent", "Queued", "Sent", "Accepted", "Unknown", "Failed", "Cancelled"}

VARIABLES phase, latch, sel, alive, barrier, abort, expired, online, crashes, kills,
          turn, turnBy, turnGen, in, body, reqEnded, foreign, blocker,
          sched, schedLost, owned, outDebt, legacyDebt,
          stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain
vars == <<phase, latch, sel, alive, barrier, abort, expired, online, crashes, kills,
          turn, turnBy, turnGen, in, body, reqEnded, foreign, blocker,
          sched, schedLost, owned, outDebt, legacyDebt,
          stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>

Rec0 == [st |-> "Absent", pin |-> None, dlv |-> {}, n |-> 0]
NoFlight == \A i \in Inputs: in[i].st # "Sent"
QueuedOrd == \E i \in Ordinary: in[i].st \in {"Queued", "Sent"}
\* The live execution behind the channel's pane, which bypass inputs reach.
Pane == IF alive["New"] THEN "New" ELSE IF alive["Old"] THEN "Old" ELSE "Other"
Eligible == /\ (FixOwner => owned) /\ (FixSched => ~sched)
OldIdle == /\ turn = "Idle" /\ ~blocker /\ ~outDebt /\ NoFlight /\ alive["Old"] /\ sel = "Old"
\* Before the barrier the whole queue must be empty; after it, arrivals wait behind it.
OldQuiet == OldIdle /\ ~QueuedOrd

Init == /\ phase = "Idle" /\ latch = [g \in Gens |-> "Fresh"] /\ sel = "Old"
        /\ alive = [g \in Gens |-> g = "Old"] /\ barrier = FALSE /\ abort = FALSE
        /\ expired = FALSE /\ online = TRUE /\ crashes = 0 /\ kills = 0
        /\ turn = "Idle" /\ turnBy = None /\ turnGen = None
        /\ in = [i \in Inputs |-> Rec0] /\ body = FALSE /\ reqEnded = FALSE
        /\ foreign = FALSE /\ blocker = FALSE /\ sched \in BOOLEAN /\ schedLost = FALSE
        /\ owned \in BOOLEAN /\ outDebt = FALSE /\ legacyDebt = FALSE
        /\ stopFlight = FALSE /\ launchFlight = FALSE /\ seedOk = FALSE
        /\ declared = FALSE /\ notices = 0 /\ badCut = FALSE /\ badDrain = FALSE

\* A turn opened on Old after the request went out means the handoff may be stale.
OpenTurn(i, g) == /\ turn' = "Open" /\ turnBy' = i /\ turnGen' = g
                  /\ foreign' = (foreign \/ (g = "Old" /\ i # Req
                                  /\ phase \in {"Asked", "Staged"} \cup PostBarrier))

\* ---------------- ordinary custody (Owner queue V2) ----------------
Arrive(i) ==
  /\ i \in Ordinary /\ in[i].st = "Absent"
  /\ in' = [in EXCEPT ![i].st = "Queued"]
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes,
       turn, turnBy, turnGen, body, reqEnded, foreign, blocker, sched, schedLost, owned,
       outDebt, legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
Pin(i) ==
  /\ online /\ i \in Ordinary /\ in[i].st = "Queued" /\ ~barrier
  /\ phase \notin PostBarrier /\ alive[sel] /\ turn = "Idle" /\ NoFlight
  /\ in' = [in EXCEPT ![i].st = "Sent", ![i].pin = sel, ![i].n = @ + 1]
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes,
       turn, turnBy, turnGen, body, reqEnded, foreign, blocker, sched, schedLost, owned,
       outDebt, legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* A dispatched write resolves later, also across a crash: accepted with receipt, landed
\* with the receipt lost, not landed with no proof, or positively not sent.
Deliver(i) ==
  /\ in[i].st = "Sent"
  /\ LET g == in[i].pin IN
     \/ /\ alive[g] /\ turn = "Idle"
        /\ \E ack \in {"Accepted", "Unknown"}:
             in' = [in EXCEPT ![i].st = ack, ![i].dlv = @ \cup {g}]
        /\ OpenTurn(i, g)
        /\ UNCHANGED <<kills, sel, alive>>
     \/ /\ in' = [in EXCEPT ![i].st = IF alive[g] THEN "Failed" ELSE "Unknown"]
        /\ UNCHANGED <<kills, turn, turnBy, turnGen, foreign, sel, alive>>
     \* The warm follow-up's stranded-draft branch retires the pane and submits to a
     \* fresh UUID. Ordinary input keeps that behaviour; with FixRecreate the reserved
     \* origins take the positive not-sent branch above instead.
     \/ /\ alive[g] /\ turn = "Idle" /\ g # "Other" /\ ~alive["Other"]
        /\ (i \in {Req, Seed} => ~FixRecreate)
        /\ in' = [in EXCEPT ![i].st = "Accepted", ![i].pin = "Other", ![i].dlv = @ \cup {"Other"}]
        /\ alive' = [alive EXCEPT ![g] = FALSE, !["Other"] = TRUE] /\ sel' = "Other"
        /\ OpenTurn(i, "Other")
  /\ UNCHANGED <<kills, phase, latch, barrier, abort, expired, online, crashes, body, reqEnded,
       blocker, sched, schedLost, owned, outDebt, legacyDebt, stopFlight, launchFlight,
       seedOk, declared, notices, badCut, badDrain>>
EndTurn ==
  /\ turn = "Open"
  /\ turn' = "Idle" /\ turnBy' = None /\ turnGen' = None
  /\ reqEnded' = (reqEnded \/ turnBy = Req)
  /\ outDebt' = (outDebt \/ (turnGen = "Old" /\ owned))
  /\ legacyDebt' = (legacyDebt \/ (turnGen = "Old" /\ ~owned))
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes, in,
       body, foreign, blocker, sched, schedLost, owned, stopFlight, launchFlight, seedOk,
       declared, notices, badCut, badDrain>>
\* O's Undelivered drains its own debt; the Legacy watcher drains while its pane lives.
DrainO ==
  /\ outDebt /\ turn = "Idle" /\ outDebt' = FALSE
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes, turn,
       turnBy, turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
DrainLegacy ==
  /\ legacyDebt /\ alive["Old"] /\ turn = "Idle" /\ legacyDebt' = FALSE
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes, turn,
       turnBy, turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned,
       outDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* The existing cold path resumes the selected UUID when its execution is gone.
ColdLaunch ==
  /\ online /\ ~barrier /\ phase \in Settled /\ ~alive[sel] /\ QueuedOrd
  /\ \A g \in Gens: ~alive[g]
  /\ alive' = [alive EXCEPT ![sel] = TRUE]
  /\ UNCHANGED <<kills, phase, latch, sel, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>

\* ---------------- inputs that bypass the Owner ----------------
\* /tui/send and slash passthrough type into the pane under the composer lock only.
DirectInput ==
  /\ online /\ in[Direct].st = "Absent" /\ alive[Pane] /\ turn = "Idle"
  /\ NoFlight /\ (FixDirect => ~barrier)
  /\ in' = [in EXCEPT ![Direct].st = "Accepted", ![Direct].pin = Pane,
                      ![Direct].dlv = {Pane}, ![Direct].n = 1]
  /\ OpenTurn(Direct, Pane)
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes, body,
       reqEnded, blocker, sched, schedLost, owned, outDebt, legacyDebt, stopFlight,
       launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* A ScheduleWakeup, /loop or CronCreate job fires inside the Old CLI when its REPL is idle.
FireSchedule ==
  /\ sched /\ alive["Old"] /\ turn = "Idle" /\ in[Fire].st = "Absent"
  /\ in' = [in EXCEPT ![Fire].st = "Accepted", ![Fire].pin = "Old",
                      ![Fire].dlv = {"Old"}, ![Fire].n = 1]
  /\ OpenTurn(Fire, "Old")
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes, body,
       reqEnded, blocker, sched, schedLost, owned, outDebt, legacyDebt, stopFlight,
       launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* The agent creates or deletes a schedule, or starts a background child, in its own turn.
ToggleSchedule ==
  /\ turn = "Open" /\ turnGen = "Old" /\ sched' = ~sched
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes, turn,
       turnBy, turnGen, in, body, reqEnded, foreign, blocker, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
AddBlocker ==
  /\ turn = "Open" /\ turnGen = "Old" /\ ~blocker /\ blocker' = TRUE
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes, turn,
       turnBy, turnGen, in, body, reqEnded, foreign, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
RemoveBlocker ==
  /\ blocker /\ blocker' = FALSE
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, online, crashes, turn,
       turnBy, turnGen, in, body, reqEnded, foreign, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>

\* ---------------- paths outside the rotation that end or replace a session ----------------
\* kill-tmux, force-kill, idle-kill, Terminate and a lost execution lease end one execution.
ExternalKill(g) ==
  /\ alive[g] /\ kills < MaxKills /\ kills' = kills + 1
  /\ alive' = [alive EXCEPT ![g] = FALSE]
  /\ IF turnGen = g THEN turn' = "Idle" /\ turnBy' = None /\ turnGen' = None
                    ELSE UNCHANGED <<turn, turnBy, turnGen>>
  /\ UNCHANGED <<phase, latch, sel, barrier, abort, expired, online, crashes, in, body,
       reqEnded, foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt, stopFlight,
       launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* An in-pane /clear or the prompt-too-long fresh session moves Old to a third UUID.
ExternalReplace ==
  /\ alive["Old"] /\ sel = "Old" /\ ~alive["Other"] /\ ~stopFlight
  /\ kills < MaxKills /\ kills' = kills + 1
  /\ alive' = [alive EXCEPT !["Old"] = FALSE, !["Other"] = TRUE] /\ sel' = "Other"
  /\ turn' = "Idle" /\ turnBy' = None /\ turnGen' = None
  /\ UNCHANGED <<phase, latch, barrier, abort, expired, online, crashes, in, body, reqEnded,
       foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt, stopFlight,
       launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* Cancel, stop, clear, Terminate, effort restart, resume: the command revokes the rotation
\* and then runs its own meaning; clear also drops waiting work.
UserControl(clear) ==
  /\ online /\ phase \in Active /\ ~abort
  /\ abort' = TRUE
  /\ turn' = "Idle" /\ turnBy' = None /\ turnGen' = None
  /\ in' = [i \in Inputs |-> IF clear /\ i \in Ordinary /\ in[i].st = "Queued"
                             THEN [in[i] EXCEPT !.st = "Cancelled"] ELSE in[i]]
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, expired, online, crashes, body, reqEnded,
       foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt, stopFlight,
       launchFlight, seedOk, declared, notices, badCut, badDrain>>

\* ---------------- rotation ----------------
Reserve ==
  /\ online /\ phase = "Idle" /\ latch["Old"] = "Fresh" /\ Eligible /\ OldQuiet
  /\ phase' = "Requesting" /\ in' = [in EXCEPT ![Req].st = "Queued", ![Req].pin = "Old"]
  /\ latch' = IF FixLatch THEN latch ELSE [latch EXCEPT !["Old"] = "Spent"]
  /\ UNCHANGED <<kills, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy, turnGen,
       body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt,
       stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* Design-1 spent the latch on an ineligible backend too; design-2 leaves it Fresh.
ReserveUnsupported ==
  /\ ~FixLatch /\ online /\ phase = "Idle" /\ latch["Old"] = "Fresh"
  /\ ~Eligible /\ OldQuiet /\ latch' = [latch EXCEPT !["Old"] = "Spent"]
  /\ UNCHANGED <<kills, phase, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* The durable MayHaveSent commit spends the latch before the key goes out.
AskDispatch ==
  /\ online /\ phase = "Requesting" /\ ~abort /\ ~expired /\ OldQuiet
  /\ Eligible /\ in[Req].st = "Queued"
  /\ in' = [in EXCEPT ![Req].st = "Sent", ![Req].n = @ + 1]
  /\ latch' = [latch EXCEPT !["Old"] = "Spent"] /\ phase' = "Asked"
  /\ UNCHANGED <<kills, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy, turnGen,
       body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt,
       stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* The handoff API stores the body under the exact request ticket; no process effect.
Stage ==
  /\ online /\ phase = "Asked" /\ turn = "Open" /\ turnBy = Req /\ turnGen = "Old"
  /\ phase' = "Staged" /\ body' = TRUE
  /\ UNCHANGED <<kills, latch, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, in, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt,
       stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* The absolute deadline covers only the phases before the barrier.
Tick ==
  /\ phase \in PreBarrier /\ ~expired /\ expired' = TRUE
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, online, crashes, turn, turnBy,
       turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>

PreAbort == \/ abort \/ expired \/ foreign \/ QueuedOrd \/ sel # "Old" \/ ~alive["Old"]
            \/ in[Req].st = "Failed" \/ (phase = "Asked" /\ reqEnded /\ ~body)
            \/ (FixSched /\ sched)
\* Before the barrier nothing was stopped: the rotation ends and Old keeps working.
AbortPre ==
  /\ online /\ phase \in PreBarrier /\ PreAbort
  /\ phase' = IF in[Req].n > 0 THEN "Released" ELSE "Idle"
  /\ in' = [in EXCEPT ![Req] = IF @.st = "Queued" THEN Rec0 ELSE @]
  /\ abort' = FALSE /\ expired' = FALSE /\ body' = FALSE /\ reqEnded' = FALSE
  /\ foreign' = FALSE
  /\ UNCHANGED <<kills, latch, sel, alive, barrier, online, crashes, turn, turnBy, turnGen, blocker,
       sched, schedLost, owned, outDebt, legacyDebt, stopFlight, launchFlight, seedOk,
       declared, notices, badCut, badDrain>>

\* The barrier commit is the cutover linearization point for managed input.
Barrier ==
  /\ online /\ phase = "Staged" /\ reqEnded /\ ~PreAbort /\ OldQuiet
  /\ phase' = "Barrier" /\ barrier' = TRUE
  /\ UNCHANGED <<kills, latch, sel, alive, abort, expired, online, crashes, turn, turnBy, turnGen,
       in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt,
       stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* A durable dispatch record is written before /exit, so a crash can only leave "maybe sent".
StopDispatch ==
  /\ online /\ phase = "Barrier" /\ ~abort /\ ~foreign /\ OldIdle
  /\ (FixSched => ~sched)
  /\ phase' = "Stopping" /\ stopFlight' = TRUE
  /\ UNCHANGED <<kills, latch, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* /exit either ends Old within EXIT_WAIT or leaves it running; it is never a kill.
StopResolve ==
  /\ stopFlight /\ stopFlight' = FALSE
  /\ \/ /\ alive["Old"] /\ alive' = [alive EXCEPT !["Old"] = FALSE]
        /\ badCut' = (badCut \/ (turn = "Open" /\ turnGen = "Old"))
        /\ badDrain' = (badDrain \/ legacyDebt \/ outDebt)
        /\ schedLost' = (schedLost \/ sched)
        /\ IF turnGen = "Old" THEN turn' = "Idle" /\ turnBy' = None /\ turnGen' = None
                              ELSE UNCHANGED <<kills, turn, turnBy, turnGen>>
     \/ UNCHANGED <<kills, alive, badCut, badDrain, schedLost, turn, turnBy, turnGen>>
  /\ UNCHANGED <<kills, phase, latch, sel, barrier, abort, expired, online, crashes, in, body,
       reqEnded, foreign, blocker, sched, owned, outDebt, legacyDebt, launchFlight, seedOk,
       declared, notices>>
ProveStopped ==
  /\ online /\ phase = "Stopping" /\ ~stopFlight /\ ~alive["Old"] /\ ~abort /\ sel = "Old"
  /\ phase' = "OldEnded"
  /\ UNCHANGED <<kills, latch, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
LaunchDispatch ==
  /\ online /\ phase = "OldEnded" /\ ~abort /\ \A g \in Gens: ~alive[g]
  /\ phase' = "Launching" /\ launchFlight' = TRUE
  /\ UNCHANGED <<kills, latch, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, seedOk, declared, notices, badCut, badDrain>>
\* FreshExact: the stored UUID starts or the launch fails; no other UUID substitutes.
LaunchResolve ==
  /\ launchFlight /\ launchFlight' = FALSE
  /\ \E ok \in BOOLEAN: alive' = [alive EXCEPT !["New"] = ok]
  /\ UNCHANGED <<kills, phase, latch, sel, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, seedOk, declared, notices, badCut, badDrain>>
Bind ==
  /\ online /\ phase = "Launching" /\ ~launchFlight /\ alive["New"] /\ ~abort
  /\ phase' = "Bound" /\ in' = [in EXCEPT ![Seed].st = "Queued", ![Seed].pin = "New"]
  /\ UNCHANGED <<kills, latch, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
SeedDispatch ==
  /\ online /\ phase = "Bound" /\ ~abort /\ alive["New"] /\ turn = "Idle"
  /\ NoFlight /\ in[Seed].st = "Queued"
  /\ phase' = "SeedPending" /\ in' = [in EXCEPT ![Seed].st = "Sent", ![Seed].n = @ + 1]
  /\ UNCHANGED <<kills, latch, sel, alive, barrier, abort, expired, online, crashes, turn, turnBy,
       turnGen, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
SeedDone ==
  /\ online /\ phase = "SeedPending" /\ in[Seed].st = "Accepted"
  /\ phase' = "Done" /\ seedOk' = TRUE /\ barrier' = FALSE /\ sel' = "New" /\ abort' = FALSE
  /\ UNCHANGED <<kills, latch, alive, expired, online, crashes, turn, turnBy, turnGen, in, body,
       reqEnded, foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt, stopFlight,
       launchFlight, declared, notices, badCut, badDrain>>

\* Each post-barrier failure has one automatic exit once its in-flight effect resolved.
\* Design-1 instead kept the barrier (Held) until an undefined explicit recovery.
PostAbort ==
  \/ phase = "Barrier" /\ (abort \/ foreign \/ ~alive["Old"] \/ sel # "Old" \/ blocker
                           \/ (FixSched /\ sched))
  \/ phase = "Stopping" /\ ~stopFlight /\ (alive["Old"] \/ abort \/ sel # "Old")
  \/ phase = "OldEnded" /\ (abort \/ sel # "Old")
  \/ phase = "Launching" /\ ~launchFlight /\ (abort \/ ~alive["New"] \/ sel # "Old")
  \/ phase = "Bound" /\ (abort \/ ~alive["New"])
  \/ phase = "SeedPending" /\ in[Seed].st \in {"Unknown", "Failed"}
\* Old stays selected until New ran; after that New is selected and one notice says the
\* handoff did not arrive. The cold path then resumes the selected UUID.
ReleaseTo == IF alive["New"] \/ phase \in {"Bound", "SeedPending"} THEN "New"
             ELSE IF sel = "Other" THEN "Other" ELSE "Old"
Release ==
  /\ online /\ phase \in PostBarrier /\ PostAbort
  /\ IF FixRelease
     THEN /\ phase' = "Released" /\ barrier' = FALSE /\ sel' = ReleaseTo
          /\ declared' = (ReleaseTo = "New")
          /\ notices' = notices + IF ReleaseTo = "New" THEN 1 ELSE 0
          /\ in' = [in EXCEPT ![Seed].st = IF @ = "Queued" THEN "Cancelled" ELSE @]
     ELSE /\ phase' = "Held" /\ UNCHANGED <<kills, barrier, sel, declared, notices, in>>
  /\ abort' = FALSE
  /\ UNCHANGED <<kills, latch, alive, expired, online, crashes, turn, turnBy, turnGen, body,
       reqEnded, foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt, stopFlight,
       launchFlight, seedOk, badCut, badDrain>>

Crash ==
  /\ online /\ crashes < MaxCrashes /\ online' = FALSE /\ crashes' = crashes + 1
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, turn, turnBy, turnGen,
       in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt, legacyDebt,
       stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>
\* Boot reads the durable phase, records and selector; in-flight effects keep resolving.
Boot ==
  /\ ~online /\ online' = TRUE
  /\ UNCHANGED <<kills, phase, latch, sel, alive, barrier, abort, expired, crashes, turn, turnBy,
       turnGen, in, body, reqEnded, foreign, blocker, sched, schedLost, owned, outDebt,
       legacyDebt, stopFlight, launchFlight, seedOk, declared, notices, badCut, badDrain>>

Internal == Reserve \/ ReserveUnsupported \/ AskDispatch \/ AbortPre \/ Barrier
  \/ StopDispatch \/ StopResolve \/ ProveStopped \/ LaunchDispatch \/ LaunchResolve
  \/ Bind \/ SeedDispatch \/ SeedDone \/ Release \/ EndTurn \/ DrainO \/ DrainLegacy
  \/ ColdLaunch \/ RemoveBlocker \/ Tick \/ Boot \/ (\E i \in Inputs: Deliver(i))
  \/ (\E i \in Ordinary: Pin(i))
Environment == Stage \/ DirectInput \/ FireSchedule \/ ToggleSchedule \/ AddBlocker
  \/ ExternalReplace \/ Crash \/ (\E g \in Gens: ExternalKill(g))
  \/ (\E c \in BOOLEAN: UserControl(c)) \/ (\E i \in Ordinary: Arrive(i))
Next == Internal \/ Environment

\* Environment, agent and provider choices carry no fairness; every owned step and every
\* dispatched effect's resolution (success or failure) is weakly fair.
Fairness ==
  /\ WF_vars(AskDispatch) /\ WF_vars(AbortPre) /\ WF_vars(Barrier) /\ WF_vars(StopDispatch)
  /\ WF_vars(StopResolve) /\ WF_vars(ProveStopped) /\ WF_vars(LaunchDispatch)
  /\ WF_vars(LaunchResolve) /\ WF_vars(Bind) /\ WF_vars(SeedDispatch) /\ WF_vars(SeedDone)
  /\ WF_vars(Release) /\ WF_vars(EndTurn) /\ WF_vars(DrainO) /\ WF_vars(DrainLegacy)
  /\ WF_vars(ColdLaunch) /\ WF_vars(RemoveBlocker) /\ WF_vars(Tick) /\ WF_vars(Boot)
  /\ \A i \in Inputs: WF_vars(Deliver(i))
  /\ \A i \in Ordinary: WF_vars(Pin(i))
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK ==
  /\ phase \in Active \cup Settled /\ latch \in [Gens -> {"Fresh", "Spent"}]
  /\ sel \in Gens /\ alive \in [Gens -> BOOLEAN] /\ turn \in {"Idle", "Open"}
  /\ turnBy \in Inputs \cup {None} /\ turnGen \in Gens \cup {None}
  /\ in \in [Inputs -> [st : States, pin : Gens \cup {None}, dlv : SUBSET Gens, n : 0..2]]
  /\ crashes \in 0..MaxCrashes /\ kills \in 0..MaxKills /\ notices \in 0..2
  /\ {barrier, abort, expired, online, body, reqEnded, foreign, blocker, sched, schedLost,
      owned, outDebt, legacyDebt, stopFlight, launchFlight, seedOk, declared, badCut,
      badDrain} \subseteq BOOLEAN

\* Rotation's /exit never lands on an open turn.
NoTurnCut == ~badCut
\* Rotation's /exit never ends Old with output it or the Legacy watcher still owes.
NoUnflushedCut == ~badDrain
\* One request and one seed per generation, never re-sent; at most one rotation notice.
NoRepeatedRequest == in[Req].n <= 1 /\ in[Seed].n <= 1
NoDuplicatePost == notices <= 1
\* The latch is spent exactly when a request may have gone out.
LatchDurable == in[Req].n > 0 => latch["Old"] = "Spent"
LatchOnlyIfAsked == latch["Old"] = "Spent" => in[Req].n > 0
\* Each input lands in at most one session; the request only in Old, the seed only in New.
InputExactlyOneSession == /\ \A i \in Inputs: Cardinality(in[i].dlv) <= 1
                          /\ in[Req].dlv \subseteq {"Old"} /\ in[Seed].dlv \subseteq {"New"}
\* New gets ordinary or direct work only after the seed, or after the one notice said it
\* did not arrive.
SeedFirst == \A i \in Ordinary \cup {Direct}: "New" \in in[i].dlv => seedOk \/ declared
\* Rotation never ends a CLI that still holds a session-only schedule.
NoScheduleLoss == ~schedLost
SingleExecution == Cardinality({g \in Gens: alive[g]}) <= 1

\* No permanent stall: the barrier is released, a waiting input leaves the queue,
\* and a started rotation settles.
BarrierReleases == barrier ~> ~barrier
QueuedLeaves == \A i \in Ordinary: (in[i].st = "Queued") ~> (in[i].st # "Queued")
ReservationResolved == (phase \in Active) ~> (phase \in Settled)
\* Reachability witnesses, each expected to break: the success path (seed then New work),
\* every automatic exit and the third-UUID path are reachable, so the rules are not vacuous.
W_ReleasedToOld == ~(phase = "Released" /\ in[Req].n = 1 /\ ~alive["Old"] /\ sel = "Old")
W_ReleasedToNew == ~(phase = "Released" /\ declared /\ \E i \in Ordinary: "New" \in in[i].dlv)
W_ReleasedToOther == ~(phase = "Released" /\ sel = "Other")
W_AbortBeforeAsk == ~(phase = "Idle" /\ latch["Old"] = "Fresh" /\ in[Fire].st = "Accepted"
                      /\ in[Req].n = 0 /\ sched = FALSE /\ turn = "Idle")
W_NewOrdinaryAfterSeed == ~(seedOk /\ \E i \in Ordinary: "New" \in in[i].dlv)
=============================================================================
