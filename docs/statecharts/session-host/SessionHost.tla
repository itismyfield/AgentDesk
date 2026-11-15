---------------------------- MODULE SessionHost ----------------------------
\* #7583 final design. Code SHA 1974cff8a67bf5d1db041abfdd4d615c518ee104.
\* Value domains: docs/design/i7583/entrypoints.md (design-1 census, kept as the
\* only source). One channel, at most three executions, one dcserver crash, one host-server loss.
\* Base: design-1 (Pending/create/location, adoption, close). Merged from the Fable model: host
\* selection per platform, tmux absence, the Windows kill-on-close job, host-server loss and
\* restart, refused input, the silent-fallback and server-in-job mutants.
EXTENDS Naturals, FiniteSets
CONSTANTS
    Platform,   \* "unix" | "windows"
    Design,     \* "current" (main) | "proposed" (final design)
    Tmux,       \* tmux binary usable on this node
    Selected,   \* channel has a configured Herdr endpoint (session_hosts) and its switch on
    Autostart,  \* dcserver can start a missing Herdr server outside its own lifetime
    Mutant,     \* "none" | "replay" | "earlyRetire" | "noWake" | "forget"
                \* | "serverInJob" | "silentFallback"
    AllowStop, MaxInputs
VARIABLES srv, host, hostSrv, row, live, record, locator, serial, flight, registry,
          adopted, recoverOwed, stop, killed, want, answer, inputs, crashes, losses,
          unknowns, terms
vars == <<srv, host, hostSrv, row, live, record, locator, serial, flight, registry,
          adopted, recoverOwed, stop, killed, want, answer, inputs, crashes, losses,
          unknowns, terms>>

\* Host a new execution gets. Herdr for a selected channel (turn_host.rs:151-173); main
\* gates it on unix (turn_host.rs:228,243). Unselected: tmux when usable, else main falls back
\* to the process host (claude.rs:794-821, codex/command_streaming.rs:184-205, qwen.rs:339-368).
\* Proposed: an unselected Windows managed Claude/Codex channel gets the implicit dedicated
\* Herdr endpoint; an unselected unix channel without tmux is refused before any spawn.
Pick ==
    IF Selected THEN (IF Platform = "unix" \/ Design = "proposed" THEN "herdr" ELSE "refuse")
    ELSE IF Platform = "windows"
         THEN (IF Design = "proposed" THEN "herdr" ELSE "process")
    ELSE IF Tmux THEN "tmux"
    ELSE IF Design = "current" \/ Mutant = "silentFallback" THEN "process"
    ELSE "refuse"

\* Execution identities are symmetric abstract values; a new one differs from every live one
\* and from the last recorded one, so a stale locator never names it.
FreeIds == {1, 2, 3} \ (live \cup {record})
Fresh == CHOOSE i \in FreeIds : \A j \in FreeIds : i <= j

HostReady(h) == h = "process" \/ hostSrv = "up"
\* tmux names and Herdr nonce workspaces survive the daemon; PROCESS_SESSIONS does not.
Discoverable == host \in {"tmux", "herdr"} /\ Mutant # "forget"
Gated == srv = "up" \/ (srv = "boot" /\ Mutant = "noWake")

Init ==
    /\ srv = "up" /\ host = "none" /\ hostSrv = "up" /\ row = "Legacy" /\ live = {}
    /\ record = 0 /\ locator = 0 /\ serial = 0 /\ flight = "idle" /\ registry = FALSE
    /\ adopted = FALSE /\ recoverOwed = FALSE /\ stop = FALSE /\ killed = FALSE
    /\ want = FALSE /\ answer = "none" /\ inputs = 0 /\ crashes = 0 /\ losses = 0
    /\ unknowns = 1 /\ terms = 0

Arrive ==
    /\ srv = "up" /\ ~want /\ inputs < MaxInputs
    /\ want' = TRUE /\ answer' = "none" /\ inputs' = inputs + 1
    /\ UNCHANGED <<srv, host, hostSrv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, killed, crashes, losses, unknowns, terms>>

\* Pending CAS before the host creates anything (herdr_launch.rs:482). The guard is the row,
\* not the real process table: a retired row with a live execution is exactly the double risk.
Reserve ==
    /\ Gated /\ want /\ ~stop /\ flight = "idle" /\ row \in {"Legacy", "Retired"}
    /\ Pick # "refuse" /\ HostReady(Pick) /\ FreeIds # {}
    /\ host' = Pick /\ row' = "Pending" /\ record' = Fresh /\ serial' = serial
    /\ flight' = "creating" /\ locator' = 0
    /\ UNCHANGED <<srv, hostSrv, live, registry, adopted, recoverOwed, stop, killed, want,
                   answer, inputs, crashes, losses, unknowns, terms>>

\* The host may act before its reply or the location CAS is durable (herdr_launch.rs:515).
CreateEffect ==
    /\ srv = "up" /\ flight = "creating" /\ HostReady(host)
    /\ live' = live \cup {record} /\ registry' = TRUE /\ flight' = "created"
    /\ UNCHANGED <<srv, host, hostSrv, row, record, locator, serial, adopted, recoverOwed,
                   stop, killed, want, answer, inputs, crashes, losses, unknowns, terms>>

\* Location CAS, then the first input of that turn (herdr_launch.rs:554; first_input).
RecordLocation ==
    /\ srv = "up" /\ flight = "created" /\ record \in live
    /\ locator' = record /\ row' = "Bound" /\ adopted' = TRUE /\ flight' = "idle"
    /\ want' = FALSE /\ answer' = "started"
    /\ UNCHANGED <<srv, host, hostSrv, live, record, serial, registry, recoverOwed, stop,
                   killed, inputs, crashes, losses, unknowns, terms>>

\* The create reply never comes back or the host server is gone: the launch returns
\* NotSent/Indeterminate and keeps the Pending row for the census; it never creates again.
CreateAborts ==
    /\ srv = "up" /\ flight \in {"creating", "created"} /\ record \notin live
    /\ (flight = "created" \/ hostSrv = "down")
    /\ flight' = "idle"
    /\ UNCHANGED <<srv, host, hostSrv, row, live, record, locator, serial, registry, adopted,
                   recoverOwed, stop, killed, want, answer, inputs, crashes, losses, unknowns,
                   terms>>

StartTurn ==
    /\ srv = "up" /\ want /\ ~stop /\ row = "Bound" /\ adopted /\ record \in live
    /\ HostReady(host) /\ flight = "idle"
    /\ want' = FALSE /\ answer' = "started"
    /\ UNCHANGED <<srv, host, hostSrv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, killed, inputs, crashes, losses, unknowns,
                   terms>>

\* No host can run the turn: the input is answered with the refusal before any spawn
\* (provider_dispatch.rs:559 for a refused Herdr turn; the proposed host-unavailable error).
Refuse ==
    /\ srv = "up" /\ want /\ ~stop /\ flight = "idle" /\ row \in {"Legacy", "Retired"}
    /\ \/ Pick = "refuse"
       \/ Pick \in {"tmux", "herdr"} /\ hostSrv = "down" /\ ~Autostart
    /\ want' = FALSE /\ answer' = "refused"
    /\ UNCHANGED <<srv, host, hostSrv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, killed, inputs, crashes, losses, unknowns,
                   terms>>

\* dcserver stops. tmux/Herdr servers are other process trees and keep their panes. A process
\* session is dcserver's child: Windows ends it with the kill-on-close job (windows_job.rs:35,52;
\* dcserver.rs:882); on unix its own process group survives unreachable (session_backend.rs:341).
\* serverInJob: a Windows Herdr server started inside that job dies with dcserver.
Crash ==
    /\ srv = "up" /\ crashes = 0
    /\ srv' = "down" /\ crashes' = 1 /\ flight' = "idle"
    /\ registry' = FALSE /\ adopted' = FALSE
    /\ LET jobKills == Platform = "windows" /\
                       (host = "process" \/ (host = "herdr" /\ Mutant = "serverInJob"))
           after == IF jobKills THEN {} ELSE live
       IN /\ live' = after
          /\ killed' = (killed \/ (jobKills /\ live # {}))
          /\ hostSrv' = IF host = "herdr" /\ Platform = "windows" /\ Mutant = "serverInJob"
                        THEN "down" ELSE hostSrv
          /\ recoverOwed' = (after # {} /\ ~stop)
    /\ locator' = IF Mutant = "forget" THEN 0 ELSE locator
    /\ UNCHANGED <<host, row, record, serial, stop, want, answer, inputs, losses, unknowns,
                   terms>>

Boot ==
    /\ srv = "down" /\ srv' = "boot"
    /\ UNCHANGED <<host, hostSrv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, killed, want, answer, inputs, crashes, losses,
                   unknowns, terms>>

\* noWake: the boot path opens intake but never reaches the Herdr restart pass on this platform
\* (rehydration.rs:270,668 behind cfg(unix) today).
BootDone ==
    /\ srv = "boot" /\ srv' = "up"
    /\ UNCHANGED <<host, hostSrv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, killed, want, answer, inputs, crashes, losses,
                   unknowns, terms>>

\* One transient Unknown probe (session_host/model.rs:52). Fairness clears it.
ProbeRecovers ==
    /\ unknowns > 0 /\ unknowns' = 0
    /\ UNCHANGED <<srv, host, hostSrv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, stop, killed, want, answer, inputs, crashes, losses,
                   terms>>

\* herdr_reader -> reconcile -> attach_restarted_herdr_source; tmux watcher restore. New API
\* connections only; no terminal attach, no server handoff, no create, no replay.
Adopt ==
    /\ srv \in {"boot", "up"} /\ recoverOwed /\ unknowns = 0 /\ Mutant # "noWake"
    /\ Discoverable /\ hostSrv = "up" /\ record \in live /\ row \in {"Pending", "Bound"}
    /\ locator' = record /\ row' = "Bound" /\ registry' = TRUE /\ adopted' = TRUE
    /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, live, record, serial, flight, stop, killed, want,
                   answer, inputs, crashes, losses, unknowns, terms>>

\* The host server ends on its own: Herdr update/stop (no live handoff on Windows), reboot,
\* tmux server killed. Its panes go with it. Not a dcserver restart.
HostLoss ==
    /\ hostSrv = "up" /\ host \in {"tmux", "herdr"} /\ losses = 0
    /\ hostSrv' = "down" /\ live' = {} /\ losses' = 1
    /\ UNCHANGED <<srv, host, row, record, locator, serial, flight, registry, adopted,
                   recoverOwed, stop, killed, want, answer, inputs, crashes, unknowns, terms>>

\* tmux starts its server with the next new-session; Herdr by its autostart (spawn.rs:48 today
\* macOS only), proposed to run outside dcserver's job on Windows.
HostStart ==
    /\ hostSrv = "down" /\ Autostart /\ srv = "up"
    /\ hostSrv' = "up"
    /\ UNCHANGED <<srv, host, row, live, record, locator, serial, flight, registry, adopted,
                   recoverOwed, stop, killed, want, answer, inputs, crashes, losses, unknowns,
                   terms>>

\* A complete same-server census proves the crashed Pending never left a live execution.
\* main reads a process row by its in-memory registry alone, empty after a restart.
ResolveEmpty ==
    /\ srv \in {"boot", "up"} /\ row = "Pending" /\ flight = "idle" /\ unknowns = 0
    /\ IF host = "process" THEN ~registry ELSE (live = {} /\ Discoverable)
    /\ row' = "Retired" /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, live, record, locator, serial, flight, registry,
                   adopted, stop, killed, want, answer, inputs, crashes, losses, unknowns,
                   terms>>

\* A recorded execution that is gone is retired with its notice before another start:
\* tmux missing-session disposition, Herdr server-replaced/row-end passes. main reads a process
\* row with an empty registry as dead whether or not its orphan still runs.
SeenDead == IF host = "process" THEN ~registry \/ record \notin live ELSE record \notin live
DetectLoss ==
    /\ srv \in {"boot", "up"} /\ row = "Bound" /\ flight = "idle" /\ unknowns = 0 /\ SeenDead
    /\ (Discoverable \/ host = "process")
    /\ row' = "Retired" /\ locator' = 0 /\ adopted' = FALSE /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, live, record, serial, flight, registry, stop, killed,
                   want, answer, inputs, crashes, losses, unknowns, terms>>

\* Guard mutant: a lost create reply is treated as permission to create again (once;
\* serial counts the replays).
ReplayPending ==
    /\ Mutant = "replay" /\ srv = "up" /\ row = "Pending" /\ locator = 0
    /\ flight = "idle" /\ serial = 0 /\ FreeIds # {}
    /\ record' = Fresh /\ serial' = 1 /\ flight' = "creating"
    /\ UNCHANGED <<srv, host, hostSrv, row, live, locator, registry, adopted, recoverOwed,
                   stop, killed, want, answer, inputs, crashes, losses, unknowns, terms>>

\* Terminate/clear/reset end request. A plain Cancel leaves the session alive and is not this.
RequestStop ==
    /\ AllowStop /\ srv = "up" /\ adopted /\ ~stop /\ terms = 0
    /\ stop' = TRUE /\ terms' = 1
    /\ UNCHANGED <<srv, host, hostSrv, row, live, record, locator, serial, flight, registry,
                   adopted, recoverOwed, killed, want, answer, inputs, crashes, losses,
                   unknowns>>

BeginClose ==
    /\ srv = "up" /\ stop /\ live # {} /\ flight = "idle" /\ unknowns = 0
    /\ (Discoverable \/ registry)
    /\ flight' = "closing"
    /\ row' = IF Mutant = "earlyRetire" THEN "Retired" ELSE row
    /\ UNCHANGED <<srv, host, hostSrv, live, record, locator, serial, registry, adopted,
                   recoverOwed, stop, killed, want, answer, inputs, crashes, losses, unknowns,
                   terms>>

\* The host confirms the exact owned execution gone; a close ACK alone never takes this step.
CloseEffect ==
    /\ srv = "up" /\ flight = "closing"
    /\ live' = live \ {record} /\ adopted' = FALSE /\ flight' = "idle"
    /\ UNCHANGED <<srv, host, hostSrv, row, record, locator, serial, registry, recoverOwed,
                   stop, killed, want, answer, inputs, crashes, losses, unknowns, terms>>

FinishTerminate ==
    /\ srv = "up" /\ stop /\ live = {} /\ flight = "idle"
    /\ row' = "Retired" /\ locator' = 0 /\ registry' = FALSE /\ stop' = FALSE
    /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, live, record, serial, flight, adopted, killed, want,
                   answer, inputs, crashes, losses, unknowns, terms>>

Next == Arrive \/ Reserve \/ CreateEffect \/ CreateAborts \/ RecordLocation \/ StartTurn \/ Refuse
        \/ Crash \/ Boot \/ BootDone \/ ProbeRecovers \/ Adopt \/ HostLoss \/ HostStart
        \/ ResolveEmpty \/ DetectLoss \/ ReplayPending \/ RequestStop \/ BeginClose
        \/ CloseEffect \/ FinishTerminate

\* Inputs, crashes, host loss and stop requests are never forced; what dcserver and the
\* hosts do on their own is.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Reserve) /\ WF_vars(CreateEffect) /\ WF_vars(CreateAborts) /\ WF_vars(RecordLocation)
        /\ WF_vars(StartTurn) /\ WF_vars(Refuse) /\ WF_vars(Boot) /\ WF_vars(BootDone)
        /\ WF_vars(ProbeRecovers) /\ WF_vars(Adopt) /\ WF_vars(HostStart)
        /\ WF_vars(ResolveEmpty) /\ WF_vars(DetectLoss) /\ WF_vars(BeginClose)
        /\ WF_vars(CloseEffect) /\ WF_vars(FinishTerminate)

TypeOK ==
    /\ srv \in {"up", "down", "boot"} /\ host \in {"none", "tmux", "process", "herdr"}
    /\ hostSrv \in {"up", "down"} /\ row \in {"Legacy", "Pending", "Bound", "Retired"}
    /\ live \subseteq {1, 2, 3} /\ record \in 0..3 /\ locator \in 0..3 /\ serial \in 0..1
    /\ flight \in {"idle", "creating", "created", "closing"}
    /\ answer \in {"none", "started", "refused"} /\ inputs \in 0..MaxInputs
    /\ crashes \in 0..1 /\ losses \in 0..1 /\ unknowns \in 0..1 /\ terms \in 0..1

\* Session loss 0: a dcserver restart alone never ends an execution, and every live
\* execution stays reachable through its durable row (registry, locator or host census).
NoSessionLoss ==
    /\ ~killed
    /\ live = {} \/ (row \in {"Pending", "Bound"} /\ (registry \/ locator # 0 \/ Discoverable))
\* Same channel double session 0.
NoDoubleSession == Cardinality(live) <= 1
\* Orphan 0 after terminate: a retired row has no live execution.
NoOrphanAfterTerminate == row = "Retired" => live = {}
\* The proposed design never starts a process-hosted session.
NoProcessHost == Design = "proposed" => host # "process"

\* After a restart the same live execution is adopted again, or it is really gone and closed.
RestartEventuallyReconnect ==
    recoverOwed ~> ((adopted /\ locator = record /\ record \in live)
                    \/ (record \notin live /\ row = "Retired"))
InputAnswered == want ~> ~want
TerminateEventuallyGone == stop ~> (~stop /\ live = {})
PendingEventuallyResolved == (row = "Pending") ~> (row \in {"Bound", "Retired"})
HostRecovers == (hostSrv = "down") ~> (hostSrv = "up")
=============================================================================
