---- MODULE HerdrIdleMeta ----
\* One exact source, owner front and live pane; only the transcript tail changes.
EXTENDS Naturals
CONSTANTS Policy, Turn, Mutant
VARIABLE r
vars == <<r>>

TurnOpen == Turn \in {"tool", "streaming", "compacting", "new-prompt", "tool-result", "unknown", "torn"}
StopCase == Turn \in {"stop-continuation", "quiet-stop"}
Observed == IF Turn = "stop-continuation"
               THEN IF r.continuationComplete THEN "Idle"
                    ELSE IF r.openerVisible THEN "Busy"
                    ELSE IF Policy = "BroadStop" THEN "Idle" ELSE "Busy"
            ELSE IF Turn = "quiet-stop" THEN "Idle"
            ELSE IF Mutant = "AnyMetaIdle" THEN "Idle"
            ELSE IF Turn = "closed" /\ Policy = "Fixed" THEN "Idle"
            ELSE IF Turn = "tool" THEN "Busy" ELSE "Held"
\* Idle readiness returns before source capture. A previously captured busy operation
\* therefore refuses, then its shared probe wakes the owner to take the Idle route.
IdleVerified == Observed = "Idle" /\ (Policy \in {"Fixed", "BroadStop"} \/ Mutant = "AnyMetaIdle")
ReadSource == ~IdleVerified
Admitted == Observed \in {"Idle", "Busy"} /\ ReadSource
ProbeReady == Observed = "Busy" \/ IdleVerified

Init == r = [phase |-> "before-tail", queued |-> FALSE,
    observed |-> "none", source |-> TRUE, idleVerified |-> FALSE,
    admits |-> FALSE, probe |-> "none", verdict |-> "none",
    route |-> "none", delivered |-> FALSE, explainedEnd |-> FALSE,
    idlePastes |-> 0, busyFrames |-> 0,
    continuingStop |-> FALSE, openerVisible |-> FALSE, continuationComplete |-> FALSE]
AppendNeutralTail == /\ r.phase = "before-tail" /\ ~StopCase
                     /\ r' = [r EXCEPT !.phase = "tail"]
AppendStopOutput == /\ r.phase = "before-tail" /\ StopCase
                    /\ r' = [r EXCEPT !.phase = "tail",
                        !.continuingStop = Turn = "stop-continuation"]
\* The CLI persists hook_success before its next hook_additional_context opener.
ContinuationOpener == /\ r.continuingStop /\ ~r.openerVisible
                      /\ r' = [r EXCEPT !.openerVisible = TRUE]
ContinuationFinishes == /\ r.openerVisible /\ ~r.continuationComplete
                        /\ r' = [r EXCEPT !.continuationComplete = TRUE]
OwnerInput == /\ r.phase = "tail"
              /\ r' = [r EXCEPT !.phase = "queued", !.queued = TRUE]
CheckAdmits == /\ r.phase = "queued"
               /\ r' = [r EXCEPT !.observed = Observed, !.source = ReadSource,
                   !.idleVerified = IdleVerified, !.admits = Admitted,
                   !.phase = IF Admitted THEN "busy-ready" ELSE "held",
                   !.route = IF Admitted THEN "busy" ELSE "none"]
HeldWatch == /\ r.phase = "held"
             /\ r' = [r EXCEPT !.observed = Observed, !.idleVerified = IdleVerified,
                 !.probe = IF ProbeReady THEN "Ready" ELSE "Unknown",
                 !.verdict = IF ProbeReady THEN "Wake" ELSE "Keep",
                 !.phase = IF ProbeReady THEN "woken" ELSE "held"]
OwnerRetry == /\ r.phase = "woken"
              /\ r' = [r EXCEPT !.phase = IF IdleVerified THEN "idle-ready" ELSE "held",
                                !.route = IF IdleVerified THEN "idle" ELSE "none"]
IdlePaste == /\ r.phase = "idle-ready" /\ IdleVerified
             /\ r' = [r EXCEPT !.phase = "done", !.delivered = TRUE,
                               !.idlePastes = @ + 1]
\* Existing busy framing is an intentional running-turn route, never an Idle paste.
BusyFrame == /\ r.phase = "busy-ready" /\ Admitted
             /\ r' = [r EXCEPT !.phase = "done", !.delivered = TRUE,
                               !.busyFrames = @ + 1]
RevalidateIdle == /\ r.phase = "idle-ready" /\ ~IdleVerified
                  /\ r' = [r EXCEPT !.phase = "held", !.idleVerified = FALSE]
RevalidateBusy == /\ r.phase = "busy-ready" /\ ~Admitted
                  /\ r' = [r EXCEPT !.phase = "held", !.idleVerified = IdleVerified]
Done == /\ r.phase = "done" /\ UNCHANGED vars
Next == AppendNeutralTail \/ AppendStopOutput \/ ContinuationOpener \/ ContinuationFinishes \/ OwnerInput \/ CheckAdmits \/ HeldWatch
        \/ OwnerRetry \/ IdlePaste \/ BusyFrame \/ RevalidateIdle \/ RevalidateBusy \/ Done
Fairness == WF_vars(AppendNeutralTail) /\ WF_vars(AppendStopOutput)
            /\ WF_vars(ContinuationOpener) /\ WF_vars(ContinuationFinishes) /\ WF_vars(OwnerInput)
            /\ WF_vars(CheckAdmits) /\ WF_vars(HeldWatch)
            /\ WF_vars(OwnerRetry) /\ WF_vars(IdlePaste) /\ WF_vars(BusyFrame)
            /\ WF_vars(RevalidateIdle) /\ WF_vars(RevalidateBusy)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == r.phase \in {"before-tail", "tail", "queued", "busy-ready", "held",
                       "woken", "idle-ready", "done"}
          /\ r.idlePastes \in 0..1 /\ r.busyFrames \in 0..1
NoIdlePasteOnRunningTurn == TurnOpen => r.idlePastes = 0
NoRunningIdleDelegation == TurnOpen => ~r.idleVerified
IdleNeedsClosedReply == r.idleVerified => Turn \in {"closed", "quiet-stop"}
    \/ (Turn = "stop-continuation" /\ r.continuationComplete)
AtMostOneDelivery == r.idlePastes + r.busyFrames <= 1
NoIdlePasteDuringStopContinuation == r.continuingStop /\ ~r.continuationComplete => r.idlePastes = 0
NoProofBeforeStopContinuationOpener == r.continuingStop /\ ~r.openerVisible => ~r.idleVerified
WakeRequiresReady == r.verdict = "Wake" => r.probe = "Ready"
ScenarioReached == <> r.queued
IdleInputEventuallyResolves == r.queued ~> (r.delivered \/ r.explainedEnd)
=============================================================================
