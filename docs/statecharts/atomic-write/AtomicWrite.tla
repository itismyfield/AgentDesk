------------------------- MODULE AtomicWrite -------------------------
EXTENDS Naturals, TLC
CONSTANT DropObservation
VARIABLES oldTarget, pc, temp, target, published, returned, returnedStage, ioError,
          enospcRecorded, diagnosed, failAt, errno, platform, inflight,
          parentSync
vars == <<oldTarget, pc, temp, target, published, returned, returnedStage, ioError,
          enospcRecorded, diagnosed, failAt, errno, platform, inflight,
          parentSync>>
Stages == {"CreateDir", "CreateTemp", "Write", "FileSync", "Rename"}
Init ==
    /\ oldTarget \in {"Old", "Absent"}
    /\ pc = "CreateDir" /\ temp = "Absent" /\ target = oldTarget
    /\ published = FALSE /\ returned = "Pending" /\ returnedStage = "None"
    /\ ioError = "None" /\ enospcRecorded = FALSE /\ diagnosed = FALSE
    /\ failAt \in Stages \cup {"None"} /\ errno \in {"Other", "ENOSPC"}
    /\ platform \in {"Unix", "Windows"} /\ inflight \in BOOLEAN
    /\ parentSync = "NotRequested"
Fail ==
    /\ pc \in Stages /\ failAt = pc
    /\ pc' = "Done" /\ returned' = "Err" /\ returnedStage' = pc
    /\ ioError' = errno
    /\ temp' = IF pc = "Write" THEN "Partial" ELSE temp
    /\ enospcRecorded' = (errno = "ENOSPC" /\ ~DropObservation)
    /\ UNCHANGED <<oldTarget, target, published, diagnosed, failAt, errno,
                    platform, inflight, parentSync>>
CreateDir ==
    /\ pc = "CreateDir" /\ failAt # pc /\ pc' = "CreateTemp"
    /\ UNCHANGED <<oldTarget, temp, target, published, returned, returnedStage, ioError,
                    enospcRecorded, diagnosed, failAt, errno, platform, inflight, parentSync>>
CreateTemp ==
    /\ pc = "CreateTemp" /\ failAt # pc /\ pc' = "Write" /\ temp' = "Empty"
    /\ UNCHANGED <<oldTarget, target, published, returned, returnedStage, ioError,
                    enospcRecorded, diagnosed, failAt, errno, platform, inflight, parentSync>>
Write ==
    /\ pc = "Write" /\ failAt # pc /\ pc' = "FileSync" /\ temp' = "Complete"
    /\ UNCHANGED <<oldTarget, target, published, returned, returnedStage, ioError,
                    enospcRecorded, diagnosed, failAt, errno, platform, inflight, parentSync>>
FileSync ==
    /\ pc = "FileSync" /\ failAt # pc /\ pc' = "Observe" /\ temp' = "Synced"
    /\ UNCHANGED <<oldTarget, target, published, returned, returnedStage, ioError,
                    enospcRecorded, diagnosed, failAt, errno, platform, inflight, parentSync>>
NeedDiagnostic == inflight /\ oldTarget = "Old"
Observe ==
    /\ pc = "Observe" /\ pc' = "Rename" /\ diagnosed' = NeedDiagnostic
    /\ UNCHANGED <<oldTarget, temp, target, published, returned, returnedStage, ioError,
                    enospcRecorded, failAt, errno, platform, inflight, parentSync>>
Rename ==
    /\ pc = "Rename" /\ failAt # pc /\ pc' = "Done"
    /\ target' = "New" /\ temp' = "Absent" /\ published' = TRUE
    /\ returned' = "Ok" /\ returnedStage' = "Rename"
    /\ UNCHANGED <<oldTarget, ioError, enospcRecorded, diagnosed, failAt, errno,
                    platform, inflight, parentSync>>
Done == pc = "Done" /\ UNCHANGED vars
Next == Fail \/ CreateDir \/ CreateTemp \/ Write \/ FileSync \/ Observe \/ Rename \/ Done
OldPreserved == ~published => target = oldTarget
NoPartialPublication == target \in {"Old", "Absent", "New"} /\ (published => target = "New")
OutcomeMatchesPublication ==
    /\ (returned = "Ok" <=> published)
    /\ (returned = "Err" => ~published /\ returnedStage = failAt /\ ioError = errno)
DiskObservation ==
    /\ (returned = "Err" /\ ioError = "ENOSPC" => enospcRecorded)
    /\ ((pc = "Rename" \/ returnedStage = "Rename") /\ NeedDiagnostic => diagnosed)
DirectorySemantics == parentSync = "NotRequested"
DirectoryCapability == IF platform = "Windows" THEN "OkButUnflushed" ELSE "Flushes"
======================================================================
