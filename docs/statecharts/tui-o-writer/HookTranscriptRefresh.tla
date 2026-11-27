------------------------ MODULE HookTranscriptRefresh ------------------------
EXTENDS Naturals
CONSTANTS Refresh, KeepTicket, RetryStop, RequireRegisteredPin
VARIABLES scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
          notice, outcome, inflight, posts, restarted, intent, diskKeys, ready
vars == <<scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
          notice, outcome, inflight, posts, restarted, intent, diskKeys, ready>>

Old == [session |-> "A", path |-> "a", ino |-> 1, dev |-> 1]
Renumbered == [Old EXCEPT !.dev = 2]
Compacted == [session |-> "B", path |-> "b", ino |-> 2, dev |-> 2]
Candidate == IF scenario = "compact" THEN Compacted ELSE Renumbered
Ticket == IF scenario = "oldTicket" THEN Old ELSE Candidate
Live == IF scenario = "foreign" THEN Old ELSE Candidate
SameFile(a, b) == a.session = b.session /\ a.path = b.path /\ a.ino = b.ino
Start(source) == [source |-> source, through |-> 0, prefix |-> "empty"]
Saved == [source |-> Old, through |-> 17, prefix |-> "old-prefix"]

Unregistered == {"noRegisteredStat", "replacedRegisteredInode"}
FirstPinCases == Unregistered \cup {"foreignFirstRecord", "registeredFirstPin"}

Init == /\ scenario \in (FirstPinCases \cup {"renumber", "compact", "foreign", "oldTicket", "bootStop"})
        /\ hook = "waiting" /\ binding = Old /\ seq = 1
        /\ cursor = Saved /\ checkpoint = 1
        /\ proof = Old /\ proofSeq = 1
        /\ notice = "pending" /\ outcome = 0 /\ inflight = TRUE
        /\ ready = (scenario # "bootStop")
        /\ posts = 0 /\ restarted = FALSE
        /\ intent = "absent" /\ diskKeys = {Old}

\* The pane name and incarnation have been checked before this durable log append.
Hook == /\ hook \in {"waiting", "retry"}
        /\ hook' = CASE scenario \in {"foreign", "foreignFirstRecord"} -> "rejected"
                       [] scenario \in Unregistered /\ RequireRegisteredPin -> "rejected"
                       [] ready -> "accepted"
                       [] RetryStop -> "retry"
                       [] OTHER -> "lost"
        /\ binding' = IF hook' = "accepted" /\ scenario \notin FirstPinCases
                           /\ (scenario = "compact" \/ Refresh)
                        THEN Candidate ELSE binding
        /\ seq' = IF binding' = binding THEN seq ELSE seq + 1
        /\ UNCHANGED <<scenario, cursor, checkpoint, proof, proofSeq,
                       notice, outcome, inflight, posts, restarted, intent, diskKeys, ready>>

Restore == /\ ~ready /\ ready' = TRUE
           /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint, proof,
                          proofSeq, notice, outcome, inflight, posts, restarted, intent, diskKeys>>

Stage == /\ SameFile(cursor.source, binding)
         /\ cursor.source # binding /\ intent = "absent" /\ intent' = "staged"
         /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
                        notice, outcome, inflight, posts, restarted, diskKeys, ready>>
Commit == /\ intent = "staged" /\ intent' = "committed"
          /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
                         notice, outcome, inflight, posts, restarted, diskKeys, ready>>
InstallKey == /\ intent = "committed" /\ binding \notin diskKeys
              /\ diskKeys' = diskKeys \cup {binding}
              /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
                             notice, outcome, inflight, posts, restarted, intent, ready>>

\* A committed intent is rolled forward before the checkpoint moves.
Apply == /\ checkpoint < seq
         /\ scenario = "compact" \/ intent = "committed" \/ cursor.source = binding
         /\ cursor' = IF SameFile(cursor.source, binding)
                         THEN [cursor EXCEPT !.source = binding]
                         ELSE Start(binding)
         /\ checkpoint' = seq
         /\ intent' = "absent" /\ diskKeys' = {binding}
         /\ UNCHANGED <<scenario, hook, binding, seq, proof, proofSeq,
                        notice, outcome, inflight, posts, restarted, ready>>

Restart == /\ ~restarted
           /\ restarted' = TRUE /\ proof' = Old /\ proofSeq' = 0
           /\ cursor' = IF intent = "committed" THEN [cursor EXCEPT !.source = binding] ELSE cursor
           /\ intent' = "absent" /\ diskKeys' = {cursor'.source}
           /\ UNCHANGED <<scenario, hook, binding, seq, checkpoint,
                          notice, outcome, inflight, posts, ready>>

Capture == /\ cursor.source = Compacted /\ cursor.through = 0
           /\ cursor' = [cursor EXCEPT !.through = 9, !.prefix = "new-prefix"]
           /\ posts' = posts + 1
           /\ UNCHANGED <<scenario, hook, binding, seq, checkpoint, proof, proofSeq,
                          notice, outcome, inflight, restarted, intent, diskKeys, ready>>

Observe == /\ hook = "accepted" /\ cursor.source = Live /\ cursor.through > 0
           /\ intent # "committed"
           /\ proofSeq # checkpoint
           /\ proof' = cursor.source /\ proofSeq' = checkpoint
           /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint,
                          notice, outcome, inflight, posts, restarted, intent, diskKeys, ready>>

Exact == /\ hook = "accepted" /\ proofSeq = seq /\ proof = binding /\ proof = Live
         /\ (Ticket = proof \/ (KeepTicket /\ SameFile(Ticket, proof) /\ checkpoint = seq))
Settle == /\ Exact /\ outcome = 0
          /\ outcome' = outcome + 1
          /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint,
                         proof, proofSeq, notice, inflight, posts, restarted, intent, diskKeys, ready>>
SettleNotice == /\ Exact /\ notice = "pending" /\ notice' = "excluded"
                /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint,
                               proof, proofSeq, outcome, inflight, posts, restarted, intent, diskKeys, ready>>
Clear == /\ outcome = 1 /\ inflight /\ inflight' = FALSE
         /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint,
                        proof, proofSeq, notice, outcome, posts, restarted, intent, diskKeys, ready>>

Next == Hook \/ Restore \/ Stage \/ Commit \/ InstallKey \/ Apply \/ Restart \/ Capture
        \/ Observe \/ Settle \/ SettleNotice \/ Clear
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Stage) /\ WF_vars(Commit)
        /\ WF_vars(Hook) /\ WF_vars(Restore) /\ WF_vars(Apply) /\ WF_vars(Capture)
        /\ WF_vars(Observe) /\ WF_vars(Settle) /\ WF_vars(SettleNotice) /\ WF_vars(Clear)

NoPrefixReplay == IF scenario = "compact" THEN posts <= 1 ELSE posts = 0
OnePublishedKey == intent # "committed" => diskKeys = {cursor.source}
CursorFrontier == IF cursor.source = Compacted
                  THEN <<cursor.through, cursor.prefix>> \in {<<0, "empty">>, <<9, "new-prefix">>}
                  ELSE <<cursor.through, cursor.prefix>> = <<Saved.through, Saved.prefix>>
OwnNameOnly == scenario = "foreign" => binding = Old /\ cursor = Saved /\ outcome = 0
ClearAfterRecord == ~inflight => outcome = 1
OneOutcome == outcome \in {0, 1}
OwnedHookSettles == (scenario \in {"renumber", "compact"} /\ hook = "accepted") ~> (~inflight /\ notice = "excluded")
CompactSettles == (scenario = "compact" /\ hook = "accepted")
                  ~> (~inflight /\ notice = "excluded" /\ posts = 1)
OldTicketSettles == (scenario = "oldTicket" /\ checkpoint = 2) ~> (~inflight /\ outcome = 1)
BootStopApplied == (scenario = "bootStop" /\ ready) ~> (cursor.source = Renumbered /\ checkpoint = 2)
FirstPinIsVerified == scenario \in (Unregistered \cup {"foreignFirstRecord"}) => hook # "accepted"
RegisteredPinAppears == (scenario = "registeredFirstPin") ~> (hook = "accepted")
=============================================================================
