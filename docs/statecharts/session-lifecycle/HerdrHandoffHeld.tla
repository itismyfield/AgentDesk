---- MODULE HerdrHandoffHeld ----
\* Companion to HerdrHandoffLaunch: a journal no launch consumes, on a channel that left Herdr or
\* whose old pane cannot be confirmed ended. Input custody and old-pane custody end separately.
EXTENDS Naturals, TLC
CONSTANTS Policy, Mapped, Host, Ended, CloseFails, Target2, Cancels, Mutant, Cap, N
VARIABLES journal, orig, ended, up, failures, spender, wait, q, holds, looks, delivered, retained,
          told, wrong, early, booted
vars == <<journal, orig, ended, up, failures, spender, wait, q, holds, looks, delivered, retained,
          told, wrong, early, booted>>
Ids == 1..N
\* The leading waiting input; N + 1 when none waits.
Front == IF \E i \in Ids: q[i] = "queued"
         THEN CHOOSE i \in Ids: q[i] = "queued" /\ \A j \in Ids: j < i => q[j] # "queued"
         ELSE N + 1
Min(a, b) == IF a < b THEN a ELSE b
\* The first input runs in the channel's own session; the second may be a scheduled one of its own.
Target(i) == IF i = 2 THEN Target2 ELSE "same"
\* Boot config is fixed for the process; AlwaysLeft reads every channel as unmapped.
Left == ~Mapped \/ Mutant = "AlwaysLeft"
\* Custody that outlives its input exists from the second design on.
Split == Policy \in {"held2", "fixed"}
\* The budget follows the input that spends it; the second design kept one count per journal.
Shared == Policy # "fixed" \/ Mutant = "SharedBudget"
\* A queue-only removal deleted the journal of its input before this design.
QueueDrops == Policy # "fixed" \/ Mutant = "QueueCancelDropsCustody"
Owner == IF spender = 0 THEN orig ELSE spender
Init == /\ journal = TRUE /\ orig = 1 /\ ended = Ended /\ up = (Host = "herdr")
        /\ failures \in 0..Cap /\ spender = 0 /\ wait = 0
        /\ q = [i \in Ids |-> IF i = 1 THEN "queued" ELSE "incoming"] /\ holds = 0
        /\ looks = [i \in Ids |-> IF i = 1 THEN failures ELSE 0]
        /\ delivered = [i \in Ids |-> 0] /\ retained = [i \in Ids |-> 0]
        /\ told = [i \in Ids |-> 0] /\ wrong = FALSE /\ early = FALSE /\ booted = FALSE
\* Later inputs of any origin, in order; one may arrive after an earlier one was retained or after a reboot.
Arrive == /\ \E i \in Ids: /\ q[i] = "incoming" /\ \A j \in Ids: j < i => q[j] # "incoming"
                           /\ q' = [q EXCEPT ![i] = "queued"]
          /\ UNCHANGED <<journal, orig, ended, up, failures, spender, wait, holds, looks, delivered, retained, told, wrong, early, booted>>
\* A mapped channel whose old pane is unconfirmed never starts without a journal: the row re-opens one.
Rejudge == /\ Front <= N /\ ~journal /\ Mapped /\ ~ended
           /\ journal' = TRUE /\ orig' = Front
           /\ UNCHANGED <<ended, up, failures, spender, wait, q, holds, looks, delivered, retained, told, wrong, early, booted>>
\* Without a journal the ordinary path owns the front: one reserve on its own target.
Ordinary == /\ Front <= N /\ ~journal /\ ~(Mapped /\ ~ended)
            /\ q' = [q EXCEPT ![Front] = "delivered"]
            /\ delivered' = [delivered EXCEPT ![Front] = @ + 1] /\ holds' = 0
            /\ UNCHANGED <<journal, orig, ended, up, failures, spender, wait, looks, retained, told, wrong, early, booted>>
\* An ended journal is dropped once its channel left Herdr.
CanRelease == /\ Front <= N /\ journal /\ Policy # "before" /\ Left /\ Mutant # "NoRelease"
              /\ ended \/ Mutant = "ReleaseUnended"
Release == /\ CanRelease
           /\ journal' = FALSE /\ failures' = 0 /\ spender' = 0 /\ wait' = 0
           /\ wrong' = (wrong \/ Mapped \/ ~ended)
           /\ UNCHANGED <<orig, ended, up, q, holds, looks, delivered, retained, told, early, booted>>
\* An ended journal whose own input is gone is closed for a front bound to another session.
CanDischarge == /\ Front <= N /\ journal /\ ended /\ ~Left /\ Policy = "fixed"
                /\ Target(Front) = "other" /\ q[orig] # "queued"
Discharge == /\ CanDischarge
             /\ journal' = FALSE /\ failures' = 0 /\ spender' = 0 /\ wait' = 0
             /\ UNCHANGED <<orig, ended, up, q, holds, looks, delivered, retained, told, wrong, early, booted>>
\* A look this journal cannot pass by itself: the channel left Herdr unended, the mapped host
\* refuses, or the old pane does not close.
Refused == Mapped /\ ~up
CloseStuck == Mapped /\ up /\ ~ended /\ CloseFails
Stuck == Left \/ Refused \/ CloseStuck
\* The old pane's custody outlives the retained input only on a mapped channel.
Custody == Mapped /\ ~ended /\ Mutant # "DropCustody"
Hold == /\ Front <= N /\ journal /\ Stuck /\ ~CanRelease /\ ~CanDischarge /\ wait = 0
        /\ LET n == (IF Shared \/ Owner = Front THEN failures ELSE 0) + 1
               unendedMapped == Mapped /\ ~ended
               counted == /\ Policy # "before" /\ Mutant # "NoCount"
               bounded == \/ Split /\ Mutant # "RetryForever"
                             /\ ~(CloseStuck /\ Mutant = "CloseHolds")
                          \/ Policy = "held1" /\ ~unendedMapped
               stop == counted /\ bounded /\ n >= Cap
           IN /\ holds' = IF stop THEN 0 ELSE Min(holds + 1, Cap + 1)
              /\ looks' = IF counted THEN [looks EXCEPT ![Front] = Min(@ + 1, Cap + 1)] ELSE looks
              /\ q' = IF stop THEN [q EXCEPT ![Front] = "retained"] ELSE q
              /\ retained' = IF stop THEN [retained EXCEPT ![Front] = @ + 1] ELSE retained
              /\ journal' = IF stop THEN Custody ELSE TRUE
              /\ failures' = IF stop THEN 0 ELSE IF counted THEN Min(n, Cap) ELSE failures
              /\ spender' = IF stop THEN 0 ELSE IF counted THEN Front ELSE spender
              /\ wait' = IF stop THEN (IF Mutant = "CustodyTimer" /\ Custody THEN 2^Cap ELSE 0)
                         ELSE IF counted THEN 2^Min(n, Cap) ELSE 1
        /\ UNCHANGED <<orig, ended, up, delivered, told, wrong, early, booted>>
\* The journal's close goes on under whichever front leads; the old pane closes before any reserve.
Close == /\ Front <= N /\ journal /\ ~Left /\ up /\ ~ended /\ ~CloseFails /\ wait = 0
         /\ ended' = TRUE
         /\ UNCHANGED <<journal, orig, up, failures, spender, wait, q, holds, looks, delivered, retained, told, wrong, early, booted>>
\* The Herdr reserve consumes the journal only for a front in the journal's own session.
Launch == /\ Front <= N /\ journal /\ ~Left /\ up /\ ended /\ wait = 0 /\ Target(Front) = "same"
          /\ q' = [q EXCEPT ![Front] = "delivered"]
          /\ delivered' = [delivered EXCEPT ![Front] = @ + 1]
          /\ journal' = FALSE /\ failures' = 0 /\ spender' = 0 /\ holds' = 0
          /\ UNCHANGED <<orig, ended, up, wait, looks, retained, told, wrong, early, booted>>
\* Mutant only: a front under custody is handed to a host before the old pane's end is confirmed.
Bypass == /\ Mutant = "SendBeforeEnd" /\ Front <= N /\ journal /\ Mapped /\ ~ended /\ wait = 0
          /\ q' = [q EXCEPT ![Front] = "delivered"]
          /\ delivered' = [delivered EXCEPT ![Front] = @ + 1] /\ early' = TRUE /\ holds' = 0
          /\ UNCHANGED <<journal, orig, ended, up, failures, spender, wait, looks, retained, told, wrong, booted>>
\* The person cancels the leading input. Its budget ends with it; an unended pane stays guarded.
Cancel == /\ Cancels /\ Front <= N
          /\ LET i == Front
                 mine == Owner = i
                 drop == journal /\ orig = i /\ (QueueDrops \/ ~(Mapped /\ ~ended))
                 reset == drop \/ (~Shared /\ mine)
             IN /\ q' = [q EXCEPT ![i] = "cancelled"] /\ holds' = 0
                /\ journal' = (journal /\ ~drop)
                /\ failures' = IF reset THEN 0 ELSE failures
                /\ spender' = IF reset THEN 0 ELSE spender
                /\ wait' = IF reset THEN 0 ELSE wait
          /\ UNCHANGED <<orig, ended, up, looks, delivered, retained, told, wrong, early, booted>>
\* Every waiting input is cancelled at once; the session itself is not ended.
ClearWaiting == /\ Cancels /\ Front <= N
                /\ q' = [i \in Ids |-> IF q[i] = "queued" THEN "cancelled" ELSE q[i]]
                /\ journal' = (journal /\ ~QueueDrops /\ Mapped /\ ~ended)
                /\ failures' = 0 /\ spender' = 0 /\ wait' = 0 /\ holds' = 0
                /\ UNCHANGED <<orig, ended, up, looks, delivered, retained, told, wrong, early, booted>>
\* The refusing host may start accepting; nothing promises it does.
HostRecovers == /\ Host = "recovers" /\ ~up /\ up' = TRUE
                /\ UNCHANGED <<journal, orig, ended, failures, spender, wait, q, holds, looks, delivered, retained, told, wrong, early, booted>>
\* Every modelled value except the hold counter is the saved snapshot; a restart keeps them.
Boot == /\ ~booted /\ booted' = TRUE
        /\ UNCHANGED <<journal, orig, ended, up, failures, spender, wait, q, holds, looks, delivered, retained, told, wrong, early>>
Tick == /\ wait > 0 /\ wait' = wait - 1
        /\ UNCHANGED <<journal, orig, ended, up, failures, spender, q, holds, looks, delivered, retained, told, wrong, early, booted>>
Notice == /\ \E i \in Ids: retained[i] = 1 /\ told[i] = 0
          /\ told' = [i \in Ids |-> retained[i]]
          /\ UNCHANGED <<journal, orig, ended, up, failures, spender, wait, q, holds, looks, delivered, retained, wrong, early, booted>>
Next == Arrive \/ Rejudge \/ Ordinary \/ Release \/ Discharge \/ Hold \/ Close \/ Launch \/ Bypass
        \/ Cancel \/ ClearWaiting \/ HostRecovers \/ Boot \/ Tick \/ Notice
Spec == Init /\ [][Next]_vars /\ WF_vars(Arrive) /\ WF_vars(Rejudge) /\ WF_vars(Ordinary)
        /\ WF_vars(Release) /\ WF_vars(Discharge) /\ WF_vars(Hold) /\ WF_vars(Close)
        /\ WF_vars(Launch) /\ WF_vars(Bypass) /\ WF_vars(Tick) /\ WF_vars(Notice)
TypeOK == /\ journal \in BOOLEAN /\ ended \in BOOLEAN /\ up \in BOOLEAN /\ wrong \in BOOLEAN
          /\ early \in BOOLEAN /\ booted \in BOOLEAN /\ orig \in Ids /\ spender \in 0..N
          /\ failures \in 0..Cap /\ wait \in 0..(2^Cap) /\ holds \in 0..(Cap + 1)
          /\ looks \in [Ids -> 0..(Cap + 1)]
          /\ q \in [Ids -> {"incoming", "queued", "delivered", "retained", "cancelled"}]
InputOnce == \A i \in Ids: delivered[i] + retained[i] <= 1
\* No silent loss: an input that left the queue was delivered, retained or cancelled by its person.
NoLoss == \A i \in Ids: q[i] \in {"incoming", "queued", "cancelled"} \/ delivered[i] + retained[i] = 1
\* Safety: the timer never re-holds one front more often than the budget.
BoundedHolds == holds <= Cap
\* An input is retained only after its own looks spent the budget, never on another's count.
OwnBudget == \A i \in Ids: retained[i] = 1 => looks[i] >= Cap
\* Only an ended journal of a channel that left Herdr is dropped with its input still queued.
OnlyEndedLeftReleased == ~wrong
\* An ended journal on a channel that left Herdr never costs its input.
EndedLeftDelivers == (~Mapped /\ Ended) => \A i \in Ids: retained[i] = 0
\* A real handoff on a mapped channel is never cut short into a retained input.
InProgressDelivers == (Mapped /\ Host = "herdr" /\ ~CloseFails) => \A i \in Ids: retained[i] = 0
\* No input reaches a host before the old pane's end is confirmed.
NoSendBeforeEnd == ~early
\* A mapped channel's unconfirmed old pane stays under its journal, with or without an input.
CustodyKept == (Mapped /\ ~ended) => journal
\* No retry deadline stays armed once no input waits.
QuietWhenEmpty == (Front > N) => wait = 0
\* Liveness: every input is delivered, or retained and told, or cancelled by its person.
FrontProgress == <> (\A i \in Ids: q[i] \in {"delivered", "retained", "cancelled"}
                                   /\ (retained[i] = 1 => told[i] = 1))
====
