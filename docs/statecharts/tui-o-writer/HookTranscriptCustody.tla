------------------------ MODULE HookTranscriptCustody ------------------------
\* One keyed ticket of execution N1 held as a boot input after a restart, and the channel
\* gate that waits on it. Rules: "S2" is the code at bd0c385be2, "D2" the second design,
\* "D3" the third design, "S3a4" and "S3a5" the first stage as the fourth and fifth designs wrote
\* it, "S3a" that stage as the sixth design writes it, "Full" both stages. Mut switches one guard off.
EXTENDS Naturals
CONSTANTS Rules, Mut, Scens, Devs, Files, N1s, Starts, Switches, Approvals, Others,
          Entries, Receipts, Hosts

VARIABLES scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser, witFinal,
          barrier, outcome, sealed, completion, cleared, clearedBy, endRec, cutKnown,
          approved, writer, switched, rowOwner, succRemoved, t2, boot, writes, attrOk, otherBad,
          entry, receipt, host, fx
vars == <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser, witFinal,
          barrier, outcome, sealed, completion, cleared, clearedBy, endRec, cutKnown,
          approved, writer, switched, rowOwner, succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>

\* What N1 left in durable state before the restart:
\*   complete       membership and newline-terminated closer
\*   closerLate     closer written after the writer's last capture
\*   rawCloser      a whole strict closer at EOF without its newline; N1 ended right after
\*   killed         membership only; the turn never closed
\*   barrierStored  a foreign-prompt barrier was stored
\*   outcomeSaved   an Executed result with its read seal, completion not yet recorded
\*   outcomeLegacy  the same result stored without a seal
AllScens == {"complete", "closerLate", "rawCloser", "killed", "barrierStored",
             "outcomeSaved", "outcomeLegacy"}
\* The transcript now: ok, late (absent for a while), gone (absent for good), or changed (other
\* inode or prefix). A reader sees late and gone alike: the path does not stat.
AllFiles == {"ok", "late", "gone", "changed"}
\* fresh: N1 is the owner's current execution. superseded: a successor already runs and no
\* end record of N1 exists (state left by a binary without end records).
ASSUME /\ Rules \in {"S2", "D2", "D3", "S3a4", "S3a5", "S3a", "Full"}
       /\ Scens \subseteq AllScens /\ Files \subseteq AllFiles
       /\ Devs \subseteq {"same", "changed"} /\ N1s \subseteq {"alive", "dead"}
       /\ Starts \subseteq {"fresh", "superseded"}
       /\ Switches \subseteq BOOLEAN /\ Approvals \subseteq BOOLEAN /\ Others \subseteq BOOLEAN
       \* restore: a stored result read with no stat first (boot restore, live completion);
       \* retry: the writer's retry job, made only after the exact stat check.
       /\ Entries \subseteq {"restore", "retry"} /\ Receipts \subseteq BOOLEAN
       \* How N1's end can be read after the restart: a Herdr record, a tmux run from before the
       \* boot, a dead tmux pane still holding N1's spawn marker, or a tmux run with nothing left.
       /\ Hosts \subseteq {"herdr", "tmuxReboot", "tmuxDead", "tmuxUnknown"}

EndRules == {"D3", "S3a4", "S3a5", "S3a", "Full"}
S3aRules == {"S3a4", "S3a5", "S3a"}
New == Rules \in {"S3a", "Full"}
\* The path does not stat; nothing tells a short absence from a lasting one.
Unstattable == file \in {"late", "gone"}
\* From the fifth design the finalizer scans N1's source up to the kept length itself.
ScanByDriver == Rules \in {"S3a5", "S3a", "Full"} /\ Mut # "ScanInWriter"
Saved == scen \in {"outcomeSaved", "outcomeLegacy"}
StoredBarrier == scen = "barrierStored"
NewlineCloser == fileCloser /\ scen # "rawCloser"
\* Both global boot gates stat a stored barrier's transcript in S2.
Refusing == StoredBarrier /\ (dev = "changed" \/ file # "ok")
            /\ (Rules = "S2" \/ Mut = "ReaperExact")

Init == /\ scen \in Scens /\ dev \in Devs /\ file \in Files /\ host \in Hosts
        /\ \E start \in Starts :
             /\ ~(start = "superseded" /\ scen = "rawCloser")
             /\ n2 = IF start = "superseded" THEN "running" ELSE "absent"
             /\ rowN = IF start = "superseded" THEN "n2" ELSE "n1"
             \* N1 survives only an application restart: same device, transcript in place.
             /\ n1 \in IF start = "fresh" /\ dev = "same" /\ file = "ok" /\ scen # "rawCloser"
                       THEN N1s ELSE N1s \cap {"dead"}
        /\ suffixPrompt = FALSE
        /\ fileCloser = (scen \in {"complete", "closerLate", "rawCloser", "outcomeSaved",
                                   "outcomeLegacy"})
        /\ witCloser = (scen \in {"complete", "outcomeSaved", "outcomeLegacy"})
        /\ witFinal = FALSE
        /\ barrier = StoredBarrier
        /\ outcome = IF Saved THEN "Executed" ELSE "none"
        /\ sealed = (scen = "outcomeSaved")
        /\ completion = IF Saved THEN "pending" ELSE "na"
        /\ cleared = FALSE /\ clearedBy = "none" /\ endRec = FALSE /\ cutKnown = FALSE
        \* A device lineage the writer already committed from a hook.
        /\ \E a \in Approvals : approved = (dev = "same" \/ a)
        /\ writer = "alive"
        \* Another source cursor of the channel whose reopen fails.
        /\ otherBad \in Others
        /\ switched \in Switches
        /\ rowOwner = IF rowN = "n2" THEN "n2" ELSE "n1"
        /\ succRemoved = FALSE /\ t2 = "open"
        /\ boot = IF Refusing THEN "refused" ELSE "ready"
        /\ writes = 0 /\ attrOk = TRUE
        /\ entry \in IF scen \in {"outcomeSaved", "outcomeLegacy"} THEN Entries ELSE {"none"}
        \* The durable membership receipt of N1's prompt; only a closed or killed turn may lack it.
        /\ receipt \in IF scen \in {"complete", "closerLate", "rawCloser", "killed"}
                       THEN Receipts ELSE {TRUE}
        /\ fx = 0
        \* A pane read dead or a run from before the boot has ended.
        /\ (host \in {"herdr", "tmuxUnknown"} \/ n1 = "dead")

Running == boot = "ready"
Open == Running /\ outcome = "none" /\ ~cleared
\* Who drives boot debt: the source writer's poll before S3a, the finalizer's tick from S3a.
DriverAlive == IF Rules \in {"S2", "D2", "D3"} \/ Mut = "WriterRetry"
               THEN writer = "alive" ELSE TRUE

\* ---- environment ----
N1Closes == /\ Open /\ n1 = "alive" /\ ~fileCloser /\ ~barrier
            /\ fileCloser' = TRUE
            /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, witCloser, witFinal,
                           barrier, outcome, sealed, completion, cleared, clearedBy, endRec,
                           cutKnown, approved, writer, switched, rowOwner, succRemoved, t2,
                           boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
N1Dies == /\ n1 = "alive" /\ n1' = "dead"
          /\ UNCHANGED <<scen, dev, file, n2, rowN, suffixPrompt, fileCloser, witCloser,
                         witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                         endRec, cutKnown, approved, writer, switched, rowOwner, succRemoved,
                         t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
FileAppears == /\ file = "late" /\ file' = "ok"
               /\ UNCHANGED <<scen, dev, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                              witFinal, barrier, outcome, sealed, completion, cleared,
                              clearedBy, endRec, cutKnown, approved, writer, switched,
                              rowOwner, succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
\* A reader whose source names another inode or prefix halts the channel writer for good,
\* whether that source is this ticket's or another cursor's.
WriterHalt == /\ writer = "alive" /\ (file = "changed" \/ otherBad) /\ writer' = "halted"
              /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                             witCloser, witFinal, barrier, outcome, sealed, completion,
                             cleared, clearedBy, endRec, cutKnown, approved, switched,
                             rowOwner, succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
\* A successor starts only on an ended execution, and resumes a transcript it can read.
\* From D3 the launch keeps N1's end, with its length when the file is in place.
\* A tmux end is read only off a live capture before the sixth design; from it also off the boot
\* time or a dead pane that still names N1.
EndReadable == \/ host = "herdr"
               \/ New /\ Mut # "TmuxLiveOnly" /\ host \in {"tmuxReboot", "tmuxDead"}
LaunchRecords == Rules \in EndRules /\ Mut # "NoLaunchRecord" /\ EndReadable
N2Launch == /\ Running /\ n2 = "absent" /\ n1 = "dead" /\ file # "late"
            /\ n2' = "running" /\ rowN' = "n2"
            /\ endRec' = (endRec \/ LaunchRecords)
            /\ cutKnown' = IF endRec THEN cutKnown ELSE (LaunchRecords /\ file = "ok")
            /\ UNCHANGED <<scen, dev, file, n1, suffixPrompt, fileCloser, witCloser, witFinal,
                           barrier, outcome, sealed, completion, cleared, clearedBy, approved,
                           writer, switched, rowOwner, succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
\* A successor's prompt hook commits the device lineage (the S2 refresh).
N2Prompt == /\ Running /\ n2 \in {"running", "idle"} /\ ~suffixPrompt
            /\ suffixPrompt' = TRUE /\ n2' = "running"
            /\ approved' = (approved \/ file = "ok")
            /\ UNCHANGED <<scen, dev, file, n1, rowN, fileCloser, witCloser, witFinal,
                           barrier, outcome, sealed, completion, cleared, clearedBy, endRec,
                           cutKnown, writer, switched, rowOwner, succRemoved, t2, boot,
                           writes, attrOk, otherBad, entry, receipt, host, fx>>
N2TakesRow == /\ Running /\ suffixPrompt /\ rowOwner = "n1"
              /\ rowOwner' = "n2"
              /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                             witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                             endRec, cutKnown, approved, writer, switched, succRemoved, t2,
                             boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
N2Idle == /\ Running /\ n2 = "running" /\ n2' = "idle"
          /\ UNCHANGED <<scen, dev, file, n1, rowN, suffixPrompt, fileCloser, witCloser,
                         witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                         endRec, cutKnown, approved, writer, switched, rowOwner, succRemoved,
                         t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>

\* Who serves N1's ticket from the writer: the fifth design kept only the current execution there;
\* the sixth hands a ticket to the finalizer only once its end, cut and approval are in place.
Handed == endRec /\ cutKnown /\ approved
WriterServes == CASE Rules = "S3a5" -> rowN = "n1"
                  [] New -> ~(ScanByDriver /\ Handed)
                  [] OTHER -> TRUE

\* ---- N1's analysed range ----
\* N1 as the current execution, or N1's range up to the length kept with its end.
CaptureAllowed ==
    \/ rowN = "n1" /\ (dev = "same" \/ approved)
    \/ Rules \in EndRules /\ Mut # "NoEndCut" /\ endRec /\ approved
       /\ (cutKnown \/ Rules = "D3")
WriterCapture == /\ Open /\ file = "ok" /\ writer = "alive" /\ CaptureAllowed /\ WriterServes
                 /\ witCloser # NewlineCloser \/ (endRec /\ ~witFinal)
                 /\ witCloser' = NewlineCloser
                 /\ witFinal' = IF Rules = "D3" THEN endRec ELSE (endRec /\ cutKnown)
                 /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                barrier, outcome, sealed, completion, cleared, clearedBy,
                                endRec, cutKnown, approved, writer, switched, rowOwner,
                                succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>

\* The finalizer's scan of N1's source up to the kept length, one source at a time.
EndScan == /\ ScanByDriver /\ Mut # "NoEndCut" /\ Open /\ file = "ok" /\ endRec /\ cutKnown
           /\ approved /\ (witCloser # NewlineCloser \/ ~witFinal)
           /\ witCloser' = NewlineCloser /\ witFinal' = TRUE
           /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                          barrier, outcome, sealed, completion, cleared, clearedBy,
                          endRec, cutKnown, approved, writer, switched, rowOwner,
                          succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>

\* The membership receipt: the writer's supply, or from the sixth design the finalizer's scan.
WriterReceipt == /\ Open /\ ~barrier /\ ~receipt /\ writer = "alive" /\ file = "ok"
                 /\ (dev = "same" \/ approved) /\ WriterServes
                 /\ receipt' = TRUE
                 /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                                witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                                endRec, cutKnown, approved, writer, switched, rowOwner,
                                succRemoved, t2, boot, writes, attrOk, otherBad, entry, host, fx>>
ScanReceipt == /\ New /\ ScanByDriver /\ Mut # "ScanNoReceipt" /\ Open /\ ~barrier
               /\ ~receipt /\ file = "ok" /\ Handed
               /\ receipt' = TRUE
               /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                              witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                              endRec, cutKnown, approved, writer, switched, rowOwner,
                              succRemoved, t2, boot, writes, attrOk, otherBad, entry, host, fx>>

\* ---- N1's end ----
\* E4: the host reads N1's own target as ended. From S3a it waits while the file is absent.
ProveEnd == /\ Rules \in EndRules /\ Mut # "NoBootProver" /\ Running /\ ~endRec
            /\ rowN = "n1" /\ DriverAlive /\ EndReadable
            /\ n1 = "dead" \/ Mut = "EndWithoutProof"
            /\ Rules = "D3" \/ file # "late"
            /\ endRec' = TRUE /\ cutKnown' = (file = "ok")
            /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                           witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                           approved, writer, switched, rowOwner, succRemoved, t2, boot,
                           writes, attrOk, otherBad, entry, receipt, host, fx>>
\* S3b: a successor admitted by the host proves N1 ended; its length stays unknown.
ProveSuperseded == /\ Rules = "Full" /\ Mut # "NoSupersededProof" /\ Running /\ ~endRec
                   /\ rowN = "n2" /\ n1 = "dead"
                   /\ endRec' = TRUE /\ cutKnown' = FALSE
                   /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                  witCloser, witFinal, barrier, outcome, sealed, completion,
                                  cleared, clearedBy, approved, writer, switched, rowOwner,
                                  succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
\* S3b: N1's end and the stored prefix commitment approve the renumbered file.
EndRefresh == /\ Rules = "Full" /\ Mut # "NoEndRefresh" /\ Running /\ endRec /\ ~approved
              /\ file = "ok"
              /\ approved' = TRUE
              /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                             witFinal, barrier, outcome, sealed, completion, cleared, clearedBy,
                             endRec, cutKnown, writer, switched, rowOwner, succRemoved, t2,
                             boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
\* The end record names the source N1 wrote last; S3b applies N1's end to every source N1 wrote.
EndApplies == endRec /\ (~switched \/ (Rules = "Full" /\ Mut # "LatestSourceOnly"))
EndGate == EndApplies \/ Mut = "NoEndProof"

\* ---- results ----
\* The S2 path: N1's ticket read off the whole current file of the same device.
CurrentPath == dev = "same" /\ file = "ok" /\ NewlineCloser /\ writer = "alive" /\ WriterServes
BoundedPath == witCloser /\ file = "ok"
               /\ (writer = "alive" \/ (ScanByDriver /\ endRec /\ cutKnown))
               /\ (approved \/ Mut = "NoApproval")
CanAttribute ==
    CASE Rules = "S2" -> CurrentPath
      [] Rules \in {"D2", "D3"} ->
           \/ BoundedPath
           \/ Mut = "NoPrefixCheck" /\ witCloser /\ file = "changed"
      [] Rules \in S3aRules -> CurrentPath \/ (BoundedPath /\ endRec /\ cutKnown)
      [] OTHER ->
           \/ BoundedPath /\ (cutKnown \/ ~endRec)
           \* Without a known length, N1's ticket is read off the whole approved prefix.
           \/ ~cutKnown /\ file = "ok" /\ (dev = "same" \/ approved) /\ NewlineCloser
              /\ writer = "alive"
           \/ Mut = "SuffixScope" /\ file = "ok" /\ suffixPrompt /\ n2 = "idle"
           \/ Mut = "NoPrefixCheck" /\ witCloser /\ file = "changed"
Settle == /\ Open /\ ~barrier /\ CanAttribute /\ receipt
          /\ outcome' = "Executed" /\ sealed' = (Rules # "S2") /\ completion' = "pending"
          /\ writes' = writes + 1
          /\ attrOk' = (file = "ok" /\ (dev = "same" \/ approved))
          /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                         witFinal, barrier, cleared, clearedBy, endRec, cutKnown, approved,
                         writer, switched, rowOwner, succRemoved, t2, boot, otherBad, entry, receipt, host, fx>>
\* The existing raw EOF proof: the hosted record still names N1.
RawEOFLive == /\ Open /\ ~barrier /\ scen = "rawCloser" /\ n1 = "dead" /\ rowN = "n1" /\ receipt
              /\ writer = "alive" /\ file = "ok" /\ dev = "same"
              /\ outcome' = "Executed" /\ sealed' = (Rules # "S2") /\ completion' = "pending"
              /\ writes' = writes + 1 /\ attrOk' = TRUE
              /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                             witCloser, witFinal, barrier, cleared, clearedBy, endRec,
                             cutKnown, approved, writer, switched, rowOwner, succRemoved, t2,
                             boot, otherBad, entry, receipt, host, fx>>
\* S3a: the raw closer up to the length kept with N1's end, whatever host record runs now.
HistRawEOF == /\ Rules \in {"S3a4", "S3a5", "S3a", "Full"} /\ Mut # "NoHistRaw" /\ Open /\ ~barrier
              /\ scen = "rawCloser" /\ EndApplies /\ cutKnown /\ approved /\ file = "ok" /\ receipt
              /\ (writer = "alive" \/ ScanByDriver)
              /\ outcome' = "Executed" /\ sealed' = TRUE /\ completion' = "pending"
              /\ writes' = writes + 1 /\ attrOk' = TRUE
              /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                             witCloser, witFinal, barrier, cleared, clearedBy, endRec,
                             cutKnown, approved, writer, switched, rowOwner, succRemoved, t2,
                             boot, otherBad, entry, receipt, host, fx>>
\* S2 folds a successor's prompt into N1's analysis and stores a barrier for it; S3a keeps it.
FormBarrier == /\ Rules \in {"S2", "S3a4", "S3a5", "S3a"} /\ Open /\ ~barrier /\ dev = "same" /\ file = "ok"
               /\ writer = "alive" /\ suffixPrompt /\ ~fileCloser
               /\ barrier' = TRUE
               /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                              witCloser, witFinal, outcome, sealed, completion, cleared,
                              clearedBy, endRec, cutKnown, approved, writer, switched, rowOwner,
                              succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>

\* The exact row CAS: only N1's own row goes.
ClearRow(by) == /\ cleared' = TRUE /\ clearedBy' = by
                /\ rowOwner' = IF rowOwner = "n1" \/ Mut = "NoRowCAS" THEN "none" ELSE rowOwner
                /\ succRemoved' = (succRemoved \/ (Mut = "NoRowCAS" /\ rowOwner = "n2"))
OwnIdleAbstain == /\ Open /\ barrier /\ n1 = "alive" /\ rowN = "n1" /\ file = "ok"
                  /\ outcome' = "Empty" /\ ClearRow("n1idle")
                  /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                 witCloser, witFinal, barrier, sealed, completion, endRec,
                                 cutKnown, approved, writer, switched, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
\* S2 and S3a: any current idle drives boot abstain once the exact stat of the same device holds.
N2IdleAbstain == /\ Open /\ barrier /\ n2 = "idle"
                 /\ \/ Rules \in {"S2", "S3a4", "S3a5", "S3a"} /\ file = "ok"
                       /\ (dev = "same" \/ Mut = "S3aCrossIdle")
                    \/ Mut = "IdleBorrow"
                 /\ outcome' = "Empty" /\ ClearRow("n2idle")
                 /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                witCloser, witFinal, barrier, sealed, completion, endRec,
                                cutKnown, approved, writer, switched, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
\* N1's end stands in for the idle N1 can no longer reach.
EndAbstain == /\ Rules \in EndRules /\ Open /\ barrier /\ EndGate /\ DriverAlive
              /\ outcome' = "Empty" /\ ClearRow("n1end")
              /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                             witCloser, witFinal, barrier, sealed, completion, endRec, cutKnown,
                             approved, writer, switched, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
\* A transcript whose original content is gone. D3 trusts a gone path; the fourth design's S3a
\* reads any path that does not stat as gone; from the fifth design only another inode or prefix.
Terminal == CASE Rules = "D3" -> file \in {"gone", "changed"}
              [] Rules = "S3a4" -> file # "ok"
              [] OTHER -> file = "changed" \/ (Mut = "EnoentTerminal" /\ Unstattable)
\* Evidence inside N1's range. D3 counts newline-terminated records only.
InRangeEvidence == IF Rules = "D3" THEN witCloser ELSE (witCloser \/ scen = "rawCloser")
Final == witFinal \/ Mut = "NoFinalCapture" \/ (Mut = "NoEndCut" /\ rowN = "n2")
ReleaseEnded ==
    /\ Rules \in EndRules /\ Open /\ ~barrier /\ EndGate /\ DriverAlive
    /\ \/ Terminal
       \/ file = "ok" /\ Final /\ (Rules = "D3" \/ cutKnown \/ Mut = "NoFinalCapture")
          /\ (~InRangeEvidence \/ Mut = "DropEvidence")
       \/ Rules = "Full" /\ file = "ok" /\ ~cutKnown /\ (dev = "same" \/ approved)
          /\ (~fileCloser \/ Mut = "DropEvidence")
    /\ outcome' = "Unverified" /\ writes' = writes + 1 /\ ClearRow("n1end")
    /\ t2' = IF Mut = "AnyTarget" THEN "released" ELSE t2
    /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                   witFinal, barrier, sealed, completion, endRec, cutKnown, approved, writer,
                   switched, boot, attrOk, otherBad, entry, receipt, host, fx>>

\* ---- completion of a stored result ----
ReadCompletion == /\ Running /\ DriverAlive /\ outcome = "Executed" /\ completion = "pending"
                  /\ file = "ok"
                  /\ \/ sealed \/ dev = "same" \/ Rules = "S2"
                     \* A restored result is read without a stat in S2, and from the sixth design.
                     \/ New /\ entry = "restore" /\ Mut # "RestoreAwait"
                  /\ completion' = "recorded" /\ fx' = fx + 1
                  /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                 witCloser, witFinal, barrier, outcome, sealed, cleared,
                                 clearedBy, endRec, cutKnown, approved, writer, switched,
                                 rowOwner, succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host>>
SkipAllowed ==
    \* S2 reads a restored or live result without a stat and goes on after a failed read; its retry
    \* job waits behind the exact stat check.
    CASE Rules = "S2" -> entry = "restore" /\ file # "ok"
      [] Rules = "D2" -> FALSE
      [] Rules \in {"D3", "S3a4"} ->
                  \/ Terminal
                  \/ ~sealed /\ dev = "changed" /\ file = "ok"
      \* An unsealed result across a device change waits, as the exact stat check does in S2.
      [] Rules = "S3a5" -> Terminal \/ (Mut = "SkipTransient" /\ file = "late")
      \* From the sixth design each entry keeps its S2 precondition.
      [] OTHER -> /\ Mut # "PendingForever"
                  /\ \/ Terminal
                     \/ entry = "restore" /\ file # "ok" /\ Mut # "RestoreAwait"
                     \/ Mut = "SkipTransient" /\ file = "late"
SkipCompletion == /\ Running /\ DriverAlive /\ outcome = "Executed"
                  /\ completion = "pending" /\ SkipAllowed
                  /\ completion' = "skipped"
                  /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser,
                                 witCloser, witFinal, barrier, outcome, sealed, cleared,
                                 clearedBy, endRec, cutKnown, approved, writer, switched,
                                 rowOwner, succRemoved, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>
Cleanup == /\ Running /\ DriverAlive /\ outcome = "Executed" /\ ~cleared
           /\ completion \in {"recorded", "skipped"} \/ Mut = "VoidCompletion"
           /\ ClearRow("outcome")
           /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                          witFinal, barrier, outcome, sealed, completion, endRec, cutKnown,
                          approved, writer, switched, t2, boot, writes, attrOk, otherBad, entry, receipt, host, fx>>

\* The row CAS or the terminal receipt is deferred; without a kept completion state the next
\* attempt runs the completion's side effects again.
Latched == New /\ Mut # "NoLatch"
CleanupDefer == /\ Running /\ outcome = "Executed" /\ completion = "recorded" /\ ~cleared
                /\ ~Latched /\ fx < 2
                /\ completion' = "pending"
                /\ UNCHANGED <<scen, dev, file, n1, n2, rowN, suffixPrompt, fileCloser, witCloser,
                               witFinal, barrier, outcome, sealed, cleared, clearedBy, endRec,
                               cutKnown, approved, writer, switched, rowOwner, succRemoved, t2,
                               boot, writes, attrOk, otherBad, entry, receipt, host, fx>>

Next == N1Closes \/ N1Dies \/ FileAppears \/ WriterHalt \/ N2Launch \/ N2Prompt
        \/ N2TakesRow \/ N2Idle \/ WriterCapture \/ EndScan \/ WriterReceipt \/ ScanReceipt
        \/ CleanupDefer \/ ProveEnd \/ ProveSuperseded \/ EndRefresh
        \/ Settle \/ RawEOFLive \/ HistRawEOF \/ FormBarrier \/ OwnIdleAbstain
        \/ N2IdleAbstain \/ EndAbstain \/ ReleaseEnded \/ ReadCompletion \/ SkipCompletion
        \/ Cleanup

\* A successor's launch, its prompt and N1's death are never assumed.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(N1Closes) /\ WF_vars(FileAppears) /\ WF_vars(WriterHalt) /\ WF_vars(N2Idle)
        /\ WF_vars(WriterCapture) /\ WF_vars(EndScan) /\ WF_vars(WriterReceipt)
        /\ WF_vars(ScanReceipt) /\ WF_vars(ProveEnd) /\ WF_vars(ProveSuperseded)
        /\ WF_vars(EndRefresh) /\ WF_vars(Settle) /\ WF_vars(RawEOFLive)
        /\ WF_vars(HistRawEOF) /\ WF_vars(FormBarrier) /\ WF_vars(OwnIdleAbstain)
        /\ WF_vars(N2IdleAbstain) /\ WF_vars(EndAbstain) /\ WF_vars(ReleaseEnded)
        /\ WF_vars(ReadCompletion) /\ WF_vars(SkipCompletion) /\ WF_vars(Cleanup)

\* ---- rules ----
\* A row leaves without a result only on N1's own idle or N1's own end.
IdleDoesNotExpand == (cleared /\ outcome # "Executed") => clearedBy \in {"n1idle", "n1end"}
\* A successor's idle never clears a ticket of a renumbered file.
IdleNotExpandedCrossDev == clearedBy = "n2idle" => dev = "same"
\* Executed only on a closer N1 itself wrote.
NoSuffixAttribution == outcome = "Executed" => fileCloser
\* A new result only from the same file, on the same device or an approved device lineage.
RejectUnverifiedFile == attrOk
\* A kept end names an execution that did end.
EndProofOwned == endRec => n1 = "dead"
\* A release on N1's end needs N1's end, applicable to this ticket's source.
ReleaseNeedsEnd == clearedBy = "n1end" => EndApplies
\* A turn whose closer is readable is never released without its result.
NoLostAnswer == (outcome = "Unverified" /\ file = "ok") => ~fileCloser
\* The row of a stored Executed result goes only after its completion step ended.
CompletionBeforeCleanup == (cleared /\ outcome = "Executed") => completion \in {"recorded", "skipped"}
\* Completion records are given up only for a transcript classified gone or changed, an unsealed
\* result across a device change, or S2's warning path on a restored or live read.
SkipOnlyWhenTerminal ==
    completion = "skipped"
    => (file \in {"gone", "changed"} \/ (~sealed /\ dev = "changed") \/ entry = "restore")
\* Nothing is given up while the original content may still come back: a completion skip or a
\* release of a closed turn needs another inode or prefix at the path.
\* S2's own warning path on a restored or live read stays as it is.
NoSilentLoss == /\ completion = "skipped" => (file = "changed" \/ entry = "restore")
                /\ (outcome = "Unverified" /\ fileCloser) => file = "changed"
\* A completion's side effects run once.
OneCompletionEffect == fx <= 1
SuccessorUntouched == ~succRemoved
\* A ticket of another target stays open (one abstract other ticket).
ExactTarget == t2 = "open"
\* Registry outcome writes only; completion side effects are not counted here.
OneOutcome == writes <= 1
SavedOutcomePreserved == Saved => outcome = "Executed"

\* The runtime starts.
BootEventuallyReady == <>(boot = "ready")
\* The channel's boot gate opens.
ChannelEventuallyReady == <>(boot = "ready" /\ cleared)
\* A turn N1 closed in a readable transcript settles as Executed.
EligibleEventuallySettles ==
    (boot = "ready" /\ fileCloser /\ file \in {"ok", "late"}) ~> (outcome = "Executed")
\* What S2 clears today still clears: a stored barrier once a successor is idle.
SameDevBarrierStillClears ==
    (boot = "ready" /\ StoredBarrier /\ dev = "same" /\ file = "ok" /\ n2 = "idle") ~> cleared
\* And a killed turn once a successor took a prompt and went idle.
SameDevKilledStillClears ==
    (boot = "ready" /\ scen = "killed" /\ dev = "same" /\ file = "ok" /\ suffixPrompt
     /\ n2 = "idle") ~> cleared
=============================================================================
