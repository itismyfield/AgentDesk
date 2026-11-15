---------------------------- MODULE SessionHost ----------------------------
\* Code SHA: 1974cff8a67bf5d1db041abfdd4d615c518ee104.
\* Domains and code entrypoints: docs/design/i7583/entrypoints.md.
\* One channel, two possible executions, one dcserver crash. Herdr itself survives.
EXTENDS Naturals, FiniteSets
CONSTANTS Host, Platform, Mode, StartBound, AllowStop
VARIABLES srv, row, live, record, locator, serial, flight, registry,
          adopted, recoverOwed, stop, terminated, crashes, unknowns
vars == <<srv, row, live, record, locator, serial, flight, registry,
          adopted, recoverOwed, stop, terminated, crashes, unknowns>>

Init ==
    /\ srv = "up"
    /\ row = IF StartBound THEN "Bound" ELSE "Legacy"
    /\ live = IF StartBound THEN {1} ELSE {}
    /\ record = IF StartBound THEN 1 ELSE 0
    /\ locator = IF StartBound /\ Host # "process" THEN 1 ELSE 0
    /\ serial = IF StartBound THEN 1 ELSE 0
    /\ flight = "idle" /\ registry = StartBound /\ adopted = StartBound
    /\ recoverOwed = FALSE /\ stop = FALSE /\ terminated = FALSE
    /\ crashes = 0 /\ unknowns = 1

\* hosted_execution.rs Pending CAS before the host creates anything.
Reserve ==
    /\ srv = "up" /\ row = "Legacy" /\ ~terminated
    /\ row' = "Pending" /\ record' = serial + 1 /\ serial' = serial + 1
    /\ flight' = "creating"
    /\ UNCHANGED <<srv, live, locator, registry, adopted, recoverOwed,
                   stop, terminated, crashes, unknowns>>

\* HerdrLaunchHost::create may act before its reply or the location CAS is durable.
CreateEffect ==
    /\ srv = "up" /\ flight = "creating"
    /\ live' = live \cup {record} /\ registry' = TRUE /\ flight' = "created"
    /\ UNCHANGED <<srv, row, record, locator, serial, adopted, recoverOwed,
                   stop, terminated, crashes, unknowns>>

RecordLocation ==
    /\ srv = "up" /\ flight = "created"
    /\ locator' = record /\ row' = "Bound" /\ adopted' = TRUE /\ flight' = "idle"
    /\ UNCHANGED <<srv, live, record, serial, registry, recoverOwed,
                   stop, terminated, crashes, unknowns>>

\* The host's pane/nonce census survives the daemon, unlike PROCESS_SESSIONS.
Discoverable == Host \in {"tmux", "herdr"} /\ Mode # "forget"
Adapter == ~(Mode = "current" /\ Host = "herdr" /\ Platform = "windows")

Crash ==
    /\ srv = "up" /\ crashes = 0 /\ row \in {"Pending", "Bound"}
    /\ srv' = "down" /\ crashes' = 1 /\ flight' = "idle"
    /\ registry' = FALSE /\ adopted' = FALSE /\ recoverOwed' = (live # {} /\ ~stop)
    /\ locator' = IF Mode = "forget" THEN 0 ELSE locator
    /\ UNCHANGED <<row, live, record, serial, stop, terminated, unknowns>>

Boot ==
    /\ srv = "down" /\ srv' = "boot"
    /\ UNCHANGED <<row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, terminated, crashes, unknowns>>

\* One transient Unknown. The fairness assumption is explicit, not a safety proof.
ProbeRecovers ==
    /\ unknowns > 0 /\ unknowns' = unknowns - 1
    /\ UNCHANGED <<srv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, terminated, crashes>>

\* herdr_reader -> reconcile_hosted -> attach_restarted_herdr_source. No terminal attach.
Adopt ==
    /\ srv \in {"boot", "up"} /\ recoverOwed /\ unknowns = 0
    /\ Adapter /\ Mode # "noWake" /\ (Discoverable \/ registry)
    /\ record \in live /\ row \in {"Pending", "Bound"}
    /\ locator' = record /\ row' = "Bound" /\ registry' = TRUE
    /\ adopted' = TRUE /\ recoverOwed' = FALSE /\ srv' = "up"
    /\ UNCHANGED <<live, record, serial, flight, stop, terminated, crashes, unknowns>>

BootDone ==
    /\ srv = "boot" /\ srv' = "up"
    /\ UNCHANGED <<row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, terminated, crashes, unknowns>>

\* A complete same-server census proves the crashed Pending never left a live execution.
ResolveEmpty ==
    /\ srv \in {"boot", "up"} /\ row = "Pending" /\ live = {}
    /\ flight = "idle" /\ unknowns = 0 /\ Discoverable /\ Adapter
    /\ row' = "Retired"
    /\ UNCHANGED <<srv, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, terminated, crashes, unknowns>>

\* Guard mutant: a lost create reply is treated as permission to create again.
ReplayPending ==
    /\ Mode = "replay" /\ srv = "up" /\ row = "Pending" /\ locator = 0
    /\ flight = "idle" /\ serial < 2
    /\ record' = serial + 1 /\ serial' = serial + 1 /\ flight' = "creating"
    /\ UNCHANGED <<srv, row, live, locator, registry, adopted, recoverOwed,
                   stop, terminated, crashes, unknowns>>

\* A separate close scenario: ordinary cancel leaves the session alive and is not this action.
RequestStop ==
    /\ AllowStop /\ srv = "up" /\ adopted /\ ~stop /\ ~terminated
    /\ stop' = TRUE
    /\ UNCHANGED <<srv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, terminated, crashes, unknowns>>

BeginClose ==
    /\ srv = "up" /\ stop /\ live # {} /\ flight = "idle"
    /\ Adapter /\ unknowns = 0 /\ (Discoverable \/ registry)
    /\ flight' = "closing"
    /\ row' = IF Mode = "earlyRetire" THEN "Retired" ELSE row
    /\ terminated' = (Mode = "earlyRetire")
    /\ UNCHANGED <<srv, live, record, locator, serial, registry, adopted,
                   recoverOwed, stop, crashes, unknowns>>

\* The host confirms the exact owned execution gone; a transport error never takes this step.
CloseEffect ==
    /\ srv = "up" /\ flight = "closing"
    /\ live' = live \ {record} /\ adopted' = FALSE /\ flight' = "idle"
    /\ UNCHANGED <<srv, row, record, locator, serial, registry, recoverOwed,
                   stop, terminated, crashes, unknowns>>

FinishTerminate ==
    /\ srv = "up" /\ stop /\ live = {} /\ ~terminated
    /\ row' = "Retired" /\ locator' = 0 /\ registry' = FALSE /\ terminated' = TRUE
    /\ UNCHANGED <<srv, live, record, serial, flight, adopted, recoverOwed,
                   stop, crashes, unknowns>>

Next == Reserve \/ CreateEffect \/ RecordLocation \/ Crash \/ Boot \/ ProbeRecovers
        \/ Adopt \/ BootDone \/ ResolveEmpty \/ ReplayPending \/ RequestStop \/ BeginClose
        \/ CloseEffect \/ FinishTerminate
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Boot) /\ WF_vars(ProbeRecovers) /\ WF_vars(Adopt)
        /\ WF_vars(CreateEffect) /\ WF_vars(RecordLocation)
        /\ WF_vars(ResolveEmpty) /\ WF_vars(BootDone)
        /\ WF_vars(BeginClose) /\ WF_vars(CloseEffect) /\ WF_vars(FinishTerminate)

TypeOK ==
    /\ Host \in {"tmux", "process", "herdr"} /\ Platform \in {"unix", "windows"}
    /\ row \in {"Legacy", "Pending", "Bound", "Retired"}
    /\ srv \in {"up", "down", "boot"} /\ live \subseteq {1, 2}
    /\ record \in 0..2 /\ locator \in 0..2 /\ serial \in 0..2
    /\ flight \in {"idle", "creating", "created", "closing"}
    /\ adopted \in BOOLEAN /\ registry \in BOOLEAN /\ recoverOwed \in BOOLEAN
    /\ stop \in BOOLEAN /\ terminated \in BOOLEAN /\ crashes \in 0..1 /\ unknowns \in 0..1
NoSessionLoss == live = {} \/
    (row \in {"Pending", "Bound"} /\ (registry \/ locator # 0 \/ Discoverable))
NoDoubleSession == Cardinality(live) <= 1
NoOrphanAfterTerminate == (terminated \/ row = "Retired") => live = {}
RestartEventuallyReconnect == recoverOwed ~> (adopted /\ locator = record)
TerminateEventuallyGone == stop ~> (terminated /\ live = {})
PendingEventuallyResolved == (row = "Pending") ~> (row \in {"Bound", "Retired"})
=============================================================================
