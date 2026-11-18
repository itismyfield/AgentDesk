---------------------- MODULE WriterThreadChildHeld ----------------------
EXTENDS Naturals
CONSTANTS Fix, Mut, Scenario
VARIABLES verified, authority, owner, busy, legacy, oldO, nextO, boots, boundary,
          ready, live, debt, aAck, pending, reading, badOwner
vars == <<verified, authority, owner, busy, legacy, oldO, nextO, boots, boundary,
          ready, live, debt, aAck, pending, reading, badOwner>>
Init == /\ verified \in BOOLEAN /\ authority \in BOOLEAN
        /\ owner = "released" /\ busy = TRUE /\ legacy = 0 /\ oldO = 0
        /\ nextO = 0 /\ boots = 1 /\ boundary = 0 /\ ready = FALSE /\ live = TRUE
        /\ debt = FALSE /\ aAck = FALSE /\ pending = (Scenario = "pending")
        /\ reading = FALSE /\ badOwner = FALSE
Eligible == verified /\ authority
Normalize == /\ Fix # "before" /\ Mut # "normalize" /\ Eligible
             /\ owner = "released" /\ owner' = "legacy"
             /\ UNCHANGED <<verified, authority, busy, legacy, oldO, nextO, boots, boundary,
                            ready, live, debt, aAck, pending, reading, badOwner>>
\* One native offer of A; the existing transport is outside this ownership model.
LegacyBody == /\ owner \in {"legacy", "deferred"} /\ legacy = 0 /\ legacy' = 1
              /\ UNCHANGED <<verified, authority, owner, busy, oldO, nextO, boots, boundary,
                             ready, live, debt, aAck, pending, reading, badOwner>>
AReceipt == /\ legacy = 1 /\ ~aAck /\ aAck' = TRUE
            /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boots,
                           boundary, ready, live, debt, pending, reading, badOwner>>
StopHost == /\ owner = "legacy" /\ live /\ live' = FALSE
            /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boots,
                           boundary, ready, debt, aAck, pending, reading, badOwner>>
\* A may end before its body reaches the Legacy send claim. B's readiness is not A's ack.
EndRun == /\ busy /\ busy' = FALSE
          /\ UNCHANGED <<verified, authority, owner, legacy, oldO, nextO, boots, boundary,
                         ready, live, debt, aAck, pending, reading, badOwner>>
ObserveBoundary == /\ ~busy /\ owner = "legacy" /\ Mut # "boundary"
                   /\ (Scenario # "pending" \/ ~pending \/ (Fix = "after" /\ Mut # "pending"))
                   /\ (~reading \/ Fix # "after" \/ Mut = "owner")
                   /\ boundary' = boundary + 1 /\ owner' = "deferred"
                   /\ debt' = (Fix = "after" /\ Mut # "debt")
                   /\ UNCHANGED <<verified, authority, busy, legacy, oldO, nextO, boots,
                                  ready, live, aAck, pending, reading, badOwner>>
Resolve == /\ pending /\ owner = "deferred" /\ pending' = FALSE
           /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boots,
                          boundary, ready, live, debt, aAck, reading, badOwner>>
RestartAtBoundary == /\ owner = "deferred" /\ ~live /\ live' = TRUE /\ boots' = boots + 1
                     /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO,
                                    boundary, ready, debt, aAck, pending, reading, badOwner>>
\* bind and fresh takeover share the debt publication fence; B is already eligible here.
Activate == /\ owner = "deferred" /\ live /\ ~pending
            /\ (~debt \/ aAck \/ Mut = "borrowB") /\ owner' = "o" /\ ready' = TRUE
            /\ UNCHANGED <<verified, authority, busy, legacy, oldO, nextO, boots, boundary,
                           live, debt, aAck, pending, reading, badOwner>>
NextBody == /\ ready /\ nextO = 0 /\ nextO' = 1
            /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, boots, boundary,
                           ready, live, debt, aAck, pending, reading, badOwner>>
OldOBody == /\ owner = "o" /\ busy /\ oldO = 0 /\ oldO' = 1
            /\ UNCHANGED <<verified, authority, owner, busy, legacy, nextO, boots, boundary,
                           ready, live, debt, aAck, pending, reading, badOwner>>
BootAgain == /\ (Fix = "before" \/ Mut = "boot" \/ ~Eligible) /\ boundary = 0 /\ boots < 3
             /\ boots' = boots + 1
             /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boundary,
                            ready, live, debt, aAck, pending, reading, badOwner>>
BeginRead == /\ Scenario = "owner" /\ busy /\ owner = "legacy" /\ ~reading /\ reading' = TRUE
             /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boots,
                            boundary, ready, live, debt, aAck, pending, badOwner>>
FinishRead == /\ reading /\ reading' = FALSE /\ badOwner' = (owner # "legacy")
              /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boots,
                             boundary, ready, live, debt, aAck, pending>>
Next == Normalize \/ LegacyBody \/ AReceipt \/ StopHost \/ EndRun \/ ObserveBoundary
        \/ Resolve \/ RestartAtBoundary \/ Activate \/ NextBody \/ OldOBody \/ BootAgain
        \/ BeginRead \/ FinishRead
Spec == Init /\ [][Next]_vars /\ WF_vars(Normalize) /\ WF_vars(LegacyBody)
        /\ WF_vars(AReceipt) /\ WF_vars(EndRun) /\ WF_vars(ObserveBoundary)
        /\ WF_vars(Resolve) /\ WF_vars(RestartAtBoundary) /\ WF_vars(Activate)
        /\ WF_vars(NextBody) /\ WF_vars(FinishRead)
NoBootChurn == Eligible => boots <= boundary + 1
OneOwner == legacy <= 1 /\ oldO = 0 /\ nextO <= 1
VerifiedOnly == ~Eligible => owner # "legacy"
NoLostA == owner = "o" => legacy = 1 /\ aAck
AtomicOwner == ~badOwner
CurrentRunAnswered == Eligible => <>(legacy = 1)
BoundaryProgress == Eligible => <>ready
ReleasedResolves == Eligible => []((owner = "legacy" /\ ~busy) ~> (owner # "legacy"))
=============================================================================
