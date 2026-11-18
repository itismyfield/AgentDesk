------------------------- MODULE CampaignProfilePin -------------------------
EXTENDS Naturals, Sequences
CONSTANTS BindSource, BindScope, BindRole, ReplaceOldPin, SnapshotOpensExecution
VARIABLES input, snapshotInput, snapshotHash, repeatedHash,
          pin, firstPin, pinInput, result, execution, requested, runtime
profileVars == <<input, snapshotInput, snapshotHash, repeatedHash,
          pin, firstPin, pinInput, result, execution, requested>>
vars == <<profileVars, runtime>>

Absent == <<3, 3, 3>>
Old == <<2, 2, 2>>
Base == <<0, 0, 0>>
Inputs == {Base, <<1, 0, 0>>, <<0, 1, 0>>, <<0, 0, 1>>}
Hash(value) == <<IF BindSource THEN value[1] ELSE 0,
                IF BindScope THEN value[2] ELSE 0,
                IF BindRole THEN value[3] ELSE 0>>

ProfileInit ==
    /\ input = Base /\ snapshotInput = Absent /\ snapshotHash = Absent
    /\ repeatedHash = Absent
    /\ pin \in {Absent, Old, Hash(Base)} /\ firstPin = pin
    /\ pinInput = IF pin = Hash(Base) THEN Base ELSE Old
    /\ result = "unchecked" /\ execution = "unavailable" /\ requested = FALSE

Init == ProfileInit /\ runtime = "inactive"

\* Each construction is a new value; the already stored execution pin is separate.
CreateSnapshot ==
    /\ snapshotInput # input
    /\ snapshotInput' = input /\ snapshotHash' = Hash(input)
    /\ repeatedHash' = Absent /\ result' = "unchecked"
    /\ execution' = IF SnapshotOpensExecution THEN "ready" ELSE execution
    /\ UNCHANGED <<input, pin, firstPin, pinInput, requested, runtime>>

RepeatSnapshot ==
    /\ snapshotInput = input /\ repeatedHash = Absent
    /\ repeatedHash' = Hash(input)
    /\ UNCHANGED <<input, snapshotInput, snapshotHash, pin, firstPin,
                    pinInput, result, execution, requested, runtime>>

PinSnapshot ==
    /\ pin = Absent /\ snapshotInput = input
    /\ pin' = snapshotHash /\ firstPin' = snapshotHash /\ pinInput' = input
    /\ result' = "unchecked"
    /\ UNCHANGED <<input, snapshotInput, snapshotHash, repeatedHash,
                    execution, requested, runtime>>

ChangeInput ==
    /\ input = Base /\ pin # Absent
    /\ input' \in Inputs \ {Base} /\ result' = "unchecked"
    /\ UNCHANGED <<snapshotInput, snapshotHash, repeatedHash,
                    pin, firstPin, pinInput, execution, requested, runtime>>

RevalidatePin ==
    /\ pin # Absent /\ snapshotInput = input /\ result = "unchecked"
    /\ result' = IF pin = snapshotHash THEN "matching" ELSE "drift"
    /\ pin' = IF ReplaceOldPin /\ pin # snapshotHash THEN snapshotHash ELSE pin
    /\ UNCHANGED <<input, snapshotInput, snapshotHash, repeatedHash,
                    firstPin, pinInput, execution, requested, runtime>>

RequestExecution ==
    /\ ~requested /\ requested' = TRUE
    /\ UNCHANGED <<input, snapshotInput, snapshotHash, repeatedHash,
                    pin, firstPin, pinInput, result, execution, runtime>>

Done == requested /\ UNCHANGED vars
Next == CreateSnapshot \/ RepeatSnapshot \/ PinSnapshot \/ ChangeInput
        \/ RevalidatePin \/ RequestExecution \/ Done

SameInputSameHash == repeatedHash = Absent \/ repeatedHash = snapshotHash
NoFalseMatch == result = "matching" => pinInput = input
MatchingPinAccepted == result = "drift" => pinInput # input
NoSilentPinReplacement == pin = firstPin
SnapshotNeverOpensExecution == execution = "unavailable"

CheckProvider == TRUE
NeedObservation == TRUE
CheckIncarnation == TRUE
Unfenced == FALSE

RuntimeInit ==
    /\ ProfileInit
    /\ runtime = [source |-> "missing", current |-> 0, observed |-> 0,
                   dispatched |-> FALSE, foreign |-> FALSE, unobserved |-> FALSE]

ObserveRole(kind) ==
    /\ ~runtime.dispatched
    /\ runtime' = [runtime EXCEPT !.source = kind, !.observed = runtime.current]
    /\ UNCHANGED profileVars

RestartRuntime ==
    /\ runtime.current = 0 /\ ~runtime.dispatched
    /\ runtime' = [runtime EXCEPT !.current = 1]
    /\ UNCHANGED profileVars

InvokeAdapter ==
    /\ ~runtime.dispatched
    /\ ~NeedObservation \/ runtime.source # "missing"
    /\ ~CheckProvider \/ runtime.source # "foreign"
    /\ ~CheckIncarnation \/ runtime.observed = runtime.current
    /\ runtime' = [runtime EXCEPT !.dispatched = TRUE,
         !.foreign = runtime.source = "foreign",
         !.unobserved = runtime.source = "missing" \/ runtime.observed # runtime.current]
    /\ UNCHANGED profileVars

RuntimeNext == RestartRuntime \/ InvokeAdapter
               \/ (\E kind \in {"allowed", "foreign", "missing"} : ObserveRole(kind))
               \/ UNCHANGED vars
NoForeignInvocation == ~runtime.foreign
NoUnobservedInvocation == ~runtime.unobserved
 =============================================================================
