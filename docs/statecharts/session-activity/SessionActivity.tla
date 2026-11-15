-------------------------- MODULE SessionActivity --------------------------
\* Production baseline 5c3ed37cddc80036fa8a274419b0219f49926704.
\* Evidence-snapshot safety is distinct from physical read-to-key timing.
EXTENDS Naturals, FiniteSets
CONSTANTS Mode, Scenario
VARIABLE r
Proposed == Mode # "current"
Readable == Mode # "permanent-unread"
Pair == Scenario = "pair"
Background == Scenario = "background"
Race == Scenario = "physical-race"
Restarting == Scenario = "restart"
InitialActual == IF Scenario = "missing" THEN "idle" ELSE IF Pair THEN "tool" ELSE IF Background THEN "idle" ELSE IF Scenario = "submitted" THEN "submitted" ELSE "generation"
Normalize(a) == IF a \in {"tool", "generation", "submitted"} THEN "busy" ELSE "idle"
Initial == [mode |-> Mode, actual |-> InitialActual, transcript |-> Normalize(InitialActual),
            presence |-> IF Scenario = "missing" THEN "missing" ELSE "present",
            observed |-> "unknown", epoch |-> 1, checkedEpoch |-> 0,
            pre |-> FALSE, post |-> FALSE, closed |-> FALSE, open |-> FALSE,
            preLost |-> FALSE, postLost |-> FALSE, submitHook |-> FALSE,
            stopHook |-> FALSE, expired |-> FALSE, didExpire |-> FALSE, submitLost |-> FALSE, terminalPending |-> FALSE,
            actualBg |-> Background, bg |-> IF Background THEN "pending" ELSE "none",
            childChecked |-> FALSE, child |-> FALSE, subStopped |-> FALSE,
            oldHook |-> FALSE, restarted |-> FALSE, watcher |-> FALSE,
            stopRead |-> FALSE, stopAllowed |-> FALSE, stopActual |-> "idle", stopEpoch |-> 0, stopProof |-> "unknown", esc |-> FALSE,
            stopSent |-> FALSE, badEvidence |-> FALSE, badPhysical |-> FALSE,
            cleaned |-> FALSE, badKill |-> FALSE]
vars == <<r>>
Init == r = Initial

\* Native user submission is not proof of generation; it only wakes observation.
UserPromptSubmit ==
    /\ ~r.submitHook /\ ~r.submitLost /\ ~Background
    /\ r' = [r EXCEPT !.submitHook = TRUE]
LoseSubmit ==
    /\ ~r.submitHook /\ ~r.submitLost
    /\ r' = [r EXCEPT !.submitLost = TRUE]

\* One tool_use_id in one source/epoch: delivery order need not match execution.
PreToolUse ==
    /\ Pair /\ ~r.pre /\ ~r.preLost
    /\ r' = [r EXCEPT !.pre = TRUE,
                     !.open = IF Proposed /\ Mode # "no-tombstone" THEN ~r.closed ELSE TRUE]
LosePreToolUse ==
    /\ Pair /\ ~r.pre /\ ~r.preLost
    /\ r' = [r EXCEPT !.preLost = TRUE]
ToolFinishes ==
    /\ Pair /\ r.actual = "tool"
    /\ r' = [r EXCEPT !.actual = "idle", !.transcript = "idle"]
PostToolUse ==
    /\ Pair /\ r.actual = "idle" /\ ~r.post /\ ~r.postLost
    /\ r' = [r EXCEPT !.post = TRUE, !.closed = TRUE, !.open = FALSE]
LosePostToolUse ==
    /\ Pair /\ r.actual = "idle" /\ ~r.post /\ ~r.postLost
    /\ r' = [r EXCEPT !.postLost = TRUE]

\* A matching terminal cursor closes unresolved foreground pairs only after
\* re-observation; silence/timeout by itself never grants idle authority.
ObserveActivity ==
    /\ r' = [r EXCEPT !.observed = IF ~Readable THEN "unknown" ELSE IF Proposed THEN IF r.expired /\ r.transcript = "busy" THEN "unknown" ELSE r.transcript ELSE "unknown",
                     !.checkedEpoch = IF Readable THEN r.epoch ELSE @,
                     !.open = IF Proposed /\ r.transcript = "idle" THEN FALSE ELSE @,
                     !.closed = IF Proposed /\ r.transcript = "idle" THEN TRUE ELSE @]
StopHook ==
    /\ r.transcript = "idle" /\ ~r.stopHook
    /\ r' = [r EXCEPT !.stopHook = TRUE]
Timeout ==
    /\ ~r.didExpire
    /\ r' = [r EXCEPT !.expired = TRUE, !.didExpire = TRUE,
                     !.observed = IF Proposed THEN "unknown" ELSE @,
                     !.childChecked = FALSE]

\* Fresh source progress, never rereading the same tail, repairs an expired lease.
ProgressArrival ==
    /\ r.expired /\ r.transcript = "busy" /\ Mode # "no-lease-recovery" /\ Readable
    /\ r' = [r EXCEPT !.expired = FALSE]

\* A subagent is not a foreground generation. SubagentStop is child-local;
\* an exact child terminal repairs a lost hook; process presence only vetoes.
ProbeChildPresence ==
    /\ Background /\ r.actualBg /\ ~r.child
    /\ r' = [r EXCEPT !.childChecked = TRUE, !.child = TRUE]
ObserveExactChildTerminal ==
    /\ Background /\ ~r.actualBg
    /\ r' = [r EXCEPT !.bg = "none", !.child = FALSE]
BackgroundFinishes ==
    /\ Background /\ r.actualBg
    /\ r' = [r EXCEPT !.actualBg = FALSE]
SubagentStop ==
    /\ Background /\ ~r.actualBg /\ ~r.subStopped
    /\ r' = [r EXCEPT !.subStopped = TRUE,
                     !.bg = IF Proposed THEN "none" ELSE @]

\* An old incarnation may wake but must not set new activity or tool authority.
OldIncarnationHook ==
    /\ ~r.oldHook
    /\ r' = [r EXCEPT !.oldHook = TRUE]
Restart ==
    /\ Restarting /\ ~r.restarted
    /\ r' = [r EXCEPT !.restarted = TRUE, !.epoch = 2,
                     !.checkedEpoch = 0, !.observed = "unknown",
                     !.open = FALSE, !.closed = FALSE, !.childChecked = FALSE,
                     !.bg = "unknown", !.watcher = FALSE]

\* Watcher and presence consume the same source-scoped activity snapshot.
WatcherReacquire ==
    /\ ~r.watcher /\ r.observed = "busy" /\ r.checkedEpoch = r.epoch
    /\ r' = [r EXCEPT !.watcher = TRUE]

\* Stop re-reads current transcript under delivery locks, then sends separately.
\* Physical completion may occur between these actions; see PhysicalRace cfg.
StopRead ==
    /\ ~r.stopRead
    /\ r' = [r EXCEPT !.stopRead = TRUE,
                     !.stopAllowed = Readable /\ (Proposed \/ Race) /\ r.transcript = "busy" /\ ~r.expired /\
                         ((~Pair /\ ~Background /\ Scenario # "submitted") \/ (Pair /\ r.open /\ ~r.closed)),
                     !.stopActual = r.actual, !.stopEpoch = r.epoch,
                     !.stopProof = IF Pair /\ r.open /\ ~r.closed THEN "tool"
                                  ELSE IF Scenario # "submitted" /\ ~Background /\ r.transcript = "busy" THEN "generation"
                                  ELSE "unknown"]
ProviderFinishesBetweenReadAndSend ==
    /\ Race /\ r.stopRead /\ ~r.stopSent /\ r.actual = "generation"
    /\ r' = [r EXCEPT !.actual = "idle", !.transcript = "idle"]
AllowedAtSend == r.stopAllowed /\ r.stopEpoch = r.epoch /\ ~r.expired /\
                 (~Pair \/ (r.open /\ ~r.closed))
StopSend ==
    /\ r.stopRead /\ ~r.stopSent
    /\ r' = [r EXCEPT !.stopSent = TRUE, !.esc = AllowedAtSend,
                     !.badEvidence = @ \/ (AllowedAtSend /\ r.stopProof \notin {"generation", "tool"}),
                     !.badPhysical = @ \/ (AllowedAtSend /\ r.actual \notin {"generation", "tool"})]

\* Screen-free evidence cannot prove absence of every modal/in-process task.
\* Present Claude TUI sessions are kept; only existing Missing row cleanup proceeds.
IdleCleanup ==
    /\ ~r.cleaned /\ r.transcript = "idle"
    /\ IF Mode = "live-kill" THEN TRUE
       ELSE IF Proposed THEN r.presence = "missing"
                        ELSE TRUE
    /\ r' = [r EXCEPT !.cleaned = TRUE,
                     !.badKill = @ \/ (r.actual # "idle" \/ r.actualBg)]

GenerationFinishes ==
    /\ Scenario = "finish" /\ r.actual = "generation"
    /\ r' = [r EXCEPT !.actual = "idle", !.terminalPending = TRUE]
TerminalArrives ==
    /\ r.terminalPending
    /\ r' = [r EXCEPT !.transcript = "idle", !.terminalPending = FALSE]

Next == UserPromptSubmit \/ LoseSubmit \/ PreToolUse \/ LosePreToolUse \/
        ToolFinishes \/ PostToolUse \/ LosePostToolUse \/ ObserveActivity \/
        StopHook \/ Timeout \/ ProgressArrival \/ ProbeChildPresence \/ ObserveExactChildTerminal \/ BackgroundFinishes \/ SubagentStop \/
        OldIncarnationHook \/ Restart \/ WatcherReacquire \/ StopRead \/
        ProviderFinishesBetweenReadAndSend \/ StopSend \/ IdleCleanup \/ GenerationFinishes \/ TerminalArrives
Fairness == WF_vars(ObserveActivity) /\ WF_vars(ObserveExactChildTerminal) /\ WF_vars(ProbeChildPresence) /\
            WF_vars(Timeout) /\ WF_vars(ProgressArrival) /\ WF_vars(WatcherReacquire) /\
            WF_vars(ToolFinishes) /\ WF_vars(BackgroundFinishes) /\
            WF_vars(GenerationFinishes) /\ WF_vars(TerminalArrives) /\ WF_vars(Restart) /\ WF_vars(StopRead) /\ WF_vars(StopSend) /\ WF_vars(IdleCleanup)
Spec == Init /\ [][Next]_vars /\ Fairness
TypeOK == r.actual \in {"idle", "submitted", "generation", "tool"} /\
          r.transcript \in {"idle", "busy"} /\ r.observed \in {"unknown", "idle", "busy"} /\
          r.epoch \in 1..2 /\ r.checkedEpoch \in 0..2 /\
          r.bg \in {"unknown", "pending", "none"}
NoEscWithoutCurrentEvidence == ~r.badEvidence
NeverEscape == ~r.esc
SubagentStopClosesChild == r.subStopped => r.bg = "none"
NoUnreadEscape == ~Readable => ~r.esc
NoSubmittedEscape == (Scenario = "submitted") => ~r.esc
NoBackgroundEscape == Background => ~r.esc
NoEscUnlessGenerationOrForegroundTool == ~r.badPhysical
NoIdleKillWhileBusy == ~r.badKill
NoLatePreReopensClosedTool == Proposed => ~(r.closed /\ r.open)
EventuallyConverges == <>[](r.observed = Normalize(r.actual) /\ r.checkedEpoch = r.epoch)
EventuallyReacquires == (~Pair /\ ~Background /\ ~Race) => <>(r.watcher)
BackgroundEventuallySettles == Background => <>[](r.bg = "none")
MissingEventuallyCleanup == (Scenario = "missing") => <>(r.cleaned)
NoPresentIdleCleanup == (Proposed /\ Mode # "live-kill" /\ r.presence = "present") => ~r.cleaned
=============================================================================
