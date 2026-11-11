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
=============================================================================
