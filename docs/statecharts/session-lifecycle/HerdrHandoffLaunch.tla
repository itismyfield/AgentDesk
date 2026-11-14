---- MODULE HerdrHandoffLaunch ----
\* Companion to HerdrBusyHandoff: launch failures before reserve after RootReplaced.
EXTENDS Naturals, Sequences, TLC
CONSTANTS Policy, Restored, AlwaysFails, Mutant, Cap
VARIABLES row, phase, journal, failures, wait, q, attempts, delivered, retained, told, rebooted, early
vars == <<row, phase, journal, failures, wait, q, attempts, delivered, retained, told, rebooted, early>>
Ids == 1..2
Terminal == {"delivered", "retained", "cancelled"}
Front == IF q[1] = "queued" THEN 1 ELSE IF q[2] = "queued" THEN 2 ELSE 3
Init == /\ row = "Bound" /\ phase = "root" /\ journal = FALSE
        /\ failures \in (IF Restored THEN 0..Cap ELSE {0}) /\ wait = 0 /\ q = [i \in Ids |-> "incoming"]
        /\ attempts = [i \in Ids |-> 0] /\ delivered = [i \in Ids |-> 0]
        /\ retained = [i \in Ids |-> 0] /\ told = [i \in Ids |-> 0]
        /\ rebooted = FALSE /\ early = FALSE
RootReplaced == /\ phase = "root" /\ row' = "Retired" /\ phase' = "input"
                /\ UNCHANGED <<journal, failures, wait, q, attempts, delivered, retained, told, rebooted, early>>
Input == /\ phase = "input" /\ phase' = "prepare"
         /\ q' = [i \in Ids |-> "queued"] /\ journal' = Restored
         /\ UNCHANGED <<row, failures, wait, attempts, delivered, retained, told, rebooted, early>>
Prepare == /\ phase = "prepare" /\ Front < 3 /\ phase' = "launch"
           /\ journal' = (journal \/ (row = "Retired" /\ (Policy = "before" \/ Mutant = "RetiredJournal")))
           /\ UNCHANGED <<row, failures, wait, q, attempts, delivered, retained, told, rebooted, early>>
Launch ==
    /\ phase = "launch" /\ Front < 3
    /\ wait = 0 \/ Policy = "before" \/ Mutant = "ImmediateWake"
    /\ LET i == Front
           n == failures + 1
           limit == IF journal THEN Cap ELSE 2
           stop == AlwaysFails /\ ~(journal /\ Policy = "before") /\ n >= limit
       IN /\ attempts' = [attempts EXCEPT ![i] = IF @ > Cap THEN @ ELSE @ + 1]
          /\ early' = (early \/ wait > 0)
          /\ row' = IF ~AlwaysFails THEN "Bound" ELSE row
          /\ phase' = IF ~AlwaysFails \/ stop THEN "prepare" ELSE "launch"
          /\ q' = IF ~AlwaysFails THEN [q EXCEPT ![i] = "delivered"] ELSE IF stop THEN [q EXCEPT ![i] = "retained"] ELSE q
          /\ delivered' = IF ~AlwaysFails THEN [delivered EXCEPT ![i] = @ + 1] ELSE delivered
          /\ retained' = IF stop THEN [retained EXCEPT ![i] = @ + 1] ELSE retained
          /\ journal' = IF ~AlwaysFails \/ stop THEN FALSE ELSE journal
          /\ failures' = IF ~AlwaysFails \/ stop THEN 0 ELSE IF journal /\ (Policy = "before" \/ Mutant = "NoCount") THEN failures ELSE n
          /\ wait' = IF ~AlwaysFails \/ stop \/ ~journal \/ Policy = "before" THEN 0 ELSE 2^n
    /\ UNCHANGED <<told, rebooted>>
Tick == /\ wait > 0 /\ wait' = wait - 1
        /\ UNCHANGED <<row, phase, journal, failures, q, attempts, delivered, retained, told, rebooted, early>>
Notice == /\ \E i \in Ids: retained[i] = 1 /\ told[i] = 0
          /\ told' = [i \in Ids |-> retained[i]]
          /\ UNCHANGED <<row, phase, journal, failures, wait, q, attempts, delivered, retained, rebooted, early>>
\* Boot preserves the snapshot's failures/deadline. Actor rights are reacquired in Prepare.
Boot == /\ ~rebooted /\ phase = "launch" /\ rebooted' = TRUE /\ phase' = "prepare"
        /\ failures' = IF journal THEN failures ELSE 0
        /\ UNCHANGED <<row, journal, wait, q, attempts, delivered, retained, told, early>>
Cancel == /\ phase \in {"prepare", "launch"} /\ Front < 3
          /\ q' = [q EXCEPT ![Front] = "cancelled"] /\ phase' = "prepare"
          /\ journal' = FALSE /\ failures' = 0 /\ wait' = 0
          /\ UNCHANGED <<row, attempts, delivered, retained, told, rebooted, early>>
Clear == /\ phase \in {"prepare", "launch"} /\ Front < 3
         /\ q' = [i \in Ids |-> IF q[i] = "queued" THEN "cancelled" ELSE q[i]]
         /\ phase' = "prepare" /\ journal' = FALSE /\ failures' = 0 /\ wait' = 0
         /\ UNCHANGED <<row, attempts, delivered, retained, told, rebooted, early>>
Next == RootReplaced \/ Input \/ Prepare \/ Launch \/ Tick \/ Notice \/ Boot \/ Cancel \/ Clear
Spec == Init /\ [][Next]_vars /\ WF_vars(RootReplaced) /\ WF_vars(Input)
        /\ WF_vars(Prepare) /\ WF_vars(Launch) /\ WF_vars(Tick) /\ WF_vars(Notice)
TypeOK == /\ journal \in BOOLEAN /\ early \in BOOLEAN
          /\ phase \in {"root", "input", "prepare", "launch"}
          /\ row \in {"Bound", "Retired"} /\ failures \in 0..Cap /\ wait \in 0..(2^Cap)
          /\ q \in [Ids -> {"incoming", "queued", "delivered", "retained", "cancelled"}]
InputOnce == \A i \in Ids: delivered[i] + retained[i] <= 1
NoLoss == \A i \in Ids: q[i] \in {"incoming", "queued", "cancelled"} \/ delivered[i] + retained[i] = 1
BoundedRetries == \A i \in Ids: attempts[i] <= Cap
Backoff == ~early
RetiredIsFresh == ~Restored /\ Policy = "fixed" => ~journal
FrontProgress == <> (\A i \in Ids: q[i] \in Terminal /\ (retained[i] = 1 => told[i] = 1))
====
