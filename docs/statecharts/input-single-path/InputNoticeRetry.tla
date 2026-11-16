------------------------- MODULE InputNoticeRetry -------------------------
EXTENDS Naturals, TLC
CONSTANTS Mode, Scenario
\* A single (source,start,end) and one later Body; the queue is seeded after
\* derive_scoped enqueued a Piece but persist_notice_evidence skipped its row.
VARIABLES evidence, excluded, owed, delayed, withheld, retryWithoutWake,
          phase, posted, restarted, sessionPresent, bodyPosted
vars == <<evidence, excluded, owed, delayed, withheld, retryWithoutWake,
          phase, posted, restarted, sessionPresent, bodyPosted>>

Init == /\ evidence = FALSE /\ excluded = FALSE /\ owed = TRUE
        /\ delayed = FALSE /\ withheld = FALSE /\ retryWithoutWake = FALSE
        /\ phase = "Select" /\ posted = 0 /\ restarted = FALSE
        /\ sessionPresent = TRUE /\ bodyPosted = FALSE

\* generation.rs select removes the delivery timer when evidence is absent.
Select == /\ phase = "Select"
          /\ delayed' = IF Mode = "Before" /\ ~evidence THEN FALSE ELSE delayed
          /\ phase' = "Deliver"
          /\ UNCHANGED <<evidence, excluded, owed, withheld, retryWithoutWake,
                         posted, restarted, sessionPresent, bodyPosted>>

Deliver ==
    /\ phase = "Deliver"
    /\ phase' = "Rotate"
    /\ IF owed /\ ~delayed
          THEN IF excluded \/ ((Scenario = "Floor" \/ (Scenario = "Untimed" /\ Mode = "NoRecordBoundary")) /\ Mode # "Before")
                 THEN /\ owed' = FALSE /\ excluded' = TRUE
                      /\ UNCHANGED <<delayed, withheld, retryWithoutWake, posted>>
               ELSE IF evidence
                 THEN /\ owed' = FALSE /\ posted' = posted + 1
                      /\ UNCHANGED <<excluded, delayed, withheld, retryWithoutWake>>
               ELSE /\ delayed' = TRUE /\ withheld' = TRUE
                    /\ retryWithoutWake' = (retryWithoutWake \/ withheld)
                    /\ UNCHANGED <<excluded, owed, posted>>
          ELSE UNCHANGED <<excluded, owed, delayed, withheld, retryWithoutWake, posted>>
    /\ UNCHANGED <<evidence, restarted, sessionPresent, bodyPosted>>

\* The finite pass rotates withheld input rather than blocking the later Body.
Rotate == /\ phase = "Rotate" /\ phase' = "Select" /\ bodyPosted' = TRUE
          /\ UNCHANGED <<evidence, excluded, owed, delayed, withheld,
                         retryWithoutWake, posted, restarted, sessionPresent>>

\* Materialized and terminal ledger writes are distinct wake dependencies.
ObserveEvidence == /\ Scenario = "Late" /\ ~evidence /\ ~excluded /\ owed
                   /\ evidence' = TRUE /\ withheld' = FALSE
                   /\ delayed' = IF Mode = "NoEvidenceWake" THEN delayed ELSE FALSE
                   /\ UNCHANGED <<excluded, owed, retryWithoutWake, phase, posted,
                                  restarted, sessionPresent, bodyPosted>>
ObserveLedger == /\ Scenario = "Ledger" /\ ~excluded /\ owed
                 /\ excluded' = TRUE /\ withheld' = FALSE
                 /\ delayed' = IF Mode = "NoLedgerWake" THEN delayed ELSE FALSE
                 /\ UNCHANGED <<evidence, owed, retryWithoutWake, phase, posted,
                                restarted, sessionPresent, bodyPosted>>

\* An actor incarnation may observe once at boot; durable acknowledgements survive.
Restart == /\ ~restarted /\ restarted' = TRUE /\ phase' = "Select"
           /\ owed' = (owed /\ ~excluded /\ posted = 0)
           /\ delayed' = FALSE /\ withheld' = FALSE
           /\ UNCHANGED <<evidence, excluded, retryWithoutWake, posted,
                          sessionPresent, bodyPosted>>
\* Missing live session alone never proves historical evidence impossible.
SessionGone == /\ sessionPresent /\ sessionPresent' = FALSE
               /\ UNCHANGED <<evidence, excluded, owed, delayed, withheld,
                              retryWithoutWake, phase, posted, restarted, bodyPosted>>

Next == Select \/ Deliver \/ Rotate \/ ObserveEvidence \/ ObserveLedger
        \/ Restart \/ SessionGone
Spec == Init /\ [][Next]_vars /\ WF_vars(Select) /\ WF_vars(Deliver)
        /\ WF_vars(Rotate) /\ WF_vars(ObserveEvidence) /\ WF_vars(ObserveLedger)

TypeOK == /\ evidence \in BOOLEAN /\ excluded \in BOOLEAN /\ owed \in BOOLEAN
          /\ delayed \in BOOLEAN /\ withheld \in BOOLEAN
          /\ retryWithoutWake \in BOOLEAN /\ phase \in {"Select", "Deliver", "Rotate"}
          /\ posted \in 0..1 /\ restarted \in BOOLEAN
          /\ sessionPresent \in BOOLEAN /\ bodyPosted \in BOOLEAN
NoRetryWithoutWake == ~retryWithoutWake
NoDuplicate == posted <= 1
NoEarlyLoss == Scenario \in {"Late", "Untimed"} => (~excluded /\ (owed \/ posted = 1))
EvidenceExactlyOnce == evidence ~> (posted = 1)
ImpossibleEventuallyClean == (Scenario = "Floor" \/ excluded) ~> ~owed
BodyEventuallyPosts == <>bodyPosted
=============================================================================
