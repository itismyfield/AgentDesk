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
\* A resumed run starts over its old transcript: a finished turn under exit metadata, or a turn cut short.
\* The daemon restarts over the resumed session and attaches the same transcript again: the start is
\* gone and a recovery floor stands at the transcript's end. Only the retaining modes end that wait.
\* An idle session whose turn ended before the restart: the code puts the same floor at its end.
\* The restored modes rebuild readiness from what the earlier incarnation recorded durably: a
\* closed, non-continuing turn of this execution, or this execution's resume start with no turn yet.
\* The terminal-only mutant trusts the transcript's last terminal alone.
\* A resume start is restored at observation. The late placement restores it only past the shared
\* observer's early return, which an old cut or permission-marked history never passes.
IdleRestartFloor == Mode = "ready-idle-restart-before"
IdleRestartRestored == Mode \in {"ready-idle-restart-after", "ready-idle-restart-continuing",
                                 "ready-idle-restart-terminal-only"}
IdleRestart == IdleRestartFloor \/ IdleRestartRestored
ResumeRestartRestored == Mode \in {"ready-resume-restart-durable", "ready-resume-restart-durable-cut",
                                   "ready-resume-restart-durable-late", "ready-resume-restart-durable-person",
                                   "ready-resume-restart-durable-no-quiet", "ready-resume-restart-tmux",
                                   "ready-resume-restart-durable-marker", "ready-resume-restart-durable-marker-l0",
                                   "ready-resume-restart-durable-turn", "ready-resume-restart-durable-turn-no-quiet"}
\* A tmux resume before its start was recorded: the restart finds no record to restore from.
ResumeRestartUnrecorded == Mode = "ready-resume-restart-tmux-before"
\* A permission marker lands after the start, before or after the restart. The recorded start length
\* keeps it a turn of this run; the L0 mutant measures from the length at restore and absorbs one
\* that landed before the restart.
Marker == Mode \in {"ready-resume-restart-durable-marker", "ready-resume-restart-durable-marker-l0"}
MarkerL0 == Mode = "ready-resume-restart-durable-marker-l0"
LateRestore == Mode = "ready-resume-restart-durable-late"
\* This run closes a turn after the recorded start, then the daemon restarts. The no-quiet variant
\* restores the start without the quiet judgment and so withholds the closed turn's own proof.
TurnFirst == Mode \in {"ready-resume-restart-durable-turn", "ready-resume-restart-durable-turn-no-quiet"}
RestoreNoQuiet == Mode = "ready-resume-restart-durable-turn-no-quiet"
Restored == IdleRestartRestored \/ ResumeRestartRestored
RetainsUnproven == IdleRestart \/ ResumeRestartRestored \/ ResumeRestartUnrecorded \/
                   Mode \in {"ready-resume-restart-after", "ready-resume-restart-person",
                             "ready-resume-restart-any-timeout", "ready-resume-restart-cut"}
ResumeRestart == Mode \in {"ready-resume-restart-after", "ready-resume-restart-person",
                           "ready-resume-restart-any-timeout", "ready-resume-restart-cut",
                           "ready-resume-restart-before"}
ResumeAfter == ResumeRestart \/ ResumeRestartRestored \/ ResumeRestartUnrecorded \/
               Mode \in {"ready-resume-meta-after", "ready-resume-cut-after", "ready-resume-person-after"}
ResumeNoQuiet == Mode \in {"ready-resume-no-quiet", "ready-resume-restart-durable-no-quiet"}
ResumeCut == Mode \in {"ready-resume-cut-before", "ready-resume-cut-after", "ready-resume-restart-cut",
                       "ready-resume-restart-durable-cut", "ready-resume-restart-durable-late"}
ResumeHistory == ResumeAfter \/ ResumeNoQuiet \/ ResumeCut \/ Mode = "ready-resume-meta-before"
ResumePerson == Mode \in {"ready-resume-person-after", "ready-resume-no-quiet",
                          "ready-resume-restart-person", "ready-resume-restart-any-timeout",
                          "ready-resume-restart-durable-person", "ready-resume-restart-durable-no-quiet",
                          "ready-resume-restart-durable-turn", "ready-resume-restart-durable-turn-no-quiet"}
Fresh == ClearPreSource \/ ResumeHistory \/ Mode \in {"ready-fresh", "ready-clear", "ready-resume", "ready-late-dialog", "ready-preflight-only"}
Dialog == Mode \in {"ready-late-dialog", "ready-preflight-only"}
Permission == Mode \in {"ready-permission-later", "ready-permission-poison"}
Race == Mode \in {"ready-race", "ready-no-revalidate"}
Blind == Mode \in {"ready-unread", "ready-permanent-unread", "ready-draft-bypass"}
ManagedStash == Mode \in {"ready-stash-before", "ready-stash-after"}
CanPark == Mode = "ready-stash-after"
Person == ManagedStash \/ Mode \in {"ready-draft", "ready-draft-bypass"}
NoWake == Mode = "ready-no-wake"
ModalWatch == Mode \in {"ready-modal-watch-before", "ready-modal-watch-after"}
Busy == Mode \in {"ready-busy-active", "ready-busy-native", "ready-block-busy", "ready-busy-finish-before",
                  "ready-busy-finish-after", "ready-busy-reexpose"}
Suppressed == Mode \in {"ready-suppressed-stop", "ready-no-latch",
                        "ready-suppressed-compact", "ready-no-latch-compact",
                        "ready-idle-restart-continuing", "ready-idle-restart-terminal-only"}
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
     restarts |-> IF Mode = "ready-restart" \/ ResumeRestart \/ IdleRestart \/ ResumeRestartRestored
                     \/ ResumeRestartUnrecorded THEN 1 ELSE 0,
     durable |-> 0, restore |-> "none", base |-> 0, marks |-> IF Marker THEN 1 ELSE 0, marked |-> FALSE,
     restored |-> "no", notices |-> [i \in Ids |-> 0], badExpire |-> FALSE,
     compactPending |-> Compact, compacts |-> 0,
     compactFence |-> FALSE, compactEvidence |-> FALSE, compactReady |-> FALSE,
     stashRoundTrips |-> 0,
     badIdleRunning |-> FALSE, badDraft |-> FALSE, badEarly |-> FALSE,
     dialog |-> IF Dialog THEN "Boot" ELSE "None", dialogKeys |-> 0,
     permissionStage |-> 0, poisoned |-> FALSE,
     badCompact |-> FALSE, badContinuation |-> FALSE,
     history |-> IF ResumeCut THEN "cut" ELSE IF ResumeHistory THEN "meta" ELSE "none",
     personTurns |-> IF ResumePerson THEN 1 ELSE 0]
Init == r = Initial
TerminalEligible ==
    /\ ~r.continuing
    /\ r.tail = "hard" \/ (r.tail = "summary" /\ r.stopBoundary = r.turn)
\* The start alone admits an empty transcript. Over old history it admits only while no turn of
\* this run exists; the no-quiet mutant drops that condition.
ResumedStart == r.started /\ ((ResumeAfter /\ r.turn = r.base) \/ ResumeNoQuiet)
CurrentEvidence ==
    /\ (r.observed = "Idle" /\ r.seenEpoch = r.epoch /\ r.seenTurn = r.turn) \/ ResumedStart
    /\ (~r.needsStart \/ r.started \/ r.restored = "passed")
    \* A restored resume start keeps the restart floor; only the start itself admits the first input.
    /\ r.restored # "floor" \/ (ResumeRestartRestored /\ ResumedStart)
    /\ ~r.continuing
    /\ ~r.compactFence
    /\ (r.started /\ r.turn = 0 /\ r.history = "none") \/ ResumedStart \/ r.compactReady \/
       (r.terminal = r.turn /\ r.turn > 0 /\ TerminalEligible)
Ready == CurrentEvidence /\ ~Before /\ ~r.poisoned
    /\ (~r.oldBinding \/ (ClearPreSource /\ Mode # "ready-clear-pending-before" /\ r.started /\ r.durableClear))
\* Idle bytes the wait has read, nothing in flight on them, and still no proof: only new activity
\* in the pane can change that, so no later look does.
Unproven == r.phase = "Idle" /\ r.observed = "Idle" /\ r.seenEpoch = r.epoch /\ r.seenTurn = r.turn
    /\ ~r.continuing /\ ~r.compactFence /\ ~CurrentEvidence
First == {i \in Ids : r.q[i] \in {"queued", "held"} /\
          \A j \in 1..(i-1) : r.q[j] \in {"done", "retained"}}
SessionStart ==
    /\ r.phase = "Booting"
    /\ ~Dialog \/ r.dialog = "Dismissed"
    /\ r' = [r EXCEPT !.phase = "Idle", !.started = TRUE, !.awake = TRUE, !.restored = "no"]
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
    /\ r' = [r EXCEPT !.phase = "Idle", !.terminal = r.turn, !.tail = "hard", !.durable = r.turn,
                     !.restored = IF @ = "floor" THEN "passed" ELSE @,
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
ObservedNow == IF r.tail = "summary" THEN "Idle"
               ELSE IF r.phase = "Booting" THEN "Unknown"
               ELSE IF r.turn = 0 /\ r.history = "cut" THEN "Running" ELSE r.phase
\* The code's quiet judgment on the same tail: no turn record or marker after the recorded start
\* length (base). The no-quiet mutants skip it; the L0 mutant measures from the length at restore.
RecordQuiet == r.turn = r.base \/ ResumeNoQuiet \/ RestoreNoQuiet \/ MarkerL0
\* The same observation restores this execution's recorded resume start only when it is quiet; the
\* late placement only past an Idle observer. A restored start keeps the floor and so ends R1.
RestoresStart == r.restore = "pending" /\ RecordQuiet /\ (~LateRestore \/ ObservedNow = "Idle")
Observe ==
    /\ r' = [r EXCEPT !.observed = ObservedNow,
                     !.restore = IF RestoresStart THEN "done" ELSE @,
                     !.started = @ \/ RestoresStart,
                     !.restored = IF RestoresStart THEN "floor" ELSE @,
                     !.base = IF RestoresStart /\ MarkerL0 THEN r.turn ELSE @,
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
ComposerPrepared == r.draft = "Empty" \/ (CanPark /\ r.draft = "Person")
Hold(i) ==
    /\ i \in First /\ r.route[i] = "idle" /\ r.head = 0 /\ r.awake
    /\ ~ClearHeld \/ Ready
    /\ ~Ready \/ ~ComposerPrepared \/ (ModalWatch /\ r.modalOpen)
    /\ ~(RetainsUnproven /\ Unproven)
    /\ r' = [r EXCEPT !.q[i] = "held", !.awake = FALSE,
                     !.heldOnce = @ \/ ClearHeld,
                     !.modalHeld = @ \/ (ModalWatch /\ r.modalOpen)]
\* The readiness budget ends on an unproven source: the owner retains the original with one resend
\* ask and releases the front. The any-timeout mutant retains whatever the wait was for.
Expire(i) ==
    /\ RetainsUnproven /\ i \in First /\ r.route[i] = "idle" /\ r.head = 0 /\ r.awake
    /\ Unproven \/ (Mode = "ready-resume-restart-any-timeout" /\ ~Ready)
    /\ r' = [r EXCEPT !.q[i] = "retained", !.notices[i] = @ + 1,
                     !.badExpire = @ \/ ~Unproven]
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
    /\ r.head # 0 /\ (~ProofCurrent \/ ~ComposerPrepared)
    /\ r' = [r EXCEPT !.q[r.head] = "held", !.head = 0, !.awake = FALSE]
Submit ==
    /\ (~ClearPreSource \/ r.pendingChanges = 0)
    /\ r.head # 0 /\ (~Race \/ r.nativeStarts = 0)
    /\ ProofCurrent \/ Mode = "ready-no-revalidate"
    /\ ComposerPrepared \/ Mode = "ready-draft-bypass"
    /\ r' = [r EXCEPT !.q[r.head] = "sent", !.effects[r.head] = @ + 1,
                     !.writes[r.head] = @ + 1, !.head = 0,
                     !.phase = "Running", !.turn = @ + 1, !.tail = "assistant",
                     !.compactReady = FALSE, !.awake = FALSE,
                     !.badIdleRunning = @ \/ r.phase = "Running",
                     !.badDraft = @ \/ (r.human /\ ~CanPark),
                     !.stashRoundTrips = @ + IF CanPark /\ r.human THEN 1 ELSE 0,
                     !.badEarly = @ \/ (r.needsStart /\ ~r.started /\ r.restored # "passed"),
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
\* An untimestamped permission marker after the start opens a turn of this run.
PermissionMarker ==
    /\ r.marks > 0 /\ r.phase = "Idle" /\ r.turn = 0
    /\ r.started \/ r.restore = "pending"
    /\ r' = [r EXCEPT !.marks = 0, !.marked = TRUE, !.phase = "Running", !.turn = 1,
                     !.tail = "permission", !.terminal = 0]
\* A person types into the resumed pane before the queued input goes.
PersonTurn ==
    /\ r.personTurns > 0 /\ r.phase = "Idle" /\ (r.started \/ r.restored = "floor")
    /\ r' = [r EXCEPT !.phase = "Running", !.turn = @ + 1, !.tail = "assistant", !.personTurns = 0]
Receipt(i) ==
    /\ r.q[i] = "sent"
    /\ r' = [r EXCEPT !.q[i] = "done"]
\* What a restored restart may rebuild from: the closed turn the transcript still ends on, recorded
\* as this execution's final Stop, or this execution's resume start while no turn followed it.
RestoreProof ==
    IF Mode = "ready-idle-restart-terminal-only" THEN r.terminal = r.turn /\ r.turn > 0
    ELSE r.durable = r.turn /\ r.terminal = r.turn /\ r.turn > 0
\* Only pre-key daemon restart is refined here; post-key Unknown is never replayed.
\* The in-memory continuation latch does not survive the restart in the idle-restart modes.
Restart ==
    /\ r.restarts > 0 /\ r.head = 0
    /\ ~TurnFirst \/ (r.personTurns = 0 /\ r.phase = "Idle")
    /\ r' = [r EXCEPT !.epoch = @ + 1, !.observed = "Unknown",
                     !.restored = IF ResumeRestart \/ IdleRestartFloor THEN "floor"
                                  ELSE IF Restored THEN (IF RestoreProof THEN "passed" ELSE "floor")
                                  ELSE @,
                     !.started = FALSE,
                     !.restore = IF ResumeRestartRestored /\ r.started THEN "pending" ELSE "none",
                     !.continuing = IF IdleRestart THEN FALSE ELSE @,
                     !.awake = FALSE, !.restarts = 0]
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
Next == CloseModal \/ WatchModal \/ WatchHeld \/ ChangeClearPending \/ PublishClearFile \/ CompactCompletionEvidence \/ FinishSelfCompactFence \/ PollDialog \/ PermissionRecord \/ SessionStart \/ Finish \/ Observe \/ Stop \/ OldStop \/ RecoverDraft \/ HumanClears \/
        SuppressStop \/ SoftSummary \/ LateAssistant \/
        NativeStart \/ Revoke \/ Submit \/ Restart \/ CompactSubmit \/ IdleWake \/ PersonTurn \/ PermissionMarker \/
        (\E i \in Ids : Hold(i) \/ Expire(i) \/ Prepare(i) \/ SelectBusy(i) \/ BusyInject(i) \/ BusyPublish(i) \/ ReexposeBusy(i) \/ BusyAbsorb(i) \/ BusyConsume(i) \/ Receipt(i) \/ Replay(i))
Fairness == WF_r(CloseModal) /\ WF_r(WatchModal) /\ WF_r(WatchHeld) /\ WF_r(ChangeClearPending) /\ WF_r(PublishClearFile) /\ WF_r(CompactCompletionEvidence) /\ WF_r(FinishSelfCompactFence) /\ WF_r(PollDialog) /\ WF_r(PermissionRecord) /\ WF_r(SessionStart) /\ WF_r(Finish) /\ WF_r(Observe) /\ WF_r(Stop) /\
            WF_r(SuppressStop) /\ WF_r(SoftSummary) /\ WF_r(LateAssistant) /\
            WF_r(RecoverDraft) /\ WF_r(HumanClears) /\ WF_r(NativeStart) /\
            WF_r(Revoke) /\ WF_r(Submit) /\ WF_r(CompactSubmit) /\
            (\A i \in Ids : WF_r(Hold(i)) /\ WF_r(Expire(i)) /\ WF_r(Prepare(i)) /\
                           WF_r(SelectBusy(i)) /\ WF_r(BusyInject(i)) /\ WF_r(BusyPublish(i)) /\ WF_r(BusyAbsorb(i)) /\ WF_r(BusyConsume(i)) /\ WF_r(Receipt(i)))
Spec == Init /\ [][Next]_r /\ Fairness
TypeOK ==
    /\ r.q \in [Ids -> {"queued", "held", "prepared", "busy-selected", "busy-written", "native-queued", "sent", "done", "retained"}]
    /\ r.restored \in {"no", "floor", "passed"} /\ r.notices \in [Ids -> 0..2]
    /\ r.effects \in [Ids -> 0..2] /\ r.writes \in [Ids -> 0..2] /\ r.head \in 0..N
    /\ r.phase \in {"Booting", "Idle", "Running"}
    /\ r.dialogKeys \in 0..1 /\ r.permissionStage \in 0..3
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
\* A retained original took no key, owes exactly one resend ask, and never leaves custody again.
RetainedOnce == \A i \in Ids : r.notices[i] <= 1 /\ (r.notices[i] = 1 <=> r.q[i] = "retained")
    /\ (r.q[i] = "retained" => r.writes[i] = 0 /\ r.effects[i] = 0)
ExpiryOnlyUnproven == ~r.badExpire
PendingEventuallySettles == \A i \in Ids : (r.q[i] \in {"queued", "held", "prepared"}) ~> (r.effects[i] >= 1 \/ r.q[i] = "retained")
\* After a daemon restart an idle session with an empty composer still takes an input.
RestartedIdleEventuallyReceives == (IdleRestart \/ ResumeRestartRestored \/ ResumeRestartUnrecorded) =>
    ((r.epoch > 1 /\ r.phase = "Idle" /\ r.draft = "Empty") ~> (\E i \in Ids : r.effects[i] >= 1))
ResumedIdleEventuallyReceives == ResumeHistory =>
    ((r.started /\ r.phase = "Idle" /\ r.draft = "Empty") ~> (\E i \in Ids : r.effects[i] >= 1))
=============================================================================
