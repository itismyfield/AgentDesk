---------------------------- MODULE WriterIdle ----------------------------
EXTENDS Naturals

\* One event can arrive on either side of any quiescent observation.
CONSTANT IdleMode
VARIABLE idle
idleEvents == {"input", "hook", "command", "spool", "transcript"}
idleSignals == {"input", "hook"}
historicalBackoff == IdleMode \in {"backoff", "no-max-timer"}
IdleInit == idle = [delay |-> 1, left |-> 1, event |-> "none",
                    pending |-> FALSE, age |-> 0, wake |-> FALSE]
IdleArrive ==
    /\ idle.event = "none"
    /\ \E e \in idleEvents:
        idle' = [idle EXCEPT !.event = e, !.pending = TRUE,
                 !.wake = (IdleMode # "before" /\
                           (e \in idleSignals \/ (historicalBackoff /\ e = "command")))]
IdleClock ==
    /\ idle.left > 0 /\ ~idle.wake
    /\ idle' = [idle EXCEPT !.left = @ - 1,
                !.age = IF idle.pending THEN @ + 1 ELSE @]
IdleReady ==
    idle.wake \/ (idle.left = 0 /\ IdleMode # "no-timer" /\
                 (IdleMode # "no-max-timer" \/ idle.delay # 15))
IdleObserve ==
    LET delay == IF idle.pending \/ ~historicalBackoff THEN 1
                    ELSE IF idle.delay = 1 THEN 5 ELSE 15
    IN idle' = [idle EXCEPT !.delay = delay, !.left = delay,
                   !.pending = FALSE, !.wake = FALSE]
IdlePoll == IdleReady /\ (idle.pending \/ IdleMode = "before") /\ IdleObserve
IdleSkip == IdleReady /\ ~idle.pending /\ IdleMode # "before" /\ IdleObserve
IdleNext == IdleArrive \/ IdleClock \/ IdlePoll \/ IdleSkip
IdleSpec == IdleInit /\ [][IdleNext]_idle /\
            WF_idle(IdleClock) /\ WF_idle(IdlePoll) /\ WF_idle(IdleSkip)
IdleEventuallyProcesses == idle.pending ~> ~idle.pending
IdlePromptWake == idle.event \in idleSignals => idle.age = 0
IdleWithinMax == idle.age <= 15
IdleWithinOne == idle.age <= 1

=============================================================================
