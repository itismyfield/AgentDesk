--------------------------- MODULE SessionRotation ---------------------------
\* Transcript-size rotation of one Claude channel, Old -> New once, as design-3 specifies it.
\* Each Fix* constant switches one repair on; a cfg that turns one off shows what it guards.
EXTENDS Naturals, FiniteSets

CONSTANTS Ordinary, Req, Seed, Direct, Fire, Native, MaxCrashes, MaxKills,
          FixRelease,   \* a post-barrier abort releases the barrier once its effects resolved
          FixLatch,     \* the latch is spent at request dispatch, never by an ineligible check
          FixRecreate,  \* reserved origins refuse the warm follow-up recreate branch
          FixDirect,    \* /tui/send, passthrough and Herdr settings keys refuse under the barrier
          FixSched,     \* a pending CLI schedule makes the generation ineligible
          FixOwner,     \* only an O-owned, Undelivered-published channel is eligible
          FixUnknown,   \* an indeterminate /exit or create keeps the barrier until it resolves
          FixHold,      \* an unknown seed's host input hold is resolved before release
          FixNotice,    \* the seed-loss notice is a durable intent with a persistent dedupe key
          FixClear,     \* clear cancels the rotation and drops waiting input in one Owner commit
          FixSelf,      \* the rotation's own New selector publish is not an external change
          FixCas,       \* the rotation never overwrites a selector another path chose
          FixLatchKey,  \* the latch is kept for every requested UUID, not only the current one
          NativeGate,   \* only a host whose /exit waits for a running turn is eligible
          HostQueuesExit, \* the host's CLI runs a slash command typed into a busy turn afterwards
          EnvBypass,    \* inputs outside the Owner (direct, native, CLI schedule) occur
          EnvControl    \* stop, clear and outside selections occur

ASSUME Ordinary # {} /\ Ordinary \cap {Req, Seed, Direct, Fire, Native} = {}
       /\ Cardinality({Req, Seed, Direct, Fire, Native}) = 5
       /\ {MaxCrashes, MaxKills} \subseteq Nat

Inputs == Ordinary \cup {Req, Seed, Direct, Fire, Native}
Bypass == {Direct, Fire, Native}
Gens == {"Old", "New", "Other"}
None == "None"
PreBarrier == {"Requesting", "Asked", "Staged"}
PostBarrier == {"Barrier", "Stopping", "OldEnded", "Launching", "Bound", "SeedPending"}
Active == PreBarrier \cup PostBarrier
Settled == {"Idle", "Released", "Done", "Held", "Cleared"}
States == {"Absent", "Queued", "Sent", "Accepted", "Unknown", "Failed", "Cancelled"}
Rec0 == [st |-> "Absent", pin |-> None, dlv |-> {}]

VARIABLE s
vars == <<s>>

NoFlight == \A i \in Inputs: s.in[i].st # "Sent"
QueuedOrd == \E i \in Ordinary: s.in[i].st \in {"Queued", "Sent"}
\* The live execution behind the channel's pane, which bypass inputs reach.
Pane == IF s.alive["New"] THEN "New" ELSE IF s.alive["Old"] THEN "Old" ELSE "Other"
\* The rotation's own publish of the fixed New UUID is not a third selector.
OwnSel == IF FixSelf THEN {"Old", "New"} ELSE {"Old"}
Eligible == /\ (FixOwner => s.owned) /\ (FixSched => ~s.sched)
            /\ (NativeGate => HostQueuesExit)
OldIdle == /\ s.turn = "Idle" /\ ~s.blocker /\ ~s.outDebt /\ NoFlight /\ s.alive["Old"]
           /\ s.sel = "Old"
OldQuiet == OldIdle /\ ~QueuedOrd

\* Ownership and a pending schedule vary per run; every other field starts empty.
InitAll == \E o \in BOOLEAN, sc \in BOOLEAN:
  s = [[phase |-> "Idle", latch |-> [g \in Gens |-> "Fresh"], asks |-> 0,
  sel |-> "Old", alive |-> [g \in Gens |-> g = "Old"], barrier |-> FALSE, abort |-> FALSE,
  expired |-> FALSE, online |-> TRUE, crashes |-> 0, kills |-> 0,
  turn |-> "Idle", turnBy |-> None, turnGen |-> None, in |-> [i \in Inputs |-> Rec0],
  body |-> FALSE, reqEnded |-> FALSE, foreign |-> FALSE, blocker |-> FALSE,
  sched |-> FALSE, schedLost |-> FALSE, owned |-> TRUE, outDebt |-> FALSE,
  legacyDebt |-> FALSE, stopFlight |-> FALSE, stopPending |-> FALSE,
  launchFlight |-> FALSE, launchPending |-> FALSE, hold |-> FALSE, holdRefused |-> FALSE,
  seedOk |-> FALSE, noticeIntent |-> FALSE, noticeRec |-> FALSE, outboxKey |-> FALSE,
  notices |-> 0, clearing |-> FALSE, clearTarget |-> {}, extSel |-> FALSE,
  badCut |-> FALSE, badDrain |-> FALSE, stopUnknownEver |-> FALSE, strayExit |-> FALSE,
  nativeInStop |-> FALSE, selfCancel |-> FALSE]
  EXCEPT !.owned = o, !.sched = sc]

\* A turn opened on Old after the request went out means the handoff may be stale.
Opened(t, i, g) == [t EXCEPT !.turn = "Open", !.turnBy = i, !.turnGen = g,
  !.foreign = @ \/ (g = "Old" /\ i # Req /\ s.phase \in {"Asked", "Staged"} \cup PostBarrier)]

\* ---------------- managed custody (Owner queue V2) ----------------
Arrive(i) == /\ i \in Ordinary /\ s.in[i].st = "Absent"
  /\ s' = [s EXCEPT !.in[i].st = "Queued"]
Pin(i) == /\ s.online /\ i \in Ordinary /\ s.in[i].st = "Queued" /\ ~s.barrier
  /\ s.phase \notin PostBarrier /\ s.alive[s.sel] /\ s.turn = "Idle" /\ NoFlight
  /\ s' = [s EXCEPT !.in[i].st = "Sent", !.in[i].pin = s.sel]
\* A dispatched write resolves later, across a crash too: accepted, landed with its receipt lost,
\* not landed, refused by a host input hold, or the warm follow-up's recreate branch.
Deliver(i) == /\ s.in[i].st = "Sent"
  /\ LET g == s.in[i].pin
         held == s.hold /\ g = "New" /\ i # Seed
     IN \/ /\ s.alive[g] /\ s.turn = "Idle" /\ ~held
           /\ \E ack \in {"Accepted", "Unknown"}:
                s' = Opened([s EXCEPT !.in[i].st = ack, !.in[i].dlv = @ \cup {g},
                                      !.hold = @ \/ (i = Seed /\ ack = "Unknown")], i, g)
        \/ /\ ~held
           /\ s' = [s EXCEPT !.in[i].st = IF s.alive[g] THEN "Failed" ELSE "Unknown",
                             !.hold = @ \/ (i = Seed /\ s.alive[g])]
        \* The existing Herdr first-input path refuses a key while an earlier hold is kept.
        \/ /\ held
           /\ s' = [s EXCEPT !.in[i].st = "Failed", !.holdRefused = TRUE]
        \/ /\ s.alive[g] /\ s.turn = "Idle" /\ g # "Other" /\ ~s.alive["Other"] /\ ~held
           /\ (i \in {Req, Seed} => ~FixRecreate)
           /\ s' = Opened([s EXCEPT !.in[i].st = "Accepted", !.in[i].pin = "Other",
                     !.in[i].dlv = @ \cup {"Other"}, !.alive[g] = FALSE,
                     !.alive["Other"] = TRUE, !.sel = "Other"], i, "Other")
\* The hold resolver releases the seed's hold: delivered, undelivered or unconfirmed with its
\* own notice row. It never resends.
HoldResolve == /\ s.hold /\ s.online
  /\ \/ /\ "New" \in s.in[Seed].dlv
        /\ s' = [s EXCEPT !.hold = FALSE, !.in[Seed].st = "Accepted"]
     \/ /\ s' = [s EXCEPT !.hold = FALSE,
                   !.in[Seed].st = IF @ = "Accepted" THEN @ ELSE "Unknown"]
EndTurn == /\ s.turn = "Open"
  /\ s' = [s EXCEPT !.turn = "Idle", !.turnBy = None, !.turnGen = None,
       !.reqEnded = @ \/ s.turnBy = Req,
       !.outDebt = @ \/ (s.turnGen = "Old" /\ s.owned),
       !.legacyDebt = @ \/ (s.turnGen = "Old" /\ ~s.owned)]
DrainO == /\ s.outDebt /\ s.turn = "Idle" /\ s' = [s EXCEPT !.outDebt = FALSE]
DrainLegacy == /\ s.legacyDebt /\ s.alive["Old"] /\ s.turn = "Idle"
  /\ s' = [s EXCEPT !.legacyDebt = FALSE]
\* The existing cold path resumes the selected UUID when no execution runs.
ColdLaunch == /\ s.online /\ ~s.barrier /\ s.phase \in Settled /\ QueuedOrd
  /\ \A g \in Gens: ~s.alive[g]
  /\ s' = [s EXCEPT !.alive[s.sel] = TRUE]

\* ---------------- inputs outside the Owner ----------------
\* /tui/send, slash passthrough and Herdr settings keys, under the composer lock.
DirectInput == /\ EnvBypass /\ s.online /\ s.in[Direct].st = "Absent" /\ s.alive[Pane] /\ s.turn = "Idle"
  /\ NoFlight /\ (FixDirect => ~s.barrier)
  /\ s' = Opened([s EXCEPT !.in[Direct] = [st |-> "Accepted", pin |-> Pane, dlv |-> {Pane}]],
                 Direct, Pane)
\* A person types into the pane. Nothing in AgentDesk can refuse it, during /exit as well.
NativeInput == /\ EnvBypass /\ s.in[Native].st = "Absent" /\ s.alive[Pane] /\ s.turn = "Idle"
  /\ s' = Opened([s EXCEPT !.in[Native] = [st |-> "Accepted", pin |-> Pane, dlv |-> {Pane}],
                 !.nativeInStop = s.stopFlight \/ s.stopPending], Native, Pane)
\* A ScheduleWakeup, /loop or CronCreate job fires inside the Old CLI when its REPL is idle.
FireSchedule == /\ EnvBypass /\ s.sched /\ s.alive["Old"] /\ s.turn = "Idle" /\ s.in[Fire].st = "Absent"
  /\ s' = Opened([s EXCEPT !.in[Fire] = [st |-> "Accepted", pin |-> "Old", dlv |-> {"Old"}]],
                 Fire, "Old")
ToggleSchedule == /\ s.turn = "Open" /\ s.turnGen = "Old" /\ s' = [s EXCEPT !.sched = ~@]
AddBlocker == /\ s.turn = "Open" /\ s.turnGen = "Old" /\ ~s.blocker
  /\ s' = [s EXCEPT !.blocker = TRUE]
RemoveBlocker == /\ s.blocker /\ s' = [s EXCEPT !.blocker = FALSE]

\* ---------------- paths outside the rotation ----------------
\* kill-tmux, force-kill, idle-kill, Terminate, a lost lease, prompt-too-long: one execution ends,
\* and an unclear /exit of that process can no longer land.
ExternalKill(g) == /\ s.alive[g] /\ s.kills < MaxKills
  /\ s' = [s EXCEPT !.alive[g] = FALSE, !.kills = @ + 1,
       !.stopPending = IF g = "Old" THEN FALSE ELSE @,
       !.turn = IF s.turnGen = g THEN "Idle" ELSE @,
       !.turnBy = IF s.turnGen = g THEN None ELSE @,
       !.turnGen = IF s.turnGen = g THEN None ELSE @]
\* /resume, manual rebind, provider reset or an in-pane /clear picks a session: the pane's
\* execution ends and the chosen UUID becomes the selector. A single-UUID latch forgets others.
ExternalSelect(g) == /\ EnvControl /\ s.online /\ g \in {"Old", "Other"} /\ s.sel # g /\ s.kills < MaxKills
  /\ ~s.stopFlight /\ ~s.launchFlight /\ ~s.launchPending
  /\ s' = [s EXCEPT !.alive = [x \in Gens |-> FALSE], !.sel = g, !.kills = @ + 1, !.stopPending = FALSE,
       !.extSel = (g = "Other"), !.turn = "Idle", !.turnBy = None, !.turnGen = None,
       !.latch = IF FixLatchKey THEN @ ELSE [x \in Gens |-> IF x = g THEN @[x] ELSE "Fresh"]]
\* Cancel, stop, Terminate, effort restart: the command revokes the rotation, then runs.
UserStop == /\ EnvControl /\ s.online /\ s.phase \in Active /\ ~s.abort
  /\ s' = [s EXCEPT !.abort = TRUE, !.turn = "Idle", !.turnBy = None, !.turnGen = None]
\* /clear bumps the clear epoch first (refusing effects not started), waits for started ones,
\* then clears the Owner in one actor turn and tears the executions down.
ClearBegin == /\ EnvControl /\ s.online /\ ~s.clearing
  /\ s' = [s EXCEPT !.clearing = TRUE, !.abort = s.phase \in Active,
       !.clearTarget = IF s.barrier THEN {i \in Ordinary: s.in[i].st = "Queued"} ELSE {}]
ClearCommit == /\ s.online /\ s.clearing
  /\ (FixClear => ~s.stopFlight /\ ~s.launchFlight)
  /\ s' = [s EXCEPT !.clearing = FALSE, !.clearTarget = {},
       !.in = [i \in Inputs |-> IF i \in Ordinary /\ s.in[i].st = "Queued"
                                THEN [s.in[i] EXCEPT !.st = "Cancelled"] ELSE s.in[i]],
       !.phase = IF @ \in Active THEN "Cleared" ELSE @,
       !.barrier = FALSE, !.abort = FALSE, !.alive = [x \in Gens |-> FALSE], !.sel = "Other",
       !.launchPending = FALSE, !.stopPending = FALSE, !.extSel = FALSE,
       !.turn = "Idle", !.turnBy = None, !.turnGen = None]

\* ---------------- rotation ----------------
\* P1: eligibility::eligible and schedules::pending_schedule precede Owner::reserve_rotation.
Reserve == /\ s.online /\ s.phase = "Idle" /\ s.latch["Old"] = "Fresh" /\ Eligible /\ OldQuiet
  /\ ~s.clearing
  /\ s' = [s EXCEPT !.phase = "Requesting", !.in[Req] = [Rec0 EXCEPT !.st = "Queued",
       !.pin = "Old"], !.latch["Old"] = IF FixLatch THEN @ ELSE "Spent"]
ReserveUnsupported == /\ ~FixLatch /\ s.online /\ s.phase = "Idle"
  /\ s.latch["Old"] = "Fresh" /\ ~Eligible /\ OldQuiet
  /\ s' = [s EXCEPT !.latch["Old"] = "Spent"]
\* P1: db/transcript_rotation_requests.rs inserts before Owner::reserve_rotation commits.
\* This model abstracts that pair and the later request send as one dispatch.
AskDispatch == /\ s.online /\ s.phase = "Requesting" /\ ~s.abort /\ ~s.expired /\ OldQuiet
  /\ Eligible /\ s.in[Req].st = "Queued" /\ ~s.clearing
  /\ s' = [s EXCEPT !.in[Req].st = "Sent", !.asks = @ + 1, !.latch["Old"] = "Spent",
       !.phase = "Asked"]
\* P1: routes/transcript_rotation.rs submit calls Owner::stage_rotation for the pinned ticket.
Stage == /\ s.online /\ s.phase = "Asked" /\ s.turn = "Open" /\ s.turnBy = Req
  /\ s.turnGen = "Old" /\ s' = [s EXCEPT !.phase = "Staged", !.body = TRUE]
Tick == /\ s.phase \in PreBarrier /\ ~s.expired /\ s' = [s EXCEPT !.expired = TRUE]
PreAbort == \/ s.abort \/ s.expired \/ s.foreign \/ QueuedOrd \/ s.sel # "Old"
            \/ ~s.alive["Old"] \/ s.in[Req].st = "Failed"
            \/ (s.phase = "Asked" /\ s.reqEnded /\ ~s.body) \/ (FixSched /\ s.sched)
AbortPre == /\ s.online /\ s.phase \in PreBarrier /\ PreAbort
  /\ s' = [s EXCEPT !.phase = IF s.asks > 0 THEN "Released" ELSE "Idle",
       !.in[Req] = IF @.st = "Queued" THEN Rec0 ELSE @, !.abort = FALSE, !.expired = FALSE,
       !.body = FALSE, !.reqEnded = FALSE, !.foreign = FALSE]
\* The barrier commit is the cutover linearization point for managed input.
Barrier == /\ s.online /\ s.phase = "Staged" /\ s.reqEnded /\ ~PreAbort /\ OldQuiet
  /\ s' = [s EXCEPT !.phase = "Barrier", !.barrier = TRUE]
\* /exit goes out under the composer lock after the barrier and clear epoch are read again.
StopDispatch == /\ s.online /\ s.phase = "Barrier" /\ ~s.abort /\ ~s.foreign /\ OldIdle
  /\ (FixSched => ~s.sched) /\ (FixClear => ~s.clearing)
  /\ s' = [s EXCEPT !.phase = "Stopping", !.stopFlight = TRUE]
\* A clean exit waits for a running turn when the CLI queues the slash command. O output still
\* drains from the file after the exit; the Legacy watcher's does not.
ExitCuts == s.turn = "Open" /\ s.turnGen = "Old"
OldEnds(t) == [t EXCEPT !.alive["Old"] = FALSE, !.badCut = @ \/ ExitCuts,
  !.badDrain = @ \/ s.legacyDebt, !.schedLost = @ \/ (s.sched /\ ~s.nativeInStop),
  !.turn = IF s.turnGen = "Old" THEN "Idle" ELSE @,
  !.turnBy = IF s.turnGen = "Old" THEN None ELSE @,
  !.turnGen = IF s.turnGen = "Old" THEN None ELSE @]
\* Ended, Untouched (no key), or Unclear/timeout/probe error: the exit may still land later.
StopResolve == /\ s.stopFlight
  /\ \/ /\ s.alive["Old"] /\ (HostQueuesExit => ~ExitCuts)
        /\ s' = OldEnds([s EXCEPT !.stopFlight = FALSE])
     \/ s' = [s EXCEPT !.stopFlight = FALSE]
     \/ s' = [s EXCEPT !.stopFlight = FALSE, !.stopPending = TRUE, !.stopUnknownEver = TRUE]
\* Host evidence later settles an unclear exit: it landed or it did not. A landing after the
\* rotation released Old ends a session other work was already handed to.
StopSettle == /\ s.stopPending
  /\ \/ /\ s.alive["Old"] /\ (HostQueuesExit => ~ExitCuts)
        /\ s' = OldEnds([s EXCEPT !.stopPending = FALSE,
                                  !.strayExit = @ \/ s.phase \in Settled])
     \/ s' = [s EXCEPT !.stopPending = FALSE]
ProveStopped == /\ s.online /\ s.phase = "Stopping" /\ ~s.stopFlight /\ ~s.stopPending
  /\ ~s.alive["Old"] /\ ~s.abort /\ s.sel = "Old"
  /\ s' = [s EXCEPT !.phase = "OldEnded"]
\* The selector is read again right before the create, as before every effect.
LaunchDispatch == /\ s.online /\ s.phase = "OldEnded" /\ ~s.abort /\ \A g \in Gens: ~s.alive[g]
  /\ s.sel = "Old"
  /\ (FixClear => ~s.clearing)
  /\ s' = [s EXCEPT !.phase = "Launching", !.launchFlight = TRUE]
\* FreshExact create: Created, NotSent, or Indeterminate (a pane may still appear).
LaunchResolve == /\ s.launchFlight
  /\ \/ s' = [s EXCEPT !.launchFlight = FALSE, !.alive["New"] = TRUE]
     \/ s' = [s EXCEPT !.launchFlight = FALSE]
     \/ s' = [s EXCEPT !.launchFlight = FALSE, !.launchPending = TRUE]
\* Exact recovery of the Pending row: the pane exists (SameExecution) or a full read says Gone.
LaunchSettle == /\ s.launchPending
  /\ \/ s' = [s EXCEPT !.launchPending = FALSE, !.alive["New"] = TRUE]
     \/ s' = [s EXCEPT !.launchPending = FALSE]
\* SessionStart of the fixed UUID moves the selector before the rotation binds it.
SelfPublish == /\ s.phase \in {"Launching", "Bound", "SeedPending"} /\ s.alive["New"]
  /\ s.sel = "Old" /\ s' = [s EXCEPT !.sel = "New"]
Bind == /\ s.online /\ s.phase = "Launching" /\ ~s.launchFlight /\ ~s.launchPending
  /\ s.alive["New"] /\ ~s.abort /\ s.sel \in OwnSel
  /\ s' = [s EXCEPT !.phase = "Bound", !.in[Seed] = [Rec0 EXCEPT !.st = "Queued", !.pin = "New"]]
SeedDispatch == /\ s.online /\ s.phase = "Bound" /\ ~s.abort /\ s.alive["New"]
  /\ s.turn = "Idle" /\ NoFlight /\ s.in[Seed].st = "Queued" /\ s.sel \in OwnSel
  /\ (FixClear => ~s.clearing)
  /\ s' = [s EXCEPT !.phase = "SeedPending", !.in[Seed].st = "Sent"]
\* The final selector publish is a CAS from the rotation's own set.
SeedDone == /\ s.online /\ s.phase = "SeedPending" /\ s.in[Seed].st = "Accepted" /\ ~s.hold
  /\ (FixCas => s.sel \in OwnSel) /\ (FixClear => ~s.clearing)
  /\ s' = [s EXCEPT !.phase = "Done", !.seedOk = TRUE, !.barrier = FALSE, !.sel = "New",
       !.abort = FALSE]

\* Each post-barrier failure has one automatic exit once its effects resolved.
Resolved == (FixUnknown => ~s.stopPending /\ ~s.launchPending) /\ (FixHold => ~s.hold)
PostAbort ==
  \/ s.phase = "Barrier" /\ (s.abort \/ s.foreign \/ ~s.alive["Old"] \/ s.sel # "Old"
                             \/ s.blocker \/ (FixSched /\ s.sched))
  \/ s.phase = "Stopping" /\ ~s.stopFlight /\ (s.alive["Old"] \/ s.abort \/ s.sel # "Old")
  \/ s.phase = "OldEnded" /\ (s.abort \/ s.sel # "Old")
  \/ s.phase = "Launching" /\ ~s.launchFlight
     /\ (s.abort \/ ~s.alive["New"] \/ s.sel \notin OwnSel)
  \/ s.phase = "Bound" /\ (s.abort \/ ~s.alive["New"] \/ s.sel \notin OwnSel)
  \/ s.phase = "SeedPending" /\ s.in[Seed].st # "Sent"
     /\ (s.in[Seed].st \in {"Unknown", "Failed"} \/ s.abort \/ ~s.alive["New"]
         \/ s.sel \notin OwnSel)
\* A selector another path chose wins; otherwise New once it ran, else Old.
ReleaseTo == IF FixCas /\ s.sel = "Other" THEN "Other"
             ELSE IF s.alive["New"] \/ s.phase \in {"Bound", "SeedPending"} THEN "New"
             ELSE IF s.sel = "Other" THEN "Other" ELSE "Old"
NeedsNotice == ReleaseTo = "New" /\ ~s.seedOk
\* The notice intent is committed in the Owner before the release that depends on it.
DeclareLoss == /\ s.online /\ FixNotice /\ s.phase \in PostBarrier /\ PostAbort /\ Resolved
  /\ NeedsNotice /\ ~s.noticeIntent /\ ~s.clearing
  /\ s' = [s EXCEPT !.noticeIntent = TRUE]
Release == /\ s.online /\ s.phase \in PostBarrier /\ PostAbort /\ Resolved
  /\ (FixClear => ~s.clearing)
  /\ (FixNotice /\ NeedsNotice => s.noticeRec)
  /\ IF FixRelease
     THEN s' = [s EXCEPT !.phase = "Released", !.barrier = FALSE, !.sel = ReleaseTo,
            !.selfCancel = s.sel = "New" /\ s.alive["New"] /\ ~s.abort /\ ~s.extSel
                           /\ s.in[Seed].st \notin {"Unknown", "Failed"},
            !.noticeIntent = @ \/ NeedsNotice, !.abort = FALSE,
            !.in[Seed].st = IF @ = "Queued" THEN "Cancelled" ELSE @]
     ELSE s' = [s EXCEPT !.phase = "Held", !.abort = FALSE]
\* The outbox post; a persistent key posts once, a TTL key may post again after it expires.
NoticePost == /\ s.online /\ s.noticeIntent /\ ~s.noticeRec /\ s.notices < 2
  /\ s' = [s EXCEPT !.outboxKey = TRUE,
       !.notices = IF FixNotice /\ s.outboxKey THEN @ ELSE @ + 1]
NoticeRecord == /\ s.online /\ s.outboxKey /\ s.noticeIntent /\ ~s.noticeRec
  /\ s' = [s EXCEPT !.noticeRec = TRUE]
\* A settled rotation opens the way for a later one; the latch decides whether it may ask.
Rearm == /\ s.online /\ s.phase \in {"Released", "Done", "Cleared"} /\ ~s.clearing
  /\ ~s.barrier /\ s.in[Req].st \notin {"Sent"}
  /\ s' = [s EXCEPT !.phase = "Idle", !.in[Req] = Rec0, !.body = FALSE, !.reqEnded = FALSE,
       !.foreign = FALSE, !.expired = FALSE]

Crash == /\ s.online /\ s.crashes < MaxCrashes
  /\ s' = [s EXCEPT !.online = FALSE, !.crashes = @ + 1]
Boot == /\ ~s.online /\ s' = [s EXCEPT !.online = TRUE]

System == AskDispatch \/ AbortPre \/ Barrier \/ StopDispatch \/ StopResolve \/ StopSettle
  \/ ProveStopped \/ LaunchDispatch \/ LaunchResolve \/ LaunchSettle \/ Bind \/ SeedDispatch
  \/ SeedDone \/ DeclareLoss \/ Release \/ NoticePost \/ NoticeRecord \/ HoldResolve
  \/ EndTurn \/ DrainO \/ DrainLegacy \/ ColdLaunch \/ RemoveBlocker \/ Tick \/ Boot
  \/ ClearCommit \/ SelfPublish \/ (\E i \in Inputs: Deliver(i)) \/ (\E i \in Ordinary: Pin(i))
Environment == Reserve \/ ReserveUnsupported \/ Rearm \/ Stage \/ DirectInput \/ NativeInput
  \/ FireSchedule \/ ToggleSchedule \/ AddBlocker \/ UserStop \/ ClearBegin
  \/ Crash \/ (\E g \in Gens: ExternalKill(g) \/ ExternalSelect(g))
  \/ (\E i \in Ordinary: Arrive(i))
Next == System \/ Environment

\* Owned steps and every effect's resolution are weakly fair; people, agents, providers and
\* the other paths are not. A best-effort notice (FixNotice off) has no retry.
Fairness ==
  /\ WF_vars(AskDispatch) /\ WF_vars(AbortPre) /\ WF_vars(Barrier) /\ WF_vars(StopDispatch)
  /\ WF_vars(StopResolve) /\ WF_vars(StopSettle) /\ WF_vars(ProveStopped)
  /\ WF_vars(LaunchDispatch) /\ WF_vars(LaunchResolve) /\ WF_vars(LaunchSettle)
  /\ WF_vars(Bind) /\ WF_vars(SeedDispatch) /\ WF_vars(SeedDone) /\ WF_vars(DeclareLoss)
  /\ WF_vars(Release) /\ WF_vars(FixNotice /\ NoticePost) /\ WF_vars(NoticeRecord)
  /\ WF_vars(HoldResolve) /\ WF_vars(EndTurn) /\ WF_vars(DrainO) /\ WF_vars(DrainLegacy)
  /\ WF_vars(ColdLaunch) /\ WF_vars(RemoveBlocker) /\ WF_vars(Tick) /\ WF_vars(Boot)
  /\ WF_vars(ClearCommit) /\ WF_vars(SelfPublish)
  /\ \A i \in Inputs: WF_vars(Deliver(i))
  /\ \A i \in Ordinary: WF_vars(Pin(i))
Spec == InitAll /\ [][Next]_vars /\ Fairness

TypeOK ==
  /\ s.phase \in Active \cup Settled /\ s.latch \in [Gens -> {"Fresh", "Spent"}]
  /\ s.asks \in 0..2 /\ s.sel \in Gens /\ s.alive \in [Gens -> BOOLEAN]
  /\ s.turn \in {"Idle", "Open"} /\ s.turnBy \in Inputs \cup {None}
  /\ s.turnGen \in Gens \cup {None} /\ s.crashes \in 0..MaxCrashes /\ s.kills \in 0..MaxKills
  /\ s.in \in [Inputs -> [st : States, pin : Gens \cup {None}, dlv : SUBSET Gens]]
  /\ s.notices \in 0..2 /\ s.clearTarget \subseteq Ordinary

\* The rotation's /exit never ends Old under an open turn, a person's included.
NoTurnCut == ~s.badCut
\* No /exit of the rotation lands after it handed Old back to other work.
NoStrayEffect == ~s.strayExit
NoUnflushedCut == ~s.badDrain
\* One request per UUID, ever (A -> B -> A included); one seed per rotation.
NoRepeatedRequest == s.asks <= 1
NoDuplicatePost == s.notices <= 1
LatchDurable == s.asks > 0 => s.latch["Old"] = "Spent"
LatchOnlyIfAsked == s.latch["Old"] = "Spent" => s.asks > 0
InputExactlyOneSession == /\ \A i \in Inputs: Cardinality(s.in[i].dlv) <= 1
                          /\ s.in[Req].dlv \subseteq {"Old"} /\ s.in[Seed].dlv \subseteq {"New"}
\* New takes other work only after the seed, or after the seed-loss notice is in the outbox.
SeedFirst == \A i \in Ordinary \cup {Direct}: "New" \in s.in[i].dlv => s.seedOk \/ s.noticeRec
\* The rotation never ends a CLI that held a session-only schedule when it chose to; a schedule a
\* person's own turn makes inside the exit window is the documented residual risk.
NoScheduleLoss == ~s.schedLost
\* At most one execution, and none left running beside the selected one once settled.
SingleExecution == /\ Cardinality({g \in Gens: s.alive[g]}) <= 1
                   /\ (s.phase \in Settled => \A g \in Gens: s.alive[g] => g = s.sel)
\* Release never hands input to a host input hold, nor a clear's input to a session.
NoHoldBypass == ~s.holdRefused
ClearedNotDelivered == \A i \in s.clearTarget: s.in[i].dlv = {}
\* A selector another path chose is never overwritten by the rotation.
NoOverwriteExternal == s.extSel => s.sel = "Other"
\* The rotation never abandons itself because its own SessionStart moved the selector.
NoSelfCancel == ~s.selfCancel

BarrierReleases == s.barrier ~> ~s.barrier
QueuedLeaves == \A i \in Ordinary: (s.in[i].st = "Queued") ~> (s.in[i].st # "Queued")
ReservationResolved == (s.phase \in Active) ~> (s.phase \in Settled)

\* Reachability witnesses, each expected to break so the rules are not vacuous.
W_NewOrdinaryAfterSeed == ~(s.seedOk /\ \E i \in Ordinary: "New" \in s.in[i].dlv)
W_ReleasedToNewAnnounced == ~(s.phase = "Released" /\ s.noticeRec
                              /\ \E i \in Ordinary: "New" \in s.in[i].dlv)
W_StopUnknownThenOld == ~(s.phase = "Released" /\ s.sel = "Old" /\ s.alive["Old"]
                          /\ s.stopUnknownEver)
W_ClearDuringRotation == ~(s.phase = "Cleared" /\ s.asks = 1)
W_ReleasedToOther == ~(s.phase = "Released" /\ s.sel = "Other" /\ s.extSel)
=============================================================================
