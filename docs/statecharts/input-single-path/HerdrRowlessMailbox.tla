---- MODULE HerdrRowlessMailbox ----
CONSTANT Mode, Shape
VARIABLE host, mailbox, row, pinned, proof, pending, sent, wrongRelease
vars == <<host, mailbox, row, pinned, proof, pending, sent, wrongRelease>>

Init ==
    /\ host = "Busy"
    /\ mailbox = "Own"
    /\ row = "Own"
    /\ pinned = FALSE
    /\ proof = "None"
    /\ pending = TRUE
    /\ sent = FALSE
    /\ wrongRelease = FALSE

Register ==
    /\ proof = "None"
    /\ host = "Busy"
    /\ proof' = "Registered"
    /\ pinned' = (Mode # "Before" /\ Shape # "Unpinned")
    /\ UNCHANGED <<host, mailbox, row, pending, sent, wrongRelease>>

RemoveProjection ==
    /\ Shape # "Busy"
    /\ proof = "Registered"
    /\ row = "Own"
    /\ row' = IF Shape = "Corrupt" THEN "Unreadable"
              ELSE IF Shape = "ForeignRow" THEN "Foreign" ELSE "Missing"
    /\ UNCHANGED <<host, mailbox, pinned, proof, pending, sent, wrongRelease>>

Close ==
    /\ proof = "Registered"
    /\ row # "Own"
    /\ host = "Busy"
    /\ host' = "Idle"
    /\ proof' = "Terminal"
    /\ UNCHANGED <<mailbox, row, pinned, pending, sent, wrongRelease>>

Unclear ==
    /\ Shape = "Unknown"
    /\ host = "Busy"
    /\ row = "Missing"
    /\ host' = "Unknown"
    /\ UNCHANGED <<mailbox, row, pinned, proof, pending, sent, wrongRelease>>

ReplaceActor ==
    /\ Shape = "Successor"
    /\ proof = "Terminal"
    /\ host = "Idle"
    /\ mailbox = "Own"
    /\ host' = "Busy"
    /\ mailbox' = "Successor"
    /\ UNCHANGED <<row, pinned, proof, pending, sent, wrongRelease>>

Settle ==
    /\ proof = "Terminal"
    /\ row = "Missing"
    /\ pinned
    /\ IF Shape = "Successor" THEN mailbox = "Successor" ELSE TRUE
    /\ mailbox = "Own" \/ (Mode = "NoActorGuard" /\ mailbox = "Successor")
    /\ mailbox' = "None"
    /\ proof' = "Settled"
    /\ wrongRelease' = (mailbox = "Successor")
    /\ UNCHANGED <<host, row, pinned, pending, sent>>

Send ==
    /\ pending
    /\ mailbox = "None"
    /\ host = "Idle"
    /\ sent' = TRUE
    /\ pending' = FALSE
    /\ UNCHANGED <<host, mailbox, row, pinned, proof, wrongRelease>>

Next == Register \/ RemoveProjection \/ Unclear \/ Close \/ ReplaceActor \/ Settle \/ Send
Spec == Init /\ [][Next]_vars /\ WF_vars(Settle) /\ WF_vars(Send)
TypeOK ==
    /\ host \in {"Busy", "Idle", "Unknown"}
    /\ mailbox \in {"Own", "Successor", "None"}
    /\ row \in {"Own", "Missing", "Unreadable", "Foreign"}
    /\ proof \in {"None", "Registered", "Terminal", "Settled"}
    /\ pinned \in BOOLEAN /\ pending \in BOOLEAN /\ sent \in BOOLEAN
    /\ wrongRelease \in BOOLEAN
NoBusyMachineWrite == sent => host = "Idle"
OnlyOwnActorReleased == ~wrongRelease
UnprovenHeld ==
    (host = "Unknown" \/ Shape \in {"Unpinned", "Corrupt", "ForeignRow", "Busy"}) =>
        mailbox = "Own"
IdleFrontEventuallyDelivered ==
    (Shape = "Missing" /\ host = "Idle" /\ pending) ~> sent
====
