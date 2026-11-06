--------------------------- MODULE WriterRebound ---------------------------
EXTENDS Naturals, Sequences, FiniteSets
CONSTANT LogCurrent
VARIABLES scenario, phase, presence, log, checkpoint, cursors, birth, rotation,
          missing, readable, continuous, view, viewTarget, viewTail, parked,
          writer, alarm, ready, decided, viewValid, required, lateParkedBind,
          retired, frontier, prefixHash, spool
vars == <<scenario,phase,presence,log,checkpoint,cursors,birth,rotation,
          missing,readable,continuous,view,viewTarget,viewTail,parked,
          writer,alarm,ready,decided,viewValid,required,lateParkedBind,
          retired,frontier,prefixHash,spool>>
\* Code baseline: 7a0c819e127964befd6bc0d022c5d4194d669c9c + park implementation.
\* LogCurrent selects the reviewed Current-of-full-log contract, not a new format.
Sources == {"A", "B", "C"}
Cases == {"cycle", "interrupted", "laterC", "orphan", "unapplied",
          "unappliedMissing", "pending", "resolved", "unreadable",
          "gapped", "noCheckpoint", "noBound", "parkRebind", "parkRefine"}
Max(S) == CHOOSE n \in S : \A m \in S : m <= n
Min(S) == CHOOSE n \in S : \A m \in S : n <= m
RawTarget(i) ==
    IF log[i] = "pending"
    THEN IF "resolveB" \in {log[j] : j \in 1..Len(log)} THEN "B" ELSE "unresolved"
    ELSE IF log[i] \in Sources THEN log[i] ELSE "none"
Bounds == {i \in 1..Len(log) : RawTarget(i) \in Sources}
LastBound == IF Bounds = {} THEN "unknown" ELSE RawTarget(Max(Bounds))
Unresolved == {i \in 1..Len(log) : RawTarget(i) = "unresolved"}
SuffixPending == IF checkpoint = 0 THEN {} ELSE
                 {i \in Unresolved : i > checkpoint}
Current == IF ~readable \/ ~continuous \/ checkpoint = 0
               \/ Bounds = {} \/ SuffixPending # {}
           THEN "unknown" ELSE LastBound
\* An unresolved Pending stops follow with Ok/wait; later binds are not applicable yet.
RequiredThrough == IF SuffixPending # {} THEN Min(SuffixPending) - 1 ELSE Len(log)
BirthHistorical(s) == rotation[s] # "none"
                      \/ \E t \in cursors : birth[t] > birth[s]
Historical(s, c) == IF LogCurrent THEN c # "unknown" /\ c # s
                    ELSE BirthHistorical(s)
Allowed(s, c) == Historical(s, c) \/ presence = "absent"
NeedsOpen == {RawTarget(i) : i \in {j \in Bounds : j <= required
                           /\ (checkpoint = 0 \/ j > checkpoint)
                           /\ RawTarget(j) \notin cursors}}
HasMissingApply == (NeedsOpen \ missing) # NeedsOpen
Evidence == <<retired,frontier,prefixHash,spool>>
OriginalEvidence == <<[s \in Sources |-> FALSE],
                      [s \in Sources |-> 17],
                      [s \in Sources |-> "prefix"],
                      [s \in Sources |-> <<"retained", "bytes">>]>>
Init == /\ scenario \in Cases /\ presence \in {"present", "unknown", "absent"}
        /\ phase = "adopted" /\ log = <<"A">> /\ checkpoint = 1
        /\ cursors = {"A"} /\ birth = [s \in Sources |-> 0]
        /\ rotation = [s \in Sources |-> "none"] /\ missing = {}
        /\ readable = TRUE /\ continuous = TRUE
        /\ view = "unknown" /\ viewTarget = "unknown" /\ viewTail = 0
        /\ parked = {} /\ writer = "booting" /\ alarm = FALSE /\ ready = FALSE
        /\ decided = FALSE /\ viewValid = FALSE /\ required = 1
        /\ lateParkedBind = FALSE
        /\ retired = [s \in Sources |-> FALSE]
        /\ frontier = [s \in Sources |-> 17]
        /\ prefixHash = [s \in Sources |-> "prefix"]
        /\ spool = [s \in Sources |-> <<"retained", "bytes">>]
BindB == /\ phase = "adopted" /\ scenario \in {"cycle", "interrupted", "laterC"}
         /\ log' = Append(log, "B") /\ checkpoint' = 2
         /\ cursors' = {"A", "B"} /\ birth' = [birth EXCEPT !["B"] = 2]
         /\ rotation' = [rotation EXCEPT !["A"] = "B"] /\ phase' = "boundB"
         /\ UNCHANGED <<scenario,presence,missing,readable,continuous,view,viewTarget,
                         viewTail,parked,writer,alarm,ready,decided,viewValid,required,
                         lateParkedBind,retired,frontier,prefixHash,spool>>
SaveRebindRotation == /\ phase = "boundB" /\ phase' = "rotationSaved"
                     /\ log' = Append(log, "A")
                     /\ rotation' = [rotation EXCEPT !["A"] = "none", !["B"] = "A"]
                     /\ UNCHANGED <<scenario,presence,checkpoint,cursors,birth,missing,
                                     readable,continuous,view,viewTarget,viewTail,parked,
                                     writer,alarm,ready,decided,viewValid,required,
                                     lateParkedBind,retired,frontier,prefixHash,spool>>
CommitCheckpoint == /\ phase = "rotationSaved" /\ scenario # "interrupted"
                    /\ checkpoint' = 3
                    /\ phase' = (IF scenario = "laterC" THEN "reboundA" ELSE "prepared")
                    /\ UNCHANGED <<scenario,presence,log,cursors,birth,rotation,missing,
                                    readable,continuous,view,viewTarget,viewTail,parked,
                                    writer,alarm,ready,decided,viewValid,required,
                                    lateParkedBind,retired,frontier,prefixHash,spool>>
InterruptAfterRotation == /\ phase = "rotationSaved" /\ scenario = "interrupted"
                         /\ phase' = "prepared"
                         /\ UNCHANGED <<scenario,presence,log,checkpoint,cursors,birth,
                                         rotation,missing,readable,continuous,view,viewTarget,
                                         viewTail,parked,writer,alarm,ready,decided,viewValid,
                                         required,lateParkedBind,retired,frontier,prefixHash,spool>>
BindC == /\ phase = "reboundA" /\ phase' = "prepared"
         /\ log' = Append(log, "C") /\ checkpoint' = 4
         /\ cursors' = Sources /\ birth' = [birth EXCEPT !["C"] = 4]
         /\ rotation' = [rotation EXCEPT !["A"] = "C"]
         /\ UNCHANGED <<scenario,presence,missing,readable,continuous,view,viewTarget,
                         viewTail,parked,writer,alarm,ready,decided,viewValid,required,
                         lateParkedBind,retired,frontier,prefixHash,spool>>
PrepareOther == /\ phase = "adopted" /\ scenario \notin {"cycle", "interrupted", "laterC"}
                /\ phase' = "prepared"
                /\ log' = (CASE scenario = "orphan" -> <<"A", "B", "C">>
                              [] scenario = "pending" -> <<"A", "pending", "C">>
                              [] scenario = "resolved" -> <<"A", "pending", "C", "resolveB">>
                              [] scenario = "noBound" -> <<"rejected">>
                              [] OTHER -> <<"A", "B">>)
                /\ checkpoint' = (CASE scenario = "noCheckpoint" -> 0
                                     [] scenario \in {"unapplied", "unappliedMissing", "pending", "resolved", "noBound"} -> 1
                                     [] scenario = "orphan" -> 3
                                     [] OTHER -> 2)
                /\ cursors' = (IF scenario \in {"unapplied", "unappliedMissing", "resolved"}
                              THEN {"A"} ELSE IF scenario \in {"orphan", "pending"}
                              THEN Sources ELSE {"A", "B"})
                /\ birth' = (IF scenario \in {"unapplied", "unappliedMissing", "resolved"}
                            THEN birth ELSE [birth EXCEPT !["B"] = 2,
                                  !["C"] = IF scenario \in {"orphan", "pending"} THEN 3 ELSE 0])
                /\ rotation' = (IF scenario \in {"orphan", "parkRebind", "parkRefine"}
                               THEN [rotation EXCEPT !["A"] = "B"] ELSE rotation)
                /\ readable' = (scenario # "unreadable") /\ continuous' = (scenario # "gapped")
                /\ UNCHANGED <<scenario,presence,missing,view,viewTarget,viewTail,parked,
                                writer,alarm,ready,decided,viewValid,required,
                                lateParkedBind,retired,frontier,prefixHash,spool>>
LoseSource == /\ phase = "prepared" /\ phase' = "missing"
              /\ missing' \in (IF scenario = "laterC" THEN {{"A"}, {"B"}, {"C"}}
                              ELSE IF scenario = "unappliedMissing" THEN {{"A", "B"}}
                              ELSE {{"A"}})
              /\ UNCHANGED <<scenario,presence,log,checkpoint,cursors,birth,rotation,
                              readable,continuous,view,viewTarget,viewTail,parked,
                              writer,alarm,ready,decided,viewValid,required,
                              lateParkedBind,retired,frontier,prefixHash,spool>>
ResumeMissing == /\ phase = "missing"
                 /\ view' = Current /\ viewTarget' = LastBound /\ viewTail' = Len(log)
                 /\ required' = (IF checkpoint = 0 THEN Len(log) ELSE RequiredThrough)
                 /\ parked' = {s \in missing \cap cursors : Allowed(s, Current)}
                 /\ LET safe == \A s \in missing \cap cursors : Allowed(s, Current)
                    IN /\ phase' = (IF ~safe THEN "checked" ELSE IF LogCurrent THEN "following" ELSE "checked")
                       /\ writer' = (IF ~safe THEN "halted" ELSE IF LogCurrent THEN "booting" ELSE "running")
                       /\ ready' = (safe /\ ~LogCurrent) /\ alarm' = ~safe
                 /\ decided' = (~LogCurrent \/ writer' = "halted") /\ viewValid' = TRUE
                 /\ UNCHANGED <<scenario,presence,log,checkpoint,cursors,birth,rotation,missing,
                                 readable,continuous,lateParkedBind,retired,frontier,prefixHash,spool>>
FollowBeforeReady == /\ phase = "following" /\ phase' = "checked"
                     /\ decided' = TRUE
                     /\ writer' = (IF HasMissingApply THEN "halted" ELSE "running")
                     /\ ready' = ~HasMissingApply /\ alarm' = HasMissingApply
                     /\ checkpoint' = (IF HasMissingApply THEN checkpoint ELSE required)
                     /\ cursors' = (IF HasMissingApply THEN cursors ELSE cursors \cup NeedsOpen)
                     /\ UNCHANGED <<scenario,presence,log,birth,rotation,missing,readable,continuous,
                                     view,viewTarget,viewTail,parked,viewValid,required,
                                     lateParkedBind,retired,frontier,prefixHash,spool>>
BindParkedAfterView == /\ scenario = "parkRebind" /\ phase = "checked" /\ ready /\ "A" \in parked
                      /\ phase' = "lateBind" /\ log' = Append(log, "A")
                      /\ lateParkedBind' = TRUE /\ decided' = FALSE /\ viewValid' = FALSE
                      /\ writer' = (IF LogCurrent THEN "halted" ELSE "running")
                      /\ alarm' = LogCurrent /\ ready' = ~LogCurrent
                      /\ checkpoint' = (IF LogCurrent THEN checkpoint ELSE Len(log) + 1)
                      /\ UNCHANGED <<scenario,presence,cursors,birth,rotation,missing,readable,continuous,
                                      view,viewTarget,viewTail,parked,required,retired,frontier,prefixHash,spool>>
RefineParkedWithinView == /\ scenario = "parkRefine" /\ phase \in {"following", "checked"}
                         /\ (phase = "following" \/ ready) /\ "A" \in parked
                         /\ phase' = "lateRefine" /\ lateParkedBind' = TRUE
                         /\ decided' = FALSE /\ viewValid' = FALSE
                         /\ writer' = (IF LogCurrent THEN "halted" ELSE "running")
                         /\ alarm' = LogCurrent /\ ready' = ~LogCurrent
                         /\ UNCHANGED <<scenario,presence,log,checkpoint,cursors,birth,rotation,
                                         missing,readable,continuous,view,viewTarget,viewTail,
                                         parked,required,retired,frontier,prefixHash,spool>>
Rest == /\ phase \in {"checked", "lateBind", "lateRefine"} /\ UNCHANGED vars
Next == BindB \/ SaveRebindRotation \/ CommitCheckpoint \/ InterruptAfterRotation
        \/ BindC \/ PrepareOther \/ LoseSource \/ ResumeMissing \/ FollowBeforeReady
        \/ BindParkedAfterView \/ RefineParkedWithinView \/ Rest
ActiveLossVisible == (decided /\ viewTarget \in missing /\ presence \in {"present", "unknown"})
                     => (viewTarget \notin parked /\ writer = "halted" /\ alarm /\ ~ready)
CurrentNeverHistorical == (viewValid /\ viewTarget \in Sources)
                          => ~Historical(viewTarget, view)
UnknownNeedsAbsence == (decided /\ view = "unknown" /\ parked # {}) => presence = "absent"
ReadyAfterFollow == ready => checkpoint # 0 /\ checkpoint >= required
ParkedReboundHalts == lateParkedBind => writer = "halted" /\ alarm /\ ~ready
ParkPreservesEvidence == Evidence = OriginalEvidence
=============================================================================
