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
          proofOwed, alarm, finish, obsInit
vars == <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, phase, own, initA,
          proofOwed, alarm, finish, obsInit>>

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
        /\ obsInit = FALSE

\* first_input.rs: the launch stamps the new generation before discovery binds B.
MoveGeneration == /\ phase = "admitted" /\ ~genMoved
    /\ genMoved' = TRUE /\ phase' = "bound"
    /\ owedEv' = (owedEv /\ ~DropsOnGeneration)
    /\ UNCHANGED <<truth, seen, everOwed, receiptEv, restarted, own, initA, proofOwed, alarm, finish, obsInit>>

\* One process restart before the commit; in-memory judgements are gone.
Restart == /\ phase \in {"admitted", "bound"} /\ ~restarted
    /\ restarted' = TRUE /\ owedEv' = (owedEv /\ ~DropsOnRestart)
    /\ UNCHANGED <<truth, seen, everOwed, receiptEv, genMoved, phase, own, initA, proofOwed, alarm, finish, obsInit>>

\* Legacy finishes A late; its receipt lands in A's (now earlier) generation.
LateDelivery == /\ phase \in {"admitted", "bound"} /\ truth = "undelivered"
    /\ truth' = "delivered" /\ receiptEv' = TRUE
    /\ UNCHANGED <<seen, owedEv, everOwed, genMoved, restarted, phase, own, initA, proofOwed, alarm, finish, obsInit>>

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
    /\ alarm' = (Verdict = "unprovable") /\ obsInit' = initA'
    /\ UNCHANGED <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, finish>>

\* Environment: the same takeover's commit (own B proof and identity) whose stored owed list
\* differs from the proof, e.g. an unproved A source, another frontier or prefix hash.
SkewedCommit == /\ phase = "bound" /\ ~Blocks
    /\ phase' = "committed" /\ own' = TRUE
    /\ initA' = (Verdict = "owed") /\ proofOwed' = (Verdict = "owed")
    /\ alarm' = (Verdict = "unprovable") /\ obsInit' = ~initA'
    /\ UNCHANGED <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, finish>>

\* Environment: another episode's init that lists an extra source this takeover never proved.
ForeignCommit == /\ phase = "bound"
    /\ phase' = "committed" /\ own' = FALSE /\ initA' = TRUE /\ proofOwed' = FALSE
    /\ obsInit' \in BOOLEAN
    /\ UNCHANGED <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, alarm, finish>>

\* Installed normalizer: finish_first adopts only this takeover's own init (identity check) whose
\* stored owed list is exactly the proved one followed by B at byte 0 (list check); the current
\* check accepts B alone. finishNoOwn drops only the identity check, finishNoList only the list.
Accepts == CASE Mode \in {"main", "finishSingleton"} -> ~obsInit
          [] Mode = "finishNoList" -> TRUE
          [] OTHER -> obsInit = proofOwed
Finish == /\ phase = "committed" /\ finish = "none"
    /\ finish' = (IF (own \/ Mode = "finishNoOwn") /\ Accepts THEN "adopted" ELSE "existing")
    /\ UNCHANGED <<truth, seen, owedEv, everOwed, receiptEv, genMoved, restarted, phase, own, initA, proofOwed, alarm, obsInit>>

Done == phase = "committed" /\ finish # "none" /\ UNCHANGED vars
Stuck == phase = "bound" /\ Blocks /\ UNCHANGED vars

Next == MoveGeneration \/ Restart \/ LateDelivery \/ Commit \/ SkewedCommit \/ ForeignCommit \/ Finish \/ Done \/ Stuck
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
AdoptedMatchesProof == finish = "adopted" => (own /\ obsInit = proofOwed)
\* An unprovable A never holds the admitted rollout back.
AdmittedEventuallyCommitted == <>(phase = "committed")
\* The takeover's own commit with its proved list is adopted within its budget; a skewed list
\* stays ExistingEffectPending.
OwnCommitAdopted == [](own /\ obsInit = proofOwed => <>(finish = "adopted"))
=============================================================================
