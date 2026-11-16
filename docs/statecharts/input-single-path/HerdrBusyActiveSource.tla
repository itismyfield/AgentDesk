---- MODULE HerdrBusyActiveSource ----
\* Active-mailbox refinement of the existing reobserve contract; the native turn never ends.
EXTENDS HerdrBusyReobserve
Absent == [session |-> 0, path |-> 0, dev |-> 0, ino |-> 0]
Old == [session |-> 1, path |-> 1, dev |-> 1, ino |-> 1]
Current == [session |-> 2, path |-> 2, dev |-> 1, ino |-> 2]
Healthy == Scenario \in {"missing", "old"}
ActiveInit == r = [phase |-> "running", historyCurrent |-> Old, registry |-> Old, pin |-> Old,
    inflight |-> IF Scenario = "missing" THEN Absent ELSE Old,
    ownerChannel |-> IF Scenario = "foreign-channel" THEN 2 ELSE 1,
    sourceNonce |-> 1, openedFirstSession |-> 1, knownBusy |-> TRUE,
    changed |-> FALSE, published |-> FALSE, observed |-> FALSE,
    snapshot |-> Absent, request |-> Absent, route |-> "none",
    effects |-> 0, used |-> Absent, proof |-> FALSE]

ChangeSession == /\ r.phase = "running"
                 /\ r' = [r EXCEPT !.phase = "changed", !.changed = TRUE, !.knownBusy = FALSE]
\* Normal hook adoption has already checked the opened first record and file identity.
PublishSource == /\ r.phase = "changed"
                 /\ r' = [r EXCEPT !.phase = "published", !.registry = Current,
                     !.pin = Current, !.historyCurrent = Current, !.published = TRUE]
\* Re-read after publication; an in-place foreign first record leaves the pinned file identity.
InspectTranscript == /\ r.phase = "published"
                     /\ r' = [r EXCEPT !.phase = "ready",
                         !.openedFirstSession = IF Scenario = "foreign-source" THEN 3 ELSE 2,
                         !.knownBusy = Scenario # "unknown"]
PersonInput == /\ r.phase = "ready"
               /\ r' = [r EXCEPT !.phase = "queued"]
ExactCurrent == r.knownBusy /\ r.registry = r.pin /\ r.registry = r.historyCurrent
                /\ r.openedFirstSession = r.pin.session
                /\ r.ownerChannel = 1 /\ r.sourceNonce = 1
\* The old active-turn entry bypasses the shared observation and uses its frozen inflight row.
SelectActive == /\ r.phase = "queued"
                /\ IF Policy = "Before" \/ Mutant = "NoBindingCheck"
                   THEN r' = [r EXCEPT !.phase = "captured", !.request = r.inflight]
                   ELSE IF ExactCurrent \/ Mutant = "UnknownBusy"
                   THEN r' = [r EXCEPT !.phase = "captured", !.observed = TRUE,
                       !.snapshot = r.registry,
                       !.request = IF Mutant = "KeepOldSession" THEN r.inflight ELSE r.registry]
                   ELSE r' = [r EXCEPT !.phase = "held"]
\* A nonempty old file can still be stat'ed; only the verified observation prevents its use.
SubmitActive == /\ r.phase = "captured"
                /\ LET allowed == r.request # Absent /\
                       (~r.observed \/ (r.request = r.snapshot /\
                         (ExactCurrent \/ Mutant = "UnknownBusy")))
                   IN IF allowed
                      THEN r' = [r EXCEPT !.phase = "sent", !.route = "busy",
                                          !.effects = @ + 1, !.used = r.request]
                      ELSE r' = [r EXCEPT !.phase = "held"]
ActiveReceipt == /\ r.phase = "sent" /\ ~r.proof
                 /\ r' = [r EXCEPT !.proof = TRUE]
ActiveSettle == /\ r.phase = "sent" /\ r.proof
                /\ r' = [r EXCEPT !.phase = "done"]
ActiveWait == UNCHANGED vars
ActiveNext == ChangeSession \/ PublishSource \/ InspectTranscript \/ PersonInput \/ SelectActive \/ SubmitActive
              \/ ActiveReceipt \/ ActiveSettle \/ ActiveWait
ActiveFairness == WF_vars(ChangeSession) /\ WF_vars(PublishSource) /\ WF_vars(InspectTranscript) /\ WF_vars(PersonInput)
                  /\ WF_vars(SelectActive) /\ WF_vars(SubmitActive)
                  /\ WF_vars(ActiveReceipt) /\ WF_vars(ActiveSettle)
ActiveSpec == ActiveInit /\ [][ActiveNext]_vars /\ ActiveFairness

ActiveTypeOK == r.phase \in {"running", "changed", "published", "ready", "queued", "captured", "held", "sent", "done"}
                /\ r.effects \in 0..1
ActiveAtMostOnce == r.effects <= 1
NoForeignSourceKey == r.effects > 0 => r.used = r.historyCurrent /\ r.ownerChannel = 1 /\ r.sourceNonce = 1
NoKeyWithoutVerifiedSource == r.effects > 0 => ExactCurrent
ActiveSettledExactlyOnce == r.phase = "done" => r.effects = 1 /\ r.proof
ActiveScenarioReached == <> (r.changed /\ r.published /\ r.phase \notin {"running", "changed", "published", "ready"})
HealthyActiveBusyDelivered == <> (r.phase = "done")
UnknownActiveHeld == <> (r.phase = "held")
=============================================================================
