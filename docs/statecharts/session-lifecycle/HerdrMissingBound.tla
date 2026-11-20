---- MODULE HerdrMissingBound ----
\* One exact hosted incarnation and one human front, including the early Busy effect race.
EXTENDS Naturals
CONSTANTS Policy, Reading, Entry, Restored, RepairFails, LaunchFails, ReplaceNonce,
          Control, RebootMidProof
VARIABLES row, pane, nonce, captured, retired, phase, mode, input, notices,
          watching, active, checked, launched, rebooted, controlled, debt, rest, submissions
vars == <<row, pane, nonce, captured, retired, phase, mode, input, notices,
          watching, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

EffectRace == Entry \in {"effect-idle", "effect-active"}
RegistrationRace == Entry = "registration"
NoticeFails == Entry = "notice-retry"

Init ==
    /\ row = "Bound" /\ pane \in (IF RegistrationRace THEN {"Busy", "Unknown", "Unreadable", "Foreign"}
                      ELSE {IF EffectRace THEN "Present" ELSE Reading}) /\ nonce = 1 /\ captured = 0 /\ retired = 0
    /\ phase = IF Entry = "restart" THEN "boot" ELSE "awaiting"
    /\ mode = "boot" /\ input = IF Restored THEN "queued" ELSE "absent"
    /\ notices = 0 /\ watching = FALSE /\ active = (Entry \in {"running", "effect-active"})
    /\ checked = FALSE /\ launched = FALSE /\ rebooted = FALSE /\ controlled = FALSE
    /\ debt = FALSE /\ rest = ~NoticeFails /\ submissions = 0

\* Restart reader reuses the stored endpoint; no replacement pane is adopted.
Reconnect ==
    /\ phase = "boot" /\ phase' = "inspect" /\ mode' = "boot"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

\* start_idle -> intake_turn -> OwnerTurn::early, including a restored queue front.
Input ==
    /\ phase = "awaiting" /\ input \in {"absent", "queued"}
    /\ input' = "queued" /\ mode' = "input"
    /\ phase' = IF RegistrationRace THEN "registration-failed"
                 ELSE IF EffectRace THEN "early-busy" ELSE IF active THEN "held" ELSE "inspect"
    /\ watching' = (active /\ ~EffectRace)
    /\ UNCHANGED <<row, pane, nonce, captured, retired, notices, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

\* Before: reconnect refuses Missing; idle starts arm the dead pane's Unknown probe.
Read ==
    /\ phase = "inspect" /\ (Control = "none" \/ mode = "boot" \/ controlled)
    /\ phase' = CASE row = "Retired" -> "launch"
                  [] pane = "Missing" /\ Policy # "before" -> "proof"
                  [] pane = "Missing" -> IF mode = "boot" THEN "awaiting" ELSE "held"
                  [] pane = "Unknown" -> IF mode = "boot" THEN "awaiting"
                                         ELSE IF Policy = "before" THEN "held" ELSE "refuse"
                  [] OTHER -> IF mode = "boot" THEN "awaiting" ELSE "deliver"
    /\ checked' = (pane = "Missing" /\ Policy = "before")
    /\ captured' = nonce
    /\ watching' = (pane \in {"Missing", "Unknown"} /\ Policy = "before" /\ mode = "input")
    /\ UNCHANGED <<row, pane, nonce, retired, mode, input, notices, active, launched, rebooted, controlled, debt, rest, submissions>>

\* A new incarnation between the read and CAS invalidates the old proof.
Proof ==
    /\ phase = "proof" /\ (~RebootMidProof \/ rebooted) /\ phase' = "cas"
    /\ nonce' = IF ReplaceNonce THEN 2 ELSE nonce
    /\ pane' = IF ReplaceNonce THEN "Present" ELSE pane
    /\ UNCHANGED <<row, captured, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

Retire ==
    /\ phase = "cas"
    /\ LET valid == ~RepairFails /\ nonce = captured /\ pane = "Missing"
       IN /\ row' = IF valid THEN "Retired" ELSE row
          /\ retired' = IF valid THEN captured ELSE retired
          /\ checked' = valid
          /\ phase' = IF mode = "boot" THEN "awaiting"
                       ELSE IF valid THEN "launch" ELSE "refuse"
    /\ UNCHANGED <<pane, nonce, captured, mode, input, notices, watching, active, launched, rebooted, controlled, debt, rest, submissions>>

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
    /\ UNCHANGED <<captured, retired, mode, notices, watching, active, checked, rebooted, controlled, debt, rest, submissions>>

Deliver ==
    /\ phase = "deliver" /\ input = "queued" /\ phase' = "done"
    /\ input' = "delivered" /\ submissions' = submissions + 1
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest>>

Reject ==
    /\ phase = "refuse" /\ input = "queued"
    /\ phase' = IF NoticeFails THEN "notice-fail" ELSE "done"
    /\ notices' = IF NoticeFails THEN notices ELSE notices + 1
    /\ input' = "discarded" /\ watching' = FALSE
    /\ debt' = (NoticeFails /\ Policy # "before-notice")
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, active, checked,
                    launched, rebooted, controlled, rest, submissions>>

FirstNoticeFails ==
    /\ phase = "notice-fail" /\ phase' = IF debt THEN "outbox" ELSE "done"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

RestRecovers ==
    /\ phase = "outbox" /\ ~rest /\ rest' = TRUE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, phase, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, submissions>>

OutboxPost ==
    /\ phase = "outbox" /\ rest /\ debt
    /\ notices' = notices + 1 /\ debt' = FALSE /\ phase' = "done"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input,
                    watching, active, checked, launched, rebooted, controlled, rest, submissions>>

\* An already completed failure or delivered nonce cannot create another notice.
RepeatFailure ==
    /\ phase = "done" /\ input = "discarded" /\ UNCHANGED vars

\* Busy registration failed with no active mailbox turn. Missing must be observed.
RegistrationFailed ==
    /\ phase = "registration-failed" /\ ~active
    /\ LET refuse == IF pane = "Foreign" THEN Policy # "hold-refusal"
                      ELSE Policy = "discard-unproven"
       IN /\ phase' = IF refuse THEN "refuse" ELSE "registration-held"
          /\ watching' = ~refuse
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices,
                    active, checked, launched, rebooted, controlled, debt, rest, submissions>>

\* The next conclusive pane read either permits the existing Idle path or proves absence.
RegistrationObservation ==
    /\ phase = "registration-held" /\ pane \in {"Busy", "Unknown", "Unreadable"}
    /\ pane' \in {"Idle", "Missing"}
    /\ UNCHANGED <<row, nonce, captured, retired, phase, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

RegistrationWake ==
    /\ phase = "registration-held" /\ watching /\ Policy # "no-registration-wake"
    /\ pane \in {"Idle", "Missing"} /\ phase' = "inspect" /\ watching' = FALSE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices,
                    active, checked, launched, rebooted, controlled, debt, rest, submissions>>

\* Entry 43: the prepared Busy transaction has not captured or written yet.
EarlyBusy ==
    /\ phase = "early-busy" /\ captured' = nonce /\ phase' = "effect-gate"
    /\ UNCHANGED <<row, pane, nonce, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

LosePane ==
    /\ phase = "effect-gate" /\ phase' = "capture-failed"
    /\ pane' = IF ReplaceNonce THEN "Present" ELSE Reading
    /\ nonce' = IF ReplaceNonce THEN 2 ELSE nonce
    /\ UNCHANGED <<row, captured, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

BusyEffect ==
    /\ phase = "capture-failed" /\ phase' = "effect-held" /\ watching' = TRUE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices,
                    active, checked, launched, rebooted, controlled, debt, rest, submissions>>

\* The probe only requests the existing drive; retirement/disposal stays in Read and CAS.
EffectWatch ==
    /\ phase = "effect-held" /\ watching
    /\ LET ready == pane = "Present" \/ (pane = "Missing" /\ Policy # "before-watch")
       IN /\ watching' = ~ready
          /\ phase' = IF ~ready THEN phase ELSE IF active THEN "held" ELSE "inspect"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices,
                    active, checked, launched, rebooted, controlled, debt, rest, submissions>>

\* Unknown alone is no absence proof; this control assumes a later readable observation.
UnknownRecovers ==
    /\ phase = "effect-held" /\ pane = "Unknown" /\ pane' = "Present"
    /\ UNCHANGED <<row, nonce, captured, retired, phase, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest, submissions>>

\* A working turn's temporary hold is retained until its existing turn boundary wake.
Boundary ==
    /\ phase \in {"held", "effect-held"} /\ active /\ active' = FALSE
    /\ phase' = "inspect" /\ watching' = FALSE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices, checked, launched, rebooted, controlled, debt, rest, submissions>>

\* A crash loses the in-memory proof, preserving the exact row and already queued input.
Reboot ==
    /\ RebootMidProof /\ ~rebooted /\ phase = "proof"
    /\ rebooted' = TRUE /\ phase' = "boot" /\ captured' = 0 /\ checked' = FALSE
    /\ UNCHANGED <<row, pane, nonce, retired, mode, input, notices, watching, active, launched, controlled, debt, rest, submissions>>

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
    /\ UNCHANGED <<pane, nonce, captured, mode, checked, launched, rebooted, debt, rest, submissions>>

\* busy::probe maps the absent execution to Unknown forever in the frozen implementation.
HeldWatch ==
    /\ phase = "held" /\ watching /\ UNCHANGED vars

Next == Reconnect \/ Input \/ Read \/ Proof \/ Retire \/ Launch \/ Deliver \/ Reject
        \/ Boundary \/ HeldWatch \/ Reboot \/ ControlAction
        \/ EarlyBusy \/ LosePane \/ BusyEffect \/ EffectWatch \/ UnknownRecovers
        \/ RegistrationFailed \/ RegistrationObservation \/ RegistrationWake
        \/ FirstNoticeFails \/ RestRecovers \/ OutboxPost \/ RepeatFailure
Spec == Init /\ [][Next]_vars /\ WF_vars(Reconnect) /\ WF_vars(Input) /\ WF_vars(Read)
        /\ WF_vars(Proof) /\ WF_vars(Retire) /\ WF_vars(Launch) /\ WF_vars(Deliver)
        /\ WF_vars(Reject) /\ WF_vars(Boundary) /\ WF_vars(Reboot) /\ WF_vars(ControlAction)
        /\ WF_vars(EarlyBusy) /\ WF_vars(LosePane) /\ WF_vars(BusyEffect)
        /\ WF_vars(EffectWatch) /\ WF_vars(UnknownRecovers)
        /\ WF_vars(RegistrationFailed) /\ WF_vars(RegistrationObservation) /\ WF_vars(RegistrationWake)
        /\ WF_vars(FirstNoticeFails) /\ WF_vars(RestRecovers) /\ WF_vars(OutboxPost)

MissingRetired == (checked /\ pane = "Missing" /\ ~RepairFails /\ ~ReplaceNonce) => row # "Bound"
NoIdleHeld == watching => (active \/ phase = "effect-held")
ActiveOriginalHeld == (active /\ input # "absent") => input = "queued"
WatchResolves == [](watching /\ pane = "Missing" => <> ~watching)
NoUnknownRetire == Reading = "Unknown" => retired = 0
NoSuccessorRetire == ReplaceNonce => retired # 2
NoticeOnce == notices <= 1 /\ (input = "discarded" => notices = 1 \/ debt)
NoOldDelivery == (input = "delivered" /\ Reading = "Missing" /\ ~ReplaceNonce) => launched
NoRetainedRefusal == input # "retained"
InputResolves == <> (input = "delivered" \/ (input = "discarded" /\ notices = 1))
ReasonDelivered == [](input = "discarded" => <> (notices = 1))
RegistrationSafety == submissions <= 1 /\ (input = "discarded" => pane = "Foreign")
RegistrationResolves == <> (submissions = 1 \/ (pane = "Foreign" /\ input = "discarded"))
====
