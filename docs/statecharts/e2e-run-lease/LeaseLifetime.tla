------------------------- MODULE LeaseLifetime -------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS Kind, Mut, CrashEnabled
Runs == {"A", "B"}
None == "none"
VARIABLES phase, parentFd, childFd, helperFd, owner, seenFree, aged,
          pending, net, netOwner, ack, twin, badLateSend, foreignCleanup
vars == <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
          pending, net, netOwner, ack, twin, badLateSend, foreignCleanup>>
Holder(r) == parentFd[r] \/ childFd[r] \/ helperFd[r]
OtherHolders(r) == \E q \in Runs \ {r}: Holder(q)
Active == {r \in Runs: phase[r] \in {"Running", "Prepared", "Draining"}}
Clean == pending = None /\ net \in {"none", "settled"}
Init ==
    /\ phase = [r \in Runs |-> "Pending"]
    /\ parentFd = [r \in Runs |-> FALSE]
    /\ childFd = [r \in Runs |-> FALSE]
    /\ helperFd = [r \in Runs |-> FALSE]
    /\ owner = None /\ seenFree = [r \in Runs |-> FALSE] /\ aged = FALSE
    /\ pending = None /\ net = "none" /\ netOwner = None /\ ack = FALSE
    /\ twin = None /\ badLateSend = FALSE /\ foreignCleanup = FALSE
ObserveFree(r) ==
    /\ Mut = "non_atomic" /\ phase[r] = "Pending" /\ owner = None /\ ~seenFree[r]
    /\ seenFree' = [seenFree EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, aged,
                   pending, net, netOwner, ack, twin, badLateSend, foreignCleanup>>
Acquire(r) ==
    /\ phase[r] = "Pending"
    /\ (owner = None \/ (Mut = "ttl_takeover" /\ aged)
                       \/ (Mut = "non_atomic" /\ seenFree[r]))
    /\ owner' = r /\ phase' = [phase EXCEPT ![r] = "Held"]
    /\ parentFd' = [parentFd EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<childFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, twin, badLateSend, foreignCleanup>>
ExpireDiagnostic(r) ==
    /\ Mut = "ttl_takeover" /\ Holder(r) /\ ~aged /\ aged' = TRUE
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree,
                   pending, net, netOwner, ack, twin, badLateSend, foreignCleanup>>
Start(r) ==
    /\ phase[r] = "Held" /\ parentFd[r] /\ owner = r
    /\ childFd' = [childFd EXCEPT ![r] = (Mut # "missing_fd")]
    /\ helperFd' = [helperFd EXCEPT ![r] = (Mut = "leak_fd")]
    /\ phase' = [phase EXCEPT ![r] = IF Clean THEN "Running" ELSE "PausedUncertain"]
    /\ twin' = IF Mut = "duplicate_child" /\ Clean THEN r ELSE twin
    /\ UNCHANGED <<parentFd, owner, seenFree, aged, pending, net,
                   netOwner, ack, badLateSend, foreignCleanup>>
Resume(r) ==
    /\ phase[r] = "PausedUncertain" /\ Clean /\ owner = r /\ childFd[r]
    /\ phase' = [phase EXCEPT ![r] = "Running"]
    /\ UNCHANGED <<parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, net, netOwner, ack, twin, badLateSend, foreignCleanup>>
Prepare(r) ==
    /\ Kind = "run" /\ phase[r] = "Running" /\ Clean
    /\ pending' = IF Mut = "no_pending" THEN None ELSE r
    /\ phase' = [phase EXCEPT ![r] = "Prepared"]
    /\ UNCHANGED <<parentFd, childFd, helperFd, owner, seenFree, aged,
                   net, netOwner, ack, twin, badLateSend, foreignCleanup>>
Send(r) ==
    /\ Kind = "run"
    /\ (phase[r] = "Prepared" \/ (Mut = "send_after_release" /\ phase[r] = "Released" /\ pending = r))
    /\ net \in {"none", "settled"}
    /\ net' = "sent" /\ netOwner' = r /\ ack' = FALSE
    /\ badLateSend' = (badLateSend \/ phase[r] = "Released")
    /\ phase' = [phase EXCEPT ![r] = IF phase[r] = "Released" THEN "Released" ELSE "Draining"]
    /\ UNCHANGED <<parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, twin, foreignCleanup>>
ServerAccept ==
    /\ net = "sent" /\ net' = "accepted"
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, netOwner, ack, twin, badLateSend, foreignCleanup>>
AckWitness ==
    /\ net \in {"sent", "accepted", "settled"} /\ ~ack /\ ack' = TRUE
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, net, netOwner, twin, badLateSend, foreignCleanup>>
ProviderSettles ==
    /\ net = "accepted" /\ net' = "settled"
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, netOwner, ack, twin, badLateSend, foreignCleanup>>
Reconcile ==
    /\ pending # None /\ net = "settled" /\ ack /\ pending = netOwner
    /\ pending' = None
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   net, netOwner, ack, twin, badLateSend, foreignCleanup>>
ClearFromIdleSnapshot ==
    /\ Mut = "clear_on_idle" /\ pending # None /\ net = "sent"
    /\ pending' = None
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   net, netOwner, ack, twin, badLateSend, foreignCleanup>>
Finish(r) ==
    /\ ((Kind = "instance" /\ phase[r] = "Running")
        \/ (Kind = "run" /\ phase[r] = "Draining" /\ Clean))
    /\ phase' = [phase EXCEPT ![r] = "Released"]
    /\ childFd' = [childFd EXCEPT ![r] = FALSE]
    /\ owner' = IF owner = r /\ ~parentFd[r] /\ ~helperFd[r] THEN None ELSE owner
    /\ twin' = IF twin = r THEN None ELSE twin
    /\ UNCHANGED <<parentFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, badLateSend, foreignCleanup>>
ParentCrash(r) ==
    /\ CrashEnabled /\ parentFd[r] /\ parentFd' = [parentFd EXCEPT ![r] = FALSE]
    /\ phase' = [phase EXCEPT ![r] = IF phase[r] = "Held" THEN "Released" ELSE @]
    /\ owner' = IF owner = r /\ ((~childFd[r] /\ ~helperFd[r]) \/ Mut = "parent_unlock")
                 THEN None ELSE owner
    /\ UNCHANGED <<childFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, twin, badLateSend, foreignCleanup>>
ChildCrash(r) ==
    /\ CrashEnabled /\ phase[r] \in {"Running", "Prepared", "Draining", "PausedUncertain"}
    /\ phase' = [phase EXCEPT ![r] = "Released"]
    /\ childFd' = [childFd EXCEPT ![r] = FALSE]
    /\ owner' = IF owner = r /\ ~parentFd[r] /\ ~helperFd[r] THEN None ELSE owner
    /\ twin' = IF twin = r THEN None ELSE twin
    /\ UNCHANGED <<parentFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, badLateSend, foreignCleanup>>
ReleaseParent(r) ==
    /\ phase[r] = "Released" /\ parentFd[r]
    /\ parentFd' = [parentFd EXCEPT ![r] = FALSE]
    /\ owner' = IF owner = r /\ ~childFd[r] /\ ~helperFd[r] THEN None ELSE owner
    /\ UNCHANGED <<phase, childFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, twin, badLateSend, foreignCleanup>>
ForeignCleanup ==
    /\ Mut = "foreign_cleanup" /\ owner # None /\ ~foreignCleanup
    /\ foreignCleanup' = TRUE
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, net, netOwner, ack, twin, badLateSend>>
Wait ==
    /\ ((\A r \in Runs: phase[r] = "Released")
       \/ (\E r \in Runs: phase[r] = "PausedUncertain")
       \/ (owner # None /\ \E r \in Runs: phase[r] = "Pending"))
    /\ UNCHANGED vars
Next == (\E r \in Runs: ObserveFree(r) \/ Acquire(r) \/ ExpireDiagnostic(r)
         \/ Start(r) \/ Resume(r) \/ Prepare(r) \/ Send(r) \/ Finish(r)
         \/ ParentCrash(r) \/ ChildCrash(r) \/ ReleaseParent(r))
        \/ ServerAccept \/ AckWitness \/ ProviderSettles \/ Reconcile
        \/ ClearFromIdleSnapshot \/ ForeignCleanup \/ Wait
SafetySpec == Init /\ [][Next]_vars
Spec == SafetySpec /\ (\A r \in Runs: WF_vars(Acquire(r)))
FairSpec == Spec /\ (\A r \in Runs:
    WF_vars(Start(r)) /\ WF_vars(Resume(r)) /\ WF_vars(Prepare(r)) /\ WF_vars(Send(r))
    /\ WF_vars(Finish(r)) /\ WF_vars(ReleaseParent(r)))
    /\ WF_vars(ServerAccept) /\ WF_vars(AckWitness) /\ WF_vars(ProviderSettles) /\ WF_vars(Reconcile)
NoDualWriter == Cardinality(Active) + (IF twin = None THEN 0 ELSE 1) <= 1
NoDualGateway == NoDualWriter
NoHolderTheft == \A r \in Runs: Holder(r) => owner = r
WriterHasInheritedFd == \A r \in Runs: phase[r] \in {"Running", "Prepared", "Draining"} => childFd[r]
UncertainSendJournaled == net \in {"sent", "accepted"} => pending = netOwner
NoNewWriterDuringUncertainty == \A r \in Active: pending # None => pending = r
NoSendAfterRelease == ~badLateSend
NoForeignCleanup == ~foreignCleanup
NextRequestProgress == \A r \in Runs:
    (<>[](Clean /\ (\A q \in Runs: ~parentFd[q] /\ ~childFd[q])))
    => <>(phase[r] # "Pending")
NextRequestStarts == \A r \in Runs: <>(phase[r] = "Running")
=============================================================================
