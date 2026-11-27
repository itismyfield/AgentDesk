------------------------ MODULE HookTranscriptCustody ------------------------
\* One keyed ticket of execution N1 held as a boot input after a restart, and the channel
\* gate that waits on it. Rules: "S2" is the code at bd0c385be2, "D2" the second design,
\* "D3" this design. Mut switches one D3 guard off.
EXTENDS Naturals
CONSTANTS Rules, Mut, Scens, Devs, Files, N1s

VARIABLES scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser, witFinal,
          barrier, outcome, sealed, completion, cleared, clearedBy, endRec,
          rowOwner, succRemoved, t2, boot, writes, attrOk
vars == <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser, witFinal,
          barrier, outcome, sealed, completion, cleared, clearedBy, endRec,
          rowOwner, succRemoved, t2, boot, writes, attrOk>>

\* What N1 left in durable state before the restart:
\*   complete       membership and closer inside N1's stored prefix
\*   closerLate     closer written by N1 after its last capture
\*   killed         membership only; the turn never closed
\*   barrierStored  a foreign-prompt barrier was stored
\*   outcomeSaved   an Executed result with its read seal, completion not yet recorded
\*   outcomeLegacy  the same result stored without a seal
AllScens == {"complete", "closerLate", "killed", "barrierStored", "outcomeSaved", "outcomeLegacy"}
\* The transcript now: ok, late (absent for a while), gone, or changed (other inode or prefix).
AllFiles == {"ok", "late", "gone", "changed"}
ASSUME /\ Rules \in {"S2", "D2", "D3"}
       /\ Scens \subseteq AllScens /\ Files \subseteq AllFiles
       /\ Devs \subseteq {"same", "changed"} /\ N1s \subseteq {"alive", "dead"}

Saved == scen \in {"outcomeSaved", "outcomeLegacy"}
StoredBarrier == scen = "barrierStored"
\* Both global boot gates stat a stored barrier's transcript in S2.
Refusing == StoredBarrier /\ (dev = "changed" \/ file # "ok")
            /\ (Rules = "S2" \/ Mut = "ReaperExact")

Init == /\ scen \in Scens /\ dev \in Devs /\ file \in Files
        \* N1 survives only an application restart: same device, transcript in place.
        /\ n1 \in IF dev = "same" /\ file = "ok" THEN N1s ELSE N1s \cap {"dead"}
        /\ n2 = "absent" /\ rowN = "n1" /\ suffixPrompt = FALSE
        /\ fileCloser = (scen \in {"complete", "closerLate", "outcomeSaved", "outcomeLegacy"})
        /\ witCloser = (scen \in {"complete", "outcomeSaved", "outcomeLegacy"})
        /\ witFinal = FALSE
        /\ barrier = StoredBarrier
        /\ outcome = IF Saved THEN "Executed" ELSE "none"
        /\ sealed = (scen = "outcomeSaved")
        /\ completion = IF Saved THEN "pending" ELSE "na"
        /\ cleared = FALSE /\ clearedBy = "none" /\ endRec = FALSE
        /\ rowOwner = "n1" /\ succRemoved = FALSE /\ t2 = "open"
        /\ boot = IF Refusing THEN "refused" ELSE "ready"
        /\ writes = 0 /\ attrOk = TRUE

Running == boot = "ready"
Open == Running /\ outcome = "none" /\ ~cleared

\* ---- environment ----
N1Closes == /\ Open /\ n1 = "alive" /\ ~fileCloser /\ ~barrier
            /\ fileCloser' = TRUE
            /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, witCloser, witFinal,
                           barrier, outcome, sealed, completion, cleared, clearedBy, endRec,
                           rowOwner, succRemoved, t2, boot, writes, attrOk>>
N1Dies == /\ n1 = "alive" /\ n1' = "dead"
          /\ UNCHANGED <<scen, dev, file, n2, rowN, suffixPrompt, fileCloser, witCloser,
                         witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                         endRec, rowOwner, succRemoved, t2, boot, writes, attrOk>>
FileAppears == /\ file = "late" /\ file' = "ok"
               /\ UNCHANGED <<scen, dev, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                              witFinal, barrier, outcome, sealed, completion, cleared,
                              clearedBy, endRec, rowOwner, succRemoved, t2, boot, writes, attrOk>>
\* A host starts a successor only on an ended execution. D3 keeps N1's end before the
\* successor replaces the host's own record of it. Nothing obliges a successor to start.
N2Launch == /\ Running /\ n2 = "absent" /\ n1 = "dead"
            /\ n2' = "running" /\ rowN' = "n2"
            /\ endRec' = (endRec \/ (Rules = "D3" /\ Mut # "NoLaunchRecord"))
            /\ UNCHANGED <<scen, dev, file, n1, suffixPrompt, fileCloser, witCloser, witFinal,
                           barrier, outcome, sealed, completion, cleared, clearedBy,
                           rowOwner, succRemoved, t2, boot, writes, attrOk>>
N2Prompt == /\ Running /\ n2 \in {"running", "idle"} /\ ~suffixPrompt
            /\ suffixPrompt' = TRUE /\ n2' = "running"
            /\ UNCHANGED <<scen, dev, file, n1, rowN, fileCloser, witCloser, witFinal,
                           barrier, outcome, sealed, completion, cleared, clearedBy, endRec,
                           rowOwner, succRemoved, t2, boot, writes, attrOk>>
N2TakesRow == /\ Running /\ suffixPrompt /\ rowOwner = "n1"
              /\ rowOwner' = "n2"
              /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                             witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                             endRec, succRemoved, t2, boot, writes, attrOk>>
N2Idle == /\ Running /\ n2 = "running" /\ n2' = "idle"
          /\ UNCHANGED <<scen, dev, file, n1, rowN, suffixPrompt, fileCloser, witCloser,
                         witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                         endRec, rowOwner, succRemoved, t2, boot, writes, attrOk>>

\* ---- N1's stored prefix ----
\* The prefix grows while N1 is the owner's current execution; D3 also lets it reach the
\* length kept with N1's end, which no successor byte precedes.
CaptureAllowed == rowN = "n1" \/ (Rules = "D3" /\ Mut # "NoEndCut" /\ endRec)
WriterCapture == /\ Open /\ file = "ok" /\ CaptureAllowed
                 /\ witCloser # fileCloser \/ (endRec /\ ~witFinal)
                 /\ witCloser' = fileCloser /\ witFinal' = endRec
                 /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                barrier, outcome, sealed, completion, cleared, clearedBy,
                                endRec, rowOwner, succRemoved, t2, boot, writes, attrOk>>

\* ---- N1's end, proven on its own target ----
ProveEndLive == /\ Rules = "D3" /\ Mut # "NoBootProver" /\ Running /\ ~endRec /\ rowN = "n1"
                /\ n1 = "dead" \/ Mut = "EndWithoutProof"
                /\ endRec' = TRUE
                /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                               witCloser, witFinal, barrier, outcome, sealed, completion,
                               cleared, clearedBy, rowOwner, succRemoved, t2, boot, writes,
                               attrOk>>
EndGate == endRec \/ Mut = "NoEndProof"

\* ---- results ----
CanAttribute ==
    IF Rules = "S2" THEN dev = "same" /\ file = "ok" /\ fileCloser
    ELSE \/ witCloser /\ (file = "ok" \/ (Mut = "NoPrefixCheck" /\ file = "changed"))
         \/ Mut = "SuffixScope" /\ file = "ok" /\ suffixPrompt /\ n2 = "idle"
Settle == /\ Open /\ ~barrier /\ CanAttribute
          /\ outcome' = "Executed" /\ sealed' = (Rules # "S2") /\ completion' = "pending"
          /\ writes' = writes + 1 /\ attrOk' = (file = "ok")
          /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                         witFinal, barrier, cleared, clearedBy, endRec, rowOwner,
                         succRemoved, t2, boot>>
\* S2 folds a successor's prompt into N1's analysis and stores a barrier for it.
FormBarrier == /\ Rules = "S2" /\ Open /\ ~barrier /\ dev = "same" /\ file = "ok"
               /\ suffixPrompt /\ ~fileCloser
               /\ barrier' = TRUE
               /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                              witCloser, witFinal, outcome, sealed, completion, cleared,
                              clearedBy, endRec, rowOwner, succRemoved, t2, boot, writes,
                              attrOk>>

\* The exact row CAS: only N1's own row goes.
ClearRow(by) == /\ cleared' = TRUE /\ clearedBy' = by
                /\ rowOwner' = IF rowOwner = "n1" \/ Mut = "NoRowCAS" THEN "none" ELSE rowOwner
                /\ succRemoved' = (succRemoved \/ (Mut = "NoRowCAS" /\ rowOwner = "n2"))
OwnIdleAbstain == /\ Open /\ barrier /\ n1 = "alive" /\ rowN = "n1" /\ file = "ok"
                  /\ outcome' = "Empty" /\ ClearRow("n1idle")
                  /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                 witCloser, witFinal, barrier, sealed, completion, endRec,
                                 t2, boot, writes, attrOk>>
N2IdleAbstain == /\ Open /\ barrier /\ n2 = "idle"
                 /\ (Rules = "S2" /\ dev = "same" /\ file = "ok") \/ Mut = "IdleBorrow"
                 /\ outcome' = "Empty" /\ ClearRow("n2idle")
                 /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                witCloser, witFinal, barrier, sealed, completion, endRec,
                                t2, boot, writes, attrOk>>
\* D3: N1's end stands in for the idle N1 can no longer reach.
EndAbstain == /\ Rules = "D3" /\ Open /\ barrier /\ EndGate
              /\ outcome' = "Empty" /\ ClearRow("n1end")
              /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                             witCloser, witFinal, barrier, sealed, completion, endRec,
                             t2, boot, writes, attrOk>>
\* D3: N1 ended and its whole prefix holds no result, or its transcript is gone or changed.
Final == witFinal \/ Mut = "NoFinalCapture" \/ (Mut = "NoEndCut" /\ rowN = "n2")
ReleaseEnded == /\ Rules = "D3" /\ Open /\ ~barrier /\ EndGate
                /\ \/ file \in {"gone", "changed"}
                   \/ file = "ok" /\ Final /\ (~witCloser \/ Mut = "DropEvidence")
                /\ outcome' = "Unverified" /\ writes' = writes + 1 /\ ClearRow("n1end")
                /\ t2' = IF Mut = "AnyTarget" THEN "released" ELSE t2
                /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                               witCloser, witFinal, barrier, sealed, completion, endRec,
                               boot, attrOk>>

\* ---- completion of a stored result ----
ReadCompletion == /\ Running /\ outcome = "Executed" /\ completion = "pending" /\ file = "ok"
                  /\ sealed \/ dev = "same" \/ Rules = "S2"
                  /\ completion' = "recorded"
                  /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                 witCloser, witFinal, barrier, outcome, sealed, cleared,
                                 clearedBy, endRec, rowOwner, succRemoved, t2, boot, writes,
                                 attrOk>>
SkipAllowed ==
    CASE Rules = "S2" -> file # "ok"
      [] Rules = "D2" -> FALSE
      [] OTHER -> /\ Mut # "PendingForever"
                  /\ \/ file \in {"gone", "changed"}
                     \/ ~sealed /\ dev = "changed" /\ file = "ok"
                     \/ Mut = "SkipTransient" /\ file = "late"
SkipCompletion == /\ Running /\ outcome = "Executed" /\ completion = "pending" /\ SkipAllowed
                  /\ completion' = "skipped"
                  /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                 witCloser, witFinal, barrier, outcome, sealed, cleared,
                                 clearedBy, endRec, rowOwner, succRemoved, t2, boot, writes,
                                 attrOk>>
Cleanup == /\ Running /\ outcome = "Executed" /\ ~cleared
           /\ completion \in {"recorded", "skipped"} \/ Mut = "VoidCompletion"
           /\ ClearRow("outcome")
           /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                          witFinal, barrier, outcome, sealed, completion, endRec, t2, boot,
                          writes, attrOk>>

Next == N1Closes \/ N1Dies \/ FileAppears \/ N2Launch \/ N2Prompt \/ N2TakesRow \/ N2Idle
        \/ WriterCapture \/ ProveEndLive \/ Settle \/ FormBarrier \/ OwnIdleAbstain
        \/ N2IdleAbstain \/ EndAbstain \/ ReleaseEnded \/ ReadCompletion \/ SkipCompletion
        \/ Cleanup

\* A successor's launch, its prompt and N1's death are never assumed.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(N1Closes) /\ WF_vars(FileAppears) /\ WF_vars(N2Idle)
        /\ WF_vars(WriterCapture) /\ WF_vars(ProveEndLive) /\ WF_vars(Settle)
        /\ WF_vars(FormBarrier) /\ WF_vars(OwnIdleAbstain) /\ WF_vars(N2IdleAbstain)
        /\ WF_vars(EndAbstain) /\ WF_vars(ReleaseEnded) /\ WF_vars(ReadCompletion)
        /\ WF_vars(SkipCompletion) /\ WF_vars(Cleanup)

\* ---- rules ----
\* A row leaves without a result only on N1's own idle or N1's own end.
IdleDoesNotExpand == (cleared /\ outcome # "Executed") => clearedBy \in {"n1idle", "n1end"}
\* Executed only on a closer N1 itself wrote.
NoSuffixAttribution == outcome = "Executed" => fileCloser
\* A new result is recorded only from a transcript whose prefix was verified.
RejectUnverifiedFile == attrOk
\* A kept end names an execution that did end.
EndProofOwned == endRec => n1 = "dead"
\* A release on N1's end needs N1's kept end.
ReleaseNeedsEnd == clearedBy = "n1end" => endRec
\* A turn whose closer is readable is never released without its result.
NoLostAnswer == (outcome = "Unverified" /\ file = "ok") => ~fileCloser
\* The row of a stored Executed result goes only after its completion step ended.
CompletionBeforeCleanup == (cleared /\ outcome = "Executed") => completion \in {"recorded", "skipped"}
\* Completion records are given up only when the transcript is gone, changed or unsealed
\* across a device change.
SkipOnlyWhenTerminal ==
    completion = "skipped" => (file \in {"gone", "changed"} \/ (~sealed /\ dev = "changed"))
SuccessorUntouched == ~succRemoved
ExactTarget == t2 = "open"
OneOutcome == writes <= 1
SavedOutcomePreserved == Saved => outcome = "Executed"

\* The runtime starts.
BootEventuallyReady == <>(boot = "ready")
\* The channel's boot gate opens.
ChannelEventuallyReady == <>(boot = "ready" /\ cleared)
\* A turn N1 closed in a readable transcript settles as Executed.
EligibleEventuallySettles ==
    (boot = "ready" /\ fileCloser /\ file \in {"ok", "late"}) ~> (outcome = "Executed")
\* A stored barrier on the same device still clears once a successor is idle.
SameDevBarrierStillClears ==
    (boot = "ready" /\ StoredBarrier /\ dev = "same" /\ file = "ok" /\ n2 = "idle") ~> cleared
=============================================================================
