--------------------------- MODULE RoutineHerdrStart ---------------------------
\* One routine or scheduled start on its turn channel: which driver takes it, and how often.
EXTENDS Naturals
CONSTANTS Host, Backend, Identity, Policy

VARIABLE s
vars == <<s>>
Terminal == {"succeeded", "failed", "interrupted"}

\* Host: the session host configured for the turn channel. Backend: the run's pinned backend.
\* Identity: "channel" when the start runs under the channel's own session key, "own" when a
\* fresh routine or a scheduled snapshot names its own key and pane.
Init == \E busy \in BOOLEAN:
    s = [run |-> "claimed", driver |-> "none", reason |-> "none", starts |-> 0, closes |-> 0,
         hostReads |-> 0, execs |-> IF Host = "herdr" THEN 1 ELSE 0,
         humanBusy |-> busy, blind |-> FALSE]

Tui == Backend = "tui"
Close(run, reason) == [s EXCEPT !.run = run, !.reason = reason, !.closes = @ + 1]
Begin(driver) == [s EXCEPT !.run = "started", !.driver = driver, !.starts = @ + 1]

\* The owner takes a TUI start and runs it on its wake; a process start, and a TUI start no owner
\* holds (Policy "built_unowned"), keep the launch path.
Owned == Tui /\ Policy # "built_unowned"
Admit == s.run = "claimed" /\ s' = [s EXCEPT !.run = IF Owned THEN "accepted" ELSE "judging"]
OwnerWake == s.run = "accepted" /\ s' = [s EXCEPT !.run = "judging"]

\* An unconfigured channel passes with no host read: its tmux pane or the process driver.
JudgeTmux ==
    /\ s.run = "judging" /\ Host = "tmux"
    /\ s' = Begin(IF Tui THEN "tmux_pane" ELSE "process")

\* Before: every routine or scheduled start on a configured channel is refused as not wired.
JudgeBefore ==
    /\ s.run = "judging" /\ Host = "herdr" /\ Policy = "before"
    /\ s' = [Close("failed", "executor_not_wired") EXCEPT !.hostReads = @ + 1]

\* Gate only: the start passes as an intake does and the unchanged driver hands it to Herdr. Its
\* wait still reads a tmux pane, so it reaches the driver without having read a busy execution.
JudgeGateOnly ==
    /\ s.run = "judging" /\ Host = "herdr" /\ Policy = "gate_only"
    /\ LET own == Identity = "own" \/ ~Tui
       IN s' = [Begin(IF own THEN "herdr_own" ELSE "herdr_channel") EXCEPT
                  !.hostReads = @ + 1, !.execs = IF own THEN @ + 1 ELSE @,
                  !.blind = ~own /\ s.humanBusy]

\* Built: a channel-key TUI start its owner holds waits for the execution's idle and runs there;
\* every other routine or scheduled start keeps the refusal it had.
Built == Policy \in {"built", "built_unowned"}
JudgeBuilt ==
    /\ s.run = "judging" /\ Host = "herdr" /\ Built
    /\ IF Owned /\ Identity = "channel"
       THEN /\ ~s.humanBusy
            /\ s' = [Begin("herdr_channel") EXCEPT !.hostReads = @ + 1]
       ELSE s' = [Close("failed", "executor_not_wired") EXCEPT !.hostReads = @ + 1]

\* Routed: backend and identity pick the driver; Policy names what an own-key TUI start gets.
JudgeRouted ==
    /\ s.run = "judging" /\ Host = "herdr" /\ ~Built /\ Policy \notin {"before", "gate_only"}
    /\ IF ~Tui THEN s' = Begin("process")
       ELSE IF Identity = "channel" \/ Policy = "own_into_channel"
       THEN /\ ~s.humanBusy
            /\ s' = [Begin("herdr_channel") EXCEPT !.hostReads = @ + 1]
       ELSE CASE Policy = "own_refused" ->
                   s' = [Close("failed", "own_session_unsupported") EXCEPT !.hostReads = @ + 1]
              [] Policy = "own_second_execution" ->
                   s' = [Begin("herdr_own") EXCEPT !.hostReads = @ + 1, !.execs = @ + 1]
              [] OTHER -> s' = [Begin("process") EXCEPT !.hostReads = @ + 1]

HumanFinish == s.humanBusy /\ s' = [s EXCEPT !.humanBusy = FALSE]
Complete == s.run = "started" /\ s' = Close("succeeded", "none")
Fail == s.run = "started" /\ \E why \in {"provider_error", "timeout"}: s' = Close("failed", why)
\* A start its deferral window or deadline gives up on closes failed, unsent.
GiveUp == s.run \in {"accepted", "judging"} /\ s' = Close("failed", "deadline")
\* Boot recovery withdraws an unsent start and interrupts the run; RoutineRun holds the detail.
Restart == s.run \in {"accepted", "judging", "started"} /\ s' = Close("interrupted", "restart")
Done == s.run \in Terminal /\ UNCHANGED s

Judge == JudgeTmux \/ JudgeBefore \/ JudgeGateOnly \/ JudgeBuilt \/ JudgeRouted
Finish == Complete \/ Fail
Next == Admit \/ OwnerWake \/ Judge \/ HumanFinish \/ Finish \/ GiveUp \/ Restart \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Admit) /\ WF_vars(OwnerWake) /\ WF_vars(Judge)
        /\ WF_vars(HumanFinish) /\ WF_vars(Finish)

NotRefusedAsUnwired == s.reason # "executor_not_wired"
StartedAtMostOnce == s.starts <= 1
ClosedAtMostOnce == s.closes <= 1
TmuxUnchanged == Host = "tmux" =>
    (s.hostReads = 0 /\ s.driver \in {"none", IF Tui THEN "tmux_pane" ELSE "process"})
OneExecutionPerChannel == s.execs <= 1
OwnSessionIsolated == (Identity = "own" \/ ~Tui) => s.driver # "herdr_channel"
ProcessStaysProcess == ~Tui => s.driver \in {"none", "process"}
BackendPinned == Tui => s.driver # "process"
NoBlindStart == ~s.blind
\* Reachability witness: broken exactly where a configured channel's run can end succeeded.
SuccessUnreachable == ~(Host = "herdr" /\ s.run = "succeeded")
EveryRunCloses == <>(s.run \in Terminal)
=============================================================================
