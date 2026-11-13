-------------------- MODULE NativeInputPreparation --------------------
EXTENDS Naturals
CONSTANT Mode
VARIABLES phase, role, pin, prepared, anyKey, original, result, source, restored
vars == <<phase, role, pin, prepared, anyKey, original, result, source, restored>>
Init == /\ phase = "Intent" /\ role \in {"Primary", "Follower"}
        /\ pin = "None" /\ prepared = FALSE /\ anyKey = FALSE /\ original = TRUE
        /\ result = "None" /\ source = "PreSource" /\ restored = FALSE
Prepare == /\ phase = "Intent"
           /\ phase' = "Prepared" /\ prepared' = TRUE
           /\ pin' = IF role = "Primary" \/ Mode = "BorrowBusy" THEN "Primary" ELSE "None"
           /\ UNCHANGED <<role, anyKey, original, result, source, restored>>
AnyKey == /\ (phase = "Prepared" \/ (Mode = "KeyBeforePrepare" /\ phase = "Intent"))
          /\ phase' = "Unknown" /\ anyKey' = TRUE
          /\ UNCHANGED <<role, pin, prepared, original, result, source, restored>>
NotSent == /\ phase \in {"Intent", "Prepared"} /\ ~anyKey
           /\ phase' = "NotSent" /\ original' = (Mode # "DropNotSent")
           /\ UNCHANGED <<role, pin, prepared, anyKey, result, source, restored>>
RuntimeReady == /\ source = "PreSource" /\ source' = "Bound"
                /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, restored>>
Restart == /\ phase \in {"Prepared", "Unknown", "NotSent"} /\ ~restored
           /\ restored' = TRUE
           /\ UNCHANGED <<phase, role, pin, prepared, anyKey, original, result, source>>
Done == /\ phase \in {"Unknown", "NotSent"} /\ source = "Bound" /\ restored
        /\ UNCHANGED vars
Next == Prepare \/ AnyKey \/ NotSent \/ RuntimeReady \/ Restart \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Prepare) /\ WF_vars(AnyKey)
        /\ WF_vars(NotSent) /\ WF_vars(RuntimeReady) /\ WF_vars(Restart)
NoKeyBeforePrepare == anyKey => prepared
FollowerNoAuthority == role = "Follower" => pin = "None"
NoSilentLoss == original
NoNativeSettlement == result = "None"
PrimaryPinPreserved == (role = "Primary" /\ prepared) => pin = "Primary"
PreparationEventuallyLeavesIntent == <>(phase # "Intent")
SourceEventuallyBound == <>(source = "Bound")
=====================================================================
