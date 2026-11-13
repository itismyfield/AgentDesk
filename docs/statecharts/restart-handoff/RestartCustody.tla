-------------------------- MODULE RestartCustody --------------------------
EXTENDS Naturals, FiniteSets
CONSTANT Mut
VARIABLES pc, owner, revision, readRevision, sampled, heldAtSample, generation,
          disk, row, staleDone, preserved, proofPc, persisted, sampledPersisted, result
vars == <<pc, owner, revision, readRevision, sampled, heldAtSample, generation,
          disk, row, staleDone, preserved, proofPc, persisted, sampledPersisted, result>>
Init == /\ pc = [w \in {1,2} |-> "start"] /\ owner = 0 /\ revision = 0
        /\ readRevision = [w \in {1,2} |-> 0]
        /\ sampled = {} /\ heldAtSample = {} /\ generation = [w \in {1,2} |-> 1]
        /\ disk = 1 /\ row = 1 /\ staleDone = FALSE /\ preserved = TRUE
        /\ proofPc = "first" /\ persisted = FALSE /\ sampledPersisted = FALSE /\ result = "pending"
AdvanceDisk == /\ disk = 1 /\ disk' = 2
               /\ UNCHANGED <<pc, owner, revision, readRevision, sampled, heldAtSample, generation,
                               row, staleDone, preserved, proofPc, persisted, sampledPersisted, result>>
Acquire(w) == /\ pc[w] = "start" /\ (owner = 0 \/ Mut = "no_lock")
              /\ pc' = [pc EXCEPT ![w] = "locked"] /\ owner' = w
              /\ UNCHANGED <<revision, readRevision, sampled, heldAtSample, generation, disk, row,
                               staleDone, preserved, proofPc, persisted, sampledPersisted, result>>
Sample(w) == /\ (pc[w] = "locked" \/ (pc[w] = "start" /\ Mut = "early_sample"))
             /\ pc' = [pc EXCEPT ![w] = "sampled"]
             /\ readRevision' = [readRevision EXCEPT ![w] = revision]
             /\ sampled' = sampled \cup {w}
             /\ heldAtSample' = IF owner = w /\ Mut # "no_lock" THEN heldAtSample \cup {w} ELSE heldAtSample
             /\ generation' = [generation EXCEPT ![w] = IF Mut = "disk_read" THEN disk ELSE 1]
             /\ UNCHANGED <<owner, revision, disk, row, staleDone, preserved, proofPc, persisted, sampledPersisted, result>>
Publish(w) == /\ pc[w] = "sampled" /\ pc' = [pc EXCEPT ![w] = "done"]
              /\ revision' = readRevision[w] + 1 /\ owner' = 0
              /\ UNCHANGED <<readRevision, sampled, heldAtSample, generation, disk, row,
                               staleDone, preserved, proofPc, persisted, sampledPersisted, result>>
Replace == /\ row = 1 /\ ~staleDone /\ row' = 2
           /\ UNCHANGED <<pc, owner, revision, readRevision, sampled, heldAtSample, generation, disk,
                            staleDone, preserved, proofPc, persisted, sampledPersisted, result>>
StaleConsume == /\ ~staleDone /\ staleDone' = TRUE
                /\ row' = IF row = 1 \/ Mut = "no_cas" THEN 0 ELSE row
                /\ preserved' = ~(row = 2 /\ Mut = "no_cas")
                /\ UNCHANGED <<pc, owner, revision, readRevision, sampled, heldAtSample, generation, disk,
                                 proofPc, persisted, sampledPersisted, result>>
FirstRead == /\ proofPc = "first"
             /\ proofPc' = IF persisted THEN "done" ELSE "cancelled"
             /\ result' = IF persisted THEN "persisted" ELSE "pending"
             /\ UNCHANGED <<pc, owner, revision, readRevision, sampled, heldAtSample, generation, disk,
                              row, staleDone, preserved, persisted, sampledPersisted>>
CancelRead == /\ proofPc = "cancelled" /\ proofPc' = "final"
              /\ UNCHANGED <<pc, owner, revision, readRevision, sampled, heldAtSample, generation, disk,
                               row, staleDone, preserved, persisted, sampledPersisted, result>>
Persist == /\ ~persisted /\ persisted' = TRUE
           /\ UNCHANGED <<pc, owner, revision, readRevision, sampled, heldAtSample, generation, disk,
                            row, staleDone, preserved, proofPc, sampledPersisted, result>>
FinalRead == /\ proofPc = "final" /\ proofPc' = "done" /\ sampledPersisted' = persisted
             /\ result' = IF persisted /\ Mut # "no_final" THEN "persisted" ELSE "cancelled"
             /\ UNCHANGED <<pc, owner, revision, readRevision, sampled, heldAtSample, generation, disk,
                              row, staleDone, preserved, persisted>>
WriterStep == \E w \in {1,2}: Acquire(w) \/ Sample(w) \/ Publish(w)
ProofStep == FirstRead \/ CancelRead \/ FinalRead
Done == /\ \A w \in {1,2}: pc[w] = "done"
        /\ proofPc = "done" /\ staleDone /\ UNCHANGED vars
Next == WriterStep \/ ProofStep \/ AdvanceDisk \/ Replace \/ StaleConsume \/ Persist \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(WriterStep) /\ WF_vars(ProofStep) /\ WF_vars(StaleConsume)
MetadataInLock == sampled \subseteq heldAtSample
BootMemo == \A w \in sampled: generation[w] = 1
NoLostUpdate == (\A w \in {1,2}: pc[w] = "done") => revision = 2
SuccessorPreserved == preserved
PersistedWins == (proofPc = "done" /\ sampledPersisted) => result = "persisted"
WritersFinish == <> (\A w \in {1,2}: pc[w] = "done")
=============================================================================
