---- MODULE HerdrHandoffNonce ----
\* Companion to HerdrHandoffLaunch: what decides whether the herdr start after a tmux handoff
\* is refused. HerdrHandoffLaunch abstracts that outcome as AlwaysFails.
\* Start: "alive" tmux still runs at the switch, "killed" it was closed by kill-tmux first,
\* "failedcreate" an earlier herdr create left its marker and a pane that may still run.
\* ResumeLogged: the last tmux resume of the same transcript appended a binding record.
\* Direct: a launch judgment may run while the old tmux still runs (no journal close first).
\* Transcript: the file at the recorded path when judged. "same" the recorded (dev, ino);
\* "renumbered" the recorded inode on a device a host reboot renumbered, first record its own;
\* "rewritten" another inode naming the same session; "foreign" the recorded inode renumbered
\* but its first record names another session.
\* IdPolicy: "before" compares (dev, ino) exactly; "tmuxonly" admits a proven renumbering only after
\* the old tmux execution ended; "fixed" also after the binding's own Herdr execution was retired.
\* Start "herdrretired": the binding was logged by Herdr execution h1, whose row is Retired;
\* "herdrother": h1 logged it and may still run while the Retired row names another nonce h0;
\* "herdrlive": h1 logged it and may still run; the row names h1 Bound or Pending.
\* Verified: whether that Herdr record is a verified pin; a resume's own start logs a stat.
\* PinPolicy: "before" refuses an unverified Herdr record; "fixed" pins the recorded file once the
\* row names its execution Retired and the file's first record proves it.
\* Again: after a delivery the host reboots once more, retiring h2 and renumbering the volume again.
EXTENDS Naturals, TLC
CONSTANTS Policy, Mutant, Ascii, Start, ResumeLogged, Direct, Transcript, IdPolicy, Again,
          Verified, PinPolicy
VARIABLES tmux, herdrPane, marker, hostKind, recorded, journal, msg, captured, rebooted, taken,
          rowState, rowNonce, rounds, verified
vars == <<tmux, herdrPane, marker, hostKind, recorded, journal, msg, captured, rebooted, taken,
          rowState, rowNonce, rounds, verified>>
HerdrStart == Start \in {"herdrretired", "herdrother", "herdrlive"}

\* t0: the tmux execution that logged the latest record; t1: the tmux resume that replaced it;
\* h1: a failed herdr create; h2: the new herdr execution.
Init == /\ tmux = IF Start = "alive" THEN "alive" ELSE "ended"
        /\ herdrPane = IF Start \in {"failedcreate", "herdrother", "herdrlive"} THEN "alive" ELSE "none"
        /\ marker = IF Start = "failedcreate" \/ HerdrStart THEN "h1" ELSE "t1"
        /\ hostKind = IF Start = "failedcreate" \/ HerdrStart THEN "herdr" ELSE "tmux"
        /\ recorded = IF HerdrStart THEN "h1" ELSE IF ResumeLogged THEN "t1" ELSE "t0"
        /\ journal = "none" /\ msg = "queued" /\ captured = FALSE /\ rebooted = FALSE
        /\ taken = "none"
        /\ rowState = IF Start = "herdrlive" THEN "live" ELSE IF HerdrStart THEN "retired" ELSE "none"
        /\ rowNonce = IF Start \in {"herdrretired", "herdrlive"} THEN "h1" ELSE "h0"
        /\ rounds = 1
        /\ verified = (HerdrStart /\ Verified)

Mismatch == marker # recorded
\* The old execution the marker names is a tmux one that is proven gone.
TmuxEnded == hostKind = "tmux" /\ tmux = "ended"
Refused ==
    CASE Policy = "before" -> Mismatch
      [] Mutant = "NoEndProof" -> Mismatch /\ hostKind # "tmux"
      [] Mutant = "NoHostCheck" -> Mismatch /\ tmux # "ended"
      [] OTHER -> Mismatch /\ ~TmuxEnded
\* A Retired row is written only for the confirmed-ended execution of its owner and nonce.
HerdrRetired == /\ (rowState = "retired" \/ Mutant = "IdNoRetired")
                /\ (rowNonce = recorded \/ Mutant = "IdNoNonce")
EndProof == TmuxEnded \/ (IdPolicy = "fixed" /\ HerdrRetired)
\* A tmux record has no hook event, so the legacy branch takes it unverified. A Herdr record needs a
\* verified pin, or the recovery pin of its Retired execution's own file (first record proves it).
PinOk == \/ verified \/ recorded \in {"t0", "t1"}
         \/ /\ PinPolicy = "fixed"
            /\ (rowState = "retired" \/ Mutant = "RecNoRetired")
            /\ (rowNonce = recorded \/ Mutant = "RecNoNonce")
            /\ Transcript \in {"same", "renumbered"}
\* The transcript check after the nonce check: only the recorded file passes, under its new
\* device number once the execution that logged the binding is proven ended.
IdentityOk ==
    \/ Transcript = "same"
    \/ /\ IdPolicy \in {"tmuxonly", "fixed"}
       /\ (Transcript = "renumbered" \/ (Mutant = "IdNoOwn" /\ Transcript = "foreign")
           \/ (Mutant = "IdNoInode" /\ Transcript = "rewritten"))
       /\ (EndProof \/ Mutant = "IdNoEnd")
OtherAlive == tmux = "alive" \/ herdrPane = "alive"
\* The pinned close accepts a non-ASCII name only when the name check is fixed.
CloseWorks == Ascii \/ Policy = "fixed"

\* The input owner begins a journal for a Legacy front and closes the live pane first.
\* A Retired row is not a Legacy front, so no journal begins for it.
Begin == /\ msg = "queued" /\ journal = "none" /\ Start # "failedcreate" /\ ~HerdrStart
         /\ journal' = IF tmux = "ended" THEN "ended" ELSE "begun"
         /\ UNCHANGED <<tmux, herdrPane, marker, hostKind, recorded, msg, captured, rebooted, taken, rowState, rowNonce, rounds, verified>>
Close == /\ journal = "begun" /\ CloseWorks
         /\ tmux' = "ended" /\ journal' = "ended"
         /\ UNCHANGED <<herdrPane, marker, hostKind, recorded, msg, captured, rebooted, taken, rowState, rowNonce, rounds, verified>>
\* Kill-tmux ends the pane and leaves every marker in place.
KillTmux == /\ tmux = "alive" /\ tmux' = "ended"
            /\ UNCHANGED <<herdrPane, marker, hostKind, recorded, journal, msg, captured, rebooted, taken, rowState, rowNonce, rounds, verified>>
Ready == \/ journal = "ended"
         \/ (Start = "failedcreate" \/ HerdrStart) /\ journal = "none"
         \/ Direct /\ journal # "ended"
Launch == /\ msg \in {"queued", "rejected"} /\ Ready
          /\ IF ~PinOk \/ Refused \/ ~IdentityOk
               THEN /\ msg' = "rejected"
                    /\ UNCHANGED <<tmux, herdrPane, marker, hostKind, recorded, journal, captured, taken,
                                   rowState, rowNonce, verified>>
               ELSE /\ captured' = (captured \/ OtherAlive) /\ taken' = Transcript
                    /\ marker' = "h2" /\ hostKind' = "herdr" /\ recorded' = "h2"
                    /\ msg' = "delivered" /\ journal' = "none"
                    /\ herdrPane' = "alive" /\ rowState' = "bound" /\ rowNonce' = "h2"
                    \* The new execution's own resume start is a stat of the same file.
                    /\ verified' = FALSE
                    /\ UNCHANGED tmux
          /\ UNCHANGED <<rebooted, rounds>>
\* A rejected input is resent by the user and judged again from the same markers.
Resend == /\ msg = "rejected" /\ msg' = "queued"
          /\ UNCHANGED <<tmux, herdrPane, marker, hostKind, recorded, journal, captured, rebooted, taken, rowState, rowNonce, rounds, verified>>
\* Restart without file cleanup: markers, the binding log and the journal stay; only memory is rebuilt.
\* Boot orphan sweep and partial marker writes are not modeled.
\* The transcript, its first record and the binding record are files, so a restart judges them alike.
Boot == /\ ~rebooted /\ rebooted' = TRUE
        /\ UNCHANGED <<tmux, herdrPane, marker, hostKind, recorded, journal, msg, captured, taken, rowState, rowNonce, rounds, verified>>
\* A restart's reconnect logs a verified source only for a live listed execution, whose Bound row
\* takes the bound path and never this judgment, so it is not modeled.

\* Another host reboot: h2 ends, recovery retires its row and the volume is renumbered again.
Reboot == /\ Again /\ msg = "delivered" /\ rounds = 1
          /\ herdrPane' = "none" /\ rowState' = "retired" /\ msg' = "queued" /\ rounds' = 2
          /\ UNCHANGED <<tmux, marker, hostKind, recorded, journal, captured, rebooted, taken, rowNonce, verified>>

Next == Begin \/ Close \/ KillTmux \/ Launch \/ Resend \/ Boot \/ Reboot
Spec == Init /\ [][Next]_vars /\ WF_vars(Begin) /\ WF_vars(Close) /\ WF_vars(Launch)
        /\ WF_vars(Reboot)
TypeOK == /\ tmux \in {"alive", "ended"} /\ herdrPane \in {"none", "alive"}
          /\ marker \in {"t1", "h1", "h2"} /\ hostKind \in {"tmux", "herdr"}
          /\ recorded \in {"t0", "t1", "h1", "h2"} /\ journal \in {"none", "begun", "ended"}
          /\ msg \in {"queued", "rejected", "delivered"}
          /\ taken \in {"none", "same", "renumbered", "rewritten", "foreign"}
          /\ rowState \in {"none", "retired", "bound", "live"} /\ rowNonce \in {"h0", "h1", "h2"}
          /\ rounds \in 1..2 /\ verified \in BOOLEAN
\* Safety: no launch takes the binding while another execution still runs.
NoCapture == ~captured
\* Safety: a launch resumes only the recorded transcript file, never another session's or inode.
OwnTranscript == taken \in {"none", "same", "renumbered"}
\* Liveness: the first message after the switch reaches the new herdr session.
Delivered == <> (msg = "delivered")
\* Liveness: after the second reboot the next message reaches a new herdr session as well.
DeliveredAgain == <> (rounds = 2 /\ msg = "delivered")
====
