------------------------- MODULE IntakeReadiness -------------------------
\* Code base: b20a366c8812348f14a9e08a7439cb8df6cfb09a.
\* Current DB orchestration and proposed service orchestration share each
\* observable step; only the holder of the same locked transaction changes.
EXTENDS Naturals, FiniteSets
CONSTANTS Boundary, Mutation, Cases
ASSUME Boundary \in {"current", "proposed"}
ASSUME Mutation \in {"none", "stale-snapshot", "early-tx-end"}
AllScenarios == {"ready", "empty", "budget", "parse-error", "offline", "unknown", "malformed-id", "legacy"}
ASSUME Cases \subseteq AllScenarios /\ Cases # {}
Scenarios == Cases
VARIABLE s
vars == <<s>>
Init == \E scenario \in Scenarios, fenced \in BOOLEAN:
    s = [pc |-> "begin", scenario |-> scenario, fenced |-> fenced,
         tx |-> 0, sourceLock |-> FALSE, ownerLock |-> FALSE,
         workerLock |-> FALSE, prepared |-> "none", drift |-> "none",
         profile |-> TRUE, forwarding |-> (IF scenario = "unknown" THEN "unknown" ELSE "ready"),
         profileVersion |-> 0, forwardingVersion |-> 0,
         lockProfileVersion |-> 0, lockForwardingVersion |-> 0,
         sampledProfile |-> FALSE, sampledForwarding |-> "unread", sampledBeforeLock |-> FALSE,
         profileVersionRead |-> 0, forwardingVersionRead |-> 0,
         outcome |-> "pending", applied |-> FALSE, appliedUnderLocks |-> TRUE,
         appliedTx |-> 0, spends |-> 0]
Begin ==
    /\ s.pc = "begin"
    /\ s' = [s EXCEPT !.pc = (IF Mutation = "stale-snapshot" THEN "early-observe" ELSE "source"), !.tx = 1]
ObserveBeforeLocks ==
    /\ s.pc = "early-observe"
    /\ s' = [s EXCEPT !.pc = "source", !.sampledProfile = s.profile,
             !.sampledForwarding = s.forwarding,
             !.profileVersionRead = s.profileVersion,
             !.forwardingVersionRead = s.forwardingVersion, !.sampledBeforeLock = TRUE]
LockSource ==
    /\ s.pc = "source"
    /\ s' = [s EXCEPT !.pc = (IF s.scenario = "empty" THEN "commit" ELSE "budget"),
             !.sourceLock = (s.scenario # "empty"),
             !.outcome = (IF s.scenario = "empty" THEN "empty" ELSE s.outcome)]
CheckFamilyBudget ==
    /\ s.pc = "budget"
    /\ s' = [s EXCEPT !.pc = (IF s.scenario = "budget" THEN "commit" ELSE "owner"),
             !.outcome = (IF s.scenario = "budget" THEN "budget" ELSE s.outcome)]
LockOwner ==
    /\ s.pc = "owner"
    /\ s' = [s EXCEPT !.pc = "worker", !.ownerLock = s.fenced]
LockWorker ==
    /\ s.pc = "worker"
    /\ s' = [s EXCEPT !.pc = (IF Mutation = "early-tx-end" THEN "early-end" ELSE "parse"),
             !.workerLock = TRUE, !.lockProfileVersion = s.profileVersion,
             !.lockForwardingVersion = s.forwardingVersion,
             !.prepared = (IF Boundary = "proposed" THEN "owned" ELSE "none")]
EndTransactionEarly ==
    /\ s.pc = "early-end"
    /\ s' = [s EXCEPT !.pc = "parse", !.tx = 2,
             !.sourceLock = FALSE, !.ownerLock = FALSE, !.workerLock = FALSE]
ParseRequirements ==
    /\ s.pc = "parse"
    /\ s' = [s EXCEPT !.pc = (IF s.scenario = "parse-error" THEN "rollback" ELSE "profile"),
             !.outcome = (IF s.scenario = "parse-error" THEN "parse-error" ELSE s.outcome)]
ObserveAuthProfile ==
    /\ s.pc = "profile"
    /\ s' = [s EXCEPT !.pc = "readiness",
             !.sampledProfile = (IF Mutation = "stale-snapshot" THEN @ ELSE s.profile),
             !.profileVersionRead = (IF Mutation = "stale-snapshot" THEN @ ELSE s.profileVersion)]
ObserveForwardingAndTime ==
    /\ s.pc = "readiness"
    /\ s' = [s EXCEPT !.pc = "apply",
             !.sampledForwarding = (IF Mutation = "stale-snapshot" THEN @ ELSE s.forwarding),
             !.forwardingVersionRead = (IF Mutation = "stale-snapshot" THEN @ ELSE s.forwardingVersion)]
Eligible == s.scenario # "offline" /\
    (s.scenario = "legacy" \/ (s.sampledProfile /\ s.sampledForwarding = "ready"))
SelectedIdExists == s.scenario # "malformed-id"
Apply ==
    /\ s.pc = "apply"
    /\ (Boundary = "current" \/ s.prepared = "owned")
    /\ s' = [s EXCEPT !.pc = "commit", !.outcome = (IF Eligible /\ SelectedIdExists THEN "retry" ELSE "no-target"),
             !.applied = (Eligible /\ SelectedIdExists),
             !.appliedUnderLocks = (s.sourceLock /\ (~s.fenced \/ s.ownerLock) /\ s.workerLock),
             !.appliedTx = s.tx,
             !.prepared = (IF Boundary = "proposed" THEN "spent" ELSE "none"),
             !.spends = @ + (IF Boundary = "proposed" THEN 1 ELSE 0)]
Commit ==
    /\ s.pc = "commit"
    /\ s' = [s EXCEPT !.pc = "done", !.tx = 0, !.sourceLock = FALSE,
             !.ownerLock = FALSE, !.workerLock = FALSE]
Rollback ==
    /\ s.pc = "rollback"
    /\ s' = [s EXCEPT !.pc = "done", !.tx = 0, !.sourceLock = FALSE,
             !.ownerLock = FALSE, !.workerLock = FALSE, !.prepared = "spent"]
\* Configuration/cached forwarding can change independently of PostgreSQL locks.
\* One change suffices to expose pre-lock capture; changes AFTER an observation
\* are not retrospectively classified as stale.
ChangeAuthProfile ==
    /\ s.pc \notin {"begin", "done", "commit", "rollback"} /\ s.drift = "none"
    /\ s' = [s EXCEPT !.profile = FALSE, !.profileVersion = 1, !.drift = "profile"]
ExpireForwarding ==
    /\ s.pc \notin {"begin", "done", "commit", "rollback"} /\ s.drift = "none"
    /\ s' = [s EXCEPT !.forwarding = "expired", !.forwardingVersion = 1, !.drift = "forwarding"]
Done == /\ s.pc = "done" /\ UNCHANGED vars
Next == Begin \/ ObserveBeforeLocks \/ LockSource \/ CheckFamilyBudget \/ LockOwner \/ LockWorker
        \/ EndTransactionEarly \/ ParseRequirements \/ ObserveAuthProfile \/ ObserveForwardingAndTime
        \/ Apply \/ Commit \/ Rollback \/ ChangeAuthProfile \/ ExpireForwarding \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)
TypeOK == /\ s.scenario \in Scenarios /\ s.tx \in 0..2 /\ s.spends \in 0..1
          /\ s.outcome \in {"pending", "empty", "budget", "parse-error", "retry", "no-target"}
LockOrder == (s.ownerLock => s.sourceLock) /\ (s.workerLock => (s.sourceLock /\ (~s.fenced \/ s.ownerLock)))
ApplyUsesLockedTransaction == s.applied => (s.appliedUnderLocks /\ s.appliedTx = 1)
NoStaleSnapshot == s.applied =>
    (s.profileVersionRead >= s.lockProfileVersion /\ s.forwardingVersionRead >= s.lockForwardingVersion)
ModernUnknownAndOfflineFailClosed == s.scenario \in {"unknown", "offline"} => ~s.applied
FirstMalformedIdDoesNotSkipAhead == s.scenario = "malformed-id" => ~s.applied
PreparationConsumedOnce == s.spends <= 1
OutcomeMatchesObservedInputs == s.pc \in {"commit", "done"} /\ s.scenario \notin {"empty", "budget", "parse-error"} =>
    (s.outcome = (IF Eligible /\ SelectedIdExists THEN "retry" ELSE "no-target"))
TransactionEventuallyEnds == <> (s.pc = "done")
=============================================================================
