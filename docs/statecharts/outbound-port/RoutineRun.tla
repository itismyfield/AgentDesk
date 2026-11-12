----------------------------- MODULE RoutineRun -----------------------------
EXTENDS Naturals
CONSTANTS Mode, Policy

VARIABLE s
vars == <<s>>
Terminal == {"succeeded", "failed", "interrupted", "skipped", "paused"}
Unsent == {"none", "input_pending", "accepted", "NotSent", "retry_scheduled"}

Init == s = [status |-> "running", action |-> IF Mode = "script" THEN "complete" ELSE "agent",
             input |-> "none", queued |-> FALSE, acceptedTurns |-> 0,
             lateAcceptedTurns |-> 0, expired |-> FALSE, recorded |-> FALSE,
             notified |-> FALSE, retryCount |-> 0, sharedDestroyed |-> FALSE, recovering |-> FALSE]

CompleteScript ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.action = "complete" /\ s.input = "none"
    /\ s' = [s EXCEPT !.status = "succeeded", !.recorded = TRUE]

SkipOrPause ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.action \in {"agent", "complete"} /\ s.input = "none"
    /\ \E result \in {"skipped", "paused"}:
         s' = [s EXCEPT !.status = result,
                        !.action = IF result = "paused" THEN "pause" ELSE "skip",
                        !.recorded = TRUE, !.notified = result = "paused"]

Admit ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.action = "agent" /\ s.input = "none" /\ ~s.expired
    /\ s' = [s EXCEPT !.input = "input_pending", !.queued = TRUE]

OwnerAccept ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "input_pending" /\ ~s.expired
    /\ Policy # "before_held"
    /\ s' = [s EXCEPT !.input = "accepted"]

RefuseBeforeKey ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "accepted" /\ ~s.expired
    /\ s' = [s EXCEPT !.input = "NotSent"]

RearmKnownUnsent ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "NotSent" /\ ~s.expired
    /\ s' = [s EXCEPT !.input = "accepted"]

Submit ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "accepted" /\ ~s.expired
    /\ s.acceptedTurns < 2
    /\ \E accepted \in {0, 1}:
         s' = [s EXCEPT !.input = "MayHaveSent", !.queued = FALSE,
                        !.acceptedTurns = @ + accepted]

ObserveAccepted ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "MayHaveSent" /\ s.acceptedTurns > 0
    /\ s' = [s EXCEPT !.input = "started"]

ObserveNotSent ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "MayHaveSent" /\ s.acceptedTurns = 0
    /\ s' = [s EXCEPT !.input = "NotSent", !.queued = TRUE]

CompleteAgent ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "started"
    /\ s' = [s EXCEPT !.status = "succeeded", !.recorded = TRUE]

ProviderFailure ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "started"
    /\ IF Policy = "before_retry" /\ s.retryCount = 0 /\ ~s.expired
       THEN s' = [s EXCEPT !.input = "retry_scheduled", !.retryCount = 1]
       ELSE s' = [s EXCEPT !.status = "failed", !.recorded = TRUE, !.notified = TRUE]

RetryStarted ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "retry_scheduled" /\ ~s.expired
    /\ s' = [s EXCEPT !.input = "accepted", !.queued = TRUE]

ResendUnknown ==
    /\ ~s.recovering
    /\ Policy = "resend_unknown" /\ s.status = "running" /\ ~s.expired
    /\ s.input = "MayHaveSent" /\ s.retryCount = 0
    /\ s' = [s EXCEPT !.input = "accepted", !.queued = TRUE, !.retryCount = 1]

Expire ==
    /\ ~s.recovering
    /\ s.status = "running" /\ ~s.expired
    /\ s' = [s EXCEPT !.expired = TRUE]

CancelUnsentAtDeadline ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.expired /\ s.input \in Unsent
    /\ ~(Policy = "before_held" /\ s.input = "input_pending")
    /\ IF Policy = "no_deadline_cancel" /\ s.queued
       THEN s' = [s EXCEPT !.status = "failed", !.input = "accepted",
                          !.recorded = TRUE, !.notified = TRUE]
       ELSE s' = [s EXCEPT !.status = "failed", !.input = "cancelled", !.queued = FALSE,
                          !.recorded = TRUE, !.notified = TRUE]

CloseUncertainAtDeadline ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.expired /\ s.input \in {"MayHaveSent", "started"}
    /\ s' = [s EXCEPT !.status = "failed", !.recorded = TRUE, !.notified = TRUE]

DropHeld ==
    /\ ~s.recovering
    /\ Policy = "drop_held" /\ s.status = "running" /\ s.input = "input_pending"
    /\ s' = [s EXCEPT !.status = "failed", !.input = "cancelled", !.queued = FALSE]

LateDispatch ==
    /\ Policy \in {"no_deadline_cancel", "no_recovery_cancel"}
    /\ s.status \in Terminal /\ s.queued
    /\ s' = [s EXCEPT !.input = "MayHaveSent", !.queued = FALSE,
                      !.acceptedTurns = @ + 1, !.lateAcceptedTurns = @ + 1]

AwaitHeld ==
    /\ ~s.recovering
    /\ Policy = "before_held" /\ s.status = "running" /\ s.input = "input_pending"
    /\ UNCHANGED s

BeginRecovery ==
    /\ s.status = "running" /\ ~s.recovering
    /\ s' = [s EXCEPT !.recovering = TRUE]

RecoverWithdraw ==
    /\ s.status = "running" /\ s.recovering /\ s.input \in Unsent
    /\ ~(Policy = "no_recovery_cancel" /\ s.queued)
    /\ s' = [s EXCEPT !.input = "cancelled", !.queued = FALSE]

RecoverUnsent ==
    /\ s.status = "running" /\ s.recovering /\ s.input = "cancelled"
    /\ s' = [s EXCEPT !.status = "interrupted", !.recorded = TRUE,
                      !.notified = TRUE, !.recovering = FALSE]

RecoverWithoutWithdraw ==
    /\ Policy = "no_recovery_cancel"
    /\ s.status = "running" /\ s.recovering /\ s.input \in Unsent /\ s.queued
    /\ s' = [s EXCEPT !.status = "interrupted", !.input = "accepted",
                      !.recorded = TRUE, !.notified = TRUE, !.recovering = FALSE]

RecoverUncertain ==
    /\ s.status = "running" /\ s.recovering /\ s.input \in {"MayHaveSent", "started"}
    /\ s' = [s EXCEPT !.status = "interrupted", !.recorded = TRUE,
                      !.notified = TRUE, !.recovering = FALSE]

AwaitRecoveryCommit == s.status = "running" /\ s.recovering /\ UNCHANGED s
RecoveryFinish == RecoverWithdraw \/ RecoverUnsent \/ RecoverWithoutWithdraw \/ RecoverUncertain
Restart == s.status = "running" /\ UNCHANGED s
Done == s.status \in Terminal /\ UNCHANGED s

Finish == CompleteScript \/ SkipOrPause \/ CompleteAgent \/ ProviderFailure
          \/ CancelUnsentAtDeadline \/ CloseUncertainAtDeadline \/ DropHeld
Next == Admit \/ OwnerAccept \/ RefuseBeforeKey \/ RearmKnownUnsent \/ Submit
        \/ ObserveAccepted \/ ObserveNotSent \/ RetryStarted \/ ResendUnknown
        \/ Expire \/ Finish \/ BeginRecovery \/ RecoveryFinish \/ AwaitRecoveryCommit
        \/ LateDispatch \/ AwaitHeld \/ Restart \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Expire) /\ WF_vars(Finish)
        /\ WF_vars(RecoveryFinish)

NoDuplicateTurn == s.acceptedTurns <= 1
SuccessfulAgentExactlyOne == (s.status = "succeeded" /\ s.action = "agent") => s.acceptedTurns = 1
ScriptHasNoAgentTurn == (Mode = "script" \/ s.action \in {"skip", "pause"}) => s.acceptedTurns = 0
NoSilentLoss == s.status \in Terminal =>
                  (s.recorded /\ (s.status \in {"failed", "interrupted", "paused"} => s.notified))
NoLateDispatch == s.lateAcceptedTurns = 0
SharedSessionPreserved == ~s.sharedDestroyed
EveryRunTerminates == <>(s.status \in Terminal)
=============================================================================
