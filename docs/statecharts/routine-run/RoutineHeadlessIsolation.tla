----------------------- MODULE RoutineHeadlessIsolation -----------------------
EXTENDS Naturals
CONSTANT Policy

VARIABLE s
vars == <<s>>
Terminal == {"succeeded", "failed", "interrupted"}

PrimaryAt(cleared, present) ==
    [session |-> "primary", history |-> 1, uploads |-> 1, boundary |-> 0,
     binding |-> "primary", memory |-> 0, cleared |-> cleared, present |-> present]

Init == \E cleared, present \in BOOLEAN, evidence \in {"ok", "missing", "unreadable"}:
    s = [run |-> "claimed", evidence |-> evidence,
         primary |-> PrimaryAt(cleared, present), primary0 |-> PrimaryAt(cleared, present),
         pin |-> "none", key |-> "none", rows |-> {}, definition |-> "headless",
         turn |-> "none", transcript |-> FALSE, executions |-> 0,
         retried |-> FALSE, restarted |-> FALSE, recovered |-> FALSE]

SharedClear(p) == [p EXCEPT !.session = "none", !.boundary = @ + 1]
SharedTake(p) == [p EXCEPT !.uploads = 0, !.cleared = FALSE, !.present = TRUE]
SharedStart(p) == [SharedTake(SharedClear(p)) EXCEPT !.binding = "run"]
Writeback(p) == [p EXCEPT !.session = "run", !.history = @ + 1]

\* Completers and recovery read the pin stored on the turn row, not the current definition.
Isolated == IF Policy = "repin" THEN s.definition = "headless" ELSE s.pin = "per_run"

Start ==
    /\ s.run = "claimed"
    /\ s.evidence = "ok" \/ Policy \in {"shared_path", "primary_key_fallback"}
    /\ LET fallback == s.evidence # "ok"
           key == IF fallback THEN "primary" ELSE "per_run"
       IN s' = [s EXCEPT !.run = "started", !.turn = "running", !.executions = @ + 1,
                         !.pin = IF Policy = "shared_path" \/ fallback THEN "shared" ELSE "per_run",
                         !.key = key, !.rows = @ \cup {key},
                         !.primary = CASE Policy = "shared_path" \/ fallback -> SharedStart(@)
                                       [] Policy = "shared_clear" -> SharedClear(@)
                                       [] Policy = "shared_take" -> SharedTake(@)
                                       [] OTHER -> @]

Refuse ==
    /\ s.run = "claimed" /\ s.evidence # "ok"
    /\ Policy \notin {"shared_path", "primary_key_fallback"}
    /\ s' = [s EXCEPT !.run = "failed"]

ProviderRetry ==
    /\ s.run = "started" /\ s.turn = "running" /\ ~s.retried
    /\ s' = [s EXCEPT !.retried = TRUE,
                      !.primary = IF Policy = "retry_clear" \/ ~Isolated
                                  THEN [@ EXCEPT !.session = "none"] ELSE @]

Answer ==
    /\ s.run = "started" /\ s.turn = "running"
    /\ s' = [s EXCEPT !.turn = "answered"]

Complete ==
    /\ s.run = "started" /\ s.turn = "answered"
    /\ LET planned == IF Isolated /\ Policy # "plan_reads_primary"
                      THEN TRUE ELSE s.primary.present /\ ~s.primary.cleared
       IN s' = [s EXCEPT !.turn = "finalized", !.transcript = planned,
                         !.primary = CASE ~Isolated \/ Policy = "shared_writeback" -> Writeback(@)
                                       [] Policy = "memory_effect" -> [@ EXCEPT !.memory = @ + 1]
                                       [] OTHER -> @]

\* Without its transcript the run has no completion evidence and ends at its deadline.
Settle ==
    /\ s.run = "started" /\ s.turn = "finalized"
    /\ s' = [s EXCEPT !.run = IF s.transcript THEN "succeeded" ELSE "failed"]

Restart ==
    /\ s.run = "started" /\ s.turn \in {"running", "answered"} /\ ~s.restarted
    /\ s' = [s EXCEPT !.restarted = TRUE, !.turn = "lost"]

Recover ==
    /\ s.run = "started" /\ s.restarted /\ ~s.recovered
    /\ s' = [s EXCEPT !.recovered = TRUE, !.run = "interrupted",
                      !.executions = IF Policy = "rerun_started" THEN @ + 1 ELSE @,
                      !.primary = IF Policy = "recovery_seed" \/ ~Isolated
                                  THEN [@ EXCEPT !.session = "run"] ELSE @]

PatchDefinition ==
    /\ s.run = "started" /\ s.definition = "headless"
    /\ s' = [s EXCEPT !.definition = "thread"]

Done == s.run \in Terminal /\ UNCHANGED s

Next == Start \/ Refuse \/ ProviderRetry \/ Answer \/ Complete \/ Settle \/ Restart \/ Recover
        \/ PatchDefinition \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Start) /\ WF_vars(Refuse) /\ WF_vars(Answer)
        /\ WF_vars(Complete) /\ WF_vars(Settle) /\ WF_vars(Recover)

PrimaryUnchanged == s.primary = s.primary0
PerRunKeyOnly == s.rows \subseteq {"per_run"}
NoPrimaryFallback == s.evidence # "ok" => s.executions = 0
IsolationPinned == s.run = "started" => s.pin = "per_run"
NoRerunAfterRestart == s.executions <= 1
EveryRunTerminates == <>(s.run \in Terminal)
FinalizedRunSucceeds == [](s.turn = "finalized" => <>(s.run = "succeeded"))
=============================================================================
