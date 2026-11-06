-------------------------- MODULE WriterParkedAlias --------------------------
EXTENDS Naturals, Sequences
CONSTANT StableOldReference
VARIABLES caseName, phase, checkpoint, tail, confirmed, unique, aPresent,
          parked, ready, alarm, cursors, rawAtPrefix, referenceRole,
          bypassUsed, bindFinished, bValidated, posted, rehosts, halts,
          restarted, retiredA, frontierA, prefixA, spoolA
vars == <<caseName,phase,checkpoint,tail,confirmed,unique,aPresent,
          parked,ready,alarm,cursors,rawAtPrefix,referenceRole,
          bypassUsed,bindFinished,bValidated,posted,rehosts,halts,
          restarted,retiredA,frontierA,prefixA,spoolA>>
\* Old code: 34822f3bf14c35907b440dbe8c81f62ad167e2ba.
\* Fixed implementation: 1e2763de57d7932e1ae2585b9951169232c730d8.
\* Fixed main baseline: 0a2996a8c537234c1cdc8da8c0ce67a06f0f600c.
\* TRUE models the proposed repair: only an established parked old graph reference
\* may use its stored name without reopening; new/parent retain strict validation.
Cases == {"confirmedOld", "freshMissingB", "unseenOld", "otherSessionOld", "ambiguousOld", "confirmedParent",
          "parkedNew", "parkedNewReturned"}
LegitimateFresh == caseName = "confirmedOld"
QueuedBound == [old |-> IF referenceRole = "old" THEN
                           IF caseName = "otherSessionOld" THEN "A2" ELSE "A1"
                       ELSE IF referenceRole = "new" THEN "A0" ELSE "none",
                new |-> IF referenceRole = "new" THEN "A1" ELSE "B",
                parent |-> IF referenceRole = "parent" THEN "A1" ELSE "none",
                cause |-> IF referenceRole = "new" THEN "Resume" ELSE "Startup"]
MayReuseOld == StableOldReference /\ referenceRole = "old" /\ confirmed /\ unique /\ parked
              /\ QueuedBound.old = rawAtPrefix
StrictReferencePasses == unique /\ aPresent
FreshTargetExists == caseName # "freshMissingB"
NextHalt == IF halts < 3 THEN halts + 1 ELSE halts
EvidenceA == <<retiredA,frontierA,prefixA,spoolA>>
OriginalEvidenceA == <<FALSE,17,"stored-prefix",<<"retained", "raw-bytes">>>>
Init == /\ caseName \in Cases /\ phase = "storedEmpty" /\ checkpoint = 0 /\ tail = 0
        /\ confirmed = FALSE /\ unique = (caseName # "ambiguousOld")
        /\ aPresent = TRUE /\ parked = FALSE /\ ready = FALSE /\ alarm = FALSE
        /\ cursors = (IF caseName = "ambiguousOld" THEN {"A0", "A2"} ELSE {"A0"})
        /\ rawAtPrefix = "A0"
        /\ referenceRole = (IF caseName = "confirmedParent" THEN "parent"
                            ELSE IF caseName \in {"parkedNew", "parkedNewReturned"}
                            THEN "new" ELSE "old")
        /\ bypassUsed = FALSE /\ bindFinished = FALSE /\ bValidated = FALSE
        /\ posted = 0 /\ rehosts = 0 /\ halts = 0 /\ restarted = FALSE
        /\ retiredA = FALSE /\ frontierA = 17 /\ prefixA = "stored-prefix"
        /\ spoolA = <<"retained", "raw-bytes">>
ApplyEmptyBound == /\ phase = "storedEmpty" /\ phase' = "emptyBound"
                   /\ checkpoint' = 1 /\ tail' = 1
                   /\ UNCHANGED <<caseName,confirmed,unique,aPresent,parked,ready,alarm,
                                   cursors,rawAtPrefix,referenceRole,bypassUsed,bindFinished,
                                   bValidated,posted,rehosts,halts,restarted,
                                   retiredA,frontierA,prefixA,spoolA>>
\* A0 has session=""; A1 has session="A" at the same path/inode. Bytes still exist.
ApplyFilledName == /\ phase = "emptyBound" /\ phase' = "prefixApplied"
                   /\ checkpoint' = 2 /\ tail' = 2
                   /\ confirmed' = (caseName # "unseenOld")
                   /\ rawAtPrefix' = (IF caseName = "unseenOld" THEN "A0" ELSE "A1")
                   /\ UNCHANGED <<caseName,unique,aPresent,parked,ready,alarm,cursors,
                                   referenceRole,bypassUsed,bindFinished,bValidated,
                                   posted,rehosts,halts,restarted,retiredA,frontierA,prefixA,spoolA>>
LoseOriginal == /\ phase = "prefixApplied" /\ phase' = "missingA"
                /\ aPresent' = FALSE
                /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,parked,ready,alarm,
                                cursors,rawAtPrefix,referenceRole,bypassUsed,bindFinished,
                                bValidated,posted,rehosts,halts,restarted,
                                retiredA,frontierA,prefixA,spoolA>>
\* Producer absence and all other park guards hold; named current is stored A0.
ParkStoredA == /\ phase = "missingA" /\ phase' = "parkedReady"
               /\ parked' = TRUE /\ ready' = TRUE
               /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,aPresent,alarm,
                               cursors,rawAtPrefix,referenceRole,bypassUsed,bindFinished,
                               bValidated,posted,rehosts,halts,restarted,
                               retiredA,frontierA,prefixA,spoolA>>
ReturnOriginalForNew == /\ phase = "parkedReady" /\ caseName = "parkedNewReturned" /\ ~aPresent
                        /\ aPresent' = TRUE
                        /\ UNCHANGED <<caseName,phase,checkpoint,tail,confirmed,unique,
                                        parked,ready,alarm,cursors,rawAtPrefix,referenceRole,
                                        bypassUsed,bindFinished,bValidated,posted,rehosts,halts,
                                        restarted,retiredA,frontierA,prefixA,spoolA>>
\* The supported path records Bound(old=raw A1,new=B,cause=Startup), with B present.
QueueFreshBound == /\ phase = "parkedReady"
                   /\ (caseName # "parkedNewReturned" \/ aPresent)
                   /\ QueuedBound.new \in {"A1", "B"}
                   /\ phase' = "bindQueued" /\ tail' = 3
                   /\ UNCHANGED <<caseName,checkpoint,confirmed,unique,aPresent,parked,ready,
                                   alarm,cursors,rawAtPrefix,referenceRole,bypassUsed,
                                   bindFinished,bValidated,posted,rehosts,halts,restarted,
                                   retiredA,frontierA,prefixA,spoolA>>
ResolveRawReference == /\ phase = "bindQueued"
                       /\ bypassUsed' = MayReuseOld
                       /\ LET ok == MayReuseOld \/ StrictReferencePasses
                          IN /\ phase' = (IF ok THEN "referenceMapped" ELSE "halted")
                             /\ ready' = ok /\ alarm' = ~ok
                             /\ halts' = (IF ok THEN halts ELSE NextHalt)
                             /\ bindFinished' = ~ok
                       /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,aPresent,
                                       parked,cursors,rawAtPrefix,referenceRole,bValidated,
                                       posted,rehosts,restarted,retiredA,frontierA,prefixA,spoolA>>
\* New and parent keep strict naming. A parked new target still reaches the C6 halt.
AttachNewOrRejectParkedTarget == /\ phase = "referenceMapped"
                                /\ bindFinished' = TRUE
                                /\ LET newB == referenceRole # "new" /\ FreshTargetExists
                                   IN /\ phase' = (IF newB THEN "replyPending" ELSE "halted")
                                      /\ checkpoint' = (IF newB THEN 3 ELSE checkpoint)
                                      /\ cursors' = (IF newB THEN cursors \cup {"B"} ELSE cursors)
                                      /\ bValidated' = newB /\ ready' = newB /\ alarm' = ~newB
                                      /\ halts' = (IF newB THEN halts ELSE NextHalt)
                                /\ UNCHANGED <<caseName,tail,confirmed,unique,aPresent,parked,
                                                rawAtPrefix,referenceRole,bypassUsed,posted,rehosts,
                                                restarted,retiredA,frontierA,prefixA,spoolA>>
PostFirstReply == /\ phase = "replyPending" /\ posted = 0
                  /\ phase' = "settled" /\ posted' = 1
                  /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,aPresent,parked,
                                  ready,alarm,cursors,rawAtPrefix,referenceRole,bypassUsed,
                                  bindFinished,bValidated,rehosts,halts,restarted,
                                  retiredA,frontierA,prefixA,spoolA>>
DuplicateBindRefresh == /\ phase = "settled" /\ phase' = "duplicateChecked"
                        /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,aPresent,
                                        parked,ready,alarm,cursors,rawAtPrefix,referenceRole,
                                        bypassUsed,bindFinished,bValidated,posted,rehosts,halts,
                                        restarted,retiredA,frontierA,prefixA,spoolA>>
RestartAfterPosted == /\ phase = "duplicateChecked" /\ phase' = "restarting"
                      /\ restarted' = TRUE /\ parked' = FALSE /\ ready' = FALSE
                      /\ bypassUsed' = FALSE
                      /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,aPresent,alarm,
                                      cursors,rawAtPrefix,referenceRole,bindFinished,
                                      bValidated,posted,rehosts,halts,retiredA,frontierA,prefixA,spoolA>>
RestorePosted == /\ phase = "restarting" /\ phase' = "finished"
                  /\ parked' = TRUE /\ ready' = TRUE
                  /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,aPresent,alarm,
                                  cursors,rawAtPrefix,referenceRole,bypassUsed,bindFinished,
                                  bValidated,posted,rehosts,halts,restarted,
                                  retiredA,frontierA,prefixA,spoolA>>
\* Counts saturate for a finite graph; the failed host/rehost flow continues forever.
RehostAfterHalt == /\ LegitimateFresh /\ phase = "halted"
                   /\ phase' = "recovering"
                   /\ rehosts' = (IF rehosts < 2 THEN rehosts + 1 ELSE rehosts) /\ parked' = FALSE
                   /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,aPresent,ready,alarm,
                                   cursors,rawAtPrefix,referenceRole,bypassUsed,bindFinished,
                                   bValidated,posted,halts,restarted,retiredA,frontierA,prefixA,spoolA>>
ResumeParkAgain == /\ phase = "recovering" /\ phase' = "bindQueued" /\ parked' = TRUE
                   /\ bypassUsed' = FALSE /\ bindFinished' = FALSE
                   /\ UNCHANGED <<caseName,checkpoint,tail,confirmed,unique,aPresent,ready,alarm,
                                   cursors,rawAtPrefix,referenceRole,bValidated,posted,rehosts,
                                   halts,restarted,retiredA,frontierA,prefixA,spoolA>>
Rest == /\ phase \in {"halted", "finished"} /\ UNCHANGED vars
Next == ApplyEmptyBound \/ ApplyFilledName \/ LoseOriginal \/ ParkStoredA
        \/ ReturnOriginalForNew \/ QueueFreshBound \/ ResolveRawReference
        \/ AttachNewOrRejectParkedTarget \/ PostFirstReply \/ DuplicateBindRefresh
        \/ RestartAfterPosted \/ RestorePosted \/ RehostAfterHalt \/ ResumeParkAgain \/ Rest
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)
FirstReplyAtMostOnce == posted <= 1
FirstReplyExactlyOnce == (LegitimateFresh => <> (posted = 1)) /\ [] (posted <= 1)
FirstReplyUsesFreshSource == posted > 0 => checkpoint = 3 /\ "B" \in cursors /\ bValidated
                           /\ QueuedBound.new = "B" /\ QueuedBound.cause = "Startup" /\ FreshTargetExists
HaltRepeatZero == LegitimateFresh => rehosts = 0
NoHaltForConfirmedOld == (LegitimateFresh /\ tail = 3) => halts = 0
ParkedBytesUnchanged == EvidenceA = OriginalEvidenceA
NarrowOldOnly == bypassUsed => confirmed /\ unique /\ parked /\ referenceRole = "old"
                /\ QueuedBound.old = rawAtPrefix
StoredNameNeverDuplicated == "A0" \in cursors /\ "A1" \notin cursors
ParkedNewStillHalts == (referenceRole = "new" /\ bindFinished) => phase = "halted" /\ ~ready
MissingFreshTargetHalts == (caseName = "freshMissingB" /\ bindFinished)
                          => phase = "halted" /\ ~ready /\ checkpoint = 2 /\ posted = 0
=============================================================================
