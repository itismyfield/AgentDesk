------------------------- MODULE WriterAdoptionBootWake -------------------------
EXTENDS Naturals, FiniteSets, TLC
CONSTANT Mode
Nodes == {"gateway", "localExecution", "foreignExecution", "staleExecution"}
Channels == {"debt", "ordinary"}
VARIABLES booted, wakes, refresh, queue, owner
vars == <<booted, wakes, refresh, queue, owner>>
Authorized(node) == IF owner = "managed" THEN node = "localExecution" ELSE node = "gateway"
Init == /\ owner \in {"managed", "unmanaged"}
        /\ booted = {}
        /\ wakes = [n \in Nodes |-> [c \in Channels |-> 0]]
        /\ refresh = [n \in Nodes |-> 0]
        /\ queue = FALSE
Wake(node, first) ==
    IF (Mode = "Before" \/ (first /\ Authorized(node)))
    THEN [wakes EXCEPT ![node]["debt"] = @ + 1]
    ELSE wakes
Bootstrap(node) == /\ node \notin booted
                   /\ booted' = booted \cup {node}
                   /\ wakes' = Wake(node, TRUE)
                   /\ UNCHANGED <<refresh, queue, owner>>
Refresh(node) == /\ node \in booted /\ refresh[node] < 2
                 /\ refresh' = [refresh EXCEPT ![node] = @ + 1]
                 /\ wakes' = Wake(node, FALSE)
                 /\ UNCHANGED <<booted, queue, owner>>
OrdinaryEnqueue == /\ ~queue /\ queue' = TRUE
                   /\ UNCHANGED <<booted, wakes, refresh, owner>>
Idle == /\ booted = Nodes /\ (\A n \in Nodes : refresh[n] = 2) /\ queue
        /\ UNCHANGED vars
Next == (\E node \in Nodes : Bootstrap(node) \/ Refresh(node)) \/ OrdinaryEnqueue \/ Idle
Spec == Init /\ [][Next]_vars /\ (\A node \in Nodes : WF_vars(Bootstrap(node)))
        /\ WF_vars(OrdinaryEnqueue)
WakeAuthority == \A n \in Nodes : ~Authorized(n) => wakes[n]["debt"] = 0
WakeOnce == \A n \in Nodes : wakes[n]["debt"] <= 1
OnlyDebt == \A n \in Nodes : wakes[n]["ordinary"] = 0
OwnedDebtEventuallyWoken == <> (\A n \in Nodes : Authorized(n) => wakes[n]["debt"] > 0)
OrdinaryQueueEventuallyRuns == <> queue
=============================================================================
