-------------------------- MODULE TurnPresence --------------------------
EXTENDS Naturals, TLC
CONSTANTS AdmissionMode, FinalMode, Grace
VARIABLES running, process, phase, identity, progress, epoch, row, changed, bumped, before, after,
          pinned, retry, accepted, busy, unknownTicks, notice, published,
          wrongAdmission, wrongPublication, publishedInput, absence, history
coreVars == <<running, process, phase, identity, progress, epoch, row, changed, bumped, before, after,
          pinned, retry, accepted, busy, unknownTicks, notice, published,
          wrongAdmission, wrongPublication, publishedInput, absence>>
vars == <<coreVars, history>>
\* Identity 0 is the original; 1 changes session, 2..6 owner/nonce/turn/source/core.
\* Identity never wraps while an old proof exists; progress may cycle forever.
Snapshot == [boot |-> process, id |-> identity, tick |-> progress, ownerEpoch |-> epoch, row |-> row]
HealthyOpen == running /\ row = "present"
Same(a, b, mode) ==
    /\ a.row = "present" /\ b.row = "present"
    /\ a.boot = b.boot /\ a.ownerEpoch = b.ownerEpoch
    /\ IF mode = "name" THEN (a.id = 1) = (b.id = 1) ELSE a.id = b.id
    /\ (mode # "strict" \/ a.tick = b.tick)
CoreInit ==
    /\ running = TRUE /\ process = 0
    /\ phase = "begin" /\ identity = 0 /\ progress = 0 /\ epoch = 0
    /\ row = "present" /\ changed = FALSE /\ bumped = FALSE
    /\ before = Snapshot /\ after = Snapshot /\ pinned = Snapshot
    /\ retry = 0 /\ accepted = FALSE /\ busy = FALSE
    /\ unknownTicks = 0 /\ notice = FALSE /\ published = FALSE
    /\ wrongAdmission = FALSE /\ wrongPublication = FALSE /\ publishedInput = Snapshot /\ absence = FALSE
Progress == /\ progress' = 1 - progress
            /\ UNCHANGED <<phase, identity, epoch, row, changed, bumped, before, after,
                 pinned, retry, accepted, busy, unknownTicks, notice, published,
                 wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

ChangeIdentity ==
    /\ ~changed /\ identity' \in 1..6 /\ changed' = TRUE
    /\ UNCHANGED <<phase, progress, epoch, row, bumped, before, after, pinned, retry,
         accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

UnrelatedEpochBump ==
    /\ ~bumped /\ epoch' = 1 /\ bumped' = TRUE
    /\ UNCHANGED <<phase, identity, progress, row, changed, before, after, pinned, retry,
         accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

ReadFail ==
    /\ row = "present" /\ ~changed /\ changed' = TRUE /\ row' \in {"unreadable", "missing"}
    /\ UNCHANGED <<phase, identity, progress, epoch, bumped, before, after,
         pinned, retry, accepted, busy, unknownTicks, notice, published,
         wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

ReadRecover ==
    /\ phase = "begin" /\ row # "present" /\ row' = "present"
    /\ UNCHANGED <<phase, identity, progress, epoch, changed, bumped, before, after,
         pinned, retry, accepted, busy, unknownTicks, notice, published,
         wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

Begin ==
    /\ phase = "begin"
    /\ phase' = ("collect-before")
    /\ retry' = (0)
    /\ published' = (FALSE)
    /\ absence' = FALSE
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, accepted, busy, unknownTicks, notice, wrongAdmission, wrongPublication>>
    /\ UNCHANGED publishedInput
    /\ UNCHANGED <<running, process>>

CollectBefore ==
    /\ phase = "collect-before"
    /\ before' = (Snapshot)
    /\ phase' = ("collect-after")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, after, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

CollectAfter ==
    /\ phase = "collect-after"
    /\ after' = (Snapshot)
    /\ phase' = ("admit")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

Retry ==
    /\ phase = "admit" /\ ~Same(before, after, AdmissionMode) /\ retry = 0 /\ before.row = "present" /\ after.row = "present"
    /\ phase' = ("collect-before")
    /\ retry' = (1)
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

Admit ==
    /\ phase = "admit" /\ Same(before, after, AdmissionMode)
    /\ accepted' = (TRUE)
    /\ pinned' = (before)
    /\ phase' = ("read-open")
    /\ wrongAdmission' = (wrongAdmission \/ before.id # after.id)
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, retry, busy, unknownTicks, notice, published, wrongPublication, absence>>
    /\ UNCHANGED publishedInput
    /\ UNCHANGED <<running, process>>

RejectAdmission ==
    /\ phase = "admit" /\ ~Same(before, after, AdmissionMode) /\ (retry = 1 \/ before.row # "present" \/ after.row # "present")
    /\ accepted' = (FALSE)
    /\ phase' = ("unbound-before")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

CollectUnboundBefore ==
    /\ phase = "unbound-before"
    /\ before' = (Snapshot)
    /\ phase' = ("unbound-after")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, after, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

CollectUnboundAfter ==
    /\ phase = "unbound-after"
    /\ after' = (Snapshot)
    /\ phase' = ("unbound")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

ObserveUnknown ==
    /\ phase = "unbound"
    /\ busy' = (FALSE)
    /\ unknownTicks' = (IF unknownTicks < Grace THEN unknownTicks + 1 ELSE Grace)
    /\ notice' = (unknownTicks + 1 >= Grace)
    /\ absence' = (before.row = "missing" /\ after.row = "missing" /\ row = "missing")
    /\ phase' = ("begin")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, accepted, published, wrongAdmission, wrongPublication>>
    /\ UNCHANGED publishedInput
    /\ UNCHANGED <<running, process>>

ReadOpen ==
    /\ phase = "read-open"
    /\ busy' = (TRUE)
    /\ unknownTicks' = (0)
    /\ notice' = (FALSE)
    /\ absence' = (FALSE)
    /\ phase' = ("publication-before")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, accepted, published, wrongAdmission, wrongPublication>>
    /\ UNCHANGED publishedInput
    /\ UNCHANGED <<running, process>>

PublicationBefore ==
    /\ phase = "publication-before"
    /\ before' = (Snapshot)
    /\ phase' = ("publication-after")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, after, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

PublicationAfter ==
    /\ phase = "publication-after"
    /\ after' = (Snapshot)
    /\ phase' = ("publication-pair")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

PublicationPair ==
    /\ phase = "publication-pair"
    /\ phase' = (IF Same(before, after, IF AdmissionMode = "name" THEN "name" ELSE "projection") /\ Same(pinned, after, IF AdmissionMode = "name" THEN "name" ELSE "projection") THEN "publication-final" ELSE "begin")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

FinalCheckAndPublish ==
    /\ phase = "publication-final"
    /\ publishedInput' = Snapshot
    /\ published' = (Same(pinned, Snapshot, FinalMode))
    /\ wrongPublication' = (wrongPublication \/ (Same(pinned, Snapshot, FinalMode) /\ pinned.id # identity))
    /\ phase' = ("begin")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, accepted, busy, unknownTicks, notice, wrongAdmission, absence>>
    /\ UNCHANGED <<running, process>>

\* Restart discards the in-memory reading; a new process collects the surviving row again.
Crash ==
    /\ running /\ ~changed
    /\ running' = FALSE /\ changed' = TRUE /\ phase' = "down"
    /\ accepted' = FALSE /\ busy' = FALSE /\ published' = FALSE /\ absence' = FALSE
    /\ unknownTicks' = 0 /\ notice' = FALSE
    /\ UNCHANGED <<process, identity, progress, epoch, row, bumped, before, after,
         pinned, retry, wrongAdmission, wrongPublication, publishedInput>>
Boot ==
    /\ ~running /\ phase = "down"
    /\ running' = TRUE /\ process' = 1 /\ phase' = "begin"
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after,
         pinned, retry, accepted, busy, unknownTicks, notice, published,
         wrongAdmission, wrongPublication, publishedInput, absence>>

Work == Boot \/ Begin \/ CollectBefore \/ CollectAfter \/ Retry \/ Admit \/ RejectAdmission
        \/ CollectUnboundBefore \/ CollectUnboundAfter \/ ObserveUnknown \/ ReadOpen
        \/ PublicationBefore \/ PublicationAfter \/ PublicationPair \/ FinalCheckAndPublish
CoreNext == Work \/ Crash \/ Progress \/ ChangeIdentity \/ UnrelatedEpochBump \/ ReadFail \/ ReadRecover
Init == CoreInit /\ history = "outside-retired-presence-scenario"
Next == CoreNext /\ UNCHANGED history
Spec == Init /\ [][Next]_vars /\ WF_coreVars(Work)
NoWrongAdmission == ~wrongAdmission
NoWrongPublication == ~wrongPublication
NoOldProofAfterObservedBoundary == published =>
    publishedInput.id = pinned.id /\ publishedInput.boot = pinned.boot /\ publishedInput.row = "present" /\ publishedInput.ownerEpoch = pinned.ownerEpoch
\* Check only the original healthy epoch: true boundary/read faults retain ordinary Unknown.
NoProgressIdentityUnknown == (identity = 0 /\ row = "present" /\ ~bumped /\ ~changed) => unknownTicks = 0
NoProgressIdentityNotice == (identity = 0 /\ row = "present" /\ ~bumped /\ ~changed) => ~notice
NoAbsenceFromPresentOrUnreadRow == absence => before.row = "missing" /\ after.row = "missing"
EventuallyStableBusy == (<>[] HealthyOpen) => <>[] (accepted /\ busy /\ ~notice)
PublicationContinues == (<>[] HealthyOpen) => []<> published

\* The separate scenario keeps the identity/publication checks above unchanged. It models the
\* durable marker history that the Unbound Herdr absence branch consumes after retirement.
HistoryInitAt(initial) ==
    /\ CoreInit
    /\ history = [stage |-> IF initial = "empty" THEN "empty" ELSE "retired",
                   row |-> IF initial = "empty" THEN "missing" ELSE "retired",
                   runtime |-> IF initial = "retired" THEN "codex_tui" ELSE "none",
                   legacyRuntime |-> IF initial = "fallback" THEN "codex_tui" ELSE "none",
                   host |-> IF initial = "empty" THEN "absent" ELSE "herdr",
                   rowNonce |-> IF initial = "empty" THEN 0 ELSE 1,
                   diskNonce |-> IF initial = "empty" THEN 0 ELSE 1,
                   tmuxProbe |-> "missing", liveProcess |-> FALSE, inflight |-> FALSE,
                   settled |-> FALSE, up |-> TRUE, reconciled |-> TRUE,
                   restarted |-> FALSE, evaluated |-> FALSE, activity |-> "unknown",
                   input |-> FALSE, started |-> FALSE, markerChanged |-> FALSE,
                   cleanupNonce |-> IF initial = "empty" THEN 0 ELSE 1, removedLive |-> FALSE, removedForeign |-> FALSE,
                   sweepEligible |-> FALSE]
HistoryInit == HistoryInitAt("empty")
HistoryQuiet == history.tmuxProbe = "missing" /\ ~history.liveProcess /\ ~history.inflight
MarkerReadable == history.runtime # "unreadable" /\ history.legacyRuntime # "unreadable"
ExpectedMarker == history.runtime \in {"none", "codex_tui"}
    /\ history.legacyRuntime \in {"none", "codex_tui"}
    /\ (history.runtime = "codex_tui" \/ history.legacyRuntime = "codex_tui")
ExactCleanup == history.host = "herdr" /\ history.diskNonce = history.cleanupNonce
    /\ history.rowNonce = history.cleanupNonce /\ MarkerReadable /\ ExpectedMarker
    /\ HistoryQuiet
\* The unsafe mode is a guard-removal mutant, never a production policy.
RemoveMarker(mode) == mode = "unsafe" \/ (mode = "cleanup" /\ ExactCleanup)
MarkerAbsent == history.runtime = "none" /\ history.legacyRuntime = "none"
HistoryAbsent == history.row = "retired" /\ history.settled /\ HistoryQuiet
    /\ history.host \in {"absent", "herdr"}
    /\ history.runtime = "none" /\ history.legacyRuntime = "none"

\* first_input.rs:220-224 writes codex_tui for the earlier TUI incarnation.
TUILaunch ==
    /\ history.stage = "empty" /\ history.up /\ history.reconciled
    /\ history' = [history EXCEPT !.stage = "tui", !.runtime = "codex_tui"]
    /\ UNCHANGED coreVars
\* herdr_launch.rs:472-488 commits Pending before stamping the host and nonce.
HerdrFreshLaunch ==
    /\ history.stage = "tui" /\ history.up /\ history.reconciled
    /\ history' = [history EXCEPT !.stage = "fresh-cleanup", !.row = "pending",
                     !.host = "herdr", !.rowNonce = 1, !.diskNonce = 1, !.cleanupNonce = 1]
    /\ UNCHANGED coreVars
\* The same logical source authority serializes the final marker checks and unlink.
FreshMarkerCleanup(mode) ==
    /\ history.stage = "fresh-cleanup" /\ history.up /\ history.reconciled
    /\ LET remove == RemoveMarker(mode)
       IN history' = [history EXCEPT
            !.stage = IF mode = "cleanup" /\ ~ExactCleanup /\ ~MarkerAbsent THEN "held" ELSE "pending",
            !.runtime = IF remove THEN "none" ELSE @,
            !.legacyRuntime = IF remove THEN "none" ELSE @,
            !.removedLive = @ \/ (remove /\ (history.row = "bound" \/ ~HistoryQuiet)),
            !.removedForeign = @ \/ (remove /\ ~ExactCleanup)]
    /\ UNCHANGED coreVars
HerdrBind ==
    /\ history.stage = "pending" /\ history.up /\ history.reconciled
    /\ history' = [history EXCEPT !.stage = "running", !.row = "bound"]
    /\ UNCHANGED coreVars
ExecutionEnds ==
    /\ history.stage = "running" /\ history.up /\ history.reconciled
    /\ history' = [history EXCEPT !.stage = "ended"]
    /\ UNCHANGED coreVars
\* herdr_terminate.rs:224-237 changes the hosted state after an end proof.
ConfirmRetired ==
    /\ history.stage = "ended" /\ HistoryQuiet /\ history.up /\ history.reconciled
    /\ history' = [history EXCEPT !.stage = "retired", !.row = "retired"]
    /\ UNCHANGED coreVars
\* Capture represents a caller's exact retired execution; a successor can win before unlink.
CaptureRetiredCleanup ==
    /\ history.row = "retired" /\ ~history.settled /\ history.up /\ history.reconciled
    /\ history.cleanupNonce # history.rowNonce
    /\ history' = [history EXCEPT !.cleanupNonce = history.rowNonce]
    /\ UNCHANGED coreVars
RetiredCleanup(mode) ==
    /\ history.row = "retired" /\ ~history.settled /\ history.up /\ history.reconciled
    /\ history.cleanupNonce = history.rowNonce
    /\ LET remove == RemoveMarker(mode)
       IN history' = [history EXCEPT
            !.settled = mode = "legacy" \/ remove
                \/ (history.runtime = "none" /\ history.legacyRuntime = "none"),
            !.runtime = IF remove THEN "none" ELSE @,
            !.legacyRuntime = IF remove THEN "none" ELSE @,
            !.removedLive = @ \/ (remove /\ (history.row = "bound" \/ ~HistoryQuiet)),
            !.removedForeign = @ \/ (remove /\ ~ExactCleanup)]
    /\ UNCHANGED coreVars
\* Rows and files survive restart. The independent orphan sweep is optional below.
HistoryRestart ==
    /\ history.up /\ ~history.restarted /\ ~history.started
    /\ history' = [history EXCEPT !.up = FALSE, !.reconciled = FALSE,
                     !.restarted = TRUE, !.evaluated = FALSE, !.activity = "unknown"]
    /\ UNCHANGED coreVars
HistoryBoot ==
    /\ ~history.up
    /\ history' = [history EXCEPT !.up = TRUE]
    /\ UNCHANGED coreVars
RestartReconcileRetired ==
    /\ history.up /\ ~history.reconciled /\ history.row = "retired"
    /\ history' = [history EXCEPT !.reconciled = TRUE, !.settled = FALSE,
                     !.cleanupNonce = history.rowNonce]
    /\ UNCHANGED coreVars
RestartReconcileOther ==
    /\ history.up /\ ~history.reconciled /\ history.row # "retired"
    /\ history' = [history EXCEPT !.reconciled = TRUE]
    /\ UNCHANGED coreVars
NewInput ==
    /\ history.row \in {"bound", "retired"} /\ ~history.input /\ history.up
    /\ history' = [history EXCEPT !.input = TRUE]
    /\ UNCHANGED coreVars
PresenceEvaluate ==
    /\ history.row = "retired" /\ history.settled /\ history.up /\ history.reconciled
    /\ ~history.started
    /\ history' = [history EXCEPT !.evaluated = TRUE,
                     !.activity = IF HistoryAbsent THEN "idle" ELSE "unknown"]
    /\ UNCHANGED coreVars
\* owner_input.rs:670-686 and queue_dispatch.rs:244-268 require Idle before start.
StartNewInput ==
    /\ history.input /\ ~history.started /\ history.evaluated /\ history.activity = "idle"
    /\ history.up /\ history.reconciled
    /\ history' = [history EXCEPT !.started = TRUE, !.row = "pending",
                     !.rowNonce = 2, !.diskNonce = 2, !.evaluated = FALSE]
    /\ UNCHANGED coreVars
\* Enum values, decode failures, nonce changes and local live slots are distinct witnesses.
ForeignRuntimeMarker ==
    /\ history.row \in {"retired", "pending"} /\ ~history.markerChanged /\ ~history.started
    /\ \E kind \in {"process_backend", "claude_tui", "legacy_tmux_wrapper", "unrecognized", "unreadable"}, location \in {"current", "legacy"}:
        history' = [history EXCEPT !.runtime = IF location = "current" THEN kind ELSE @,
                     !.legacyRuntime = IF location = "legacy" THEN kind ELSE @,
                     !.markerChanged = TRUE, !.evaluated = FALSE]
    /\ UNCHANGED coreVars
ForeignHostMarker ==
    /\ history.row \in {"retired", "pending"} /\ ~history.markerChanged /\ ~history.started
    /\ history' = [history EXCEPT !.host = "process", !.markerChanged = TRUE,
                     !.evaluated = FALSE]
    /\ UNCHANGED coreVars
SuccessorNonce ==
    /\ history.row = "retired" /\ ~history.markerChanged /\ ~history.started
    /\ history' = [history EXCEPT !.row = "bound", !.rowNonce = 2, !.diskNonce = 2,
                     !.runtime = "codex_tui", !.markerChanged = TRUE, !.evaluated = FALSE]
    /\ UNCHANGED coreVars
LiveSlotAppears ==
    /\ history.row = "retired" /\ ~history.markerChanged /\ ~history.started
    /\ \E slot \in {"tmux", "probe-failed", "process", "inflight"}:
        history' = [history EXCEPT !.tmuxProbe = IF slot = "tmux" THEN "present" ELSE IF slot = "probe-failed" THEN "failed" ELSE "missing",
                     !.liveProcess = slot = "process", !.inflight = slot = "inflight",
                     !.markerChanged = TRUE, !.evaluated = FALSE]
    /\ ~HistoryQuiet'
    /\ UNCHANGED coreVars
\* A stale cleanup still performs the exact final guard under the source authority.
StaleCleanupAttempt(mode) ==
    /\ history.markerChanged /\ ~history.started /\ history.up /\ history.reconciled
    /\ LET remove == RemoveMarker(mode)
       IN history' = [history EXCEPT !.runtime = IF remove THEN "none" ELSE @,
            !.legacyRuntime = IF remove THEN "none" ELSE @,
            !.removedLive = @ \/ (remove /\ (history.row = "bound" \/ ~HistoryQuiet)),
            !.removedForeign = @ \/ (remove /\ ~ExactCleanup)]
    /\ UNCHANGED coreVars
\* Existing orphan sweep requires another dead cleanup and sufficiently old files; no fairness
\* is assumed for it. Its broad live-Herdr policy is an existing path outside this fix.
SweepBecomesEligible ==
    /\ history.restarted /\ ~history.sweepEligible /\ ~history.started
    /\ history' = [history EXCEPT !.sweepEligible = TRUE]
    /\ UNCHANGED coreVars
ExistingOrphanSweep ==
    /\ history.sweepEligible /\ history.row = "retired" /\ HistoryQuiet
    /\ ~history.started /\ history.runtime # "none"
    /\ history' = [history EXCEPT !.runtime = "none", !.legacyRuntime = "none", !.host = "absent", !.diskNonce = 0,
                     !.evaluated = FALSE]
    /\ UNCHANGED coreVars
HistoryDone == history.started /\ UNCHANGED vars
HistoryWork(mode) == TUILaunch \/ HerdrFreshLaunch \/ FreshMarkerCleanup(mode) \/ HerdrBind
    \/ ExecutionEnds \/ ConfirmRetired \/ CaptureRetiredCleanup \/ RetiredCleanup(mode)
    \/ HistoryBoot \/ RestartReconcileRetired \/ RestartReconcileOther \/ NewInput
    \/ PresenceEvaluate \/ StartNewInput
HistoryNext(mode) == HistoryWork(mode) \/ HistoryRestart \/ HistoryDone
HistoryFair(mode) == WF_vars(TUILaunch) /\ WF_vars(HerdrFreshLaunch)
    /\ WF_vars(FreshMarkerCleanup(mode)) /\ WF_vars(HerdrBind) /\ WF_vars(ExecutionEnds)
    /\ WF_vars(ConfirmRetired) /\ WF_vars(CaptureRetiredCleanup) /\ WF_vars(RetiredCleanup(mode))
    /\ WF_vars(HistoryBoot) /\ WF_vars(RestartReconcileRetired) /\ WF_vars(RestartReconcileOther)
    /\ WF_vars(NewInput) /\ WF_vars(PresenceEvaluate) /\ WF_vars(StartNewInput)
RetiredPresenceBeforeSpec == HistoryInit /\ [][HistoryNext("legacy")]_vars /\ HistoryFair("legacy")
RetiredPresenceCleanupSpec == HistoryInit /\ [][HistoryNext("cleanup")]_vars /\ HistoryFair("cleanup")
RetiredPresenceRestartBeforeSpec == HistoryInitAt("retired")
    /\ [][HistoryNext("legacy")]_vars /\ HistoryFair("legacy")
RetiredPresenceRestartFixedSpec == HistoryInitAt("retired")
    /\ [][HistoryNext("cleanup")]_vars /\ HistoryFair("cleanup")
RetiredPresenceFallbackFixedSpec == HistoryInitAt("fallback")
    /\ [][HistoryNext("cleanup")]_vars /\ HistoryFair("cleanup")
RetiredPresenceForeignSpec == HistoryInitAt("retired")
    /\ [][HistoryNext("cleanup") \/ ForeignRuntimeMarker \/ ForeignHostMarker
           \/ SuccessorNonce \/ LiveSlotAppears \/ StaleCleanupAttempt("cleanup")]_vars
RetiredPresenceUnsafeSpec == HistoryInitAt("retired")
    /\ [][HistoryNext("unsafe") \/ ForeignRuntimeMarker \/ ForeignHostMarker
           \/ SuccessorNonce \/ LiveSlotAppears \/ StaleCleanupAttempt("unsafe")]_vars
RetiredPresenceSweepSpec == HistoryInit
    /\ [][HistoryNext("legacy") \/ SweepBecomesEligible \/ ExistingOrphanSweep]_vars
    /\ HistoryFair("legacy")
RetiredOnlyQuietNotUnknown ==
    (history.row = "retired" /\ history.settled /\ HistoryQuiet
     /\ history.host = "herdr" /\ history.runtime \in {"none", "codex_tui"}
     /\ history.legacyRuntime \in {"none", "codex_tui"}
     /\ history.diskNonce = history.rowNonce /\ history.evaluated) => history.activity = "idle"
NoForeignMarkerAbsence ==
    (history.row = "retired" /\ history.evaluated
     /\ (history.host = "process" \/ history.runtime \in {"process_backend", "claude_tui", "legacy_tmux_wrapper", "unrecognized", "unreadable"}
          \/ history.legacyRuntime \in {"process_backend", "claude_tui", "legacy_tmux_wrapper", "unrecognized", "unreadable"}))
    => history.activity = "unknown"
NoLiveMarkerRemoval == ~history.removedLive
NoForeignMarkerRemoval == ~history.removedForeign
\* Stable owned scenario only: eventual end proof, boot/reconcile, presence sampling and input
\* scheduling are fair. A foreign successor or permanently unreadable input is not promised progress.
NewInputEventuallyStarts == history.input ~> history.started
=============================================================================
