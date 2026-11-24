-------------------- MODULE NativeReceiptRestart --------------------
\* An origin-carrying native execution whose terminal receipt meets a restart.
\* Shutdown may mark the row DrainRestart before or after its native result commits the receipt.
EXTENDS Naturals
CONSTANTS Mode   \* "Before": shutdown marks the stamped row and boot fails on a pending receipt.
                 \* "MarkerStrict": the native CAS refuses every restart marker, even after the receipt commits.

VARIABLES pg, receipt, row, marker, phase
vars == <<pg, receipt, row, marker, phase>>

\* The execution either has no native result yet or already stamped a failed receipt write.
Init == /\ pg = "Down" /\ receipt \in {"None", "Stamped"} /\ row = TRUE /\ marker = FALSE
        /\ phase = "Live"

\* A marker written by this generation always holds the row; a prior-generation DrainRestart
\* passes the native CAS once the receipt committed.
CasPasses == ~marker \/ (Mode = "Fixed" /\ phase \in {"Booting", "Running"})

\* One cleanup attempt: commit the receipt, then the native snapshot CAS removes the row.
Cleanup(next) == /\ row /\ pg = "Up"
                 /\ receipt' = "Committed"
                 /\ row' = ~CasPasses
                 /\ phase' = next
                 /\ UNCHANGED <<pg, marker>>

PgRecover == /\ pg = "Down" /\ pg' = "Up" /\ UNCHANGED <<receipt, row, marker, phase>>

\* The native result arrives: commit the receipt or stamp the row when PG is down.
Complete == /\ receipt = "None" /\ row /\ phase \in {"Live", "Stopped", "Running"}
            /\ IF pg = "Up" THEN Cleanup(phase)
               ELSE receipt' = "Stamped" /\ UNCHANGED <<pg, row, marker, phase>>

LiveRetry == phase = "Live" /\ receipt # "None" /\ Cleanup("Live")

\* Graceful shutdown marks live rows DrainRestart; a stamped row ended and is left as it is.
Shutdown == /\ phase = "Live" /\ phase' = "Stopped"
            /\ marker' = (marker \/ (row /\ (Mode = "Before" \/ receipt # "Stamped")))
            /\ UNCHANGED <<pg, receipt, row>>

Boot == /\ phase \in {"Stopped", "Held"} /\ phase' = "Booting"
        /\ UNCHANGED <<pg, receipt, row, marker>>

\* Native boot settlement runs before the runtime starts; any refusal holds the whole runtime.
BootSettle == /\ phase = "Booting"
              /\ \/ ((~row \/ receipt = "None") /\ phase' = "Running"
                     /\ UNCHANGED <<pg, receipt, row, marker>>)
                 \/ (row /\ receipt # "None" /\ pg = "Down" /\ UNCHANGED <<pg, receipt, row, marker>>
                     /\ phase' = IF Mode = "Before" THEN "Held" ELSE "Running")
                 \/ (row /\ receipt # "None" /\ pg = "Up" /\ ~CasPasses /\ receipt' = "Committed"
                     /\ phase' = "Held" /\ UNCHANGED <<pg, row, marker>>)
                 \/ (row /\ receipt # "None" /\ pg = "Up" /\ CasPasses /\ Cleanup("Running"))

\* The running writer retries the retained boot debt.
WriterRetry == phase = "Running" /\ receipt # "None" /\ Cleanup("Running")

Next == PgRecover \/ Complete \/ LiveRetry \/ Shutdown \/ Boot \/ BootSettle \/ WriterRetry

Spec == Init /\ [][Next]_vars /\ WF_vars(PgRecover) /\ WF_vars(Complete) /\ WF_vars(Boot)
        /\ WF_vars(BootSettle) /\ WF_vars(WriterRetry)

NoBootHeld == phase # "Held"
NoRowLossWithoutReceipt == ~row => receipt = "Committed"
ReceiptEventuallySettled == (phase # "Live") ~> (receipt = "Committed" /\ ~row)
=============================================================================
