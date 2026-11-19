-------------------------- MODULE CampaignDeployment --------------------------
EXTENDS Naturals
CONSTANTS RequireWitness, RequireFresh, RequireAllHosts, SettleDeadline
VARIABLES job
vars == <<job>>

Init == job = [stage |-> "planned", worker |-> 0, sampledBy |-> 2,
               compiled |-> "unknown", reported |-> "none", targetCount |-> 0,
               newBoot |-> FALSE, matched |-> FALSE]

Start ==
    /\ job.stage = "planned"
    /\ job' = [job EXCEPT !.stage = "observing"]

Produce(source, clean) ==
    /\ job.stage = "observing"
    /\ job' = [job EXCEPT !.compiled = IF clean THEN source ELSE "unknown",
              !.newBoot = TRUE]

Sample(count) ==
    /\ job.stage = "observing"
    /\ job' = [job EXCEPT !.sampledBy = job.worker, !.reported = job.compiled,
              !.targetCount = count, !.matched = job.newBoot]

Restart ==
    /\ job.stage = "observing" /\ job.worker = 0
    /\ job' = [job EXCEPT !.worker = 1]

Confirm ==
    /\ job.stage = "observing" /\ job.sampledBy # 2
    /\ job.targetCount > 0
    /\ ~RequireWitness \/ (job.reported = "expected" /\ job.matched)
    /\ ~RequireFresh \/ job.sampledBy = job.worker
    /\ ~RequireAllHosts \/ job.targetCount = 2
    /\ job' = [job EXCEPT !.stage = "confirmed"]

Expire ==
    /\ SettleDeadline /\ job.stage = "observing"
    /\ job' = [job EXCEPT !.stage = "failed"]

Next == Start \/ Restart \/ Confirm \/ Expire \/ UNCHANGED vars
        \/ (\E source \in {"expected", "other"}, clean \in BOOLEAN : Produce(source, clean))
        \/ (\E count \in 0..2 : Sample(count))

Spec == Init /\ [][Next]_vars /\ WF_vars(Expire)
OnlyWitnessedSource == job.stage = "confirmed" => job.reported = "expected" /\ job.matched
NoPreRestartSample == job.stage = "confirmed" => job.sampledBy = job.worker
EveryHostWitnessed == job.stage = "confirmed" => job.targetCount = 2
ObservationSettles == (job.stage = "observing") ~> (job.stage \in {"confirmed", "failed"})
=============================================================================
