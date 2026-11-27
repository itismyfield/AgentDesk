---- MODULE HerdrRowlessMailbox ----
CONSTANT Mode, Shape, Disposition
VARIABLE host, mailbox, row, pinned, proof, proofActor, pending, sent, wrongRelease
vars == <<host, mailbox, row, pinned, proof, proofActor, pending, sent, wrongRelease>>

Init ==
    /\ host = "Busy"
    /\ mailbox = "A"
    /\ row = "Own"
    /\ pinned = FALSE
    /\ proof = "None"
    /\ proofActor = "None"
    /\ pending = TRUE
    /\ sent = FALSE
    /\ wrongRelease = FALSE

Register ==
    /\ proof = "None"
    /\ proof' = "Registered"
    /\ proofActor' = mailbox
    /\ pinned' = (Mode # "Before" /\ Shape # "Unpinned")
    /\ UNCHANGED <<host, mailbox, row, pending, sent, wrongRelease>>

RemoveProjection ==
    /\ Shape # "Busy"
    /\ proof = "Registered"
    /\ row = "Own"
    /\ row' = IF Shape = "Corrupt" THEN "Unreadable"
              ELSE IF Shape = "ForeignRow" THEN "Foreign" ELSE "Missing"
    /\ UNCHANGED <<host, mailbox, pinned, proof, proofActor, pending, sent, wrongRelease>>

ReplaceActor ==
    /\ Shape = "Successor"
    /\ proof = "Registered"
    /\ row = "Missing"
    /\ mailbox = "A"
    /\ mailbox' = "B"
    /\ UNCHANGED <<host, row, pinned, proof, proofActor, pending, sent, wrongRelease>>

ObserveUnknown ==
    /\ Shape = "Unknown"
    /\ proof = "Registered"
    /\ row = "Missing"
    /\ host = "Busy"
    /\ host' = "Unknown"
    /\ UNCHANGED <<mailbox, row, pinned, proof, proofActor, pending, sent, wrongRelease>>

Close ==
    /\ Shape # "Unknown"
    /\ proof = "Registered"
    /\ row # "Own"
    /\ (Shape # "Successor" \/ mailbox = "B")
    /\ host' = IF mailbox = "B" THEN "Busy" ELSE "Idle"
    /\ proof' = "Terminal"
    /\ UNCHANGED <<mailbox, row, pinned, proofActor, pending, sent, wrongRelease>>

OwnDisposition == Disposition \in {"Executed", "Cancelled"}

Settle ==
    /\ proof = "Terminal"
    /\ row = "Missing"
    /\ pinned
    /\ (mailbox = proofActor \/ (Mode = "NoActorGuard" /\ mailbox # "None"))
    /\ (Disposition = "Executed" \/ (Disposition = "Cancelled" /\ Mode # "W5")
        \/ Mode = "NoDispositionGuard")
    /\ mailbox' = "None"
    /\ proof' = "Settled"
    /\ wrongRelease' = (mailbox # proofActor)
    /\ UNCHANGED <<host, row, pinned, proofActor, pending, sent>>

Send ==
    /\ pending
    /\ mailbox = "None"
    /\ host = "Idle"
    /\ sent' = TRUE
    /\ pending' = FALSE
    /\ UNCHANGED <<host, mailbox, row, pinned, proof, proofActor, wrongRelease>>

Next == Register \/ RemoveProjection \/ ReplaceActor \/ ObserveUnknown \/ Close \/ Settle \/ Send
Spec ==
    /\ Init /\ [][Next]_vars
    /\ WF_vars(Register) /\ WF_vars(RemoveProjection) /\ WF_vars(ReplaceActor)
    /\ WF_vars(ObserveUnknown) /\ WF_vars(Close) /\ WF_vars(Settle) /\ WF_vars(Send)

TypeOK ==
    /\ host \in {"Busy", "Idle", "Unknown"}
    /\ mailbox \in {"A", "B", "None"}
    /\ row \in {"Own", "Missing", "Unreadable", "Foreign"}
    /\ proof \in {"None", "Registered", "Terminal", "Settled"}
    /\ proofActor \in {"A", "B", "None"}
    /\ Disposition \in {"Executed", "Cancelled", "WithdrawnNotExecuted", "TransferredUnverified"}
    /\ pinned \in BOOLEAN /\ pending \in BOOLEAN /\ sent \in BOOLEAN
    /\ wrongRelease \in BOOLEAN

NoBusyMachineWrite == sent => host = "Idle"
OnlyOwnActorReleased == ~wrongRelease
UnprovenHeld ==
    (host = "Unknown" \/ Shape \in {"Unpinned", "Corrupt", "ForeignRow", "Busy"}
     \/ ~OwnDisposition) => mailbox = "A"
IdleFrontEventuallyDelivered == <>sent
BusyEventuallyRegistered == <>(proof = "Registered")
UnknownEventuallyObserved == <>(host = "Unknown")
TerminalEventuallyObserved == <>(proof \in {"Terminal", "Settled"})
====
