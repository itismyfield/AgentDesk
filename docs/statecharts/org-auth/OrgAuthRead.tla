------------------------ MODULE OrgAuthRead ------------------------
EXTENDS Naturals, TLC
CONSTANT AuthInstalls, DropInstall, DropClear
VARIABLES mode, input, pc, initial, catalog, installs, clears, projected
vars == <<mode, input, pc, initial, catalog, installs, clears, projected>>
Init ==
    /\ mode \in {"Runtime", "Auth"}
    /\ input \in {"Missing", "Malformed", "ReadError", "RecoveryInvalid", "Decoded"}
    /\ initial \in {"Old", "Empty"}
    /\ catalog = initial /\ pc = "Read"
    /\ installs = 0 /\ clears = 0 /\ projected = FALSE
Decoded == input \in {"Decoded", "RecoveryInvalid"}
Read ==
    /\ pc = "Read" /\ pc' = "Effect"
    /\ UNCHANGED <<mode, input, initial, catalog, installs, clears, projected>>
Effect ==
    /\ pc = "Effect" /\ pc' = "Project"
    /\ LET runtime == mode = "Runtime" \/ AuthInstalls
           valid == input = "Decoded"
       IN /\ installs' = IF runtime /\ valid /\ ~DropInstall THEN 1 ELSE 0
          /\ clears' = IF runtime /\ ~valid /\ ~DropClear THEN 1 ELSE 0
          /\ catalog' = IF runtime /\ valid /\ ~DropInstall THEN "New"
                         ELSE IF runtime /\ ~valid /\ ~DropClear THEN "Empty"
                         ELSE catalog
    /\ UNCHANGED <<mode, input, initial, projected>>
Project ==
    /\ pc = "Project" /\ pc' = "Done"
    /\ projected' = IF mode = "Auth" THEN Decoded ELSE input = "Decoded"
    /\ UNCHANGED <<mode, input, initial, catalog, installs, clears>>
Done == pc = "Done" /\ UNCHANGED vars
Next == Read \/ Effect \/ Project \/ Done
AuthNoEffects == mode = "Auth" => installs = 0 /\ clears = 0 /\ catalog = initial
RuntimeInstall == (mode = "Runtime" /\ pc \in {"Project", "Done"} /\ input = "Decoded")
                  => installs = 1 /\ clears = 0 /\ catalog = "New"
RuntimeClear == (mode = "Runtime" /\ pc \in {"Project", "Done"} /\ input # "Decoded")
                => clears = 1 /\ installs = 0 /\ catalog = "Empty"
Projection == pc = "Done" => (projected <=> (Decoded /\ (mode = "Auth" \/ input = "Decoded")))
====================================================================
