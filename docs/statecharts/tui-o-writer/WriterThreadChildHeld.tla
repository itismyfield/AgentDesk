---------------------- MODULE WriterThreadChildHeld ----------------------
EXTENDS Naturals
CONSTANTS Fix, Mut
VARIABLES verified, authority, owner, busy, legacy, oldO, nextO, boots, boundary, ready, live
vars == <<verified, authority, owner, busy, legacy, oldO, nextO, boots, boundary, ready, live>>
Init == /\ verified \in BOOLEAN /\ authority \in BOOLEAN
        /\ owner = "released" /\ busy = TRUE /\ legacy = 0 /\ oldO = 0
        /\ nextO = 0 /\ boots = 1 /\ boundary = 0 /\ ready = FALSE /\ live = TRUE
Eligible == verified /\ authority
Normalize == /\ Fix = "after" /\ Mut # "normalize" /\ Eligible
             /\ owner = "released" /\ owner' = "legacy"
             /\ UNCHANGED <<verified, authority, busy, legacy, oldO, nextO, boots, boundary, ready, live>>
LegacyBody == /\ owner = "legacy" /\ busy /\ legacy = 0 /\ legacy' = 1
              /\ UNCHANGED <<verified, authority, owner, busy, oldO, nextO, boots, boundary, ready, live>>
StopHost == /\ owner = "legacy" /\ live /\ live' = FALSE
            /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boots, boundary, ready>>
EndRun == /\ legacy = 1 /\ busy /\ busy' = FALSE
          /\ UNCHANGED <<verified, authority, owner, legacy, oldO, nextO, boots, boundary, ready, live>>
\* Turn end, session end and a new execution bind all end this frozen authority.
ObserveBoundary == /\ ~busy /\ owner = "legacy" /\ Mut # "boundary"
                   /\ boundary' = boundary + 1 /\ owner' = "deferred"
                   /\ UNCHANGED <<verified, authority, busy, legacy, oldO, nextO, boots, ready, live>>
RestartAtBoundary == /\ owner = "deferred" /\ ~live /\ live' = TRUE /\ boots' = boots + 1
                     /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boundary, ready>>
Activate == /\ owner = "deferred" /\ live /\ owner' = "o" /\ ready' = TRUE
            /\ UNCHANGED <<verified, authority, busy, legacy, oldO, nextO, boots, boundary, live>>
NextBody == /\ ready /\ nextO = 0 /\ nextO' = 1
            /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, boots, boundary, ready, live>>
OldOBody == /\ owner = "o" /\ busy /\ oldO = 0 /\ oldO' = 1
            /\ UNCHANGED <<verified, authority, owner, busy, legacy, nextO, boots, boundary, ready, live>>
BootAgain == /\ (Fix = "before" \/ Mut = "boot") /\ boundary = 0 /\ boots < 3
             /\ boots' = boots + 1
             /\ UNCHANGED <<verified, authority, owner, busy, legacy, oldO, nextO, boundary, ready, live>>
Next == Normalize \/ LegacyBody \/ StopHost \/ EndRun \/ ObserveBoundary \/ RestartAtBoundary \/ Activate \/ NextBody \/ OldOBody \/ BootAgain
Spec == Init /\ [][Next]_vars /\ WF_vars(Normalize) /\ WF_vars(LegacyBody)
        /\ WF_vars(EndRun) /\ WF_vars(ObserveBoundary) /\ WF_vars(RestartAtBoundary)
        /\ WF_vars(Activate) /\ WF_vars(NextBody)
NoBootChurn == boots <= boundary + 1
OneOwner == legacy <= 1 /\ oldO = 0 /\ nextO <= 1
VerifiedOnly == ~Eligible => owner # "legacy"
CurrentRunAnswered == Eligible => <>(legacy = 1)
BoundaryProgress == Eligible => <>ready
=============================================================================
