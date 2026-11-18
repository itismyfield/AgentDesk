------------------------ MODULE CampaignApprovalGate ------------------------
EXTENDS Naturals, Sequences
CONSTANTS Slots, TargetKinds, IgnoredField, DuplicateApprovals, OmittedClears
VARIABLES current, bound, gateState, required, configured, accepts,
          acceptedRequest, wrongBinding, omittedTried, omittedCleared
vars == <<current, bound, gateState, required, configured, accepts,
          acceptedRequest, wrongBinding, omittedTried, omittedCleared>>

\* Key positions: run, node, gate, definition revision, target kind/identity/head, policy.
Key(slot, kind) == <<0, 0, slot, 0, kind, 0, 0, 0>>
Change(key, field) ==
    [key EXCEPT ![field] = IF field = 3 THEN (IF @ = 6 THEN 1 ELSE @ + 1)
                           ELSE IF field = 5 THEN (IF @ = 3 THEN 1 ELSE @ + 1)
                           ELSE 1]
Requests == {bound} \cup {Change(bound, field) : field \in 1..8}
Matches(a, b) == \A field \in 1..8 :
    IF IgnoredField = 9 \/ field = IgnoredField THEN TRUE ELSE a[field] = b[field]

Init ==
    /\ \E slot \in Slots, kind \in TargetKinds :
        /\ kind = (IF slot <= 4 THEN 1 ELSE IF slot = 5 THEN 2 ELSE 3)
        /\ current = Key(slot, kind)
    /\ bound = current /\ acceptedRequest = current
    /\ gateState = "absent" /\ required = {} /\ configured = FALSE
    /\ accepts = 0 /\ wrongBinding = FALSE
    /\ omittedTried = FALSE /\ omittedCleared = FALSE

ConfigureGates ==
    /\ ~configured /\ configured' = TRUE /\ required' = 1..6
    /\ UNCHANGED <<current, bound, gateState, accepts, acceptedRequest,
                    wrongBinding, omittedTried, omittedCleared>>

OpenGate ==
    /\ gateState = "absent" /\ current[3] \in required
    /\ bound' = current /\ gateState' = "open"
    /\ UNCHANGED <<current, required, configured, accepts, acceptedRequest,
                    wrongBinding, omittedTried, omittedCleared>>

ChangeCurrentBinding ==
    /\ gateState # "absent" /\ current = bound
    /\ \E field \in 1..8 : current' = Change(current, field)
    /\ UNCHANGED <<bound, gateState, required, configured, accepts,
                    acceptedRequest, wrongBinding, omittedTried, omittedCleared>>

\* Reopening target A after B keeps A's full-key history, including its prior approval.
RestoreTargetBinding ==
    /\ gateState # "absent" /\ current # bound
    /\ \A field \in {1, 2, 3, 4, 5, 8} : current[field] = bound[field]
    /\ current' = bound
    /\ UNCHANGED <<bound, gateState, required, configured, accepts,
                    acceptedRequest, wrongBinding, omittedTried, omittedCleared>>

Approve(request) ==
    /\ gateState \in {"open", "approved"} /\ bound[3] \in required
    /\ Matches(request, bound) /\ Matches(current, bound)
    /\ accepts < 2
    /\ gateState' = "approved" /\ acceptedRequest' = request
    /\ accepts' = IF gateState = "open" \/ DuplicateApprovals THEN accepts + 1 ELSE accepts
    /\ wrongBinding' = (wrongBinding \/ request # bound \/ current # bound)
    /\ UNCHANGED <<current, bound, required, configured, omittedTried, omittedCleared>>

EditWithOmittedOptions ==
    /\ required # {} /\ ~omittedTried /\ omittedTried' = TRUE
    /\ required' = IF OmittedClears THEN {} ELSE required
    /\ omittedCleared' = OmittedClears
    /\ UNCHANGED <<current, bound, gateState, configured, accepts,
                    acceptedRequest, wrongBinding>>

EditWithExplicitOptions ==
    /\ configured /\ current[4] = 0
    /\ current' = Change(current, 4) /\ required' \in {{}, 1..6}
    /\ UNCHANGED <<bound, gateState, configured, accepts, acceptedRequest,
                    wrongBinding, omittedTried, omittedCleared>>

Done == UNCHANGED vars
ApproveGate == \E request \in Requests : Approve(request)
Next == ConfigureGates \/ OpenGate \/ ChangeCurrentBinding \/ RestoreTargetBinding
        \/ ApproveGate
        \/ EditWithOmittedOptions \/ EditWithExplicitOptions \/ Done

NoWrongBindingAccepted == ~wrongBinding
SameGateAcceptedAtMostOnce == accepts <= 1
OmittedEditNeverClearsGate == ~omittedCleared
ApprovalDefaultsOff == ~configured => required = {}
=============================================================================
