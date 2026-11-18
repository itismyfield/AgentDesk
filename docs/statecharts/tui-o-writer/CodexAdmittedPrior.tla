-------------------------- MODULE CodexAdmittedPrior --------------------------
\* #7651 D1b (a): an earlier rollout A when a fresh admitted rollout B is taken.
\* The launch records B's tmux generation before discovery binds B, so A's own receipts are
\* in the previous generation (o_writer_legacy.rs codex_records reads only the current one).
\* The model keeps what is true about A apart from the evidence the takeover holds:
\* truth is A's real delivery state, ownedEv is a durable owed judgement made before the
\* admission, receiptEv is an earlier-generation receipt. The generation move, one restart and a
\* late Legacy delivery of A may come before the takeover commits. After the commit the installed
\* normalizer (output_disposition.rs finish_first) must accept the takeover's own init.
\* Mode "main" is the landed head (admitted init [B@0]); "design" is the D1b design; the rest are
\* mutants of one design rule each.
EXTENDS Naturals
CONSTANTS Mode
VARIABLES truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, phase, own, initA,
          proofOwed, alarm, finish
vars == <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, phase, own, initA,
          proofOwed, alarm, finish>>

Design == Mode # "main"
\* The owed judgement is durable and survives the generation move and a restart.
DropsOnGeneration == Mode = "dropOnGen"
DropsOnRestart == Mode = "memoryOnly"

Init == /\ truth \in {"delivered", "undelivered", "ambiguous"}
        /\ seen \in BOOLEAN
        \* Before the admission the actor judged A owed only when it really was unsent.
        /\ owedEv = (seen /\ truth = "undelivered") /\ everOwed = owedEv
        /\ receiptEv = (truth = "delivered")
        /\ genMoved = FALSE /\ restarted = FALSE /\ phase = "admitted" /\ own = FALSE
        /\ initA = FALSE /\ proofOwed = FALSE /\ alarm = FALSE /\ finish = "none"

\* first_input.rs: the launch stamps the new generation before discovery binds B.
MoveGeneration == /\ phase = "admitted" /\ ~genMoved
    /\ genMoved' = TRUE /\ phase' = "bound"
    /\ owedEv' = (owedEv /\ ~DropsOnGeneration)
    /\ UNCHANGED <<truth, seen, everOwed, receiptEv, restarted, own, initA, proofOwed, alarm, finish>>

\* One process restart before the commit; in-memory judgements are gone.
Restart == /\ phase \in {"admitted", "bound"} /\ ~restarted
    /\ restarted' = TRUE /\ owedEv' = (owedEv /\ ~DropsOnRestart)
    /\ UNCHANGED <<truth, seen, everOwed, receiptEv, genMoved, phase, own, initA, proofOwed, alarm, finish>>

\* Legacy finishes A late; its receipt lands in A's (now earlier) generation.
LateDelivery == /\ phase \in {"admitted", "bound"} /\ truth = "undelivered"
    /\ truth' = "delivered" /\ receiptEv' = TRUE
    /\ UNCHANGED <<seen, owedEv, everOwed, genMoved, restarted, phase, own, initA, proofOwed, alarm, finish>>

\* The takeover's judgement of A from the evidence it holds, never from the truth.
ReceiptSeen == receiptEv /\ (Mode # "ignoreLate" \/ ~genMoved)
Verdict == CASE ~Design -> "skip"
          [] ReceiptSeen -> "end"
          [] owedEv -> "owed"
          [] Mode = "absenceAsOwed" -> "owed"
          [] OTHER -> "unprovable"
Blocks == Mode = "designBlock" /\ Verdict = "unprovable"

\* The actor commits under the candidate lock and records the owed list it proved with the init.
Commit == /\ phase = "bound" /\ ~Blocks
    /\ phase' = "committed" /\ own' = TRUE
    /\ initA' = (Verdict = "owed") /\ proofOwed' = (Verdict = "owed")
    /\ alarm' = (Verdict = "unprovable")
    /\ UNCHANGED <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, finish>>

\* Environment: another episode's init that lists an extra source this takeover never proved.
ForeignCommit == /\ phase = "bound"
    /\ phase' = "committed" /\ own' = FALSE /\ initA' = TRUE /\ proofOwed' = FALSE
    /\ UNCHANGED <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, alarm, finish>>

\* Installed normalizer: finish_first adopts only an init that is exactly this takeover's
\* proved owed list followed by B at byte 0; the current check accepts B alone.
Accepts == CASE Mode \in {"main", "finishSingleton"} -> ~initA
          [] Mode = "finishAnyB" -> TRUE
          [] OTHER -> initA = proofOwed
Finish == /\ phase = "committed" /\ finish = "none"
    /\ finish' = (IF (own \/ Mode = "finishAnyB") /\ Accepts THEN "adopted" ELSE "existing")
    /\ UNCHANGED <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, phase, own, initA, proofOwed, alarm>>

Done == phase = "committed" /\ finish # "none" /\ UNCHANGED vars
Stuck == phase = "bound" /\ Blocks /\ UNCHANGED vars

Next == MoveGeneration \/ Restart \/ LateDelivery \/ Commit \/ ForeignCommit \/ Finish \/ Done \/ Stuck
Spec == Init /\ [][Next]_vars /\ WF_vars(MoveGeneration) /\ WF_vars(Commit) /\ WF_vars(Finish)

\* A proven-unsent A that was not delivered meanwhile stays owed, whatever moved in between.
ProvenOwedKept == own /\ everOwed /\ truth = "undelivered" => initA
\* A delivered A, also one Legacy delivered late, is never owed again.
DeliveredNotReposted == own /\ truth = "delivered" => ~initA
\* A missing receipt alone is never taken as proof that A is unsent.
NoAbsenceAsOwed == own /\ initA => everOwed
\* Taking B without an unprovable A is visible.
UnprovableAlarmed == Design /\ own /\ ~initA /\ ~ReceiptSeen /\ truth # "delivered" => alarm
\* The normalizer never adopts an init the takeover did not prove.
AdoptedMatchesProof == finish = "adopted" => (own /\ initA = proofOwed)
\* An unprovable A never holds the admitted rollout back.
AdmittedEventuallyCommitted == <>(phase = "committed")
\* The takeover's own commit is adopted by the installed normalizer within its budget.
OwnCommitAdopted == [](own => <>(finish = "adopted"))
=============================================================================
