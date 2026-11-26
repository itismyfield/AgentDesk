---- MODULE HerdrIdleExpire ----
\* One channel's Herdr execution under the 6-hour idle-kill tick (policies/timeouts/idle-kill.js ->
\* kill-tmux), across one dcserver restart. Code base: c28732c52c (Before: the host guard keeps
\* every Herdr row and records an idle_cleanup_preserved alarm each tick).
\* Expire TRUE is the fix: herdr_terminate/idle_expire.rs closes the pane, retires the row and
\* disconnects it with its provider session kept. Each other constant removes one guard.
\* turn: the mailbox holds an active turn token; inflight: the channel's inflight row file exists;
\* dispatch: sessions.active_dispatch_id is set; idle: status idle with a heartbeat over 6 h old.
EXTENDS Naturals

CONSTANTS Expire, CheckTurn, CheckInflight, CheckIdle, Exclude, RecordResume

VARIABLES prov, srv, registered, rec, pane, turn, inflight, dispatch, idle, phase, alerts,
          closedBusy, closedUsed, selector, intent, resumed

vars == <<prov, srv, registered, rec, pane, turn, inflight, dispatch, idle, phase, alerts,
          closedBusy, closedUsed, selector, intent, resumed>>

Init ==
    /\ prov \in {"claude", "codex"}
    /\ srv = "up" /\ registered = TRUE
    /\ rec = "bound" /\ pane = "live"
    /\ turn = FALSE /\ inflight = FALSE /\ dispatch = FALSE /\ idle = FALSE
    /\ phase = "none" /\ alerts = 0 /\ closedBusy = FALSE /\ closedUsed = FALSE
    /\ selector = "live" /\ intent = FALSE /\ resumed = "none"

\* phase "reserved": the transition lock and the sessions row lock are held and the fence is shut.
\* No input starts under the transition lock (terminate_scoped acquire, queue_io.rs users).
Unlocked == phase # "reserved"

\* The next launch on a Retired row resumes the kept selector: Claude from claude_session_id, Codex
\* only through the owner's idle resume intent naming the ended thread (restart_intent.rs).
Resumes == selector = "kept" /\ (prov = "claude" \/ intent)

\* Human, bot or task-notification input. The mailbox token is set before the inflight row is
\* saved. On a retired row the turn launches a new execution (install_pending_pg over Retired).
Input ==
    /\ srv = "up" /\ registered /\ Unlocked /\ ~turn
    /\ turn' = TRUE /\ idle' = FALSE
    /\ IF rec = "retired"
         THEN /\ rec' = "bound" /\ pane' = "live" /\ selector' = "live" /\ intent' = FALSE
              /\ resumed' = IF Resumes THEN "same" ELSE "fresh"
         ELSE UNCHANGED <<rec, pane, selector, intent, resumed>>
    /\ UNCHANGED <<prov, srv, registered, inflight, dispatch, phase, alerts, closedBusy,
                   closedUsed>>

SaveInflight ==
    /\ srv = "up" /\ turn /\ ~inflight
    /\ inflight' = TRUE
    /\ UNCHANGED <<prov, srv, registered, rec, pane, turn, dispatch, idle, phase, alerts,
                   closedBusy, closedUsed, selector, intent, resumed>>

\* A session PATCH writes active_dispatch_id with a plain UPDATE, which waits behind the idle
\* end's row lock (Exclude); the row stops being idle.
DispatchAttach ==
    /\ srv = "up" /\ ~dispatch /\ (~Exclude \/ phase # "reserved")
    /\ dispatch' = TRUE /\ idle' = FALSE
    /\ UNCHANGED <<prov, srv, registered, rec, pane, turn, inflight, phase, alerts, closedBusy,
                   closedUsed, selector, intent, resumed>>

\* The finalizer clears both and refreshes the heartbeat, so the row is not idle again yet.
TurnEnd ==
    /\ srv = "up" /\ turn
    /\ turn' = FALSE /\ inflight' = FALSE /\ dispatch' = FALSE
    /\ UNCHANGED <<prov, srv, registered, rec, pane, idle, phase, alerts, closedBusy, closedUsed,
                   selector, intent, resumed>>

\* Six hours pass with no heartbeat. A quiet long turn or a restart can leave the heartbeat
\* stale while a turn or its inflight row is open.
Elapse ==
    /\ ~idle
    /\ idle' = TRUE
    /\ UNCHANGED <<prov, srv, registered, rec, pane, turn, inflight, dispatch, phase, alerts,
                   closedBusy, closedUsed, selector, intent, resumed>>

TurnClear == (~CheckTurn \/ ~turn) /\ (~CheckInflight \/ ~inflight)

\* idle-kill.js selects status idle, no dispatch, 6 hours, up to 50 rows read once. kill-tmux then
\* pre-checks dispatch, inflight and the runtime, and records a Codex owner's idle resume intent.
IdleKillTick ==
    /\ srv = "up" /\ rec = "bound" /\ idle /\ ~dispatch /\ phase = "none"
    /\ IF ~Expire
         THEN /\ alerts' = IF alerts < 2 THEN alerts + 1 ELSE alerts
              /\ UNCHANGED <<phase, intent>>
         ELSE /\ phase' = IF registered /\ TurnClear THEN "judged" ELSE "none"
              /\ intent' = (registered /\ TurnClear /\ prov = "codex" /\ RecordResume)
              /\ UNCHANGED alerts
    /\ UNCHANGED <<prov, srv, registered, rec, pane, turn, inflight, dispatch, idle, closedBusy,
                   closedUsed, selector, resumed>>

\* Under the transition lock: the same-turn read, then the row lock reading status, heartbeat and
\* dispatch (IdleGate::reserve), the inflight re-read, the fence. A refusal cancels the intent.
Reserve ==
    /\ srv = "up" /\ phase = "judged"
    /\ IF TurnClear /\ (~CheckIdle \/ idle) /\ ~dispatch
         THEN /\ phase' = "reserved" /\ UNCHANGED intent
         ELSE /\ phase' = "none" /\ intent' = FALSE
    /\ UNCHANGED <<prov, srv, registered, rec, pane, turn, inflight, dispatch, idle, alerts,
                   closedBusy, closedUsed, selector, resumed>>

\* close_fenced and read_end, the row lock released, then retire_confirmed and the disconnect with
\* keep_provider_session = true. Nothing is re-checked here: the reservation is the exclusion.
Close ==
    /\ srv = "up" /\ phase = "reserved"
    /\ rec' = "retired" /\ pane' = "closed" /\ phase' = "none" /\ selector' = "kept"
    /\ closedBusy' = (closedBusy \/ turn \/ inflight \/ dispatch)
    /\ closedUsed' = (closedUsed \/ ~idle)
    /\ UNCHANGED <<prov, srv, registered, turn, inflight, dispatch, idle, alerts, intent, resumed>>

\* The mailbox and both locks are lost; the inflight file, the pane and the owner's intent survive.
Crash ==
    /\ srv = "up"
    /\ srv' = "down" /\ registered' = FALSE /\ turn' = FALSE /\ phase' = "none"
    /\ UNCHANGED <<prov, rec, pane, inflight, dispatch, idle, alerts, closedBusy, closedUsed,
                   selector, intent, resumed>>

Boot ==
    /\ srv = "down" /\ srv' = "up"
    /\ UNCHANGED <<prov, registered, rec, pane, turn, inflight, dispatch, idle, phase, alerts,
                   closedBusy, closedUsed, selector, intent, resumed>>

Register ==
    /\ srv = "up" /\ ~registered
    /\ registered' = TRUE
    /\ UNCHANGED <<prov, srv, rec, pane, turn, inflight, dispatch, idle, phase, alerts,
                   closedBusy, closedUsed, selector, intent, resumed>>

\* restore_inflight.rs puts the persisted turn back in the mailbox after the runtime registers.
Restore ==
    /\ srv = "up" /\ registered /\ inflight /\ ~turn /\ rec = "bound"
    /\ turn' = TRUE
    /\ UNCHANGED <<prov, srv, registered, rec, pane, inflight, dispatch, idle, phase, alerts,
                   closedBusy, closedUsed, selector, intent, resumed>>

Next ==
    \/ Input \/ SaveInflight \/ DispatchAttach \/ TurnEnd \/ Elapse \/ IdleKillTick \/ Reserve
    \/ Close \/ Crash \/ Boot \/ Register \/ Restore

Spec == Init /\ [][Next]_vars
    /\ WF_vars(IdleKillTick) /\ WF_vars(Reserve) /\ WF_vars(Close)
    /\ WF_vars(Boot) /\ WF_vars(Register)

\* INVARIANT: a pane is never closed under an active turn, an open inflight row or a dispatch.
NeverClosesBusy == ~closedBusy
\* INVARIANT: a pane used again after the tick selected it is not closed (idle read at the close).
NeverClosesUsed == ~closedUsed
\* INVARIANT: the next message after an idle close resumes the same provider session.
NextMessageResumes == resumed # "fresh"
\* INVARIANT: the preserved alarm is not repeated tick after tick.
AlarmNotRepeated == alerts <= 1
TypeOK ==
    /\ prov \in {"claude", "codex"} /\ rec \in {"bound", "retired"} /\ pane \in {"live", "closed"}
    /\ phase \in {"none", "judged", "reserved"} /\ alerts \in 0..2
    /\ selector \in {"live", "kept"} /\ resumed \in {"none", "same", "fresh"}

\* PROPERTY: a quiet, idle, undispatched Herdr session is eventually closed unless work arrives.
Quiet == srv = "up" /\ rec = "bound" /\ idle /\ ~turn /\ ~inflight /\ ~dispatch
IdleEventuallyCloses == Quiet ~> (rec = "retired" \/ turn \/ dispatch \/ srv = "down")
====
