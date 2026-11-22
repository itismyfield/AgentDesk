-------------------- MODULE NativeReceiptRestart --------------------
\* An origin-carrying native execution whose terminal receipt write failed, across a restart.
\* The row stays marked until the receipt commits; the cleanup CAS refuses a restart-marked row.
EXTENDS Naturals
CONSTANTS Mode   \* "Before": shutdown marks the stamped row and boot fails on a pending receipt.

VARIABLES pg, receipt, row, marker, phase
vars == <<pg, receipt, row, marker, phase>>

\* The live drive already tried once while PG was down: the row holds a terminal receipt stamp.
Init == /\ pg = "Down" /\ receipt = "Stamped" /\ row = TRUE /\ marker = FALSE
        /\ phase = "Live"

\* One cleanup attempt: commit the receipt, then the snapshot CAS removes an unmarked row.
Cleanup(next) == /\ row /\ pg = "Up"
                 /\ receipt' = "Committed"
                 /\ IF marker THEN row' = row ELSE row' = FALSE
                 /\ phase' = next
                 /\ UNCHANGED <<pg, marker>>

PgRecover == /\ pg = "Down" /\ pg' = "Up" /\ UNCHANGED <<receipt, row, marker, phase>>

LiveRetry == phase = "Live" /\ Cleanup("Live")

\* Graceful shutdown marks live rows DrainRestart; a stamped row ended and is left as it is.
Shutdown == /\ phase = "Live" /\ phase' = "Stopped"
            /\ marker' = (marker \/ (row /\ (Mode = "Before" \/ receipt # "Stamped")))
            /\ UNCHANGED <<pg, receipt, row>>

Boot == /\ phase \in {"Stopped", "Held"} /\ phase' = "Booting"
        /\ UNCHANGED <<pg, receipt, row, marker>>

\* Native boot settlement runs before the runtime starts; any refusal holds the whole runtime.
BootSettle == /\ phase = "Booting"
              /\ \/ (~row /\ phase' = "Running" /\ UNCHANGED <<pg, receipt, row, marker>>)
                 \/ (row /\ pg = "Down" /\ UNCHANGED <<pg, receipt, row, marker>>
                     /\ phase' = IF Mode = "Before" THEN "Held" ELSE "Running")
                 \/ (row /\ pg = "Up" /\ marker /\ receipt' = "Committed" /\ phase' = "Held"
                     /\ UNCHANGED <<pg, row, marker>>)
                 \/ (row /\ pg = "Up" /\ ~marker /\ Cleanup("Running"))

\* The running writer retries the retained boot debt.
WriterRetry == phase = "Running" /\ Cleanup("Running")

Next == PgRecover \/ LiveRetry \/ Shutdown \/ Boot \/ BootSettle \/ WriterRetry

Spec == Init /\ [][Next]_vars /\ WF_vars(PgRecover) /\ WF_vars(Boot)
        /\ WF_vars(BootSettle) /\ WF_vars(WriterRetry)

NoBootHeld == phase # "Held"
NoRowLossWithoutReceipt == ~row => receipt = "Committed"
ReceiptEventuallySettled == (phase # "Live") ~> (receipt = "Committed" /\ ~row)
=============================================================================
