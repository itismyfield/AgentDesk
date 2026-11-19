---- MODULE HerdrRetiredRow ----
\* A Herdr execution retired under a running turn (HTTP force cancel or live row end) and the
\* channel's sessions row status after it. Code base: 8caf0e5d8c80b7ebc29a62111b36b4fac7decf03;
\* Fence/Cleanup TRUE is the r4 fix (retired_status_fence.rs, herdr_row_end_live.rs disconnect_ended).
\* exec: hosted_execution state; row: sessions.status; nonce: active_turn_nonce (1 retired turn, 2 new).
\* bridge: the retired turn's reader. A force cancel settles it mutely (finalize.rs:203 keeps the
\* token uncancelled), so it keeps posting "working" every 30 s (stream_tick.rs:663).
\* lost: the inflight row the reader owned is gone or a successor's; ReaderStops TRUE is the r5 fix
\* (turn_bridge/session_row_writer.rs) that ends the reader's active writes from then on.
EXTENDS Naturals
CONSTANTS Path, Fence, FenceNonce, Cleanup, NewTurns, Restarts, ReaderStops
VARIABLES exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
          lost, lostWrite, oldOverNew

vars == <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
          lost, lostWrite, oldOverNew>>
readerVars == <<lost, lostWrite, oldOverNew>>
Ended == {"disconnected", "idle"}

Init ==
    /\ exec = "bound" /\ row = "turn_active" /\ nonce = 1 /\ fresh = TRUE /\ bridge = "alive"
    /\ ended = FALSE /\ refused = FALSE /\ turns = 0 /\ restarts = 0 /\ pres = "busy"
    /\ phase = "run" /\ lost = FALSE /\ lostWrite = FALSE /\ oldOverNew = FALSE

\* update_target WHERE (canonical_identity.rs:927) plus retired_revival_fence when Fence.
Blocked(s, n) ==
    /\ Fence /\ s = "turn_active" /\ exec = "retired" /\ row # "turn_active"
    /\ (~FenceNonce \/ n = 0 \/ n = nonce)

\* An accepted hook write sets status, the turn nonce it carries, and last_heartbeat.
Write(s, n) ==
    /\ row' = IF Blocked(s, n) THEN row ELSE s
    /\ nonce' = IF Blocked(s, n) \/ n = 0 THEN nonce ELSE n
    /\ fresh' = IF Blocked(s, n) THEN fresh ELSE TRUE
    /\ ended' = (ended \/ (exec = "retired" /\ row' \in Ended))

\* The retired turn's reader writing status s: stopped once lost under ReaderStops; an accepted
\* write after the loss or over the new turn's row is recorded.
ReaderWrite(s) ==
    IF ReaderStops /\ lost
    THEN UNCHANGED <<row, nonce, fresh, ended, lostWrite, oldOverNew>>
    ELSE /\ Write(s, 0)
         /\ lostWrite' = (lostWrite \/ (lost /\ ~Blocked(s, 0)))
         /\ oldOverNew' = (oldOverNew \/ (nonce = 2 /\ ~Blocked(s, 0)))

\* herdr_terminate.rs:607 retire_confirmed; queue.rs:464 force or herdr_row_end_live.rs:67.
Retire ==
    /\ phase = "run" /\ exec' = "retired"
    /\ phase' = IF Path = "force" THEN "postlude" ELSE "liveend"
    /\ lost' = (Path = "force")
    /\ UNCHANGED <<row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, lostWrite, oldOverNew>>

\* cancel_effects.rs:383 -> http_force.rs:34 disconnect_retired_pg; the reader is not cancelled.
ForcePostlude ==
    /\ phase = "postlude" /\ phase' = "done" /\ row' = "disconnected" /\ ended' = TRUE
    /\ UNCHANGED <<exec, nonce, fresh, bridge, refused, turns, restarts, pres, readerVars>>

\* herdr_row_end_live.rs:82-93: (fix) disconnect_ended, then settle_captured, then cancel the reader.
\* One atomic step: the model does not check the order inside it; a PG test pins disconnect first.
LiveEnd ==
    /\ phase = "liveend" /\ phase' = "done"
    /\ row' = IF Cleanup THEN "disconnected" ELSE row
    /\ ended' = (ended \/ Cleanup)
    /\ bridge' = IF bridge = "alive" THEN "exiting" ELSE bridge
    /\ lost' = TRUE
    /\ UNCHANGED <<exec, nonce, fresh, refused, turns, restarts, pres, lostWrite, oldOverNew>>

\* stream_tick.rs:663: the reader's 30 s "working" hook (normalized to turn_active).
Heartbeat ==
    /\ bridge = "alive" /\ ~(ReaderStops /\ lost) /\ ReaderWrite("turn_active")
    /\ UNCHANGED <<exec, bridge, refused, turns, restarts, pres, phase, lost>>

\* post_loop_finalize.rs:450 TURN_ACTIVE once the reader's loop ends.
BridgeExit ==
    /\ bridge = "exiting" /\ bridge' = "final" /\ ReaderWrite("turn_active")
    /\ UNCHANGED <<exec, refused, turns, restarts, pres, phase, lost>>

\* completion_postlude.rs:211 terminal IDLE under the turn's nonce, or skipped when its
\* channel-effect/status-panel conditions fail.
BridgeFinal ==
    /\ bridge = "final" /\ bridge' = "gone"
    /\ \/ nonce = 1 /\ Write("idle", 0)
       \/ UNCHANGED <<row, nonce, fresh, ended>>
    /\ UNCHANGED <<exec, refused, turns, restarts, pres, phase, readerVars>>

\* STALE_TURN_GRACE passes only while no accepted heartbeat lands; a running new turn's own
\* reader keeps its row fresh.
StaleTick ==
    /\ fresh /\ ~(bridge = "alive" /\ ~(ReaderStops /\ lost) /\ ~Blocked("turn_active", 0))
    /\ ~(nonce = 2 /\ pres = "busy")
    /\ fresh' = FALSE
    /\ UNCHANGED <<exec, row, nonce, bridge, ended, refused, turns, restarts, pres, phase, readerVars>>

\* stale_turn_reconciler.rs:698/766: no dispatch, stale heartbeat, no tmux pane -> idle.
Sweep ==
    /\ row = "turn_active" /\ ~fresh /\ row' = "idle"
    /\ ended' = (ended \/ exec = "retired")
    /\ UNCHANGED <<exec, nonce, fresh, bridge, refused, turns, restarts, pres, phase, readerVars>>

\* A new incarnation's Pending record (hosted_execution.rs:407 install_pending_pg).
Install ==
    /\ NewTurns > 0 /\ phase = "done" /\ exec = "retired" /\ exec' = "pending"
    /\ UNCHANGED <<row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase, readerVars>>

\* adk_session.rs:252 / session_locator.rs:429 turn start under a new nonce, before or after Install.
NewTurn ==
    /\ phase = "done" /\ turns < NewTurns /\ turns' = turns + 1
    /\ refused' = (refused \/ Blocked("turn_active", 2)) /\ Write("turn_active", 2)
    /\ pres' = "busy"
    /\ UNCHANGED <<exec, bridge, restarts, phase, readerVars>>

\* The new turn's own terminal IDLE under nonce 2.
NewTurnEnd ==
    /\ nonce = 2 /\ row = "turn_active" /\ pres = "busy" /\ pres' = "idle"
    /\ Write("idle", 0)
    /\ UNCHANGED <<exec, bridge, refused, turns, restarts, phase, readerVars>>

\* dcserver restart: the reader dies with the process; the inflight row the cancel removed is not
\* restored, so boot posts no status for the retired turn.
Restart ==
    /\ phase = "done" /\ restarts < Restarts /\ restarts' = restarts + 1 /\ bridge' = "gone"
    /\ UNCHANGED <<exec, row, nonce, fresh, ended, refused, turns, pres, phase, readerVars>>

\* turn_presence/activity.rs:733-735: an open transcript quiet for STALE_OPEN_AFTER leaves busy.
PresenceStale ==
    /\ pres = "busy" /\ exec # "bound" /\ nonce = 1 /\ pres' = "unknown"
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, phase, readerVars>>

Next ==
    \/ Retire \/ ForcePostlude \/ LiveEnd \/ Heartbeat \/ BridgeExit \/ BridgeFinal
    \/ StaleTick \/ Sweep \/ Install \/ NewTurn \/ NewTurnEnd \/ Restart \/ PresenceStale

Fair ==
    /\ WF_vars(Retire) /\ WF_vars(ForcePostlude) /\ WF_vars(LiveEnd) /\ WF_vars(Heartbeat)
    /\ WF_vars(BridgeExit) /\ WF_vars(BridgeFinal) /\ WF_vars(StaleTick) /\ WF_vars(Sweep)
    /\ WF_vars(NewTurnEnd) /\ WF_vars(PresenceStale) /\ WF_vars(Restart)

Spec == Init /\ [][Next]_vars /\ Fair

\* The retired turn's row, once ended, never reads as an active turn of that turn again.
NoRevival == ~(ended /\ row = "turn_active" /\ nonce = 1)
\* Once the cancel or row end finished, the cancel selection (status = 'turn_active') skips the
\* retired turn's row.
EndedTurnNotSelectable == (phase = "done" /\ nonce = 1) => row # "turn_active"
\* A turn start under a new nonce is never refused.
NewTurnNotRefused == ~refused
\* The reader that lost its inflight row never writes the session row again.
LostReaderNeverWrites == ~lostWrite
\* The retired turn's reader never overwrites the new incarnation turn's row.
NewTurnRowNotOverwritten == ~oldOverNew

RowSettles == <>[](row \in Ended)
PresenceLeavesBusy == (exec = "retired" /\ nonce = 1) ~> (pres # "busy")
====
