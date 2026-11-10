------------------------- MODULE WriterArchivedSource -------------------------
EXTENDS Naturals
CONSTANTS RetireArchive, IgnoreDrained, AllowHistoricalPark
VARIABLES fd, end, pending, archive, phase, retired, ready, restarted, attached,
          newBound, retried, parked, attachPermit
vars == <<fd,end,pending,archive,phase,retired,ready,restarted,attached,
          newBound,retried,parked,attachPermit>>

\* Cursor and a prior EOF witness cover byte 1; only the retained FD sees growth.
Init == /\ fd \in BOOLEAN /\ end = 1 /\ pending \in BOOLEAN
        /\ archive = FALSE /\ phase = "present" /\ retired = FALSE
        /\ ready = FALSE /\ restarted = FALSE /\ attached = FALSE
        /\ newBound = FALSE /\ retried = FALSE /\ parked = FALSE /\ attachPermit = FALSE
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
Next == Grow \/ Archive \/ Classify \/ Restart \/ BindNew \/ ResumeBound \/ AttachNew \/ Rest
Spec == Init /\ [][Next]_vars /\ WF_vars(Classify) /\ WF_vars(ResumeBound) /\ WF_vars(AttachNew)
NoUnsentTailLost == retired => Drained
ArchiveAtVerifiedEofRetires ==
    (phase = "classified" /\ archive /\ fd /\ Drained) => retired
UnprovenBootHolds ==
    (phase = "classified" /\ ~fd /\ ~retired /\ ~newBound) => ~ready
NewSourceNeedsReadiness == attached => (attachPermit /\ newBound)
HistoricalParkIsNotRetirement == parked => (newBound /\ ~retired /\ ~pending)
NewBindingProgress == newBound ~> attached
=============================================================================
