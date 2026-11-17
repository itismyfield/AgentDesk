---- MODULE HerdrClearLaunch ----
\* One-shot clear fence, native repinning, verified release and a later restart.
EXTENDS Naturals
CONSTANTS FenceLaunch, CommitHeldClear, ClearResult, RepinRequired, RaceClear,
          NoRelease, ReleaseWithoutToken, FilterAfterJudge
VARIABLE s
vars == <<s>>
Fresh(turn) == IF turn = 1 THEN "fresh1" ELSE "fresh2"
Init == s = [phase |-> "ready", turn |-> 1, row |-> "retired",
    clearedMemory |-> FALSE, epoch |-> 0, fence |-> "none", captured |-> "none",
    firstPending |-> FALSE, clearedSession |-> "old", selector |-> "old",
    native |-> "old", nativeVerified |-> ~RepinRequired,
    candidate |-> "none", resume |-> FALSE, firstSession |-> "none",
    verifiedSession |-> "none", expectedResume |-> "old",
    lastLaunch |-> [epoch |-> 0, first |-> FALSE, session |-> "none", resume |-> FALSE],
    lastApi |-> "none", apiLeak |-> FALSE, unsafeRepins |-> 0, journalWrites |-> 0,
    restarted1 |-> FALSE, retried |-> FALSE, wrongReleaseChanged |-> FALSE,
    rolledOver |-> FALSE]

\* The ordinary Retired clear is the observed path. A transcript-only clear
\* sets volatile clearedMemory, but cannot fence native-journal selection.
ClearRetired ==
    /\ s.phase = "ready" /\ ClearResult = "retired"
    /\ s' = [s EXCEPT !.phase = "cleared", !.epoch = 1, !.firstPending = TRUE,
        !.clearedMemory = TRUE, !.selector = "none",
        !.fence = IF FenceLaunch THEN "t1" ELSE "none"]
ClearHeld ==
    /\ s.phase = "ready" /\ ClearResult = "held"
    /\ s' = [s EXCEPT !.phase = "cleared", !.row = "held", !.epoch = 1,
        !.firstPending = TRUE, !.clearedMemory = CommitHeldClear,
        !.selector = IF CommitHeldClear THEN "none" ELSE @,
        !.fence = IF FenceLaunch /\ CommitHeldClear THEN "t1" ELSE "none"]
OldExecutionEnds ==
    /\ s.phase = "cleared" /\ s.row = "held"
    /\ s' = [s EXCEPT !.row = "retired"]

\* The actual router consumes a volatile memory flag; after restart it may
\* supply an old DB selector again. Only the launch's durable fence decides fresh.
LoadSelector ==
    /\ (s.phase = "load"
        \/ (s.phase = "cleared" /\ s.row = "retired")
        \/ (s.phase = "ready" /\ ClearResult = "none"))
    /\ s' = [s EXCEPT !.candidate = IF s.clearedMemory THEN "none" ELSE s.selector,
        !.clearedMemory = FALSE, !.captured = "none", !.phase = "fence"]
ReadFence ==
    /\ s.phase = "fence"
    /\ s' = [s EXCEPT !.captured = IF FenceLaunch THEN s.fence ELSE "none",
        !.phase = IF FenceLaunch /\ s.fence # "none" /\ ~FilterAfterJudge
                  THEN "select" ELSE "judge"]
Judge ==
    /\ s.phase = "judge"
    /\ s' = [s EXCEPT !.phase = IF s.nativeVerified THEN "select" ELSE "repin",
        !.candidate = IF s.nativeVerified THEN s.native ELSE @]
\* Seq21 is this synchronous pin_retired_resume write during selection, not
\* a late hook. Filtering its result after judging cannot undo the journal write.
RepinRetired ==
    /\ s.phase = "repin"
    /\ s' = [s EXCEPT !.nativeVerified = TRUE, !.journalWrites = @ + 1,
        !.unsafeRepins = @ + (IF s.captured # "none" THEN 1 ELSE 0),
        !.candidate = s.native, !.phase = "select"]
Selected == IF s.captured # "none" THEN Fresh(s.turn)
            ELSE IF s.candidate # "none" THEN s.candidate ELSE Fresh(s.turn)
SelectedResume == s.captured = "none" /\ s.candidate # "none"
Select ==
    /\ s.phase = "select"
    /\ s' = [s EXCEPT !.candidate = Selected, !.resume = SelectedResume,
        !.phase = "launch"]
NotSent ==
    /\ s.phase = "launch" /\ s.turn = 1 /\ ~s.retried
    /\ s' = [s EXCEPT !.phase = "load", !.retried = TRUE,
        !.candidate = "none", !.captured = "none"]
LaunchRecord == [epoch |-> s.epoch, first |-> s.firstPending,
                 session |-> s.candidate, resume |-> s.resume]
Launch ==
    /\ s.phase = "launch" /\ s.turn = 1
    /\ s' = [s EXCEPT !.lastLaunch = LaunchRecord, !.firstPending = FALSE,
        !.firstSession = s.candidate, !.phase = "write-selector"]
Launch2 ==
    /\ s.phase = "launch" /\ s.turn = 2
    /\ s' = [s EXCEPT !.lastLaunch = LaunchRecord, !.firstPending = FALSE,
        !.phase = "write-selector"]
\* Provider startup can rewrite the selector while the fence is still active.
LaunchWritesSelector ==
    /\ s.phase = "write-selector"
    /\ s' = [s EXCEPT !.selector = s.candidate,
        !.phase = IF s.turn = 1 THEN "verify" ELSE "done"]
Verified == s' = [s EXCEPT !.native = s.candidate, !.nativeVerified = TRUE,
    !.verifiedSession = s.candidate, !.expectedResume = s.candidate, !.phase = "release"]
FreshVerified == s.phase = "verify" /\ ~s.resume /\ Verified
ResumeVerified == s.phase = "verify" /\ s.resume /\ Verified

\* A verified fresh SessionStart releases only the token this launch captured.
Release ==
    /\ s.phase = "release" /\ ~RaceClear
    /\ s' = [s EXCEPT !.fence = IF ~NoRelease /\ s.fence = s.captured
                                  THEN "none" ELSE @, !.phase = "released"]
ClearAgain ==
    /\ s.phase = "release" /\ RaceClear /\ s.epoch = 1
    /\ s' = [s EXCEPT !.epoch = 2, !.fence = "t2", !.selector = "none",
        !.clearedMemory = TRUE, !.firstPending = TRUE,
        !.clearedSession = s.verifiedSession]
WrongTokenRelease ==
    /\ s.phase = "release" /\ RaceClear /\ s.epoch = 2
    /\ s' = [s EXCEPT !.fence = IF ReleaseWithoutToken THEN "none" ELSE @,
        !.wrongReleaseChanged = ReleaseWithoutToken /\ s.captured # s.fence,
        !.phase = "released"]
\* Fork/compact/continuation after release follows the existing native binding;
\* no permanent allowlist may force it back to the first verified session.
Rollover ==
    /\ s.phase = "released" /\ s.fence = "none" /\ ~RaceClear /\ ~s.rolledOver
    /\ s' = [s EXCEPT !.native = "rolled", !.selector = "rolled",
        !.expectedResume = "rolled", !.rolledOver = TRUE]

DcRestart ==
    /\ ~s.restarted1 /\ s.turn = 1
    /\ s.phase \in {"ready", "cleared", "load", "fence", "judge", "repin", "select", "launch"}
    /\ s' = [s EXCEPT !.restarted1 = TRUE, !.clearedMemory = FALSE,
        !.candidate = "none", !.captured = "none", !.phase = "down1"]
Boot ==
    /\ s.phase = "down1"
    /\ s' = [s EXCEPT !.phase = IF s.epoch = 0 THEN "ready" ELSE "cleared"]
SecondRestart ==
    /\ s.phase = "released"
    /\ s' = [s EXCEPT !.turn = 2, !.phase = "down2", !.clearedMemory = FALSE,
        !.candidate = "none", !.captured = "none"]
Boot2 == s.phase = "down2" /\ s' = [s EXCEPT !.phase = "load"]
VisibleSelector == IF FenceLaunch /\ s.fence # "none" THEN "none" ELSE s.selector
ReadApi ==
    /\ s.phase \notin {"down1", "down2"}
    /\ s' = [s EXCEPT !.lastApi = VisibleSelector,
        !.apiLeak = @ \/ (s.fence # "none" /\ VisibleSelector # "none")
                       \/ (s.epoch > 0 /\ s.verifiedSession = "none"
                           /\ VisibleSelector = s.clearedSession)]
Done == s.phase = "done" /\ UNCHANGED vars
Next == ClearRetired \/ ClearHeld \/ OldExecutionEnds \/ LoadSelector \/ ReadFence
    \/ Judge \/ RepinRetired \/ Select \/ NotSent \/ Launch \/ Launch2
    \/ LaunchWritesSelector \/ FreshVerified \/ ResumeVerified \/ Release \/ ClearAgain
    \/ WrongTokenRelease \/ Rollover \/ DcRestart \/ Boot \/ SecondRestart \/ Boot2
    \/ ReadApi \/ Done

ClearNeverResumesPrior ==
    (s.lastLaunch.first /\ s.lastLaunch.epoch = s.epoch /\ s.epoch > 0) =>
        s.lastLaunch.session # s.clearedSession /\ ~s.lastLaunch.resume
FreshRestartKeepsSession ==
    (s.phase = "done" /\ s.epoch = 1 /\ s.firstSession = "fresh1") =>
        s.lastLaunch.session = s.expectedResume /\ s.lastLaunch.resume
UnclearedRestartKeepsResume ==
    (s.phase = "done" /\ s.epoch = 0) =>
        s.lastLaunch.session = s.expectedResume /\ s.lastLaunch.resume
WrongTokenReleaseNoEffect == ~s.wrongReleaseChanged
NoRepinUnderFence == s.unsafeRepins = 0
ApiSelectorDoesNotExposePrior == ~s.apiLeak
====
