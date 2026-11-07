------------------------- MODULE ManagedObserverRebind -------------------------
\* The owner's idle submit and the producer observer that ends its managed turn, across a /clear
\* whose new session B is adopted before or after the observer is built (before the submit).
\* Session A is the cached one the observer resolves while B's adoption is still pending.
CONSTANTS RepairRebind, Clears
VARIABLES clear, adopt, observer, submitted, inputIn, answered, closed
vars == <<clear, adopt, observer, submitted, inputIn, answered, closed>>

Init ==
    /\ clear \in Clears
    /\ adopt = IF clear THEN "pending" ELSE "none"
    /\ observer = "none" /\ ~submitted /\ inputIn = "none" /\ ~answered /\ ~closed

\* owner_observer resolves before the submit: B once adopted, else the cached A.
Build ==
    /\ observer = "none"
    /\ observer' = IF adopt = "adopted" THEN "B" ELSE "A"
    /\ UNCHANGED <<clear, adopt, submitted, inputIn, answered, closed>>
\* The submit reaches the session the pane runs.
Submit ==
    /\ observer # "none" /\ ~submitted
    /\ submitted' = TRUE /\ inputIn' = IF clear THEN "B" ELSE "A"
    /\ UNCHANGED <<clear, adopt, observer, answered, closed>>
\* B's transcript is verified and adopted, at any point after the clear.
Adopt ==
    /\ adopt = "pending" /\ adopt' = "adopted"
    /\ UNCHANGED <<clear, observer, submitted, inputIn, answered, closed>>
Answer ==
    /\ submitted /\ ~answered /\ answered' = TRUE
    /\ UNCHANGED <<clear, adopt, observer, submitted, inputIn, closed>>
\* After the submit and before A grows, the observer follows the pane's verified session.
Rebind ==
    /\ RepairRebind /\ submitted /\ observer = "A" /\ inputIn # "A" /\ adopt = "adopted"
    /\ observer' = "B"
    /\ UNCHANGED <<clear, adopt, submitted, inputIn, answered, closed>>
\* The producer ends the turn only on an answer in the session it reads.
Close ==
    /\ answered /\ observer = inputIn /\ ~closed /\ closed' = TRUE
    /\ UNCHANGED <<clear, adopt, observer, submitted, inputIn, answered>>
Done == closed /\ UNCHANGED vars

Next == Build \/ Submit \/ Adopt \/ Answer \/ Rebind \/ Close \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Build) /\ WF_vars(Submit) /\ WF_vars(Adopt)
        /\ WF_vars(Answer) /\ WF_vars(Rebind) /\ WF_vars(Close)

SubmittedEventuallyCloses == submitted ~> closed
ClosesOnItsOwnSession == closed => observer = inputIn
NoRebindAwayFromItsInput == observer = "B" => inputIn # "A"
=============================================================================
