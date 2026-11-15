---- MODULE HerdrColdIdle ----
\* Public Idle proof delegates to the existing driver; pin0 never grants Busy writes.
EXTENDS Naturals
CONSTANTS Policy, Scenario, Mutant
VARIABLE r
vars == <<r>>
Recovery == Scenario \in {"recover", "front", "generation", "wake-nonce", "unknownkeep", "discard"}
Init == r = [phase |-> IF Scenario = "unknownkeep" THEN "unknownKeep" ELSE "queued",
    native |-> IF Recovery \/ Scenario = "unknown" THEN "unknown"
               ELSE IF Scenario = "busy" THEN "busy" ELSE "idle",
    pin |-> FALSE, hooks |-> 0, nonce |-> 1, target |-> IF Scenario = "target" THEN 2 ELSE 1,
    front |-> 1, generation |-> 1, version |-> 1,
    proof |-> ~Recovery /\ Scenario \notin {"busy", "unknown"},
    proofNonce |-> IF Scenario = "nonce" THEN 2 ELSE 1, proofTarget |-> 1,
    proofVersion |-> 1, snapshotVersion |-> 1, captured |-> FALSE,
    observed |-> Scenario = "unknownkeep", published |-> FALSE, changed |-> FALSE,
    reserved |-> FALSE, awake |-> FALSE, watch |-> Scenario = "unknownkeep",
    notice |-> Scenario = "unknownkeep", probed |-> FALSE, wakeIssued |-> FALSE,
    owned |-> Scenario # "unknownkeep", terminal |-> Scenario = "unknownkeep",
    baseEffects |-> IF Scenario = "unknownkeep" THEN 1 ELSE 0,
    effects |-> IF Scenario = "unknownkeep" THEN 1 ELSE 0, receipt |-> FALSE, route |-> "none"]

IdentityExact == r.nonce = 1 /\ r.target = 1 /\ r.front = 1 /\ r.generation = 1
OwnerExact == IdentityExact /\ r.owned /\ ~r.terminal
OwnerAllowed == /\ (r.nonce = 1 \/ Mutant = "IgnoreNonce") /\ r.target = 1
                /\ (r.front = 1 \/ Mutant = "IgnoreFront")
                /\ (r.generation = 1 \/ Mutant = "IgnoreGeneration")
                /\ ((r.owned /\ ~r.terminal) \/ Mutant = "Resurrect")
ProofCurrent == r.proof /\ r.native = "idle" /\ r.proofNonce = r.nonce
                /\ r.proofTarget = r.target /\ r.proofVersion = r.version
ProofAllowed == r.proof /\ r.native = "idle" /\ r.proofNonce = r.nonce
                /\ r.proofTarget = r.target
                /\ (r.proofVersion = r.version \/ Mutant = "IgnoreCurrent")
SnapshotAllowed == r.snapshotVersion = r.version \/ Mutant = "IgnoreCurrent"

Observe == /\ r.phase = "queued"
           /\ r' = [r EXCEPT !.phase = "observed", !.observed = TRUE,
                     !.captured = ProofAllowed, !.snapshotVersion = r.version]
\* The fixture invalidates between observation/reserve or reserve/current, before delegation.
Invalidate == /\ ~r.changed
              /\ ((Scenario = "stale-proof" /\ r.phase = "observed")
                  \/ (Scenario = "stale-reservation" /\ r.phase = "reserved"))
              /\ r' = [r EXCEPT !.changed = TRUE, !.version = 2]
Reserve == /\ r.phase = "observed"
           /\ (Scenario # "stale-proof" \/ r.changed)
           /\ LET allow == (Policy = "Fixed" \/ r.pin) /\ OwnerAllowed
                           /\ r.captured /\ ProofAllowed /\ SnapshotAllowed
              IN r' = [r EXCEPT !.phase = IF allow THEN "reserved" ELSE "held",
                        !.reserved = allow, !.watch = ~allow]
CheckCurrent == /\ r.phase = "reserved"
                /\ (Scenario # "stale-reservation" \/ r.changed)
                /\ IF OwnerAllowed /\ ProofAllowed /\ SnapshotAllowed
                   THEN r' = [r EXCEPT !.phase = "releasing"]
                   ELSE r' = [r EXCEPT !.phase = "held", !.reserved = FALSE, !.watch = TRUE]
\* Classification is pre-key; its reservation is returned before the driver reserves again.
Release == /\ r.phase = "releasing"
           /\ r' = [r EXCEPT !.phase = "delegate", !.reserved = (Mutant = "NoRelease")]
Delegate == /\ r.phase = "delegate"
            /\ r' = [r EXCEPT !.phase = "idle", !.route = "idle"]
Notify == /\ r.phase = "held" /\ ~r.notice
          /\ r' = [r EXCEPT !.notice = TRUE]
Discard == /\ Scenario = "discard" /\ r.phase = "held" /\ ~r.published
           /\ r' = [r EXCEPT !.phase = "discarded", !.owned = FALSE, !.terminal = TRUE]
Publish == /\ Recovery /\ ~r.published
           /\ IF Scenario = "discard" THEN r.phase = "discarded"
              ELSE r.phase \in {"held", "unknownKeep"}
           /\ r' = [r EXCEPT !.published = TRUE, !.native = "idle", !.proof = TRUE,
                     !.front = IF Scenario = "front" THEN 2 ELSE 1,
                     !.generation = IF Scenario = "generation" THEN 2 ELSE 1,
                     !.nonce = IF Scenario = "wake-nonce" THEN 2 ELSE 1,
                     !.proofNonce = IF Scenario = "wake-nonce" THEN 2 ELSE 1]
Reobserve == /\ r.published /\ r.watch /\ ~r.probed
             /\ LET allow == Policy = "Fixed" /\ Mutant # "NoWake" /\ OwnerAllowed /\ ProofAllowed
                IN r' = [r EXCEPT !.probed = TRUE, !.awake = allow, !.wakeIssued = allow]
Reevaluate == /\ r.awake /\ r.phase \in {"held", "unknownKeep", "discarded"}
              /\ OwnerAllowed
              /\ r' = [r EXCEPT !.phase = "queued", !.awake = FALSE,
                        !.owned = TRUE, !.watch = FALSE]
\* Existing Idle driver repeats its own current/first-key check; receipt is a later action.
Write == /\ r.phase = "idle" /\ OwnerExact /\ ProofCurrent /\ ~r.reserved
         /\ r' = [r EXCEPT !.phase = "sent", !.effects = @ + 1]
Refuse == /\ r.phase = "idle" /\ ~(OwnerExact /\ ProofCurrent /\ ~r.reserved)
          /\ r' = [r EXCEPT !.phase = "held", !.watch = TRUE]
Receipt == /\ r.phase = "sent" /\ ~r.receipt
           /\ r' = [r EXCEPT !.receipt = TRUE]
Settle == /\ r.phase = "sent" /\ r.receipt
          /\ r' = [r EXCEPT !.phase = "done"]
Wait == UNCHANGED vars
Next == Observe \/ Invalidate \/ Reserve \/ CheckCurrent \/ Release \/ Delegate
        \/ Notify \/ Discard \/ Publish \/ Reobserve \/ Reevaluate \/ Write \/ Refuse \/ Receipt \/ Settle \/ Wait
Fairness == WF_vars(Observe) /\ WF_vars(Invalidate) /\ WF_vars(Reserve) /\ WF_vars(CheckCurrent)
            /\ WF_vars(Release) /\ WF_vars(Delegate) /\ WF_vars(Notify) /\ WF_vars(Discard)
            /\ WF_vars(Publish) /\ WF_vars(Reobserve) /\ WF_vars(Reevaluate)
            /\ WF_vars(Write) /\ WF_vars(Refuse) /\ WF_vars(Receipt) /\ WF_vars(Settle)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == r.phase \in {"queued", "observed", "reserved", "releasing", "delegate", "held",
                       "idle", "sent", "done", "unknownKeep", "discarded"}
          /\ r.effects \in 0..2 /\ r.version \in 1..2
PinAndHookUnchanged == ~r.pin /\ r.hooks = 0
InputAtMostOnce == r.effects <= 1
SettledExactlyOnce == r.phase = "done" => r.effects = 1 /\ r.receipt
IdleDelegationNeedsCurrentProof == r.route = "idle" => ProofCurrent /\ r.snapshotVersion = r.version
IdleDelegationNeedsSameOwner == r.route = "idle" => OwnerExact
IdleReservationReleased == r.route = "idle" => ~r.reserved
WakeNeedsSameOwner == r.wakeIssued => OwnerExact
NoResurrection == r.terminal => r.effects = r.baseEffects /\ r.phase \in {"unknownKeep", "discarded"}
NoKeyWithoutAuthority == r.effects > r.baseEffects => OwnerExact /\ ProofCurrent
VisibleResolution == <> (r.effects > r.baseEffects \/ r.notice \/ r.terminal)
ScenarioReached == <> (IF Recovery THEN r.published ELSE r.observed /\
                         (Scenario \notin {"stale-proof", "stale-reservation"} \/ r.changed))
HealthyIdleProgress == <> (r.phase = "done")
=============================================================================
