---- MODULE HerdrMissingBound ----
\* One exact hosted incarnation and one human front. Frozen code: 1161162a18.
\* The fixed policy is a proposal until its production implementation is checked.
EXTENDS Naturals
CONSTANTS Policy, Reading, Entry, Restored, RepairFails, LaunchFails, ReplaceNonce,
          Control, RebootMidProof
VARIABLES row, pane, nonce, captured, retired, phase, mode, input, notices,
          watching, active, checked, launched, rebooted, controlled
vars == <<row, pane, nonce, captured, retired, phase, mode, input, notices,
          watching, active, checked, launched, rebooted, controlled>>

Init ==
    /\ row = "Bound" /\ pane = Reading /\ nonce = 1 /\ captured = 0 /\ retired = 0
    /\ phase = IF Entry = "restart" THEN "boot" ELSE "awaiting"
    /\ mode = "boot" /\ input = IF Restored THEN "queued" ELSE "absent"
    /\ notices = 0 /\ watching = FALSE /\ active = (Entry = "running")
    /\ checked = FALSE /\ launched = FALSE /\ rebooted = FALSE /\ controlled = FALSE

\* Restart reader reuses the stored endpoint; no replacement pane is adopted.
Reconnect ==
    /\ phase = "boot" /\ phase' = "inspect" /\ mode' = "boot"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, input, notices,
                    watching, active, checked, launched, rebooted, controlled>>

\* start_idle -> intake_turn -> OwnerTurn::early, including a restored queue front.
Input ==
    /\ phase = "awaiting" /\ input \in {"absent", "queued"}
    /\ input' = "queued" /\ mode' = "input"
    /\ phase' = IF active THEN "held" ELSE "inspect"
    /\ watching' = active
    /\ UNCHANGED <<row, pane, nonce, captured, retired, notices, active, checked, launched, rebooted, controlled>>

\* Before: reconnect refuses Missing; idle starts arm the dead pane's Unknown probe.
Read ==
    /\ phase = "inspect" /\ (Control = "none" \/ mode = "boot" \/ controlled)
    /\ phase' = CASE row = "Retired" -> "launch"
                  [] pane = "Missing" /\ Policy \in {"fixed", "retire-only"} -> "proof"
                  [] pane = "Missing" -> IF mode = "boot" THEN "awaiting" ELSE "held"
                  [] pane = "Unknown" -> IF mode = "boot" THEN "awaiting"
                                         ELSE IF Policy = "before" THEN "held" ELSE "refuse"
                  [] OTHER -> IF mode = "boot" THEN "awaiting" ELSE "deliver"
    /\ checked' = (pane = "Missing" /\ Policy = "before")
    /\ captured' = nonce
    /\ watching' = (pane \in {"Missing", "Unknown"} /\ Policy = "before" /\ mode = "input")
    /\ UNCHANGED <<row, pane, nonce, retired, mode, input, notices, active, launched, rebooted, controlled>>

\* A new incarnation between the read and CAS invalidates the old proof.
Proof ==
    /\ phase = "proof" /\ (~RebootMidProof \/ rebooted) /\ phase' = "cas"
    /\ nonce' = IF ReplaceNonce THEN 2 ELSE nonce
    /\ pane' = IF ReplaceNonce THEN "Present" ELSE pane
    /\ UNCHANGED <<row, captured, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled>>

Retire ==
    /\ phase = "cas"
    /\ LET valid == ~RepairFails /\ nonce = captured /\ pane = "Missing"
       IN /\ row' = IF valid THEN "Retired" ELSE row
          /\ retired' = IF valid THEN captured ELSE retired
          /\ checked' = valid
          /\ phase' = IF mode = "boot" THEN "awaiting"
                       ELSE IF valid THEN "launch" ELSE "refuse"
    /\ UNCHANGED <<pane, nonce, captured, mode, input, notices, watching, active, launched, rebooted, controlled>>

\* Only the retired row enters a new launch. Failed normalization is never queued for retry.
Launch ==
    /\ phase = "launch" /\ row = "Retired"
    /\ phase' = IF ~LaunchFails THEN "deliver"
                 ELSE IF Policy = "retire-only" THEN "retained" ELSE "refuse"
    /\ input' = IF LaunchFails /\ Policy = "retire-only" THEN "retained" ELSE input
    /\ row' = IF LaunchFails THEN row ELSE "Bound"
    /\ pane' = IF LaunchFails THEN pane ELSE "Present"
    /\ nonce' = IF LaunchFails THEN nonce ELSE 2
    /\ launched' = ~LaunchFails
    /\ UNCHANGED <<captured, retired, mode, notices, watching, active, checked, rebooted, controlled>>

Deliver ==
    /\ phase = "deliver" /\ input = "queued" /\ phase' = "done"
    /\ input' = "delivered"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, notices,
                    watching, active, checked, launched, rebooted, controlled>>

Reject ==
    /\ phase = "refuse" /\ input = "queued" /\ phase' = "done"
    /\ notices' = notices + 1 /\ input' = "discarded" /\ watching' = FALSE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, active, checked, launched, rebooted, controlled>>

\* A working turn's temporary hold is retained until its existing turn boundary wake.
Boundary ==
    /\ phase = "held" /\ active /\ active' = FALSE
    /\ phase' = "inspect" /\ watching' = FALSE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices, checked, launched, rebooted, controlled>>

\* A crash loses the in-memory proof, preserving the exact row and already queued input.
Reboot ==
    /\ RebootMidProof /\ ~rebooted /\ phase = "proof"
    /\ rebooted' = TRUE /\ phase' = "boot" /\ captured' = 0 /\ checked' = FALSE
    /\ UNCHANGED <<row, pane, nonce, retired, mode, input, notices, watching, active, launched, controlled>>

\* Adjacent explicit controls are acknowledgements, not an automatic kill authority.
\* Cancel/clear/stop do not retire here; terminate represents its existing ended proof.
ControlAction ==
    /\ Control # "none" /\ ~controlled /\ phase = "inspect" /\ mode = "input"
    /\ controlled' = TRUE
    /\ row' = IF Control = "terminate" /\ pane = "Missing" THEN "Retired" ELSE row
    /\ retired' = IF Control = "terminate" /\ pane = "Missing" THEN nonce ELSE retired
    /\ LET removes == Control \in {"cancel", "clear"}
       IN /\ phase' = IF removes THEN "done" ELSE "inspect"
          /\ input' = IF removes THEN "discarded" ELSE input
          /\ notices' = IF removes THEN 1 ELSE notices
    /\ watching' = FALSE /\ active' = FALSE
    /\ UNCHANGED <<pane, nonce, captured, mode, checked, launched, rebooted>>

\* busy::probe maps the absent execution to Unknown forever in the frozen implementation.
HeldWatch ==
    /\ phase = "held" /\ watching /\ UNCHANGED vars

Next == Reconnect \/ Input \/ Read \/ Proof \/ Retire \/ Launch \/ Deliver \/ Reject
        \/ Boundary \/ HeldWatch \/ Reboot \/ ControlAction
Spec == Init /\ [][Next]_vars /\ WF_vars(Reconnect) /\ WF_vars(Input) /\ WF_vars(Read)
        /\ WF_vars(Proof) /\ WF_vars(Retire) /\ WF_vars(Launch) /\ WF_vars(Deliver)
        /\ WF_vars(Reject) /\ WF_vars(Boundary) /\ WF_vars(Reboot) /\ WF_vars(ControlAction)

MissingRetired == (checked /\ pane = "Missing" /\ ~RepairFails /\ ~ReplaceNonce) => row # "Bound"
NoIdleHeld == watching => active
NoUnknownRetire == Reading = "Unknown" => retired = 0
NoSuccessorRetire == ReplaceNonce => retired # 2
NoticeOnce == notices <= 1 /\ (input = "discarded" => notices = 1)
NoOldDelivery == (input = "delivered" /\ Reading = "Missing" /\ ~ReplaceNonce) => launched
NoRetainedRefusal == input # "retained"
InputResolves == <> (input \in {"delivered", "discarded"})
====
