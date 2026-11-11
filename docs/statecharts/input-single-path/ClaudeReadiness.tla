-------------------------- MODULE ClaudeReadiness --------------------------
\* Readiness refinement of InputPath; code baseline 574af580b0c5d6e6ce280994f4ed243463de5d2d.
\* One record makes the refinement inert in the older InputPath configurations.
EXTENDS Naturals, FiniteSets
CONSTANTS N, Mode
VARIABLE r
Ids == 1..N
Before == Mode = "ready-before-narrow"
Fresh == Mode \in {"ready-fresh", "ready-clear", "ready-resume"}
Race == Mode \in {"ready-race", "ready-no-revalidate"}
Blind == Mode \in {"ready-unread", "ready-permanent-unread", "ready-draft-bypass"}
Person == Mode \in {"ready-draft", "ready-draft-bypass"}
NoWake == Mode = "ready-no-wake"
Compact == Mode \in {"ready-compact", "ready-busy-compact"}
Initial ==
    [phase |-> IF Fresh THEN "Booting" ELSE "Running",
     epoch |-> 1, turn |-> IF Fresh THEN 0 ELSE 1,
     observed |-> "Unknown", seenEpoch |-> 0, seenTurn |-> 0,
     started |-> FALSE, needsStart |-> Fresh, terminal |-> 0,
     stop |-> 0, oldStopLeft |-> 1, awake |-> TRUE,
     human |-> Person, draft |-> IF Blind THEN "Unread" ELSE IF Person THEN "Person" ELSE "Empty",
     q |-> [i \in Ids |-> "queued"], effects |-> [i \in Ids |-> 0],
     head |-> 0, proofEpoch |-> 0, proofTurn |-> 0,
     nativeStarts |-> IF Race THEN 1 ELSE 0,
     restarts |-> IF Mode = "ready-restart" THEN 1 ELSE 0,
     compactPending |-> Compact, compacts |-> 0,
     badRunning |-> FALSE, badDraft |-> FALSE, badEarly |-> FALSE, badCompact |-> FALSE]
Init == r = Initial
CurrentEvidence ==
    /\ r.observed = "Idle" /\ r.seenEpoch = r.epoch /\ r.seenTurn = r.turn
    /\ (~r.needsStart \/ r.started)
    /\ r.started \/ (r.terminal = r.turn /\ r.turn > 0)
Ready == CurrentEvidence /\ ~Before
First == {i \in Ids : r.q[i] \in {"queued", "held"} /\
          \A j \in 1..(i-1) : r.q[j] = "done"}
SessionStart ==
    /\ r.phase = "Booting"
    /\ r' = [r EXCEPT !.phase = "Idle", !.started = TRUE, !.awake = TRUE]
Finish ==
    /\ r.phase = "Running"
    /\ r' = [r EXCEPT !.phase = "Idle", !.terminal = r.turn]
\* The periodic observation runs even when no hook or mtime edge was seen.
Observe ==
    /\ r' = [r EXCEPT !.observed = IF r.phase = "Booting" THEN "Unknown" ELSE r.phase,
                     !.seenEpoch = r.epoch, !.seenTurn = r.turn,
                     !.awake = IF NoWake THEN @ ELSE TRUE]
\* Stop and duplicate/old delivery are wake hints; neither changes transcript evidence.
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
    /\ r.human /\ r.draft = "Person"
    /\ r' = [r EXCEPT !.human = FALSE, !.draft = "Empty", !.awake = TRUE]
Hold(i) ==
    /\ i \in First /\ r.head = 0 /\ r.awake
    /\ ~Ready \/ r.draft # "Empty"
    /\ r' = [r EXCEPT !.q[i] = "held", !.awake = FALSE]
Prepare(i) ==
    /\ i \in First /\ r.head = 0 /\ r.awake /\ Ready
    /\ r.draft = "Empty" \/ Mode = "ready-draft-bypass"
    /\ r' = [r EXCEPT !.q[i] = "prepared", !.head = i,
                     !.proofEpoch = r.epoch, !.proofTurn = r.turn]
\* An observed native start between readiness and the first key revokes the proof.
\* Atomic observation of an unreported native key is an explicit implementation limit.
NativeStart ==
    /\ Race /\ r.head # 0 /\ r.nativeStarts > 0 /\ r.phase = "Idle"
    /\ r' = [r EXCEPT !.phase = "Running", !.turn = @ + 1, !.nativeStarts = 0]
ProofCurrent == Ready /\ r.proofEpoch = r.epoch /\ r.proofTurn = r.turn
Revoke ==
    /\ r.head # 0 /\ (~ProofCurrent \/ r.draft # "Empty")
    /\ r' = [r EXCEPT !.q[r.head] = "held", !.head = 0, !.awake = FALSE]
Submit ==
    /\ r.head # 0 /\ (~Race \/ r.nativeStarts = 0)
    /\ ProofCurrent \/ Mode = "ready-no-revalidate"
    /\ r.draft = "Empty" \/ Mode = "ready-draft-bypass"
    /\ r' = [r EXCEPT !.q[r.head] = "sent", !.effects[r.head] = @ + 1,
                     !.head = 0, !.phase = "Running", !.turn = @ + 1,
                     !.awake = FALSE,
                     !.badRunning = @ \/ r.phase = "Running",
                     !.badDraft = @ \/ r.human,
                     !.badEarly = @ \/ (r.needsStart /\ ~r.started)]
Receipt(i) ==
    /\ r.q[i] = "sent"
    /\ r' = [r EXCEPT !.q[i] = "done"]
\* Pre-key daemon restart only: the originals survive, all ephemeral readiness is discarded.
\* Post-key Unknown retention remains in InputPath/HerdrBusyHandoff, without replay here.
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
                     !.phase = "Running", !.turn = @ + 1, !.awake = FALSE,
                     !.badCompact = @ \/ r.phase # "Idle" \/ r.terminal # r.turn]
Next == SessionStart \/ Finish \/ Observe \/ Stop \/ OldStop \/ RecoverDraft \/ HumanClears \/
        NativeStart \/ Revoke \/ Submit \/ Restart \/ CompactSubmit \/
        (\E i \in Ids : Hold(i) \/ Prepare(i) \/ Receipt(i) \/ Replay(i))
Fairness == WF_r(SessionStart) /\ WF_r(Finish) /\ WF_r(Observe) /\ WF_r(Stop) /\
            WF_r(RecoverDraft) /\ WF_r(HumanClears) /\ WF_r(NativeStart) /\
            WF_r(Revoke) /\ WF_r(Submit) /\ WF_r(CompactSubmit) /\
            (\A i \in Ids : WF_r(Hold(i)) /\ WF_r(Prepare(i)) /\ WF_r(Receipt(i)))
Spec == Init /\ [][Next]_r /\ Fairness
TypeOK ==
    /\ r.q \in [Ids -> {"queued", "held", "prepared", "sent", "done"}]
    /\ r.effects \in [Ids -> 0..2] /\ r.head \in 0..N
    /\ r.phase \in {"Booting", "Idle", "Running"}
NoDuplicate == \A i \in Ids : r.effects[i] <= 1
NoSilentLoss == \A i \in Ids : r.q[i] = "done" => r.effects[i] = 1
NoRunningInjection == ~r.badRunning
NoHumanDraftExecution == ~r.badDraft
NoEarlyInput == ~r.badEarly
CompactAfterStopIdle == ~r.badCompact
PendingEventuallyExecutes == \A i \in Ids : (r.q[i] \in {"queued", "held", "prepared"}) ~> (r.effects[i] >= 1)
CompactEventuallyExecutes == r.compactPending ~> (r.compacts = 1)
=============================================================================
