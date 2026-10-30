---------------------------- MODULE HostMigration ----------------------------
(* #7503: one local Claude channel moving tmux -> Herdr (forward) and Herdr -> tmux (Return),
   keeping its Claude session id. Code: feat/i7503-claude-resume-both db99888121.

   Each code effect is one action whose guard is what the code re-observes before that effect,
   so a crash or restart may fall between any two effects (the plan keeps no receipt, §1-4).
   A turn that fails changes nothing; it stays pending, which is the same as the user sending it
   again. A pending input that no action can ever serve is a channel that stays blocked.

   Session ids: "A" is the session the channel ran on tmux; "B" is a fresh one.               *)
EXTENDS Naturals

CONSTANTS
    AllowFresh,   \* the forward turn may start fresh (force_fresh, no transcript, other account)
    AllowIdOnly,  \* the channel may be registered by id only (no name, no alias)
    Mut           \* "none" or one code guard removed (see the report)

VARIABLES
    mapping,   \* boot-approved host of the channel: "tmux" | "herdr"
    row,       \* hosted_execution state: "Legacy" | "Pending" | "Bound" | "Retired"
    tmuxP,     \* legacy tmux Claude of the logical key: "run" | "dead"
    tmuxSid,
    herdrP,    \* Herdr Claude of the row's execution: "none" | "run" | "dead"
    herdrSid,
    marker,    \* .host_kind marker beside the logical key: "tmux" | "herdr"
    listed,    \* this process lists the execution as a Herdr pane (in memory)
    chanSid,   \* the channel's stored provider session (turn.session_id)
    named,     \* the channel has a session name now (core.sessions or registration)
    regName,   \* the registration carries a name or alias
    pending,   \* inputs waiting in the channel queue
    inputs,    \* inputs the user still sends (bound)
    restarts,  \* dcserver restarts left (bound); a restart may change the mapping
    badResume, \* a resume continued another session than the one the leaving host ran
    authSid    \* the session the .auth_session record vouches for (session_markers.rs:37-91)

vars == <<mapping, row, tmuxP, tmuxSid, herdrP, herdrSid, marker, listed, chanSid, named,
          regName, pending, inputs, restarts, badResume, authSid>>

Init ==
    /\ mapping = "tmux" /\ row = "Legacy" /\ marker = "tmux" /\ listed = FALSE
    /\ tmuxP \in {"run", "dead"} /\ tmuxSid = "A" /\ chanSid = "A"
    /\ herdrP = "none" /\ herdrSid = "A"
    /\ named = TRUE /\ regName \in (IF AllowIdOnly THEN BOOLEAN ELSE {TRUE})
    /\ pending = 0 /\ inputs = 3 /\ restarts = 2 /\ badResume = FALSE /\ authSid = "A"

\* turn_host.rs:114-127 — a Retired row has finished moving back when its marker names tmux and
\* the process lists it no more.
Returned == IF Mut = "returnedIgnoresMarker" THEN ~listed ELSE marker = "tmux" /\ ~listed
\* turn_host.rs:102-120 — the Return plan of an unconfigured channel.
Due == mapping = "tmux" /\ (row = "Bound" \/ (row = "Retired" /\ ~Returned))
\* turn_host.rs:164-166 — a Pending or Bound row of an unconfigured channel refuses (no Return).
HerdrHeld == row \in {"Pending", "Bound"} \/ (row = "Retired" /\ ~Returned)

\* nameless_gate.rs:22-26,184-195 — a queued input of a channel whose kept Herdr row holds the
\* queue starts only when for_turn, keyed by build_adk_session_key (adk_session.rs:68-82), plans a
\* Return; with no name there is no key, so no row is read and no Return is planned.
TurnStarts == pending > 0 /\ (named \/ ~(mapping = "tmux" /\ HerdrHeld))

---------------------------------------------------------------------------
UserSends ==
    /\ inputs > 0 /\ pending' = pending + 1 /\ inputs' = inputs - 1
    /\ UNCHANGED <<mapping, row, tmuxP, tmuxSid, herdrP, herdrSid, marker, listed, chanSid,
                   named, regName, restarts, badResume, authSid>>

\* bootstrap + migration.rs:208-235: processes outlive the restart (shutdown.rs keeps tmux); the
\* listing is rebuilt from rows; an id-only channel has no name until a turn writes one. Removing
\* the mapping is refused unless the row is Legacy, Retired, or (new, V1) Bound.
Restart ==
    /\ restarts > 0 /\ restarts' = restarts - 1
    /\ mapping' \in IF mapping = "herdr" /\ row = "Pending" THEN {"herdr"} ELSE {"tmux", "herdr"}
    /\ listed' = (row \in {"Pending", "Bound"})
    /\ named' = regName
    /\ UNCHANGED <<row, tmuxP, tmuxSid, herdrP, herdrSid, marker, chanSid, regName, pending,
                   inputs, badResume, authSid>>

---------------------------------------------------------------------------
\* Forward. herdr_turn.rs:524-576 launched_source, for a Legacy or Retired row of a mapped channel.
ForwardReady == mapping = "herdr" /\ TurnStarts /\ row \in {"Legacy", "Retired"}

\* herdr_turn.rs:544-549 + session_handoff.rs:60-84: a resume first ends the legacy tmux by /exit.
ForwardEndsTmux ==
    /\ ForwardReady /\ tmuxP = "run" /\ Mut /= "noExitBeforeLaunch"
    /\ tmuxP' = "dead"
    /\ UNCHANGED <<mapping, row, tmuxSid, herdrP, herdrSid, marker, listed, chanSid, named,
                   regName, pending, inputs, restarts, badResume, authSid>>

\* herdr_turn.rs:571 launch_herdr_session (Pending CAS, marker herdr at herdr_launch.rs:684, create)
\* resuming chanSid; end_by_exit answered Ended (session_handoff.rs:65).
ForwardLaunchResume ==
    /\ ForwardReady /\ (tmuxP /= "run" \/ Mut = "noExitBeforeLaunch")
    /\ row' = "Pending" /\ herdrP' = "run" /\ herdrSid' = chanSid
    /\ marker' = "herdr" /\ listed' = TRUE
    /\ badResume' = (badResume \/ chanSid /= tmuxSid)
    /\ authSid' = chanSid   \* herdr_turn.rs:562-564
    /\ UNCHANGED <<mapping, tmuxP, tmuxSid, chanSid, named, regName, pending, inputs, restarts>>

\* herdr_turn.rs:538-543: resumed_session answers None, so a fresh session and no /exit.
ForwardLaunchFresh ==
    /\ ForwardReady /\ AllowFresh
    /\ row' = "Pending" /\ herdrP' = "run" /\ herdrSid' = "B"
    /\ marker' = "herdr" /\ listed' = TRUE /\ authSid' = "B"
    /\ UNCHANGED <<mapping, tmuxP, tmuxSid, chanSid, named, regName, pending, inputs, restarts,
                   badResume>>

\* herdr_turn.rs:575-576 resumed SessionStart then attach: Pending -> Bound.
Attach ==
    /\ row = "Pending" /\ herdrP = "run"
    /\ row' = "Bound"
    /\ UNCHANGED <<mapping, tmuxP, tmuxSid, herdrP, herdrSid, marker, listed, chanSid, named,
                   regName, pending, inputs, restarts, badResume, authSid>>

\* A Herdr turn answers; channel_writeback.rs:220-221 stores the session it ran.
HerdrServes ==
    /\ mapping = "herdr" /\ TurnStarts /\ row = "Bound" /\ herdrP = "run"
    /\ pending' = pending - 1
    /\ chanSid' = herdrSid /\ named' = TRUE   \* a turn writes its core session
    /\ UNCHANGED <<mapping, row, tmuxP, tmuxSid, herdrP, herdrSid, marker, listed, regName,
                   inputs, restarts, badResume, authSid>>

---------------------------------------------------------------------------
\* Return. herdr_return.rs:50-89 execute, entered from provider_dispatch.rs:275.
ReturnReady == Due /\ TurnStarts /\
               (tmuxP /= "run" \/ Mut = "noTmuxCheck") /\  \* herdr_return.rs:66-70
               (authSid = chanSid \/ Mut = "noAuthCheck")  \* herdr_return.rs:113-117

\* herdr_return.rs:72-73: a Bound execution ends by /exit on its own gated pane.
ReturnEndsHerdr ==
    /\ ReturnReady /\ row = "Bound" /\ herdrP = "run"
    /\ herdrP' = "dead"
    /\ UNCHANGED <<mapping, row, tmuxP, tmuxSid, herdrSid, marker, listed, chanSid, named,
                   regName, pending, inputs, restarts, badResume, authSid>>

\* herdr_return.rs:74-79: retire_pg only after end_by_exit returned Ok.
ReturnRetires ==
    /\ ReturnReady /\ row = "Bound" /\ (herdrP /= "run" \/ Mut = "retireBeforeExit")
    /\ row' = "Retired"
    /\ UNCHANGED <<mapping, tmuxP, tmuxSid, herdrP, herdrSid, marker, listed, chanSid, named,
                   regName, pending, inputs, restarts, badResume, authSid>>

\* herdr_return.rs:83 release_herdr_execution, then :88 mark_returned_to_tmux.
ReturnReleases ==
    /\ ReturnReady /\ row = "Retired" /\ listed
    /\ listed' = FALSE
    /\ UNCHANGED <<mapping, row, tmuxP, tmuxSid, herdrP, herdrSid, marker, chanSid, named,
                   regName, pending, inputs, restarts, badResume, authSid>>

ReturnMarksTmux ==
    /\ ReturnReady /\ row = "Retired" /\ ~listed /\ marker = "herdr"
    /\ marker' = "tmux"
    /\ UNCHANGED <<mapping, row, tmuxP, tmuxSid, herdrP, herdrSid, listed, chanSid, named,
                   regName, pending, inputs, restarts, badResume, authSid>>

\* The tmux turn: claude.rs:1770,1831-1855 resumes chanSid in place of the dead pane; host_gate
\* refuses while the marker names Herdr; tui_session_launch.rs:26 refuses while mapped to Herdr.
TmuxLaunch ==
    /\ mapping = "tmux" /\ TurnStarts /\ ~HerdrHeld /\ marker = "tmux" /\ tmuxP /= "run"
    /\ tmuxP' = "run" /\ tmuxSid' = chanSid
    /\ badResume' = (badResume \/ (row = "Retired" /\ chanSid /= herdrSid))
    /\ authSid' = chanSid   \* claude.rs:1860-1864
    /\ UNCHANGED <<mapping, row, herdrP, herdrSid, marker, listed, chanSid, named, regName,
                   pending, inputs, restarts>>

TmuxServes ==
    /\ mapping = "tmux" /\ TurnStarts /\ ~HerdrHeld /\ marker = "tmux" /\ tmuxP = "run"
    /\ pending' = pending - 1 /\ chanSid' = tmuxSid /\ named' = TRUE
    /\ UNCHANGED <<mapping, row, tmuxP, tmuxSid, herdrP, herdrSid, marker, listed, regName,
                   inputs, restarts, badResume, authSid>>

\* Nothing owed and nothing more to send.
Done ==
    /\ pending = 0 /\ inputs = 0 /\ restarts = 0
    /\ UNCHANGED vars

Next ==
    \/ UserSends \/ Restart
    \/ ForwardEndsTmux \/ ForwardLaunchResume \/ ForwardLaunchFresh \/ Attach \/ HerdrServes
    \/ ReturnEndsHerdr \/ ReturnRetires \/ ReturnReleases \/ ReturnMarksTmux
    \/ TmuxLaunch \/ TmuxServes
    \/ Done

---------------------------------------------------------------------------
\* Rule 1. One Claude session id has at most one live provider.
OneProviderPerSession == ~(tmuxP = "run" /\ herdrP = "run" /\ tmuxSid = herdrSid)

\* Rule 2. A move resumes the session the leaving host ran, never another one.
ResumesSameSession == ~badResume

\* Rule 4. The record and the processes agree: a Pending/Bound row owns the channel's only live
\* Claude, and a Legacy/Retired row runs none on Herdr.
RecordMatchesProcesses ==
    /\ (row \in {"Pending", "Bound"} => tmuxP /= "run")
    /\ (row \in {"Legacy", "Retired"} => herdrP /= "run")

\* Rule 3 is TLC's deadlock check: a pending input that nothing can serve.
=============================================================================
