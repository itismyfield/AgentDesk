---- MODULE HerdrBusyReobserve ----
\* Slice2 projection: one unchanged Slice1 operation and one captured owner front.
\* Before leaves the new reobserve wake inactive; direct clear entry is in HerdrBusyInput.
EXTENDS Naturals
CONSTANTS Policy, Entry, Scenario, Mutant
VARIABLE r
vars == <<r>>
Init == r = [phase |-> IF Scenario = "unknownkeep" THEN "unknownKeep" ELSE "queued",
    source |-> IF Scenario \in {"writer", "observer-held"} THEN "ready" ELSE "unknown",
    observer |-> Scenario \notin {"writer", "observer-held", "clear-observer-held"},
    nonce |-> 1, sourceNonce |-> 1, pane |-> 1, front |-> 1, generation |-> 1,
    owned |-> Scenario # "unknownkeep", terminal |-> Scenario = "unknownkeep",
    terminalEffects |-> IF Scenario = "unknownkeep" THEN 1 ELSE 0,
    effects |-> IF Scenario = "unknownkeep" THEN 1 ELSE 0, proof |-> FALSE,
    awake |-> Scenario # "unknownkeep", watch |-> Scenario = "unknownkeep",
    notice |-> Scenario = "unknownkeep", published |-> FALSE, probed |-> FALSE,
    wakeIssued |-> FALSE, route |-> "none", diagnostic |-> FALSE]

IdentityExact == r.nonce = 1 /\ r.pane = 1 /\ r.front = 1 /\ r.generation = 1
SourceExact == r.source = "ready" /\ r.sourceNonce = 1 /\ IdentityExact
IdentityAllowed == /\ (r.nonce = 1 \/ Mutant = "IgnoreNonce")
                   /\ (r.pane = 1 \/ Mutant = "IgnoreTarget")
                   /\ (r.front = 1 \/ Mutant = "IgnoreFront")
                   /\ (r.generation = 1 \/ Mutant = "IgnoreGeneration")
SourceAllowed == r.source = "ready" /\ IdentityAllowed
                 /\ (r.sourceNonce = 1 \/ Mutant \in {"IgnoreNonce", "IgnoreSource"})
ClearAllowed == r.source = "clear" /\ IdentityAllowed
                /\ (r.sourceNonce = 1 \/ Mutant = "IgnoreNonce")
BusyAllowed == SourceAllowed /\ (r.observer \/ Mutant = "NoObserver")

Drive == /\ r.phase = "queued" /\ r.awake
         /\ r' = [r EXCEPT !.awake = FALSE,
             !.diagnostic = (Entry = "early" /\ Policy = "Before"),
             !.phase = IF ClearAllowed /\ Mutant # "BusyClear" THEN "idle"
                       ELSE IF BusyAllowed \/ (ClearAllowed /\ Mutant = "BusyClear")
                       THEN "may" ELSE "held",
             !.route = IF ClearAllowed /\ Mutant # "BusyClear" THEN "idle"
                       ELSE IF BusyAllowed \/ (ClearAllowed /\ Mutant = "BusyClear")
                       THEN "busy" ELSE "none",
             !.watch = ~(ClearAllowed \/ BusyAllowed)]

Notify == /\ r.phase = "held" /\ ~r.notice
          /\ r' = [r EXCEPT !.notice = TRUE]
Discard == /\ Scenario = "discard" /\ r.phase = "held" /\ ~r.published
           /\ r' = [r EXCEPT !.phase = "discarded", !.owned = FALSE,
                     !.terminal = TRUE, !.terminalEffects = r.effects, !.awake = FALSE]

\* Fixture evidence changes; the prior native turn never ends in this model.
Publish == /\ ~r.published
           /\ IF Scenario = "discard" THEN r.phase = "discarded"
              ELSE r.phase \in {"held", "unknownKeep"}
           /\ r' = [r EXCEPT !.published = TRUE,
               !.source = IF Scenario \in {"source", "source-mismatch", "writer", "observer-held"}
                          THEN "ready" ELSE "clear",
               !.sourceNonce = IF Scenario \in {"source-mismatch", "foreign-nonce"} THEN 2 ELSE 1,
               !.nonce = IF Scenario = "foreign-nonce" THEN 2 ELSE 1,
               !.pane = IF Scenario = "foreign-target" THEN 2 ELSE 1,
               !.front = IF Scenario = "foreign-front" THEN 2 ELSE 1,
               !.generation = IF Scenario = "foreign-generation" THEN 2 ELSE 1,
               !.observer = IF Scenario = "writer" THEN TRUE ELSE r.observer]

Reobserve == /\ r.published /\ r.watch /\ ~r.probed
             /\ r' = [r EXCEPT !.probed = TRUE,
                 !.awake = Policy = "Fixed" /\ Mutant # "NoWake"
                           /\ (r.owned \/ Mutant = "Resurrect")
                           /\ (ClearAllowed \/ BusyAllowed),
                 !.wakeIssued = Policy = "Fixed" /\ Mutant # "NoWake"
                                /\ (r.owned \/ Mutant = "Resurrect")
                                /\ (ClearAllowed \/ BusyAllowed)]
Reevaluate == /\ r.awake /\ r.phase \in {"held", "unknownKeep", "discarded"}
              /\ r.owned \/ Mutant = "Resurrect"
              /\ r' = [r EXCEPT !.phase = "queued", !.watch = FALSE,
                        !.owned = (r.owned \/ Mutant = "Resurrect")]

\* Existing Idle readiness and Slice1's per-write target gate remain external contracts.
Write == /\ r.phase \in {"may", "idle"} /\ IdentityExact
         /\ r' = [r EXCEPT !.phase = "sent", !.effects = @ + 1]
Receipt == /\ r.phase = "sent" /\ ~r.proof
           /\ r' = [r EXCEPT !.proof = TRUE]
Settle == /\ r.phase = "sent" /\ r.proof
          /\ r' = [r EXCEPT !.phase = "done"]
Wait == UNCHANGED vars
Next == Drive \/ Notify \/ Discard \/ Publish \/ Reobserve \/ Reevaluate
        \/ Write \/ Receipt \/ Settle \/ Wait
Fairness == WF_vars(Drive) /\ WF_vars(Notify) /\ WF_vars(Discard)
            /\ WF_vars(Publish) /\ WF_vars(Reobserve) /\ WF_vars(Reevaluate)
            /\ WF_vars(Write) /\ WF_vars(Receipt) /\ WF_vars(Settle)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == r.phase \in {"queued", "held", "may", "idle", "sent", "done", "unknownKeep", "discarded"}
          /\ r.effects \in 0..2 /\ r.source \in {"unknown", "ready", "clear"}
AtMostOnce == r.effects <= 1
SettledExactlyOnce == r.phase = "done" => r.effects = 1 /\ r.proof
BusyNeedsExactSourceObserver == r.route = "busy" => SourceExact /\ r.observer
ClearIsNotBusyAuthority == r.source = "clear" => r.route # "busy"
WakeNeedsSameOwner == r.wakeIssued => IdentityExact /\ r.owned /\ ~r.terminal
NoResurrection == r.terminal => r.effects = r.terminalEffects
                               /\ r.phase \in {"unknownKeep", "discarded"}
EarlyUsesCommonObservation == Entry = "early" /\ Policy = "Fixed" => ~r.diagnostic
VisibleResolution == <> (r.effects > 0 \/ r.notice \/ r.phase = "discarded")
ScenarioObserved == <> r.published
HealthyRecoveryDelivers == <> (r.phase = "done")
VerifiedClearReevaluatesIdle == <> (r.route = "idle")
=============================================================================
