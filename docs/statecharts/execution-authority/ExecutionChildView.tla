------------------------- MODULE ExecutionChildView -------------------------
EXTENDS ExecutionAuthority
CONSTANT ChildMode
VARIABLES childRecorded, childVisible, childPermit, childRest
childVars == <<childRecorded, childVisible, childPermit, childRest>>
allVars == <<vars, childVars>>
ChildInit == Init /\ childRecorded = TRUE /\ childVisible = FALSE
                 /\ childPermit = "none" /\ childRest = 0
\* O-a prepares a reader view; it does not install the parent's permit for the child.
ReadChild ==
    /\ ~childVisible
    /\ childVisible' = (ChildMode # "missing")
    /\ childPermit' = IF ChildMode = "alias" THEN "parent" ELSE "none"
    /\ childRest' = IF ChildMode = "rest" THEN 1 ELSE 0
    /\ UNCHANGED <<vars, childRecorded>>
ChildNext == (Next /\ UNCHANGED childVars) \/ ReadChild
ChildSpec == ChildInit /\ [][ChildNext]_allVars /\ WF_allVars(ReadChild) /\ Fairness
ChildRecordRead == <>childVisible
ChildOwnPermit == childPermit = "none"
ChildRecordedNoRest == childRest = 0
=============================================================================
