-------------------------- MODULE ClaudeReadiness --------------------------
\* Readiness refinement; production code baseline 574af580b0c5d6e6ce280994f4ed243463de5d2d.
\* Idle submissions and the unchanged native busy queue have distinct actions.
EXTENDS Naturals, FiniteSets
CONSTANTS N, Mode
VARIABLE r
Ids == 1..N
Before == Mode = "ready-before-narrow"
Dual == Mode \in {"ready-dual-valid", "ready-dual-invalid", "ready-dual-recovery",
                   "ready-dual-stale-ledger", "ready-dual-bypass", "ready-dual-coupled"}
DualRecovery == Mode = "ready-dual-recovery"
DualBypass == Mode = "ready-dual-bypass"
DualCoupled == Mode = "ready-dual-coupled"
CurrentClear == Mode \in {"ready-current-clear-valid", "ready-current-clear-invalid",
    "ready-current-clear-toctou", "ready-current-clear-ignore-cert"}
CurrentToctou == Mode = "ready-current-clear-toctou"
CurrentIgnoreCert == Mode = "ready-current-clear-ignore-cert"
CertificateScenario == Dual \/ CurrentClear
Clear == Mode \in {"ready-clear", "ready-clear-before", "ready-clear-restart", "ready-clear-forged",
                    "ready-clear-no-revalidate", "ready-clear-drop-cursor", "ready-clear-split", "ready-clear-consumed",
                    "ready-clear-volatile", "ready-clear-volatile-before",
                    "ready-clear-replay", "ready-clear-replay-before", "ready-clear-old-output", "ready-clear-old-output-before", "ready-clear-unread"}
ClearHeld == Mode \in {"ready-clear-held-before", "ready-clear-held-after"}
ClearPreSource == Clear \/ ClearHeld \/ Mode \in {"ready-clear-pending-before", "ready-clear-pending-after", "ready-clear-pending-recheck"}
InputPreSource == ClearPreSource \/ CertificateScenario
Fresh == InputPreSource \/ Mode \in {"ready-fresh", "ready-clear", "ready-resume", "ready-late-dialog", "ready-preflight-only"}
Dialog == Mode \in {"ready-late-dialog", "ready-preflight-only"}
Permission == Mode \in {"ready-permission-later", "ready-permission-poison"}
Race == Mode \in {"ready-race", "ready-no-revalidate"}
Blind == Mode \in {"ready-unread", "ready-permanent-unread", "ready-draft-bypass", "ready-clear-unread"}
ManagedStash == Mode \in {"ready-stash-before", "ready-stash-after"}
CanPark == Mode = "ready-stash-after"
Person == ManagedStash \/ Mode \in {"ready-draft", "ready-draft-bypass"}
NoWake == Mode = "ready-no-wake"
ModalWatch == Mode \in {"ready-modal-watch-before", "ready-modal-watch-after"}
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
     oldBinding |-> InputPreSource, filePublished |-> ~InputPreSource,
     durableClear |-> IF Clear THEN Mode \in {"ready-clear-consumed", "ready-clear-volatile", "ready-clear-volatile-before"} ELSE InputPreSource,
     clearProof |-> Mode \notin {"ready-clear-forged", "ready-clear-no-revalidate"},
     proofReboots |-> IF Mode \in {"ready-clear-replay", "ready-clear-replay-before"} THEN 1 ELSE 0,
     clearDone |-> FALSE, clearConsumed |-> Mode = "ready-clear-consumed", oldCursor |-> 7,
     pendingSeq |-> IF InputPreSource THEN 1 ELSE 0,
     proofPending |-> 0, pendingChanges |-> IF Mode = "ready-clear-pending-recheck" THEN 1 ELSE 0,
     badClear |-> FALSE, heldOnce |-> FALSE,
     modalOpen |-> ModalWatch, modalHeld |-> FALSE, badModalWake |-> FALSE,
     tail |-> "none", continuing |-> FALSE, stopBoundary |-> 0, softStage |-> 0,
     stop |-> 0, oldStopLeft |-> IF ClearHeld \/ ModalWatch THEN 0 ELSE 1, awake |-> TRUE,
     idleWakes |-> IF ClearHeld \/ ModalWatch THEN 0 ELSE 2,
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
     stashRoundTrips |-> 0,
     badIdleRunning |-> FALSE, badDraft |-> FALSE, badEarly |-> FALSE,
     dialog |-> IF Dialog THEN "Boot" ELSE "None", dialogKeys |-> 0,
     permissionStage |-> 0, poisoned |-> FALSE,
     badCompact |-> FALSE, badContinuation |-> FALSE,
     ledgerCurrent |-> Mode # "ready-dual-stale-ledger",
     diskCurrent |-> Mode \in {"ready-dual-valid", "ready-dual-stale-ledger",
                                "ready-current-clear-valid", "ready-current-clear-toctou"},
     awaitingAdoption |-> CertificateScenario, adopted |-> FALSE,
     adoptionRefused |-> FALSE, repairLeft |-> DualRecovery,
     certificateObserved |-> FALSE, certificateChanged |-> FALSE]
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
    /\ (~Dual \/ (r.ledgerCurrent /\ (~DualCoupled \/ r.diskCurrent)))
    /\ (~CurrentClear \/ (r.ledgerCurrent /\ (r.diskCurrent \/ CurrentIgnoreCert)))
    /\ (~Clear \/ r.filePublished \/ (r.clearDone /\ (~r.clearConsumed \/ r.head # 0)))
    /\ (~r.oldBinding \/ (InputPreSource /\ Mode # "ready-clear-pending-before" /\ r.started /\ r.durableClear))
First == {i \in Ids : r.q[i] \in {"queued", "held"} /\
          \A j \in 1..(i-1) : r.q[j] = "done"}
SessionStart ==
    /\ r.phase = "Booting"
    /\ Mode \notin {"ready-clear-volatile", "ready-clear-volatile-before"}
    /\ ~Dialog \/ r.dialog = "Dismissed"
    /\ r' = [r EXCEPT !.phase = IF Clear THEN @ ELSE "Idle", !.started = TRUE,
                     !.durableClear = @ \/ (Clear /\ r.clearProof), !.awake = TRUE]
\* Restoring authenticated Pending retains the durable old source even if its volatile binding cache was lost.
ClearRetire ==
    /\ Clear /\ ~r.clearDone /\ ~r.clearConsumed
    /\ Mode \notin {"ready-clear-before", "ready-clear-volatile-before"}
    /\ (r.started \/ r.durableClear) /\ r.restarts = 0
    /\ r.clearProof \/ Mode = "ready-clear-no-revalidate"
    /\ r' = [r EXCEPT !.clearDone = TRUE, !.durableClear = TRUE, !.turn = 0,
                     !.phase = "Idle", !.started = TRUE, !.awake = TRUE,
                     !.oldBinding = IF Mode = "ready-clear-split" THEN FALSE ELSE @,
                     !.oldCursor = IF Mode = "ready-clear-drop-cursor" THEN 0 ELSE @,
                     !.badClear = @ \/ ~r.clearProof]
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
    /\ ~InputPreSource \/ r.filePublished \/ Mode = "ready-clear-old-output-before"
    /\ ~Busy \/ r.q[1] # "queued"
    /\ Mode # "ready-busy-finish-after" \/ r.writes[1] > 0
    /\ ~Suppressed \/ r.softStage >= 4
    /\ ~Permission \/ r.permissionStage = 3
    /\ r' = [r EXCEPT !.phase = "Idle", !.terminal = r.turn, !.tail = "hard",
                     !.badClear = @ \/ (InputPreSource /\ ~r.filePublished),
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
                     !.awake = IF NoWake \/ (ClearHeld /\ r.heldOnce) \/ (ModalWatch /\ r.modalHeld) THEN @ ELSE TRUE]
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
    /\ ~ManagedStash
    /\ r.human /\ r.draft = "Person" /\ (~ClearHeld \/ r.heldOnce)
    /\ r' = [r EXCEPT !.human = FALSE, !.draft = "Empty",
                     !.awake = IF ClearHeld THEN @ ELSE TRUE]
ComposerPrepared == r.draft = "Empty" \/ (~Fresh /\ r.draft = "Unread") \/ (CanPark /\ r.draft = "Person")
Hold(i) ==
    /\ i \in First /\ r.route[i] = "idle" /\ r.head = 0 /\ r.awake
    /\ ~ClearHeld \/ Ready
    /\ ~Ready \/ ~ComposerPrepared \/ (ModalWatch /\ r.modalOpen)
    /\ r' = [r EXCEPT !.q[i] = "held", !.awake = FALSE,
                     !.heldOnce = @ \/ ClearHeld,
                     !.modalHeld = @ \/ (ModalWatch /\ r.modalOpen)]
\* Source evidence and modal focus are independent; close itself emits no owner wake.
CloseModal ==
    /\ ModalWatch /\ r.modalHeld /\ r.modalOpen
    /\ r' = [r EXCEPT !.modalOpen = FALSE]
WatchModal ==
    /\ ModalWatch /\ r.modalHeld /\ ~r.awake /\ Ready
    /\ Mode = "ready-modal-watch-before" \/ ~r.modalOpen
    /\ r' = [r EXCEPT !.awake = TRUE, !.badModalWake = @ \/ r.modalOpen]
WatchHeld ==
    /\ ClearHeld /\ r.heldOnce /\ r.q[1] = "held" /\ r.draft = "Empty"
    /\ Ready /\ (~r.oldBinding \/ Mode # "ready-clear-held-before")
    /\ r' = [r EXCEPT !.awake = TRUE]
Prepare(i) ==
    /\ ~ModalWatch \/ ~r.modalOpen
    /\ ~CompactCompletion \/ ~r.compactPending
    /\ i \in First /\ r.route[i] = "idle" /\ r.head = 0 /\ r.awake /\ Ready
    /\ ComposerPrepared \/ Mode = "ready-draft-bypass"
    /\ r' = [r EXCEPT !.q[i] = "prepared", !.head = i,
                     !.proofEpoch = r.epoch, !.proofTurn = r.turn, !.proofPending = r.pendingSeq,
                     !.clearConsumed = IF Clear THEN TRUE ELSE @,
                     !.certificateObserved = @ \/ CurrentClear]
NativeStart ==
    /\ Race /\ r.head # 0 /\ r.nativeStarts > 0 /\ r.phase = "Idle"
    /\ r' = [r EXCEPT !.phase = "Running", !.turn = @ + 1, !.nativeStarts = 0,
                     !.tail = "assistant"]
ProofCurrent == Ready /\ r.proofEpoch = r.epoch /\ r.proofTurn = r.turn
    /\ (~InputPreSource \/ r.proofPending = r.pendingSeq)
ChangeClearPending ==
    /\ InputPreSource /\ r.head # 0 /\ r.pendingChanges > 0
    /\ r' = [r EXCEPT !.pendingSeq = @ + 1, !.pendingChanges = 0]
PublishClearFile ==
    /\ InputPreSource /\ r.filePublished /\ r.oldBinding
    /\ r' = [r EXCEPT !.oldBinding = FALSE]
\* Historical and current certificates share the last pre-key adoption check, not public readiness.
AdoptAwaitedClear ==
    /\ CertificateScenario /\ r.awaitingAdoption /\ r.head # 0 /\ Ready /\ r.diskCurrent
    /\ ~CurrentToctou \/ r.certificateChanged
    /\ r' = [r EXCEPT !.oldBinding = FALSE, !.awaitingAdoption = FALSE, !.adopted = TRUE]
RefuseAwaitedClear ==
    /\ CertificateScenario /\ ~DualBypass /\ r.awaitingAdoption /\ r.head # 0
    /\ Ready \/ (CurrentClear /\ r.certificateObserved)
    /\ ~r.diskCurrent
    /\ r' = [r EXCEPT !.q[r.head] = "held", !.head = 0,
                     !.awake = FALSE, !.adoptionRefused = TRUE]
RepairClearCertificate ==
    /\ DualRecovery /\ r.adoptionRefused /\ r.repairLeft
    /\ r' = [r EXCEPT !.diskCurrent = TRUE, !.repairLeft = FALSE, !.awake = TRUE]
\* The race occurs after proof observation and before adoption or the first key.
InvalidateObservedCertificate ==
    /\ CurrentToctou /\ r.head # 0 /\ r.certificateObserved /\ ~r.certificateChanged /\ ~r.adopted
    /\ r' = [r EXCEPT !.diskCurrent = FALSE, !.certificateChanged = TRUE]
Revoke ==
    /\ r.head # 0 /\ (~ProofCurrent \/ ~ComposerPrepared)
    /\ r' = [r EXCEPT !.q[r.head] = "held", !.head = 0, !.awake = FALSE]
Submit ==
    /\ (~CertificateScenario \/ ~r.awaitingAdoption \/ DualBypass)
    /\ (~InputPreSource \/ r.pendingChanges = 0)
    /\ r.head # 0 /\ (~Race \/ r.nativeStarts = 0)
    /\ ProofCurrent \/ Mode = "ready-no-revalidate"
    /\ ComposerPrepared \/ Mode = "ready-draft-bypass"
    /\ r' = [r EXCEPT !.q[r.head] = "sent", !.effects[r.head] = @ + 1,
                     !.writes[r.head] = @ + 1, !.head = 0,
                     !.phase = "Running", !.turn = @ + 1, !.tail = "assistant",
                     !.compactReady = FALSE, !.awake = FALSE,
                     !.badIdleRunning = @ \/ r.phase = "Running",
                     !.badDraft = @ \/ (r.human /\ r.draft = "Person" /\ ~CanPark),
                     !.stashRoundTrips = @ + IF CanPark /\ r.human THEN 1 ELSE 0,
                     !.badEarly = @ \/ (r.needsStart /\ ~r.started),
                     !.badClear = @ \/ (r.oldBinding /\ (~r.started \/ ~r.durableClear \/ r.proofPending # r.pendingSeq)),
                     !.filePublished = IF InputPreSource THEN Mode \notin {"ready-clear-replay", "ready-clear-replay-before", "ready-clear-old-output", "ready-clear-old-output-before"} ELSE @,
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
    /\ ~Clear \/ (r.durableClear /\ ~r.clearDone)
    /\ r' = [r EXCEPT !.epoch = @ + 1, !.observed = "Unknown",
                     !.started = FALSE, !.awake = FALSE, !.restarts = 0]
\* A daemon restart loses local readiness, but keeps the consumed Pending certificate.
RestartClaimed ==
    /\ Clear /\ r.clearConsumed /\ r.proofReboots > 0 /\ r.head = 0 /\ r.effects[1] = 1
    /\ r' = [r EXCEPT !.proofReboots = 0, !.clearDone = FALSE,
                     !.clearConsumed = Mode # "ready-clear-replay-before",
                     !.phase = "Booting", !.started = FALSE, !.observed = "Unknown",
                     !.q[1] = "queued", !.awake = TRUE]
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
Next == AdoptAwaitedClear \/ RefuseAwaitedClear \/ RepairClearCertificate \/ InvalidateObservedCertificate \/ RestartClaimed \/ ClearRetire \/ CloseModal \/ WatchModal \/ WatchHeld \/ ChangeClearPending \/ PublishClearFile \/ CompactCompletionEvidence \/ FinishSelfCompactFence \/ PollDialog \/ PermissionRecord \/ SessionStart \/ Finish \/ Observe \/ Stop \/ OldStop \/ RecoverDraft \/ HumanClears \/
        SuppressStop \/ SoftSummary \/ LateAssistant \/
        NativeStart \/ Revoke \/ Submit \/ Restart \/ CompactSubmit \/ IdleWake \/
        (\E i \in Ids : Hold(i) \/ Prepare(i) \/ SelectBusy(i) \/ BusyInject(i) \/ BusyPublish(i) \/ ReexposeBusy(i) \/ BusyAbsorb(i) \/ BusyConsume(i) \/ Receipt(i) \/ Replay(i))
Fairness == WF_r(AdoptAwaitedClear) /\ WF_r(RefuseAwaitedClear) /\ WF_r(RepairClearCertificate) /\ WF_r(InvalidateObservedCertificate) /\ WF_r(RestartClaimed) /\ WF_r(ClearRetire) /\ WF_r(Restart) /\ WF_r(CloseModal) /\ WF_r(WatchModal) /\ WF_r(WatchHeld) /\ WF_r(ChangeClearPending) /\ WF_r(PublishClearFile) /\ WF_r(CompactCompletionEvidence) /\ WF_r(FinishSelfCompactFence) /\ WF_r(PollDialog) /\ WF_r(PermissionRecord) /\ WF_r(SessionStart) /\ WF_r(Finish) /\ WF_r(Observe) /\ WF_r(Stop) /\
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
NoUnsafeClearRetire == ~r.badClear
ClearCursorPreserved == r.oldCursor = 7
ClearAtomic == ClearPreSource => (~r.filePublished => r.oldBinding)
NoModalOpenWake == ~r.badModalWake
ModalCloseEventuallySends == ModalWatch => ((r.modalHeld /\ ~r.modalOpen) ~> (r.effects[1] = 1))
NoDuplicate == \A i \in Ids : r.effects[i] <= 1 /\ r.writes[i] <= 1
NoSilentLoss == \A i \in Ids : r.q[i] = "done" => r.effects[i] = 1
NoIdleRunningInjection == ~r.badIdleRunning
NoHumanDraftExecution == ~r.badDraft
ManagedDraftPreserved == ManagedStash =>
    (r.human /\ r.draft = "Person" /\
     r.stashRoundTrips = Cardinality({i \in Ids : r.effects[i] > 0}))
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
ClearAdoptionBeforeKey == Dual =>
    (\A i \in Ids : r.writes[i] > 0 => (r.adopted /\ ~r.awaitingAdoption))
ClearRefusalKeepsOriginal == (Dual /\ r.adoptionRefused /\ ~r.adopted) =>
    (r.oldBinding /\ (\A i \in Ids : r.writes[i] = 0 /\ r.q[i] \in {"queued", "held", "prepared"}))
LedgerRejectedNoKey == (Dual /\ ~r.ledgerCurrent) => (\A i \in Ids : r.writes[i] = 0)
PublicReadyDoesNotAdopt == (Dual /\ Ready /\ r.awaitingAdoption) => r.oldBinding
PublicLedgerReadyEventually == (Dual /\ r.ledgerCurrent) => <>Ready
InvalidCertificateEventuallyHeld == (Dual /\ r.ledgerCurrent /\ ~r.diskCurrent) =>
    <>(r.adoptionRefused /\ r.q[1] = "held")
LedgerRejectedEventuallyHeld == (Dual /\ ~r.ledgerCurrent) => <>(r.q[1] = "held")
CurrentReadyRequiresCertificate == (CurrentClear /\ Ready) => (r.ledgerCurrent /\ r.diskCurrent)
CurrentNoKeyWithoutCertificate == CurrentClear =>
    (\A i \in Ids : r.writes[i] > 0 => (r.diskCurrent /\ r.adopted /\ ~r.awaitingAdoption))
CurrentInvalidKeepsOriginal == (CurrentClear /\ ~r.diskCurrent) =>
    (r.oldBinding /\ (\A i \in Ids : r.writes[i] = 0))
CurrentToctouNoKey == (CurrentToctou /\ r.certificateChanged) =>
    (r.certificateObserved /\ r.oldBinding /\ (\A i \in Ids : r.writes[i] = 0))
CurrentReadyEventually == (Mode = "ready-current-clear-valid") => <>Ready
CurrentBlockedEventuallyHeld == (CurrentClear /\ ~r.diskCurrent) => <>(r.q[1] = "held")
CurrentObservedProofLosesCertificate == CurrentToctou =>
    <>(r.certificateObserved /\ r.certificateChanged /\ r.q[1] = "held")
=============================================================================
