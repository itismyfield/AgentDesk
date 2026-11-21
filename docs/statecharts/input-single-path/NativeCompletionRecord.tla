-------------------- MODULE NativeCompletionRecord --------------------
\* Completion records of a native-owned turn: transcript, scheduled outcome, work, cleanup order.
EXTENDS Naturals
CONSTANTS Mode, Scenario

VARIABLES outcome, row, cleared, stage, ended, recorded, rows, scope, sched, schedWrites,
          afterClear, deleted, acked, booted, crashed, nativeWork, watcherWork, registry
vars == <<outcome, row, cleared, stage, ended, recorded, rows, scope, sched, schedWrites,
          afterClear, deleted, acked, booted, crashed, nativeWork, watcherWork, registry>>

Primary == Scenario # "Follower"
EmptyBody == Scenario = "Empty"
Readable == Scenario # "Unreadable"
WriteOk == Scenario # "WriteFail"
ClearScenario == Scenario = "ClearBeforeRecord"
\* Work dispatch A belongs to the turn; its row lacks the id when the intake lookup failed.
DispatchTurn == Scenario \in {"Normal", "WatcherRace", "CrashAfterRecord", "NoDispatchId",
                              "PromptOnlyId", "FirstMissLateHit", "RegistryContended"}
\* Intake asks twice: before the prompt and before the row. A first hit pins the row to it;
\* a first miss leaves the row to the later lookup, as the base did.
\* PromptOnlyId: the first lookup finds A and the later one fails.
\* FirstMissLateHit: the first lookup errs (also an early retry) and the later one finds A.
RowHasId == CASE Scenario = "NoDispatchId" -> FALSE
              [] Scenario = "PromptOnlyId" -> Mode # "TwoLookups"
              [] Scenario = "FirstMissLateHit" -> Mode \notin {"SingleDecision", "EarlyRetry"}
              [] OTHER -> TRUE
\* A Herdr watcher resumes past the closed turn's range and never reads it.
WatcherReads == Scenario \notin {"PromptOnlyId", "FirstMissLateHit"}
\* Native custody holds the primary row from registration; a watcher pass may also see its turn.
WatcherScenario == Scenario = "WatcherRace"
WatcherMayClear == WatcherScenario /\ (~Primary \/ Mode = "NoWatcherClearGate")
\* The watcher keeps the work of a row without a dispatch id, as before native recording.
WatcherMayComplete == \/ WatcherScenario /\ (~Primary \/ Mode = "NoWatcherWorkGate")
                      \/ ~RowHasId /\ WatcherReads /\ Mode # "GateAll"
Expected == IF EmptyBody THEN "EmptyResponse" ELSE "Delivered"

Init == /\ outcome = "None" /\ row = TRUE /\ cleared = FALSE
        /\ stage = "Pending" /\ ended = FALSE /\ recorded = FALSE
        /\ rows = 0 /\ scope = "None" /\ sched = "None" /\ schedWrites = 0
        /\ afterClear = FALSE /\ deleted = FALSE /\ acked = FALSE
        /\ booted = TRUE /\ crashed = FALSE
        /\ nativeWork = FALSE /\ watcherWork = FALSE /\ registry = "Free"

Commit == /\ booted /\ outcome = "None"
          /\ outcome' = IF Scenario = "Cancelled" THEN "Cancelled" ELSE "Executed"
          /\ UNCHANGED <<row, cleared, stage, ended, recorded, rows, scope, sched, schedWrites,
                         afterClear, deleted, acked, booted, crashed, nativeWork, watcherWork, registry>>

AdjacentClear == /\ ClearScenario /\ row /\ ~deleted /\ stage = "Pending"
                 /\ row' = FALSE /\ cleared' = TRUE
                 /\ UNCHANGED <<outcome, stage, ended, recorded, rows, scope, sched, schedWrites,
                                afterClear, deleted, acked, booted, crashed, nativeWork, watcherWork, registry>>

\* The watcher's terminal-commit epilogue clears the row it re-read by identity and nonce.
WatcherClear == /\ WatcherMayClear /\ booted /\ row /\ ~deleted
                /\ row' = FALSE /\ deleted' = TRUE
                /\ UNCHANGED <<outcome, cleared, stage, ended, recorded, rows, scope, sched, schedWrites,
                               afterClear, acked, booted, crashed, nativeWork, watcherWork, registry>>

\* The watcher's streaming finalizer completes a work dispatch with its own body. Without a row
\* id that is the pre-existing path, whose body choice this model leaves out of scope.
WatcherCompleteWork == /\ WatcherMayComplete /\ booted /\ outcome = "Executed" /\ ~watcherWork
                       /\ watcherWork' = TRUE
                       /\ scope' = IF RowHasId THEN "Foreign" ELSE scope
                       /\ UNCHANGED <<outcome, row, cleared, stage, ended, recorded, rows, sched,
                                      schedWrites, afterClear, deleted, acked, booted, crashed,
                                      nativeWork, registry>>

\* Live and boot share one rule: the current matching row, never a stale capture.
RowAvailable == row \/ (Mode = "CapturedFallback" /\ cleared)
Eligible == Primary /\ outcome = "Executed" /\ RowAvailable /\ Mode # "NoRecord"
            /\ ~(Mode = "SecondRegistryRead" /\ registry = "Held")
SessionMapReady == ~crashed
WritesText == Eligible /\ Readable /\ WriteOk /\ ~EmptyBody
              /\ (Scenario # "BootEmptySessionMap" \/ SessionMapReady \/ Mode # "MemorySessionOnly")
SettlesScheduled == Eligible /\ Readable /\ WriteOk /\ (Mode # "EmptyGate" \/ ~EmptyBody)
\* Native completes only the dispatch its row names.
CompletesWork == Eligible /\ Readable /\ ~EmptyBody /\ DispatchTurn
                 /\ (RowHasId \/ Mode = "NativeWithoutId")

Record == /\ booted /\ outcome # "None" /\ stage = "Pending" /\ ~acked
          /\ (Scenario # "BootEmptySessionMap" \/ crashed)
          /\ (Scenario # "RegistryContended" \/ registry = "Held")
          /\ stage' = "Done" /\ ended' = TRUE
          /\ rows' = IF WritesText THEN 1 ELSE rows
          /\ scope' = IF WritesText
                       THEN (IF Mode = "RangeStartBeforeRun" THEN "Foreign" ELSE "Exact")
                       ELSE scope
          /\ sched' = IF SettlesScheduled /\ sched = "None" THEN Expected ELSE sched
          /\ schedWrites' = IF SettlesScheduled /\ sched = "None" THEN schedWrites + 1 ELSE schedWrites
          /\ recorded' = (recorded \/ (Eligible /\ Readable /\ WriteOk))
          /\ afterClear' = (afterClear \/ (Eligible /\ cleared))
          \* A re-run sees its own earlier completion; a second writer is counted separately.
          /\ nativeWork' = (nativeWork \/ CompletesWork)
          /\ UNCHANGED <<outcome, row, cleared, deleted, acked, booted, crashed, watcherWork, registry>>

DeleteRow == /\ booted /\ outcome # "None" /\ row /\ registry # "Held"
             /\ (stage = "Done" \/ Mode = "NoRecordOrdering")
             /\ row' = FALSE /\ deleted' = TRUE
             /\ UNCHANGED <<outcome, cleared, stage, ended, recorded, rows, scope, sched,
                            schedWrites, afterClear, acked, booted, crashed, nativeWork, watcherWork, registry>>

Ack == /\ booted /\ outcome # "None" /\ ~row /\ ~acked
       /\ (stage = "Done" \/ Mode = "NoRecordOrdering")
       /\ acked' = TRUE
       /\ UNCHANGED <<outcome, row, cleared, stage, ended, recorded, rows, scope, sched,
                      schedWrites, afterClear, deleted, booted, crashed, nativeWork, watcherWork, registry>>

\* A crash loses the in-memory stage; boot replays Record from the durable result.
Crash == /\ ~crashed /\ booted /\ ~acked
         /\ ((Scenario = "BootEmptySessionMap" /\ outcome = "Executed" /\ stage = "Pending")
             \/ (Scenario = "CrashAfterRecord" /\ stage = "Done" /\ row)
             \/ (Scenario = "CrashAfterDelete" /\ deleted))
         /\ crashed' = TRUE /\ booted' = FALSE /\ stage' = "Pending"
         /\ UNCHANGED <<outcome, row, cleared, ended, recorded, rows, scope, sched, schedWrites,
                        afterClear, deleted, acked, nativeWork, watcherWork, registry>>

Boot == /\ crashed /\ ~booted /\ booted' = TRUE
        /\ UNCHANGED <<outcome, row, cleared, stage, ended, recorded, rows, scope, sched,
                       schedWrites, afterClear, deleted, acked, crashed, nativeWork, watcherWork, registry>>

\* A foreign publication holds the shared history lock across the optional recorder read.
HoldRegistry == /\ Scenario = "RegistryContended" /\ outcome = "Executed" /\ registry = "Free"
                /\ registry' = "Held"
                /\ UNCHANGED <<outcome, row, cleared, stage, ended, recorded, rows, scope, sched,
                               schedWrites, afterClear, deleted, acked, booted, crashed, nativeWork, watcherWork>>

ReleaseRegistry == /\ registry = "Held" /\ stage = "Done" /\ registry' = "Released"
                   /\ UNCHANGED <<outcome, row, cleared, stage, ended, recorded, rows, scope, sched,
                                  schedWrites, afterClear, deleted, acked, booted, crashed, nativeWork, watcherWork>>

Next == Commit \/ AdjacentClear \/ WatcherClear \/ WatcherCompleteWork \/ Record \/ DeleteRow
        \/ Ack \/ Crash \/ Boot \/ HoldRegistry \/ ReleaseRegistry
Spec == Init /\ [][Next]_vars /\ WF_vars(Commit) /\ WF_vars(Record) /\ WF_vars(DeleteRow)
        /\ WF_vars(Ack) /\ WF_vars(Boot) /\ WF_vars(WatcherCompleteWork)
        /\ WF_vars(Crash) /\ WF_vars(HoldRegistry) /\ WF_vars(ReleaseRegistry)

TranscriptRowAtMostOne == rows <= 1
\* Transcript and work completion are both written from the run's own body only.
NoForeignBody == scope # "Foreign"
NoTranscriptForEmpty == EmptyBody => rows = 0
ScheduledAtMostOnce == schedWrites <= 1
ScheduledMatchesBody == sched # "None" => sched = Expected
NoRecordForCancelledOrFollower == (outcome = "Cancelled" \/ ~Primary) => (rows = 0 /\ sched = "None")
NoResurrectionAfterClear == ~afterClear
NoCleanupBeforeRecordStage == deleted => ended
WorkCount == (IF nativeWork THEN 1 ELSE 0) + (IF watcherWork THEN 1 ELSE 0)
\* Exactly one writer per turn: native for a row with an id, the watcher for one without.
WorkAtMostOnce == WorkCount <= 1

CleanupEventuallyEnds == outcome # "None" ~> acked
ScheduledEventuallySettles == (outcome = "Executed" /\ Primary /\ Readable /\ WriteOk /\ ~ClearScenario)
                              ~> sched = Expected
TranscriptEventuallyRecorded == (outcome = "Executed" /\ Primary /\ Readable /\ WriteOk
                                 /\ ~ClearScenario /\ ~EmptyBody) ~> rows = 1
WorkEventuallyCompleted == (outcome = "Executed" /\ DispatchTurn) ~> WorkCount >= 1
=============================================================================
