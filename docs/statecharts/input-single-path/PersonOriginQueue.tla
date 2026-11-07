--------------------------- MODULE PersonOriginQueue ---------------------------
EXTENDS Naturals, Sequences, TLC
CONSTANT Mut
VARIABLES queue, binds, delivered, ownTurns
vars == <<queue, binds, delivered, ownTurns>>
Init == /\ queue = <<"machine", "first", "second">>
        /\ binds = queue /\ delivered = <<>> /\ ownTurns = 0
\* Selection and slot binds publish together; the first person remains before the second.
Promote == /\ Len(queue) > 1 /\ Head(queue) = "machine"
           /\ queue' = <<queue[2], "machine">> \o SubSeq(queue, 3, Len(queue))
           /\ binds' = IF Mut = "splitBinds" THEN binds ELSE queue'
           /\ UNCHANGED <<delivered, ownTurns>>
Join == /\ Len(queue) > 0 /\ Head(queue) # "machine"
        /\ delivered' = Append(delivered, Head(queue))
        /\ queue' = Tail(queue) /\ binds' = Tail(binds) /\ UNCHANGED ownTurns
Idle == /\ queue = <<"machine">> /\ delivered = <<"first", "second">>
        /\ queue' = <<>> /\ binds' = <<>> /\ ownTurns' = ownTurns + 1 /\ UNCHANGED delivered
Next == Promote \/ Join \/ Idle
AtomicBinds == queue = binds
PersonOrder == delivered \in {<<>>, <<"first">>, <<"first", "second">>}
MachineOwnTurn == ownTurns > 0 => delivered = <<"first", "second">>
MachineKept == ownTurns = 0 => \E i \in 1..Len(queue) : queue[i] = "machine"
=============================================================================
