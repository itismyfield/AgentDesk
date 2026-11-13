-------------------------- MODULE CodexReadiness --------------------------
\* Codex ready / busy / end / stop judgment (#8082 design d1).
\* Production code baseline 082c77aa4e06b81c76b614f55b8fed73ce01923f.
\* "before" reads the pane; "after" reads the bound rollout, the Stop hook and the process.
\* One Codex pane, N queued inputs, at most one person turn typed straight into the TUI.
EXTENDS Naturals, FiniteSets
CONSTANTS
    N,          \* queued inputs
    Mode,       \* judgment variant, see Modes
    Screen,     \* "ok" | "unread": a narrow pane or a new status line is unread for good
    Loss,       \* which end signal of the first ended turn never arrives
    Compress,   \* the idle rollout may be archived to .zst
    Restarts,   \* dcserver restarts (in-memory latch and hook buffer are lost)
    Person,     \* turns a person starts in the TUI
    Exits,      \* provider process exits
    BusyRoute,  \* the existing busy entry may deliver into an open turn
    Silent,     \* a running tool writes nothing to the rollout for a long time
    Stops       \* /stop requests on an owned running turn
VARIABLE r

Ids == 1..N
Modes == {"before", "after", "after-latch", "after-no-proc", "after-no-revalidate",
          "after-no-fallback", "after-no-floor", "after-no-child", "after-stop-screen"}
After == Mode # "before"
MaxInc == 4

Initial ==
    [mode |-> Mode, screen |-> Screen,  \* constants, kept in the record for the chart labels
     inc |-> 1, phase |-> "Booting", started |-> FALSE,
     turn |-> 0, rec |-> "none", late |-> FALSE, file |-> "plain",
     latch |-> FALSE, hook |-> FALSE, owner |-> 0,
     q |-> [i \in Ids |-> "queued"], effects |-> [i \in Ids |-> 0],
     head |-> 0, proofInc |-> 0, proofTurn |-> 0,
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
\* herdr process_info / tmux pane liveness names this incarnation's provider process;
\* "started" is that reading, not a SessionStart hook.
Alive == r.phase \in {"Idle", "Running"}
ProcOK == Mode = "after-no-proc" \/ (Alive /\ r.started)
\* A readable pane shows the truth; an unread pane proves nothing.
ScreenIdle == Screen = "ok" /\ r.phase = "Idle"
HookTier == Mode # "after-no-fallback" /\ r.hook
ReadyAfter ==
    /\ ProcOK
    /\ \/ Level \in {"Ended", "NoStart"}
       \/ Level \in {"Open", "Unknown"} /\ HookTier
Ready ==
    CASE Mode = "before" -> ScreenIdle
      [] Mode = "after-latch" -> r.latch \/ ReadyAfter
      [] OTHER -> ReadyAfter
First == {i \in Ids : r.q[i] = "queued" /\ \A j \in 1..(i-1) : r.q[j] # "queued"}
Natives == {i \in Ids : r.q[i] = "native"}
NewTurn(i) ==
    [r EXCEPT !.phase = "Running", !.turn = @ + 1, !.rec = "started", !.late = FALSE,
              !.hook = FALSE, !.owner = i]

\* ---- provider ----
\* The provider process becomes the pane's foreground execution and its composer takes keys.
\* Codex reports SessionStart only with the first prompt, so no hook marks this step.
Boot ==
    /\ r.phase = "Booting"
    /\ r' = [r EXCEPT !.phase = "Idle", !.started = TRUE]
PersonStart ==
    /\ r.person > 0 /\ r.phase = "Idle" /\ ~r.late
    /\ r' = [NewTurn(0) EXCEPT !.person = @ - 1]
\* The turn ends in the TUI. Its terminal record is late, lost, or unnamed; its Stop hook may be lost.
Finish ==
    /\ r.phase = "Running"
    /\ LET lossy == r.lossLeft > 0
           hookSeen == ~(lossy /\ Loss \in {"hook", "both", "unnamed-both"})
           ended == [r EXCEPT !.phase = "Idle", !.hook = hookSeen,
                              !.lossLeft = IF lossy THEN @ - 1 ELSE @,
                              !.q = [i \in Ids |-> IF i = r.owner \/ r.q[i] = "native"
                                                   THEN "done" ELSE r.q[i]]]
       IN r' = CASE lossy /\ Loss \in {"record", "both"} -> ended
                 [] lossy /\ Loss \in {"unnamed", "unnamed-both"} -> [ended EXCEPT !.rec = "unknown"]
                 [] OTHER -> [ended EXCEPT !.late = TRUE]
\* The late terminal record reaches the file; a following tail latches composer_ready.
WriteLate ==
    /\ r.late
    /\ r' = [r EXCEPT !.late = FALSE, !.rec = "ended", !.latch = TRUE]
\* Codex CLI archives an old idle rollout; by then the hook buffer has expired.
CompressRollout ==
    /\ r.compress > 0 /\ r.phase = "Idle" /\ ~r.late /\ r.file = "plain" /\ r.head = 0
    /\ r' = [r EXCEPT !.compress = @ - 1, !.file = "zst", !.hook = FALSE]
Exit ==
    /\ r.exits > 0 /\ Alive /\ Natives = {} /\ r.stop # "asked"
    /\ r' = [r EXCEPT !.exits = @ - 1, !.phase = "Exited",
                     !.q = [i \in Ids |-> IF i = r.owner /\ r.q[i] = "sent" THEN "done" ELSE r.q[i]]]

\* ---- AgentDesk ----
Restart ==
    /\ r.restarts > 0
    /\ r' = [r EXCEPT !.restarts = @ - 1, !.latch = FALSE, !.hook = FALSE, !.head = 0]
Prepare ==
    /\ r.head = 0
    /\ \E i \in First :
        /\ Ready
        /\ r' = [r EXCEPT !.head = i, !.proofInc = r.inc, !.proofTurn = r.turn]
\* Under the composer lock the proof is read again right before the first key.
Submit ==
    /\ r.head # 0
    /\ LET i == r.head
           proven == Mode = "after-no-revalidate"
                     \/ (Ready /\ r.proofInc = r.inc /\ r.proofTurn = r.turn)
           sent == [r EXCEPT !.head = 0, !.effects[i] = @ + 1, !.q[i] = "sent"]
       IN r' = IF ~proven THEN [r EXCEPT !.head = 0]
               ELSE CASE r.phase = "Idle" ->
                           [NewTurn(i) EXCEPT !.head = 0, !.effects[i] = @ + 1, !.q[i] = "sent"]
                      [] r.phase = "Running" -> [sent EXCEPT !.badEarly = TRUE]
                      [] OTHER -> [sent EXCEPT !.badShell = TRUE]
\* The unchanged busy entry: one paste and Enter with a nonce receipt, steering or starting a turn.
BusyDeliver ==
    /\ BusyRoute /\ r.head = 0 /\ Alive /\ r.started
    /\ r.file = "plain" /\ r.rec = "started" /\ ~r.hook
    /\ \E i \in First :
        r' = IF r.phase = "Running"
             THEN [r EXCEPT !.effects[i] = @ + 1, !.q[i] = "native"]
             ELSE [NewTurn(i) EXCEPT !.effects[i] = @ + 1, !.q[i] = "sent"]
\* A quiet rollout with no tool child; "after-no-child" skips the child check.
Quiet == r.phase = "Idle" \/ (Silent /\ Mode = "after-no-child" /\ r.phase = "Running")
Stuck ==
    /\ After /\ Mode # "after-no-fallback" /\ Alive /\ r.started /\ ~r.hook /\ Quiet
    /\ \/ Level = "Unknown"
       \/ Level = "Open" /\ ~BusyRoute
\* A new incarnation: the process is gone, or no signal can ever prove the end.
Relaunch ==
    /\ r.inc < MaxInc /\ r.head = 0 /\ Natives = {}
    /\ r.phase = "Exited" \/ Stuck
    /\ \E i \in Ids : r.q[i] = "queued"
    /\ r' = [r EXCEPT !.inc = @ + 1, !.phase = "Booting", !.started = FALSE,
                     !.rec = IF Mode = "after-no-floor" THEN @ ELSE "none",
                     !.late = FALSE, !.file = "plain", !.hook = FALSE, !.owner = 0,
                     !.badKill = @ \/ r.phase = "Running",
                     !.q = [i \in Ids |-> IF i = r.owner /\ r.q[i] = "sent" THEN "done" ELSE r.q[i]]]

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
Next == Boot \/ PersonStart \/ Finish \/ WriteLate \/ CompressRollout \/ Exit \/ Restart
        \/ Prepare \/ Submit \/ BusyDeliver \/ Relaunch \/ StopAsk \/ StopJudge \/ Wait
Spec == Init /\ [][Next]_r
        /\ WF_r(Boot) /\ WF_r(Finish) /\ WF_r(WriteLate) /\ WF_r(Prepare) /\ WF_r(Submit)
        /\ WF_r(BusyDeliver) /\ WF_r(Relaunch) /\ WF_r(StopJudge)

\* ---- rules ----
CodexTypeOK ==
    /\ Mode \in Modes /\ Screen \in {"ok", "unread"}
    /\ Loss \in {"none", "record", "hook", "both", "unnamed", "unnamed-both"}
    /\ r.phase \in {"Booting", "Idle", "Running", "Exited"}
    /\ r.rec \in {"none", "started", "ended", "unknown"}
    /\ r.inc \in 1..MaxInc /\ r.head \in 0..N
    /\ \A i \in Ids : r.q[i] \in {"queued", "sent", "native", "done"}
\* No Idle-route key reaches a running turn.
CodexNoEarlyInput == ~r.badEarly
\* No key reaches a pane whose Codex has not booted or has exited.
CodexNoShellInput == ~r.badShell
CodexNoDuplicate == \A i \in Ids : r.effects[i] <= 1
CodexNoSilentLoss == \A i \in Ids : r.q[i] # "queued" => r.effects[i] = 1
\* A relaunch never kills a running turn.
CodexNoRunningKill == ~r.badKill
\* /stop never answers "nothing is running" while its own turn runs.
CodexStopNoFalseIdle == ~r.badStopIdle
\* The stop key is written only while its own turn is open.
CodexStopKeyOnOpenTurn == ~r.badStopKey
\* A missing or late signal never holds or refuses an input for good.
CodexPendingEventuallyDelivered == \A i \in Ids : <>(r.effects[i] = 1)
=============================================================================
