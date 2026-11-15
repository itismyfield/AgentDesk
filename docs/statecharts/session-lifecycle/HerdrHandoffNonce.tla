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
\* IdPolicy: "before" compares (dev, ino) exactly, "fixed" also admits a proven renumbering.
EXTENDS Naturals, TLC
CONSTANTS Policy, Mutant, Ascii, Start, ResumeLogged, Direct, Transcript, IdPolicy
VARIABLES tmux, herdrPane, marker, hostKind, recorded, journal, msg, captured, rebooted, taken
vars == <<tmux, herdrPane, marker, hostKind, recorded, journal, msg, captured, rebooted, taken>>

\* t0: the tmux execution that logged the latest record; t1: the tmux resume that replaced it;
\* h1: a failed herdr create; h2: the new herdr execution.
Init == /\ tmux = IF Start = "alive" THEN "alive" ELSE "ended"
        /\ herdrPane = IF Start = "failedcreate" THEN "alive" ELSE "none"
        /\ marker = IF Start = "failedcreate" THEN "h1" ELSE "t1"
        /\ hostKind = IF Start = "failedcreate" THEN "herdr" ELSE "tmux"
        /\ recorded = IF ResumeLogged THEN "t1" ELSE "t0"
        /\ journal = "none" /\ msg = "queued" /\ captured = FALSE /\ rebooted = FALSE
        /\ taken = "none"

Mismatch == marker # recorded
\* The old execution the marker names is a tmux one that is proven gone.
TmuxEnded == hostKind = "tmux" /\ tmux = "ended"
Refused ==
    CASE Policy = "before" -> Mismatch
      [] Mutant = "NoEndProof" -> Mismatch /\ hostKind # "tmux"
      [] Mutant = "NoHostCheck" -> Mismatch /\ tmux # "ended"
      [] OTHER -> Mismatch /\ ~TmuxEnded
\* The transcript check after the nonce check: only the recorded file passes, under its new
\* device number once the old tmux execution is proven ended.
IdentityOk ==
    \/ Transcript = "same"
    \/ /\ IdPolicy = "fixed"
       /\ (Transcript = "renumbered" \/ (Mutant = "IdNoOwn" /\ Transcript = "foreign")
           \/ (Mutant = "IdNoInode" /\ Transcript = "rewritten"))
       /\ (TmuxEnded \/ Mutant = "IdNoEnd")
OtherAlive == tmux = "alive" \/ herdrPane = "alive"
\* The pinned close accepts a non-ASCII name only when the name check is fixed.
CloseWorks == Ascii \/ Policy = "fixed"

\* The input owner begins a journal for a Legacy front and closes the live pane first.
Begin == /\ msg = "queued" /\ journal = "none" /\ Start # "failedcreate"
         /\ journal' = IF tmux = "ended" THEN "ended" ELSE "begun"
         /\ UNCHANGED <<tmux, herdrPane, marker, hostKind, recorded, msg, captured, rebooted, taken>>
Close == /\ journal = "begun" /\ CloseWorks
         /\ tmux' = "ended" /\ journal' = "ended"
         /\ UNCHANGED <<herdrPane, marker, hostKind, recorded, msg, captured, rebooted, taken>>
\* Kill-tmux ends the pane and leaves every marker in place.
KillTmux == /\ tmux = "alive" /\ tmux' = "ended"
            /\ UNCHANGED <<herdrPane, marker, hostKind, recorded, journal, msg, captured, rebooted, taken>>
Ready == \/ journal = "ended"
         \/ Start = "failedcreate" /\ journal = "none"
         \/ Direct /\ journal # "ended"
Launch == /\ msg \in {"queued", "rejected"} /\ Ready
          /\ IF Refused \/ ~IdentityOk
               THEN /\ msg' = "rejected"
                    /\ UNCHANGED <<tmux, herdrPane, marker, hostKind, recorded, journal, captured, taken>>
               ELSE /\ captured' = (captured \/ OtherAlive) /\ taken' = Transcript
                    /\ marker' = "h2" /\ hostKind' = "herdr" /\ recorded' = "h2"
                    /\ msg' = "delivered" /\ journal' = "none"
                    /\ UNCHANGED <<tmux, herdrPane>>
          /\ UNCHANGED rebooted
\* A rejected input is resent by the user and judged again from the same markers.
Resend == /\ msg = "rejected" /\ msg' = "queued"
          /\ UNCHANGED <<tmux, herdrPane, marker, hostKind, recorded, journal, captured, rebooted, taken>>
\* Restart without file cleanup: markers, the binding log and the journal stay; only memory is rebuilt.
\* Boot orphan sweep and partial marker writes are not modeled.
\* The transcript, its first record and the binding record are files, so a restart judges them alike.
Boot == /\ ~rebooted /\ rebooted' = TRUE
        /\ UNCHANGED <<tmux, herdrPane, marker, hostKind, recorded, journal, msg, captured, taken>>

Next == Begin \/ Close \/ KillTmux \/ Launch \/ Resend \/ Boot
Spec == Init /\ [][Next]_vars /\ WF_vars(Begin) /\ WF_vars(Close) /\ WF_vars(Launch)
TypeOK == /\ tmux \in {"alive", "ended"} /\ herdrPane \in {"none", "alive"}
          /\ marker \in {"t1", "h1", "h2"} /\ hostKind \in {"tmux", "herdr"}
          /\ recorded \in {"t0", "t1", "h2"} /\ journal \in {"none", "begun", "ended"}
          /\ msg \in {"queued", "rejected", "delivered"}
          /\ taken \in {"none", "same", "renumbered", "rewritten", "foreign"}
\* Safety: no launch takes the binding while another execution still runs.
NoCapture == ~captured
\* Safety: a launch resumes only the recorded transcript file, never another session's or inode.
OwnTranscript == taken \in {"none", "same", "renumbered"}
\* Liveness: the first message after the switch reaches the new herdr session.
Delivered == <> (msg = "delivered")
====
