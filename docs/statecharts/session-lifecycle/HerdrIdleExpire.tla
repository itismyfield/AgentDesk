---- MODULE HerdrIdleExpire ----
\* One channel's Herdr execution under the 6-hour idle-kill tick (policies/timeouts/idle-kill.js ->
\* kill-tmux), across one dcserver restart. Code base: c28732c52c (Before: the host guard keeps
\* every Herdr row and records an idle_cleanup_preserved alarm each tick).
\* Expire TRUE is the fix: herdr_terminate/idle_expire.rs closes the pane, retires the row and
\* disconnects it with its provider session kept. CheckTurn/CheckInflight remove one guard each.
\* turn: the mailbox holds an active turn token; inflight: the channel's inflight row file exists;
\* dispatch: sessions.active_dispatch_id is set. idle: the row's heartbeat is older than 6 hours.
EXTENDS Naturals

CONSTANTS Expire, CheckTurn, CheckInflight

VARIABLES srv, registered, rec, pane, turn, inflight, dispatch, idle, phase, alerts,
          closedBusy, resumed, kept

vars == <<srv, registered, rec, pane, turn, inflight, dispatch, idle, phase, alerts,
          closedBusy, resumed, kept>>

Init ==
    /\ srv = "up" /\ registered = TRUE
    /\ rec = "bound" /\ pane = "live"
    /\ turn = FALSE /\ inflight = FALSE /\ dispatch = FALSE /\ idle = FALSE
    /\ phase = "none" /\ alerts = 0 /\ closedBusy = FALSE
    /\ resumed = "none" /\ kept = TRUE

\* A turn holds the transition lock to start, so no input starts while the expire holds it
\* (herdr_terminate.rs terminate_scoped acquire, queue_io.rs / mailbox_ops.rs lock users).
Unlocked == phase # "fenced"

\* Human, bot or task-notification input. The mailbox token is set before the inflight row is
\* saved. On a retired row the turn launches a new execution that resumes the kept provider
\* session (herdr_launch.rs install_pending_pg over Retired).
Input ==
    /\ srv = "up" /\ registered /\ Unlocked /\ ~turn
    /\ turn' = TRUE /\ idle' = FALSE
    /\ IF rec = "retired"
         THEN /\ rec' = "bound" /\ pane' = "live"
              /\ resumed' = IF kept THEN "same" ELSE "fresh"
         ELSE UNCHANGED <<rec, pane, resumed>>
    /\ UNCHANGED <<srv, registered, inflight, dispatch, phase, alerts, closedBusy, kept>>

SaveInflight ==
    /\ srv = "up" /\ turn /\ ~inflight
    /\ inflight' = TRUE
    /\ UNCHANGED <<srv, registered, rec, pane, turn, dispatch, idle, phase, alerts, closedBusy,
                   resumed, kept>>

\* A dispatch is attached in PG without the transition lock; its turn starts like an input.
DispatchAttach ==
    /\ srv = "up" /\ ~dispatch
    /\ dispatch' = TRUE /\ idle' = FALSE
    /\ UNCHANGED <<srv, registered, rec, pane, turn, inflight, phase, alerts, closedBusy,
                   resumed, kept>>

TurnEnd ==
    /\ srv = "up" /\ turn
    /\ turn' = FALSE /\ inflight' = FALSE /\ dispatch' = FALSE
    /\ UNCHANGED <<srv, registered, rec, pane, idle, phase, alerts, closedBusy, resumed, kept>>

\* Six hours pass with no heartbeat. A quiet long turn or a restart can leave the heartbeat
\* stale while a turn or its inflight row is open (the tmux route's live-activity guard exists
\* for the same reason).
Elapse ==
    /\ ~idle
    /\ idle' = TRUE
    /\ UNCHANGED <<srv, registered, rec, pane, turn, inflight, dispatch, phase, alerts,
                   closedBusy, resumed, kept>>

TurnClear == (~CheckTurn \/ ~turn) /\ (~CheckInflight \/ ~inflight)

\* idle-kill.js selects status idle, no dispatch, 6 hours; kill-tmux then reaches the host guard.
IdleKillTick ==
    /\ srv = "up" /\ rec = "bound" /\ idle /\ ~dispatch /\ phase = "none"
    /\ IF ~Expire
         THEN /\ alerts' = IF alerts < 2 THEN alerts + 1 ELSE alerts
              /\ UNCHANGED phase
         ELSE /\ phase' = IF registered /\ TurnClear THEN "judged" ELSE "none"
              /\ UNCHANGED alerts
    /\ UNCHANGED <<srv, registered, rec, pane, turn, inflight, dispatch, idle, closedBusy,
                   resumed, kept>>

\* The transition lock, then the same-turn read and the inflight re-read before the fence.
Fence ==
    /\ srv = "up" /\ phase = "judged"
    /\ phase' = IF TurnClear THEN "fenced" ELSE "none"
    /\ UNCHANGED <<srv, registered, rec, pane, turn, inflight, dispatch, idle, alerts,
                   closedBusy, resumed, kept>>

\* Under the fence the row must be unchanged and name no dispatch (dispatch_holds with none).
Close ==
    /\ srv = "up" /\ phase = "fenced"
    /\ IF dispatch
         THEN /\ phase' = "none"
              /\ UNCHANGED <<rec, pane, closedBusy>>
         ELSE /\ rec' = "retired" /\ pane' = "closed" /\ phase' = "none"
              /\ closedBusy' = (closedBusy \/ turn \/ inflight)
    /\ UNCHANGED <<srv, registered, turn, inflight, dispatch, idle, alerts, resumed, kept>>

\* The mailbox is lost; the inflight file and the pane survive. The runtime registers late.
Crash ==
    /\ srv = "up"
    /\ srv' = "down" /\ registered' = FALSE /\ turn' = FALSE /\ phase' = "none"
    /\ UNCHANGED <<rec, pane, inflight, dispatch, idle, alerts, closedBusy, resumed, kept>>

Boot ==
    /\ srv = "down" /\ srv' = "up"
    /\ UNCHANGED <<registered, rec, pane, turn, inflight, dispatch, idle, phase, alerts,
                   closedBusy, resumed, kept>>

Register ==
    /\ srv = "up" /\ ~registered
    /\ registered' = TRUE
    /\ UNCHANGED <<srv, rec, pane, turn, inflight, dispatch, idle, phase, alerts, closedBusy,
                   resumed, kept>>

\* restore_inflight.rs puts the persisted turn back in the mailbox after the runtime registers.
Restore ==
    /\ srv = "up" /\ registered /\ inflight /\ ~turn /\ rec = "bound"
    /\ turn' = TRUE
    /\ UNCHANGED <<srv, registered, rec, pane, inflight, dispatch, idle, phase, alerts,
                   closedBusy, resumed, kept>>

Next ==
    \/ Input \/ SaveInflight \/ DispatchAttach \/ TurnEnd \/ Elapse \/ IdleKillTick \/ Fence
    \/ Close \/ Crash \/ Boot \/ Register \/ Restore

Spec == Init /\ [][Next]_vars
    /\ WF_vars(IdleKillTick) /\ WF_vars(Fence) /\ WF_vars(Close)
    /\ WF_vars(Boot) /\ WF_vars(Register)

\* INVARIANT: a pane is never closed under an active turn or an open inflight row.
NeverClosesBusy == ~closedBusy
\* INVARIANT: the next message after an idle close resumes the same provider session.
NextMessageResumes == resumed # "fresh"
\* INVARIANT: the preserved alarm is not repeated tick after tick.
AlarmNotRepeated == alerts <= 1
TypeOK ==
    /\ rec \in {"bound", "retired"} /\ pane \in {"live", "closed"}
    /\ phase \in {"none", "judged", "fenced"} /\ alerts \in 0..2

\* PROPERTY: a quiet, idle, undispatched Herdr session is eventually closed unless work arrives.
Quiet == srv = "up" /\ rec = "bound" /\ idle /\ ~turn /\ ~inflight /\ ~dispatch
IdleEventuallyCloses == Quiet ~> (rec = "retired" \/ turn \/ dispatch \/ srv = "down")
====
