---- MODULE RefusedDisposition ----
\* One input whose readiness wait ended without evidence, followed past that end: where its body
\* stands, the one notice it owes, a daemon restart at any point and a retry of its ticket.
EXTENDS Naturals
CONSTANTS Policy, Mutant, Restarts
VARIABLES place, body, continuationBody, told, posting, outbox, restartsLeft, returned, keys
vars == <<place, body, continuationBody, told, posting, outbox, restartsLeft, returned, keys>>

\* "Before": the end is the retained proven-unsent input, which a restart requeues until it is
\* told. "Fixed": the end is a refusal for good that keeps no body.
Final == Policy = "Fixed"

\* The reserved input is an effort continuation: its restart intent owns another original.
Init == /\ place = "head" /\ body = TRUE /\ continuationBody = TRUE
        /\ told = FALSE /\ posting = FALSE /\ outbox = 0
        /\ restartsLeft = Restarts /\ returned = FALSE /\ keys = 0

\* The owner settles the head on the leaf's proof that no key went out.
Settle == /\ place = "head"
          /\ place' = IF Final THEN "refused" ELSE "retained"
          /\ body' = ~Final
          /\ continuationBody' = (~Final \/ Mutant = "RetainContinuation")
          /\ UNCHANGED <<told, posting, outbox, restartsLeft, returned, keys>>
\* The drain posts the owed notice; the outbox key keeps a repeat to one row.
Post == /\ place \in {"refused", "retained"} /\ ~told /\ ~posting
        /\ ~(Mutant = "DrainSkipsRefused" /\ place = "refused")
        /\ posting' = TRUE
        /\ outbox' = IF outbox = 0 \/ Mutant = "NoDedupe" THEN outbox + 1 ELSE outbox
        /\ UNCHANGED <<place, body, continuationBody, told, restartsLeft, returned, keys>>
\* The owner records that its notice went out.
Mark == /\ posting /\ posting' = FALSE /\ told' = TRUE
        /\ UNCHANGED <<place, body, continuationBody, outbox, restartsLeft, returned, keys>>
\* Recovery requeues before it drains; only an untold body the owner still keeps goes back.
Restart == /\ restartsLeft > 0 /\ place # "head"
           /\ restartsLeft' = restartsLeft - 1 /\ posting' = FALSE
           /\ IF place = "retained" /\ body /\ ~told
                 THEN place' = "items" /\ returned' = TRUE
                 ELSE UNCHANGED <<place, returned>>
           /\ UNCHANGED <<body, continuationBody, told, outbox, keys>>
\* The caller retries its ticket. The receipt and the kept identity answer; only the mutant
\* admits the copy the caller brought.
Retry == /\ place = "refused" /\ Mutant = "RetryAdmits"
         /\ place' = "items" /\ returned' = TRUE
         /\ UNCHANGED <<body, continuationBody, told, posting, outbox, restartsLeft, keys>>
\* A queued input is driven again without the person resending it.
Drive == /\ place = "items" /\ place' = "sent" /\ keys' = keys + 1
         /\ UNCHANGED <<body, continuationBody, told, posting, outbox, restartsLeft, returned>>
Quiet == /\ place # "head" /\ UNCHANGED vars

Next == Settle \/ Post \/ Mark \/ Restart \/ Retry \/ Drive \/ Quiet
Spec == Init /\ [][Next]_vars /\ WF_vars(Settle) /\ WF_vars(Post) /\ WF_vars(Mark)

TypeOK == /\ place \in {"head", "retained", "refused", "items", "sent"}
          /\ body \in BOOLEAN /\ continuationBody \in BOOLEAN
          /\ outbox \in 0..(Restarts + 1) /\ restartsLeft \in 0..Restarts /\ keys \in 0..1
\* A refused input never returns to the queue.
RefusedStaysOut == ~returned
NoBodyOnceSettled == place # "head" => (~body /\ ~continuationBody)
NoKeyAfterEnd == keys = 0
NoticeAtMostOnce == outbox <= 1
\* Exactly one notice: one row, and the owner knows it went out.
NoticeExactlyOnce == <>[](outbox = 1 /\ told)
=============================================================================
