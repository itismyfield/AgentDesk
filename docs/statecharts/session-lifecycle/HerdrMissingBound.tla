---- MODULE HerdrMissingBound ----
\* One exact hosted incarnation and one human front, including the early Busy effect race.
EXTENDS Naturals
CONSTANTS Policy, Reading, Entry, Restored, RepairFails, LaunchFails, ReplaceNonce,
          Control, RebootMidProof
VARIABLES row, pane, nonce, captured, retired, phase, mode, input, notices,
          watching, active, checked, launched, rebooted, controlled, debt, rest, attach
vars == <<row, pane, nonce, captured, retired, phase, mode, input, notices,
          watching, active, checked, launched, rebooted, controlled, debt, rest>>

allVars == <<vars, attach>>

EffectRace == Entry \in {"effect-idle", "effect-active"}
NoticeFails == Entry = "notice-retry"

Init ==
    /\ row = "Bound" /\ pane = (IF EffectRace THEN "Present" ELSE Reading) /\ nonce = 1 /\ captured = 0 /\ retired = 0
    /\ phase = IF Entry = "restart" THEN "boot" ELSE "awaiting"
    /\ mode = "boot" /\ input = IF Restored THEN "queued" ELSE "absent"
    /\ notices = 0 /\ watching = FALSE /\ active = (Entry \in {"running", "effect-active"})
    /\ checked = FALSE /\ launched = FALSE /\ rebooted = FALSE /\ controlled = FALSE
    /\ debt = FALSE /\ rest = ~NoticeFails

\* Restart reader reuses the stored endpoint; no replacement pane is adopted.
Reconnect ==
    /\ phase = "boot" /\ phase' = "inspect" /\ mode' = "boot"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest>>

\* start_idle -> intake_turn -> OwnerTurn::early, including a restored queue front.
Input ==
    /\ phase = "awaiting" /\ input \in {"absent", "queued"}
    /\ input' = "queued" /\ mode' = "input"
    /\ phase' = IF EffectRace THEN "early-busy" ELSE IF active THEN "held" ELSE "inspect"
    /\ watching' = (active /\ ~EffectRace)
    /\ UNCHANGED <<row, pane, nonce, captured, retired, notices, active, checked, launched, rebooted, controlled, debt, rest>>

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
    /\ UNCHANGED <<row, pane, nonce, retired, mode, input, notices, active, launched, rebooted, controlled, debt, rest>>

\* A new incarnation between the read and CAS invalidates the old proof.
Proof ==
    /\ phase = "proof" /\ (~RebootMidProof \/ rebooted) /\ phase' = "cas"
    /\ nonce' = IF ReplaceNonce THEN 2 ELSE nonce
    /\ pane' = IF ReplaceNonce THEN "Present" ELSE pane
    /\ UNCHANGED <<row, captured, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest>>

Retire ==
    /\ phase = "cas"
    /\ LET valid == ~RepairFails /\ nonce = captured /\ pane = "Missing"
       IN /\ row' = IF valid THEN "Retired" ELSE row
          /\ retired' = IF valid THEN captured ELSE retired
          /\ checked' = valid
          /\ phase' = IF mode = "boot" THEN "awaiting"
                       ELSE IF valid THEN "launch" ELSE "refuse"
    /\ UNCHANGED <<pane, nonce, captured, mode, input, notices, watching, active, launched, rebooted, controlled, debt, rest>>

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
    /\ UNCHANGED <<captured, retired, mode, notices, watching, active, checked, rebooted, controlled, debt, rest>>

Deliver ==
    /\ phase = "deliver" /\ input = "queued" /\ phase' = "done"
    /\ input' = "delivered"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest>>

Reject ==
    /\ phase = "refuse" /\ input = "queued"
    /\ phase' = IF NoticeFails THEN "notice-fail" ELSE "done"
    /\ notices' = IF NoticeFails THEN notices ELSE notices + 1
    /\ input' = "discarded" /\ watching' = FALSE
    /\ debt' = (NoticeFails /\ Policy # "before-notice")
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, active, checked,
                    launched, rebooted, controlled, rest>>

FirstNoticeFails ==
    /\ phase = "notice-fail" /\ phase' = IF debt THEN "outbox" ELSE "done"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest>>

RestRecovers ==
    /\ phase = "outbox" /\ ~rest /\ rest' = TRUE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, phase, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt>>

OutboxPost ==
    /\ phase = "outbox" /\ rest /\ debt
    /\ notices' = notices + 1 /\ debt' = FALSE /\ phase' = "done"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input,
                    watching, active, checked, launched, rebooted, controlled, rest>>

\* An already completed failure or delivered nonce cannot create another notice.
RepeatFailure ==
    /\ phase = "done" /\ input = "discarded" /\ UNCHANGED vars

\* Entry 43: the prepared Busy transaction has not captured or written yet.
EarlyBusy ==
    /\ phase = "early-busy" /\ captured' = nonce /\ phase' = "effect-gate"
    /\ UNCHANGED <<row, pane, nonce, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest>>

LosePane ==
    /\ phase = "effect-gate" /\ phase' = "capture-failed"
    /\ pane' = IF ReplaceNonce THEN "Present" ELSE Reading
    /\ nonce' = IF ReplaceNonce THEN 2 ELSE nonce
    /\ UNCHANGED <<row, captured, retired, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest>>

BusyEffect ==
    /\ phase = "capture-failed" /\ phase' = "effect-held" /\ watching' = TRUE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices,
                    active, checked, launched, rebooted, controlled, debt, rest>>

\* The probe only requests the existing drive; retirement/disposal stays in Read and CAS.
EffectWatch ==
    /\ phase = "effect-held" /\ watching
    /\ LET ready == pane = "Present" \/ (pane = "Missing" /\ Policy # "before-watch")
       IN /\ watching' = ~ready
          /\ phase' = IF ~ready THEN phase ELSE IF active THEN "held" ELSE "inspect"
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices,
                    active, checked, launched, rebooted, controlled, debt, rest>>

\* Unknown alone is no absence proof; this control assumes a later readable observation.
UnknownRecovers ==
    /\ phase = "effect-held" /\ pane = "Unknown" /\ pane' = "Present"
    /\ UNCHANGED <<row, nonce, captured, retired, phase, mode, input, notices,
                    watching, active, checked, launched, rebooted, controlled, debt, rest>>

\* A working turn's temporary hold is retained until its existing turn boundary wake.
Boundary ==
    /\ phase \in {"held", "effect-held"} /\ active /\ active' = FALSE
    /\ phase' = "inspect" /\ watching' = FALSE
    /\ UNCHANGED <<row, pane, nonce, captured, retired, mode, input, notices, checked, launched, rebooted, controlled, debt, rest>>

\* A crash loses the in-memory proof, preserving the exact row and already queued input.
Reboot ==
    /\ RebootMidProof /\ ~rebooted /\ phase = "proof"
    /\ rebooted' = TRUE /\ phase' = "boot" /\ captured' = 0 /\ checked' = FALSE
    /\ UNCHANGED <<row, pane, nonce, retired, mode, input, notices, watching, active, launched, controlled, debt, rest>>

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
    /\ UNCHANGED <<pane, nonce, captured, mode, checked, launched, rebooted, debt, rest>>

\* busy::probe maps the absent execution to Unknown forever in the frozen implementation.
HeldWatch ==
    /\ phase = "held" /\ watching /\ UNCHANGED vars

Next == Reconnect \/ Input \/ Read \/ Proof \/ Retire \/ Launch \/ Deliver \/ Reject
        \/ Boundary \/ HeldWatch \/ Reboot \/ ControlAction
        \/ EarlyBusy \/ LosePane \/ BusyEffect \/ EffectWatch \/ UnknownRecovers
        \/ FirstNoticeFails \/ RestRecovers \/ OutboxPost \/ RepeatFailure
Spec == Init /\ attach = "off" /\ [][Next /\ UNCHANGED attach]_allVars
        /\ WF_vars(Reconnect) /\ WF_vars(Input) /\ WF_vars(Read)
        /\ WF_vars(Proof) /\ WF_vars(Retire) /\ WF_vars(Launch) /\ WF_vars(Deliver)
        /\ WF_vars(Reject) /\ WF_vars(Boundary) /\ WF_vars(Reboot) /\ WF_vars(ControlAction)
        /\ WF_vars(EarlyBusy) /\ WF_vars(LosePane) /\ WF_vars(BusyEffect)
        /\ WF_vars(EffectWatch) /\ WF_vars(UnknownRecovers)
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

\* #8280 boot-to-first-input projection. See the design evidence table.
\* The historical Missing Bound variables remain fixed in this projection.
AttachInit ==
    /\ Init
    /\ \E target \in (IF Entry = "owner-reattach" THEN {"idle", "busy", "dead", "rowOnly", "shellHeld"}
                        ELSE IF ReplaceNonce THEN {"idle"} ELSE {"idle", "busy", "dead", "rowOnly"}) :
       attach = [phase |-> "down", target |-> target, initial |-> target,
                 reconciled |-> FALSE, scan |-> FALSE, mirror |-> FALSE, owner |-> FALSE,
                 route |-> "none", changed |-> FALSE, deadWrite |-> FALSE,
                 delivered |-> 0, receipt |-> "none", redrive |-> FALSE,
                 banner |-> FALSE, notice |-> FALSE, reinjected |-> FALSE,
                 grant |-> "none", front |-> 1, claimed |-> 0, recovery |-> FALSE,
                 ownerControl |-> "none", submits |-> 0]

Boot ==
    /\ attach.phase = "down"
    /\ attach' = [attach EXCEPT !.phase = "await"]
    /\ UNCHANGED vars

FirstInput ==
    /\ attach.phase = "await"
    /\ attach' = [attach EXCEPT !.phase = "queued"]
    /\ UNCHANGED vars

ReconcileIntake ==
    /\ attach.phase # "down" /\ ~attach.reconciled
    /\ attach' = [attach EXCEPT !.reconciled = TRUE]
    /\ UNCHANGED vars

ReconnectSource ==
    /\ attach.phase # "down" /\ ~attach.scan
    /\ attach' = [attach EXCEPT !.scan = TRUE, !.mirror = attach.target \in {"idle", "busy"}]
    /\ UNCHANGED vars

ChooseRoute ==
    /\ attach.phase = "queued" /\ attach.reconciled
    /\ CASE attach.target = "rowOnly" /\ Policy # "trust-row" ->
               attach' = [attach EXCEPT !.phase = "waiting"]
         [] attach.target = "dead" ->
               attach' = [attach EXCEPT !.phase = "start", !.route = "ended"]
         [] OTHER ->
               attach' = [attach EXCEPT !.phase = IF attach.target = "busy" THEN "waiting" ELSE "submit",
                          !.route = "existing", !.owner = Policy # "before" /\ attach.target # "rowOnly",
                          !.banner = Policy = "before", !.notice = Policy = "before",
                          !.reinjected = Policy = "before"]
    /\ UNCHANGED vars

ReadinessChanges ==
    /\ attach.phase = "waiting"
    /\ attach.target \in {"busy", "rowOnly"}
    /\ \E nextTarget \in (IF attach.target = "busy" THEN {"idle"} ELSE {"idle", "dead"}) :
       attach' = [attach EXCEPT !.target = nextTarget,
                  !.phase = IF Policy = "no-wake" THEN "waiting" ELSE "queued"]
    /\ UNCHANGED vars

LosePaneBeforeEffect ==
    /\ ReplaceNonce /\ ~attach.changed /\ attach.phase = "submit"
    /\ attach' = [attach EXCEPT !.changed = TRUE, !.target = "dead", !.owner = FALSE, !.mirror = FALSE]
    /\ UNCHANGED vars

RecheckEffect ==
    /\ attach.phase = "submit" /\ attach.target = "dead" /\ Policy # "no-recheck"
    /\ attach' = [attach EXCEPT !.phase = "queued", !.route = "none"]
    /\ UNCHANGED vars

SubmitExisting ==
    /\ attach.phase = "submit" /\ (~ReplaceNonce \/ attach.changed)
    /\ (attach.owner \/ Policy \in {"before", "no-recheck", "trust-row"})
    /\ (attach.target = "idle" \/ Policy \in {"no-recheck", "trust-row"})
    /\ attach' = [attach EXCEPT !.phase = "done", !.owner = attach.target = "idle",
                  !.deadWrite = attach.target # "idle",
                  !.delivered = @ + (IF attach.target = "idle" THEN 1 ELSE 0),
                  !.receipt = IF attach.target = "idle" THEN "confirmed" ELSE "none"]
    /\ UNCHANGED vars

ResumeEnded ==
    /\ attach.phase = "start" /\ attach.target = "dead"
    /\ attach' = [attach EXCEPT !.phase = "resumed", !.target = "idle", !.owner = TRUE,
                  !.banner = Policy # "no-recovery-effects", !.notice = Policy # "no-recovery-effects",
                  !.reinjected = Policy # "no-recovery-effects"]
    /\ UNCHANGED vars

SubmitResumed ==
    /\ attach.phase = "resumed"
    /\ attach' = [attach EXCEPT !.phase = "done", !.delivered = @ + 1,
                  !.receipt = "confirmed"]
    /\ UNCHANGED vars

RedriveFront ==
    /\ attach.phase = "done" /\ ~attach.redrive
    /\ attach.receipt = "confirmed"
    /\ attach' = [attach EXCEPT !.redrive = TRUE,
                  !.phase = IF Policy # "duplicate" THEN @
                            ELSE IF attach.route = "existing" THEN "submit" ELSE "resumed"]
    /\ UNCHANGED vars

AttachDone ==
    /\ attach.phase = "done" /\ UNCHANGED attach
    /\ UNCHANGED vars

AttachNext == Boot \/ FirstInput \/ ReconcileIntake \/ ReconnectSource \/ ChooseRoute
              \/ ReadinessChanges \/ LosePaneBeforeEffect \/ RecheckEffect \/ SubmitExisting
              \/ ResumeEnded \/ SubmitResumed \/ RedriveFront \/ AttachDone

AttachSpec ==
    /\ AttachInit /\ [][AttachNext]_allVars
    /\ WF_attach(Boot) /\ WF_attach(FirstInput) /\ WF_attach(ReconcileIntake)
    /\ WF_attach(ReconnectSource) /\ WF_attach(ChooseRoute) /\ WF_attach(ReadinessChanges)
    /\ WF_attach(LosePaneBeforeEffect) /\ WF_attach(RecheckEffect) /\ WF_attach(SubmitExisting)
    /\ WF_attach(ResumeEnded) /\ WF_attach(SubmitResumed)

NoLiveRestore ==
    attach.route = "existing" => ~(attach.banner \/ attach.notice \/ attach.reinjected)
EndedResumeShowsRecovery ==
    attach.route = "ended" /\ attach.phase \in {"resumed", "done"}
        => attach.banner /\ attach.notice /\ attach.reinjected
NoDeadPaneWrite == ~attach.deadWrite
ExactlyOnce == attach.delivered <= 1 /\ (attach.phase = "done" => attach.delivered = 1)
FirstInputDelivered == [](attach.phase = "queued" => <>(attach.phase = "done" /\ attach.delivered = 1))

\* The owner projection exercises the existing ticket, grant, reserve and receipt boundary.
\* The foreground-only shell case keeps its own alive/unreadable provider proof pending.
OwnerInit ==
    /\ AttachInit
    /\ attach.target \in {"idle", "busy", "dead", "rowOnly", "shellHeld"}

OwnerSelect ==
    /\ attach.phase = "queued" /\ attach.reconciled
    /\ CASE attach.target = "dead" ->
             attach' = [attach EXCEPT !.phase = "waiting", !.recovery = TRUE]
         [] attach.target \in {"rowOnly", "shellHeld"} ->
             attach' = [attach EXCEPT !.phase = "waiting"]
         [] attach.target = "busy" ->
             \/ attach' = [attach EXCEPT !.phase = "waiting"]
             \/ attach' = [attach EXCEPT !.phase = "granted", !.grant = "busy"]
         [] OTHER ->
             attach' = [attach EXCEPT !.phase = "granted", !.grant = "idle"]
    /\ UNCHANGED vars

OwnerWake ==
    /\ attach.phase = "waiting" /\ Policy # "owner-no-wake"
    /\ attach' = [attach EXCEPT !.phase = "queued", !.target = "idle",
                   !.grant = "none", !.claimed = 0]
    /\ UNCHANGED vars

OwnerClaim ==
    /\ attach.phase = "granted" /\ attach.grant \in {"idle", "busy"}
    /\ attach' = [attach EXCEPT !.phase = "reserved", !.claimed = attach.front]
    /\ UNCHANGED vars

OwnerCatchUp ==
    /\ attach.phase \in {"waiting", "granted", "reserved", "unknown", "done"}
    /\ ~attach.redrive
    /\ attach' = [attach EXCEPT !.redrive = TRUE]
    /\ UNCHANGED vars

OwnerInterrupt ==
    /\ attach.phase = "reserved" /\ attach.ownerControl = "none"
    /\ Control \in {"death", "stop", "reset"}
    /\ attach' = [attach EXCEPT !.ownerControl = Control,
                   !.target = IF Control = "death" THEN "dead" ELSE @]
    /\ UNCHANGED vars

OwnerRejectBeforeKey ==
    /\ attach.phase = "reserved" /\ attach.ownerControl \in {"death", "stop", "reset"}
    /\ Policy # "owner-no-first-key"
    /\ attach' = [attach EXCEPT !.phase = "waiting", !.ownerControl = "settled",
                   !.claimed = 0, !.grant = "none"]
    /\ UNCHANGED vars

OwnerSubmit ==
    /\ attach.phase = "reserved" /\ attach.claimed = attach.front /\ attach.submits < 2
    /\ (Control = "none" \/ attach.ownerControl # "none")
    /\ (attach.ownerControl \in {"none", "settled"} \/ Policy = "owner-no-first-key")
    /\ \E receipt \in {"confirmed", "unknown"} :
        attach' = [attach EXCEPT !.phase = IF receipt = "confirmed" THEN "done" ELSE "unknown",
                   !.submits = @ + 1, !.receipt = receipt,
                   !.deadWrite = attach.ownerControl \in {"death", "stop", "reset"}]
    /\ UNCHANGED vars

OwnerReceipt ==
    /\ attach.phase = "unknown"
    /\ attach' = [attach EXCEPT !.phase = "done", !.receipt = "confirmed"]
    /\ UNCHANGED vars

OwnerRepeatUnknown ==
    /\ Policy = "owner-resend-unknown" /\ attach.phase = "unknown"
    /\ attach' = [attach EXCEPT !.phase = "reserved"]
    /\ UNCHANGED vars

OwnerNext ==
    Boot \/ FirstInput \/ ReconcileIntake \/ ReconnectSource \/ OwnerSelect \/ OwnerWake
    \/ OwnerClaim \/ OwnerCatchUp \/ OwnerInterrupt \/ OwnerRejectBeforeKey
    \/ OwnerSubmit \/ OwnerReceipt \/ OwnerRepeatUnknown \/ AttachDone

OwnerAttachSpec ==
    /\ OwnerInit /\ [][OwnerNext]_allVars
    /\ WF_attach(Boot) /\ WF_attach(FirstInput) /\ WF_attach(ReconcileIntake)
    /\ WF_attach(OwnerSelect) /\ WF_attach(OwnerWake) /\ WF_attach(OwnerClaim)
    /\ WF_attach(OwnerInterrupt) /\ WF_attach(OwnerRejectBeforeKey)
    /\ WF_attach(OwnerSubmit) /\ WF_attach(OwnerReceipt)

SameOriginal ==
    attach.front = 1 /\ (attach.phase \in {"reserved", "unknown", "done"} => attach.claimed = attach.front)
OwnerExactlyOnce ==
    attach.submits <= 1 /\ (attach.phase \in {"unknown", "done"} => attach.submits = 1)
BeforeFirstKey == ~attach.deadWrite
OwnerDelivers ==
    [](attach.phase = "queued" => <>(attach.phase = "done" /\ attach.submits = 1))


\* The revised parity projection keeps the first input under the original owner.
ParityTargets ==
    IF Control \in {"busy", "unproven", "duplicate", "other-nonce", "headless-race"} THEN {"busy"}
    ELSE {"idle", "busy", "dead", "unknown"}

ParityNames ==
    CASE Control = "duplicate" -> {"duplicate"}
      [] Control = "other-nonce" -> {"other"}
      [] Control \in {"busy", "unproven", "headless-race"} -> {"unique"}
      [] OTHER -> {"unique", "duplicate", "other"}

ParityInit ==
    /\ Init
    /\ \E t \in ParityTargets, n \in ParityNames, composer \in {"open", "modal"} :
       \E unproven \in (IF Control = "unproven" THEN {TRUE}
                       ELSE IF Control = "all" /\ t = "busy" THEN BOOLEAN ELSE {FALSE}) :
           attach = [phase |-> "down", initial |-> t, target |-> t, name |-> n,
                     composer |-> composer, unproven |-> unproven, reconciled |-> FALSE,
                     front |-> 1, claimed |-> 0, route |-> "none", reattached |-> 0,
                     locationFresh |-> FALSE, lease |-> FALSE, grant |-> "none",
                     provedEnd |-> FALSE, submits |-> 0, headlessClaim |-> "none"]

ParityAssess ==
    /\ attach.phase = "queued" /\ attach.reconciled
    /\ CASE attach.unproven /\ Policy = "parity-before" ->
               attach' = [attach EXCEPT !.phase = "discarded"]
         [] attach.target = "unknown" ->
               attach' = [attach EXCEPT !.phase = "identityHeld"]
         [] attach.target = "dead" ->
               attach' = [attach EXCEPT !.phase = "endProved", !.provedEnd = TRUE]
         [] OTHER ->
               attach' = [attach EXCEPT !.phase = IF attach.name = "unique"
                                                    \/ Policy = "parity-trust-name"
                                                THEN "named" ELSE "exact"]
    /\ UNCHANGED vars

ParityObserve ==
    /\ attach.phase = "identityHeld"
    /\ \E observed \in {"idle", "busy", "dead"} :
       attach' = [attach EXCEPT !.target = observed, !.phase = "queued"]
    /\ UNCHANGED vars

ParityAttach ==
    /\ attach.phase \in {"named", "exact"} /\ attach.reattached = 0
    /\ attach.headlessClaim # "new"
    /\ attach' = [attach EXCEPT !.phase = "attached", !.route = attach.phase,
                  !.locationFresh = attach.phase = "named", !.reattached = @ + 1,
                  !.lease = attach.target = "busy",
                  !.grant = IF attach.target = "busy" THEN "busy" ELSE "idle"]
    /\ UNCHANGED vars

\* The headless claim uses the same channel transition as reattach. The previous Busy
\* execution takes its existing lease before the new claimant can mint a mailbox turn.
ParityHeadlessClaim ==
    /\ Control = "headless-race" /\ attach.target = "busy"
    /\ attach.phase \in {"named", "exact", "attached", "reserved"}
    /\ attach.headlessClaim = "none"
    /\ attach' = [attach EXCEPT !.headlessClaim =
                     IF Policy = "parity-headless-first" /\ ~attach.lease
                     THEN "new" ELSE "refused"]
    /\ UNCHANGED vars

ParityInputRule ==
    /\ attach.phase = "attached"
    /\ attach' = [attach EXCEPT !.phase =
                     IF attach.composer = "modal" THEN "composerHeld"
                     ELSE IF attach.target = "busy" /\ Policy = "parity-before" THEN "idleOnly"
                     ELSE "reserved",
                  !.claimed = attach.front]
    /\ UNCHANGED vars

ParityComposerClears ==
    /\ attach.phase = "composerHeld"
    /\ attach' = [attach EXCEPT !.phase = "attached", !.composer = "open"]
    /\ UNCHANGED vars

ParitySubmit ==
    /\ attach.phase = "reserved" /\ attach.claimed = attach.front /\ attach.submits < 2
    /\ \E receipt \in {"confirmed", "unknown"} :
       attach' = [attach EXCEPT !.phase = IF receipt = "confirmed" THEN "receipt" ELSE "unknown",
                     !.submits = @ + 1]
    /\ UNCHANGED vars

ParityObserveReceipt ==
    /\ attach.phase = "unknown"
    /\ attach' = [attach EXCEPT !.phase = "receipt"]
    /\ UNCHANGED vars

ParityResendUnknown ==
    /\ Policy = "parity-resend-unknown" /\ attach.phase = "unknown" /\ attach.submits = 1
    /\ attach' = [attach EXCEPT !.phase = "reserved"]
    /\ UNCHANGED vars

ParityPublishReply ==
    /\ attach.phase = "receipt"
    /\ attach' = [attach EXCEPT !.phase = "done"]
    /\ UNCHANGED vars

ParityReject ==
    /\ attach.phase = "endProved" /\ attach.provedEnd
    /\ attach' = [attach EXCEPT !.phase = "rejected"]
    /\ UNCHANGED vars

ParityNext ==
    Boot \/ FirstInput \/ ReconcileIntake \/ ParityAssess \/ ParityObserve \/ ParityAttach
    \/ ParityHeadlessClaim \/ ParityInputRule \/ ParityComposerClears \/ ParitySubmit \/ ParityObserveReceipt
    \/ ParityResendUnknown \/ ParityPublishReply \/ ParityReject \/ AttachDone

ParitySpec ==
    /\ ParityInit /\ [][ParityNext]_allVars
    /\ WF_attach(Boot) /\ WF_attach(FirstInput) /\ WF_attach(ReconcileIntake)
    /\ WF_attach(ParityAssess) /\ WF_attach(ParityObserve) /\ WF_attach(ParityAttach)
    /\ WF_attach(ParityInputRule) /\ WF_attach(ParityComposerClears) /\ WF_attach(ParitySubmit)
    /\ WF_attach(ParityObserveReceipt) /\ WF_attach(ParityPublishReply) /\ WF_attach(ParityReject)

NoLiveDiscard == attach.target \in {"idle", "busy"} => attach.phase # "discarded"
NoIdleOnly == attach.phase # "idleOnly"
NamedOnlyCurrent == attach.route = "named" => attach.name = "unique" /\ attach.locationFresh
BusyAuthority == attach.grant = "busy" => attach.lease /\ attach.reattached = 1
NoCompetingClaim == attach.headlessClaim # "new"
ProvenRejection == attach.phase = "rejected" => attach.provedEnd /\ attach.target = "dead"
ParityResolves ==
    [](attach.phase = "queued" => <> ((attach.phase = "done" /\ attach.submits = 1)
                                  \/ (attach.phase = "rejected" /\ attach.provedEnd)))

====
