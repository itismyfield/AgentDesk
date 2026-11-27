---------------------------- MODULE WriterIdle ----------------------------
EXTENDS Naturals

\* Publishing actor idle wait. One event may arrive on either side of any timer reset.
CONSTANT IdleMode
VARIABLE idle
idleEvents == {"input", "hook", "command", "spool", "transcript"}
idleSignals == {"input", "hook", "command"}
IdleInit == idle = [delay |-> 1, left |-> 1, event |-> "none",
                    pending |-> FALSE, age |-> 0, wake |-> FALSE]
IdleArrive ==
    /\ idle.event = "none"
    /\ \E e \in idleEvents:
        idle' = [idle EXCEPT !.event = e, !.pending = TRUE,
                 !.wake = (IdleMode # "before" /\ e \in idleSignals)]
IdleClock ==
    /\ idle.left > 0 /\ ~idle.wake
    /\ idle' = [idle EXCEPT !.left = @ - 1,
                !.age = IF idle.pending THEN @ + 1 ELSE @]
IdlePoll ==
    /\ idle.wake \/ (idle.left = 0 /\ (IdleMode # "no-max-timer" \/ idle.delay # 15))
    /\ LET delay == IF idle.pending \/ IdleMode = "before" THEN 1
                    ELSE IF idle.delay = 1 THEN 5 ELSE 15
       IN idle' = [idle EXCEPT !.delay = delay, !.left = delay,
                   !.pending = FALSE, !.wake = FALSE]
IdleNext == IdleArrive \/ IdleClock \/ IdlePoll
IdleSpec == IdleInit /\ [][IdleNext]_idle /\ WF_idle(IdleClock) /\ WF_idle(IdlePoll)
IdleEventuallyProcesses == idle.pending ~> ~idle.pending
IdlePromptWake == idle.event \in idleSignals => idle.age = 0
IdleWithinMax == idle.age <= 15

=============================================================================
