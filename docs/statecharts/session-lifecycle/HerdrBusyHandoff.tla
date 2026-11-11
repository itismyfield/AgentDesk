---- MODULE HerdrBusyHandoff ----
\* Claude forward handoff refinement of HerdrSession/HostMigration; design, not landed code.
\* Code baseline 1fe3a57d266e1e7fe384796d22a925076f1d99fc; see herdr-busy-handoff.report.md.
EXTENDS Naturals, FiniteSets
CONSTANTS Fixed, Screen, HasDraft, StopFailures, InitiallyRetained, N, Restarts, Mutant, Controls
VARIABLES server, crashes, awake, phase, oldLive, newLive, newSession,
          remainingFailures, draft, notice, draftLost, draftExecuted,
          q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete
vars == <<server, crashes, awake, phase, oldLive, newLive, newSession,
          remainingFailures, draft, notice, draftLost, draftExecuted,
          q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
Ids == 1..N
Owed == {i \in Ids : q[i] \notin {"done", "cancelled", "lost"}}
Front == {i \in Ids : q[i] = "queued" /\ \A j \in 1..(i-1) : q[j] \in {"done", "cancelled"}}
Init ==
    /\ server = "up" /\ crashes = 0 /\ awake = TRUE /\ phase = "legacy"
    /\ oldLive = TRUE /\ newLive = FALSE /\ newSession = "none"
    /\ remainingFailures = StopFailures
    /\ draft = HasDraft /\ notice = FALSE /\ draftLost = FALSE /\ draftExecuted = FALSE
    /\ q = [i \in Ids |-> IF InitiallyRetained THEN "retained" ELSE "queued"]
    /\ chosen = [i \in Ids |-> "none"] /\ effects = [i \in Ids |-> 0]
    /\ receipt = [i \in Ids |-> "none"] /\ parked = FALSE /\ resendAsks = 0 /\ readable = (Screen # "unread")
    /\ oldComplete = [i \in Ids |-> FALSE]

\* Existing recover excludes attempt-less/told retained rows (retained.rs:283-294).
\* Proposed strict Claude handoff migration moves the originals in one owner snapshot.
RecoverRetained ==
    /\ server = "up" /\ Fixed /\ Mutant # "NoRetainedRecovery"
    /\ \E i \in Ids : q[i] = "retained"
    /\ q' = [i \in Ids |-> IF q[i] = "retained" THEN "queued" ELSE q[i]]
    /\ awake' = TRUE
    /\ UNCHANGED <<server, crashes, phase, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
Begin ==
    /\ server = "up" /\ awake /\ phase = "legacy" /\ Front # {}
    /\ phase' = "stopping"
    /\ UNCHANGED <<server, crashes, awake, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* Current when_ready_to_exit/ready_then: busy or unmeasured stash refuses before content.
BeforeRefuse ==
    /\ server = "up" /\ ~Fixed /\ phase = "stopping" /\ Screen # "ready"
    /\ q' = [i \in Ids |-> IF q[i] = "queued" THEN "retained" ELSE q[i]]
    /\ resendAsks' = resendAsks + 1
    /\ phase' = "legacy"
    /\ UNCHANGED <<server, crashes, awake, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, chosen, effects, receipt, parked, readable, oldComplete>>
TellDraftPolicy ==
    /\ server = "up" /\ phase = "stopping" /\ Fixed /\ ~notice
    /\ notice' = TRUE
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* A failed stop does not certify absence. Only a new exact-live observation permits fallback.
StopFailed ==
    /\ server = "up" /\ Fixed /\ phase = "stopping" /\ remainingFailures > 0
    /\ phase' = "fallback" /\ remainingFailures' = remainingFailures - 1
    /\ UNCHANGED <<server, crashes, awake, oldLive, newLive, newSession, draft, notice,
                    draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
SignalStop ==
    /\ server = "up" /\ phase = "stopping" /\ remainingFailures = 0
    /\ (Fixed \/ Screen = "ready")
    /\ (notice \/ ~draft \/ Mutant = "NoDraftNotice")
    /\ phase' = "checking"
    /\ UNCHANGED <<server, crashes, awake, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* Environment assumption: the verified stop eventually ends this provider (not a replacement).
ProviderEnds ==
    /\ phase = "checking" /\ oldLive
    /\ oldLive' = FALSE /\ draftLost' = (draftLost \/ draft) /\ draft' = FALSE
    /\ UNCHANGED <<server, crashes, awake, phase, newLive, newSession, remainingFailures,
                    notice, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
VerifyEnd ==
    /\ server = "up" /\ phase = "checking" /\ ~oldLive
    /\ phase' = "ended"
    /\ UNCHANGED <<server, crashes, awake, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* Matches durable Pending -> create/evidence -> exact resumed SessionStart -> attach.
Launch ==
    /\ server = "up" /\ phase = "ended" /\ ~oldLive /\ ~newLive
    /\ phase' = "pending"
    /\ UNCHANGED <<server, crashes, awake, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
Create ==
    /\ server = "up" /\ phase = "pending" /\ ~newLive
    /\ newLive' = TRUE /\ phase' = "created"
    /\ newSession' = IF Mutant = "WrongSession" THEN "B" ELSE "A"
    /\ UNCHANGED <<server, crashes, awake, oldLive, remainingFailures, draft, notice,
                    draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
Attach ==
    /\ server = "up" /\ phase = "created" /\ newLive
    /\ phase' = "ready"
    /\ UNCHANGED <<server, crashes, awake, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* Persist the one destination before content; no second route while its attempt is ambiguous.
Choose(i) ==
    /\ server = "up" /\ awake /\ i \in Front /\ chosen[i] = "none"
    /\ phase \in {"ready", "fallback"}
    /\ chosen' = [chosen EXCEPT ![i] = IF phase = "ready" THEN "new" ELSE "old"]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, q, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* A fallback input never submits a human draft. A measured draft is parked on the old pane.
ParkDraft ==
    /\ server = "up" /\ phase = "fallback" /\ draft /\ ~parked /\ readable
    /\ parked' = TRUE
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, q, chosen, effects, receipt, resendAsks, readable, oldComplete>>
Arm(i) ==
    /\ server = "up" /\ awake /\ i \in Front /\ chosen[i] # "none"
    /\ (chosen[i] = "new" => phase = "ready" /\ newLive)
    /\ (chosen[i] = "old" => phase = "fallback" /\ oldLive /\ readable /\ (~draft \/ parked))
    /\ q' = [q EXCEPT ![i] = "armed"]
    /\ receipt' = [receipt EXCEPT ![i] = "no-key"]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, chosen, effects, parked, resendAsks, readable, oldComplete>>
\* Backend send and owner settlement are separate; crash may intervene between them.
Submit(i) ==
    /\ server = "up" /\ q[i] = "armed" /\ receipt[i] = "no-key"
    /\ effects' = [effects EXCEPT ![i] = @ + 1]
    /\ q' = [q EXCEPT ![i] = "await-receipt"]
    /\ receipt' = [receipt EXCEPT ![i] = IF Mutant = "LostReceipt" THEN "unknown" ELSE "provider-evidence"]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, chosen, parked, resendAsks, readable, oldComplete>>
Confirm(i) ==
    /\ server = "up" /\ q[i] \in {"await-receipt", "recover"}
    /\ receipt[i] = "provider-evidence"
    /\ q' = [q EXCEPT ![i] = "done"]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* This abstract proof MUST be a writer's exact NoKeyYet evidence, never transcript absence.
RecoverNoKey(i) ==
    /\ server = "up" /\ q[i] = "recover" /\ receipt[i] = "no-key"
    /\ q' = [q EXCEPT ![i] = "armed"]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* A receipt alone is not completion: never stop an old-route queued input just after Enter.
OldCompletes(i) ==
    /\ phase = "fallback" /\ chosen[i] = "old" /\ q[i] = "done" /\ ~oldComplete[i]
    /\ oldComplete' = [oldComplete EXCEPT ![i] = TRUE]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, q, chosen,
                    effects, receipt, parked, resendAsks, readable>>
FallbackDone ==
    /\ server = "up" /\ phase = "fallback"
    /\ \E i \in Ids : chosen[i] = "old" /\ q[i] \in {"done", "cancelled"} /\ (oldComplete[i] \/ q[i] = "cancelled")
    /\ ~\E i \in Ids : chosen[i] = "old" /\ (q[i] # "cancelled" /\ (q[i] # "done" \/ ~oldComplete[i]))
    /\ phase' = "stopping"
    /\ UNCHANGED <<server, crashes, awake, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
Crash ==
    /\ server = "up" /\ crashes < Restarts
    /\ server' = "down" /\ crashes' = crashes + 1 /\ awake' = FALSE
    /\ q' = [i \in Ids |-> IF q[i] \in {"armed", "await-receipt"} THEN "recover" ELSE q[i]]
    /\ UNCHANGED <<phase, oldLive, newLive, newSession, remainingFailures, draft, notice,
                    draftLost, draftExecuted, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
Boot ==
    /\ server = "down" /\ server' = "up"
    /\ awake' = (Mutant # "NoWake")
    /\ UNCHANGED <<crashes, phase, oldLive, newLive, newSession, remainingFailures,
                    draft, notice, draftLost, draftExecuted, q, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
Duplicate ==
    /\ Mutant = "Duplicate" /\ \E i \in Ids : q[i] = "done" /\ effects[i] = 1
    /\ \E i \in Ids : /\ q[i] = "done" /\ effects[i] = 1
                       /\ effects' = [effects EXCEPT ![i] = 2]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, q, chosen, receipt, parked, resendAsks, readable, oldComplete>>
SilentDrop ==
    /\ Mutant = "SilentDrop" /\ \E i \in Ids : q[i] = "queued"
    /\ \E i \in Ids : /\ q[i] = "queued" /\ q' = [q EXCEPT ![i] = "lost"]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, chosen, effects, receipt, parked, resendAsks, readable, oldComplete>>
\* External assumption for old-route fallback: its readable composer eventually returns.
RecoverScreen ==
    /\ server = "up" /\ phase = "fallback" /\ ~readable
    /\ readable' = TRUE
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, q, chosen,
                    effects, receipt, parked, resendAsks, oldComplete>>
\* A long-running owner may await events forever; PROPERTY, not deadlock, checks progress.
AwaitEvents == UNCHANGED vars
\* Explicit Cancel/clear before content revokes the original; it is an authorized terminal.
CancelBeforeContent(i) ==
    /\ Controls /\ server = "up" /\ q[i] \in {"queued", "retained", "armed", "recover"}
    /\ receipt[i] \in {"none", "no-key"}
    /\ q' = [q EXCEPT ![i] = "cancelled"]
    /\ UNCHANGED <<server, crashes, awake, phase, oldLive, newLive, newSession,
                    remainingFailures, draft, notice, draftLost, draftExecuted, chosen,
                    effects, receipt, parked, resendAsks, readable, oldComplete>>
Next == AwaitEvents \/ RecoverRetained \/ Begin \/ BeforeRefuse \/ TellDraftPolicy \/ StopFailed \/ SignalStop
        \/ ProviderEnds \/ VerifyEnd \/ Launch \/ Create \/ Attach \/ ParkDraft \/ FallbackDone
        \/ (\E i \in Ids : Choose(i) \/ Arm(i) \/ Submit(i) \/ Confirm(i) \/ RecoverNoKey(i) \/ OldCompletes(i) \/ CancelBeforeContent(i))
        \/ Crash \/ Boot \/ Duplicate \/ SilentDrop \/ RecoverScreen
Fairness == WF_vars(RecoverRetained) /\ WF_vars(Begin) /\ WF_vars(BeforeRefuse)
        /\ WF_vars(TellDraftPolicy) /\ WF_vars(StopFailed) /\ WF_vars(SignalStop)
        /\ WF_vars(ProviderEnds) /\ WF_vars(VerifyEnd) /\ WF_vars(Launch)
        /\ WF_vars(Create) /\ WF_vars(Attach) /\ WF_vars(ParkDraft) /\ WF_vars(FallbackDone)
        /\ WF_vars(Boot) /\ WF_vars(RecoverScreen)
        /\ \A i \in Ids : (WF_vars(Choose(i)) /\ WF_vars(Arm(i)) /\ WF_vars(Submit(i))
                            /\ WF_vars(Confirm(i)) /\ WF_vars(RecoverNoKey(i)) /\ WF_vars(OldCompletes(i)))
Spec == Init /\ [][Next]_vars /\ Fairness
TypeOK == /\ server \in {"up", "down"} /\ crashes \in 0..Restarts
          /\ q \in [Ids -> {"queued", "retained", "armed", "await-receipt", "recover", "done", "cancelled", "lost"}]
          /\ chosen \in [Ids -> {"none", "old", "new"}] /\ effects \in [Ids -> 0..2]
InputAtMostOnce == \A i \in Ids : effects[i] <= 1
NoSilentLoss == \A i \in Ids : q[i] # "lost" /\ (q[i] = "done" => effects[i] = 1)
NoUserResend == resendAsks = 0
DraftNeverExecuted == ~draftExecuted
DraftLossTold == draftLost => notice
OneProvider == ~(oldLive /\ newLive)
SameSession == newLive => newSession = "A"
MessageProgress == \A i \in Ids : (q[i] # "cancelled") ~> ((q[i] = "done" /\ effects[i] = 1) \/ q[i] = "cancelled")
HandoffProgress == (\E i \in Ids : q[i] # "cancelled")
                   ~> ((phase = "ready" /\ newLive /\ ~oldLive) \/ (\A i \in Ids : q[i] = "cancelled"))
=============================================================================
