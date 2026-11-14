----------------------------- MODULE RoutineRun -----------------------------
EXTENDS Naturals
CONSTANTS Mode, Policy, PrejudgeEnabled

VARIABLE s
vars == <<s>>
Terminal == {"succeeded", "failed", "interrupted", "skipped", "paused"}
Unsent == {"none", "input_pending", "accepted", "NotSent", "retry_scheduled"}

Init == \E c1, noReply, held, verified \in BOOLEAN:
 s = [mode |-> Mode, status |-> "running", action |-> IF Mode = "script" THEN "complete" ELSE "agent",
             input |-> "none", queued |-> FALSE, acceptedTurns |-> 0,
             lateAcceptedTurns |-> 0, expired |-> FALSE, recorded |-> FALSE,
             notified |-> FALSE, retryCount |-> 0, sharedDestroyed |-> FALSE, recovering |-> FALSE,
             keyPending |-> FALSE, outcomeKind |-> "pending", reason |-> "none",
             prejudgeDone |-> (~PrejudgeEnabled \/ Mode = "script"),
             prejudgeCalls |-> 0, prejudgeResult |-> "none",
             c1 |-> c1, noReply |-> noReply, replayHeld |-> held,
             bodyPosts |-> 0, rawHash |-> 17, restarted |-> FALSE, recoverySettled |-> FALSE, episodePresent |-> TRUE,
             drainMarker |-> FALSE, drainRequired |-> FALSE, receiptVerified |-> verified]

CompleteScript ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.action = "complete" /\ s.input = "none"
    /\ s' = [s EXCEPT !.status = "succeeded", !.recorded = TRUE, !.outcomeKind = "script"]

SkipOrPause ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.action \in {"agent", "complete"} /\ s.input = "none"
    /\ \E result \in {"skipped", "paused"}:
         s' = [s EXCEPT !.status = result,
                        !.action = IF result = "paused" THEN "pause" ELSE "skip",
                        !.recorded = TRUE, !.notified = result = "paused", !.outcomeKind = "script"]

Admit ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "none" /\ ~s.expired /\ s.prejudgeDone
    /\ (s.action = "agent" \/ (Mode = "script" /\ Policy = "script_agent"))
    /\ s' = [s EXCEPT !.input = "input_pending", !.queued = TRUE, !.action = "agent"]

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
    /\ s.status = "running" /\ s.input = "accepted" /\ s.queued
    /\ s.acceptedTurns < 2
    /\ \E accepted \in {0, 1}:
         s' = [s EXCEPT !.input = "MayHaveSent", !.queued = FALSE,
                        !.acceptedTurns = @ + accepted, !.keyPending = accepted = 0]

ObserveAccepted ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "MayHaveSent" /\ s.acceptedTurns > 0 /\ ~s.keyPending
    /\ s' = [s EXCEPT !.input = "started"]

ObserveNotSent ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "MayHaveSent" /\ s.keyPending
    /\ s' = [s EXCEPT !.input = "NotSent", !.queued = TRUE, !.keyPending = FALSE]

NativeAccept ==
    /\ s.keyPending /\ s.input \in {"MayHaveSent", "cancelled"}
    /\ s.acceptedTurns < 2
    /\ s' = [s EXCEPT !.keyPending = FALSE, !.acceptedTurns = @ + 1,
                      !.lateAcceptedTurns = @ + IF s.status \in Terminal /\ s.input = "cancelled" THEN 1 ELSE 0]

CompleteAgent ==
    /\ ~s.recovering
    /\ s.status = "running" /\ s.input = "started"
    /\ s' = [s EXCEPT !.status = "succeeded", !.recorded = TRUE, !.outcomeKind = "agent",
                      !.bodyPosts = IF s.c1 /\ s.noReply THEN 0 ELSE 1]

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
    /\ s.status \in Terminal /\ s.queued /\ s.acceptedTurns < 2
    /\ s' = [s EXCEPT !.input = "MayHaveSent", !.queued = FALSE,
                      !.acceptedTurns = @ + 1, !.lateAcceptedTurns = @ + 1, !.keyPending = FALSE]

Prejudge ==
    /\ ~s.recovering /\ ~s.expired /\ s.status = "running"
    /\ Mode # "script" /\ PrejudgeEnabled /\ ~s.prejudgeDone
    /\ s.action = "agent" /\ s.input = "none" /\ s.prejudgeCalls = 0
    /\ \E result \in {"rc0_no", "rc0_yes", "rc2", "rc3"}:
         IF result = "rc0_no"
         THEN s' = [s EXCEPT !.prejudgeDone = TRUE, !.prejudgeCalls = 1,
                            !.prejudgeResult = result, !.status = "skipped", !.action = "skip",
                            !.reason = "prejudge_no_work", !.outcomeKind = "script", !.recorded = TRUE]
         ELSE s' = [s EXCEPT !.prejudgeDone = TRUE, !.prejudgeCalls = 1, !.prejudgeResult = result]

FailureKinds == {"thread_setup_permanent", "js_error", "migration_invalid", "command_nonzero", "deferral_exhausted"}
FailBeforeSubmit(kind) ==
    /\ ~s.recovering /\ s.status = "running" /\ s.input = "none"
    /\ kind \in FailureKinds
    /\ (kind # "command_nonzero" \/ Mode = "script")
    /\ s' = [s EXCEPT !.status = "failed", !.reason = kind,
                      !.recorded = TRUE, !.notified = TRUE, !.outcomeKind = "failed"]

ConsumeCommand ==
    /\ ~s.recovering /\ ~s.expired /\ s.status = "running" /\ s.prejudgeDone
    /\ s.action = "agent" /\ s.input = "none"
    /\ s' = [s EXCEPT !.status = "succeeded", !.recorded = TRUE,
                      !.action = IF Policy = "consumed_as_agent" THEN "agent" ELSE "complete",
                      !.outcomeKind = IF Policy = "consumed_as_agent" THEN "agent" ELSE "command_consumed"]

CancelAfterKey ==
    /\ Policy = "cancel_after_key" /\ s.status = "running"
    /\ (s.expired \/ s.recovering) /\ s.input = "MayHaveSent" /\ s.keyPending
    /\ s' = [s EXCEPT !.status = "failed", !.input = "cancelled", !.queued = FALSE,
                      !.recorded = TRUE, !.notified = TRUE, !.recovering = FALSE]

TeardownChannel ==
    /\ Policy = "channel_teardown" /\ Mode = "channel"
    /\ s.status \in Terminal /\ ~s.sharedDestroyed
    /\ s' = [s EXCEPT !.sharedDestroyed = TRUE]

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
RestartCompleted ==
    /\ s.status = "succeeded" /\ s.outcomeKind = "agent" /\ ~s.restarted
    /\ \E drain \in BOOLEAN:
         s' = [s EXCEPT !.restarted = TRUE, !.recovering = TRUE, !.drainMarker = drain, !.drainRequired = drain]
RestoreCompleted ==
    /\ s.status = "succeeded" /\ s.recovering
    /\ s.receiptVerified
    /\ ~(Policy = "before_drain_settlement" /\ s.drainMarker /\ s.c1 /\ s.noReply)
    /\ s' = [s EXCEPT !.recovering = FALSE, !.recoverySettled = TRUE, !.episodePresent = s.replayHeld,
                      !.drainMarker = FALSE,
                      !.bodyPosts = IF Policy = "before_recovery_presentation" /\ s.c1 /\ s.noReply
                                   THEN @ + 1 ELSE @]
ConsumeUnverifiedMarker ==
    /\ Policy = "before_receipt_check" /\ s.recovering /\ s.drainMarker
    /\ s.c1 /\ s.noReply /\ ~s.receiptVerified
    /\ s' = [s EXCEPT !.drainMarker = FALSE]
Restart == s.status = "running" /\ UNCHANGED s
Done == s.status \in Terminal /\ UNCHANGED s

Finish == CompleteScript \/ SkipOrPause \/ CompleteAgent \/ ProviderFailure
          \/ CancelUnsentAtDeadline \/ CloseUncertainAtDeadline \/ DropHeld \/ CancelAfterKey
          \/ (\E kind \in FailureKinds: FailBeforeSubmit(kind)) \/ ConsumeCommand
Next == Prejudge \/ Admit \/ OwnerAccept \/ RefuseBeforeKey \/ RearmKnownUnsent \/ Submit
        \/ NativeAccept \/ ObserveAccepted \/ ObserveNotSent \/ RetryStarted \/ ResendUnknown
        \/ Expire \/ Finish \/ BeginRecovery \/ RecoveryFinish \/ AwaitRecoveryCommit
        \/ LateDispatch \/ TeardownChannel \/ AwaitHeld \/ Restart \/ RestartCompleted \/ RestoreCompleted \/ ConsumeUnverifiedMarker \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Expire) /\ WF_vars(Finish)
        /\ WF_vars(RecoveryFinish) /\ WF_vars(RestoreCompleted)

NoDuplicateTurn == s.acceptedTurns <= 1
SuccessfulAgentExactlyOne == (s.status = "succeeded" /\ s.outcomeKind = "agent") => s.acceptedTurns = 1
ScriptHasNoAgentTurn == (Mode = "script" \/ s.action \in {"skip", "pause"}) => s.acceptedTurns = 0
NoSilentLoss == s.status \in Terminal =>
                  (s.recorded /\ (s.status \in {"failed", "interrupted", "paused"} => s.notified))
TerminalLeavesNothingQueued == s.status \in Terminal => ~s.queued
PrejudgeAtMostOnce == s.prejudgeCalls <= 1
ScriptHasNoPrejudge == Mode = "script" => s.prejudgeCalls = 0
PrejudgeSkipHasNoAgent == s.reason = "prejudge_no_work" =>
                           (s.status = "skipped" /\ s.acceptedTurns = 0 /\ ~s.queued)
CommandConsumedHasNoAgent == s.outcomeKind = "command_consumed" => s.acceptedTurns = 0
NoLateDispatch == s.lateAcceptedTurns = 0
SharedSessionPreserved == ~s.sharedDestroyed
NoReplyRecoveryBodyZero == (s.c1 /\ s.noReply) => s.bodyPosts = 0
RecoveryPreservesRaw == s.rawHash = 17
HeldEpisodeSurvives == s.replayHeld => s.episodePresent
EveryRunTerminates == <>(s.status \in Terminal)
QuietRestartEventuallySettles == (s.c1 /\ s.noReply /\ s.restarted /\ s.receiptVerified) ~> s.recoverySettled
UnverifiedReceiptKeepsMarker == (s.c1 /\ s.noReply /\ s.drainRequired /\ ~s.receiptVerified) => s.drainMarker
=============================================================================
