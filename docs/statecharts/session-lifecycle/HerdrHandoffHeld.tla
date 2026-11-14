---- MODULE HerdrHandoffHeld ----
\* Companion to HerdrHandoffLaunch: a journal that outlives its channel's Herdr mapping.
EXTENDS Naturals, TLC
CONSTANTS Policy, Mapped, Host, Ended, Mutant, Cap
VARIABLES journal, ended, failures, wait, q, holds, delivered, retained, told, wrong
vars == <<journal, ended, failures, wait, q, holds, delivered, retained, told, wrong>>
Ids == 1..2
Front == IF q[1] = "queued" THEN 1 ELSE IF q[2] = "queued" THEN 2 ELSE 3
\* Boot config is fixed for the process; AlwaysLeft reads every channel as unmapped.
Left == ~Mapped \/ Mutant = "AlwaysLeft"
Init == /\ journal = TRUE /\ ended = Ended /\ failures \in 0..Cap /\ wait = 0
        /\ q = [i \in Ids |-> "queued"] /\ holds = 0
        /\ delivered = [i \in Ids |-> 0] /\ retained = [i \in Ids |-> 0]
        /\ told = [i \in Ids |-> 0] /\ wrong = FALSE
\* Without a journal the ordinary path owns the front: one reserve, one delivery.
Ordinary == /\ Front < 3 /\ ~journal
            /\ q' = [q EXCEPT ![Front] = "delivered"]
            /\ delivered' = [delivered EXCEPT ![Front] = @ + 1] /\ holds' = 0
            /\ UNCHANGED <<journal, ended, failures, wait, retained, told, wrong>>
\* The fixed prepare drops a journal whose old pane ended once its channel left Herdr.
CanRelease == /\ Front < 3 /\ journal /\ Policy = "fixed" /\ Left /\ Mutant # "NoRelease"
              /\ ended \/ Mutant = "ReleaseUnended"
Release == /\ CanRelease
           /\ journal' = FALSE /\ failures' = 0 /\ wait' = 0
           /\ wrong' = (wrong \/ Mapped \/ ~ended)
           /\ UNCHANGED <<ended, q, holds, delivered, retained, told>>
\* A hold no later look clears: the channel left Herdr unended, or its mapped host refuses.
Stuck == Left \/ (Mapped /\ Host = "refused")
Hold == /\ Front < 3 /\ journal /\ Stuck /\ ~CanRelease
        /\ wait = 0
        /\ LET n == failures + 1
               counted == Policy = "fixed" /\ Mutant # "NoCount"
               \* A mapped channel's unended handoff keeps custody like a failed close.
               stop == counted /\ n >= Cap /\ (Left \/ ended)
           IN /\ holds' = IF stop THEN 0 ELSE IF holds > Cap THEN holds ELSE holds + 1
              /\ q' = IF stop THEN [q EXCEPT ![Front] = "retained"] ELSE q
              /\ retained' = IF stop THEN [retained EXCEPT ![Front] = @ + 1] ELSE retained
              /\ journal' = ~stop
              /\ failures' = IF stop THEN 0 ELSE IF ~counted THEN failures ELSE IF n > Cap THEN Cap ELSE n
              /\ wait' = IF stop THEN 0 ELSE IF ~counted THEN 1 ELSE IF n > Cap THEN 2^Cap ELSE 2^n
        /\ UNCHANGED <<ended, delivered, told, wrong>>
\* A real handoff on a mapped channel: the old pane closes first, then one Herdr reserve.
Close == /\ Front < 3 /\ journal /\ ~Left /\ Host = "herdr" /\ ~ended
         /\ ended' = TRUE
         /\ UNCHANGED <<journal, failures, wait, q, holds, delivered, retained, told, wrong>>
Launch == /\ Front < 3 /\ journal /\ ~Left /\ Host = "herdr" /\ ended
          /\ q' = [q EXCEPT ![Front] = "delivered"]
          /\ delivered' = [delivered EXCEPT ![Front] = @ + 1]
          /\ journal' = FALSE /\ failures' = 0 /\ holds' = 0
          /\ UNCHANGED <<ended, wait, retained, told, wrong>>
Tick == /\ wait > 0 /\ wait' = wait - 1
        /\ UNCHANGED <<journal, ended, failures, q, holds, delivered, retained, told, wrong>>
Notice == /\ \E i \in Ids: retained[i] = 1 /\ told[i] = 0
          /\ told' = [i \in Ids |-> retained[i]]
          /\ UNCHANGED <<journal, ended, failures, wait, q, holds, delivered, retained, wrong>>
Next == Ordinary \/ Release \/ Hold \/ Close \/ Launch \/ Tick \/ Notice
Spec == Init /\ [][Next]_vars /\ WF_vars(Ordinary) /\ WF_vars(Release) /\ WF_vars(Hold)
        /\ WF_vars(Close) /\ WF_vars(Launch) /\ WF_vars(Tick) /\ WF_vars(Notice)
TypeOK == /\ journal \in BOOLEAN /\ ended \in BOOLEAN /\ wrong \in BOOLEAN
          /\ failures \in 0..Cap /\ wait \in 0..(2^Cap) /\ holds \in 0..(Cap + 1)
          /\ q \in [Ids -> {"queued", "delivered", "retained"}]
InputOnce == \A i \in Ids: delivered[i] + retained[i] <= 1
\* Safety: the timer never re-holds one front more often than the launch budget.
BoundedHolds == holds <= Cap
\* Only an ended journal of a channel that left Herdr is dropped with its input still queued.
OnlyEndedLeftReleased == ~wrong
\* An ended journal on a channel that left Herdr never costs its input.
EndedLeftDelivers == (~Mapped /\ Ended) => \A i \in Ids: retained[i] = 0
\* A real handoff on a mapped channel is never cut short into a retained input.
InProgressDelivers == (Mapped /\ Host = "herdr") => \A i \in Ids: retained[i] = 0
\* Liveness: every front is delivered, or retained and told.
FrontProgress == <> (\A i \in Ids: q[i] # "queued" /\ (retained[i] = 1 => told[i] = 1))
====
