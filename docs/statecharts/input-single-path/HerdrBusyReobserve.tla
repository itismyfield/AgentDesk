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

\* Restart/readiness refinement: the same pre-key owner front, no new input or hook needed.
\* Domains and five two-second looks come from 8222-restart-held-readiness.design.md section 2.
RHInit == r = [phase |-> "restart", age |-> 0, visible |-> FALSE,
    nonce |-> 1, native |-> 1, inode |-> 1, front |-> 1, generation |-> 1,
    afterLaunch |-> Scenario \notin {"new-incarnation", "old-reply"},
    pinned |-> Scenario # "legacy", complete |-> Scenario \notin {"missing", "replay"},
    turn |-> IF Scenario = "prompt" THEN "prompt"
             ELSE IF Scenario = "tool" THEN "tool"
             ELSE IF Scenario = "stop-context" THEN "stop-context" ELSE "reply",
    fence |-> Scenario \in {"pending", "compact", "continuation"},
    screen |-> Scenario \notin {"draft", "pane-unreadable"},
    owned |-> TRUE, gate |-> TRUE, proof |-> FALSE, reserved |-> FALSE,
    keys |-> 0, refusals |-> 0, notices |-> 0, requeues |-> 0,
    busyKeys |-> 0, fallbackIdle |-> FALSE, keyValid |-> FALSE,
    changed |-> FALSE, replayed |-> FALSE, reason |-> "none"]
RHIdentity == r.nonce = 1 /\ r.native = 1 /\ r.inode = 1 /\ r.pinned
RHContent == r.complete /\ r.turn = "reply" /\ r.afterLaunch /\ ~r.fence
RHExact == RHIdentity /\ RHContent /\ r.screen
RHAdmit == /\ (r.nonce = 1 \/ Mutant = "SkipNonce")
           /\ (r.native = 1 \/ Mutant = "SkipNative")
           /\ (r.inode = 1 \/ Mutant = "SkipInode") /\ r.pinned
           /\ r.complete /\ (r.turn = "reply" \/ Mutant = "SkipContinuation")
           /\ (r.afterLaunch \/ Mutant = "SkipLaunch")
           /\ (~r.fence \/ Mutant = "SkipContinuation") /\ r.screen
RHOwner == r.owned /\ r.front = 1 /\ r.generation = 1
RHFixed == Policy = "Fixed"
RHEnter == /\ r.phase = "restart"
           /\ r' = [r EXCEPT !.phase = "queued"]
\* General observe has no proof; running fallback reads Unknown and cannot create Idle.
RHBusyEntry == /\ r.phase = "queued"
               /\ r' = [r EXCEPT !.phase = "held"]
\* The first probe also reads Unknown. The fixture then exposes its existing source evidence.
RHFirstKeep == /\ r.phase = "held" /\ ~r.visible
               /\ r' = [r EXCEPT !.age = 1, !.visible = TRUE,
                   !.nonce = IF Scenario = "nonce" THEN 2 ELSE 1,
                   !.native = IF Scenario = "native" THEN 2 ELSE 1,
                   !.inode = IF Scenario = "inode" THEN 2 ELSE 1]
\* Same captured probe: restore only at the evidence boundary, and wake the Idle driver.
RHRestoreWake == /\ RHFixed /\ r.phase = "held" /\ r.visible /\ RHOwner /\ RHAdmit
                 /\ r.age < 5
                 /\ r' = [r EXCEPT !.phase = "waking", !.proof = TRUE]
RHKeep == /\ r.phase = "held" /\ r.visible
          /\ ~(RHFixed /\ RHOwner /\ RHAdmit /\ r.age < 5)
          /\ r' = [r EXCEPT !.age = IF Mutant = "ResetDeadline" THEN 1
                                  ELSE IF r.age < 5 THEN r.age + 1 ELSE 5]
\* No running fallback change and no key permission are carried by the wake.
RHIdleReserve == /\ r.phase = "waking"
                 /\ IF (RHOwner /\ r.gate /\ r.proof /\ RHExact)
                        \/ Mutant = "SkipDriverRecheck"
                    THEN r' = [r EXCEPT !.phase = "reserved", !.reserved = TRUE]
                    ELSE r' = [r EXCEPT !.phase = "held", !.proof = FALSE,
                        !.age = IF r.age < 5 THEN r.age + 1 ELSE 5]
RHWrite == /\ r.phase = "reserved" /\ r.reserved /\ RHOwner /\ r.gate
           /\ ((RHExact /\ r.proof) \/ Mutant = "SkipDriverRecheck")
           /\ r' = [r EXCEPT !.phase = "done", !.keys = @ + 1,
               !.keyValid = (RHExact /\ r.proof /\ RHOwner /\ r.gate)]
\* At most one source swap between wake and reservation. The deadline never starts again.
RHRace == /\ Scenario = "race" /\ r.phase = "waking" /\ ~r.changed
          /\ r' = [r EXCEPT !.native = 2, !.proof = FALSE, !.changed = TRUE]
\* This is an owner-only atomic pre-key refusal, not a fabricated NotSentProof or attempt.
RHExpire == /\ RHFixed /\ Mutant # "NoDeadline" /\ r.phase = "held" /\ r.age = 5
            /\ RHOwner /\ r.gate /\ ~r.reserved
            /\ r' = [r EXCEPT !.phase = "refused", !.refusals = 1,
                !.owned = FALSE, !.reason = Scenario]
RHNotice == /\ r.phase = "refused" /\ r.notices = 0
            /\ r' = [r EXCEPT !.phase = "explained", !.notices = 1]
\* Cancel/clear/Terminate/stop win the same owner gate before a deadline callback can mutate it.
RHLifecycle == /\ Scenario \in {"cancel", "clear", "terminate", "stop"}
               /\ r.phase \in {"held", "waking", "reserved"} /\ r.visible
               /\ r' = [r EXCEPT !.phase = "refused", !.refusals = 1,
                   !.owned = FALSE, !.gate = FALSE, !.reason = Scenario]
\* A restart preserves the deadline for this original; explained refusals replay only notices.
RHReplay == /\ Scenario = "replay" /\ ~r.replayed /\ r.phase \in {"held", "explained"}
            /\ r' = [r EXCEPT !.replayed = TRUE]
RHBusyWrite == /\ Mutant = "BusyDirect" /\ r.phase = "waking"
               /\ r' = [r EXCEPT !.phase = "done", !.keys = 1,
                   !.busyKeys = 1, !.keyValid = RHExact]
RHFallback == /\ Mutant = "FallbackIdle" /\ r.phase = "held" /\ ~r.fallbackIdle
              /\ r' = [r EXCEPT !.fallbackIdle = TRUE]
RHRepeatNotice == /\ Mutant = "RepeatNotice" /\ r.phase = "explained" /\ r.notices = 1
                  /\ r' = [r EXCEPT !.notices = 2]
RHRequeue == /\ Mutant = "RequeueBody" /\ r.phase = "explained" /\ r.requeues = 0
             /\ r' = [r EXCEPT !.requeues = 1]
RHTick == /\ r.phase = "held" /\ r.visible /\ r.age < 5
          /\ ~(RHFixed /\ RHExact) /\ Mutant # "ResetDeadline"
          /\ r' = [r EXCEPT !.age = @ + 1]
RHWait == UNCHANGED vars
RHNext == RHEnter \/ RHBusyEntry \/ RHFirstKeep \/ RHRestoreWake \/ RHKeep
          \/ RHIdleReserve \/ RHWrite \/ RHRace \/ RHExpire \/ RHNotice
          \/ RHLifecycle \/ RHReplay \/ RHBusyWrite \/ RHFallback \/ RHRequeue \/ RHRepeatNotice \/ RHTick \/ RHWait
RHSpec == RHInit /\ [][RHNext]_vars
          /\ WF_vars(RHEnter) /\ WF_vars(RHBusyEntry) /\ WF_vars(RHFirstKeep)
          /\ WF_vars(RHRestoreWake) /\ WF_vars(RHKeep) /\ WF_vars(RHIdleReserve)
          /\ WF_vars(RHWrite) /\ WF_vars(RHExpire) /\ WF_vars(RHNotice)
          /\ WF_vars(RHLifecycle) /\ WF_vars(RHReplay) /\ WF_vars(RHTick)
RHTypeOK == /\ r.phase \in {"restart", "queued", "held", "waking", "reserved",
                           "done", "refused", "explained"}
            /\ r.age \in 0..5 /\ r.nonce \in 1..2 /\ r.native \in 1..2 /\ r.inode \in 1..2
            /\ r.keys \in 0..1 /\ r.refusals \in 0..1 /\ r.notices \in 0..1
RHOneDisposition == r.keys + r.refusals <= 1 /\ r.notices <= r.refusals
                    /\ (r.phase = "done" => r.keys = 1 /\ r.refusals = 0)
                    /\ (r.phase = "explained" => r.refusals = 1 /\ r.notices = 1
                                                 /\ r.reason # "none")
RHNoRequeue == r.requeues = 0
RHNoBusyDirect == r.busyKeys = 0
RHRunningContract == ~r.fallbackIdle
RHProofNeedsEvidence == r.proof => RHExact
RHKeyNeedsRecheck == r.keys = 0 \/ r.keyValid
RHReachedKeep == <>(r.phase = "held" /\ r.age = 1)
RHNoInfiniteKeep == (r.phase = "held") ~> (r.phase \in {"done", "explained"})
RHEndExactlyOnce == <>((r.keys = 1 /\ r.refusals = 0) \/ (r.keys = 0 /\ r.refusals = 1 /\ r.notices = 1))
RHHealthyDelivers == <>(r.phase = "done")
=============================================================================
