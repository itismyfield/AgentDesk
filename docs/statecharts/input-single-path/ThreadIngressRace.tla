------------------------ MODULE ThreadIngressRace ------------------------
\* Accepted distinct source tickets from API start, API deliver and Discord converge.
\* The per-child transition rechecks absence before choosing cold versus warm.
EXTENDS Naturals
CONSTANTS N, Mutation
Ids == 1..N
\* Baseline: ad04f6f3ec; HerdrSpec adds external API/Discord same-channel owner joins.
VARIABLES phase, snapshot, owner, session, launches, delivered, reply, failed
VARIABLES activity, epoch, readEpoch, reservation, begun, staleEffect, restarts, origins, nativeChanged, idleWhileBusy
hfields == <<activity, epoch, readEpoch, reservation, begun, staleEffect, restarts, origins, nativeChanged, idleWhileBusy>>
HFields ==
    /\ activity = IF Mutation = "herdrBusy" THEN "busy" ELSE "idle"
    /\ epoch = 1 /\ readEpoch = [i \in Ids |-> 0]
    /\ reservation = FALSE /\ begun = [i \in Ids |-> FALSE]
    /\ staleEffect = FALSE /\ restarts = 0 /\ nativeChanged = FALSE /\ idleWhileBusy = FALSE
    /\ origins = [i \in Ids |-> IF i = 1 THEN "imessageStart" ELSE IF i = 2 THEN "humanApiDeliver" ELSE "discordPerson"]
vars == <<phase, snapshot, owner, session, launches, delivered, reply, failed>>
Init ==
    /\ HFields
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
BaseNext == (\E i \in Ids : Peek(i) \/ TakeTransition(i) \/ Start(i) \/ StartupReady(i)
                        \/ Warm(i) \/ Failure(i) \/ Post(i)) \/ Done
Next == BaseNext /\ UNCHANGED hfields
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

HInit ==
    /\ HFields /\ phase = [i \in Ids |-> "queued"]
    /\ snapshot = [i \in Ids |-> "none"] /\ owner = 0
    /\ session = "live" /\ launches = 0
    /\ delivered = [i \in Ids |-> 0] /\ reply = [i \in Ids |-> 0] /\ failed = {}
HClaim(i) ==
    /\ phase[i] = "queued" /\ owner = 0 /\ owner' = i
    /\ phase' = [phase EXCEPT ![i] = "observe"]
    /\ UNCHANGED <<snapshot, session, launches, delivered, reply, failed, hfields>>
HObserve(i) ==
    /\ owner = i /\ phase[i] = "observe"
    /\ phase' = [phase EXCEPT ![i] = IF activity = "idle" THEN "idleProbe" ELSE "busyProbe"]
    /\ readEpoch' = [readEpoch EXCEPT ![i] = epoch]
    /\ reservation' = (activity = "idle")
    /\ UNCHANGED <<snapshot, owner, session, launches, delivered, reply, failed,
                   activity, epoch, begun, staleEffect, restarts, origins, nativeChanged, idleWhileBusy>>
HReturn(i) ==
    /\ owner = i /\ phase[i] = "idleProbe"
    /\ phase' = [phase EXCEPT ![i] = "delegated"] /\ reservation' = FALSE
    /\ UNCHANGED <<snapshot, owner, session, launches, delivered, reply, failed,
                   activity, epoch, readEpoch, begun, staleEffect, restarts, origins, nativeChanged, idleWhileBusy>>
HNativeStart ==
    /\ owner \in Ids /\ phase[owner] \in {"delegated", "driverReserved"}
    /\ activity = "idle" /\ ~nativeChanged
    /\ activity' = "busy" /\ nativeChanged' = TRUE /\ reservation' = FALSE
    /\ UNCHANGED <<vars, epoch, readEpoch, begun, staleEffect, restarts, origins, idleWhileBusy>>
HDriverReserve(i) ==
    /\ owner = i /\ phase[i] = "delegated" /\ activity = "idle" /\ ~reservation
    /\ phase' = [phase EXCEPT ![i] = "driverReserved"]
    /\ reservation' = TRUE /\ readEpoch' = [readEpoch EXCEPT ![i] = epoch]
    /\ UNCHANGED <<snapshot, owner, session, launches, delivered, reply, failed,
                   activity, epoch, begun, staleEffect, restarts, origins, nativeChanged, idleWhileBusy>>
HReobserve(i) ==
    /\ owner = i /\ phase[i] \in {"delegated", "driverReserved"}
    /\ (activity # "idle" \/ readEpoch[i] # epoch \/ (phase[i] = "driverReserved" /\ ~reservation))
    /\ phase' = [phase EXCEPT ![i] = "observe"] /\ reservation' = FALSE
    /\ UNCHANGED <<snapshot, owner, session, launches, delivered, reply, failed,
                   activity, epoch, readEpoch, begun, staleEffect, restarts, origins, nativeChanged, idleWhileBusy>>
HBegin(i) ==
    /\ owner = i
    /\ ((phase[i] = "driverReserved" /\ activity = "idle" /\ reservation)
        \/ phase[i] = "busyProbe"
        \/ (Mutation = "herdrNoIdleRecheck" /\ phase[i] \in {"delegated", "driverReserved"})
        \/ (Mutation = "herdrStaleEpoch" /\ phase[i] = "delegated" /\ activity = "idle" /\ readEpoch[i] # epoch))
    /\ (readEpoch[i] = epoch \/ Mutation = "herdrStaleEpoch")
    /\ phase' = [phase EXCEPT ![i] = "begun"] /\ begun' = [begun EXCEPT ![i] = TRUE]
    /\ delivered' = [delivered EXCEPT ![i] = @ + 1] /\ reservation' = FALSE
    /\ staleEffect' = (staleEffect \/ (readEpoch[i] # epoch)) /\ activity' = "busy"
    /\ idleWhileBusy' = (idleWhileBusy \/ (phase[i] # "busyProbe" /\ activity = "busy"))
    /\ launches' = launches + IF phase[i] # "busyProbe" THEN 1 ELSE 0
    /\ UNCHANGED <<snapshot, owner, session, reply, failed,
                   epoch, readEpoch, restarts, origins, nativeChanged>>
HReceipt(i) ==
    /\ owner = i /\ phase[i] = "begun"
    /\ phase' = [phase EXCEPT ![i] = "submitted"] /\ owner' = 0
    /\ UNCHANGED <<snapshot, session, launches, delivered, reply, failed, hfields>>
HRestart ==
    /\ restarts = 0 /\ owner \in Ids
    /\ phase[owner] \in {"observe", "idleProbe", "delegated", "driverReserved", "busyProbe", "begun"}
    /\ restarts' = 1 /\ epoch' = epoch + 1 /\ reservation' = FALSE
    /\ phase' = [phase EXCEPT ![owner] =
          IF @ = "begun" THEN "uncertain"
          ELSE IF Mutation = "herdrStaleEpoch" /\ @ = "delegated" THEN "delegated" ELSE "observe"]
    /\ owner' = IF phase[owner] = "begun" THEN 0 ELSE owner
    /\ UNCHANGED <<snapshot, session, launches, delivered, reply, failed,
                   activity, readEpoch, begun, staleEffect, origins, nativeChanged, idleWhileBusy>>
HShowFailure(i) ==
    /\ phase[i] = "uncertain" /\ phase' = [phase EXCEPT ![i] = "failed"]
    /\ failed' = failed \cup {i}
    /\ UNCHANGED <<snapshot, owner, session, launches, delivered, reply, hfields>>
HPost(i) == Post(i) /\ UNCHANGED hfields
HNext == (\E i \in Ids : HClaim(i) \/ HObserve(i) \/ HReturn(i) \/ HDriverReserve(i) \/ HReobserve(i) \/ HBegin(i)
                         \/ HReceipt(i) \/ HShowFailure(i) \/ HPost(i)) \/ HRestart \/ HNativeStart
         \/ (Done /\ UNCHANGED hfields)
hvars == <<vars, hfields>>
HerdrSpec == HInit /\ [][HNext]_hvars
    /\ \A i \in Ids : WF_hvars(HClaim(i)) /\ WF_hvars(HObserve(i))
        /\ WF_hvars(HReturn(i)) /\ WF_hvars(HDriverReserve(i))
        /\ WF_hvars(HReobserve(i)) /\ WF_hvars(HBegin(i))
        /\ WF_hvars(HReceipt(i)) /\ WF_hvars(HShowFailure(i)) /\ WF_hvars(HPost(i))
HNoIdleStartWhileBusy == ~idleWhileBusy
HerdrCurrentExecutor == ~staleEffect
HerdrReservationReturned ==
    (\E i \in Ids : phase[i] \in {"delegated", "begun"}) => ~reservation
HerdrSourcePreserved ==
    origins = [i \in Ids |-> IF i = 1 THEN "imessageStart" ELSE IF i = 2 THEN "humanApiDeliver" ELSE "discordPerson"]
EachDeliveredGetsOutcome == \A i \in Ids : delivered[i] = 1 ~> (reply[i] = 1 \/ i \in failed)
EachAcceptedStarts == \A i \in Ids : TRUE ~> begun[i]
=============================================================================
