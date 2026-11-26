---- MODULE HerdrIdleExpire ----
\* One channel's Herdr execution under the 6-hour idle-kill tick (policies/timeouts/idle-kill.js ->
\* kill-tmux), across one dcserver restart. Code base: c28732c52c (Before: the host guard keeps
\* every Herdr row and records an idle_cleanup_preserved alarm each tick).
\* Expire TRUE is the fix: herdr_terminate/idle_expire.rs closes the pane, retires the row and
\* disconnects it with its provider session kept. Each other constant removes one guard.
\* turn: the mailbox holds an active turn token; inflight: the channel's inflight row file exists;
\* dispatch: sessions.active_dispatch_id is set; idle: status idle with a heartbeat over 6 h old.
\* gone: the owning bot was removed, so its runtime never registers again (it may Return later).
EXTENDS Naturals

CONSTANTS Expire, CheckTurn, CheckInflight, CheckIdle, Exclude, RecordResume, KeepAtLock,
          OrphanCodex

VARIABLES prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle, phase,
          alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed

vars == <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle, phase,
          alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>

Init ==
    /\ prov \in {"claude", "codex"} /\ gone \in BOOLEAN
    /\ srv = "up" /\ registered = ~gone /\ grace = FALSE
    /\ rec = "bound" /\ pane = "live"
    /\ turn = FALSE /\ inflight = FALSE /\ dispatch = FALSE /\ idle = FALSE
    /\ phase = "none" /\ alerts = 0 /\ closedBusy = FALSE /\ closedUsed = FALSE
    /\ selector = "live" /\ slot = "none" /\ file = FALSE /\ effort = "none" /\ resumed = "none"

\* phase "reserved": the transition lock and the sessions row lock are held and the fence is shut.
\* No input or /effort end runs under the transition lock (terminate_scoped acquire).
Unlocked == phase # "reserved"

\* slot: the owner's one effort intent (mailbox snapshot), "cancelled" once its operation is
\* cancelled. file: the idle end's own thread record (restart_intent/idle_resume.rs), kept apart
\* from the owner. The next launch on a Retired row resumes the kept selector: Claude from
\* claude_session_id, Codex only through an open intent or record (restart_intent.rs resume_of).
Resumes == selector = "kept" /\ (prov = "claude" \/ slot \in {"effort", "idle"} \/ file)

\* Input of the owning bot. On a retired row the turn launches a new execution, which spends any
\* intent or record naming the old one; a pending /effort end still names the old one.
Input ==
    /\ srv = "up" /\ registered /\ Unlocked /\ ~turn
    /\ turn' = TRUE /\ idle' = FALSE
    /\ IF rec = "retired"
         THEN /\ rec' = "bound" /\ pane' = "live" /\ selector' = "live"
              /\ slot' = "none" /\ file' = FALSE
              /\ resumed' = IF Resumes THEN "same" ELSE "fresh"
              /\ effort' = IF effort = "recorded" THEN "stale" ELSE effort
         ELSE UNCHANGED <<rec, pane, selector, slot, file, resumed, effort>>
    /\ UNCHANGED <<prov, gone, srv, registered, grace, inflight, dispatch, phase, alerts,
                   closedBusy, closedUsed>>

SaveInflight ==
    /\ srv = "up" /\ turn /\ ~inflight
    /\ inflight' = TRUE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>

\* A session PATCH writes active_dispatch_id with a plain UPDATE, which waits behind the idle
\* end's row lock (Exclude); the row stops being idle.
DispatchAttach ==
    /\ srv = "up" /\ ~dispatch /\ (~Exclude \/ phase # "reserved")
    /\ dispatch' = TRUE /\ idle' = FALSE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, phase, alerts,
                   closedBusy, closedUsed, selector, slot, file, effort, resumed>>

\* The finalizer clears both and refreshes the heartbeat, so the row is not idle again yet.
TurnEnd ==
    /\ srv = "up" /\ turn
    /\ turn' = FALSE /\ inflight' = FALSE /\ dispatch' = FALSE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, idle, phase, alerts, closedBusy,
                   closedUsed, selector, slot, file, effort, resumed>>

\* Six hours pass with no heartbeat. A quiet long turn or a restart can leave the heartbeat
\* stale while a turn or its inflight row is open.
Elapse ==
    /\ ~idle
    /\ idle' = TRUE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>

\* /effort with no turn (effort_restart/herdr.rs restart): its intent is stored in the owner slot
\* before the transition lock, then the end of that exact execution runs under the lock. An end
\* that finds it already retired cancels its own operation only (cancel_effort compares it).
EffortStart ==
    /\ srv = "up" /\ registered /\ rec = "bound" /\ ~turn /\ effort = "none"
    /\ effort' = "recorded" /\ slot' = "effort"
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle,
                   phase, alerts, closedBusy, closedUsed, selector, file, resumed>>

EffortEnd ==
    /\ srv = "up" /\ effort # "none" /\ Unlocked
    /\ effort' = "none"
    /\ IF rec = "bound" /\ effort = "recorded"
         THEN /\ rec' = "retired" /\ pane' = "closed" /\ selector' = "kept"
              /\ UNCHANGED slot
         ELSE /\ slot' = IF slot = "effort" THEN "cancelled" ELSE slot
              /\ UNCHANGED <<rec, pane, selector>>
    /\ UNCHANGED <<prov, gone, srv, registered, grace, turn, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, file, resumed>>

TurnClear == (~CheckTurn \/ ~turn) /\ (~CheckInflight \/ ~inflight)

\* Without the owner's runtime once the boot grace passed, the orphan path ends the execution
\* (end_orphan); a Codex one only when OrphanCodex.
Orphan == ~registered /\ grace /\ (prov = "claude" \/ OrphanCodex)

\* idle-kill.js selects status idle, no dispatch, 6 hours, up to 50 rows read once. kill-tmux then
\* pre-checks dispatch, inflight and the runtime. Without KeepAtLock (the previous head) the owned
\* path stored its Codex intent in the owner slot here, before the transition lock.
IdleKillTick ==
    /\ srv = "up" /\ rec = "bound" /\ idle /\ ~dispatch /\ phase = "none"
    /\ IF ~Expire
         THEN /\ alerts' = IF alerts < 2 THEN alerts + 1 ELSE alerts
              /\ UNCHANGED <<phase, slot>>
         ELSE /\ phase' = IF (registered \/ Orphan) /\ TurnClear THEN "judged" ELSE "none"
              /\ slot' = IF registered /\ TurnClear /\ prov = "codex" /\ RecordResume
                            /\ ~KeepAtLock
                           THEN "idle" ELSE slot
              /\ UNCHANGED alerts
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle,
                   closedBusy, closedUsed, selector, file, effort, resumed>>

\* Under the transition lock: the same-turn read, then the row lock reading status, heartbeat and
\* dispatch (IdleGate::reserve), the inflight re-read, the Codex thread record (IdleGate::keep),
\* the fence. A refusal writes nothing; the previous head cancelled the slot it had overwritten.
Reserve ==
    /\ srv = "up" /\ phase = "judged"
    /\ IF rec = "bound" /\ TurnClear /\ (~CheckIdle \/ idle) /\ ~dispatch
         THEN /\ phase' = "reserved"
              /\ file' = (file \/ (KeepAtLock /\ prov = "codex" /\ RecordResume))
              /\ UNCHANGED slot
         ELSE /\ phase' = "none"
              /\ slot' = IF ~KeepAtLock /\ slot = "idle" THEN "cancelled" ELSE slot
              /\ UNCHANGED file
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle,
                   alerts, closedBusy, closedUsed, selector, effort, resumed>>

\* close_fenced (or a pane already read as ended) and read_end, the row lock released, then
\* retire_confirmed and the disconnect with keep_provider_session = true.
Close ==
    /\ srv = "up" /\ phase = "reserved"
    /\ rec' = "retired" /\ pane' = "closed" /\ phase' = "none" /\ selector' = "kept"
    /\ closedBusy' = (closedBusy \/ turn \/ inflight \/ dispatch)
    /\ closedUsed' = (closedUsed \/ ~idle)
    /\ UNCHANGED <<prov, gone, srv, registered, grace, turn, inflight, dispatch, idle, alerts,
                   slot, file, effort, resumed>>

\* The mailbox and both locks are lost; the inflight file, the pane, the slot and the record survive.
Crash ==
    /\ srv = "up"
    /\ srv' = "down" /\ registered' = FALSE /\ grace' = FALSE /\ turn' = FALSE
    /\ phase' = "none" /\ effort' = "none"
    /\ UNCHANGED <<prov, gone, rec, pane, inflight, dispatch, idle, alerts, closedBusy,
                   closedUsed, selector, slot, file, resumed>>

Boot ==
    /\ srv = "down" /\ srv' = "up"
    /\ UNCHANGED <<prov, gone, registered, grace, rec, pane, turn, inflight, dispatch, idle,
                   phase, alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>

\* ORPHAN_AFTER_BOOT_SECS: the registry is old enough that a missing owner runtime is gone.
GracePass ==
    /\ srv = "up" /\ ~grace
    /\ grace' = TRUE
    /\ UNCHANGED <<prov, gone, srv, registered, rec, pane, turn, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>

Register ==
    /\ srv = "up" /\ ~registered /\ ~gone
    /\ registered' = TRUE
    /\ UNCHANGED <<prov, gone, srv, grace, rec, pane, turn, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>

\* The removed bot is added back; its next input is the owner's next launch.
Return ==
    /\ gone
    /\ gone' = FALSE
    /\ UNCHANGED <<prov, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>

\* restore_inflight.rs puts the persisted turn back in the mailbox after the runtime registers.
Restore ==
    /\ srv = "up" /\ registered /\ inflight /\ ~turn /\ rec = "bound"
    /\ turn' = TRUE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>

Next ==
    \/ Input \/ SaveInflight \/ DispatchAttach \/ TurnEnd \/ Elapse \/ EffortStart \/ EffortEnd
    \/ IdleKillTick \/ Reserve \/ Close \/ Crash \/ Boot \/ GracePass \/ Register \/ Return
    \/ Restore

Spec == Init /\ [][Next]_vars
    /\ WF_vars(IdleKillTick) /\ WF_vars(Reserve) /\ WF_vars(Close)
    /\ WF_vars(Boot) /\ WF_vars(GracePass) /\ WF_vars(Register)

\* INVARIANT: a pane is never closed under an active turn, an open inflight row or a dispatch.
NeverClosesBusy == ~closedBusy
\* INVARIANT: a pane used again after the tick selected it is not closed (idle read at the close).
NeverClosesUsed == ~closedUsed
\* INVARIANT: the next message after an end that kept the session resumes the same session.
NextMessageResumes == resumed # "fresh"
\* INVARIANT: the preserved alarm is not repeated tick after tick.
AlarmNotRepeated == alerts <= 1
TypeOK ==
    /\ prov \in {"claude", "codex"} /\ gone \in BOOLEAN /\ rec \in {"bound", "retired"}
    /\ pane \in {"live", "closed"} /\ phase \in {"none", "judged", "reserved"} /\ alerts \in 0..2
    /\ selector \in {"live", "kept"} /\ slot \in {"none", "effort", "idle", "cancelled"}
    /\ file \in BOOLEAN /\ effort \in {"none", "recorded", "stale"} /\ resumed \in {"none", "same", "fresh"}

\* PROPERTY: a quiet, idle, undispatched Herdr session is eventually closed unless work arrives,
\* with or without its owner's runtime.
Quiet == srv = "up" /\ rec = "bound" /\ idle /\ ~turn /\ ~inflight /\ ~dispatch
IdleEventuallyCloses == Quiet ~> (rec = "retired" \/ turn \/ dispatch \/ srv = "down")
====
