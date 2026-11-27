------------------------ MODULE HookTranscriptRefresh ------------------------
EXTENDS Naturals
CONSTANT Refresh
VARIABLES scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
          notice, outcome, inflight, posts, restarted, intent, diskKeys
vars == <<scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
          notice, outcome, inflight, posts, restarted, intent, diskKeys>>

Old == [session |-> "A", path |-> "a", ino |-> 1, dev |-> 1]
Renumbered == [Old EXCEPT !.dev = 2]
Compacted == [session |-> "B", path |-> "b", ino |-> 2, dev |-> 2]
Candidate == IF scenario = "compact" THEN Compacted ELSE Renumbered
Live == IF scenario = "foreign" THEN Old ELSE Candidate
SameFile(a, b) == a.session = b.session /\ a.path = b.path /\ a.ino = b.ino
Start(source) == [source |-> source, through |-> 0, prefix |-> "empty"]
Saved == [source |-> Old, through |-> 17, prefix |-> "old-prefix"]

Init == /\ scenario \in {"renumber", "compact", "foreign"}
        /\ hook = "waiting" /\ binding = Old /\ seq = 1
        /\ cursor = Saved /\ checkpoint = 1
        /\ proof = Old /\ proofSeq = 1
        /\ notice = "pending" /\ outcome = FALSE /\ inflight = TRUE
        /\ posts = 0 /\ restarted = FALSE
        /\ intent = "absent" /\ diskKeys = {Old}

\* The pane name and incarnation have been checked before this durable log append.
Hook == /\ hook = "waiting"
        /\ hook' = IF scenario = "foreign" THEN "rejected" ELSE "accepted"
        /\ binding' = IF scenario = "compact" \/ (Refresh /\ scenario = "renumber")
                        THEN Candidate ELSE binding
        /\ seq' = IF binding' = binding THEN seq ELSE seq + 1
        /\ UNCHANGED <<scenario, cursor, checkpoint, proof, proofSeq,
                       notice, outcome, inflight, posts, restarted, intent, diskKeys>>

Stage == /\ scenario = "renumber" /\ SameFile(cursor.source, binding)
         /\ cursor.source # binding /\ intent = "absent" /\ intent' = "staged"
         /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
                        notice, outcome, inflight, posts, restarted, diskKeys>>
Commit == /\ intent = "staged" /\ intent' = "committed"
          /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
                         notice, outcome, inflight, posts, restarted, diskKeys>>
InstallKey == /\ intent = "committed" /\ binding \notin diskKeys
              /\ diskKeys' = diskKeys \cup {binding}
              /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint, proof, proofSeq,
                             notice, outcome, inflight, posts, restarted, intent>>

\* A committed intent is rolled forward before the checkpoint moves.
Apply == /\ checkpoint < seq
         /\ scenario = "compact" \/ intent = "committed" \/ cursor.source = binding
         /\ cursor' = IF SameFile(cursor.source, binding)
                         THEN [cursor EXCEPT !.source = binding]
                         ELSE Start(binding)
         /\ checkpoint' = seq
         /\ intent' = "absent" /\ diskKeys' = {binding}
         /\ UNCHANGED <<scenario, hook, binding, seq, proof, proofSeq,
                        notice, outcome, inflight, posts, restarted>>

Restart == /\ ~restarted
           /\ restarted' = TRUE /\ proof' = Old /\ proofSeq' = 0
           /\ cursor' = IF intent = "committed" THEN [cursor EXCEPT !.source = binding] ELSE cursor
           /\ intent' = "absent" /\ diskKeys' = {cursor'.source}
           /\ UNCHANGED <<scenario, hook, binding, seq, checkpoint,
                          notice, outcome, inflight, posts>>

Capture == /\ cursor.source = Compacted /\ cursor.through = 0
           /\ cursor' = [cursor EXCEPT !.through = 9, !.prefix = "new-prefix"]
           /\ posts' = posts + 1
           /\ UNCHANGED <<scenario, hook, binding, seq, checkpoint, proof, proofSeq,
                          notice, outcome, inflight, restarted, intent, diskKeys>>

Observe == /\ hook = "accepted" /\ cursor.source = Live /\ cursor.through > 0
           /\ intent # "committed"
           /\ proofSeq # checkpoint
           /\ proof' = cursor.source /\ proofSeq' = checkpoint
           /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint,
                          notice, outcome, inflight, posts, restarted, intent, diskKeys>>

Exact == hook = "accepted" /\ proofSeq = seq /\ proof = binding /\ proof = Live
Settle == /\ Exact /\ ~outcome
          /\ outcome' = TRUE
          /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint,
                         proof, proofSeq, notice, inflight, posts, restarted, intent, diskKeys>>
SettleNotice == /\ Exact /\ notice = "pending" /\ notice' = "excluded"
                /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint,
                               proof, proofSeq, outcome, inflight, posts, restarted, intent, diskKeys>>
Clear == /\ outcome /\ inflight /\ inflight' = FALSE
         /\ UNCHANGED <<scenario, hook, binding, seq, cursor, checkpoint,
                        proof, proofSeq, notice, outcome, posts, restarted, intent, diskKeys>>

Next == Hook \/ Stage \/ Commit \/ InstallKey \/ Apply \/ Restart \/ Capture
        \/ Observe \/ Settle \/ SettleNotice \/ Clear
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Stage) /\ WF_vars(Commit)
        /\ WF_vars(Hook) /\ WF_vars(Apply) /\ WF_vars(Capture)
        /\ WF_vars(Observe) /\ WF_vars(Settle) /\ WF_vars(SettleNotice) /\ WF_vars(Clear)

NoPrefixReplay == IF scenario = "compact" THEN posts <= 1 ELSE posts = 0
OnePublishedKey == intent # "committed" => diskKeys = {cursor.source}
CursorFrontier == IF cursor.source = Compacted
                  THEN <<cursor.through, cursor.prefix>> \in {<<0, "empty">>, <<9, "new-prefix">>}
                  ELSE <<cursor.through, cursor.prefix>> = <<Saved.through, Saved.prefix>>
OwnNameOnly == scenario = "foreign" => binding = Old /\ cursor = Saved /\ ~outcome
ClearAfterRecord == ~inflight => outcome
OwnedHookSettles == (hook = "accepted") ~> (~inflight /\ notice = "excluded")
CompactSettles == (scenario = "compact" /\ hook = "accepted")
                  ~> (~inflight /\ notice = "excluded" /\ posts = 1)
=============================================================================
