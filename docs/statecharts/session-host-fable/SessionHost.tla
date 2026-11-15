---- MODULE SessionHost ----
\* One Discord channel's managed provider session on the host main picks for it (tmux, Herdr,
\* process), across dcserver restarts, loss of the host server and a terminate.
\* AgentDesk main 1974cff8a67bf5d1db041abfdd4d615c518ee104. Each action cites its code.
\* Out of scope, covered elsewhere: the Herdr launch steps (HerdrSession), a channel moving
\* between tmux and Herdr (HostMigration), deferred tmux boot probes (TmuxSession T-A/T-B).
EXTENDS Naturals

CONSTANTS
    Platform,      \* "unix" | "windows"
    Design,        \* "current" (main) | "proposed" (#7583: no process host)
    TmuxInstalled, \* tmux binary present; FALSE on windows (platform/tmux/availability.rs:110)
    HerdrMapped,   \* session_hosts.herdr.channels names the channel and its provider switch is on
    HostAutostart, \* something outside dcserver's job starts a missing Herdr server again
    Mutant,        \* "none" | "server_in_job" | "retire_before_close" | "boot_opens_early" | "silent_fallback"
    MaxCrashes, MaxHostLosses, MaxTerms, MaxInputs

VARIABLES
    srv,       \* "up" | "down": dcserver
    phase,     \* "recon" | "open": boot reconcile runs before intake opens
    hostSrv,   \* "up" | "down": the tmux or Herdr server of `kind`; unused for process
    kind,      \* "none" | "tmux" | "herdr" | "process": HostKind the durable record names
    rec,       \* "none" | "pending" | "bound" | "retired": hosted record or legacy inflight row
    live,      \* subset of {"tmux","herdr","process"}: hosts where a reachable provider runs
    orphans,   \* provider processes of this channel that no record or registry can reach
    attached,  \* this dcserver process holds the session: registry, watcher or pane listing
    closing,   \* in memory only: a terminate retired the row and still owes the host its close
    turn,      \* "idle" | "running"
    want,      \* an input waits
    answer,    \* "none" | "started" | "refused": what the last input got
    killed,    \* a dcserver restart alone ended a provider
    crashes, hostLosses, terms, inputs

vars == <<srv, phase, hostSrv, kind, rec, live, orphans, attached, closing, turn, want, answer,
          killed, crashes, hostLosses, terms, inputs>>

Init ==
    /\ srv = "up" /\ phase = "open" /\ hostSrv = "up" /\ kind = "none" /\ rec = "none"
    /\ live = {} /\ orphans = 0 /\ attached = FALSE /\ closing = FALSE /\ turn = "idle"
    /\ want = FALSE /\ answer = "none" /\ killed = FALSE
    /\ crashes = 0 /\ hostLosses = 0 /\ terms = 0 /\ inputs = 0

\* The host a new session gets. Herdr first for a mapped channel (turn_host.rs:157-172); main
\* runs it on unix only (turn_host.rs:228,243; provider_dispatch.rs:553-565 refuses elsewhere).
\* Then tmux when installed, then the process host (claude.rs:794-821, codex/command_streaming.rs:
\* 184-203, qwen.rs:339-368). Proposed: no process host, so the turn is refused.
Pick ==
    IF HerdrMapped /\ (Platform = "unix" \/ Design = "proposed") THEN "herdr"
    ELSE IF HerdrMapped THEN "refuse"
    ELSE IF Platform = "unix" /\ TmuxInstalled THEN "tmux"
    ELSE IF Design = "current" \/ Mutant = "silent_fallback" THEN "process"
    ELSE "refuse"

Recorded == rec \in {"pending", "bound"}
HostReady(h) == h = "process" \/ hostSrv = "up"

Arrive ==
    /\ srv = "up" /\ ~want /\ inputs < MaxInputs
    /\ want' = TRUE /\ answer' = "none" /\ inputs' = inputs + 1
    /\ UNCHANGED <<srv, phase, hostSrv, kind, rec, live, orphans, attached, closing, turn, killed,
                   crashes, hostLosses, terms>>

\* A new session. The launch sees a tmux session by name (claude/host_gate.rs:84) and a Herdr
\* row as Occupied (herdr_launch.rs:453-454); a process orphan left by an earlier dcserver is in
\* nobody's registry (session_backend.rs:341), so nothing stops a second provider.
Launch ==
    /\ srv = "up" /\ phase = "open" /\ want /\ turn = "idle" /\ ~closing
    /\ rec \in {"none", "retired"} /\ live = {}
    /\ Pick # "refuse" /\ HostReady(Pick)
    /\ kind' = Pick /\ live' = {Pick} /\ attached' = TRUE
    /\ rec' = IF Pick = "herdr" THEN "pending" ELSE "bound"
    /\ turn' = IF Pick = "herdr" THEN "idle" ELSE "running"
    /\ want' = (Pick = "herdr") /\ answer' = IF Pick = "herdr" THEN answer ELSE "started"
    /\ UNCHANGED <<srv, phase, hostSrv, orphans, closing, killed, crashes, hostLosses, terms,
                   inputs>>

\* The first input binds a Pending Herdr execution (claude/herdr_turn/first_input.rs:24-94).
Bind ==
    /\ srv = "up" /\ phase = "open" /\ rec = "pending" /\ attached /\ kind \in live /\ want
    /\ hostSrv = "up"
    /\ rec' = "bound" /\ turn' = "running" /\ want' = FALSE /\ answer' = "started"
    /\ UNCHANGED <<srv, phase, hostSrv, kind, live, orphans, attached, closing, killed, crashes,
                   hostLosses, terms, inputs>>

\* A follow-up on the session this process holds.
StartTurn ==
    /\ srv = "up" /\ phase = "open" /\ want /\ turn = "idle" /\ rec = "bound" /\ attached
    /\ kind \in live /\ HostReady(kind)
    /\ turn' = "running" /\ want' = FALSE /\ answer' = "started"
    /\ UNCHANGED <<srv, phase, hostSrv, kind, rec, live, orphans, attached, closing, killed,
                   crashes, hostLosses, terms, inputs>>

\* No host can run the turn: the input is answered with the refusal, never dropped
\* (provider_dispatch.rs:341 for a refused Herdr turn; the proposed tmux-missing error).
Refuse ==
    /\ srv = "up" /\ phase = "open" /\ want /\ turn = "idle" /\ ~Recorded /\ ~closing
    /\ \/ Pick = "refuse"
       \/ Pick \in {"tmux", "herdr"} /\ hostSrv = "down" /\ ~HostAutostart
    /\ want' = FALSE /\ answer' = "refused"
    /\ UNCHANGED <<srv, phase, hostSrv, kind, rec, live, orphans, attached, closing, turn, killed,
                   crashes, hostLosses, terms, inputs>>

TurnEnds ==
    /\ turn = "running" /\ kind \in live
    /\ turn' = "idle"
    /\ UNCHANGED <<srv, phase, hostSrv, kind, rec, live, orphans, attached, closing, want, answer,
                   killed, crashes, hostLosses, terms, inputs>>

\* dcserver stops. A tmux or Herdr server is another process tree and keeps its panes. A process
\* session is dcserver's child: Windows ends it with the job (platform/windows_job.rs:35,52);
\* on unix its own process group survives (session_backend.rs:240-244) with nobody able to reach
\* it. server_in_job: a Windows Herdr server started inside that job dies with dcserver.
DcCrash ==
    /\ srv = "up" /\ crashes < MaxCrashes
    /\ srv' = "down" /\ phase' = "recon" /\ attached' = FALSE /\ closing' = FALSE
    /\ crashes' = crashes + 1
    /\ IF "process" \in live
       THEN /\ live' = {} /\ turn' = "idle" /\ killed' = TRUE
            /\ orphans' = IF Platform = "unix" THEN orphans + 1 ELSE orphans
            /\ hostSrv' = hostSrv
       ELSE IF Mutant = "server_in_job" /\ Platform = "windows" /\ kind = "herdr"
       THEN /\ hostSrv' = "down" /\ live' = {} /\ turn' = "idle"
            /\ killed' = (killed \/ live # {}) /\ orphans' = orphans
       ELSE UNCHANGED <<hostSrv, live, turn, killed, orphans>>
    /\ UNCHANGED <<kind, rec, want, answer, hostLosses, terms, inputs>>

Boot ==
    /\ srv = "down"
    /\ srv' = "up"
    /\ UNCHANGED <<phase, hostSrv, kind, rec, live, orphans, attached, closing, turn, want, answer,
                   killed, crashes, hostLosses, terms, inputs>>

\* Boot reconcile before intake opens (runtime_bootstrap/recovery_flush.rs:128). A live tmux
\* session gets its watcher back (watchers/lifecycle/restore.rs); a live Herdr pane is read
\* again by its recorded pane over new API connections (recovery_engine/herdr_reader.rs:220-228,
\* called from tui_prompt_relay/rehydration.rs:270,668), with no terminal attach and no server
\* handoff. A process row has an empty
\* registry (session_backend.rs:341,526-536) and reads as dead whether or not an orphan runs.
Reconcile ==
    /\ srv = "up" /\ phase = "recon"
    /\ phase' = "open"
    /\ IF Mutant = "boot_opens_early" THEN UNCHANGED <<rec, attached>>
       ELSE IF Recorded /\ kind \in live /\ kind # "process" /\ hostSrv = "up"
       THEN attached' = TRUE /\ rec' = rec
       ELSE IF Recorded THEN rec' = "retired" /\ attached' = FALSE
       ELSE UNCHANGED <<rec, attached>>
    /\ UNCHANGED <<srv, hostSrv, kind, live, orphans, closing, turn, want, answer, killed, crashes,
                   hostLosses, terms, inputs>>

\* The tmux or Herdr server goes away on its own: a Herdr update or stop on Windows (no live
\* handoff there), a reboot, a killed tmux server. Its panes go with it.
HostLoss ==
    /\ hostSrv = "up" /\ kind \in {"tmux", "herdr"} /\ hostLosses < MaxHostLosses
    /\ hostSrv' = "down" /\ live' = {} /\ turn' = "idle" /\ hostLosses' = hostLosses + 1
    /\ UNCHANGED <<srv, phase, kind, rec, orphans, attached, closing, want, answer, killed,
                   crashes, terms, inputs>>

\* tmux starts its server with the next new-session (platform/tmux.rs:184); Herdr is started
\* again by its autostart (session_host/herdr/autostart/spawn.rs, unix only today).
HostStart ==
    /\ hostSrv = "down" /\ HostAutostart /\ srv = "up"
    /\ hostSrv' = "up"
    /\ UNCHANGED <<srv, phase, kind, rec, live, orphans, attached, closing, turn, want, answer,
                   killed, crashes, hostLosses, terms, inputs>>

\* A recorded session whose provider is gone is retired with its notice before another start:
\* tmux by the missing-session disposition (recovery_engine/restore_inflight.rs:1563-1625) and
\* the watcher's death path, Herdr by the replaced-server and row-end passes
\* (recovery_engine/herdr_server_replaced.rs, herdr_row_end.rs).
DetectLoss ==
    /\ srv = "up" /\ phase = "open" /\ Recorded /\ kind \notin live
    /\ rec' = "retired" /\ attached' = FALSE
    /\ UNCHANGED <<srv, phase, hostSrv, kind, live, orphans, closing, turn, want, answer, killed,
                   crashes, hostLosses, terms, inputs>>

\* Terminate, clear, force-kill: the host closes the session first and the row is retired on
\* its acknowledgement (tmux kill-session then row cleanup; Herdr close then retire,
\* session_host/herdr/contract.rs:305-312, termination_audit/host_terminate.rs:116-133).
TermClose ==
    /\ srv = "up" /\ phase = "open" /\ Recorded /\ attached /\ kind \in live /\ ~closing
    /\ terms < MaxTerms /\ Mutant # "retire_before_close"
    /\ live' = {} /\ turn' = "idle" /\ terms' = terms + 1
    /\ UNCHANGED <<srv, phase, hostSrv, kind, rec, orphans, attached, closing, want, answer,
                   killed, crashes, hostLosses, inputs>>

\* retire_before_close: the row is retired first and the close follows from memory.
TermRetireFirst ==
    /\ srv = "up" /\ phase = "open" /\ Recorded /\ attached /\ kind \in live /\ ~closing
    /\ terms < MaxTerms /\ Mutant = "retire_before_close"
    /\ rec' = "retired" /\ attached' = FALSE /\ closing' = TRUE /\ terms' = terms + 1
    /\ UNCHANGED <<srv, phase, hostSrv, kind, live, orphans, turn, want, answer, killed, crashes,
                   hostLosses, inputs>>

TermCloseLate ==
    /\ srv = "up" /\ closing
    /\ live' = {} /\ turn' = "idle" /\ closing' = FALSE
    /\ UNCHANGED <<srv, phase, hostSrv, kind, rec, orphans, attached, want, answer, killed,
                   crashes, hostLosses, terms, inputs>>

Next ==
    \/ Arrive \/ Launch \/ Bind \/ StartTurn \/ Refuse \/ TurnEnds
    \/ DcCrash \/ Boot \/ Reconcile \/ HostLoss \/ HostStart \/ DetectLoss
    \/ TermClose \/ TermRetireFirst \/ TermCloseLate

\* The environment (inputs, crashes, host loss, terminates) is not forced; everything dcserver and
\* the hosts do on their own is.
Fair ==
    /\ WF_vars(Launch) /\ WF_vars(Bind) /\ WF_vars(StartTurn) /\ WF_vars(Refuse)
    /\ WF_vars(TurnEnds) /\ WF_vars(Boot) /\ WF_vars(Reconcile) /\ WF_vars(HostStart)
    /\ WF_vars(DetectLoss) /\ WF_vars(TermCloseLate)

Spec == Init /\ [][Next]_vars /\ Fair

\* ---- Rules ----

\* A dcserver restart alone never ends a session (session loss 0).
NoKillByRestart == ~killed

\* One channel never has two providers (double session 0).
NoDoubleSession ==
    /\ \A a \in live : \A b \in live : a = b
    /\ ~(orphans > 0 /\ live # {})
    /\ orphans <= 1

\* No provider runs that no live record names (orphan 0, also after a terminate).
NoOrphan ==
    /\ orphans = 0
    /\ live # {} => (Recorded /\ live = {kind})

\* The proposed design never starts a process-hosted session.
NoProcessHost == Design = "proposed" => "process" \notin live

\* After a restart the channel ends up open with its session held again or its row closed.
Settled == srv = "up" /\ phase = "open" /\ (Recorded => (attached /\ kind \in live))
Reconnects == (srv = "down") ~> Settled

\* Every input is started or refused, never left waiting.
InputAnswered == want ~> ~want

\* A lost host server comes back without anyone's hands.
HostRecovers == (hostSrv = "down") ~> (hostSrv = "up")
====
