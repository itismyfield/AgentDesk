------------------------- MODULE WriterArchivedSource -------------------------
EXTENDS Naturals
CONSTANTS RetireArchive, IgnoreDrained
VARIABLES fd, end, pending, archive, phase, retired, ready, restarted, attached
vars == <<fd, end, pending, archive, phase, retired, ready, restarted, attached>>

\* Cursor and a previously saved EOF witness both cover byte 1. Only the FD
\* can distinguish a tail appended after that witness, before archival.
Init == /\ fd \in BOOLEAN /\ end = 1 /\ pending \in BOOLEAN
        /\ archive = FALSE /\ phase = "present" /\ retired = FALSE
        /\ ready = FALSE /\ restarted = FALSE /\ attached = FALSE
Grow == /\ phase = "present" /\ end = 1 /\ end' = 2
        /\ UNCHANGED <<fd,pending,archive,phase,retired,ready,restarted,attached>>
Archive == /\ phase = "present" /\ archive' \in BOOLEAN /\ phase' = "missing"
           /\ UNCHANGED <<fd,end,pending,retired,ready,restarted,attached>>
Drained == end = 1 /\ ~pending
CanRetire == RetireArchive /\ archive /\ fd /\ (IgnoreDrained \/ Drained)
Classify == /\ phase = "missing" /\ phase' = "classified"
            /\ retired' = (retired \/ CanRetire)
            /\ ready' = (retired \/ CanRetire)
            /\ UNCHANGED <<fd,end,pending,archive,restarted,attached>>
Restart == /\ phase = "classified" /\ ~restarted
           /\ fd' = FALSE /\ phase' = "missing" /\ ready' = FALSE
           /\ restarted' = TRUE
           /\ UNCHANGED <<end,pending,archive,retired,attached>>
AttachNew == /\ phase = "classified" /\ ready /\ ~attached
             /\ attached' = TRUE
             /\ UNCHANGED <<fd,end,pending,archive,phase,retired,ready,restarted>>
Rest == UNCHANGED vars
Next == Grow \/ Archive \/ Classify \/ Restart \/ AttachNew \/ Rest
NoUnsentTailLost == retired => Drained
ArchiveAtVerifiedEofRetires ==
    (phase = "classified" /\ archive /\ fd /\ Drained) => retired
UnprovenBootHolds ==
    (phase = "classified" /\ ~fd /\ ~retired) => ~ready
NewSourceNeedsReadiness == attached => retired
=============================================================================
