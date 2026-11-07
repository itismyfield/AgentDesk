------------------------- MODULE ManagedObserverRebind -------------------------
\* The owner's idle submit and the producer observer that ends its managed turn, across a /clear
\* whose new session B is adopted before or after the observer is built (before the submit).
\* Session A is the cached one the observer resolves while B's adoption is still pending.
\* After the submit the observer polls: this submit's receipt in A fixes A, a verified rebind
\* fixes B, the grace fixes A. /clear appends metadata to A, which is not a receipt.
\* Mut: "none"; "metaIsInput" counts A's growth as the receipt; "singlePoll" asks the rebind once.
\* GraceCoversAdoption: after a /clear the grace outlasts B's adoption and the next poll.
\* RepairLate: a reader of A with no receipt there leaves once for a B adopted after the grace.
CONSTANTS RepairRebind, Clears, Mut, GraceCoversAdoption, RepairLate
VARIABLES clear, adopt, observer, submitted, inputIn, aMeta, receiptA, answered, closed, left
vars == <<clear, adopt, observer, submitted, inputIn, aMeta, receiptA, answered, closed, left>>

Init ==
    /\ clear \in Clears
    /\ adopt = IF clear THEN "pending" ELSE "none"
    /\ observer = "none" /\ submitted = FALSE /\ inputIn = "none"
    /\ aMeta = FALSE /\ receiptA = FALSE
    /\ answered = FALSE /\ closed = FALSE /\ left = FALSE

\* owner_observer resolves before the submit: B once adopted, else the cached A.
Build ==
    /\ observer = "none"
    /\ observer' = IF adopt = "adopted" THEN "fixedB"
                   ELSE IF RepairRebind THEN "pollingA" ELSE "fixedA"
    /\ UNCHANGED <<clear, adopt, submitted, inputIn, aMeta, receiptA, answered, closed, left>>
\* The submit reaches the session the pane runs.
Submit ==
    /\ observer # "none" /\ ~submitted
    /\ submitted' = TRUE /\ inputIn' = IF clear THEN "B" ELSE "A"
    /\ UNCHANGED <<clear, adopt, observer, aMeta, receiptA, answered, closed, left>>
\* /clear's cost-state lands in the session it left.
ClearMetadata ==
    /\ clear /\ ~aMeta /\ aMeta' = TRUE
    /\ UNCHANGED <<clear, adopt, observer, submitted, inputIn, receiptA, answered, closed, left>>
\* The writer's receipt pins this submit's user row where it landed.
ReceiptInA ==
    /\ submitted /\ inputIn = "A" /\ ~receiptA /\ receiptA' = TRUE
    /\ UNCHANGED <<clear, adopt, observer, submitted, inputIn, aMeta, answered, closed, left>>
\* B's transcript is verified and adopted, at any point after the clear.
Adopt ==
    /\ adopt = "pending" /\ adopt' = "adopted"
    /\ UNCHANGED <<clear, observer, submitted, inputIn, aMeta, receiptA, answered, closed, left>>
Answer ==
    /\ submitted /\ ~answered /\ answered' = TRUE
    /\ UNCHANGED <<clear, adopt, observer, submitted, inputIn, aMeta, receiptA, closed, left>>
OwnEvidence == receiptA \/ (Mut = "metaIsInput" /\ aMeta)
\* One poll of rebound_source after the submit.
Poll ==
    /\ submitted /\ observer = "pollingA"
    /\ observer' = IF OwnEvidence THEN "fixedA"
                   ELSE IF adopt = "adopted" THEN "fixedB"
                   ELSE IF Mut = "singlePoll" THEN "fixedA"
                   ELSE "pollingA"
    /\ UNCHANGED <<clear, adopt, submitted, inputIn, aMeta, receiptA, answered, closed, left>>
\* The grace expires: the observer reads A as it did before the rebind wait.
Deadline ==
    /\ submitted /\ observer = "pollingA"
    /\ (~GraceCoversAdoption \/ adopt = "none")
    /\ observer' = "fixedA"
    /\ UNCHANGED <<clear, adopt, submitted, inputIn, aMeta, receiptA, answered, closed, left>>
\* After the grace a reader of A holding no receipt of this submit leaves once for the adopted B.
LateHandoff ==
    /\ RepairLate /\ submitted /\ observer = "fixedA" /\ adopt = "adopted"
    /\ ~OwnEvidence /\ ~left /\ ~closed
    /\ observer' = "fixedB" /\ left' = TRUE
    /\ UNCHANGED <<clear, adopt, submitted, inputIn, aMeta, receiptA, answered, closed>>
Reads == IF observer = "fixedA" THEN "A" ELSE IF observer = "fixedB" THEN "B" ELSE "none"
\* The producer ends the turn only on an answer in the session it reads.
Close ==
    /\ answered /\ Reads = inputIn /\ ~closed /\ closed' = TRUE
    /\ UNCHANGED <<clear, adopt, observer, submitted, inputIn, aMeta, receiptA, answered, left>>
\* A reader on the other session keeps tailing a file its answer never reaches: a lasso.
Tail == answered /\ ~closed /\ Reads \notin {"none", inputIn} /\ UNCHANGED vars
Done == closed /\ UNCHANGED vars

Next == Build \/ Submit \/ ClearMetadata \/ ReceiptInA \/ Adopt \/ Answer \/ Poll \/ Deadline
        \/ LateHandoff \/ Close \/ Tail \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Build) /\ WF_vars(Submit) /\ WF_vars(ClearMetadata)
        /\ WF_vars(ReceiptInA) /\ WF_vars(Adopt) /\ WF_vars(Answer) /\ WF_vars(Poll)
        /\ WF_vars(LateHandoff) /\ WF_vars(Close)

SubmittedEventuallyCloses == submitted ~> closed
ClosesOnItsOwnSession == closed => Reads = inputIn
FixedAOnlyOnEvidenceOrGrace == observer = "fixedA" /\ submitted /\ RepairRebind
                               => receiptA \/ inputIn = "A" \/ ~GraceCoversAdoption
=============================================================================
