------------------------ MODULE ThreadIngressRace ------------------------
\* Accepted distinct source tickets from API start, API deliver and Discord converge.
\* The per-child transition rechecks absence before choosing cold versus warm.
EXTENDS Naturals
CONSTANTS N, Mutation
Ids == 1..N
VARIABLES phase, snapshot, owner, session, launches, delivered, reply, failed
vars == <<phase, snapshot, owner, session, launches, delivered, reply, failed>>
Init ==
    /\ phase = [i \in Ids |-> "queued"] /\ snapshot = [i \in Ids |-> "none"]
    /\ owner = 0 /\ session = "absent" /\ launches = 0
    /\ delivered = [i \in Ids |-> 0] /\ reply = [i \in Ids |-> 0] /\ failed = {}
Peek(i) ==
    /\ phase[i] = "queued" /\ session # "booting"
    /\ snapshot' = [snapshot EXCEPT ![i] = session]
    /\ phase' = [phase EXCEPT ![i] = "peeked"]
    /\ UNCHANGED <<owner, session, launches, delivered, reply, failed>>
TakeTransition(i) ==
    /\ phase[i] = "peeked" /\ owner = 0
    /\ owner' = i /\ phase' = [phase EXCEPT ![i] = "preparing"]
    /\ snapshot' = [snapshot EXCEPT ![i] = IF Mutation = "staleRead" THEN @ ELSE session]
    /\ UNCHANGED <<session, launches, delivered, reply, failed>>
Start(i) ==
    /\ owner = i /\ phase[i] = "preparing" /\ snapshot[i] = "absent"
    /\ launches' = launches + 1 /\ session' = "booting"
    /\ phase' = [phase EXCEPT ![i] = "starting"]
    /\ UNCHANGED <<snapshot, owner, delivered, reply, failed>>
StartupReady(i) ==
    /\ owner = i /\ phase[i] = "starting" /\ session = "booting"
    /\ session' = "live" /\ owner' = 0
    /\ delivered' = [delivered EXCEPT ![i] = @ + 1]
    /\ phase' = [phase EXCEPT ![i] = "submitted"]
    /\ UNCHANGED <<snapshot, launches, reply, failed>>
Warm(i) ==
    /\ owner = i /\ phase[i] = "preparing" /\ snapshot[i] = "live"
    /\ delivered' = [delivered EXCEPT ![i] = @ + 1]
    /\ owner' = 0 /\ phase' = [phase EXCEPT ![i] = "submitted"]
    /\ UNCHANGED <<snapshot, session, launches, reply, failed>>
Failure(i) ==
    /\ owner = i /\ phase[i] \in {"preparing", "starting"}
    /\ failed' = failed \cup {i} /\ phase' = [phase EXCEPT ![i] = "failed"] /\ owner' = 0
    /\ session' = IF session = "booting" THEN "absent" ELSE session
    /\ UNCHANGED <<snapshot, launches, delivered, reply>>
Post(i) ==
    /\ phase[i] = "submitted"
    /\ reply' = [reply EXCEPT ![i] = @ + 1] /\ phase' = [phase EXCEPT ![i] = "done"]
    /\ UNCHANGED <<snapshot, owner, session, launches, delivered, failed>>
Done == /\ \A i \in Ids : phase[i] \in {"done", "failed"}
        /\ UNCHANGED vars
Next == (\E i \in Ids : Peek(i) \/ TakeTransition(i) \/ Start(i) \/ StartupReady(i)
                        \/ Warm(i) \/ Failure(i) \/ Post(i)) \/ Done
Spec == Init /\ [][Next]_vars
        /\ \A i \in Ids : WF_vars(Peek(i)) /\ WF_vars(TakeTransition(i))
            /\ WF_vars(Start(i) \/ Warm(i) \/ Failure(i))
            /\ WF_vars(StartupReady(i) \/ Failure(i)) /\ WF_vars(Post(i))
NoDuplicate == \A i \in Ids : delivered[i] <= 1 /\ reply[i] <= 1
NoConcurrentStartup == session = "booting" => owner \in Ids /\ phase[owner] = "starting"
\* Only successful live startup forbids a later stale cold start; failed startup may retry.
NoStaleBootstrap == (\E i \in Ids : delivered[i] = 1) =>
    session = "live" /\ ~(\E i \in Ids : phase[i] = "starting")
SuccessBacked == \A i \in Ids : phase[i] = "done" => delivered[i] = 1 /\ reply[i] = 1
EachAcceptedCompletes == \A i \in Ids : TRUE ~> (delivered[i] = 1 \/ i \in failed)
EachDeliveredGetsReply == \A i \in Ids : delivered[i] = 1 ~> reply[i] = 1
=============================================================================
