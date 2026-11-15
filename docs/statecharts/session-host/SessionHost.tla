---------------------------- MODULE SessionHost ----------------------------
\* #7583 final design (revision 3). Code SHA 1974cff8a67bf5d1db041abfdd4d615c518ee104.
\* Value domains: docs/design/i7583/entrypoints.md (design-1 census, the only source).
\* One channel, at most three executions, one dcserver crash, one host-server loss.
\* Revision 2 split the location CAS from Bound (a placed Pending), separated the endpoint from
\* the provider switch, added the Cancel gate and kept an input refused for want of a host.
\* Revision 3 separates a proven provider from a bound source (Codex sends its first hook only
\* with its first prompt), keeps the Unix Escape switch, and lets tmux come back so a held
\* input is submitted again.
EXTENDS Naturals, FiniteSets
CONSTANTS
    Platform,     \* "unix" | "windows"
    Design,       \* "current" (main) | "proposed" (final design)
    Tmux,         \* tmux binary usable on this node at start
    Endpoint,     \* channel has a configured Herdr endpoint (session_hosts) or explicit choice
    Switch,       \* the provider's run switch (herdr_turn_enabled / herdr_codex_turn_enabled)
    CancelSwitch, \* herdr_cancel_enabled
    Autostart,    \* dcserver can start a missing Herdr server outside its own lifetime
    Codex,        \* the channel's provider is Codex (else Claude)
    Mutant,       \* "none" | "replay" | "earlyRetire" | "noWake" | "forget" | "serverInJob"
                  \* | "silentFallback" | "skipPlaced" | "gateRun" | "gateCancel" | "errConsumes"
                  \* | "bindProven" | "escapeNoSwitch" | "noAvailWake"
    AllowStop, MaxInputs
VARIABLES srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record, locator,
          serial, flight, registry, adopted, recoverOwed, stop, killed, want, answer, kept,
          lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms
vars == <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record, locator,
          serial, flight, registry, adopted, recoverOwed, stop, killed, want, answer, kept,
          lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* Host for a new execution. A configured endpoint goes to Herdr only with its switch, and main
\* gates that on unix (turn_host.rs:151-173, :228, :243). No endpoint: tmux when usable, else main
\* falls back to the process host (claude.rs:794-821, codex/command_streaming.rs:184-203,
\* qwen.rs:357-368). Proposed: an unconfigured Windows managed channel gets the implicit endpoint
\* (PlatformDefault, switches not consulted); no usable host is "none".
Pick ==
    IF Endpoint
    THEN (IF Switch /\ (Platform = "unix" \/ Design = "proposed") THEN "herdr" ELSE "refuse")
    ELSE IF Platform = "windows"
         THEN (IF Design = "current" THEN "process"
               ELSE IF Mutant = "gateRun" /\ ~Switch THEN "refuse" ELSE "herdr")
    ELSE IF tmuxUp THEN "tmux"
    ELSE IF Design = "current" \/ Mutant = "silentFallback" THEN "process"
    ELSE "none"

HostReady(h) == h = "process" \/ hostSrv = "up"
Usable == Pick \notin {"refuse", "none"} /\ HostReady(Pick)
Discoverable == host \in {"tmux", "herdr"} /\ Mutant # "forget"
Gated == srv = "up" \/ (srv = "boot" /\ Mutant = "noWake")
\* main leaves a placed Herdr Pending to its next turn (herdr_reader.rs:463) and its unplaced
\* resolver takes only rows with no location (unplaced.rs:353); proposed resolves it.
PlacedFix == host # "herdr" \/ (Design = "proposed" /\ Mutant # "skipPlaced")
\* Every Herdr Escape (Claude and Codex) goes through codex_stop_delivery.rs:250-262, which needs
\* herdr_cancel_enabled; only the proposed PlatformDefault permit skips it.
CancelOk == host # "herdr" \/ CancelSwitch
            \/ (Mutant = "escapeNoSwitch" /\ ~Codex)
            \/ (Design = "proposed" /\ Platform = "windows" /\ ~Endpoint /\ Mutant # "gateCancel")
FreeIds == {1, 2, 3} \ (live \cup {record})
Fresh == CHOOSE i \in FreeIds : \A j \in FreeIds : i <= j
\* A placed Pending whose provider runs and that has taken no first prompt yet: the next input
\* goes through the existing first-input path; nothing has to resolve it before then.
ReadyIdle == row = "Pending" /\ record \in live /\ locator = record /\ prov
             /\ firstIn = "none" /\ ~want /\ ~kept

Init ==
    /\ srv = "up" /\ host = "none" /\ hostSrv = "up" /\ tmuxUp = Tmux /\ row = "Legacy"
    /\ live = {} /\ prov = FALSE /\ src = FALSE /\ firstIn = "none" /\ waiting = FALSE
    /\ record = 0 /\ locator = 0 /\ serial = 0 /\ flight = "idle"
    /\ registry = FALSE /\ adopted = FALSE /\ recoverOwed = FALSE /\ stop = FALSE
    /\ killed = FALSE /\ want = FALSE /\ answer = "none" /\ kept = FALSE /\ lost = FALSE
    /\ requeues = 0 /\ cancel = "none" /\ inputs = 0 /\ crashes = 0 /\ losses = 0
    /\ tmuxBack = 0 /\ unknowns = 1 /\ terms = 0

Arrive ==
    /\ srv = "up" /\ ~want /\ ~kept /\ inputs < MaxInputs
    /\ want' = TRUE /\ answer' = "none" /\ inputs' = inputs + 1
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed, kept,
                   lost, requeues, cancel, crashes, losses, tmuxBack, unknowns, terms>>

\* Pending CAS before the host creates anything (herdr_launch.rs:482).
Reserve ==
    /\ Gated /\ want /\ ~stop /\ flight = "idle" /\ row \in {"Legacy", "Retired"}
    /\ Usable /\ FreeIds # {}
    /\ host' = Pick /\ row' = "Pending" /\ record' = Fresh /\ flight' = "creating"
    /\ locator' = 0 /\ prov' = FALSE /\ src' = FALSE /\ firstIn' = "none" /\ waiting' = FALSE
    /\ UNCHANGED <<srv, hostSrv, tmuxUp, live, serial, registry, adopted, recoverOwed, stop, killed,
                   want, answer, kept, lost, requeues, cancel, inputs, crashes, losses,
                   tmuxBack, unknowns, terms>>

\* The host acts before its reply (herdr_launch.rs:515). A tmux or process session runs its
\* provider at once; a Herdr pane may hold only a shell until the provider starts.
CreateEffect ==
    /\ srv = "up" /\ flight = "creating" /\ HostReady(host)
    /\ live' = live \cup {record} /\ registry' = TRUE /\ flight' = "created"
    /\ prov' = (host # "herdr") /\ src' = (host # "herdr")
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, firstIn, waiting, record, locator, serial,
                   adopted, recoverOwed, stop, killed, want, answer, kept, lost, requeues,
                   cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

CreateAborts ==
    /\ srv = "up" /\ flight \in {"creating", "created"} /\ record \notin live
    /\ (flight = "created" \/ hostSrv = "down")
    /\ flight' = "idle"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, registry, adopted, recoverOwed, stop, killed, want, answer,
                   kept, lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns,
                   terms>>

\* Location CAS, also for a CreatedUnconfirmed reply (herdr_launch.rs:529-569): the row stays
\* Pending with its pane recorded.
RecordLocation ==
    /\ srv = "up" /\ flight = "created" /\ record \in live
    /\ locator' = record /\ flight' = "idle"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record, serial,
                   registry, adopted, recoverOwed, stop, killed, want, answer, kept, lost,
                   requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* The Herdr provider starts in its pane, or never does (environment, not forced). Claude logs
\* SessionStart as it starts; Codex sends no hook before its first prompt (codex/herdr_turn.rs:266).
ProviderStarts ==
    /\ host = "herdr" /\ record \in live /\ ~prov /\ row = "Pending"
    /\ prov' = TRUE /\ src' = (src \/ ~Codex)
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, firstIn, waiting, record, locator, serial,
                   flight, registry, adopted, recoverOwed, stop, killed, want, answer, kept,
                   lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* A turn on a Pending whose own source is logged attaches it and sends its first prompt
\* (Claude pending_source, claude/herdr_turn.rs:395-428).
Bind ==
    /\ srv = "up" /\ want /\ ~stop /\ flight = "idle" /\ row = "Pending"
    /\ locator = record /\ record \in live /\ prov /\ src /\ firstIn = "none"
    /\ row' = "Bound" /\ adopted' = TRUE /\ want' = FALSE /\ answer' = "started"
    /\ firstIn' = "sent"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, live, prov, src, waiting, record, locator, serial,
                   flight, registry, recoverOwed, stop, killed, kept, lost, requeues, cancel,
                   inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* Codex's first prompt to its Pending execution (pending_launch then first_prompt,
\* codex/herdr_turn.rs:156-166): only with no hold and no hook of it logged.
FirstPrompt ==
    /\ Codex /\ srv = "up" /\ want /\ ~stop /\ flight = "idle" /\ row = "Pending"
    /\ locator = record /\ record \in live /\ prov /\ ~src /\ firstIn = "none"
    /\ firstIn' = "sent" /\ waiting' = TRUE /\ want' = FALSE /\ answer' = "started"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, record, locator, serial, flight,
                   registry, adopted, recoverOwed, stop, killed, kept, lost, requeues, cancel,
                   inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* Codex sends its first hook with the first prompt it took.
HookArrives ==
    /\ Codex /\ firstIn = "sent" /\ record \in live /\ prov /\ ~src
    /\ src' = TRUE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, firstIn, waiting, record, locator,
                   serial, flight, registry, adopted, recoverOwed, stop, killed, want, answer,
                   kept, lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns,
                   terms>>

\* The turn that sent the first prompt binds once its hook gives the source.
TurnAttach ==
    /\ srv = "up" /\ waiting /\ src /\ row = "Pending" /\ record \in live /\ locator = record
    /\ flight = "idle"
    /\ row' = "Bound" /\ adopted' = TRUE /\ registry' = TRUE /\ waiting' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, live, prov, src, firstIn, record, locator, serial,
                   flight, recoverOwed, stop, killed, want, answer, kept, lost, requeues,
                   cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* A turn on a placed Pending without a provider is refused with why (never_attached.rs); one
\* whose first prompt was already taken is refused too (codex/herdr_turn.rs:234-238, not_held).
TurnFails ==
    /\ srv = "up" /\ want /\ ~stop /\ flight = "idle" /\ row = "Pending"
    /\ locator = record /\ locator # 0 /\ (~(record \in live /\ prov) \/ firstIn = "sent")
    /\ want' = FALSE /\ answer' = "failed"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed, kept,
                   lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* A later prompt needs the bound source (codex/herdr_turn.rs:515-531).
StartTurn ==
    /\ srv = "up" /\ want /\ ~stop /\ row = "Bound" /\ adopted /\ record \in live /\ src
    /\ HostReady(host) /\ flight = "idle"
    /\ want' = FALSE /\ answer' = "started"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed, kept,
                   lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

FollowupNoSource ==
    /\ srv = "up" /\ want /\ ~stop /\ row = "Bound" /\ adopted /\ record \in live /\ ~src
    /\ flight = "idle"
    /\ want' = FALSE /\ answer' = "failed"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed, kept,
                   lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* Policy refusal before any reset (intake_turn/host_refusal.rs).
PolicyRefuse ==
    /\ srv = "up" /\ want /\ ~stop /\ flight = "idle" /\ row \in {"Legacy", "Retired"}
    /\ Pick = "refuse"
    /\ want' = FALSE /\ answer' = "refused"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed, kept,
                   lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* No usable host: one notice, the input stays in the durable queue (proposed admission
\* boundary). errConsumes: a plain provider Err after dequeue, as a non-owner Qwen turn settles.
Unavailable ==
    /\ srv = "up" /\ want /\ ~stop /\ flight = "idle" /\ row \in {"Legacy", "Retired"}
    /\ \/ Pick = "none"
       \/ Pick \in {"tmux", "herdr"} /\ hostSrv = "down" /\ ~Autostart
    /\ want' = FALSE /\ answer' = "unavailable"
    /\ kept' = (Mutant # "errConsumes") /\ lost' = (lost \/ Mutant = "errConsumes")
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed,
                   requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* tmux becomes usable again (installed, or the availability cache recovers); not forced.
TmuxReturns ==
    /\ ~tmuxUp /\ tmuxBack = 0
    /\ tmuxUp' = TRUE /\ tmuxBack' = 1
    /\ UNCHANGED <<srv, host, hostSrv, row, live, prov, src, firstIn, waiting, record, locator,
                   serial, flight, registry, adopted, recoverOwed, stop, killed, want, answer,
                   kept, lost, requeues, cancel, inputs, crashes, losses, unknowns, terms>>

\* The availability wake submits a kept input once a host is usable again.
Requeue ==
    /\ srv = "up" /\ kept /\ Usable /\ Mutant # "noAvailWake"
    /\ kept' = FALSE /\ want' = TRUE /\ requeues' = 1
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed,
                   answer, lost, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* Cancel of the running turn; it never ends the session.
RequestCancel ==
    /\ srv = "up" /\ row = "Bound" /\ adopted /\ record \in live /\ cancel = "none"
    /\ cancel' = "requested"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed, want,
                   answer, kept, lost, requeues, inputs, crashes, losses, tmuxBack, unknowns,
                   terms>>

DeliverCancel ==
    /\ srv = "up" /\ cancel = "requested"
    /\ cancel' = IF CancelOk THEN "delivered" ELSE "dropped"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed, want,
                   answer, kept, lost, requeues, inputs, crashes, losses, tmuxBack, unknowns,
                   terms>>

\* dcserver stops. A process session dies with Windows' kill-on-close job (windows_job.rs:35,52);
\* on unix it survives unreachable (session_backend.rs:341). serverInJob: so does a Herdr server.
\* The hook log and the input hold are on disk; the waiting turn is not.
Crash ==
    /\ srv = "up" /\ crashes = 0
    /\ srv' = "down" /\ crashes' = 1 /\ flight' = "idle" /\ waiting' = FALSE
    /\ registry' = FALSE /\ adopted' = FALSE
    /\ LET jobKills == Platform = "windows" /\
                       (host = "process" \/ (host = "herdr" /\ Mutant = "serverInJob"))
           after == IF jobKills THEN {} ELSE live
       IN /\ live' = after
          /\ prov' = (prov /\ ~jobKills)
          /\ killed' = (killed \/ (jobKills /\ live # {}))
          /\ hostSrv' = IF host = "herdr" /\ Platform = "windows" /\ Mutant = "serverInJob"
                        THEN "down" ELSE hostSrv
          /\ recoverOwed' = (after # {} /\ ~stop)
    /\ locator' = IF Mutant = "forget" THEN 0 ELSE locator
    /\ UNCHANGED <<host, tmuxUp, row, src, firstIn, record, serial, stop, want, answer, kept, lost,
                   requeues, cancel, inputs, losses, tmuxBack, unknowns, terms>>

Boot ==
    /\ srv = "down" /\ srv' = "boot"
    /\ UNCHANGED <<host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record, locator,
                   serial, flight, registry, adopted, recoverOwed, stop, killed, want, answer,
                   kept, lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns,
                   terms>>

BootDone ==
    /\ srv = "boot" /\ srv' = "up"
    /\ UNCHANGED <<host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record, locator,
                   serial, flight, registry, adopted, recoverOwed, stop, killed, want, answer,
                   kept, lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns,
                   terms>>

ProbeRecovers ==
    /\ unknowns > 0 /\ unknowns' = 0
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, stop, killed, want,
                   answer, kept, lost, requeues, cancel, inputs, crashes, losses, tmuxBack,
                   terms>>

\* Restart pass on a Bound row: new API connections, no attach, create or replay.
Adopt ==
    /\ srv \in {"boot", "up"} /\ recoverOwed /\ unknowns = 0 /\ Mutant # "noWake"
    /\ Discoverable /\ hostSrv = "up" /\ record \in live /\ row = "Bound"
    /\ locator' = record /\ registry' = TRUE /\ adopted' = TRUE /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record, serial,
                   flight, stop, killed, want, answer, kept, lost, requeues, cancel, inputs,
                   crashes, losses, tmuxBack, unknowns, terms>>

\* Unplaced Pending (no location): the existing census adopts its pane or retires it.
AdoptUnplaced ==
    /\ srv \in {"boot", "up"} /\ row = "Pending" /\ locator = 0 /\ flight = "idle"
    /\ unknowns = 0 /\ Discoverable /\ Mutant # "noWake" /\ record \in live /\ hostSrv = "up"
    /\ locator' = record
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record, serial,
                   flight, registry, adopted, recoverOwed, stop, killed, want, answer, kept,
                   lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

ResolveEmpty ==
    /\ srv \in {"boot", "up"} /\ row = "Pending" /\ locator = 0 /\ flight = "idle"
    /\ unknowns = 0 /\ Mutant # "noWake"
    /\ IF host = "process" THEN ~registry ELSE (record \notin live /\ Discoverable)
    /\ row' = "Retired" /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, live, prov, src, firstIn, waiting, record, locator,
                   serial, flight, registry, adopted, stop, killed, want, answer, kept, lost,
                   requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* Placed Pending resolver (proposed), same nonce, pane and server witness. A proven provider is
\* not a bound source: the row binds only once its own source is logged after the first prompt
\* (bindProven: revision 2 bound on the provider alone). A provider that took no first prompt is
\* only confirmed and stays Pending for the existing first-input path.
AdoptPlaced ==
    /\ PlacedFix /\ srv \in {"boot", "up"} /\ Mutant # "noWake" /\ row = "Pending"
    /\ locator = record /\ locator # 0 /\ record \in live /\ prov /\ flight = "idle"
    /\ unknowns = 0 /\ hostSrv = "up" /\ Discoverable /\ ~waiting
    /\ IF Mutant = "bindProven" THEN TRUE ELSE (src /\ firstIn = "sent")
    /\ row' = "Bound" /\ adopted' = TRUE /\ registry' = TRUE /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, live, prov, src, firstIn, waiting, record, locator,
                   serial, flight, stop, killed, want, answer, kept, lost, requeues, cancel,
                   inputs, crashes, losses, tmuxBack, unknowns, terms>>

ConfirmReady ==
    /\ PlacedFix /\ srv \in {"boot", "up"} /\ Mutant # "noWake" /\ row = "Pending"
    /\ recoverOwed /\ locator = record /\ locator # 0 /\ record \in live /\ prov
    /\ firstIn = "none" /\ flight = "idle" /\ unknowns = 0 /\ hostSrv = "up" /\ Discoverable
    /\ adopted' = TRUE /\ registry' = TRUE /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, stop, killed, want, answer, kept, lost, requeues,
                   cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

ResolvePlacedGone ==
    /\ PlacedFix /\ srv \in {"boot", "up"} /\ Mutant # "noWake" /\ row = "Pending"
    /\ locator = record /\ locator # 0 /\ flight = "idle" /\ unknowns = 0
    /\ IF host = "process" THEN ~registry ELSE (record \notin live /\ Discoverable)
    /\ row' = "Retired" /\ locator' = 0 /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, live, prov, src, firstIn, waiting, record, serial,
                   flight, registry, adopted, stop, killed, want, answer, kept, lost, requeues,
                   cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

ResidueClose ==
    /\ PlacedFix /\ srv = "up" /\ Mutant # "noWake" /\ row = "Pending" /\ ~want /\ ~stop
    /\ locator = record /\ locator # 0 /\ record \in live /\ ~prov /\ flight = "idle"
    /\ unknowns = 0 /\ hostSrv = "up" /\ Discoverable
    /\ flight' = "closing"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, registry, adopted, recoverOwed, stop, killed, want, answer,
                   kept, lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns,
                   terms>>

HostLoss ==
    /\ hostSrv = "up" /\ host \in {"tmux", "herdr"} /\ losses = 0
    /\ hostSrv' = "down" /\ live' = {} /\ prov' = FALSE /\ losses' = 1
    /\ UNCHANGED <<srv, host, tmuxUp, row, src, firstIn, waiting, record, locator, serial, flight,
                   registry, adopted, recoverOwed, stop, killed, want, answer, kept, lost,
                   requeues, cancel, inputs, crashes, tmuxBack, unknowns, terms>>

HostStart ==
    /\ hostSrv = "down" /\ Autostart /\ srv = "up"
    /\ hostSrv' = "up"
    /\ UNCHANGED <<srv, host, tmuxUp, row, live, prov, src, firstIn, waiting, record, locator, serial,
                   flight, registry, adopted, recoverOwed, stop, killed, want, answer, kept,
                   lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

SeenDead == IF host = "process" THEN ~registry \/ record \notin live ELSE record \notin live
DetectLoss ==
    /\ srv \in {"boot", "up"} /\ row = "Bound" /\ flight = "idle" /\ unknowns = 0 /\ SeenDead
    /\ (Discoverable \/ host = "process")
    /\ row' = "Retired" /\ locator' = 0 /\ adopted' = FALSE /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, live, prov, src, firstIn, waiting, record, serial,
                   flight, registry, stop, killed, want, answer, kept, lost, requeues, cancel,
                   inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* Guard mutant: a lost create reply is treated as permission to create again (once).
ReplayPending ==
    /\ Mutant = "replay" /\ srv = "up" /\ row = "Pending" /\ locator = 0
    /\ flight = "idle" /\ serial = 0 /\ FreeIds # {}
    /\ record' = Fresh /\ serial' = 1 /\ flight' = "creating"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, locator,
                   registry, adopted, recoverOwed, stop, killed, want, answer, kept, lost,
                   requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

RequestStop ==
    /\ AllowStop /\ srv = "up" /\ adopted /\ ~stop /\ terms = 0
    /\ stop' = TRUE /\ terms' = 1
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, live, prov, src, firstIn, waiting, record,
                   locator, serial, flight, registry, adopted, recoverOwed, killed, want,
                   answer, kept, lost, requeues, cancel, inputs, crashes, losses, tmuxBack,
                   unknowns>>

BeginClose ==
    /\ srv = "up" /\ stop /\ live # {} /\ flight = "idle" /\ unknowns = 0
    /\ (Discoverable \/ registry)
    /\ flight' = "closing"
    /\ row' = IF Mutant = "earlyRetire" THEN "Retired" ELSE row
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, live, prov, src, firstIn, waiting, record, locator,
                   serial, registry, adopted, recoverOwed, stop, killed, want, answer, kept,
                   lost, requeues, cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

\* The host confirms the exact owned execution gone; a close ACK alone never takes this step.
CloseEffect ==
    /\ srv = "up" /\ flight = "closing"
    /\ live' = live \ {record} /\ prov' = FALSE /\ adopted' = FALSE /\ flight' = "idle"
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, row, src, firstIn, waiting, record, locator, serial,
                   registry, recoverOwed, stop, killed, want, answer, kept, lost, requeues,
                   cancel, inputs, crashes, losses, tmuxBack, unknowns, terms>>

FinishTerminate ==
    /\ srv = "up" /\ stop /\ live = {} /\ flight = "idle"
    /\ row' = "Retired" /\ locator' = 0 /\ registry' = FALSE /\ stop' = FALSE
    /\ recoverOwed' = FALSE
    /\ UNCHANGED <<srv, host, hostSrv, tmuxUp, live, prov, src, firstIn, waiting, record, serial,
                   flight, adopted, killed, want, answer, kept, lost, requeues, cancel, inputs,
                   crashes, losses, tmuxBack, unknowns, terms>>

Next == Arrive \/ Reserve \/ CreateEffect \/ CreateAborts \/ RecordLocation \/ ProviderStarts
        \/ Bind \/ FirstPrompt \/ HookArrives \/ TurnAttach \/ TurnFails \/ StartTurn
        \/ FollowupNoSource \/ PolicyRefuse \/ Unavailable \/ TmuxReturns \/ Requeue
        \/ RequestCancel \/ DeliverCancel \/ Crash \/ Boot \/ BootDone \/ ProbeRecovers
        \/ Adopt \/ AdoptUnplaced \/ ResolveEmpty \/ AdoptPlaced \/ ConfirmReady
        \/ ResolvePlacedGone \/ ResidueClose \/ HostLoss \/ HostStart \/ DetectLoss
        \/ ReplayPending \/ RequestStop \/ BeginClose \/ CloseEffect \/ FinishTerminate

\* Inputs, crashes, host loss, tmux return, provider start, cancel and stop requests are never
\* forced. Codex's first hook follows the first prompt it took.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Reserve) /\ WF_vars(CreateEffect) /\ WF_vars(CreateAborts)
        /\ WF_vars(RecordLocation) /\ WF_vars(Bind) /\ WF_vars(FirstPrompt)
        /\ WF_vars(HookArrives) /\ WF_vars(TurnAttach) /\ WF_vars(TurnFails)
        /\ WF_vars(StartTurn) /\ WF_vars(FollowupNoSource)
        /\ WF_vars(PolicyRefuse) /\ WF_vars(Unavailable) /\ WF_vars(Requeue)
        /\ WF_vars(DeliverCancel) /\ WF_vars(Boot) /\ WF_vars(BootDone)
        /\ WF_vars(ProbeRecovers) /\ WF_vars(Adopt) /\ WF_vars(AdoptUnplaced)
        /\ WF_vars(ResolveEmpty) /\ WF_vars(AdoptPlaced) /\ WF_vars(ConfirmReady)
        /\ WF_vars(ResolvePlacedGone) /\ WF_vars(ResidueClose) /\ WF_vars(HostStart)
        /\ WF_vars(DetectLoss) /\ WF_vars(BeginClose) /\ WF_vars(CloseEffect)
        /\ WF_vars(FinishTerminate)

TypeOK ==
    /\ srv \in {"up", "down", "boot"} /\ host \in {"none", "tmux", "process", "herdr"}
    /\ hostSrv \in {"up", "down"} /\ tmuxUp \in BOOLEAN
    /\ row \in {"Legacy", "Pending", "Bound", "Retired"}
    /\ live \subseteq {1, 2, 3} /\ prov \in BOOLEAN /\ src \in BOOLEAN
    /\ firstIn \in {"none", "sent"} /\ waiting \in BOOLEAN /\ record \in 0..3 /\ locator \in 0..3
    /\ serial \in 0..1 /\ flight \in {"idle", "creating", "created", "closing"}
    /\ answer \in {"none", "started", "refused", "unavailable", "failed"}
    /\ kept \in BOOLEAN /\ lost \in BOOLEAN /\ requeues \in 0..1
    /\ cancel \in {"none", "requested", "delivered", "dropped"} /\ inputs \in 0..MaxInputs
    /\ crashes \in 0..1 /\ losses \in 0..1 /\ tmuxBack \in 0..1 /\ unknowns \in 0..1
    /\ terms \in 0..1

\* Session loss 0: a dcserver restart alone never ends an execution, and every live
\* execution stays reachable through its durable row (registry, locator or host census).
NoSessionLoss ==
    /\ ~killed
    /\ live = {} \/ (row \in {"Pending", "Bound"} /\ (registry \/ locator # 0 \/ Discoverable))
NoDoubleSession == Cardinality(live) <= 1
NoOrphanAfterTerminate == row = "Retired" => live = {}
NoProcessHost == Design = "proposed" => host # "process"
\* An input refused for want of a host stays in the durable queue.
NoInputLoss == ~lost
\* Default config (no endpoint) is never refused by policy, and its Cancel is never dropped.
NoDefaultRefusal == answer = "refused" => Endpoint
NoDefaultCancelDrop == cancel = "dropped" => Endpoint
\* A Bound row has its own source; a source-less Bound refuses every prompt.
BoundHasSource == row = "Bound" => src
\* On unix an endpoint channel's Escape stays off while herdr_cancel_enabled is off, as in main.
UnixCancelAsMain == (Platform = "unix" /\ Endpoint /\ ~CancelSwitch /\ host = "herdr")
                    => cancel # "delivered"
\* Reachability witness: a cfg lists it to show a kept input is submitted again at least once.
NeverRequeued == requeues = 0

RestartEventuallyReconnect ==
    recoverOwed ~> ((adopted /\ locator = record /\ record \in live)
                    \/ (record \notin live /\ row = "Retired"))
InputAnswered == want ~> ~want
TerminateEventuallyGone == stop ~> (~stop /\ live = {})
PendingEventuallyResolved == (row = "Pending") ~> (row \in {"Bound", "Retired"} \/ ReadyIdle)
HostRecovers == (hostSrv = "down") ~> (hostSrv = "up")
CodexFirstInputDelivered ==
    (Codex /\ row = "Pending" /\ want /\ prov /\ ~src /\ firstIn = "none" /\ record \in live)
    ~> (firstIn = "sent" \/ row = "Retired" \/ record \notin live)
HeldInputSubmitted == (kept /\ Usable) ~> ~kept
=============================================================================
