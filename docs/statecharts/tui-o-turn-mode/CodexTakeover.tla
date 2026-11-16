-------------------------- MODULE CodexTakeover --------------------------
EXTENDS Naturals
CONSTANT Policy
VARIABLES original, binding, ended, confirmed, pending, executions, legacy, restarted
vars == <<original, binding, ended, confirmed, pending, executions, legacy, restarted>>
Init == /\ original \in {"present", "archived", "missing"}
        /\ binding \in {"same", "advanced"}
        /\ ended = (original = "archived" \/ binding = "advanced")
        /\ legacy \in BOOLEAN
        /\ confirmed = FALSE /\ pending = TRUE /\ executions = 0 /\ restarted = FALSE
End == /\ original = "present" /\ ~ended /\ ended' = TRUE
       /\ UNCHANGED <<original, binding, confirmed, pending, executions, legacy, restarted>>
Archive == /\ original = "present" /\ ended /\ original' = "archived"
           /\ UNCHANGED <<binding, ended, confirmed, pending, executions, legacy, restarted>>
Advance == /\ binding = "same" /\ binding' = "advanced" /\ ended' = TRUE
           /\ UNCHANGED <<original, confirmed, pending, executions, legacy, restarted>>
LegacyEnds == /\ legacy /\ legacy' = FALSE
              /\ UNCHANGED <<original, binding, ended, confirmed, pending, executions, restarted>>
Recognized == (original = "present" /\ ended)
           \/ (Policy \in {"after", "no_binding"} /\ original = "archived")
           \/ (Policy \in {"after", "no_archive"} /\ binding = "advanced")
Confirm == /\ ~confirmed /\ ((Recognized /\ ~legacy) \/ Policy = "retry_all")
           /\ confirmed' = TRUE
           /\ UNCHANGED <<original, binding, ended, pending, executions, legacy, restarted>>
Deliver == /\ confirmed /\ pending /\ pending' = FALSE /\ executions' = executions + 1
           /\ UNCHANGED <<original, binding, ended, confirmed, legacy, restarted>>
Restart == /\ ~restarted /\ ~confirmed /\ restarted' = TRUE
           /\ UNCHANGED <<original, binding, ended, confirmed, pending, executions, legacy>>
Rest == UNCHANGED vars
Next == End \/ Archive \/ Advance \/ LegacyEnds \/ Confirm \/ Deliver \/ Restart \/ Rest
Spec == Init /\ [][Next]_vars /\ WF_vars(Confirm) /\ WF_vars(Deliver) /\ WF_vars(LegacyEnds)
NoEarlyConfirmation == confirmed => (ended /\ ~legacy)
NoDuplicateExecution == executions <= 1
NoSilentLoss == IF pending THEN executions = 0 ELSE executions = 1
NoInputBeforeConfirmation == executions > 0 => confirmed
TakeoverEventuallyConfirms == (ended /\ ~legacy) ~> confirmed
QueuedInputEventuallyRuns == confirmed ~> (executions = 1)
=============================================================================
