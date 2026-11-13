---- MODULE HerdrRestart ----
\* Restart ports plus Codex resume A over 2c8873381478c7179f0347d5bfc5b850d5cb90e2.
\* A bounded projection of one explicit restart, its old inflight turn, and the
\* existing owner's queued inputs. This is not a model of a new Core or host trait.
EXTENDS Naturals, FiniteSets
CONSTANTS Policy, Mutant, N, StartKind, CloseReply, CreateReply, SeedReply, Identity, Busy, Supersede
CONSTANTS APolicy, AMutant, AResumeIntent, ACursorMatches, ACrash
VARIABLES s, a
vars == <<s, a>>
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
RestartNext == Next /\ UNCHANGED a
RestartFair == WF_vars(Progress /\ UNCHANGED a)
        /\ (\A i \in Ids : WF_vars(Admit(i) /\ UNCHANGED a) /\ WF_vars(Deliver(i) /\ UNCHANGED a))
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

\* A is the provider helper boundary, independently of the still-unwired command.
\* Offsets 0/1/2 represent fresh, confirmed old EOF, and new startup bytes.
AInit == a = [resumeIntent |-> AResumeIntent, cursorMatches |-> ACursorMatches, phase |-> "end", endConfirmed |-> FALSE, captured |-> FALSE,
              capturedOffset |-> 0, currentEof |-> 0, mode |-> "fresh",
              prepared |-> FALSE, persisted |-> FALSE, durableOffset |-> 0,
              durableNonce |-> 0, loadedOffset |-> 0, loadedNonce |-> 0,
              launched |-> FALSE, cause |-> "Startup", crashed |-> FALSE,
              recovered |-> FALSE, tailStarted |-> FALSE, readOffset |-> 0,
              replayedHistory |-> FALSE, skippedNewPrefix |-> FALSE, visible |-> FALSE]
AConfirmEnd == /\ a.phase = "end" /\ APolicy = "after"
               /\ a' = [a EXCEPT !.endConfirmed = AResumeIntent, !.phase = "capture"]
ABeforeRefusal == /\ a.phase = "end" /\ APolicy = "before"
                  /\ a' = [a EXCEPT !.phase = "failed", !.visible = TRUE]
\* The EOF is read after exact end. Cursor disagreement refuses resume in A.
ACapture == /\ (a.phase = "capture" \/ (a.phase = "end" /\ AMutant = "BeforeEndCapture"))
            /\ a' = [a EXCEPT !.captured = TRUE,
                             !.capturedOffset = IF AResumeIntent THEN 1 ELSE 0,
                             !.currentEof = IF AResumeIntent THEN 1 ELSE 0,
                             !.prepared = ~AResumeIntent \/ ACursorMatches \/ AMutant = "IgnoreCursorMismatch",
                             !.phase = IF AResumeIntent /\ ~ACursorMatches /\ AMutant # "IgnoreCursorMismatch"
                                       THEN "failed" ELSE "record",
                             !.visible = AResumeIntent /\ ~ACursorMatches /\ AMutant # "IgnoreCursorMismatch"]
\* The incarnation persists the offset before launch. Ordinary launch stays fresh.
APersist == /\ a.phase = "record"
            /\ a' = [a EXCEPT !.mode = IF AResumeIntent \/ AMutant = "OrdinaryResume" THEN "resume" ELSE "fresh",
                             !.persisted = AMutant # "VolatileOffset",
                             !.durableOffset = IF AMutant = "VolatileOffset" THEN 0 ELSE a.capturedOffset,
                             !.durableNonce = IF AMutant = "VolatileOffset" THEN 0 ELSE 1,
                             !.loadedOffset = a.capturedOffset, !.loadedNonce = 1, !.phase = "launch"]
ALaunch == /\ a.phase = "launch"
           /\ a' = [a EXCEPT !.launched = TRUE,
                            !.cause = IF a.mode = "resume" THEN "Resume" ELSE "Startup",
                            !.currentEof = a.capturedOffset + 1, !.phase = "before_seed"]
\* A process can crash after launch/startup metadata but before its first seed.
ACrashBeforeSeed == /\ a.phase = "before_seed" /\ ACrash /\ ~a.crashed
                    /\ a' = [a EXCEPT !.crashed = TRUE, !.loadedOffset = 0,
                                     !.loadedNonce = 0, !.phase = "boot"]
ABoot == /\ a.phase = "boot"
         /\ a' = [a EXCEPT !.recovered = TRUE,
                          !.loadedOffset = IF AMutant = "BootRecapture" THEN a.currentEof ELSE a.durableOffset,
                          !.loadedNonce = a.durableNonce,
                          !.phase = IF a.persisted THEN "observe" ELSE "failed",
                          !.visible = ~a.persisted]
ANoCrash == /\ a.phase = "before_seed" /\ ~ACrash
            /\ a' = [a EXCEPT !.phase = "observe"]
ACauseAccepted == IF AMutant = "StartupOnly" THEN a.cause = "Startup"
                  ELSE (a.mode = "fresh" /\ a.cause = "Startup")
                    \/ (a.mode = "resume" /\ a.cause = "Resume")
\* The recorded incarnation, not a boot-time EOF read or zero, starts the tail.
AObserve == /\ a.phase = "observe"
            /\ LET at == IF AMutant = "OffsetZero" THEN 0 ELSE a.loadedOffset
               IN a' = [a EXCEPT !.tailStarted = ACauseAccepted,
                                !.readOffset = at,
                                !.replayedHistory = ACauseAccepted /\ at < a.capturedOffset,
                                !.skippedNewPrefix = ACauseAccepted /\ at > a.capturedOffset,
                                !.phase = IF ACauseAccepted THEN "done" ELSE "failed",
                                !.visible = ~ACauseAccepted]
AWait == /\ a.phase \in {"done", "failed"} /\ UNCHANGED a
AProgress == AConfirmEnd \/ ABeforeRefusal \/ ACapture \/ APersist \/ ALaunch
             \/ ACrashBeforeSeed \/ ABoot \/ ANoCrash \/ AObserve
RRequest == Request /\ UNCHANGED a
RRefuseIdentity == RefuseIdentity /\ UNCHANGED a
RCloseFails == CloseFails /\ UNCHANGED a
RCloseExact == CloseExact /\ UNCHANGED a
RRetireAndSettle == RetireAndSettle /\ UNCHANGED a
RInstallPending == InstallPending /\ UNCHANGED a
RCreate == Create /\ UNCHANGED a
RBindReady == BindReady /\ UNCHANGED a
RNotifyFailure == NotifyFailure /\ UNCHANGED a
RAbortSuperseded == AbortSuperseded /\ UNCHANGED a
RClearSupersedes == ClearSupersedes /\ UNCHANGED a
RUnsafeForeignClose == UnsafeForeignClose /\ UNCHANGED a
RWait == Wait /\ UNCHANGED a
RAdmit(i) == Admit(i) /\ UNCHANGED a
RDeliver(i) == Deliver(i) /\ UNCHANGED a
RStaleOldDelivery(i) == StaleOldDelivery(i) /\ UNCHANGED a
RReplayInput(i) == ReplayInput(i) /\ UNCHANGED a
PConfirmEnd == AConfirmEnd /\ UNCHANGED s
PBeforeRefusal == ABeforeRefusal /\ UNCHANGED s
PCapture == ACapture /\ UNCHANGED s
PPersist == APersist /\ UNCHANGED s
PLaunch == ALaunch /\ UNCHANGED s
PCrashBeforeSeed == ACrashBeforeSeed /\ UNCHANGED s
PBoot == ABoot /\ UNCHANGED s
PNoCrash == ANoCrash /\ UNCHANGED s
PObserve == AObserve /\ UNCHANGED s
PWait == AWait /\ UNCHANGED s
CombinedNext == RRequest \/ RRefuseIdentity \/ RCloseFails \/ RCloseExact \/ RRetireAndSettle \/ RInstallPending \/ RCreate \/ RBindReady \/ RNotifyFailure \/ RAbortSuperseded \/ RClearSupersedes \/ RUnsafeForeignClose \/ RWait
  \/ (\E i \in Ids : RAdmit(i) \/ RDeliver(i) \/ RStaleOldDelivery(i) \/ RReplayInput(i))
  \/ PConfirmEnd \/ PBeforeRefusal \/ PCapture \/ PPersist \/ PLaunch \/ PCrashBeforeSeed \/ PBoot \/ PNoCrash \/ PObserve \/ PWait
Spec == Init /\ AInit /\ [][CombinedNext]_vars /\ RestartFair
        /\ WF_vars(AProgress /\ UNCHANGED s)
AAfterConfirmedEnd == a.captured /\ AResumeIntent => a.endConfirmed
ACursorAgreement == a.prepared /\ AResumeIntent => ACursorMatches
AOrdinaryFresh == ~AResumeIntent => a.mode = "fresh"
ADurableBeforeLaunch == a.launched => a.persisted /\ a.durableOffset = a.capturedOffset /\ a.durableNonce = 1
ABootOffsetStable == a.recovered /\ a.persisted => a.loadedOffset = a.durableOffset /\ a.loadedNonce = a.durableNonce
AOffsetPreserved == a.tailStarted => a.readOffset = a.capturedOffset
ANoHistoricalReplay == ~a.replayedHistory
ANoNewPrefixSkip == ~a.skippedNewPrefix
AScopedCause == a.tailStarted => ((a.mode = "resume" /\ a.cause = "Resume") \/ (a.mode = "fresh" /\ a.cause = "Startup"))
ARequestResolves == <> (a.phase = "done" \/ (a.phase = "failed" /\ a.visible))
AResumeAvailable == <> (a.phase = "done" \/ (~ACursorMatches /\ a.phase = "failed" /\ a.visible))
====
