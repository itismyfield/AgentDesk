-------------------------- MODULE ClaudeReadiness --------------------------
\* Readiness refinement; production code baseline 574af580b0c5d6e6ce280994f4ed243463de5d2d.
\* Idle submissions and the unchanged native busy queue have distinct actions.
EXTENDS Naturals, FiniteSets
CONSTANTS N, Mode
VARIABLE r
Ids == 1..N
Before == Mode = "ready-before-narrow"
ClearHeld == Mode \in {"ready-clear-held-before", "ready-clear-held-after"}
ClearPreSource == ClearHeld \/ Mode \in {"ready-clear-pending-before", "ready-clear-pending-after", "ready-clear-pending-recheck"}
Fresh == ClearPreSource \/ Mode \in {"ready-fresh", "ready-clear", "ready-resume", "ready-late-dialog", "ready-preflight-only"}
Dialog == Mode \in {"ready-late-dialog", "ready-preflight-only"}
Permission == Mode \in {"ready-permission-later", "ready-permission-poison"}
Race == Mode \in {"ready-race", "ready-no-revalidate"}
Blind == Mode \in {"ready-unread", "ready-permanent-unread", "ready-draft-bypass"}
Person == Mode \in {"ready-draft", "ready-draft-bypass"}
NoWake == Mode = "ready-no-wake"
Busy == Mode \in {"ready-busy-active", "ready-busy-native", "ready-block-busy", "ready-busy-finish-before",
                  "ready-busy-finish-after", "ready-busy-reexpose"}
Suppressed == Mode \in {"ready-suppressed-stop", "ready-no-latch",
                        "ready-suppressed-compact", "ready-no-latch-compact"}
NoLatch == Mode \in {"ready-no-latch", "ready-no-latch-compact"}
CompactCompletion == Mode \in {"ready-compact-completion", "ready-compact-no-completion"}
Compact == CompactCompletion \/ Mode \in {"ready-compact", "ready-busy-compact",
                     "ready-suppressed-compact", "ready-no-latch-compact"}
Initial ==
    [phase |-> IF Fresh THEN "Booting" ELSE "Running",
     epoch |-> 1, turn |-> IF Fresh THEN 0 ELSE 1,
     observed |-> "Unknown", seenEpoch |-> 0, seenTurn |-> 0,
     started |-> FALSE, needsStart |-> Fresh, terminal |-> 0,
     oldBinding |-> ClearPreSource, filePublished |-> ~ClearPreSource,
     durableClear |-> ClearPreSource, pendingSeq |-> IF ClearPreSource THEN 1 ELSE 0,
     proofPending |-> 0, pendingChanges |-> IF Mode = "ready-clear-pending-recheck" THEN 1 ELSE 0,
     badClear |-> FALSE, heldOnce |-> FALSE,
     tail |-> "none", continuing |-> FALSE, stopBoundary |-> 0, softStage |-> 0,
     stop |-> 0, oldStopLeft |-> IF ClearHeld THEN 0 ELSE 1, awake |-> TRUE,
     idleWakes |-> IF ClearHeld THEN 0 ELSE 2,
     human |-> Person \/ ClearHeld, draft |-> IF Blind THEN "Unread" ELSE IF Person \/ ClearHeld THEN "Person" ELSE "Empty",
     q |-> [i \in Ids |-> "queued"], effects |-> [i \in Ids |-> 0],
     writes |-> [i \in Ids |-> 0], busyWrites |-> [i \in Ids |-> 0],
     busyKeys |-> [i \in Ids |-> "none"],
     route |-> [i \in Ids |-> IF Busy /\ i = 1 THEN "busy" ELSE "idle"],
     head |-> 0, proofEpoch |-> 0, proofTurn |-> 0,
     nativeStarts |-> IF Race THEN 1 ELSE 0,
     restarts |-> IF Mode = "ready-restart" THEN 1 ELSE 0,
     compactPending |-> Compact, compacts |-> 0,
     compactFence |-> FALSE, compactEvidence |-> FALSE, compactReady |-> FALSE,
     badIdleRunning |-> FALSE, badDraft |-> FALSE, badEarly |-> FALSE,
     dialog |-> IF Dialog THEN "Boot" ELSE "None", dialogKeys |-> 0,
     permissionStage |-> 0, poisoned |-> FALSE,
     badCompact |-> FALSE, badContinuation |-> FALSE]
Init == r = Initial
TerminalEligible ==
    /\ ~r.continuing
    /\ r.tail = "hard" \/ (r.tail = "summary" /\ r.stopBoundary = r.turn)
CurrentEvidence ==
    /\ r.observed = "Idle" /\ r.seenEpoch = r.epoch /\ r.seenTurn = r.turn
    /\ (~r.needsStart \/ r.started)
    /\ ~r.continuing
    /\ ~r.compactFence
    /\ (r.started /\ r.turn = 0) \/ r.compactReady \/
       (r.terminal = r.turn /\ r.turn > 0 /\ TerminalEligible)
Ready == CurrentEvidence /\ ~Before /\ ~r.poisoned
    /\ (~r.oldBinding \/ (ClearPreSource /\ Mode # "ready-clear-pending-before" /\ r.started /\ r.durableClear))
First == {i \in Ids : r.q[i] \in {"queued", "held"} /\
          \A j \in 1..(i-1) : r.q[j] = "done"}
SessionStart ==
    /\ r.phase = "Booting"
    /\ ~Dialog \/ r.dialog = "Dismissed"
    /\ r' = [r EXCEPT !.phase = "Idle", !.started = TRUE, !.awake = TRUE]
PollDialog ==
    /\ Dialog /\ r.phase = "Booting"
    /\ r.dialog = "Boot" \/ (r.dialog = "Picker" /\ Mode # "ready-preflight-only")
    /\ r' = [r EXCEPT !.dialog = IF @ = "Boot" THEN "Picker" ELSE "Dismissed",
                     !.dialogKeys = IF r.dialog = "Picker" THEN @ + 1 ELSE @]
PermissionRecord ==
    /\ Permission /\ r.permissionStage < 3 /\ r.phase = "Running"
    /\ r' = [r EXCEPT !.permissionStage = @ + 1, !.terminal = 0,
                     !.tail = IF r.permissionStage = 0 THEN "permission" ELSE "assistant",
                     !.poisoned = @ \/ Mode = "ready-permission-poison"]
Finish ==
    /\ r.phase = "Running" /\ ~r.compactFence
    /\ ~Busy \/ r.q[1] # "queued"
    /\ Mode # "ready-busy-finish-after" \/ r.writes[1] > 0
    /\ ~Suppressed \/ r.softStage >= 4
    /\ ~Permission \/ r.permissionStage = 3
    /\ r' = [r EXCEPT !.phase = "Idle", !.terminal = r.turn, !.tail = "hard",
                     !.continuing = FALSE, !.softStage = IF Suppressed THEN 5 ELSE @]
\* A raw matching Stop boundary is recorded together with continuation disposition.
SuppressStop ==
    /\ Suppressed /\ r.softStage = 0 /\ r.phase = "Running"
    /\ r' = [r EXCEPT !.softStage = 1, !.stopBoundary = r.turn,
                     !.continuing = ~NoLatch]
SoftSummary ==
    /\ Suppressed /\ r.softStage = 1
    /\ r' = [r EXCEPT !.softStage = 2, !.tail = "summary", !.terminal = r.turn]
\* Shared observer may report Idle for a summary while the provider is still Running.
Observe ==
    /\ r' = [r EXCEPT !.observed = IF r.tail = "summary" THEN "Idle"
                                  ELSE IF r.phase = "Booting" THEN "Unknown" ELSE r.phase,
                     !.seenEpoch = r.epoch, !.seenTurn = r.turn,
                     !.softStage = IF Suppressed /\ r.softStage = 2 THEN 3 ELSE @,
                     !.awake = IF NoWake \/ (ClearHeld /\ r.heldOnce) THEN @ ELSE TRUE]
LateAssistant ==
    /\ Suppressed /\ r.softStage = 3
    /\ r' = [r EXCEPT !.softStage = 4, !.tail = "assistant", !.terminal = 0]
\* Stop replay can wake but cannot clear a continuation latch.
Stop ==
    /\ r.phase = "Idle" /\ r.terminal = r.turn /\ r.turn > 0 /\ r.stop # r.turn
    /\ Mode \notin {"ready-lost-stop", "ready-no-wake"}
    /\ r' = [r EXCEPT !.stop = r.turn, !.awake = TRUE]
OldStop ==
    /\ r.oldStopLeft > 0
    /\ r' = [r EXCEPT !.oldStopLeft = 0, !.awake = IF NoWake THEN @ ELSE TRUE]
RecoverDraft ==
    /\ r.draft = "Unread" /\ Mode # "ready-permanent-unread"
    /\ r' = [r EXCEPT !.draft = IF r.human THEN "Person" ELSE "Empty", !.awake = TRUE]
HumanClears ==
    /\ r.human /\ r.draft = "Person" /\ (~ClearHeld \/ r.heldOnce)
    /\ r' = [r EXCEPT !.human = FALSE, !.draft = "Empty",
                     !.awake = IF ClearHeld THEN @ ELSE TRUE]
Hold(i) ==
    /\ i \in First /\ r.route[i] = "idle" /\ r.head = 0 /\ r.awake
    /\ ~ClearHeld \/ Ready
    /\ ~Ready \/ r.draft # "Empty"
    /\ r' = [r EXCEPT !.q[i] = "held", !.awake = FALSE,
                     !.heldOnce = @ \/ ClearHeld]
WatchHeld ==
    /\ ClearHeld /\ r.heldOnce /\ r.q[1] = "held" /\ r.draft = "Empty"
    /\ Ready /\ (~r.oldBinding \/ Mode # "ready-clear-held-before")
    /\ r' = [r EXCEPT !.awake = TRUE]
Prepare(i) ==
    /\ ~CompactCompletion \/ ~r.compactPending
    /\ i \in First /\ r.route[i] = "idle" /\ r.head = 0 /\ r.awake /\ Ready
    /\ r.draft = "Empty" \/ Mode = "ready-draft-bypass"
    /\ r' = [r EXCEPT !.q[i] = "prepared", !.head = i,
                     !.proofEpoch = r.epoch, !.proofTurn = r.turn, !.proofPending = r.pendingSeq]
NativeStart ==
    /\ Race /\ r.head # 0 /\ r.nativeStarts > 0 /\ r.phase = "Idle"
    /\ r' = [r EXCEPT !.phase = "Running", !.turn = @ + 1, !.nativeStarts = 0,
                     !.tail = "assistant"]
ProofCurrent == Ready /\ r.proofEpoch = r.epoch /\ r.proofTurn = r.turn
    /\ (~ClearPreSource \/ r.proofPending = r.pendingSeq)
ChangeClearPending ==
    /\ ClearPreSource /\ r.head # 0 /\ r.pendingChanges > 0
    /\ r' = [r EXCEPT !.pendingSeq = @ + 1, !.pendingChanges = 0]
PublishClearFile ==
    /\ ClearPreSource /\ r.filePublished /\ r.oldBinding
    /\ r' = [r EXCEPT !.oldBinding = FALSE]
Revoke ==
    /\ r.head # 0 /\ (~ProofCurrent \/ r.draft # "Empty")
    /\ r' = [r EXCEPT !.q[r.head] = "held", !.head = 0, !.awake = FALSE]
Submit ==
    /\ (~ClearPreSource \/ r.pendingChanges = 0)
    /\ r.head # 0 /\ (~Race \/ r.nativeStarts = 0)
    /\ ProofCurrent \/ Mode = "ready-no-revalidate"
    /\ r.draft = "Empty" \/ Mode = "ready-draft-bypass"
    /\ r' = [r EXCEPT !.q[r.head] = "sent", !.effects[r.head] = @ + 1,
                     !.writes[r.head] = @ + 1, !.head = 0,
                     !.phase = "Running", !.turn = @ + 1, !.tail = "assistant",
                     !.compactReady = FALSE, !.awake = FALSE,
                     !.badIdleRunning = @ \/ r.phase = "Running",
                     !.badDraft = @ \/ r.human,
                     !.badEarly = @ \/ (r.needsStart /\ ~r.started),
                     !.badClear = @ \/ (r.oldBinding /\ (~r.started \/ ~r.durableClear \/ r.proofPending # r.pendingSeq)),
                     !.filePublished = IF ClearPreSource THEN TRUE ELSE @,
                     !.badContinuation = @ \/ (Suppressed /\ r.softStage < 5)]
\* Selection, first mutation, and publication are separate; AnyTurn can write after Finish.
SelectBusy(i) ==
    /\ Busy /\ i \in First /\ r.route[i] = "busy" /\ r.phase = "Running"
    /\ r' = [r EXCEPT !.q[i] = "busy-selected"]
BusyInject(i) ==
    /\ Busy /\ Mode # "ready-block-busy" /\ r.q[i] = "busy-selected"
    /\ Mode # "ready-busy-finish-before" \/ r.phase = "Idle"
    /\ r.draft = "Empty" /\ r.head = 0
    /\ r' = [r EXCEPT !.q[i] = "busy-written", !.writes[i] = @ + 1,
                     !.busyWrites[i] = @ + 1, !.busyKeys[i] = "paste,Enter"]
BusyPublish(i) ==
    /\ r.q[i] = "busy-written"
    /\ Mode # "ready-busy-finish-after" \/ r.phase = "Idle"
    /\ r' = [r EXCEPT !.q[i] = "native-queued"]
IdleWake ==
    /\ r.phase = "Idle" /\ r.idleWakes > 0
    /\ r' = [r EXCEPT !.awake = TRUE, !.idleWakes = @ - 1]
ReexposeBusy(i) ==
    /\ Mode = "ready-busy-reexpose" /\ r.q[i] = "busy-written" /\ r.phase = "Idle"
    /\ r' = [r EXCEPT !.q[i] = "queued", !.route[i] = "idle", !.awake = TRUE]
BusyAbsorb(i) ==
    /\ r.q[i] = "native-queued" /\ r.phase = "Running"
    /\ r' = [r EXCEPT !.q[i] = "sent", !.effects[i] = @ + 1]
BusyConsume(i) ==
    /\ r.q[i] = "native-queued" /\ r.phase = "Idle"
    /\ r' = [r EXCEPT !.q[i] = "sent", !.effects[i] = @ + 1,
                     !.phase = "Running", !.turn = @ + 1, !.tail = "assistant"]
Receipt(i) ==
    /\ r.q[i] = "sent"
    /\ r' = [r EXCEPT !.q[i] = "done"]
\* Only pre-key daemon restart is refined here; post-key Unknown is never replayed.
Restart ==
    /\ r.restarts > 0 /\ r.head = 0
    /\ r' = [r EXCEPT !.epoch = @ + 1, !.observed = "Unknown",
                     !.started = FALSE, !.awake = FALSE, !.restarts = 0]
Replay(i) ==
    /\ Mode = "ready-replay" /\ r.q[i] = "done" /\ r.effects[i] = 1
    /\ r' = [r EXCEPT !.q[i] = "queued", !.awake = TRUE]
CompactSubmit ==
    /\ r.compactPending /\ r.head = 0 /\ r.draft = "Empty"
    /\ (Ready /\ r.terminal = r.turn /\ r.turn > 0) \/ Mode = "ready-busy-compact"
    /\ r' = [r EXCEPT !.compactPending = FALSE, !.compacts = @ + 1,
                     !.compactFence = CompactCompletion, !.compactReady = FALSE,
                     !.phase = "Running", !.turn = @ + 1, !.tail = "assistant", !.awake = FALSE,
                     !.badCompact = @ \/ r.phase # "Idle" \/ r.terminal # r.turn,
                     !.badContinuation = @ \/ (Suppressed /\ r.softStage < 5)]
\* A local command finishes without a native assistant/turn-duration record.
CompactCompletionEvidence ==
    /\ CompactCompletion /\ r.compactFence /\ ~r.compactEvidence
    /\ r' = [r EXCEPT !.compactEvidence = TRUE, !.phase = "Idle", !.tail = "local-output"]
FinishSelfCompactFence ==
    /\ r.compactFence /\ r.compactEvidence /\ Mode # "ready-compact-no-completion"
    /\ r' = [r EXCEPT !.compactFence = FALSE, !.compactReady = TRUE, !.awake = TRUE]
Next == WatchHeld \/ ChangeClearPending \/ PublishClearFile \/ CompactCompletionEvidence \/ FinishSelfCompactFence \/ PollDialog \/ PermissionRecord \/ SessionStart \/ Finish \/ Observe \/ Stop \/ OldStop \/ RecoverDraft \/ HumanClears \/
        SuppressStop \/ SoftSummary \/ LateAssistant \/
        NativeStart \/ Revoke \/ Submit \/ Restart \/ CompactSubmit \/ IdleWake \/
        (\E i \in Ids : Hold(i) \/ Prepare(i) \/ SelectBusy(i) \/ BusyInject(i) \/ BusyPublish(i) \/ ReexposeBusy(i) \/ BusyAbsorb(i) \/ BusyConsume(i) \/ Receipt(i) \/ Replay(i))
Fairness == WF_r(WatchHeld) /\ WF_r(ChangeClearPending) /\ WF_r(PublishClearFile) /\ WF_r(CompactCompletionEvidence) /\ WF_r(FinishSelfCompactFence) /\ WF_r(PollDialog) /\ WF_r(PermissionRecord) /\ WF_r(SessionStart) /\ WF_r(Finish) /\ WF_r(Observe) /\ WF_r(Stop) /\
            WF_r(SuppressStop) /\ WF_r(SoftSummary) /\ WF_r(LateAssistant) /\
            WF_r(RecoverDraft) /\ WF_r(HumanClears) /\ WF_r(NativeStart) /\
            WF_r(Revoke) /\ WF_r(Submit) /\ WF_r(CompactSubmit) /\
            (\A i \in Ids : WF_r(Hold(i)) /\ WF_r(Prepare(i)) /\
                           WF_r(SelectBusy(i)) /\ WF_r(BusyInject(i)) /\ WF_r(BusyPublish(i)) /\ WF_r(BusyAbsorb(i)) /\ WF_r(BusyConsume(i)) /\ WF_r(Receipt(i)))
Spec == Init /\ [][Next]_r /\ Fairness
TypeOK ==
    /\ r.q \in [Ids -> {"queued", "held", "prepared", "busy-selected", "busy-written", "native-queued", "sent", "done"}]
    /\ r.effects \in [Ids -> 0..2] /\ r.writes \in [Ids -> 0..2] /\ r.head \in 0..N
    /\ r.phase \in {"Booting", "Idle", "Running"}
    /\ r.dialogKeys \in 0..1 /\ r.permissionStage \in 0..3
NoDuplicate == \A i \in Ids : r.effects[i] <= 1 /\ r.writes[i] <= 1
NoSilentLoss == \A i \in Ids : r.q[i] = "done" => r.effects[i] = 1
NoIdleRunningInjection == ~r.badIdleRunning
NoHumanDraftExecution == ~r.badDraft
NoEarlyInput == ~r.badEarly
NoClearAuthorityViolation == ~r.badClear
CompactAfterStopIdle == ~r.badCompact
NoContinuationEarlyInput == ~r.badContinuation
BusyKeyPlanPreserved == \A i \in Ids : r.busyWrites[i] > 0 =>
    (r.route[i] = "busy" /\ r.busyWrites[i] = 1 /\ r.busyKeys[i] = "paste,Enter")
ClearHeldEventuallySends == ClearHeld => ((r.heldOnce /\ r.draft = "Empty") ~> (r.effects[1] = 1))
PendingEventuallyExecutes == \A i \in Ids : (r.q[i] \in {"queued", "held", "prepared", "busy-selected", "busy-written", "native-queued"}) ~> (r.effects[i] >= 1)
CompactEventuallyExecutes == r.compactPending ~> (r.compacts = 1)
SelfCompactEventuallyReleases == (r.compacts = 1 /\ r.compactEvidence) ~> (\E i \in Ids : r.writes[i] > 0)
BusyEventuallyQueues == Busy => <>(r.busyWrites[1] = 1)
=============================================================================
