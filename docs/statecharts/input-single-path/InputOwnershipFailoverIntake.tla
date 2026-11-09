---------------- MODULE InputOwnershipFailoverIntake ----------------
\* One Discord message across a gateway handback between a standby and the preferred node.
\* Each node keeps its own V2 queue file; the only shared evidence is the completed turn receipt.
EXTENDS Naturals
CONSTANTS Fix, StandbyRuns, Order, MaxSwitches
VARIABLES holder, arrived, caught, queue, runs, receipt, retired, switches
vars == <<holder, arrived, caught, queue, runs, receipt, retired, switches>>
Nodes == {"standby", "preferred"}

Init ==
    /\ holder = "standby"
    /\ arrived = FALSE
    /\ caught = [n \in Nodes |-> FALSE]
    /\ queue = [n \in Nodes |-> FALSE]
    /\ runs = [n \in Nodes |-> 0]
    /\ receipt = FALSE
    /\ retired = 0
    /\ switches = 0

\* The standby is not the O home, so it may run the channel only when StandbyRuns.
CanRun(n) == n = "preferred" \/ StandbyRuns

\* The message posts while the preferred node is down.
Arrive ==
    /\ ~arrived /\ arrived' = TRUE
    /\ UNCHANGED <<holder, caught, queue, runs, receipt, retired, switches>>

\* Catch-up on the holder enqueues unless its own queue file or its own ledger knows the message;
\* its checkpoint then passes the message, so each node recovers it at most once.
CatchUp(n) ==
    /\ holder = n /\ arrived /\ ~caught[n]
    /\ (Order = "any" \/ n = "preferred" \/ switches = 0)
    /\ ~queue[n] /\ runs[n] = 0
    /\ caught' = [caught EXCEPT ![n] = TRUE]
    /\ queue' = [queue EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<holder, arrived, runs, receipt, retired, switches>>

\* Fence or acquire: the gateway lease moves; the node that loses it keeps its queue file.
Handback ==
    /\ switches < MaxSwitches
    /\ holder' = IF holder = "standby" THEN "preferred" ELSE "standby"
    /\ switches' = switches + 1
    /\ UNCHANGED <<arrived, caught, queue, runs, receipt, retired>>

\* queued.rs start_owner_front: a completed receipt retires the front without a turn.
RetireSettledElsewhere(n) ==
    /\ Fix /\ holder = n /\ queue[n] /\ receipt
    /\ queue' = [queue EXCEPT ![n] = FALSE]
    /\ retired' = retired + 1
    /\ UNCHANGED <<holder, arrived, caught, runs, receipt, switches>>

\* A node that may run the channel delivers its front; the turn's terminal records the receipt.
\* Order "measured" replays 11-09: the standby recovers the input only during its first lease and
\* runs it only after the preferred node answered it.
Deliver(n) ==
    /\ holder = n /\ queue[n] /\ CanRun(n)
    /\ (Order = "any" \/ n = "preferred" \/ runs["preferred"] > 0)
    /\ ~(Fix /\ receipt)
    /\ queue' = [queue EXCEPT ![n] = FALSE]
    /\ runs' = [runs EXCEPT ![n] = @ + 1]
    /\ receipt' = TRUE
    /\ UNCHANGED <<holder, arrived, caught, retired, switches>>

Next ==
    \/ Arrive \/ Handback
    \/ \E n \in Nodes: CatchUp(n) \/ RetireSettledElsewhere(n) \/ Deliver(n)
    \/ UNCHANGED vars

NoCrossNodeRedelivery == runs["standby"] + runs["preferred"] <= 1
RetiredOnlyAfterCompletion == retired > 0 => receipt
=====================================================================
