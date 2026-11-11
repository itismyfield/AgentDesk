---- MODULE HerdrBusyHandoff ----
\* Claude forward handoff design r2; production baseline 1fe3a57d266e1e7fe384796d22a925076f1d99fc.
\* Reviewed d1 artifact fae2d9ee41bf57897dcef1e56869b1a19b1265cc; no historical retained replay.
EXTENDS Naturals, FiniteSets
CONSTANTS Policy, Entry, OldUnread, HasDraft, StopFailures, FirstRefusals, N, Restarts, BootCase, IdentityRace, Mutant, Controls, RotateNative
VARIABLES server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks
vars == <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

Ids == 1..N
Terminal == {"done", "cancelled"}
HasInput == \E i \in Ids : q[i] \notin Terminal
Front == {i \in Ids : q[i] = "queued" /\ \A j \in 1..(i-1) : q[j] \in Terminal}
RaceReady == IdentityRace = "none" \/ raced
Init ==
    /\ server = IF BootCase = "journal-only" THEN "down" ELSE "up"
    /\ crashes = 0 /\ awake = (BootCase # "journal-only") /\ actor = awake
    /\ phase = IF BootCase = "journal-only" THEN "stopping" ELSE "legacy"
    /\ journal = (BootCase = "journal-only") /\ oldLive = TRUE /\ newLive = FALSE
    /\ peer = 0 /\ capPeer = IF BootCase = "journal-only" THEN 2 ELSE 0
    /\ pane = 1 /\ capPane = 1 /\ raced = FALSE
    /\ foreignKilled = FALSE /\ nativeSid = "B" /\ resumeSid = "B" /\ newSid = "none"
    /\ nativeReady = FALSE /\ remainingFailures = StopFailures /\ remainingRefusals = FirstRefusals
    /\ draft = HasDraft /\ parked = FALSE /\ draftExecuted = FALSE
    /\ q = [i \in Ids |-> IF BootCase = "journal-only" THEN "done" ELSE "incoming"]
    /\ chosen = [i \in Ids |-> IF BootCase = "journal-only" THEN "old" ELSE "none"]
    /\ receipt = [i \in Ids |-> IF BootCase = "journal-only" THEN "provider" ELSE "none"]
    /\ keys = [i \in Ids |-> IF BootCase = "journal-only" THEN 1 ELSE 0]
    /\ effects = [i \in Ids |-> IF BootCase = "journal-only" THEN 1 ELSE 0]
    /\ oldComplete = [i \in Ids |-> BootCase = "journal-only"]
    /\ probeMarker = [i \in Ids |-> "none"] /\ bypassed = FALSE /\ resendAsks = 0

ProbeIntake(i) ==
    /\ server = "up"
    /\ q[i] = "incoming"
    /\ IF i = 1 THEN TRUE ELSE q[i-1] # "incoming"
    /\ q' = ([q EXCEPT ![i] = "probing"])
    /\ probeMarker' = ([probeMarker EXCEPT ![i] = IF journal THEN "present" ELSE "absent"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, bypassed, resendAsks>>

CommitIntake(i) ==
    /\ server = "up" /\ awake
    /\ q[i] = "probing"
    /\ q' = ([q EXCEPT ![i] = IF ~journal /\ Entry = "active" /\ (Policy = "main" \/ Mutant = "MissingActiveBegin") THEN "blocked" ELSE "queued"])
    /\ journal' = (journal \/ (~(Entry = "active" /\ (Policy = "main" \/ Mutant = "MissingActiveBegin"))))
    /\ phase' = (IF ~journal /\ ~(Entry = "active" /\ (Policy = "main" \/ Mutant = "MissingActiveBegin")) THEN "stopping" ELSE phase)
    /\ capPeer' = (IF ~journal THEN peer ELSE capPeer)
    /\ capPane' = (IF ~journal THEN pane ELSE capPane)
    /\ resumeSid' = (IF ~journal THEN nativeSid ELSE resumeSid)
    /\ bypassed' = (bypassed \/ (Mutant = "StaleIntake" /\ journal /\ probeMarker[i] = "absent"))
    /\ UNCHANGED <<server, crashes, awake, actor, oldLive, newLive, peer, pane, raced, foreignKilled, nativeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, resendAsks>>

BeforeReadyRefusal ==
    /\ server = "up" /\ awake /\ journal /\ phase = "stopping"
    /\ Policy = "main" /\ OldUnread
    /\ q' = ([i \in Ids |-> IF q[i] = "queued" THEN "retained" ELSE q[i]])
    /\ phase' = ("retained")
    /\ resendAsks' = (resendAsks + 1)
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed>>

StopFailed ==
    /\ server = "up" /\ awake /\ phase = "stopping" /\ journal
    /\ Policy # "main" /\ remainingFailures > 0 /\ RaceReady
    /\ phase' = ("fallback")
    /\ remainingFailures' = (remainingFailures - 1)
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

ChooseOld(i) ==
    /\ server = "up" /\ awake /\ phase = "fallback" /\ oldLive
    /\ i \in Front /\ chosen[i] = "none"
    /\ chosen' = ([chosen EXCEPT ![i] = "old"])
    /\ receipt' = ([receipt EXCEPT ![i] = "no-key"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

ParkDraft(i) ==
    /\ server = "up" /\ awake /\ phase = "fallback" /\ ~OldUnread
    /\ draft /\ ~parked /\ chosen[i] = "old" /\ q[i] = "queued"
    /\ parked' = (TRUE)
    /\ keys' = ([keys EXCEPT ![i] = 1])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, draftExecuted, q, chosen, receipt, effects, oldComplete, probeMarker, bypassed, resendAsks>>

ReleaseOldNoKey(i) ==
    /\ server = "up" /\ awake /\ phase = "fallback"
    /\ Policy = "fixed" /\ Mutant # "NoOldRelease"
    /\ chosen[i] = "old" /\ q[i] = "queued" /\ receipt[i] = "no-key" /\ keys[i] = 0
    /\ OldUnread
    /\ chosen' = ([chosen EXCEPT ![i] = "none"])
    /\ receipt' = ([receipt EXCEPT ![i] = "none"])
    /\ phase' = ("revalidating")
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

RevalidateOld ==
    /\ server = "up" /\ awake /\ phase = "revalidating"
    /\ phase' = (IF oldLive THEN "stopping" ELSE "ended")
    /\ capPeer' = (peer)
    /\ capPane' = (pane)
    /\ resumeSid' = (nativeSid)
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, pane, raced, foreignKilled, nativeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

ReacquireOldPeer ==
    /\ server = "up" /\ awake /\ phase = "stopping" /\ oldLive
    /\ capPeer # peer /\ ~raced
    /\ capPeer' = (peer)
    /\ capPane' = (pane)
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, pane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

NativeContinuation ==
    /\ RotateNative /\ oldLive /\ nativeSid = "B"
    /\ phase = "stopping"
    /\ nativeSid' = ("C")
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

OldFallbackComplete ==
    /\ server = "up" /\ awake /\ phase = "fallback"
    /\ \E i \in Ids : chosen[i] = "old" /\ q[i] \in Terminal
    /\ \A i \in Ids : chosen[i] # "old" \/ q[i] = "cancelled" \/ (q[i] = "done" /\ oldComplete[i])
    /\ phase' = ("stopping")
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

IdentityChanges ==
    /\ IdentityRace # "none" /\ ~raced /\ phase = "stopping"
    /\ raced' = (TRUE)
    /\ oldLive' = (FALSE)
    /\ peer' = (IF IdentityRace = "server" THEN peer + 1 ELSE peer)
    /\ pane' = (IF IdentityRace = "pane" THEN 2 ELSE pane)
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, newLive, capPeer, capPane, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

UnsafeNumericSignal ==
    /\ server = "up" /\ phase = "stopping" /\ raced
    /\ (Mutant = "PidSignal" /\ IdentityRace = "pid") \/ (Mutant = "ReconnectNumber" /\ IdentityRace = "server")
    /\ foreignKilled' = (TRUE)
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

ClosePinnedPane ==
    /\ server = "up" /\ awake /\ phase = "stopping" /\ journal /\ oldLive
    /\ Policy # "main" \/ ~OldUnread
    /\ remainingFailures = 0 /\ RaceReady
    /\ ~RotateNative \/ nativeSid = "C"
    /\ capPeer = peer /\ capPane = pane
    /\ \A i \in Ids : chosen[i] # "old" \/ q[i] = "cancelled" \/ (q[i] = "done" /\ oldComplete[i])
    /\ phase' = ("checking")
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

ProviderEnds ==
    /\ phase = "checking" /\ oldLive
    /\ oldLive' = (FALSE)
    /\ draft' = (FALSE)
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

VerifyOldAbsent ==
    /\ server = "up" /\ awake /\ journal
    /\ phase \in {"stopping", "checking"} /\ ~oldLive /\ RaceReady
    /\ phase' = ("ended")
    /\ resumeSid' = (IF Mutant = "StaleNative" THEN resumeSid ELSE nativeSid)
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

Launch ==
    /\ server = "up" /\ awake /\ phase = "ended" /\ ~oldLive
    /\ phase' = ("pending")
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

Create ==
    /\ server = "up" /\ awake /\ phase = "pending" /\ ~newLive
    /\ newLive' = (TRUE)
    /\ phase' = ("created")
    /\ newSid' = (IF Mutant = "WrongSession" THEN "A" ELSE resumeSid)
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

OwnResumeStartAndAttach ==
    /\ server = "up" /\ awake /\ phase = "created" /\ newLive
    /\ phase' = ("bound")
    /\ nativeReady' = (Mutant # "NoNativeReady")
    /\ UNCHANGED <<server, crashes, awake, actor, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

ChooseNew(i) ==
    /\ server = "up" /\ awake /\ phase = "bound"
    /\ i \in Front /\ chosen[i] = "none"
    /\ chosen' = ([chosen EXCEPT ![i] = "new"])
    /\ receipt' = ([receipt EXCEPT ![i] = "no-key"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

NewPreKeyRefused(i) ==
    /\ server = "up" /\ awake /\ phase = "bound"
    /\ i \in Front /\ chosen[i] = "new" /\ keys[i] = 0 /\ remainingRefusals > 0
    /\ q' = ([q EXCEPT ![i] = IF Policy # "fixed" \/ Mutant = "UntypedFirst" THEN "retained" ELSE "held"])
    /\ remainingRefusals' = (remainingRefusals - 1)
    /\ resendAsks' = (IF Policy # "fixed" \/ Mutant = "UntypedFirst" THEN resendAsks + 1 ELSE resendAsks)
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed>>

NativeReadyWake(i) ==
    /\ server = "up" /\ awake /\ q[i] = "held"
    /\ chosen[i] = "new" /\ receipt[i] = "no-key" /\ keys[i] = 0
    /\ nativeReady /\ Mutant # "NoFirstWake"
    /\ q' = ([q EXCEPT ![i] = "queued"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

Arm(i) ==
    /\ server = "up" /\ awake /\ i \in Front
    /\ chosen[i] # "none"
    /\ chosen[i] = "new" => (phase = "bound" /\ nativeReady /\ remainingRefusals = 0)
    /\ chosen[i] = "old" => (phase = "fallback" /\ ~OldUnread /\ (~draft \/ parked \/ Mutant = "DraftEnter"))
    /\ q' = ([q EXCEPT ![i] = "armed"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

Submit(i) ==
    /\ server = "up" /\ awake /\ q[i] = "armed" /\ receipt[i] = "no-key"
    /\ q' = ([q EXCEPT ![i] = "sent"])
    /\ effects' = ([effects EXCEPT ![i] = @ + 1])
    /\ keys' = ([keys EXCEPT ![i] = 1])
    /\ receipt' = ([receipt EXCEPT ![i] = IF Mutant = "LostReceipt" THEN "unknown" ELSE "provider"])
    /\ draftExecuted' = (draftExecuted \/ (chosen[i] = "old" /\ draft /\ ~parked))
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, chosen, oldComplete, probeMarker, bypassed, resendAsks>>

Confirm(i) ==
    /\ server = "up" /\ awake /\ q[i] \in {"sent", "recover"}
    /\ receipt[i] = "provider"
    /\ q' = ([q EXCEPT ![i] = "done"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

OldProcessingTerminal(i) ==
    /\ chosen[i] = "old" /\ q[i] = "done" /\ ~oldComplete[i]
    /\ oldComplete' = ([oldComplete EXCEPT ![i] = TRUE])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, probeMarker, bypassed, resendAsks>>

RecoverNoKey(i) ==
    /\ server = "up" /\ awake /\ q[i] = "recover" /\ receipt[i] = "no-key"
    /\ q' = ([q EXCEPT ![i] = "queued"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

FinishJournal ==
    /\ server = "up" /\ awake /\ journal /\ phase = "bound"
    /\ \A i \in Ids : q[i] \in Terminal
    /\ journal' = (FALSE)
    /\ phase' = ("complete")
    /\ UNCHANGED <<server, crashes, awake, actor, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

CancelBeforeKey(i) ==
    /\ Controls /\ server = "up" /\ q[i] \in {"queued", "held", "armed"}
    /\ receipt[i] \in {"none", "no-key"} /\ keys[i] = 0
    /\ q' = ([q EXCEPT ![i] = "cancelled"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

Crash ==
    /\ server = "up" /\ crashes < Restarts
    /\ server' = ("down")
    /\ crashes' = (crashes + 1)
    /\ awake' = (FALSE)
    /\ actor' = (FALSE)
    /\ capPeer' = (2)
    /\ q' = ([i \in Ids |-> IF q[i] \in {"armed", "sent"} THEN "recover" ELSE q[i]])
    /\ UNCHANGED <<phase, journal, oldLive, newLive, peer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

BootCensus ==
    /\ server = "down"
    /\ server' = ("up")
    /\ actor' = (HasInput \/ (journal /\ Mutant # "NoJournalCensus"))
    /\ UNCHANGED <<crashes, awake, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

BootWake ==
    /\ server = "up" /\ actor /\ ~awake
    /\ Mutant # "NoWake"
    /\ awake' = (TRUE)
    /\ UNCHANGED <<server, crashes, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

Duplicate ==
    /\ Mutant = "Duplicate"
    /\ \E i \in Ids : effects[i] = 1 /\ q[i] = "done"
    /\ effects' = (CHOOSE e \in [Ids -> 0..2] : \E i \in Ids : q[i] = "done" /\ effects[i] = 1 /\ e = [effects EXCEPT ![i] = 2])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, q, chosen, receipt, keys, oldComplete, probeMarker, bypassed, resendAsks>>

SilentDrop(i) ==
    /\ Mutant = "SilentDrop" /\ q[i] = "queued"
    /\ q' = ([q EXCEPT ![i] = "lost"])
    /\ UNCHANGED <<server, crashes, awake, actor, phase, journal, oldLive, newLive, peer, capPeer, pane, capPane, raced, foreignKilled, nativeSid, resumeSid, newSid, nativeReady, remainingFailures, remainingRefusals, draft, parked, draftExecuted, chosen, receipt, keys, effects, oldComplete, probeMarker, bypassed, resendAsks>>

AwaitEvents == UNCHANGED vars
Next == AwaitEvents \/ BeforeReadyRefusal \/ StopFailed \/ OldFallbackComplete \/ RevalidateOld \/ ReacquireOldPeer \/ NativeContinuation \/ IdentityChanges \/ UnsafeNumericSignal \/ ClosePinnedPane \/ ProviderEnds \/ VerifyOldAbsent \/ Launch \/ Create \/ OwnResumeStartAndAttach \/ FinishJournal \/ Crash \/ BootCensus \/ BootWake \/ Duplicate \/ (\E i \in Ids : ProbeIntake(i) \/ CommitIntake(i) \/ ChooseOld(i) \/ ParkDraft(i) \/ ReleaseOldNoKey(i) \/ ChooseNew(i) \/ NewPreKeyRefused(i) \/ NativeReadyWake(i) \/ Arm(i) \/ Submit(i) \/ Confirm(i) \/ OldProcessingTerminal(i) \/ RecoverNoKey(i) \/ CancelBeforeKey(i) \/ SilentDrop(i))
Fairness == WF_vars(BeforeReadyRefusal) /\ WF_vars(StopFailed) /\ WF_vars(OldFallbackComplete) /\ WF_vars(RevalidateOld) /\ WF_vars(ReacquireOldPeer) /\ WF_vars(NativeContinuation) /\ WF_vars(IdentityChanges) /\ WF_vars(ClosePinnedPane) /\ WF_vars(ProviderEnds) /\ WF_vars(VerifyOldAbsent) /\ WF_vars(Launch) /\ WF_vars(Create) /\ WF_vars(OwnResumeStartAndAttach) /\ WF_vars(FinishJournal) /\ WF_vars(BootCensus) /\ WF_vars(BootWake)
    /\ \A i \in Ids : (WF_vars(ProbeIntake(i)) /\ WF_vars(CommitIntake(i)) /\ WF_vars(ChooseOld(i)) /\ WF_vars(ParkDraft(i)) /\ WF_vars(ReleaseOldNoKey(i)) /\ WF_vars(ChooseNew(i)) /\ WF_vars(NewPreKeyRefused(i)) /\ WF_vars(NativeReadyWake(i)) /\ WF_vars(Arm(i)) /\ WF_vars(Submit(i)) /\ WF_vars(Confirm(i)) /\ WF_vars(OldProcessingTerminal(i)) /\ WF_vars(RecoverNoKey(i)))

Spec == Init /\ [][Next]_vars /\ Fairness
TypeOK == /\ server \in {"up", "down"} /\ crashes \in 0..Restarts
          /\ effects \in [Ids -> 0..2] /\ keys \in [Ids -> 0..1]
InputAtMostOnce == \A i \in Ids : effects[i] <= 1
NoSilentLoss == \A i \in Ids : q[i] # "lost" /\ (q[i] = "done" => effects[i] = 1)
NoUserResend == resendAsks = 0
DraftNeverExecuted == ~draftExecuted
OneProvider == ~(oldLive /\ newLive)
SameSession == newLive => (newSid = resumeSid /\ resumeSid = nativeSid)
NoForeignClose == ~foreignKilled
NoIntakeBypass == ~bypassed
MessageProgress == \A i \in Ids : (q[i] # "incoming") ~> (q[i] \in Terminal)
HandoffProgress == (journal \/ (\E i \in Ids : q[i] # "incoming")) ~> (phase \in {"bound", "complete"})
JournalProgress == journal ~> ~journal
=============================================================================
