-------------------------- MODULE SessionIdentityGate --------------------------
EXTENDS Naturals, FiniteSets
CONSTANT GuardEnabled
VARIABLES marker, legacy, operation, effect, checked
vars == <<marker, legacy, operation, effect, checked>>
Markers == {"absent", "same", "foreign", "malformed", "unset", "probe_failed"}
Operations == {"send", "paste", "kill", "pane"}
Init == /\ marker \in Markers
        /\ legacy \in BOOLEAN
        /\ operation \in Operations
        /\ effect = FALSE
        /\ checked = FALSE
Admitted == legacy \/ marker = "same"
Attempt == /\ ~checked
           /\ checked' = TRUE
           /\ effect' = (~GuardEnabled \/ Admitted)
           /\ UNCHANGED <<marker, legacy, operation>>
Next == Attempt
NoForeignMutation == ~legacy /\ marker # "same" => ~effect
Spec == Init /\ [][Next]_vars
=============================================================================
