----------------------------- MODULE CampaignMerge -----------------------------
EXTENDS Naturals
CONSTANTS CheckHead, CheckStop, CheckException, CheckMain, SettleDeadline
VARIABLES job
vars == <<job>>

Init == job = [stage |-> "opened", current |-> 1, checked |-> 0, request |-> 0,
               reviews |-> FALSE, approval |-> FALSE, tlc |-> FALSE, ci |-> "unknown",
               stopped |-> FALSE, wroteAfterStop |-> FALSE, validException |-> TRUE,
               mergeHead |-> 0, mainWitness |-> FALSE]

Observe(reviews, approval, tlc, ci) ==
    /\ job.stage = "opened" /\ job.current = 1
    /\ job' = [job EXCEPT !.checked = job.current, !.reviews = reviews,
               !.approval = approval, !.tlc = tlc, !.ci = ci]

HeadMoves ==
    /\ job.stage = "opened" /\ job.current = 1
    /\ job' = [job EXCEPT !.current = 2]

Stop ==
    /\ ~job.stopped
    /\ job' = [job EXCEPT !.stopped = TRUE]

Merge ==
    /\ job.stage = "opened" /\ job.checked > 0
    /\ job.reviews /\ job.approval /\ job.tlc
    /\ ~CheckStop \/ ~job.stopped
    /\ ~CheckHead \/ job.checked = job.current
    /\ job.ci \in {"pass", "paired"} \/ (~CheckException /\ job.ci = "unpaired")
    /\ job' = [job EXCEPT !.stage = "journaled", !.request = job.checked,
         !.mergeHead = job.current, !.wroteAfterStop = job.stopped,
         !.validException = job.ci # "unpaired"]

Collect ==
    /\ job.stage = "journaled"
    /\ job' = [job EXCEPT !.stage = "waiting_main"]

Confirm(witness) ==
    /\ job.stage = "waiting_main"
    /\ ~CheckMain \/ witness
    /\ job' = [job EXCEPT !.stage = "adopted", !.mainWitness = witness]

Timeout ==
    /\ SettleDeadline /\ job.stage \in {"opened", "waiting_main"}
    /\ job' = [job EXCEPT !.stage = "failed"]

Next == HeadMoves \/ Stop \/ Merge \/ Collect \/ Timeout \/ UNCHANGED vars
      \/ (\E witness \in BOOLEAN : Confirm(witness))
      \/ (\E r, a, t \in BOOLEAN, ci \in {"unknown", "pass", "paired", "unpaired"} :
             Observe(r, a, t, ci))

Spec == Init /\ [][Next]_vars /\ WF_vars(Collect) /\ WF_vars(Timeout)
ExactMergedHead == job.mergeHead > 0 => job.request = job.mergeHead
NoMergeAfterStop == ~job.wroteAfterStop
OnlyPairedException == job.validException
MainNeedsWitness == job.stage = "adopted" => job.mainWitness
OpenedEventuallySettles == (job.stage \in {"opened", "journaled", "waiting_main"})
                        ~> (job.stage \in {"adopted", "failed"})
=============================================================================
