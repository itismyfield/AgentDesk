------------------------- MODULE WriterArchivedSource -------------------------
EXTENDS Naturals
CONSTANTS RetireArchive, IgnoreDrained, AllowHistoricalPark, BootDesign, BootFault
VARIABLES fd, end, pending, archive, phase, retired, ready, restarted, attached,
          newBound, retried, parked, attachPermit, detail
vars == <<fd,end,pending,archive,phase,retired,ready,restarted,attached,
          newBound,retried,parked,attachPermit,detail>>

NeutralDetail == [entry |-> "legacy", case |-> "valid", real |-> FALSE,
                  phantom |-> FALSE, race |-> "none", changed |-> FALSE,
                  original |-> "missing", owner |-> "owned", proof |-> "none",
                  oldPosts |-> 1, newPosts |-> 0, replayed |-> FALSE, resumed |-> FALSE]

\* Cursor and a prior EOF witness cover byte 1; only the retained FD sees growth.
Init == /\ fd \in BOOLEAN /\ end = 1 /\ pending \in BOOLEAN
        /\ archive = FALSE /\ phase = "present" /\ retired = FALSE
        /\ ready = FALSE /\ restarted = FALSE /\ attached = FALSE
        /\ newBound = FALSE /\ retried = FALSE /\ parked = FALSE /\ attachPermit = FALSE
        /\ detail = NeutralDetail
Grow == /\ phase = "present" /\ end = 1 /\ end' = 2
        /\ UNCHANGED <<fd,pending,archive,phase,retired,ready,restarted,attached,
                        newBound,retried,parked,attachPermit>>
Archive == /\ phase = "present" /\ archive' \in BOOLEAN /\ phase' = "missing"
           /\ UNCHANGED <<fd,end,pending,retired,ready,restarted,attached,
                           newBound,retried,parked,attachPermit>>
Drained == end = 1 /\ ~pending
CanRetire == RetireArchive /\ archive /\ fd /\ (IgnoreDrained \/ Drained)
CanPark == AllowHistoricalPark /\ newBound /\ ~fd /\ ~pending /\ ~retired
Classify == /\ phase = "missing" /\ phase' = "classified"
            /\ retired' = (retired \/ CanRetire)
            /\ ready' = (retired \/ CanRetire \/ CanPark) /\ parked' = CanPark
            /\ UNCHANGED <<fd,end,pending,archive,restarted,attached,
                            newBound,retried,attachPermit>>
Restart == /\ phase = "classified" /\ ~restarted
           /\ fd' = FALSE /\ phase' = "missing" /\ ready' = FALSE
           /\ restarted' = TRUE /\ parked' = FALSE
           /\ UNCHANGED <<end,pending,archive,retired,attached,newBound,retried,attachPermit>>
\* The external valid B bind arrives after A halted; the host consumes it once.
BindNew == /\ phase = "classified" /\ archive /\ ~pending
           /\ ~newBound /\ newBound' = TRUE
           /\ UNCHANGED <<fd,end,pending,archive,phase,retired,ready,restarted,
                           attached,retried,parked,attachPermit>>
ResumeBound == /\ newBound /\ phase = "classified" /\ ~ready /\ ~retried
               /\ phase' = "missing" /\ fd' = FALSE /\ retried' = TRUE
               /\ UNCHANGED <<end,pending,archive,retired,ready,restarted,
                               attached,newBound,parked,attachPermit>>
AttachNew == /\ phase = "classified" /\ ready /\ newBound /\ ~attached
             /\ attached' = TRUE /\ attachPermit' = ready
             /\ UNCHANGED <<fd,end,pending,archive,phase,retired,ready,restarted,
                             newBound,retried,parked>>
Rest == UNCHANGED vars
Next == (Grow \/ Archive \/ Classify \/ Restart \/ BindNew \/ ResumeBound \/ AttachNew \/ Rest)
        /\ UNCHANGED detail
Spec == Init /\ [][Next]_vars /\ WF_vars(Classify /\ UNCHANGED detail)
        /\ WF_vars(ResumeBound /\ UNCHANGED detail) /\ WF_vars(AttachNew /\ UNCHANGED detail)
NoUnsentTailLost == retired => Drained
ArchiveAtVerifiedEofRetires ==
    (phase = "classified" /\ archive /\ fd /\ Drained) => retired
UnprovenBootHolds ==
    (phase = "classified" /\ ~fd /\ ~retired /\ ~newBound) => ~ready
NewSourceNeedsReadiness == attached => (attachPermit /\ newBound)
HistoricalParkIsNotRetirement == parked => (newBound /\ ~retired /\ ~pending)
NewBindingProgress == newBound ~> attached

\* Bounded boot cases come from reopen/resume, retained capture, promotion and ledger replay.
BootEntries == {"legacy_current_present", "legacy_current_absent", "configured",
                "unknown_current", "live", "standby_restart", "gate_reacquire", "manual_retired"}
BootCases == {"valid", "short", "hash", "extra", "partial", "decoder"}
BootInit ==
    /\ detail \in [entry : BootEntries, case : BootCases, real : BOOLEAN,
                    phantom : BOOLEAN, race : {"none", "original", "archive"},
                    changed : {FALSE}, original : {"missing", "present"},
                    owner : {"owned", "standby", "unknown"}, proof : {"none", "manual"},
                    oldPosts : {1}, newPosts : {0}, replayed : {FALSE}, resumed : {FALSE}]
    /\ pending \in BOOLEAN
    /\ (detail.entry = "manual_retired" =>
            detail.case = "valid" /\ ~detail.real /\ ~pending /\ detail.race = "none")
    /\ detail.original = IF detail.entry = "live" THEN "present" ELSE "missing"
    /\ detail.owner = CASE detail.entry = "standby_restart" -> "standby"
                         [] detail.entry = "gate_reacquire" -> "unknown"
                         [] OTHER -> "owned"
    /\ detail.proof = IF detail.entry = "manual_retired" THEN "manual" ELSE "none"
    /\ fd = (detail.entry = "live")
    /\ end = CASE detail.case = "short" -> 0
                  [] detail.case \in {"extra", "partial"} -> 2 [] OTHER -> 1
    /\ archive \in BOOLEAN
    /\ (detail.entry = "live" => ~archive)
    /\ phase = CASE detail.entry = "live" -> "present"
                    [] detail.owner = "standby" -> "waiting_owner" [] OTHER -> "booting"
    /\ retired = (detail.entry = "manual_retired")
    /\ ready = FALSE /\ restarted = FALSE /\ attached = FALSE
    /\ newBound = FALSE /\ retried = FALSE /\ parked = FALSE /\ attachPermit = FALSE

EffectiveUnsealed == detail.real \/ (detail.phantom /\ ~(BootDesign /\ BootFault # "phantom"))
BootNoDebt == (BootFault = "debt" \/ ~pending)
              /\ (BootFault = "unsealed" \/ ~EffectiveUnsealed)
FdGood == end = 1 /\ detail.case # "hash"
ArchiveGood == end = 1 /\ detail.case # "hash" /\ detail.case # "decoder"
ArchiveStable == detail.original = "missing" /\ ~detail.changed
OriginalPolicyAdmits == detail.entry \in {"configured", "legacy_current_absent"} \/ newBound
ArchiveChecks ==
    /\ (BootFault = "length" \/ end = 1)
    /\ (BootFault = "hash" \/ detail.case # "hash")
    /\ (BootFault = "decoder" \/ detail.case # "decoder")
CommitChecks == BootFault = "race" \/ ArchiveStable

BootArchive == /\ phase = "present" /\ archive' \in BOOLEAN /\ phase' = "booting"
               /\ detail' = [detail EXCEPT !.original = "missing"]
               /\ UNCHANGED <<fd,end,pending,retired,ready,restarted,attached,
                               newBound,retried,parked,attachPermit>>
BootPromote == /\ phase = "waiting_owner" /\ detail.owner = "standby"
              /\ phase' = "booting" /\ fd' = FALSE
              /\ detail' = [detail EXCEPT !.owner = "owned"]
              /\ UNCHANGED <<end,pending,archive,retired,ready,restarted,attached,
                              newBound,retried,parked,attachPermit>>
BootReacquire == /\ detail.owner = "unknown"
                /\ ready' = detail.resumed /\ detail' = [detail EXCEPT !.owner = "owned"]
                /\ UNCHANGED <<fd,end,pending,archive,phase,retired,restarted,attached,
                                newBound,retried,parked,attachPermit>>

BootCanFd == fd /\ archive /\ RetireArchive /\ FdGood /\ BootNoDebt
BootCanVerify == ~fd /\ BootDesign /\ archive /\ BootNoDebt
                 /\ (OriginalPolicyAdmits \/ BootFault # "reopen")
BootCanPark == ~fd /\ newBound /\ AllowHistoricalPark /\ ~pending /\ ~retired
               /\ (BootFault = "park" \/ detail.original = "missing")
BootClassify ==
    /\ phase = "booting"
    /\ IF retired THEN
           /\ phase' = "classified" /\ ready' = (detail.owner = "owned")
           /\ detail' = [detail EXCEPT !.resumed = TRUE] /\ UNCHANGED <<retired,parked>>
       ELSE IF BootCanFd THEN
           /\ phase' = "classified" /\ ready' = (detail.owner = "owned") /\ retired' = TRUE
           /\ detail' = [detail EXCEPT !.proof = "fd", !.resumed = TRUE] /\ UNCHANGED parked
       ELSE IF BootCanPark THEN
           /\ phase' = "classified" /\ ready' = (detail.owner = "owned") /\ parked' = TRUE
           /\ detail' = [detail EXCEPT !.resumed = TRUE] /\ UNCHANGED retired
       ELSE IF BootCanVerify THEN
           /\ phase' = "verifying" /\ ready' = FALSE /\ UNCHANGED <<retired,parked,detail>>
       ELSE /\ phase' = "classified" /\ ready' = FALSE /\ UNCHANGED <<retired,parked,detail>>
    /\ UNCHANGED <<fd,end,pending,archive,restarted,attached,newBound,retried,attachPermit>>
BootVerify == /\ phase = "verifying" /\ phase' = IF ArchiveChecks THEN "verified" ELSE "classified"
              /\ UNCHANGED <<fd,end,pending,archive,retired,ready,restarted,attached,
                              newBound,retried,parked,attachPermit,detail>>
BootCommit == /\ phase = "verified" /\ phase' = "classified"
              /\ retired' = (CommitChecks /\ BootNoDebt)
              /\ ready' = (retired' /\ detail.owner = "owned")
              /\ detail' = [detail EXCEPT !.proof = IF retired' THEN "archive" ELSE "none",
                                           !.resumed = retired']
              /\ UNCHANGED <<fd,end,pending,archive,restarted,attached,newBound,retried,
                              parked,attachPermit>>
BootOriginalReturns ==
    /\ phase \in {"verifying", "verified"} /\ detail.race = "original" /\ ~detail.changed
    /\ detail' = [detail EXCEPT !.original = "present", !.changed = TRUE]
    /\ UNCHANGED <<fd,end,pending,archive,phase,retired,ready,restarted,attached,
                    newBound,retried,parked,attachPermit>>
BootArchiveChanges ==
    /\ phase \in {"verifying", "verified"} /\ detail.race = "archive" /\ ~detail.changed
    /\ detail' = [detail EXCEPT !.changed = TRUE]
    /\ UNCHANGED <<fd,end,pending,archive,phase,retired,ready,restarted,attached,
                    newBound,retried,parked,attachPermit>>
BootRestart == /\ phase = "classified" /\ ~restarted
               /\ fd' = FALSE /\ phase' = "booting" /\ ready' = FALSE
               /\ restarted' = TRUE /\ parked' = FALSE
               /\ detail' = [detail EXCEPT !.replayed = FALSE, !.resumed = FALSE]
               /\ UNCHANGED <<end,pending,archive,retired,attached,newBound,retried,attachPermit>>
BootBindNew == /\ phase = "classified" /\ archive /\ ~pending /\ ~newBound
               /\ newBound' = TRUE
               /\ UNCHANGED <<fd,end,pending,archive,phase,retired,ready,restarted,
                               attached,retried,parked,attachPermit,detail>>
BootRetryBound == /\ newBound /\ phase = "classified" /\ ~ready /\ ~retried
                  /\ phase' = "booting" /\ fd' = FALSE /\ retried' = TRUE
                  /\ UNCHANGED <<end,pending,archive,retired,ready,restarted,attached,
                                  newBound,parked,attachPermit,detail>>
BootAttachNew == /\ phase = "classified" /\ ready /\ newBound /\ ~attached
                 /\ attached' = TRUE /\ attachPermit' = TRUE
                 /\ detail' = [detail EXCEPT !.newPosts = @ + 1]
                 /\ UNCHANGED <<fd,end,pending,archive,phase,retired,ready,restarted,
                                 newBound,retried,parked>>
BootReplayPosted == /\ ready /\ ~detail.replayed
                    /\ detail' = [detail EXCEPT !.replayed = TRUE,
                                    !.oldPosts = IF BootFault = "posted" THEN @ + 1 ELSE @]
                    /\ UNCHANGED <<fd,end,pending,archive,phase,retired,ready,restarted,
                                    attached,newBound,retried,parked,attachPermit>>
BootNext == BootArchive \/ BootPromote \/ BootReacquire \/ BootClassify \/ BootVerify
            \/ BootCommit \/ BootOriginalReturns \/ BootArchiveChanges \/ BootRestart
            \/ BootBindNew \/ BootRetryBound \/ BootAttachNew \/ BootReplayPosted \/ Rest
BootSpec == BootInit /\ [][BootNext]_vars /\ WF_vars(BootArchive)
            /\ WF_vars(BootPromote) /\ WF_vars(BootReacquire) /\ WF_vars(BootClassify)
            /\ WF_vars(BootVerify) /\ WF_vars(BootCommit) /\ WF_vars(BootRetryBound)
            /\ WF_vars(BootAttachNew) /\ WF_vars(BootReplayPosted)
NoArchiveObligationLost == retired =>
    /\ end = 1 /\ ~pending
    /\ detail.proof \in {"manual", "fd", "archive"}
    /\ (detail.proof = "fd" => FdGood)
    /\ (detail.proof = "archive" => archive /\ ArchiveGood /\ ArchiveStable)
NoRealUnsealedRetired == retired => ~detail.real
NoDuplicatePostedBodies == detail.oldPosts = 1 /\ detail.newPosts <= 1
NoHistoricalParkOverOriginal == parked => detail.original = "missing"
VerifiedArchiveOnlyProgress ==
    (archive /\ ~fd /\ ArchiveGood /\ detail.race = "none" /\ ~pending
     /\ ~detail.real /\ detail.owner = "owned") ~> ready
\* Reappearing/replaced paths retain the real reopen guard; old missing-only liveness stays in Spec.
BootNewBindingProgress == (newBound /\ detail.race # "original") ~> attached
=============================================================================
