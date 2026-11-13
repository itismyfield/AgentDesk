---- MODULE HerdrRestart ----
\* Design port model over main 33d1f4b5a9dce7023e2d0014e45a05ed6c530bba.
\* A bounded projection of one explicit restart, its old inflight turn, and the
\* existing owner's queued inputs. This is not a model of a new Core or host trait.
EXTENDS Naturals, FiniteSets
CONSTANTS Policy, Mutant, N, StartKind, CloseReply, CreateReply, SeedReply, Identity, Busy, Supersede
VARIABLE s
vars == <<s>>
Ids == 1..N
Restarting == s.phase \notin {"idle", "ready", "failed"}
Healthy == StartKind \in {"Bound", "Legacy"} /\ CloseReply = "confirmed"
           /\ CreateReply = "Created" /\ SeedReply = "accepted" /\ Identity = "same"
Init == s = [phase |-> "idle", row |-> StartKind, oldLive |-> StartKind # "Legacy",
             oldPermit |-> TRUE, inflight |-> Busy, settled |-> ~Busy, warned |-> FALSE,
             nonce |-> 0, creates |-> 0, newPanes |-> 0, seedEffects |-> 0, oldInputs |-> 0,
             foreignClosed |-> FALSE, visible |-> FALSE, awake |-> FALSE,
             generation |-> 0, intentGeneration |-> 0, clearSeen |-> FALSE, clearReceipts |-> {},
             accepted |-> {1}, q |-> [i \in Ids |-> IF i = 1 THEN "queued" ELSE "incoming"],
             effects |-> [i \in Ids |-> 0]]
\* restart.rs:219 currently refuses before cancellation. The design acquires the
\* restart owner/exclusion before any old-target write can be admitted.
Request == /\ s.phase = "idle"
           /\ s' = [s EXCEPT !.phase = IF Policy = "before" THEN "notice" ELSE "closing",
                            !.oldPermit = IF Policy = "before" THEN @ ELSE Mutant = "OldInput", !.warned = Policy = "after" /\ Busy,
                            !.q = IF Policy = "before" THEN @ ELSE [i \in Ids |-> IF @ [i] = "queued" THEN "held" ELSE @ [i]]]
\* Intake accepts originals into existing owner custody during the restart;
\* it never gains permission to type into the captured old execution.
Admit(i) == /\ s.phase # "idle" /\ s.q[i] = "incoming"
            /\ s' = [s EXCEPT !.accepted = @ \cup {i}, !.q[i] = "held"]
\* Unknown rows and changed pane/server identities cause a visible refusal.
RefuseIdentity == /\ s.phase = "closing"
                  /\ StartKind = "Unknown" \/ Identity # "same"
                  /\ s' = [s EXCEPT !.phase = "notice"]
CloseFails == /\ s.phase = "closing" /\ StartKind # "Unknown" /\ Identity = "same"
              /\ CloseReply # "confirmed"
              /\ s' = [s EXCEPT !.phase = "notice"]
\* terminate_scoped: exact close, absence read, retire CAS, captured-token
\* settlement. The four effects here summarize existing ordered service calls.
CloseExact == /\ s.phase = "closing" /\ StartKind # "Unknown" /\ Identity = "same"
              /\ CloseReply = "confirmed"
              /\ s' = [s EXCEPT !.oldLive = IF Mutant = "SkipClose" THEN @ ELSE FALSE,
                               !.phase = "retiring"]
RetireAndSettle == /\ s.phase = "retiring"
                   /\ s' = [s EXCEPT !.row = IF StartKind = "Legacy" THEN "Legacy" ELSE "Retired", !.inflight = FALSE,
                                    !.settled = TRUE, !.phase = "preparing",
                                    !.q = IF Mutant = "DropQueue"
                                      THEN [i \in Ids |-> IF i \in s.accepted THEN "lost" ELSE s.q[i]]
                                      ELSE @]
\* herdr_launch.rs:466-492 prepares a fresh nonce and commits Pending before
\* physical create. Transfer of the restart exclusion is a design port, not code.
InstallPending == /\ s.phase = "preparing" /\ s.row \in {"Retired", "Legacy"}
                  /\ s' = [s EXCEPT !.nonce = IF Mutant = "ReuseNonce" THEN 0 ELSE 1,
                                   !.row = "Pending", !.phase = "creating"]
ContinueAllowed == s.generation = s.intentGeneration \/ Mutant = "NoEpoch"
Create == /\ s.phase = "creating" /\ s.creates < 2 /\ ContinueAllowed
          /\ ~Supersede \/ s.clearSeen
          /\ s' = [s EXCEPT !.creates = @ + 1,
                           !.newPanes = @ + (IF CreateReply = "NotSent" THEN 0 ELSE 1),
                           !.phase = IF CreateReply = "Created" THEN "binding"
                                     ELSE IF Mutant = "ReplayUnknown" /\ s.creates = 0
                                          /\ CreateReply = "Indeterminate"
                                     THEN "creating" ELSE "notice"]
\* Launched without evidence and an unknown create acknowledgement stay Pending
\* and fail visibly; they do not authorize replay or success wording.
\* This port includes one durable seed ticket and its first submission before
\* source binding: Codex binds after first input, Claude may bind before it.
\* No ordinary queued original is submitted until that provider-specific source
\* binding completes. The caller must not wait for Codex Bound before its seed.
BindReady == /\ s.phase = "binding" /\ s.newPanes = 1 /\ ContinueAllowed /\ s.seedEffects < 2
             /\ s' = [s EXCEPT !.row = IF SeedReply = "accepted" THEN "Bound" ELSE @,
                              !.phase = IF SeedReply = "accepted"
                                        THEN IF Mutant = "ReplaySeed" /\ s.seedEffects = 0 THEN "binding" ELSE "ready"
                                        ELSE "notice",
                              !.seedEffects = IF SeedReply = "accepted" THEN @ + 1 ELSE @,
                              !.awake = Mutant # "NoWake"]
\* Explicit !clear can proceed after its transition wait times out (control.rs:
\* 523-552). It changes the queue generation and emits terminal drop receipts;
\* the old restart must observe supersession before create/publication.
ClearSupersedes == /\ Supersede /\ s.phase = "creating" /\ ~s.clearSeen
                   /\ s' = [s EXCEPT !.generation = 1, !.clearSeen = TRUE,
                                    !.clearReceipts = s.accepted,
                                    !.q = [i \in Ids |-> IF i \in s.accepted THEN "cancelled" ELSE s.q[i]]]
AbortSuperseded == /\ s.phase \in {"creating", "binding"} /\ ~ContinueAllowed
                   /\ s' = [s EXCEPT !.phase = "notice"]
NotifyFailure == /\ s.phase = "notice" /\ Mutant # "NoNotice"
                 /\ s' = [s EXCEPT !.visible = TRUE, !.phase = "failed"]
\* New delivery uses only the successor's incarnation and existing owner receipt.
Deliver(i) == /\ s.phase = "ready" /\ s.awake /\ s.q[i] \in {"held", "queued"}
              /\ \A j \in 1..(i-1) : s.q[j] = "delivered"
              /\ s.effects[i] < 2
              /\ s' = [s EXCEPT !.effects[i] = @ + 1, !.q[i] = "delivered"]
StaleOldDelivery(i) == /\ Policy = "after" /\ Restarting /\ s.oldLive /\ s.oldPermit
                       /\ s.q[i] \in {"held", "queued"} /\ s.oldInputs = 0
                       /\ s' = [s EXCEPT !.oldInputs = 1, !.effects[i] = @ + 1,
                                        !.q[i] = "delivered"]
ReplayInput(i) == /\ Mutant = "ReplayInput" /\ s.phase = "ready"
                  /\ s.q[i] = "delivered" /\ s.effects[i] = 1
                  /\ s' = [s EXCEPT !.q[i] = "held"]
UnsafeForeignClose == /\ Mutant = "NumericTarget" /\ s.phase = "closing"
                      /\ Identity # "same" /\ ~s.foreignClosed
                      /\ s' = [s EXCEPT !.foreignClosed = TRUE]
Wait == /\ s.phase \in {"ready", "failed", "notice"} /\ UNCHANGED s
Progress == Request \/ RefuseIdentity \/ CloseFails \/ CloseExact \/ RetireAndSettle
            \/ InstallPending \/ Create \/ BindReady \/ NotifyFailure \/ AbortSuperseded \/ ClearSupersedes
Next == Progress \/ (\E i \in Ids : Admit(i) \/ Deliver(i) \/ StaleOldDelivery(i) \/ ReplayInput(i))
        \/ UnsafeForeignClose \/ Wait
\* Every operation phase eventually returns; the failure notice transport also
\* eventually answers. These are explicit scheduler/I/O assumptions, not timeouts.
Spec == Init /\ [][Next]_vars /\ WF_vars(Progress)
        /\ (\A i \in Ids : WF_vars(Admit(i)) /\ WF_vars(Deliver(i)))
InputCustody == \A i \in s.accepted : s.q[i] \in {"queued", "held", "delivered"}
                 \/ (s.q[i] = "cancelled" /\ i \in s.clearReceipts)
InputAtMostOnce == \A i \in Ids : s.effects[i] <= 1
NoOldPaneInput == s.oldInputs = 0
NoForeignClose == ~s.foreignClosed
NoBlindCreate == s.creates <= 1
SeedAtMostOnce == s.seedEffects <= 1
FreshIncarnation == s.row \in {"Pending", "Bound"} /\ s.creates > 0 => s.nonce = 1
SingleReadySession == s.phase = "ready" => s.newPanes = 1 /\ ~s.oldLive
InflightSettledBeforeReady == s.phase = "ready" => s.settled /\ ~s.inflight /\ (~Busy \/ s.warned)
RequestResolves == <> (s.phase = "ready" \/ (s.phase = "failed" /\ s.visible))
NoSupersededReady == s.clearSeen => s.phase # "ready"
RestartAvailable == <> (s.phase = "ready" \/ ((~Healthy \/ Supersede) /\ s.phase = "failed" /\ s.visible))
AcceptedInputResolves == \A i \in Ids : (i \in s.accepted) ~>
                        (s.q[i] = "delivered" \/ (s.q[i] = "cancelled" /\ i \in s.clearReceipts) \/ (s.phase = "failed" /\ s.visible))
====
