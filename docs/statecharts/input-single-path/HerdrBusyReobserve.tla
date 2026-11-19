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
\* `elapsed` is real time since the first typed hold; `age` is what the stored due allows.
RHScreenHold == Scenario \in {"tmux-modal-held", "tmux-draft-held"}
RHTmux == Scenario \in {"tmux-meta", "tmux-meta-reboot", "tmux-retry-due", "tmux-late-follower"}
          \/ RHScreenHold
RHInit == r = [phase |-> IF RHTmux THEN "queued" ELSE "restart",
    age |-> 0, elapsed |-> 0, visible |-> FALSE, retried |-> FALSE,
    nonce |-> 1, native |-> 1, inode |-> 1, front |-> 1, generation |-> 1,
    afterLaunch |-> Scenario \notin {"new-incarnation", "old-reply"},
    pinned |-> Scenario # "legacy", complete |-> ~RHTmux /\ Scenario \notin {"missing", "replay"},
    turn |-> IF Scenario = "prompt" THEN "prompt"
             ELSE IF Scenario = "tool" THEN "tool"
             ELSE IF Scenario = "stop-context" THEN "stop-context" ELSE "reply",
    fence |-> Scenario \in {"pending", "compact", "continuation"},
    screen |-> ~RHScreenHold /\ Scenario \notin {"draft", "pane-unreadable"},
    owned |-> TRUE, gate |-> TRUE, proof |-> FALSE, reserved |-> FALSE, permit |-> FALSE,
    keys |-> 0, refusals |-> 0, notices |-> 0, requeues |-> 0, leafRefused |-> FALSE,
    records |-> 0, recordKey |-> 0, pendingAck |-> FALSE, epoch |-> 0,
    dueStored |-> FALSE, armed |-> FALSE, other |-> "none", cbFired |-> FALSE,
    busyKeys |-> 0, fallbackIdle |-> FALSE, keyValid |-> FALSE,
    changed |-> FALSE, replayed |-> FALSE, reason |-> "none"]
Inc5(x) == IF x < 5 THEN x + 1 ELSE 5
Inc10(x) == IF x < 10 THEN x + 1 ELSE 10
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
\* The identity a deadline callback captured (front 1, generation 1) against the current front.
RHSameFront == /\ (r.front = 1 \/ Mutant = "IgnoreFront")
               /\ (r.generation = 1 \/ Mutant = "IgnoreGeneration")
\* "Unwired" is S1 restoration and the owner deadline without the Busy-held entry wiring.
RHFixed == Policy \in {"Fixed", "Unwired"}
RHWired == Policy = "Fixed"
RHUnstarted == r.phase \in {"held", "waking", "reserved"}
RHReason == IF RHExact THEN "deadline" ELSE Scenario
\* Healthy-delivery premise only: driver steps run before the clock while they are enabled.
RHPromptDriver == Entry = "prompt-driver"
                  /\ (r.phase \in {"waking", "reserved"}
                      \/ (r.phase = "held" /\ RHFixed /\ RHOwner /\ RHAdmit))
RHEnter == /\ r.phase = "restart"
           /\ r' = [r EXCEPT !.phase = "queued"]
\* General observe has no proof; running fallback reads Unknown and cannot create Idle.
RHBusyEntry == /\ r.phase = "queued" /\ ~RHTmux /\ Scenario # "no-holder-live"
               /\ r' = [r EXCEPT !.phase = "held"]
\* A live pane with no active turn reaches the missing-recovery refusal before any watch.
RHRefuseUnheld == /\ r.phase = "queued" /\ Scenario = "no-holder-live"
                  /\ r' = [r EXCEPT !.phase = "refused", !.refusals = 1,
                      !.owned = FALSE, !.reason = Scenario]
\* The first probe also reads Unknown. The fixture then exposes its existing source evidence.
\* The wired Busy-held entry records the owner hold (a stored due survives; a hold without one gets
\* it once here) and arms the drive's deadline timer, which a restart does not keep.
RHFirstKeep == /\ r.phase = "held" /\ ~r.visible /\ ~RHTmux
               /\ r' = [r EXCEPT !.visible = TRUE,
                   !.dueStored = r.dueStored \/ (RHWired /\ Mutant # "NoWiring"),
                   !.armed = r.armed \/ (RHWired /\ Mutant # "NoArm"),
                   !.age = IF r.dueStored /\ r.age > 0 THEN r.age ELSE 1,
                   !.elapsed = IF r.age = 0 THEN Inc10(r.elapsed) ELSE r.elapsed,
                   !.nonce = IF Scenario = "nonce" THEN 2 ELSE 1,
                   !.native = IF Scenario = "native" THEN 2 ELSE 1,
                   !.inode = IF Scenario = "inode" THEN 2 ELSE 1]
\* An owned tmux idle start reserves before its ordinary readiness wait. The metadata-only
\* source cannot grant a first key. Its exact NotSent closes the attempt and keeps the original.
RHTmuxReserve == /\ RHTmux /\ r.phase = "queued"
                 /\ r' = [r EXCEPT !.phase = "reserved", !.reserved = TRUE]
RHTmuxNotSent == /\ RHTmux /\ r.phase = "reserved" /\ r.keys = 0 /\ ~r.permit /\ ~r.proof
                 /\ (~r.dueStored \/ r.age < 5)
                 /\ r' = [r EXCEPT !.phase = "held", !.reserved = FALSE,
                     !.dueStored = r.dueStored \/ (RHWired /\ Mutant # "NoWiring"
                         /\ (r.screen \/ Mutant = "NoScreenGuard"))]
\* Hold and due are one actor commit. The drive re-arms the timer even if the pane never wakes.
RHTmuxLook == /\ RHTmux /\ r.phase = "held" /\ ~r.visible
              /\ ~(r.replayed /\ r.dueStored /\ r.age = 5)
              /\ r' = [r EXCEPT !.visible = TRUE,
                  !.armed = r.dueStored /\ Mutant # "NoArm",
                  !.age = IF r.age = 0 THEN 1 ELSE r.age,
                  !.elapsed = IF r.age = 0 THEN Inc10(r.elapsed) ELSE r.elapsed]
\* One existing external wake can retry the same front, with its first due unchanged.
RHTmuxRetry == /\ RHTmux /\ r.phase = "held" /\ r.visible /\ ~r.retried
               /\ (~r.dueStored \/ r.age < 5)
               /\ ~RHScreenHold
               /\ (Scenario # "tmux-retry-due" \/ r.age = 4)
               /\ r' = [r EXCEPT !.phase = "reserved", !.reserved = TRUE, !.retried = TRUE]
\* The existing watch retries after both focus and source are ready.
RHReleaseScreen == /\ RHScreenHold /\ r.phase = "held" /\ ~r.screen /\ r.age = 5
                   /\ r' = [r EXCEPT !.screen = TRUE, !.complete = TRUE]
\* B commits after A's hold and before ingress can issue its wake.
RHLateArrival == /\ Scenario = "tmux-late-follower" /\ r.phase = "held"
                 /\ r.visible /\ r.other = "none" /\ r.age = 4
                 /\ r' = [r EXCEPT !.other = "queued"]
RHLateDisposition == IF Scenario = "tmux-late-follower" /\ Mutant = "NoMemberBoundary"
                     /\ r.other = "queued" THEN "refused" ELSE r.other
\* Without the inherited due, the reserved warm wait spends another preparation window.
RHOverdueRetry == /\ Scenario = "tmux-retry-due" /\ Mutant = "NoInheritedDue"
                  /\ r.phase = "reserved" /\ r.retried /\ r.age = 5 /\ r.elapsed = 5
                  /\ r' = [r EXCEPT !.elapsed = 6]
\* Same captured probe: restore only at the evidence boundary, and wake the Idle driver.
RHRestoreWake == /\ RHFixed /\ r.phase = "held" /\ r.visible /\ RHOwner /\ RHAdmit
                 /\ (r.age < 5 \/ ~r.dueStored)
                 /\ r' = [r EXCEPT !.phase = "waking", !.proof = TRUE]
RHKeep == /\ r.phase = "held" /\ r.visible
          /\ ~(RHFixed /\ RHOwner /\ RHAdmit /\ (r.age < 5 \/ ~r.dueStored))
          /\ r' = [r EXCEPT !.age = IF Mutant = "ResetDeadline" THEN 1 ELSE Inc5(r.age),
                            !.elapsed = IF r.age < 5 THEN Inc10(r.elapsed) ELSE r.elapsed]
\* The clock runs in every unstarted phase, Ready or not; a wake or reserve never stops it.
RHTick == /\ RHUnstarted /\ r.visible /\ r.dueStored /\ r.age < 5
          /\ Mutant # "ResetDeadline" /\ ~RHPromptDriver
          /\ r' = [r EXCEPT !.age = @ + 1, !.elapsed = Inc10(@)]
\* No running fallback change and no key permission are carried by the wake.
RHIdleReserve == /\ r.phase = "waking"
                 /\ IF (RHOwner /\ r.gate /\ r.proof /\ RHExact)
                        \/ Mutant = "SkipDriverRecheck"
                    THEN r' = [r EXCEPT !.phase = "reserved", !.reserved = TRUE]
                    ELSE r' = [r EXCEPT !.phase = "held", !.proof = FALSE]
\* The leaf's current/first-key check before due hands a writer its permit; the key may lag.
RHArm == /\ r.phase = "reserved" /\ ~r.permit /\ r.keys = 0 /\ (r.age < 5 \/ ~r.dueStored)
         /\ RHOwner /\ r.gate
         /\ ((RHExact /\ r.proof) \/ Mutant = "SkipDriverRecheck")
         /\ r' = [r EXCEPT !.permit = TRUE,
             !.keyValid = (RHExact /\ r.proof /\ RHOwner /\ r.gate)]
\* A failed leaf recheck is NotSent: the original returns to its hold with the same due.
RHRecheckFail == /\ ~RHTmux /\ r.phase = "reserved" /\ ~r.permit /\ Mutant # "SkipDriverRecheck"
                 /\ ~(RHExact /\ r.proof)
                 /\ r' = [r EXCEPT !.phase = "held", !.reserved = FALSE, !.proof = FALSE]
\* First-key commit through the writer gate; a refusal that closed the gate leaves it 0.
RHWrite == /\ r.permit /\ r.keys = 0 /\ (r.gate \/ Mutant = "NoFirstKeyGate")
           /\ r' = [r EXCEPT !.keys = 1, !.permit = FALSE,
               !.phase = IF r.phase = "reserved" THEN "done" ELSE r.phase]
\* At most one source swap after wake and before a permit. The deadline never starts again.
RHRace == /\ Scenario = "race" /\ r.phase \in {"waking", "reserved"} /\ ~r.permit /\ ~r.changed
          /\ r' = [r EXCEPT !.native = 2, !.proof = FALSE, !.changed = TRUE]
\* Owner-only atomic pre-key refusal of an attemptless front, not a fabricated NotSentProof.
\* It revokes the front's grant/claim; it cannot revoke a reserved leaf's writer gate.
RHExpire == /\ RHFixed /\ Mutant # "NoDeadline" /\ r.dueStored /\ r.age = 5
            /\ (r.armed \/ (RHTmux /\ r.replayed /\ ~r.visible))
            /\ (r.phase \in {"held", "waking"} \/ (Mutant = "AttemptlessLeaf" /\ r.phase = "reserved"))
            /\ r.owned /\ RHSameFront /\ r.gate /\ r.keys = 0
            /\ r' = [r EXCEPT !.phase = "refused", !.refusals = @ + 1, !.owned = FALSE,
                !.gate = r.phase = "reserved", !.reserved = FALSE, !.reason = RHReason,
                !.other = RHLateDisposition]
\* A reserved attempt at due settles through its own NotSent and closes its writer gate.
RHLeafExpire == /\ RHFixed /\ Mutant \notin {"NoDeadline", "NoReservedExpire"}
                /\ r.phase = "reserved" /\ r.dueStored /\ r.age = 5 /\ r.keys = 0 /\ r.gate
                /\ ~(Scenario = "tmux-retry-due" /\ Mutant = "NoInheritedDue" /\ r.retried)
                /\ r' = [r EXCEPT !.phase = "refused", !.refusals = @ + 1, !.owned = FALSE,
                    !.gate = FALSE, !.leafRefused = TRUE, !.reason = RHReason,
                    !.other = RHLateDisposition]
NoticeKey == IF Mutant = "EpochNoticeKey" THEN r.epoch ELSE 0
\* The outbox keeps one record per key; the durable ack may be lost to a crash after enqueue.
RHNoticeEnqueue == /\ r.phase = "refused" /\ ~r.pendingAck
                   /\ r' = [r EXCEPT !.pendingAck = TRUE, !.recordKey = NoticeKey,
                       !.records = IF r.records > 0 /\ r.recordKey = NoticeKey
                                   THEN r.records ELSE r.records + 1]
RHNoticeAck == /\ r.phase = "refused" /\ r.pendingAck
               /\ r' = [r EXCEPT !.phase = "explained", !.notices = 1, !.pendingAck = FALSE]
\* Cancel/clear/Terminate/stop win the same owner gate before a deadline callback can mutate it.
RHLifecycle == /\ Scenario \in {"cancel", "clear", "terminate", "stop"}
               /\ RHUnstarted /\ r.visible /\ r.keys = 0
               /\ r' = [r EXCEPT !.phase = "refused", !.refusals = 1,
                   !.owned = FALSE, !.gate = FALSE, !.reason = Scenario]
\* The original leaves by an existing disposition; a new front (or generation) takes its place.
RHSwap == /\ Scenario \in {"stale-front", "stale-generation"} /\ r.other = "none"
          /\ r.phase \in {"held", "waking"} /\ r.visible /\ r.keys = 0
          /\ r' = [r EXCEPT !.phase = "refused", !.refusals = 1, !.owned = FALSE,
              !.gate = FALSE, !.reason = "clear", !.other = "queued",
              !.front = IF Scenario = "stale-front" THEN 2 ELSE 1,
              !.generation = IF Scenario = "stale-generation" THEN 2 ELSE 1]
\* The old original's deadline callback arrives at the new front and must be a no-op.
RHStaleCallback == /\ Scenario \in {"stale-front", "stale-generation"}
                   /\ r.other = "queued" /\ ~r.cbFired
                   /\ r' = [r EXCEPT !.cbFired = TRUE,
                       !.other = IF RHSameFront THEN "refused" ELSE "queued"]
\* One restart: durable hold/due/refusal/notice intent survive; proof, watch and drain do not.
\* An active reserved attempt is the existing possibly-sent recovery's and is not modeled here.
RHReboot == /\ Scenario \in {"replay", "reboot", "old-snapshot", "tmux-meta-reboot"} /\ ~r.replayed /\ ~r.permit
            /\ r.phase \in {"held", "waking", "refused", "explained"}
            /\ r' = [r EXCEPT !.replayed = TRUE, !.epoch = 1, !.proof = FALSE,
                !.pendingAck = FALSE, !.armed = FALSE,
                !.visible = IF r.phase \in {"held", "waking"} THEN FALSE ELSE r.visible,
                !.phase = IF r.phase = "waking" THEN "held" ELSE r.phase,
                !.age = IF Mutant = "RebootResetDue" /\ r.phase \in {"held", "waking"}
                        THEN 1 ELSE r.age,
                !.dueStored = r.dueStored /\ Mutant # "RegrantMissing"]
RHBusyWrite == /\ Mutant = "BusyDirect" /\ r.phase = "waking"
               /\ r' = [r EXCEPT !.phase = "done", !.keys = 1,
                   !.busyKeys = 1, !.keyValid = RHExact]
RHFallback == /\ Mutant = "FallbackIdle" /\ r.phase = "held" /\ ~r.fallbackIdle
              /\ r' = [r EXCEPT !.fallbackIdle = TRUE]
RHRepeatNotice == /\ Mutant = "RepeatNotice" /\ r.phase = "explained" /\ r.notices = 1
                  /\ r' = [r EXCEPT !.notices = 2]
RHRequeue == /\ Mutant = "RequeueBody" /\ r.phase = "explained" /\ r.requeues = 0
             /\ r' = [r EXCEPT !.requeues = 1]
RHWait == UNCHANGED vars
RHNext == RHEnter \/ RHBusyEntry \/ RHRefuseUnheld \/ RHFirstKeep \/ RHRestoreWake \/ RHKeep \/ RHTick
          \/ RHTmuxReserve \/ RHTmuxNotSent \/ RHTmuxLook \/ RHTmuxRetry
          \/ RHReleaseScreen \/ RHLateArrival \/ RHOverdueRetry
          \/ RHIdleReserve \/ RHArm \/ RHRecheckFail \/ RHWrite \/ RHRace
          \/ RHExpire \/ RHLeafExpire \/ RHNoticeEnqueue \/ RHNoticeAck
          \/ RHLifecycle \/ RHSwap \/ RHStaleCallback \/ RHReboot
          \/ RHBusyWrite \/ RHFallback \/ RHRequeue \/ RHRepeatNotice \/ RHWait
RHSpec == RHInit /\ [][RHNext]_vars
          /\ WF_vars(RHEnter) /\ WF_vars(RHBusyEntry) /\ WF_vars(RHRefuseUnheld)
          /\ WF_vars(RHFirstKeep)
          /\ WF_vars(RHTmuxReserve) /\ WF_vars(RHTmuxNotSent) /\ WF_vars(RHTmuxLook)
          /\ WF_vars(RHRestoreWake) /\ WF_vars(RHKeep) /\ WF_vars(RHIdleReserve)
          /\ WF_vars(RHArm) /\ WF_vars(RHRecheckFail) /\ WF_vars(RHWrite)
          /\ WF_vars(RHExpire) /\ WF_vars(RHLeafExpire) /\ WF_vars(RHNoticeEnqueue)
          /\ WF_vars(RHNoticeAck) /\ WF_vars(RHLifecycle) /\ WF_vars(RHTick)
          /\ WF_vars(RHReleaseScreen) /\ WF_vars(RHOverdueRetry)
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
\* An unstarted original never outlives the due its first typed hold fixed.
RHBoundedHold == RHUnstarted => r.elapsed <= 5
RHOtherHoldPreserved == RHScreenHold => ~r.dueStored /\ r.refusals = 0
RHNoRetryPastDue == Scenario = "tmux-retry-due" => r.elapsed <= 5
RHNoLateLoss == Scenario = "tmux-late-follower" => r.other # "refused"
RHStaleNoop == r.other # "refused"
RHOneNoticeRecord == r.records <= 1
\* Witnesses: each is expected to break, proving its competing state is reached (not vacuous).
RHNoFrontDueRace == ~(r.phase = "waking" /\ r.age = 5)
RHNoLeafDueRace == ~(r.permit /\ r.keys = 0 /\ r.gate /\ r.age = 5)
RHNoDeadlineWin == ~(r.permit /\ r.keys = 0 /\ r.leafRefused)
RHNoKeyAtDue == ~(r.keys = 1 /\ r.age = 5)
RHNoReservedSwap == ~(r.phase = "reserved" /\ r.changed)
RHNoStaleCallback == ~(r.cbFired /\ r.other = "queued")
RHNoRebootMidHold == ~(r.replayed /\ r.phase = "held" /\ ~r.visible /\ r.age >= 2)
RHNoCrashReplay == ~(r.replayed /\ r.records = 1 /\ r.phase = "refused" /\ ~r.pendingAck)
RHNoUnheldKeep == Scenario = "no-holder-live" => r.phase # "held"
RHReachedKeep == <>(r.phase = "held" /\ r.age = 1)
RHNoInfiniteKeep == (r.phase = "held") ~> (r.phase \in {"done", "explained"})
RHEndExactlyOnce == <>((r.keys = 1 /\ r.refusals = 0) \/ (r.keys = 0 /\ r.refusals = 1 /\ r.notices = 1))
RHHealthyDelivers == <>(r.phase = "done")
\* Without the driver premise a healthy pane delivers or loses only the due race, never evidence.
RHHealthyOrDeadline == <>(r.phase = "done" \/ (r.phase = "explained" /\ r.reason = "deadline"))
=============================================================================
