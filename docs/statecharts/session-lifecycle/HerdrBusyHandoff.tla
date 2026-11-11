---- MODULE HerdrBusyHandoff ----
\* Slice A: no old fallback and no native-first readiness capability.
\* Production baseline 07b57b5724f6bede32f7cc0cf00c7a23652ff3c2.
EXTENDS Naturals, FiniteSets
CONSTANTS Policy, Entry, N, AlwaysFails, Failures, FirstRefusals, Blocked, HasDraft, Restarts, CrashPoint, BootCase, IdentityRace, Mutant
VARIABLES up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes
vars == <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>
Ids == 1..N
Terminal == {"done", "cancelled", "retained"}
Front == {i \in Ids : q[i] \in {"queued", "held"} /\ \A j \in 1..(i-1) : q[j] \in Terminal}
Init ==
    /\ up = (BootCase # "journal-only")
    /\ awake = (BootCase # "journal-only")
    /\ actor = (BootCase # "journal-only")
    /\ phase = (IF BootCase = "journal-only" THEN "bound" ELSE "legacy")
    /\ journal = (BootCase = "journal-only")
    /\ oldLive = (BootCase # "journal-only")
    /\ newLive = (BootCase = "journal-only")
    /\ peer = (0)
    /\ capPeer = (IF BootCase = "journal-only" THEN 2 ELSE 0)
    /\ pane = (1)
    /\ capPane = (1)
    /\ raced = (FALSE)
    /\ foreignClosed = (FALSE)
    /\ failures = (Failures)
    /\ notice = (FALSE)
    /\ retrySeen = (FALSE)
    /\ q = ([i \in Ids |-> IF BootCase = "journal-only" THEN "done" ELSE "incoming"])
    /\ probe = ([i \in Ids |-> FALSE])
    /\ attempt = ([i \in Ids |-> "none"])
    /\ effects = ([i \in Ids |-> IF BootCase = "journal-only" THEN 1 ELSE 0])
    /\ nativeLog = ([i \in Ids |-> BootCase = "journal-only"])
    /\ seen = ([i \in Ids |-> FALSE])
    /\ oldKeys = (0)
    /\ retained = (FALSE)
    /\ draft = ([i \in Ids |-> HasDraft])
    /\ executedDraft = (FALSE)
    /\ crashes = (0)

Probe(i) ==
    /\ up
    /\ q[i] = "incoming"
    /\ q' = ([q EXCEPT ![i] = "probing"])
    /\ probe' = ([probe EXCEPT ![i] = journal])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Commit(i) ==
    /\ up /\ awake
    /\ q[i] = "probing"
    /\ q' = ([q EXCEPT ![i] = IF Policy = "main" /\ Entry = "active" /\ ~journal THEN "blocked" ELSE "queued"])
    /\ journal' = (journal \/ ~(Policy = "main" /\ Entry = "active"))
    /\ phase' = (IF ~journal /\ ~(Policy = "main" /\ Entry = "active") THEN "revalidate" ELSE phase)
    /\ oldKeys' = (IF Mutant = "StaleIntake" /\ journal /\ ~probe[i] THEN 1 ELSE oldKeys)
    /\ UNCHANGED <<up, awake, actor, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, attempt, effects, nativeLog, seen, retained, draft, executedDraft, crashes>>

Revalidate ==
    /\ up /\ awake /\ journal
    /\ phase = "revalidate"
    /\ phase' = (IF oldLive THEN "closing" ELSE "ended")
    /\ capPeer' = (peer)
    /\ capPane' = (pane)
    /\ UNCHANGED <<up, awake, actor, journal, oldLive, newLive, peer, pane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

IdentityChanges ==
    /\ phase = "closing"
    /\ IdentityRace # "none" /\ ~raced
    /\ peer' = (IF IdentityRace = "server" THEN 1 ELSE peer)
    /\ pane' = (IF IdentityRace = "pane" THEN 2 ELSE pane)
    /\ oldLive' = (FALSE)
    /\ raced' = (TRUE)
    /\ UNCHANGED <<up, awake, actor, phase, journal, newLive, capPeer, capPane, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

BeforeExitRefusal ==
    /\ up /\ awake /\ phase = "closing"
    /\ Policy = "main"
    /\ retained' = (TRUE)
    /\ phase' = ("stuck")
    /\ UNCHANGED <<up, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, draft, executedDraft, crashes>>

CloseFails ==
    /\ up /\ awake /\ phase = "closing"
    /\ Policy = "fixed" /\ (AlwaysFails \/ failures > 0)
    /\ IdentityRace = "none" \/ raced
    /\ phase' = ("held-close")
    /\ failures' = (IF failures > 0 THEN failures - 1 ELSE 0)
    /\ notice' = (TRUE)
    /\ oldKeys' = (IF Mutant = "OldFallback" THEN 1 ELSE oldKeys)
    /\ UNCHANGED <<up, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, retrySeen, q, probe, attempt, effects, nativeLog, seen, retained, draft, executedDraft, crashes>>

RetryClose ==
    /\ up /\ awake /\ phase = "held-close"
    /\ Mutant # "NoRetry"
    /\ phase' = ("revalidate")
    /\ retrySeen' = (TRUE)
    /\ UNCHANGED <<up, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Close ==
    /\ up /\ awake /\ phase = "closing"
    /\ Policy = "fixed" /\ ~AlwaysFails /\ failures = 0
    /\ IdentityRace = "none" \/ raced
    /\ oldLive /\ capPeer = peer /\ capPane = pane
    /\ phase' = ("checking")
    /\ UNCHANGED <<up, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

UnsafeClose ==
    /\ phase = "closing" /\ raced
    /\ Mutant = "ReconnectNumber"
    /\ foreignClosed' = (TRUE)
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

ProviderEnds ==
    /\ phase = "checking" /\ oldLive
    /\ oldLive' = (FALSE)
    /\ UNCHANGED <<up, awake, actor, phase, journal, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

VerifyAbsent ==
    /\ up /\ awake /\ journal
    /\ ~oldLive /\ phase \in {"closing", "checking"}
    /\ IdentityRace = "none" \/ raced
    /\ phase' = ("ended")
    /\ UNCHANGED <<up, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Launch ==
    /\ up /\ awake /\ phase = "ended"
    /\ ~oldLive
    /\ phase' = ("bound")
    /\ newLive' = (TRUE)
    /\ UNCHANGED <<up, awake, actor, journal, oldLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Refuse(i) ==
    /\ up /\ awake /\ phase = "bound"
    /\ i \in Front
    /\ attempt[i] = "none"
    /\ (FirstRefusals /\ q[i] = "queued") \/ Blocked \/ draft[i]
    /\ q' = ([q EXCEPT ![i] = "held"])
    /\ attempt' = ([attempt EXCEPT ![i] = "no-key"])
    /\ retained' = (retained \/ Mutant = "UntypedHeld")
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, effects, nativeLog, seen, oldKeys, draft, executedDraft, crashes>>

WakeFirst(i) ==
    /\ up /\ awake /\ i \in Front
    /\ q[i] = "held" /\ attempt[i] = "no-key"
    /\ ~Blocked /\ ~draft[i]
    /\ Mutant # "NoFirstWake"
    /\ q' = ([q EXCEPT ![i] = "queued"])
    /\ attempt' = ([attempt EXCEPT ![i] = "ready"])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Reserve(i) ==
    /\ up /\ awake /\ phase = "bound"
    /\ i \in Front
    /\ ~Blocked \/ Mutant = "DraftBypass"
    /\ ~draft[i] \/ Mutant = "DraftBypass"
    /\ attempt[i] \in {"none", "ready"}
    /\ ~FirstRefusals \/ attempt[i] = "ready" \/ Mutant = "DraftBypass"
    /\ q' = ([q EXCEPT ![i] = "reserved"])
    /\ attempt' = ([attempt EXCEPT ![i] = IF i = 1 THEN "created" ELSE "existing"])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

CommitMayHaveSent(i) ==
    /\ up /\ awake /\ q[i] = "reserved"
    /\ q' = ([q EXCEPT ![i] = "may"])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Effect(i) ==
    /\ up /\ awake /\ q[i] = "may"
    /\ CrashPoint # "before-effect" \/ crashes = Restarts
    /\ q' = ([q EXCEPT ![i] = "sent"])
    /\ effects' = ([effects EXCEPT ![i] = @ + 1])
    /\ executedDraft' = (executedDraft \/ draft[i])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, attempt, nativeLog, seen, oldKeys, retained, draft, crashes>>

NativeReceipt(i) ==
    /\ effects[i] = 1 /\ ~nativeLog[i]
    /\ nativeLog' = ([nativeLog EXCEPT ![i] = TRUE])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, seen, oldKeys, retained, draft, executedDraft, crashes>>

ReadReceipt(i) ==
    /\ up /\ awake /\ nativeLog[i]
    /\ q[i] \in {"sent", "uncertain"}
    /\ seen' = ([seen EXCEPT ![i] = TRUE])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, oldKeys, retained, draft, executedDraft, crashes>>

Settle(i) ==
    /\ up /\ awake /\ seen[i]
    /\ q[i] \in {"sent", "uncertain"}
    /\ q' = ([q EXCEPT ![i] = "done"])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

TickUnknown(i) ==
    /\ up /\ awake /\ q[i] = "uncertain"
    /\ q' = [q EXCEPT ![i] = "retained"]
    /\ retained' = TRUE
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, attempt, effects, nativeLog, seen, oldKeys, draft, executedDraft, crashes>>

ReplayUnknown(i) ==
    /\ Mutant = "ReplayUnknown" /\ q[i] = "uncertain"
    /\ q' = ([q EXCEPT ![i] = "may"])
    /\ UNCHANGED <<up, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Crash ==
    /\ up /\ crashes < Restarts
    /\ IF CrashPoint = "before-effect" THEN \E i \in Ids : q[i] = "may" ELSE IF CrashPoint = "after-effect" THEN \E i \in Ids : q[i] = "sent" ELSE TRUE
    /\ up' = (FALSE)
    /\ awake' = (FALSE)
    /\ actor' = (FALSE)
    /\ crashes' = (crashes + 1)
    /\ capPeer' = (2)
    /\ seen' = ([i \in Ids |-> FALSE])
    /\ q' = ([i \in Ids |-> IF q[i] \in {"may", "sent"} THEN "uncertain" ELSE IF q[i] = "reserved" THEN "queued" ELSE q[i]])
    /\ UNCHANGED <<phase, journal, oldLive, newLive, peer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, probe, attempt, effects, nativeLog, oldKeys, retained, draft, executedDraft>>

Census ==
    /\ ~up
    /\ up' = (TRUE)
    /\ actor' = ((\E i \in Ids : q[i] \notin Terminal) \/ (journal /\ Mutant # "NoJournalCensus"))
    /\ UNCHANGED <<awake, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

BootWake ==
    /\ up /\ actor /\ ~awake
    /\ Mutant # "NoBootWake"
    /\ awake' = (TRUE)
    /\ UNCHANGED <<up, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Complete ==
    /\ up /\ awake /\ phase = "bound" /\ journal
    /\ \A i \in Ids : q[i] \in Terminal
    /\ phase' = ("complete")
    /\ journal' = (FALSE)
    /\ UNCHANGED <<up, awake, actor, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignClosed, failures, notice, retrySeen, q, probe, attempt, effects, nativeLog, seen, oldKeys, retained, draft, executedDraft, crashes>>

Wait == UNCHANGED vars
Next == Wait \/ Revalidate \/ IdentityChanges \/ BeforeExitRefusal \/ CloseFails \/ RetryClose \/ Close \/ UnsafeClose \/ ProviderEnds \/ VerifyAbsent \/ Launch \/ Crash \/ Census \/ BootWake \/ Complete \/ (\E i \in Ids : Probe(i) \/ Commit(i) \/ Refuse(i) \/ WakeFirst(i) \/ Reserve(i) \/ CommitMayHaveSent(i) \/ Effect(i) \/ NativeReceipt(i) \/ ReadReceipt(i) \/ Settle(i) \/ TickUnknown(i) \/ ReplayUnknown(i))
Fairness == WF_vars(Revalidate) /\ WF_vars(IdentityChanges) /\ WF_vars(BeforeExitRefusal) /\ WF_vars(CloseFails) /\ WF_vars(RetryClose) /\ WF_vars(Close) /\ WF_vars(ProviderEnds) /\ WF_vars(VerifyAbsent) /\ WF_vars(Launch) /\ WF_vars(Census) /\ WF_vars(BootWake) /\ WF_vars(Complete) /\ (\A i \in Ids : WF_vars(Probe(i)) /\ WF_vars(Commit(i)) /\ WF_vars(Refuse(i)) /\ WF_vars(WakeFirst(i)) /\ WF_vars(Reserve(i)) /\ WF_vars(CommitMayHaveSent(i)) /\ WF_vars(Effect(i)) /\ WF_vars(NativeReceipt(i)) /\ WF_vars(ReadReceipt(i)) /\ WF_vars(Settle(i)) /\ WF_vars(TickUnknown(i)))
Spec == Init /\ [][Next]_vars /\ Fairness /\ (CrashPoint = "before-effect" => WF_vars(Crash))
TypeOK == effects \in [Ids -> 0..2] /\ crashes \in 0..Restarts
InputAtMostOnce == \A i \in Ids : effects[i] <= 1
NoSilentLoss == \A i \in Ids : q[i] = "done" => effects[i] = 1
\* Crash recovery keeps main's Unknown-to-retained contract.
NoRetained == (crashes = 0 => ~retained)
NoOldKeys == oldKeys = 0
NoHumanDraftExecution == ~executedDraft
NoForeignClose == ~foreignClosed
OneProvider == ~(oldLive /\ newLive)
MessageProgress == \A i \in Ids : (q[i] # "incoming") ~> (q[i] \in Terminal)
HandoffProgress == ((journal \/ (\E i \in Ids : q[i] # "incoming")) /\ ~AlwaysFails) ~> (phase \in {"bound", "complete"})
RetryProgress == (AlwaysFails /\ journal) ~> retrySeen
JournalProgress == (journal /\ ~AlwaysFails) ~> ~journal
=============================================================================
