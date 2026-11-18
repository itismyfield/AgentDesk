---------------------------- MODULE CampaignReview ----------------------------
EXTENDS Naturals
CONSTANTS BlockFindings, RequireOutput, RequireScope, FailTimeout, CheckRepair
VARIABLES job
vars == <<job>>

InitJob == [stage |-> "planned", body |-> "none", verdict |-> "none",
            scope |-> FALSE, blocker |-> FALSE, changed |-> FALSE, mode |-> "full", round |-> 0]

Init == job = InitJob

WaitForSlot ==
    /\ job.stage = "planned"
    /\ job' = [job EXCEPT !.stage = "waiting"]

Send ==
    /\ job.stage \in {"planned", "waiting"}
    /\ job' = [job EXCEPT !.stage = "sent"]

ProviderUnavailable ==
    /\ job.stage \in {"planned", "waiting", "sent"}
    /\ job' = [job EXCEPT !.stage = "failed"]

Reply(body) ==
    LET blocker == body = "p1" \/ (job.blocker /\ body # "resolution") IN
    /\ job.stage = "sent"
    /\ job' = [job EXCEPT !.body = body, !.blocker = blocker,
         !.stage = IF RequireOutput /\ body \in {"empty", "refused"} THEN "failed" ELSE "received",
         !.verdict = IF RequireOutput /\ body \in {"empty", "refused"} THEN "none"
                     ELSE IF BlockFindings /\ blocker THEN "issues" ELSE "clean"]

Timeout ==
    /\ FailTimeout /\ job.stage = "sent"
    /\ job' = [job EXCEPT !.stage = "failed"]

VerifyScope ==
    /\ job.stage = "received" /\ ~job.scope
    /\ job' = [job EXCEPT !.scope = TRUE]

Land ==
    /\ job.stage = "received" /\ job.verdict = "clean"
    /\ ~RequireScope \/ job.scope
    /\ job' = [job EXCEPT !.stage = "landed"]

Repair(changed) ==
    /\ job.stage = "received" /\ job.verdict = "issues" /\ job.round = 0
    /\ job' = [InitJob EXCEPT !.round = 1, !.changed = changed, !.blocker = job.blocker,
                       !.mode = IF CheckRepair /\ changed THEN "full" ELSE "delta"]

Next == WaitForSlot \/ Send \/ ProviderUnavailable \/ Timeout \/ VerifyScope \/ Land
        \/ (\E body \in {"p1", "p2", "empty", "refused", "clean", "resolution"} : Reply(body))
        \/ (\E changed \in BOOLEAN : Repair(changed)) \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars /\ WF_vars(Timeout)
NoBlockingLanding == job.stage = "landed" => ~job.blocker
NoEmptyVerdict == job.body \in {"empty", "refused"} => job.verdict = "none"
ScopeBeforeLanding == job.stage = "landed" => job.scope
ChangedContractGetsFull == job.changed => job.mode = "full"
SentEventuallySettles == (job.stage = "sent") ~> (job.stage # "sent")
 =============================================================================
