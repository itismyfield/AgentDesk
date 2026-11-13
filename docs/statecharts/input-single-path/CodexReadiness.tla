-------------------------- MODULE CodexReadiness --------------------------
\* Codex ready / busy / end / stop judgment (#8082 design d3).
\* Production code baseline 082c77aa4e06b81c76b614f55b8fed73ce01923f.
\* "before" is the code as it is: tmux waits on the pane and replaces it after a timeout,
\* Herdr waits on the pane and refuses. "after" reads the bound rollout, the hooks and the process.
\* One Codex pane, N queued inputs, at most one person turn typed straight into the TUI.
EXTENDS Naturals, FiniteSets
CONSTANTS
    N,            \* queued inputs
    Mode,         \* judgment variant, see Modes
    Host,         \* "tmux" | "herdr"
    Screen,       \* "ok" | "unread": a narrow pane or a new status line is unread for good
    Loss,         \* which end signal of the first ended turn never arrives
    Compress,     \* the idle rollout may be archived to .zst
    Restarts,     \* dcserver restarts (in-memory latch and hook evidence are lost)
    Person,       \* turns a person starts in the TUI
    Exits,        \* provider process exits
    BusyRoute,    \* the existing busy entry may deliver into an open turn
    Silent,       \* a running turn that writes nothing to the rollout: "none" | "tool" | "model"
    Stops,        \* /stop requests on an owned running turn
    HerdrReplace, \* Herdr can end and relaunch a pane and observe its children (#8128 B2a/B2b)
    Config,       \* tmux warm reuse becomes ineligible: launch options, binding or runtime kind changed
    Blocked       \* UserPromptSubmit hooks that are followed by no turn
VARIABLE r

Ids == 1..N
Modes == {"before", "after", "after-latch", "after-no-proc", "after-no-revalidate",
          "after-no-fallback", "after-no-floor", "after-no-child", "after-stop-screen",
          "after-timeout-replace", "after-reclaim-open", "after-no-ups",
          "after-guard-all", "after-ups-until-end"}
After == Mode # "before"
MaxInc == 5

Initial ==
    [mode |-> Mode, host |-> Host, screen |-> Screen,  \* constants, kept for the chart labels
     inc |-> 1, phase |-> "Booting", started |-> FALSE,
     turn |-> 0, seen |-> 0, rec |-> "none", pending |-> FALSE, late |-> FALSE, file |-> "plain",
     latch |-> FALSE, hook |-> FALSE, ups |-> FALSE, owner |-> 0,
     q |-> [i \in Ids |-> "queued"], effects |-> [i \in Ids |-> 0],
     head |-> 0, wait |-> 0, proofInc |-> 0, proofSeen |-> 0,
     mismatch |-> FALSE, config |-> Config, blocked |-> Blocked,
     lossLeft |-> IF Loss = "none" THEN 0 ELSE 1,
     person |-> Person, exits |-> Exits, restarts |-> Restarts,
     compress |-> IF Compress THEN 1 ELSE 0,
     stops |-> Stops, stop |-> "none", stopTurn |-> 0,
     badEarly |-> FALSE, badShell |-> FALSE, badKill |-> FALSE,
     badStopIdle |-> FALSE, badStopKey |-> FALSE]
Init == r = Initial

\* ---- what each judgment reads ----
\* The bound rollout's latest turn, read from this incarnation's floor (busy_inject/rollout.rs NativeEnd).
Level ==
    IF r.file = "zst" THEN "Unknown"
    ELSE CASE r.rec = "none" -> "NoStart"
           [] r.rec = "started" -> "Open"
           [] r.rec = "ended" -> "Ended"
           [] OTHER -> "Unknown"
\* The lenient reader of the Discord intake pre-check (tui_turn_state.rs observe_codex_jsonl_turn_state).
\* It reads turn_aborted and every event it does not know as Streaming (tui_turn_state.rs:731~:735).
LenientBusy == r.file = "plain" /\ r.rec \in {"started", "unknown"}
\* herdr process_info / tmux pane liveness names this incarnation's provider process;
\* "started" is that reading, not a SessionStart hook.
Alive == r.phase \in {"Idle", "Running"}
ProcOK == Mode = "after-no-proc" \/ (Alive /\ r.started)
\* A readable pane shows the truth; an unread pane proves nothing.
ScreenIdle == Screen = "ok" /\ r.phase = "Idle"
HookTier == Mode # "after-no-fallback" /\ r.hook
\* A UserPromptSubmit hook with no later end of its turn is Busy evidence (r.ups).
ReadyAfter ==
    /\ ProcOK /\ ~r.ups
    /\ \/ Level \in {"Ended", "NoStart"}
       \/ Level \in {"Open", "Unknown"} /\ HookTier
\* The Busy judgment: UserPromptSubmit evidence, or an open turn without a Stop hook.
BusyVerdict == ProcOK /\ (r.ups \/ (Level = "Open" /\ ~HookTier))
Ready ==
    CASE Mode = "before" -> ScreenIdle
      [] Mode = "after-latch" -> r.latch \/ ReadyAfter
      [] OTHER -> ReadyAfter
First == {i \in Ids : r.q[i] = "queued" /\ \A j \in 1..(i-1) : r.q[j] # "queued"}
Natives == {i \in Ids : r.q[i] = "native"}
\* A turn AgentDesk starts: its own submission is serialized by the channel mailbox, so its start
\* is taken as recorded at once (the InputOwnership models cover that order).
NewTurn(i) ==
    [r EXCEPT !.phase = "Running", !.turn = @ + 1, !.seen = r.turn + 1, !.rec = "started",
              !.pending = FALSE, !.late = FALSE, !.hook = FALSE, !.ups = FALSE, !.owner = i]
\* tmux relaunches with the prompt in the argv: the new incarnation runs input i as its first turn.
ArgvLaunch(i) ==
    [NewTurn(i) EXCEPT !.inc = @ + 1, !.started = TRUE, !.file = "plain", !.head = 0, !.wait = 0,
                       !.effects[i] = @ + 1, !.mismatch = FALSE,
                       !.q = [j \in Ids |-> IF j = i THEN "sent"
                                            ELSE IF j = r.owner /\ r.q[j] = "sent" THEN "done"
                                            ELSE r.q[j]],
                       !.badKill = @ \/ r.phase = "Running"]

\* ---- provider ----
\* The provider process becomes the pane's foreground execution and its composer takes keys.
\* Codex reports SessionStart only with the first prompt, so no hook marks this step.
Boot ==
    /\ r.phase = "Booting"
    /\ r' = [r EXCEPT !.phase = "Idle", !.started = TRUE]
\* A person presses Enter in the TUI. The UserPromptSubmit hook is delivered before the turn goes
\* on; the rollout's task_started is written a moment later.
PersonEnter ==
    /\ r.person > 0 /\ r.phase = "Idle" /\ ~r.late
    /\ r' = [r EXCEPT !.person = @ - 1, !.phase = "Running", !.turn = @ + 1, !.pending = TRUE,
                     !.hook = FALSE, !.owner = 0, !.ups = (After /\ Mode # "after-no-ups")]
\* The task_started record releases the UserPromptSubmit evidence when it can be read:
\* from there the open turn itself is the Busy evidence.
RecordStart ==
    /\ r.pending
    /\ r' = [r EXCEPT !.pending = FALSE, !.rec = "started", !.seen = r.turn, !.late = FALSE,
                     !.ups = IF Mode # "after-ups-until-end" /\ r.file = "plain" THEN FALSE ELSE @]
\* A UserPromptSubmit hook arrives and no turn follows (a prompt that a user-level hook blocks).
PersonBlocked ==
    /\ r.blocked > 0 /\ r.phase = "Idle" /\ ~r.late
    /\ r' = [r EXCEPT !.blocked = @ - 1, !.ups = (After /\ Mode # "after-no-ups")]
\* The operator changes the launch options, or the binding no longer matches: warm reuse is refused.
ConfigChange ==
    /\ r.config > 0 /\ Host = "tmux" /\ r.head = 0 /\ r.wait = 0
    /\ r' = [r EXCEPT !.config = @ - 1, !.mismatch = TRUE]
\* The turn ends in the TUI. Its terminal record is late, lost, or unnamed; its Stop hook may be lost.
Finish ==
    /\ r.phase = "Running" /\ ~r.pending
    /\ LET lossy == r.lossLeft > 0
           hookSeen == ~(lossy /\ Loss \in {"hook", "both", "unnamed-both"})
           ended == [r EXCEPT !.phase = "Idle", !.hook = hookSeen,
                              !.ups = IF hookSeen THEN FALSE ELSE @,
                              !.lossLeft = IF lossy THEN @ - 1 ELSE @,
                              !.q = [i \in Ids |-> IF i = r.owner \/ r.q[i] = "native"
                                                   THEN "done" ELSE r.q[i]]]
       IN r' = CASE lossy /\ Loss \in {"record", "both"} -> ended
                 [] lossy /\ Loss \in {"unnamed", "unnamed-both"} -> [ended EXCEPT !.rec = "unknown"]
                 [] OTHER -> [ended EXCEPT !.late = TRUE]
\* The late terminal record reaches the file; a following tail latches composer_ready.
WriteLate ==
    /\ r.late
    /\ r' = [r EXCEPT !.late = FALSE, !.rec = "ended", !.latch = TRUE, !.ups = FALSE]
\* Codex CLI archives an old idle rollout; by then the hook evidence has expired.
CompressRollout ==
    /\ r.compress > 0 /\ r.phase = "Idle" /\ ~r.late /\ r.file = "plain" /\ r.head = 0 /\ r.wait = 0
    /\ r' = [r EXCEPT !.compress = @ - 1, !.file = "zst", !.hook = FALSE]
Exit ==
    /\ r.exits > 0 /\ Alive /\ Natives = {} /\ r.stop # "asked"
    /\ r' = [r EXCEPT !.exits = @ - 1, !.phase = "Exited", !.pending = FALSE,
                     !.q = [i \in Ids |-> IF i = r.owner /\ r.q[i] = "sent" THEN "done" ELSE r.q[i]]]

\* ---- AgentDesk ----
Restart ==
    /\ r.restarts > 0
    /\ r' = [r EXCEPT !.restarts = @ - 1, !.latch = FALSE, !.hook = FALSE, !.ups = FALSE,
                     !.head = 0, !.wait = 0]
\* tmux, as it is: the intake pre-check lets a follow-up into the 45 s pane wait unless the
\* rollout reads busy (intake_turn.rs:1549, warm_followup.rs:542).
Admit ==
    /\ Mode = "before" /\ Host = "tmux" /\ r.wait = 0 /\ r.head = 0 /\ Alive /\ ~LenientBusy
    /\ ~r.mismatch
    /\ \E i \in First : r' = [r EXCEPT !.wait = i]
\* tmux, as it is: the wait timed out on a pane that never read ready, so the pane is replaced and
\* the prompt goes in the new argv (absence_launch.rs:273). Nothing asks whether a turn runs.
\* "after-timeout-replace" keeps that replacement under the new judgment.
TimeoutReplace ==
    /\ Host = "tmux" /\ Alive /\ Natives = {} /\ r.inc < MaxInc /\ r.stop # "asked"
    /\ \/ Mode = "before" /\ r.wait # 0 /\ ~ScreenIdle /\ r' = ArgvLaunch(r.wait)
       \/ Mode = "after-timeout-replace" /\ r.head = 0 /\ ~Ready
          /\ \E i \in First : r' = ArgvLaunch(i)
\* tmux, a warm fallback that is not about readiness (warm_followup.rs:92~:98). The Discord intake
\* pre-check queues the input while it reads busy; otherwise the pane is replaced, before and after.
\* The pre-check and the replacement are one step here; the gap between them is not modelled.
\* "after-guard-all" asks for Gone or stage 4 on every reason (the d2 wording), so an idle pane
\* with changed options is never replaced.
ConfigReplace ==
    /\ Host = "tmux" /\ r.mismatch /\ r.head = 0 /\ r.wait = 0 /\ Alive /\ Natives = {}
    /\ r.inc < MaxInc /\ r.stop # "asked"
    /\ \/ Mode = "before" /\ ~LenientBusy
       \/ After /\ Mode # "after-guard-all" /\ ~BusyVerdict
    /\ \E i \in First : r' = ArgvLaunch(i)
Prepare ==
    /\ r.head = 0 /\ ~r.mismatch
    /\ \E i \in First :
        /\ Ready
        /\ ~(Mode = "before" /\ Host = "tmux") \/ r.wait = i
        /\ r' = [r EXCEPT !.head = i, !.wait = 0, !.proofInc = r.inc, !.proofSeen = r.seen]
\* Under the composer lock the proof is read again right before the first key.
Submit ==
    /\ r.head # 0
    /\ LET i == r.head
           proven == Mode = "after-no-revalidate"
                     \/ (Ready /\ r.proofInc = r.inc /\ r.proofSeen = r.seen)
           sent == [r EXCEPT !.head = 0, !.effects[i] = @ + 1, !.q[i] = "sent"]
       IN r' = IF ~proven THEN [r EXCEPT !.head = 0]
               ELSE CASE r.phase = "Idle" ->
                           [NewTurn(i) EXCEPT !.head = 0, !.effects[i] = @ + 1, !.q[i] = "sent"]
                      [] r.phase = "Running" -> [sent EXCEPT !.badEarly = TRUE]
                      [] OTHER -> [sent EXCEPT !.badShell = TRUE]
\* The unchanged busy entry: one paste and Enter with a nonce receipt, steering or starting a turn.
\* It reads the rollout's open turn; the new judgment sends an input here only without a Stop hook.
BusyDeliver ==
    /\ BusyRoute /\ r.head = 0 /\ r.wait = 0 /\ Alive /\ r.started
    /\ r.file = "plain" /\ r.rec = "started" /\ (Mode = "before" \/ ~r.hook)
    /\ \E i \in First :
        r' = IF r.phase = "Running"
             THEN [r EXCEPT !.effects[i] = @ + 1, !.q[i] = "native"]
             ELSE [NewTurn(i) EXCEPT !.effects[i] = @ + 1, !.q[i] = "sent"]
\* What stage 4 can observe. Neither tells an ended turn from a running one that is silent:
\* the rollout has not grown for the quiet window, and the provider process has no child.
QuietObserved == r.phase = "Idle" \/ (r.phase = "Running" /\ ~r.pending /\ Silent # "none")
NoChildObserved == r.phase = "Idle" \/ (r.phase = "Running" /\ Silent = "model")
\* UserPromptSubmit evidence with no readable task_started expires under the stage 4 guard, and the
\* judgment is then made from the rollout alone. "after-ups-until-end" keeps it until an end signal.
UpsExpire ==
    /\ r.ups /\ Mode # "after-ups-until-end" /\ Alive /\ r.head = 0
    /\ QuietObserved /\ (Mode = "after-no-child" \/ NoChildObserved)
    /\ r' = [r EXCEPT !.ups = FALSE]
\* Stage 4 opens on an unreadable source only; "after-reclaim-open" also opens it on an open turn.
Stuck ==
    /\ After /\ Mode # "after-no-fallback" /\ Alive /\ r.started /\ ~r.hook
    /\ Host = "tmux" \/ HerdrReplace
    /\ QuietObserved /\ (Mode = "after-no-child" \/ NoChildObserved)
    /\ \/ Level = "Unknown" /\ ~r.ups
       \/ Mode = "after-reclaim-open" /\ Level = "Open" /\ ~BusyRoute
\* A new incarnation: the process is gone, or no signal can ever prove the end.
Relaunch ==
    /\ r.inc < MaxInc /\ r.head = 0 /\ Natives = {}
    /\ r.phase = "Exited" \/ Stuck
    /\ \E i \in First :
        r' = IF Host = "tmux" THEN ArgvLaunch(i)
             ELSE [r EXCEPT !.inc = @ + 1, !.phase = "Booting", !.started = FALSE,
                            !.rec = IF Mode = "after-no-floor" THEN @ ELSE "none",
                            !.pending = FALSE, !.late = FALSE, !.file = "plain",
                            !.hook = FALSE, !.ups = FALSE, !.owner = 0, !.wait = 0, !.mismatch = FALSE,
                            !.badKill = @ \/ r.phase = "Running",
                            !.q = [j \in Ids |-> IF j = r.owner /\ r.q[j] = "sent"
                                                 THEN "done" ELSE r.q[j]]]

\* ---- /stop ----
StopAsk ==
    /\ r.stops > 0 /\ r.stop = "none" /\ r.phase = "Running" /\ r.owner # 0
    /\ r' = [r EXCEPT !.stops = @ - 1, !.stop = "asked", !.stopTurn = r.turn]
StopJudge ==
    /\ r.stop = "asked"
    /\ LET own == r.turn = r.stopTurn
           open == own /\ r.phase = "Running"
           running == IF Mode \in {"before", "after-stop-screen"} THEN Screen = "ok" /\ open
                      ELSE Alive /\ Level = "Open" /\ own /\ ~r.hook
           unobserved == After /\ Mode # "after-stop-screen" /\ Level = "Unknown"
       IN r' = CASE running /\ open ->
                       [r EXCEPT !.stop = "sent", !.phase = "Idle", !.late = TRUE,
                                 !.q = [i \in Ids |-> IF i = r.owner \/ r.q[i] = "native"
                                                      THEN "done" ELSE r.q[i]]]
                 [] running /\ ~open -> [r EXCEPT !.stop = "sent", !.badStopKey = TRUE]
                 [] unobserved -> [r EXCEPT !.stop = "unobserved"]
                 [] OTHER -> [r EXCEPT !.stop = "idle", !.badStopIdle = open]

\* Nothing happens; a stuck input is caught by the delivery property, not by a deadlock.
Wait == UNCHANGED r
Next == Boot \/ PersonEnter \/ RecordStart \/ PersonBlocked \/ Finish \/ WriteLate \/ CompressRollout
        \/ Exit \/ Restart \/ ConfigChange \/ Admit \/ TimeoutReplace \/ ConfigReplace \/ Prepare
        \/ Submit \/ BusyDeliver \/ UpsExpire \/ Relaunch \/ StopAsk \/ StopJudge \/ Wait
Spec == Init /\ [][Next]_r
        /\ WF_r(Boot) /\ WF_r(RecordStart) /\ WF_r(Finish) /\ WF_r(WriteLate)
        /\ WF_r(Admit) /\ WF_r(TimeoutReplace) /\ WF_r(Prepare) /\ WF_r(Submit)
        /\ WF_r(BusyDeliver) /\ WF_r(Relaunch) /\ WF_r(StopJudge)
        /\ WF_r(ConfigReplace) /\ WF_r(UpsExpire)

\* ---- rules ----
CodexTypeOK ==
    /\ Mode \in Modes /\ Host \in {"tmux", "herdr"} /\ Screen \in {"ok", "unread"}
    /\ Loss \in {"none", "record", "hook", "both", "unnamed", "unnamed-both"}
    /\ Silent \in {"none", "tool", "model"}
    /\ Host = "herdr" => Config = 0
    /\ r.phase \in {"Booting", "Idle", "Running", "Exited"}
    /\ r.rec \in {"none", "started", "ended", "unknown"}
    /\ r.inc \in 1..MaxInc /\ r.head \in 0..N /\ r.wait \in 0..N
    /\ \A i \in Ids : r.q[i] \in {"queued", "sent", "native", "done"}
\* No Idle-route key reaches a running turn.
CodexNoEarlyInput == ~r.badEarly
\* No key reaches a pane whose Codex has not booted or has exited.
CodexNoShellInput == ~r.badShell
CodexNoDuplicate == \A i \in Ids : r.effects[i] <= 1
CodexNoSilentLoss == \A i \in Ids : r.q[i] # "queued" => r.effects[i] = 1
\* A replacement or relaunch never kills a running turn.
CodexNoRunningKill == ~r.badKill
\* /stop never answers "nothing is running" while its own turn runs.
CodexStopNoFalseIdle == ~r.badStopIdle
\* The stop key is written only while its own turn is open.
CodexStopKeyOnOpenTurn == ~r.badStopKey
\* A missing or late signal never holds or refuses an input for good.
CodexPendingEventuallyDelivered == \A i \in Ids : <>(r.effects[i] = 1)
=============================================================================
