------------------------- MODULE InputCancelSelection -------------------------
EXTENDS Naturals, FiniteSets, TLC
CONSTANTS GuardAmbiguity, GuardLegacy, RevokeFront
VARIABLES location, offered, offeredFrom, executed, executedFrom,
          grant, frontRemoved, cancelRevoked
vars == <<location, offered, offeredFrom, executed, executedFrom,
          grant, frontRemoved, cancelRevoked>>
Originals == {"alpha", "beta", "normal"}
Primary(o) == IF o = "normal" THEN "N" ELSE "S"
Rank(o) == CASE o = "alpha" -> 1 [] o = "beta" -> 2 [] OTHER -> 3
Queued == {o \in Originals : location[o] = "queued"}
Front == IF Queued = {} \/ (\E o \in Originals : location[o] = "protected") THEN "none"
         ELSE CHOOSE o \in Queued : \A p \in Queued : Rank(o) <= Rank(p)
Eligible == {o \in Originals : location[o] \in {"queued", "retained"}}
Unique(pool, o) == Cardinality({p \in pool : Primary(p) = Primary(o)}) = 1
Init == /\ location = [o \in Originals |-> "absent"]
        /\ offered = {} /\ offeredFrom = {} /\ executed = {} /\ executedFrom = {}
        /\ grant = "none" /\ frontRemoved = TRUE /\ cancelRevoked = TRUE
Move == \E o \in Originals, to \in {"queued", "retained", "protected", "absent"} :
          /\ location' = [location EXCEPT ![o] = to]
          /\ grant' = IF grant = o \/ to = "protected" THEN "none" ELSE grant
          /\ UNCHANGED <<offered, offeredFrom, executed, executedFrom,
                         frontRemoved, cancelRevoked>>
Autocomplete == \E query \in SUBSET Originals :
                  /\ offeredFrom' = Eligible
                  /\ offered' = {o \in Eligible : o \in query /\
                                      (~GuardAmbiguity \/ Unique(Eligible, o))}
                  /\ UNCHANGED <<location, executed, executedFrom, grant,
                                 frontRemoved, cancelRevoked>>
Cancel == \E o \in Eligible :
            /\ executedFrom' = Eligible
            /\ executed' = IF ~GuardAmbiguity \/ Unique(Eligible, o) THEN {o} ELSE {}
            /\ location' = IF executed' = {} THEN location
                            ELSE [location EXCEPT ![o] = "absent"]
            /\ grant' = IF executed' # {} /\ o = Front THEN "none" ELSE grant
            /\ UNCHANGED <<offered, offeredFrom, frontRemoved, cancelRevoked>>
OwnerGrant == /\ Front # "none"
              /\ grant' = Front
              /\ UNCHANGED <<location, offered, offeredFrom, executed, executedFrom,
                             frontRemoved, cancelRevoked>>
OwnerClaim == \E requested \in Originals :
                /\ grant' = IF grant = requested /\ Front = requested THEN "none" ELSE grant
                /\ UNCHANGED <<location, offered, offeredFrom, executed, executedFrom,
                               frontRemoved, cancelRevoked>>
LegacyCancelFront ==
    /\ Front # "none"
    /\ LET removed == ~GuardLegacy \/ Unique(Eligible, Front)
       IN /\ frontRemoved' = removed
          /\ cancelRevoked' = (~removed \/ grant = "none" \/ RevokeFront)
          /\ grant' = IF removed /\ RevokeFront THEN "none" ELSE grant
          /\ location' = IF removed THEN [location EXCEPT ![Front] = "absent"] ELSE location
    /\ UNCHANGED <<offered, offeredFrom, executed, executedFrom>>
Next == Move \/ Autocomplete \/ Cancel \/ OwnerGrant \/ OwnerClaim \/ LegacyCancelFront
Spec == Init /\ [][Next]_vars
NoAmbiguousOffer == \A o \in offered : Unique(offeredFrom, o)
NoAmbiguousExecution == \A o \in executed : Unique(executedFrom, o)
LegacyFrontRemoved == frontRemoved
FrontGrantRevoked == cancelRevoked
=============================================================================
