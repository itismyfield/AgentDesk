----------------------- MODULE InstanceAdmission -----------------------
EXTENDS Naturals, FiniteSets
CONSTANTS Mut, Bound, Modes
Runs == {"A", "B"}
None == "none"
VARIABLES phase, parentFd, childFd, helperFd, owner, seenFree, childAlive,
          mode, identity, witness, cloexec, gateway, elapsed, spawns,
          foreignCleanup, lateSpawn, duplicate
vars == <<phase, parentFd, childFd, helperFd, owner, seenFree, childAlive,
          mode, identity, witness, cloexec, gateway, elapsed, spawns,
          foreignCleanup, lateSpawn, duplicate>>
Holder(r) == parentFd[r] \/ childFd[r] \/ helperFd[r]
FreeAfter(r) == ~parentFd[r] /\ ~helperFd[r]
Init ==
    /\ phase = [r \in Runs |-> "Pending"]
    /\ parentFd = [r \in Runs |-> FALSE]
    /\ childFd = [r \in Runs |-> FALSE]
    /\ helperFd = [r \in Runs |-> FALSE]
    /\ owner = None /\ seenFree = [r \in Runs |-> FALSE]
    /\ childAlive = [r \in Runs |-> FALSE]
    /\ mode = [r \in Runs |-> "missing"]
    /\ identity = [r \in Runs |-> FALSE] /\ witness = [r \in Runs |-> FALSE]
    /\ cloexec = [r \in Runs |-> FALSE] /\ gateway = [r \in Runs |-> FALSE]
    /\ elapsed = [r \in Runs |-> 0] /\ spawns = [r \in Runs |-> 0]
    /\ foreignCleanup = FALSE /\ lateSpawn = FALSE /\ duplicate = None
ObserveFree(r) ==
    /\ Mut = "non_atomic" /\ phase[r] = "Pending" /\ owner = None /\ ~seenFree[r]
    /\ seenFree' = [seenFree EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, childAlive,
                    mode, identity, witness, cloexec, gateway, elapsed, spawns,
                    foreignCleanup, lateSpawn, duplicate>>
Acquire(r) ==
    /\ phase[r] = "Pending"
    /\ (owner = None \/ (Mut = "non_atomic" /\ seenFree[r]))
    /\ parentFd' = [parentFd EXCEPT ![r] = TRUE] /\ owner' = r
    /\ phase' = [phase EXCEPT ![r] = "Held"]
    /\ UNCHANGED <<childFd, helperFd, seenFree, childAlive, mode, identity,
                    witness, cloexec, gateway, elapsed, spawns, foreignCleanup,
                    lateSpawn, duplicate>>
Spawn(r, m) ==
    /\ phase[r] = "Held" /\ parentFd[r] /\ owner = r
    /\ (elapsed[r] < Bound \/ Mut = "late_spawn")
    /\ phase' = [phase EXCEPT ![r] = "Inherited"]
    /\ childAlive' = [childAlive EXCEPT ![r] = TRUE]
    /\ childFd' = [childFd EXCEPT ![r] = m \in {"valid", "bad_witness"}]
    /\ mode' = [mode EXCEPT ![r] = m]
    /\ identity' = [identity EXCEPT ![r] = m # "foreign"]
    /\ witness' = [witness EXCEPT ![r] = m # "bad_witness"]
    /\ spawns' = [spawns EXCEPT ![r] = @ + 1]
    /\ lateSpawn' = (lateSpawn \/ elapsed[r] >= Bound)
    /\ duplicate' = IF Mut = "duplicate_child" THEN r ELSE duplicate
    /\ UNCHANGED <<parentFd, helperFd, owner, seenFree, cloexec, gateway, elapsed, foreignCleanup>>
Proof(r) == childFd[r] /\ identity[r] /\ witness[r] /\ owner = r
Admit(r) ==
    /\ phase[r] \in {"Inherited", "Stopping"} /\ childAlive[r] /\ ~gateway[r]
    /\ (Proof(r) \/ Mut = "skip_admission"
        \/ (Mut = "skip_same_fd" /\ identity[r] /\ witness[r] /\ owner = r))
    /\ phase' = [phase EXCEPT ![r] = IF @ = "Stopping" THEN @ ELSE "Admitted"]
    /\ cloexec' = [cloexec EXCEPT ![r] = Mut # "skip_cloexec"]
    /\ gateway' = [gateway EXCEPT ![r] = TRUE]
    /\ helperFd' = [helperFd EXCEPT ![r] = IF Mut = "leak_fd" THEN TRUE ELSE @]
    /\ UNCHANGED <<parentFd, childFd, owner, seenFree, childAlive,
                    mode, identity, witness, elapsed, spawns, foreignCleanup,
                    lateSpawn, duplicate>>
Reject(r) ==
    /\ phase[r] = "Inherited" /\ childAlive[r] /\ ~Proof(r)
    /\ phase' = [phase EXCEPT ![r] = "Released"]
    /\ childAlive' = [childAlive EXCEPT ![r] = FALSE]
    /\ childFd' = [childFd EXCEPT ![r] = FALSE]
    /\ owner' = IF owner = r /\ FreeAfter(r) THEN None ELSE owner
    /\ duplicate' = IF duplicate = r THEN None ELSE duplicate
    /\ UNCHANGED <<parentFd, helperFd, seenFree, mode, identity, witness,
                    cloexec, gateway, elapsed, spawns, foreignCleanup, lateSpawn>>
Tick(r) ==
    /\ phase[r] \in {"Held", "Inherited", "Admitted", "Stopping"} /\ elapsed[r] < Bound
    /\ elapsed' = [elapsed EXCEPT ![r] = @ + 1]
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, childAlive,
                    mode, identity, witness, cloexec, gateway, spawns,
                    foreignCleanup, lateSpawn, duplicate>>
Timeout(r) ==
    /\ elapsed[r] = Bound /\ phase[r] \in {"Held", "Inherited", "Admitted"}
    /\ phase' = [phase EXCEPT ![r] = IF childAlive[r] THEN "Stopping" ELSE "Released"]
    /\ UNCHANGED <<parentFd, childFd, helperFd, owner, seenFree, childAlive,
                    mode, identity, witness, cloexec, gateway, elapsed, spawns,
                    foreignCleanup, lateSpawn, duplicate>>
ParentExit(r) ==
    /\ parentFd[r] /\ parentFd' = [parentFd EXCEPT ![r] = FALSE]
    /\ phase' = [phase EXCEPT ![r] = IF phase[r] = "Held" THEN "Released" ELSE @]
    /\ owner' = IF owner = r /\ ((~childFd[r] /\ ~helperFd[r]) \/ Mut = "parent_unlock")
                THEN None ELSE owner
    /\ UNCHANGED <<childFd, helperFd, seenFree, childAlive, mode, identity,
                    witness, cloexec, gateway, elapsed, spawns, foreignCleanup,
                    lateSpawn, duplicate>>
ChildExit(r) ==
    /\ childAlive[r] /\ childAlive' = [childAlive EXCEPT ![r] = FALSE]
    /\ phase' = [phase EXCEPT ![r] = "Released"]
    /\ childFd' = [childFd EXCEPT ![r] = FALSE]
    /\ gateway' = [gateway EXCEPT ![r] = FALSE]
    /\ owner' = IF owner = r /\ FreeAfter(r) THEN None ELSE owner
    /\ duplicate' = IF duplicate = r THEN None ELSE duplicate
    /\ UNCHANGED <<parentFd, helperFd, seenFree, mode, identity, witness,
                    cloexec, elapsed, spawns, foreignCleanup, lateSpawn>>
ForeignCleanup ==
    /\ Mut = "foreign_cleanup" /\ owner # None /\ ~foreignCleanup
    /\ foreignCleanup' = TRUE
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, childAlive,
                    mode, identity, witness, cloexec, gateway, elapsed, spawns,
                    lateSpawn, duplicate>>
Wait == UNCHANGED vars
Next == (\E r \in Runs: ObserveFree(r) \/ Acquire(r)
         \/ (\E m \in Modes: Spawn(r, m)) \/ Admit(r) \/ Reject(r)
         \/ Tick(r) \/ Timeout(r) \/ ParentExit(r) \/ ChildExit(r))
        \/ ForeignCleanup \/ Wait
Spec == Init /\ [][Next]_vars
        /\ (\A r \in Runs: WF_vars(Acquire(r)) /\ WF_vars(Tick(r))
            /\ WF_vars(Timeout(r)) /\ WF_vars(Reject(r)))
NoDualGateway == Cardinality({r \in Runs: gateway[r]})
                 + (IF duplicate # None /\ gateway[duplicate] THEN 1 ELSE 0) <= 1
NoHolderTheft == \A r \in Runs: Holder(r) => owner = r
NoForeignCleanup == ~foreignCleanup
AdmissionBeforeEffects == \A r \in Runs: gateway[r] => Proof(r) /\ cloexec[r]
NoSpawnAfterDeadline == ~lateSpawn
SingleChildNoRetry == \A r \in Runs: spawns[r] <= 1
OrphanReacquire == \A r \in Runs:
    (<>[](\A q \in Runs: ~parentFd[q] /\ ~childFd[q])) => <>(phase[r] # "Pending")
=============================================================================
