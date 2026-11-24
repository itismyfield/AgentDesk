-------------------- MODULE WatcherRetryReceipt --------------------
\* A watcher stale-resume schedules a same-id retry, then its watcher-death cleanup removes the row.
\* Receipts keep the first terminal and merge origins; a card needs an origin and a terminal.
EXTENDS Naturals
CONSTANTS Mode   \* "Before": cleanup records failed. "Fixed": a carried retry owns the terminal.
CONSTANT CarryLost \* The origin carry write failed, so cleanup cannot see the retry owner.

VARIABLES carried, origin, terminal, row, mailbox, retry, ran, cards
vars == <<carried, origin, terminal, row, mailbox, retry, ran, cards>>

First(t) == IF terminal = "None" THEN t ELSE terminal

Init == /\ carried = (Mode = "Fixed" /\ ~CarryLost) /\ origin = carried
        /\ terminal = "None" /\ row = TRUE /\ mailbox = "Held" /\ retry = "Scheduled"
        /\ ran = FALSE /\ cards = 0

\* Watcher-death cleanup: commit before the row goes, then release the dead turn's mailbox.
Cleanup == /\ row
           /\ IF Mode = "Fixed" /\ carried
                 THEN UNCHANGED <<terminal, origin>>
                 ELSE terminal' = First("failed") /\ origin' = TRUE
           /\ row' = FALSE /\ mailbox' = "Free"
           /\ UNCHANGED <<carried, retry, ran, cards>>

\* The retry enqueues after its history read; the dead turn still holding the slot refuses it.
Enqueue == /\ retry = "Scheduled"
           /\ retry' = IF mailbox = "Held" THEN "Refused" ELSE "Enqueued"
           /\ UNCHANGED <<carried, origin, terminal, row, mailbox, ran, cards>>

\* A refused retry never runs, so the execution failed; Before leaves nothing behind.
Refused == /\ retry = "Refused" /\ retry' = "Done"
           /\ IF Mode = "Fixed" THEN terminal' = First("failed") ELSE UNCHANGED terminal
           /\ UNCHANGED <<carried, origin, row, mailbox, ran, cards>>

\* The same-id retry runs and records its own receipt under the same execution id.
Run == /\ retry = "Enqueued" /\ mailbox = "Free" /\ retry' = "Done" /\ ran' = TRUE
       /\ terminal' = First("completed")
       /\ UNCHANGED <<carried, origin, row, mailbox, cards>>

Publish == /\ origin /\ terminal # "None" /\ cards = 0 /\ cards' = 1
           /\ UNCHANGED <<carried, origin, terminal, row, mailbox, retry, ran>>

Next == Cleanup \/ Enqueue \/ Refused \/ Run \/ Publish

Spec == Init /\ [][Next]_vars /\ WF_vars(Cleanup) /\ WF_vars(Enqueue) /\ WF_vars(Refused)
        /\ WF_vars(Run) /\ WF_vars(Publish)

NoFailedAfterRetrySuccess == ran => terminal # "failed"
AtMostOneCard == cards <= 1
CardEventuallyPublished == <>(cards = 1)
=============================================================================
