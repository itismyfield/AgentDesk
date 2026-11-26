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
          OrphanCodex, ClearForgets

VARIABLES prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle, phase,
          alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed, cleared,
          clearResumed

vars == <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle, phase,
          alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed, cleared,
          clearResumed>>

\* Every action below leaves the /clear pair as it is unless it names it.
KeepClear == UNCHANGED <<cleared, clearResumed>>

Init ==
    /\ prov \in {"claude", "codex"} /\ gone \in BOOLEAN
    /\ srv = "up" /\ registered = ~gone /\ grace = FALSE
    /\ rec = "bound" /\ pane = "live"
    /\ turn = FALSE /\ inflight = FALSE /\ dispatch = FALSE /\ idle = FALSE
    /\ phase = "none" /\ alerts = 0 /\ closedBusy = FALSE /\ closedUsed = FALSE
    /\ selector = "live" /\ slot = "none" /\ file = FALSE /\ effort = "none" /\ resumed = "none"
    /\ cleared = FALSE /\ clearResumed = FALSE

\* phase "reserved": the transition lock and the sessions row lock are held and the fence is shut.
\* No input or /effort end runs under the transition lock (terminate_scoped acquire).
Unlocked == phase # "reserved"

\* slot: the owner's one effort intent (mailbox snapshot), "cancelled" once its operation is
\* cancelled. file: the idle end's own thread record (restart_intent/idle_resume.rs), kept apart
\* from the owner. The next launch on a Retired row resumes: Claude from the kept claude_session_id,
\* Codex only through an open intent or record, whatever the SQL selector says
\* (codex/herdr_turn.rs:216 -> restart_intent.rs:68-71 -> restart_intent/idle_resume.rs:86).
Resumes == IF prov = "claude" THEN selector = "kept" ELSE slot \in {"effort", "idle"} \/ file

\* Input of the owning bot. On a retired row the turn launches a new execution, which spends any
\* intent or record naming the old one; a pending /effort end still names the old one. After a
\* /clear the launch must be fresh (hosted_execution.rs:419 install_pending_pg over Retired).
Input ==
    /\ srv = "up" /\ registered /\ Unlocked /\ ~turn
    /\ turn' = TRUE /\ idle' = FALSE
    /\ IF rec = "retired"
         THEN /\ rec' = "bound" /\ pane' = "live" /\ selector' = "live"
              /\ slot' = "none" /\ file' = FALSE
              /\ resumed' = IF cleared THEN resumed ELSE IF Resumes THEN "same" ELSE "fresh"
              /\ effort' = IF effort = "recorded" THEN "stale" ELSE effort
              /\ clearResumed' = (clearResumed \/ (cleared /\ Resumes))
              /\ cleared' = FALSE
         ELSE UNCHANGED <<rec, pane, selector, slot, file, resumed, effort, cleared, clearResumed>>
    /\ UNCHANGED <<prov, gone, srv, registered, grace, inflight, dispatch, phase, alerts,
                   closedBusy, closedUsed>>

SaveInflight ==
    /\ srv = "up" /\ turn /\ ~inflight
    /\ inflight' = TRUE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

\* A session PATCH writes active_dispatch_id with a plain UPDATE (db/dispatched_sessions.rs:2136),
\* which waits behind the idle end's row lock (Exclude, idle_expire.rs:88); the row stops being idle.
DispatchAttach ==
    /\ srv = "up" /\ ~dispatch /\ (~Exclude \/ phase # "reserved")
    /\ dispatch' = TRUE /\ idle' = FALSE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, phase, alerts,
                   closedBusy, closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

\* The finalizer clears both and refreshes the heartbeat, so the row is not idle again yet.
TurnEnd ==
    /\ srv = "up" /\ turn
    /\ turn' = FALSE /\ inflight' = FALSE /\ dispatch' = FALSE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, idle, phase, alerts, closedBusy,
                   closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

\* Six hours pass with no heartbeat. A quiet long turn or a restart can leave the heartbeat
\* stale while a turn or its inflight row is open.
Elapse ==
    /\ ~idle
    /\ idle' = TRUE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

\* /effort with no turn (effort_restart/herdr.rs:141 restart): its intent is stored in the owner
\* slot before the transition lock (:207 tmux::record, :222 terminate_for_http), then the end of that exact execution runs under the lock. An end
\* that finds it already retired cancels its own operation only (:232 -> effort_continue.rs:118).
EffortStart ==
    /\ srv = "up" /\ registered /\ rec = "bound" /\ ~turn /\ effort = "none"
    /\ effort' = "recorded" /\ slot' = "effort"
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle,
                   phase, alerts, closedBusy, closedUsed, selector, file, resumed>>
    /\ KeepClear

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
    /\ KeepClear

TurnClear == (~CheckTurn \/ ~turn) /\ (~CheckInflight \/ ~inflight)

\* Without the owner's runtime once the boot grace passed, the orphan path ends the execution
\* (idle_expire.rs:235-242 -> :284 end_orphan); a Codex one only when OrphanCodex.
Orphan == ~registered /\ grace /\ (prov = "claude" \/ OrphanCodex)

\* idle-kill.js selects status idle, no dispatch, 6 hours, up to 50 rows read once. kill-tmux
\* (tmux_cleanup.rs:118-121 -> idle_expire.rs:179) pre-checks dispatch (:205), inflight (:208) and
\* the runtime. Without KeepAtLock (the previous head) the owned
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
    /\ KeepClear

\* Under the transition lock (herdr_terminate.rs:510): the same-turn read, then (:550 ->
\* idle_expire.rs:59) the row lock reading status, heartbeat and dispatch (:88), the inflight
\* re-read, the Codex thread record (:114 -> idle_resume.rs:43), the fence; orphan: :300-303. A refusal writes nothing; the previous head cancelled the slot it had overwritten.
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
    /\ KeepClear

\* close_fenced (herdr_terminate.rs:623; orphan: skipped when idle_expire.rs:345 proves pane and
\* provider gone) and read_end, the row lock released (:629 -> idle_expire.rs:125), retire
\* (herdr_terminate.rs:632, idle_expire.rs:318) and the disconnect (idle_expire.rs:270).
Close ==
    /\ srv = "up" /\ phase = "reserved"
    /\ rec' = "retired" /\ pane' = "closed" /\ phase' = "none" /\ selector' = "kept"
    /\ closedBusy' = (closedBusy \/ turn \/ inflight \/ dispatch)
    /\ closedUsed' = (closedUsed \/ ~idle)
    /\ UNCHANGED <<prov, gone, srv, registered, grace, turn, inflight, dispatch, idle, alerts,
                   slot, file, effort, resumed>>
    /\ KeepClear

\* The mailbox and both locks are lost; the inflight file, the pane, the slot and the record survive.
Crash ==
    /\ srv = "up"
    /\ srv' = "down" /\ registered' = FALSE /\ grace' = FALSE /\ turn' = FALSE
    /\ phase' = "none" /\ effort' = "none"
    /\ UNCHANGED <<prov, gone, rec, pane, inflight, dispatch, idle, alerts, closedBusy,
                   closedUsed, selector, slot, file, resumed>>
    /\ KeepClear

Boot ==
    /\ srv = "down" /\ srv' = "up"
    /\ UNCHANGED <<prov, gone, registered, grace, rec, pane, turn, inflight, dispatch, idle,
                   phase, alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

\* ORPHAN_AFTER_BOOT_SECS (idle_expire.rs:162, health.rs:449): a missing owner runtime is gone.
GracePass ==
    /\ srv = "up" /\ ~grace
    /\ grace' = TRUE
    /\ UNCHANGED <<prov, gone, srv, registered, rec, pane, turn, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

Register ==
    /\ srv = "up" /\ ~registered /\ ~gone
    /\ registered' = TRUE
    /\ UNCHANGED <<prov, gone, srv, grace, rec, pane, turn, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

\* The removed bot is added back; its next input is the owner's next launch.
Return ==
    /\ gone
    /\ gone' = FALSE
    /\ UNCHANGED <<prov, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

\* restore_inflight.rs puts the persisted turn back in the mailbox after the runtime registers.
Restore ==
    /\ srv = "up" /\ registered /\ inflight /\ ~turn /\ rec = "bound"
    /\ turn' = TRUE
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, inflight, dispatch, idle, phase,
                   alerts, closedBusy, closedUsed, selector, slot, file, effort, resumed>>
    /\ KeepClear

\* The owner's /clear of a retired execution (commands/control/herdr_recovery.rs:116-131): the
\* idle record for that owner and nonce is cancelled first (ClearForgets, idle_resume.rs:75), then
\* the SQL selector (http_force.rs clear_retired_selector_pg); the owner slot's open intent is
\* cancelled too (queued_head/v2/clear.rs:219-224).
Clear ==
    /\ srv = "up" /\ registered /\ rec = "retired" /\ Unlocked /\ ~cleared
    /\ cleared' = TRUE /\ selector' = "live"
    /\ slot' = IF slot \in {"effort", "idle"} THEN "cancelled" ELSE slot
    /\ file' = (file /\ ~ClearForgets)
    /\ UNCHANGED <<prov, gone, srv, registered, grace, rec, pane, turn, inflight, dispatch, idle,
                   phase, alerts, closedBusy, closedUsed, effort, resumed, clearResumed>>

Next ==
    \/ Input \/ SaveInflight \/ DispatchAttach \/ TurnEnd \/ Elapse \/ EffortStart \/ EffortEnd
    \/ IdleKillTick \/ Reserve \/ Close \/ Crash \/ Boot \/ GracePass \/ Register \/ Return
    \/ Restore \/ Clear

Spec == Init /\ [][Next]_vars
    /\ WF_vars(IdleKillTick) /\ WF_vars(Reserve) /\ WF_vars(Close)
    /\ WF_vars(Boot) /\ WF_vars(GracePass) /\ WF_vars(Register)

\* INVARIANT: a pane is never closed under an active turn, an open inflight row or a dispatch.
NeverClosesBusy == ~closedBusy
\* INVARIANT: a pane used again after the tick selected it is not closed (idle read at the close).
NeverClosesUsed == ~closedUsed
\* INVARIANT: the next message after an end that kept the session resumes the same session.
NextMessageResumes == resumed # "fresh"
\* INVARIANT: the first launch after a /clear never resumes the conversation the clear ended.
ClearNeverResumes == ~clearResumed
\* INVARIANT: the preserved alarm is not repeated tick after tick.
AlarmNotRepeated == alerts <= 1
TypeOK ==
    /\ prov \in {"claude", "codex"} /\ gone \in BOOLEAN /\ rec \in {"bound", "retired"}
    /\ pane \in {"live", "closed"} /\ phase \in {"none", "judged", "reserved"} /\ alerts \in 0..2
    /\ selector \in {"live", "kept"} /\ slot \in {"none", "effort", "idle", "cancelled"}
    /\ file \in BOOLEAN /\ effort \in {"none", "recorded", "stale"} /\ resumed \in {"none", "same", "fresh"}
    /\ cleared \in BOOLEAN /\ clearResumed \in BOOLEAN

\* PROPERTY: a quiet, idle, undispatched Herdr session is eventually closed unless work arrives,
\* with or without its owner's runtime.
Quiet == srv = "up" /\ rec = "bound" /\ idle /\ ~turn /\ ~inflight /\ ~dispatch
IdleEventuallyCloses == Quiet ~> (rec = "retired" \/ turn \/ dispatch \/ srv = "down")
====
