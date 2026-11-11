------------------------- MODULE HostedCleanup -------------------------
EXTENDS Naturals, TLC
CONSTANT Boundary, Recheck
VARIABLES phase, route, row, snapshot, marker, seen, held, markerWritten,
          rowChanged, restarted, failed, staged, earlierExists, deleted,
          markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart
vars == <<phase, route, row, snapshot, marker, seen, held, markerWritten,
          rowChanged, restarted, failed, staged, earlierExists, deleted,
          markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
SafeMarkers == {"absent", "tmux"}
Records == {"legacy", "retired", "bound", "unknown", "foreign"}
RecordAllows(r) == r.record \in {"legacy", "retired"}
ObservationAllows(r, m) == RecordAllows(r) /\ (r.record # "legacy" \/ m \in SafeMarkers)
MarkerFor(r) == IF marker \notin SafeMarkers THEN marker
                ELSE IF r.alias = 1 THEN aliasMarker ELSE marker
SameJudgedRow == row.generation = snapshot.generation /\ row.record = snapshot.record
                /\ (route # "explicit" \/ row.alias = snapshot.alias)
Init ==
    /\ Boundary \in {"DB", "service"}
    /\ phase = "start"
    /\ route \in {"bulk", "explicit"}
    /\ row \in [generation : {0}, record : Records, status : {"disconnected"}, alias : {0}]
    /\ snapshot = row
    /\ marker \in {"absent", "tmux", "non-tmux", "read-failed", "unknown"}
    /\ seen = "none"
    /\ held \in BOOLEAN
    /\ markerWritten = FALSE /\ rowChanged = FALSE
    /\ restarted = FALSE /\ failed = FALSE
    /\ staged = FALSE /\ earlierExists = TRUE /\ deleted = FALSE
    /\ markerAtDelete = "none" /\ checkedGeneration = 0 /\ sawWaitMarker = FALSE
    /\ lastOutcome = "none" /\ aliasMarker = "absent" /\ earlierAtStart = TRUE
ReadCandidate ==
    /\ earlierAtStart' = earlierExists
    /\ phase = "start"
    /\ snapshot' = row
    /\ phase' = IF route = "bulk" /\ row.status # "disconnected" THEN "done" ELSE "candidate"
    /\ UNCHANGED <<route, row, marker, seen, held, markerWritten, rowChanged,
         restarted, failed, staged, earlierExists, deleted, markerAtDelete,
         checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker>>
ObserveMarker ==
    /\ phase = "candidate"
    /\ seen' = (IF snapshot.record = "legacy" THEN MarkerFor(snapshot) ELSE "not-read")
    /\ phase' = IF ObservationAllows(snapshot, MarkerFor(snapshot))
                 THEN IF held THEN "waiting" ELSE IF route = "explicit" THEN "postwait" ELSE "ready"
                 ELSE "done"
    /\ UNCHANGED <<route, row, snapshot, marker, held, markerWritten, rowChanged,
         restarted, failed, staged, earlierExists, deleted, markerAtDelete,
         checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
WriteMarkerDuringWait ==
    /\ phase = "waiting" /\ held /\ ~markerWritten
    /\ marker' \in {"non-tmux", "read-failed", "unknown"}
    /\ markerWritten' = TRUE
    /\ UNCHANGED <<phase, route, row, snapshot, seen, held, rowChanged,
         restarted, failed, staged, earlierExists, deleted, markerAtDelete,
         checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
NewTurnOrRespawn ==
    /\ phase \in {"candidate", "waiting", "postwait", "ready"} /\ ~rowChanged
    /\ row' \in {[row EXCEPT !.status = "active"],
                  [row EXCEPT !.generation = 1, !.record = "bound"]}
    /\ rowChanged' = TRUE
    /\ UNCHANGED <<phase, route, snapshot, marker, seen, held, markerWritten,
         restarted, failed, staged, earlierExists, deleted, markerAtDelete,
         checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
AddAlias ==
    /\ phase \in {"candidate", "waiting", "postwait", "ready"}
    /\ row.alias = 0
    /\ row' = [row EXCEPT !.alias = 1]
    /\ aliasMarker' \in {"non-tmux", "read-failed", "unknown"}
    /\ UNCHANGED <<phase, route, snapshot, marker, seen, held, markerWritten,
         rowChanged, restarted, failed, staged, earlierExists, deleted,
         markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, earlierAtStart>>
ReleasePriorLock ==
    /\ phase = "waiting" /\ held
    /\ held' = FALSE
    /\ phase' = IF route = "explicit" THEN "postwait" ELSE "ready"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, markerWritten, rowChanged,
         restarted, failed, staged, earlierExists, deleted, markerAtDelete,
         checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
ObserveAfterWait ==
    /\ phase = "postwait"
    /\ seen' = (IF snapshot.record = "legacy" THEN MarkerFor(row) ELSE "not-read") /\ sawWaitMarker' = (markerWritten /\ row.record = "legacy")
    /\ phase' = IF SameJudgedRow /\ ObservationAllows(row, MarkerFor(row)) THEN "ready" ELSE "done"
    /\ UNCHANGED <<route, row, snapshot, marker, held, markerWritten, rowChanged,
         restarted, failed, staged, earlierExists, deleted, markerAtDelete,
         checkedGeneration, lastOutcome, aliasMarker, earlierAtStart>>
ConditionalApply ==
    /\ phase = "ready" /\ ~held
    /\ checkedGeneration' = IF Recheck THEN row.generation ELSE snapshot.generation
    /\ staged' = IF Recheck THEN SameJudgedRow /\ RecordAllows(row)
                    /\ (route # "bulk" \/ row.status = "disconnected")
                  ELSE RecordAllows(snapshot)
    /\ phase' = "applied"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, held, markerWritten,
         rowChanged, restarted, failed, earlierExists, deleted, markerAtDelete,
         sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
Commit ==
    /\ lastOutcome' = "commit"
    /\ phase = "applied"
    /\ deleted' = staged
    /\ markerAtDelete' = IF staged THEN MarkerFor(row) ELSE "none"
    /\ earlierExists' = FALSE
    /\ phase' = "done"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, held, markerWritten,
         rowChanged, restarted, failed, staged, checkedGeneration, sawWaitMarker, aliasMarker, earlierAtStart>>
Rollback ==
    /\ lastOutcome' = "rollback"
    /\ phase = "applied" /\ ~failed
    /\ failed' = TRUE /\ staged' = FALSE /\ phase' = "done"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, held, markerWritten,
         rowChanged, restarted, earlierExists, deleted, markerAtDelete,
         checkedGeneration, sawWaitMarker, aliasMarker, earlierAtStart>>
Restart ==
    /\ phase \in {"candidate", "waiting", "postwait", "ready", "applied"} /\ ~restarted
    /\ restarted' = TRUE /\ staged' = FALSE /\ phase' = "start"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, held, markerWritten,
         rowChanged, failed, earlierExists, deleted, markerAtDelete,
         checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
Retry ==
    /\ phase = "done" /\ ~deleted /\ row.generation = 0
    /\ ObservationAllows(row, MarkerFor(row)) /\ (route # "bulk" \/ row.status = "disconnected")
    /\ phase' = "start"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, held, markerWritten,
         rowChanged, restarted, failed, staged, earlierExists, deleted,
         markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
Done == phase = "done" /\ UNCHANGED vars
Next == ReadCandidate \/ ObserveMarker \/ WriteMarkerDuringWait \/ NewTurnOrRespawn
        \/ AddAlias \/ ReleasePriorLock \/ ObserveAfterWait \/ ConditionalApply \/ Commit
        \/ Rollback \/ Restart \/ Retry \/ Done
Fairness == WF_vars(ReadCandidate) /\ WF_vars(ObserveMarker)
            /\ WF_vars(ReleasePriorLock) /\ WF_vars(ObserveAfterWait)
            /\ WF_vars(ConditionalApply) /\ WF_vars(Commit) /\ WF_vars(Retry)
Spec == Init /\ [][Next]_vars /\ Fairness
\* Current policy protects Legacy markers as observed; Known Retired skips marker I/O.
LegacyObservedMarkerSafe == ~deleted \/ snapshot.record # "legacy" \/ seen \in SafeMarkers
KnownRetiredSkipsMarker == ~deleted \/ snapshot.record # "retired" \/ seen = "not-read"
UnknownAndUnownedKept == ~deleted \/ snapshot.record \notin {"unknown", "foreign", "bound"}
ExplicitLockWaitMarkerNotMissed == ~deleted \/ route # "explicit"
    \/ snapshot.record # "legacy" \/ ~markerWritten \/ sawWaitMarker
ChangedRowRechecked == ~deleted \/ checkedGeneration = row.generation
ExplicitAliasRechecked == ~deleted \/ route # "explicit" \/ row.alias = snapshot.alias
BulkDisconnectedRechecked == ~deleted \/ route # "bulk" \/ row.status = "disconnected"
RollbackKeepsEarlier == lastOutcome = "rollback" => earlierExists = earlierAtStart
\* Characterization only: these stronger rules expose gaps retained by this move.
LegacyCurrentMarkerSafe == ~deleted \/ snapshot.record # "legacy" \/ markerAtDelete \in SafeMarkers
BulkLockWaitMarkerNotMissed == ~deleted \/ route # "bulk"
    \/ snapshot.record # "legacy" \/ ~markerWritten \/ sawWaitMarker
AllAliasesRechecked == ~deleted \/ row.alias = snapshot.alias
ExplicitActivePreserved == ~deleted \/ route # "explicit" \/ row.status # "active"
StableTarget == row.generation = 0 /\ ObservationAllows(row, MarkerFor(row))
    /\ (route # "bulk" \/ row.status = "disconnected")
EligibleEventuallyDeleted == (<>[]StableTarget) => <>deleted

\* Thread GC retains the same SQL authority and observation order at both boundaries.
\* Source baseline: 2ddece1ebb2de1f66c4263c4bc29239b36250ec3.
GCRecordAllows(r) == r.record \in {"legacy", "retired"}
GCSqlEligible(r) == r.thread /\ r.status = "idle" /\ ~r.dispatch
    /\ ~r.children /\ ~r.openChild /\ r.stale
GCOccupancyAllows(r) == r.status = "idle" /\ ~r.dispatch /\ ~r.children /\ ~r.openChild
GCInit ==
    /\ Boundary \in {"DB", "service"}
    /\ phase = "start" /\ route = "thread-gc"
    /\ row \in [generation : {0}, record : Records \cup {"lookup-failed", "unreadable", "missing"},
        status : {"idle"}, alias : {0}, dispatch : {FALSE}, children : {FALSE},
        openChild : {FALSE}, stale : {TRUE}, thread : {TRUE}, owner : {"local", "foreign", "unreadable"}]
    /\ snapshot = row
    /\ marker \in {"absent", "tmux", "non-tmux", "read-failed", "unknown"}
    /\ seen = [host |-> FALSE, marker |-> "none", occupancy |-> "none", owner |-> "none", probe |-> "none"]
    /\ held = FALSE /\ markerWritten = FALSE /\ rowChanged = FALSE
    /\ restarted = FALSE /\ failed = FALSE /\ staged = FALSE
    /\ earlierExists = TRUE /\ deleted = FALSE /\ markerAtDelete = "none"
    /\ checkedGeneration = 0 /\ sawWaitMarker = FALSE /\ lastOutcome = "none"
    /\ aliasMarker \in {"Missing", "Present", "ProbeFailed"} /\ earlierAtStart = TRUE
GCReadCandidate ==
    /\ phase = "start"
    /\ snapshot' = row
    /\ phase' = IF GCSqlEligible(row) /\ row.record \notin {"lookup-failed", "missing"} THEN "candidate" ELSE "done"
    /\ UNCHANGED <<route, row, marker, seen, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCObserveHost ==
    /\ phase = "candidate"
    /\ seen' = [seen EXCEPT !.host = GCRecordAllows(snapshot) /\ (snapshot.record # "legacy" \/ marker \in SafeMarkers), !.marker = IF snapshot.record = "legacy" THEN marker ELSE "not-read"]
    /\ phase' = IF GCRecordAllows(snapshot) /\ (snapshot.record # "legacy" \/ marker \in SafeMarkers) THEN "host-ok" ELSE "done"
    /\ UNCHANGED <<route, row, snapshot, marker, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCObserveOccupancy ==
    /\ phase = "host-ok"
    /\ lastOutcome = "none"
    /\ lastOutcome' = "occupancy-read"
    /\ seen' = [seen EXCEPT !.occupancy = IF GCOccupancyAllows(row) THEN "unoccupied" ELSE "occupied"]
    /\ phase' = IF GCOccupancyAllows(row) THEN "occupancy-ok" ELSE "done"
    /\ UNCHANGED <<route, row, snapshot, marker, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, aliasMarker, earlierAtStart>>
GCOccupancyLookupFailed ==
    /\ phase = "host-ok"
    /\ lastOutcome = "none"
    /\ lastOutcome' = "lookup-failed"
    /\ seen' = [seen EXCEPT !.occupancy = "lookup-failed"]
    /\ phase' = "done"
    /\ UNCHANGED <<route, row, snapshot, marker, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, aliasMarker, earlierAtStart>>
GCObserveOwner ==
    /\ phase = "occupancy-ok"
    /\ seen' = [seen EXCEPT !.owner = row.owner]
    /\ phase' = IF row.owner = "local" THEN "owner-ok" ELSE "done"
    /\ UNCHANGED <<route, row, snapshot, marker, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCProbe ==
    /\ phase = "owner-ok"
    /\ seen' = [seen EXCEPT !.probe = aliasMarker]
    /\ phase' = IF aliasMarker = "Missing" THEN "ready" ELSE "done"
    /\ UNCHANGED <<route, row, snapshot, marker, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCNewTurn ==
    /\ phase \in {"host-ok", "occupancy-ok", "owner-ok", "ready"}
    /\ ~rowChanged
    /\ row' \in {[row EXCEPT !.status = "active"], [row EXCEPT !.dispatch = TRUE], [row EXCEPT !.children = TRUE], [row EXCEPT !.openChild = TRUE], [row EXCEPT !.stale = FALSE], [row EXCEPT !.generation = 1, !.record = "bound"]}
    /\ rowChanged' = TRUE
    /\ sawWaitMarker' = TRUE
    /\ UNCHANGED <<phase, route, snapshot, marker, seen, held, markerWritten, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, lastOutcome, aliasMarker, earlierAtStart>>
GCRowProjectionChanges ==
    /\ phase \in {"host-ok", "occupancy-ok", "owner-ok", "ready"}
    /\ ~rowChanged
    /\ row' \in {[row EXCEPT !.generation = 1], [row EXCEPT !.generation = 1, !.record = "unknown"], [row EXCEPT !.generation = 1, !.record = "foreign"], [row EXCEPT !.thread = FALSE]}
    /\ rowChanged' = TRUE
    /\ UNCHANGED <<phase, route, snapshot, marker, seen, held, markerWritten, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCKillFailed ==
    /\ phase \in {"host-ok", "occupancy-ok", "owner-ok"}
    /\ ~failed
    /\ failed' = TRUE
    /\ aliasMarker' \in {"Present", "ProbeFailed"}
    /\ UNCHANGED <<phase, route, row, snapshot, marker, seen, held, markerWritten, rowChanged, restarted, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, earlierAtStart>>
GCMarkerChanges ==
    /\ phase \in {"host-ok", "occupancy-ok", "owner-ok", "ready"}
    /\ ~markerWritten
    /\ marker' = "non-tmux"
    /\ markerWritten' = TRUE
    /\ UNCHANGED <<phase, route, row, snapshot, seen, held, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCOwnerChanges ==
    /\ phase = "ready"
    /\ row.owner = "local"
    /\ row' = [row EXCEPT !.owner = "foreign"]
    /\ UNCHANGED <<phase, route, snapshot, marker, seen, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCAliasAdded ==
    /\ phase \in {"host-ok", "occupancy-ok", "owner-ok", "ready"}
    /\ row.alias = 0
    /\ row' = [row EXCEPT !.alias = 1]
    /\ UNCHANGED <<phase, route, snapshot, marker, seen, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCConditionalDelete ==
    /\ phase = "ready"
    /\ checkedGeneration' = IF Recheck THEN row.generation ELSE snapshot.generation
    /\ staged' = IF Recheck THEN SameJudgedRow /\ GCRecordAllows(row) /\ GCSqlEligible(row) ELSE GCRecordAllows(snapshot)
    /\ phase' = "applied"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, held, markerWritten, rowChanged, restarted, failed, earlierExists, deleted, markerAtDelete, sawWaitMarker, lastOutcome, aliasMarker, earlierAtStart>>
GCAutocommit ==
    /\ phase = "applied"
    /\ deleted' = staged
    /\ markerAtDelete' = IF staged THEN marker ELSE "none"
    /\ lastOutcome' = "commit"
    /\ phase' = "done"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, held, markerWritten, rowChanged, restarted, failed, staged, earlierExists, checkedGeneration, sawWaitMarker, aliasMarker, earlierAtStart>>
GCStatementFailure ==
    /\ phase = "applied"
    /\ staged' = FALSE
    /\ lastOutcome' = "rollback"
    /\ phase' = "done"
    /\ UNCHANGED <<route, row, snapshot, marker, seen, held, markerWritten, rowChanged, restarted, failed, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, aliasMarker, earlierAtStart>>
GCRestart ==
    /\ phase \in {"candidate", "host-ok", "occupancy-ok", "owner-ok", "ready", "applied"}
    /\ ~restarted
    /\ phase' = "start"
    /\ restarted' = TRUE
    /\ staged' = FALSE
    /\ lastOutcome' = "none"
    /\ seen' = [host |-> FALSE, marker |-> "none", occupancy |-> "none", owner |-> "none", probe |-> "none"]
    /\ UNCHANGED <<route, row, snapshot, marker, held, markerWritten, rowChanged, failed, earlierExists, deleted, markerAtDelete, checkedGeneration, sawWaitMarker, aliasMarker, earlierAtStart>>
GCNext == GCReadCandidate \/ GCObserveHost \/ GCObserveOccupancy \/ GCOccupancyLookupFailed
    \/ GCObserveOwner \/ GCProbe \/ GCNewTurn \/ GCRowProjectionChanges \/ GCKillFailed \/ GCMarkerChanges
    \/ GCOwnerChanges \/ GCAliasAdded \/ GCConditionalDelete \/ GCAutocommit
    \/ GCStatementFailure \/ GCRestart \/ Done
GCSpec == GCInit /\ [][GCNext]_vars
GCNewTurnKept == ~deleted \/ ~sawWaitMarker
GCUnknownLookupKept == ~deleted \/ snapshot.record \notin {"unknown", "foreign", "bound", "lookup-failed", "unreadable", "missing"}
GCObservedHostBeforeProbe == seen.probe = "none" \/ seen.host
GCObservedLegacyMarkerSafe == ~deleted \/ snapshot.record # "legacy" \/ seen.marker \in SafeMarkers
GCObservedMissingAndLocal == ~deleted \/ (seen.host /\ seen.occupancy = "unoccupied" /\ seen.owner = "local" /\ seen.probe = "Missing")
GCKillFailureKept == ~deleted \/ ~failed
GCConditionalRefusalsKept == ~deleted \/ (SameJudgedRow /\ GCRecordAllows(row) /\ GCSqlEligible(row))
\* Stronger characterizations expose existing filesystem/alias gaps; no new fence is proposed.
GCCurrentLegacyMarkerSafe == ~deleted \/ snapshot.record # "legacy" \/ markerAtDelete \in SafeMarkers
GCCurrentOwnerLocal == ~deleted \/ row.owner = "local"
GCAliasesRechecked == ~deleted \/ row.alias = snapshot.alias
=============================================================================
