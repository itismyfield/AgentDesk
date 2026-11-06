------------------------- MODULE InputCancelSelection -------------------------
EXTENDS FiniteSets, TLC
CONSTANT GuardAmbiguity
VARIABLES location, offered, offeredFrom, executed, executedFrom
vars == <<location, offered, offeredFrom, executed, executedFrom>>
Originals == {"alpha", "beta", "normal"}
Primary(o) == IF o = "normal" THEN "N" ELSE "S"
Eligible == {o \in Originals : location[o] \in {"queued", "retained"}}
Unique(pool, o) == Cardinality({p \in pool : Primary(p) = Primary(o)}) = 1
Init == /\ location = [o \in Originals |-> "absent"]
        /\ offered = {} /\ offeredFrom = {} /\ executed = {} /\ executedFrom = {}
Move == \E o \in Originals, to \in {"queued", "retained", "protected", "absent"} :
          /\ location' = [location EXCEPT ![o] = to]
          /\ UNCHANGED <<offered, offeredFrom, executed, executedFrom>>
Autocomplete == \E query \in SUBSET Originals :
                  /\ offeredFrom' = Eligible
                  /\ offered' = {o \in Eligible : o \in query /\
                                      (~GuardAmbiguity \/ Unique(Eligible, o))}
                  /\ UNCHANGED <<location, executed, executedFrom>>
Cancel == \E o \in Eligible :
            /\ executedFrom' = Eligible
            /\ executed' = IF ~GuardAmbiguity \/ Unique(Eligible, o) THEN {o} ELSE {}
            /\ location' = IF executed' = {} THEN location
                            ELSE [location EXCEPT ![o] = "absent"]
            /\ UNCHANGED <<offered, offeredFrom>>
Next == Move \/ Autocomplete \/ Cancel
Spec == Init /\ [][Next]_vars
NoAmbiguousOffer == \A o \in offered : Unique(offeredFrom, o)
NoAmbiguousExecution == \A o \in executed : Unique(executedFrom, o)
=============================================================================
