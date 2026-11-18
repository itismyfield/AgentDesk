------------------------- MODULE CampaignProfilePin -------------------------
EXTENDS Naturals, Sequences
CONSTANTS BindSource, BindScope, BindRole, ReplaceOldPin, SnapshotOpensExecution
VARIABLES input, snapshotInput, snapshotHash, repeatedHash,
          pin, firstPin, pinInput, result, execution, requested
vars == <<input, snapshotInput, snapshotHash, repeatedHash,
          pin, firstPin, pinInput, result, execution, requested>>

Absent == <<3, 3, 3>>
Old == <<2, 2, 2>>
Base == <<0, 0, 0>>
Inputs == {Base, <<1, 0, 0>>, <<0, 1, 0>>, <<0, 0, 1>>}
Hash(value) == <<IF BindSource THEN value[1] ELSE 0,
                IF BindScope THEN value[2] ELSE 0,
                IF BindRole THEN value[3] ELSE 0>>

Init ==
    /\ input = Base /\ snapshotInput = Absent /\ snapshotHash = Absent
    /\ repeatedHash = Absent
    /\ pin \in {Absent, Old, Hash(Base)} /\ firstPin = pin
    /\ pinInput = IF pin = Hash(Base) THEN Base ELSE Old
    /\ result = "unchecked" /\ execution = "unavailable" /\ requested = FALSE

\* Each construction is a new value; the already stored execution pin is separate.
CreateSnapshot ==
    /\ snapshotInput # input
    /\ snapshotInput' = input /\ snapshotHash' = Hash(input)
    /\ repeatedHash' = Absent /\ result' = "unchecked"
    /\ execution' = IF SnapshotOpensExecution THEN "ready" ELSE execution
    /\ UNCHANGED <<input, pin, firstPin, pinInput, requested>>

RepeatSnapshot ==
    /\ snapshotInput = input /\ repeatedHash = Absent
    /\ repeatedHash' = Hash(input)
    /\ UNCHANGED <<input, snapshotInput, snapshotHash, pin, firstPin,
                    pinInput, result, execution, requested>>

PinSnapshot ==
    /\ pin = Absent /\ snapshotInput = input
    /\ pin' = snapshotHash /\ firstPin' = snapshotHash /\ pinInput' = input
    /\ result' = "unchecked"
    /\ UNCHANGED <<input, snapshotInput, snapshotHash, repeatedHash,
                    execution, requested>>

ChangeInput ==
    /\ input = Base /\ pin # Absent
    /\ input' \in Inputs \ {Base} /\ result' = "unchecked"
    /\ UNCHANGED <<snapshotInput, snapshotHash, repeatedHash,
                    pin, firstPin, pinInput, execution, requested>>

RevalidatePin ==
    /\ pin # Absent /\ snapshotInput = input /\ result = "unchecked"
    /\ result' = IF pin = snapshotHash THEN "matching" ELSE "drift"
    /\ pin' = IF ReplaceOldPin /\ pin # snapshotHash THEN snapshotHash ELSE pin
    /\ UNCHANGED <<input, snapshotInput, snapshotHash, repeatedHash,
                    firstPin, pinInput, execution, requested>>

RequestExecution ==
    /\ ~requested /\ requested' = TRUE
    /\ UNCHANGED <<input, snapshotInput, snapshotHash, repeatedHash,
                    pin, firstPin, pinInput, result, execution>>

Done == requested /\ UNCHANGED vars
Next == CreateSnapshot \/ RepeatSnapshot \/ PinSnapshot \/ ChangeInput
        \/ RevalidatePin \/ RequestExecution \/ Done

SameInputSameHash == repeatedHash = Absent \/ repeatedHash = snapshotHash
NoFalseMatch == result = "matching" => pinInput = input
MatchingPinAccepted == result = "drift" => pinInput # input
NoSilentPinReplacement == pin = firstPin
SnapshotNeverOpensExecution == execution = "unavailable"
=============================================================================
