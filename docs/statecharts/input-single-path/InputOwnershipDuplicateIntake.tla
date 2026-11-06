---------------- MODULE InputOwnershipDuplicateIntake ----------------
EXTENDS InputOwnershipAdmission, FiniteSets
CONSTANT CommittedRetry
VARIABLES arrivals, receipts, kept, deliveries, warnings, rebooted
intakeVars == <<arrivals, receipts, kept, deliveries, warnings, rebooted>>
duplicateVars == <<vars, intakeVars>>
Messages == {"A", "B"}
Paths == {"live", "catchUp"}
DuplicateInit ==
    /\ AdmissionInit
    /\ arrivals = {} /\ receipts = {} /\ kept = {}
    /\ deliveries = [m \in Messages |-> 0]
    /\ warnings = 0 /\ rebooted = FALSE

\* Actor-serialized admission; a committed ticket survives its original's delivery.
Receive(path, message) ==
    /\ <<path, message>> \notin arrivals
    /\ arrivals' = arrivals \cup {<<path, message>>}
    /\ IF message \notin receipts
       THEN /\ receipts' = receipts \cup {message}
            /\ kept' = kept \cup {message}
            /\ warnings' = warnings
       ELSE /\ UNCHANGED <<receipts, kept>>
            /\ warnings' = IF message \in kept \/ CommittedRetry \/ path = "catchUp"
                            THEN warnings ELSE warnings + 1
    /\ UNCHANGED <<vars, deliveries, rebooted>>
Deliver(message) ==
    /\ message \in kept
    /\ kept' = kept \ {message}
    /\ deliveries' = [deliveries EXCEPT ![message] = @ + 1]
    /\ UNCHANGED <<vars, arrivals, receipts, warnings, rebooted>>
Restart ==
    /\ ~rebooted /\ rebooted' = TRUE
    /\ UNCHANGED <<vars, arrivals, receipts, kept, deliveries, warnings>>
DuplicateNext == (\E p \in Paths, m \in Messages: Receive(p, m))
                 \/ (\E m \in Messages: Deliver(m)) \/ Restart
                 \/ UNCHANGED duplicateVars
DuplicateSpec == DuplicateInit /\ [][DuplicateNext]_duplicateVars
                 /\ (\A p \in Paths, m \in Messages: WF_duplicateVars(Receive(p, m)))
                 /\ (\A m \in Messages: WF_duplicateVars(Deliver(m)))
ExactlyOneDelivery == \A m \in Messages: deliveries[m] <= 1
NoIntakeLoss == \A m \in receipts: m \in kept \/ deliveries[m] = 1
NoFalseWarning == warnings = 0
EveryMessageDelivered == <>(\A m \in Messages: deliveries[m] = 1)
=====================================================================
