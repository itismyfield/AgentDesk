--------------------- MODULE ThreadIngressTerminal ---------------------
EXTENDS Naturals, FiniteSets

CONSTANTS Contract, BodyOwner, Mutation, CanRestart
Ids == 1..2
Original == [i \in Ids |-> IF i = 1 THEN "imessage" ELSE "externalAPI"]
Presenter == Contract = "boundGateway"

VARIABLES phase, effects, published, receipt, retained, bodyReady, origin,
          active, token, inflight, wake, epoch, captured, restarted,
          lateCleanupTried
vars == <<phase, effects, published, receipt, retained, bodyReady, origin,
          active, token, inflight, wake, epoch, captured, restarted,
          lateCleanupTried>>

Init ==
    /\ phase = [i \in Ids |-> "queued"]
    /\ effects = [i \in Ids |-> 0]
    /\ published = [i \in Ids |-> 0]
    /\ receipt = [i \in Ids |-> FALSE]
    /\ retained = [i \in Ids |-> TRUE]
    /\ bodyReady = [i \in Ids |-> FALSE]
    /\ origin = Original
    /\ active = 0 /\ token = 0 /\ inflight = 0
    /\ wake = FALSE /\ epoch = 1
    /\ captured = [i \in Ids |-> 0]
    /\ restarted = FALSE /\ lateCleanupTried = FALSE

Start(i) ==
    /\ phase[i] = "queued"
    /\ active = 0 /\ token = 0 /\ inflight = 0
    /\ (i = 1 \/ (phase[1] = "done" /\ wake))
    /\ phase' = [phase EXCEPT ![i] = "running"]
    /\ effects' = [effects EXCEPT ![i] = @ + 1]
    /\ active' = i /\ token' = i /\ inflight' = i
    /\ captured' = [captured EXCEPT ![i] = epoch]
    /\ origin' = IF Mutation = "reclassify"
                 THEN [origin EXCEPT ![i] = "discordPerson"] ELSE origin
    /\ UNCHANGED <<published, receipt, retained, bodyReady, wake, epoch,
                   restarted, lateCleanupTried>>

NativeTerminal(i) ==
    /\ phase[i] = "running"
    /\ phase' = [phase EXCEPT ![i] = "terminal"]
    /\ bodyReady' = [bodyReady EXCEPT ![i] = TRUE]
    /\ UNCHANGED <<effects, published, receipt, retained, origin, active,
                   token, inflight, wake, epoch, captured, restarted,
                   lateCleanupTried>>

Admit(i) ==
    /\ phase[i] = "terminal" /\ token = i /\ captured[i] = epoch
    /\ phase' = [phase EXCEPT ![i] =
                  IF Presenter \/ Mutation = "headlessAuthority"
                  THEN "admitted" ELSE "rejected"]
    /\ UNCHANGED <<effects, published, receipt, retained, bodyReady, origin,
                   active, token, inflight, wake, epoch, captured,
                   restarted, lateCleanupTried>>

\* O reads the native source independently; admitting a frame is no publish ACK.
Publish(i) ==
    /\ bodyReady[i]
    /\ (BodyOwner = "O" \/ phase[i] = "admitted")
    /\ (published[i] = 0 \/
         (Mutation = "doublePublish" /\ published[i] = 1))
    /\ published' = [published EXCEPT ![i] = @ + 1]
    /\ UNCHANGED <<phase, effects, receipt, retained, bodyReady, origin,
                   active, token, inflight, wake, epoch, captured,
                   restarted, lateCleanupTried>>

\* Terminal receipt may settle before O publishes; the canonical native source remains O custody.
RecordReceipt(i) ==
    /\ phase[i] = "admitted" /\ ~receipt[i]
    /\ (published[i] = 1 \/ (BodyOwner = "O" /\ bodyReady[i]))
    /\ receipt' = [receipt EXCEPT ![i] = TRUE]
    /\ UNCHANGED <<phase, effects, published, retained, bodyReady, origin,
                   active, token, inflight, wake, epoch, captured,
                   restarted, lateCleanupTried>>

ClearInflight(i) ==
    /\ phase[i] = "admitted" /\ receipt[i] /\ inflight = i
    /\ Mutation # "keepInflight"
    /\ inflight' = 0
    /\ UNCHANGED <<phase, effects, published, receipt, retained, bodyReady,
                   origin, active, token, wake, epoch, captured,
                   restarted, lateCleanupTried>>

\* Live completion releases its captured allocation; a prior admitted row has no actor.
End(i) ==
    /\ phase[i] = "admitted" /\ receipt[i]
    /\ (published[i] = 1 \/ (BodyOwner = "O" /\ bodyReady[i]))
    /\ (inflight = 0 \/ Mutation = "keepInflight")
    /\ ((captured[i] = epoch /\ token = i) \/
         (captured[i] # epoch /\ token = 0))
    /\ phase' = [phase EXCEPT ![i] = "done"]
    /\ retained' = [retained EXCEPT ![i] = FALSE]
    /\ token' = 0 /\ active' = 0
    /\ UNCHANGED <<effects, published, receipt, bodyReady, origin,
                   inflight, wake, epoch, captured, restarted,
                   lateCleanupTried>>

WakeNext ==
    /\ phase[1] = "done" /\ ~wake /\ Mutation # "noWake"
    /\ wake' = TRUE
    /\ UNCHANGED <<phase, effects, published, receipt, retained, bodyReady,
                   origin, active, token, inflight, epoch, captured,
                   restarted, lateCleanupTried>>

Restart ==
    /\ CanRestart /\ ~restarted /\ active # 0
    /\ phase[active] = "admitted"
    /\ epoch' = 2 /\ restarted' = TRUE /\ token' = 0
    /\ UNCHANGED <<phase, effects, published, receipt, retained, bodyReady,
                   origin, active, inflight, wake, captured,
                   lateCleanupTried>>

\* A delayed end/probe for the previous input cannot clear its successor's custody.
LateCleanup ==
    /\ phase[1] = "done" /\ active = 2 /\ ~lateCleanupTried
    /\ lateCleanupTried' = TRUE
    /\ token' = IF Mutation = "staleCleanup" THEN 0 ELSE token
    /\ inflight' = IF Mutation = "staleCleanup" THEN 0 ELSE inflight
    /\ UNCHANGED <<phase, effects, published, receipt, retained, bodyReady,
                   origin, active, wake, epoch, captured, restarted>>

DropCustody(i) ==
    /\ Mutation = "dropCustody" /\ phase[i] = "terminal"
    /\ published[i] = 0 /\ retained[i]
    /\ retained' = [retained EXCEPT ![i] = FALSE]
    /\ UNCHANGED <<phase, effects, published, receipt, bodyReady, origin,
                   active, token, inflight, wake, epoch, captured,
                   restarted, lateCleanupTried>>

Next == (\E i \in Ids : Start(i) \/ NativeTerminal(i) \/ Admit(i) \/
                          Publish(i) \/ RecordReceipt(i) \/ ClearInflight(i) \/
                          End(i) \/ DropCustody(i)) \/
        WakeNext \/ Restart \/ LateCleanup \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars
        /\ (\A i \in Ids : WF_vars(Start(i)) /\ WF_vars(NativeTerminal(i))
             /\ WF_vars(Admit(i)) /\ WF_vars(Publish(i))
             /\ WF_vars(RecordReceipt(i)) /\ WF_vars(ClearInflight(i))
             /\ WF_vars(End(i)))
        /\ WF_vars(WakeNext)

TypeOK ==
    /\ phase \in [Ids -> {"queued", "running", "terminal", "admitted",
                          "rejected", "done"}]
    /\ effects \in [Ids -> 0..1] /\ published \in [Ids -> 0..2]
    /\ receipt \in [Ids -> BOOLEAN] /\ retained \in [Ids -> BOOLEAN]
    /\ bodyReady \in [Ids -> BOOLEAN]
    /\ origin \in [Ids -> {"imessage", "externalAPI", "discordPerson"}]
    /\ active \in 0..2 /\ token \in 0..2 /\ inflight \in 0..2
    /\ epoch \in 1..2 /\ captured \in [Ids -> 0..2]
    /\ wake \in BOOLEAN /\ restarted \in BOOLEAN
    /\ lateCleanupTried \in BOOLEAN

ExactlyOneReply == \A i \in Ids : published[i] <= 1
NoSilentLoss == \A i \in Ids : retained[i] \/ published[i] >= 1 \/
                 (receipt[i] /\ BodyOwner = "O" /\ bodyReady[i])
NoResidueAfterEnd == \A i \in Ids : phase[i] = "done" =>
                       (token # i /\ inflight # i /\ receipt[i])
TerminalRequiresPresenter == \A i \in Ids :
                              phase[i] \in {"admitted", "done"} => Presenter
SourcePreserved == origin = Original
LiveCustody == \A i \in Ids : phase[i] \in {"running", "terminal"} =>
                (token = i /\ inflight = i /\ captured[i] = epoch)

EachReply == \A i \in Ids : <>(published[i] = 1)
EachEnds == \A i \in Ids : <>(phase[i] = "done")
NextProgress == <>(effects[2] = 1)
EventuallyDrained == <>(phase[2] = "done" /\ token = 0 /\ inflight = 0)
=============================================================================
