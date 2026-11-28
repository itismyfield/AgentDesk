---------------------------- MODULE WriterIdle ----------------------------
EXTENDS Naturals

\* One event can arrive on either side of any quiescent observation.
CONSTANT IdleMode
VARIABLE idle
idleEvents == {"input", "hook", "command", "spool", "transcript",
               "restart", "regrowth", "reader-deadline"}
idleSignals == {"input", "hook", "restart"}
historicalBackoff == IdleMode \in {"backoff", "no-max-timer"}
IdleInit == idle = [delay |-> 1, left |-> 1, event |-> "none",
                    pending |-> FALSE, age |-> 0, wake |-> FALSE,
                    leaseBusy |-> FALSE, observed |-> FALSE, quietWork |-> FALSE]
IdleArrive ==
    /\ idle.event = "none"
    /\ \E e \in idleEvents:
        idle' = [idle EXCEPT !.event = e, !.pending = TRUE,
                 !.leaseBusy = (e = "restart"),
                 !.wake = (IdleMode # "before" /\
                           (e \in idleSignals \/ (historicalBackoff /\ e = "command")))]
IdleClock ==
    /\ idle.left > 0 /\ ~idle.wake
    /\ idle' = [idle EXCEPT !.left = @ - 1,
                !.age = IF idle.pending /\ (~idle.leaseBusy \/ ~idle.observed) /\ @ < 16
                        THEN @ + 1 ELSE @]
IdleRelease ==
    /\ idle.leaseBusy /\ ~idle.wake
    /\ idle' = [idle EXCEPT !.leaseBusy = FALSE, !.age = 0]
IdleReady ==
    idle.wake \/ (idle.left = 0 /\ IdleMode # "no-timer" /\
                 (IdleMode # "no-max-timer" \/ idle.delay # 15))
IdleNeedsWork ==
    \/ IdleMode \in {"before", "rotation-busy"}
    \/ idle.pending /\ (idle.event # "restart" \/ IdleMode # "notice-skip" \/ idle.wake)
IdleObserve(work) ==
    LET delay == IF idle.pending \/ ~historicalBackoff THEN 1
                    ELSE IF idle.delay = 1 THEN 5 ELSE 15
    IN idle' = [idle EXCEPT !.delay = delay, !.left = delay,
                   !.pending = idle.pending /\ (~work \/ idle.leaseBusy),
                   !.observed = @ \/ (work /\ idle.pending),
                   !.quietWork = @ \/ (work /\ ~idle.pending), !.wake = FALSE]
IdlePoll == IdleReady /\ IdleNeedsWork /\ IdleObserve(TRUE)
IdleSkip == IdleReady /\ ~IdleNeedsWork /\ IdleObserve(FALSE)
IdleNext == IdleArrive \/ IdleClock \/ IdlePoll \/ IdleSkip \/ IdleRelease
IdleSpec == IdleInit /\ [][IdleNext]_idle /\
            WF_idle(IdleClock) /\ WF_idle(IdlePoll) /\ WF_idle(IdleSkip) /\ WF_idle(IdleRelease)
IdleEventuallyProcesses == idle.pending ~> ~idle.pending
IdlePromptWake ==
    idle.event \in idleSignals /\ (idle.event # "restart" \/ ~idle.observed) => idle.age = 0
IdleWithinMax == idle.age <= 15
IdleWithinOne == idle.age <= 1
IdleSkipsQuietReader == ~idle.quietWork

=============================================================================
