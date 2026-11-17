---------------------- MODULE ThreadIngressLiveEnd ----------------------
EXTENDS Naturals
CONSTANTS Source, Exact, ExecutionEnded, Mutation
VARIABLES phase, posted, finished, token, inflight, readerWoken, readerOwns
vars == <<phase, posted, finished, token, inflight, readerWoken, readerOwns>>
Eligible == Exact /\ ExecutionEnded
Init == phase = "active" /\ posted = 0 /\ finished = 0
        /\ token = 1 /\ inflight = 1 /\ readerWoken = FALSE /\ readerOwns = FALSE

Inspect ==
    /\ phase = "active"
    /\ phase' = IF ~Eligible THEN "held"
                 ELSE IF Source = "terminal" /\ Mutation # "stealTerminal"
                      THEN "reader"
                 ELSE IF Source = "unknown" /\ Mutation # "ignoreUnknown"
                      THEN "held"
                 ELSE IF Mutation = "omitLiveEnd" THEN "held"
                      ELSE "notice"
    /\ UNCHANGED <<posted, finished, token, inflight, readerWoken, readerOwns>>

\* The existing exact native admission owns a terminal found after execution end.
Reader ==
    /\ phase = "reader"
    /\ phase' = "done" /\ posted' = 1 /\ finished' = 1
    /\ token' = 0 /\ inflight' = 0 /\ readerWoken' = TRUE /\ readerOwns' = TRUE

Notice ==
    /\ phase = "notice"
    /\ phase' = "settle" /\ posted' = 1
    /\ UNCHANGED <<finished, token, inflight, readerWoken, readerOwns>>

Settle ==
    /\ phase = "settle" /\ Mutation # "noSettlement"
    /\ phase' = "settled" /\ finished' = 1 /\ token' = 0 /\ inflight' = 0
    /\ UNCHANGED <<posted, readerWoken, readerOwns>>

Wake ==
    /\ phase = "settled" /\ Mutation # "noWake"
    /\ phase' = "done" /\ readerWoken' = TRUE
    /\ UNCHANGED <<posted, finished, token, inflight, readerOwns>>

Next == Inspect \/ Reader \/ Notice \/ Settle \/ Wake \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars /\ WF_vars(Inspect) /\ WF_vars(Reader)
        /\ WF_vars(Notice) /\ WF_vars(Settle) /\ WF_vars(Wake)
TypeOK == phase \in {"active", "reader", "notice", "settle", "settled", "done", "held"}
          /\ posted \in 0..1 /\ finished \in 0..1 /\ token \in 0..1 /\ inflight \in 0..1
          /\ readerWoken \in BOOLEAN /\ readerOwns \in BOOLEAN
TerminalReaderOnly == Source = "terminal" =>
                      (phase \notin {"notice", "settle", "settled"} /\
                       (finished = 1 => readerOwns))
UnknownHeld == (~Eligible \/ Source = "unknown") =>
                (posted = 0 /\ finished = 0 /\ token = 1 /\ inflight = 1 /\ ~readerWoken)
SettlementAfterDelivery == finished = 1 => posted = 1
DoneNoResidue == phase = "done" =>
                  (finished = 1 /\ posted = 1 /\ token = 0 /\ inflight = 0 /\ readerWoken)
EligibleEnds == <>(IF Eligible /\ Source # "unknown" THEN phase = "done" ELSE phase = "held")
NextCanProgress == <>(IF Eligible /\ Source # "unknown"
                     THEN readerWoken /\ token = 0 /\ inflight = 0
                     ELSE phase = "held" /\ token = 1 /\ inflight = 1)
=============================================================================
