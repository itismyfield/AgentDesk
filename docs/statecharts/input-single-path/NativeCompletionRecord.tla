-------------------- MODULE NativeCompletionRecord --------------------
\* Completion records of a native-owned turn: transcript, scheduled outcome, cleanup order.
EXTENDS Naturals
CONSTANTS Mode, Scenario

VARIABLES outcome, row, cleared, stage, ended, recorded, rows, scope, sched, schedWrites,
          afterClear, deleted, acked, booted, crashed
vars == <<outcome, row, cleared, stage, ended, recorded, rows, scope, sched, schedWrites,
          afterClear, deleted, acked, booted, crashed>>

Primary == Scenario # "Follower"
EmptyBody == Scenario = "Empty"
Readable == Scenario # "Unreadable"
WriteOk == Scenario # "WriteFail"
ClearScenario == Scenario = "ClearBeforeRecord"
Expected == IF EmptyBody THEN "EmptyResponse" ELSE "Delivered"

Init == /\ outcome = "None" /\ row = TRUE /\ cleared = FALSE
        /\ stage = "Pending" /\ ended = FALSE /\ recorded = FALSE
        /\ rows = 0 /\ scope = "None" /\ sched = "None" /\ schedWrites = 0
        /\ afterClear = FALSE /\ deleted = FALSE /\ acked = FALSE
        /\ booted = TRUE /\ crashed = FALSE

Commit == /\ booted /\ outcome = "None"
          /\ outcome' = IF Scenario = "Cancelled" THEN "Cancelled" ELSE "Executed"
          /\ UNCHANGED <<row, cleared, stage, ended, recorded, rows, scope, sched, schedWrites,
                         afterClear, deleted, acked, booted, crashed>>

AdjacentClear == /\ ClearScenario /\ row /\ ~deleted /\ stage = "Pending"
                 /\ row' = FALSE /\ cleared' = TRUE
                 /\ UNCHANGED <<outcome, stage, ended, recorded, rows, scope, sched, schedWrites,
                                afterClear, deleted, acked, booted, crashed>>

\* Live and boot share one rule: the current matching row, never a stale capture.
RowAvailable == row \/ (Mode = "CapturedFallback" /\ cleared)
Eligible == Primary /\ outcome = "Executed" /\ RowAvailable /\ Mode # "NoRecord"
WritesText == Eligible /\ Readable /\ WriteOk /\ ~EmptyBody
SettlesScheduled == Eligible /\ Readable /\ WriteOk /\ (Mode # "EmptyGate" \/ ~EmptyBody)

Record == /\ booted /\ outcome # "None" /\ stage = "Pending" /\ ~acked
          /\ stage' = "Done" /\ ended' = TRUE
          /\ rows' = IF WritesText THEN 1 ELSE rows
          /\ scope' = IF WritesText
                       THEN (IF Mode = "RangeStartBeforeRun" THEN "Foreign" ELSE "Exact")
                       ELSE scope
          /\ sched' = IF SettlesScheduled /\ sched = "None" THEN Expected ELSE sched
          /\ schedWrites' = IF SettlesScheduled /\ sched = "None" THEN schedWrites + 1 ELSE schedWrites
          /\ recorded' = (recorded \/ (Eligible /\ Readable /\ WriteOk))
          /\ afterClear' = (afterClear \/ (Eligible /\ cleared))
          /\ UNCHANGED <<outcome, row, cleared, deleted, acked, booted, crashed>>

DeleteRow == /\ booted /\ outcome # "None" /\ row
             /\ (stage = "Done" \/ Mode = "NoRecordOrdering")
             /\ row' = FALSE /\ deleted' = TRUE
             /\ UNCHANGED <<outcome, cleared, stage, ended, recorded, rows, scope, sched,
                            schedWrites, afterClear, acked, booted, crashed>>

Ack == /\ booted /\ outcome # "None" /\ ~row /\ ~acked
       /\ (stage = "Done" \/ Mode = "NoRecordOrdering")
       /\ acked' = TRUE
       /\ UNCHANGED <<outcome, row, cleared, stage, ended, recorded, rows, scope, sched,
                      schedWrites, afterClear, deleted, booted, crashed>>

\* A crash loses the in-memory stage; boot replays Record from the durable result.
Crash == /\ ~crashed /\ booted /\ ~acked
         /\ ((Scenario = "CrashAfterRecord" /\ stage = "Done" /\ row)
             \/ (Scenario = "CrashAfterDelete" /\ deleted))
         /\ crashed' = TRUE /\ booted' = FALSE /\ stage' = "Pending"
         /\ UNCHANGED <<outcome, row, cleared, ended, recorded, rows, scope, sched, schedWrites,
                        afterClear, deleted, acked>>

Boot == /\ crashed /\ ~booted /\ booted' = TRUE
        /\ UNCHANGED <<outcome, row, cleared, stage, ended, recorded, rows, scope, sched,
                       schedWrites, afterClear, deleted, acked, crashed>>

Next == Commit \/ AdjacentClear \/ Record \/ DeleteRow \/ Ack \/ Crash \/ Boot
Spec == Init /\ [][Next]_vars /\ WF_vars(Commit) /\ WF_vars(Record) /\ WF_vars(DeleteRow)
        /\ WF_vars(Ack) /\ WF_vars(Boot)

TranscriptRowAtMostOne == rows <= 1
NoForeignBody == rows = 1 => scope = "Exact"
NoTranscriptForEmpty == EmptyBody => rows = 0
ScheduledAtMostOnce == schedWrites <= 1
ScheduledMatchesBody == sched # "None" => sched = Expected
NoRecordForCancelledOrFollower == (outcome = "Cancelled" \/ ~Primary) => (rows = 0 /\ sched = "None")
NoResurrectionAfterClear == ~afterClear
NoCleanupBeforeRecordStage == deleted => ended

CleanupEventuallyEnds == outcome # "None" ~> acked
ScheduledEventuallySettles == (outcome = "Executed" /\ Primary /\ Readable /\ WriteOk /\ ~ClearScenario)
                              ~> sched = Expected
TranscriptEventuallyRecorded == (outcome = "Executed" /\ Primary /\ Readable /\ WriteOk
                                 /\ ~ClearScenario /\ ~EmptyBody) ~> rows = 1
=============================================================================
