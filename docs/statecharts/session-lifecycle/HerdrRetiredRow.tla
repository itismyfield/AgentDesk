---- MODULE HerdrRetiredRow ----
\* A Herdr execution retired under a running turn (HTTP force cancel or live row end) and the
\* channel's sessions row status after it. Code base: 8caf0e5d8c80b7ebc29a62111b36b4fac7decf03;
\* Fence/Cleanup TRUE is the r4 fix (retired_status_fence.rs, herdr_row_end_live.rs disconnect_ended).
\* exec: hosted_execution state; row: sessions.status; nonce: active_turn_nonce (1 retired turn, 2 new).
\* bridge: the retired turn's reader. A force cancel settles it mutely (finalize.rs:203 keeps the
\* token uncancelled), so it keeps posting "working" every 30 s (stream_tick.rs:663).
\* lost: the inflight row the reader owned is gone or a successor's; ReaderStops TRUE is the r5 fix
\* (turn_bridge/session_row_writer.rs) that ends the reader's active writes from then on.
\* seen: a guarded save of the reader read its owned row as gone. FlushObserves TRUE (r5 fix 1) makes
\* the tick's dirty flush release the writer too; ToolGuard TRUE (r5 fix 1) makes a released reader's
\* tool frame record no tool mark or background child (stream_loop/tool_arms/session_activity.rs).
\* Owner # "off" (#8435 S3, code b04ea56650) adds the turn's inflight row after an HTTP terminate
\* (Path "terminate") and who ends it once dcserver restarts; see the ownership section below.
EXTENDS Naturals
CONSTANTS Path, Fence, FenceNonce, Cleanup, NewTurns, Restarts, ReaderStops, FlushObserves, ToolGuard,
          Owner, Shapes, Flaws
VARIABLES exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
          lost, seen, lostWrite, oldOverNew,
          inf, igen, shape, boot, committed, notices, gone, reader, inq

vars == <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
          lost, seen, lostWrite, oldOverNew, inf, igen, shape, boot, committed, notices, gone,
          reader, inq>>
readerVars == <<lost, seen, lostWrite, oldOverNew>>
ownVars == <<inf, igen, shape, boot, committed, notices, gone, reader, inq>>
Ended == {"disconnected", "idle"}
On == Owner # "off"

Init ==
    /\ exec = "bound" /\ row = "turn_active" /\ nonce = 1 /\ fresh = TRUE /\ bridge = "alive"
    /\ ended = FALSE /\ refused = FALSE /\ turns = 0 /\ restarts = 0 /\ pres = "busy"
    /\ phase = "run" /\ lost = FALSE /\ seen = FALSE /\ lostWrite = FALSE /\ oldOverNew = FALSE
    /\ inf = (IF On THEN "present" ELSE "none") /\ igen = "current"
    /\ shape \in (IF On THEN Shapes ELSE {"pathless"})
    /\ boot = "idle" /\ committed = FALSE /\ notices = 0 /\ gone = "no"
    /\ reader = "none" /\ inq = "none"

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
         /\ lostWrite' = (lostWrite \/ (seen /\ ~Blocked(s, 0)))
         /\ oldOverNew' = (oldOverNew \/ (nonce = 2 /\ ~Blocked(s, 0)))

\* herdr_terminate.rs:606 retire_confirmed; queue.rs:464 force or herdr_row_end_live.rs:67. A force
\* cancel removes the reader's inflight row; the reader's first guarded save to read that is either
\* its tick preflight (stream_tick/runtime.rs) or its dirty flush (stream_tick.rs dirty_flush).
Retire ==
    /\ phase = "run" /\ exec' = "retired"
    /\ phase' = CASE Path = "force" -> "postlude" [] Path = "live_end" -> "liveend"
                 [] OTHER -> "settle"
    /\ seen' = (Path = "force")
    /\ \/ lost' = (Path = "force")
       \/ Path = "force" /\ lost' = FlushObserves
    /\ UNCHANGED <<row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, lostWrite, oldOverNew>>
    /\ UNCHANGED ownVars

\* The next tick's preflight reads the row as gone and releases the writer.
NextPreflight ==
    /\ seen /\ ~lost /\ bridge = "alive" /\ lost' = TRUE
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
                   seen, lostWrite, oldOverNew>>
    /\ UNCHANGED ownVars

\* tool_arms.rs: a late tool frame marks tool use (session_observability.rs mark_session_tool_use_pg,
\* refused on disconnected/aborted and retired inactive rows) and registers a background child,
\* which always writes the parent's active_children.
ToolUse ==
    /\ bridge = "alive"
    /\ IF ToolGuard /\ lost
       THEN UNCHANGED <<row, lostWrite, oldOverNew>>
       ELSE /\ row' = IF row \in {"disconnected", "aborted"}
                         \/ (exec = "retired" /\ row # "turn_active")
                      THEN row ELSE "turn_active"
            /\ lostWrite' = (lostWrite \/ seen)
            /\ oldOverNew' = (oldOverNew \/ nonce = 2)
    /\ UNCHANGED <<exec, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase, lost, seen>>
    /\ UNCHANGED ownVars

\* cancel_effects.rs:382 -> http_force.rs:34 disconnect_retired_pg; the reader is not cancelled.
ForcePostlude ==
    /\ phase = "postlude" /\ phase' = "done" /\ row' = "disconnected" /\ ended' = TRUE
    /\ UNCHANGED <<exec, nonce, fresh, bridge, refused, turns, restarts, pres, readerVars>>
    /\ UNCHANGED ownVars

\* herdr_row_end_live.rs:83-93: (fix) disconnect_ended, then settle_captured, then cancel the reader.
\* One atomic step: the model does not check the order inside it; a PG test pins disconnect first.
LiveEnd ==
    /\ phase = "liveend" /\ phase' = "done"
    /\ row' = IF Cleanup THEN "disconnected" ELSE row
    /\ ended' = (ended \/ Cleanup)
    /\ bridge' = IF bridge = "alive" THEN "exiting" ELSE bridge
    /\ lost' = TRUE /\ seen' = TRUE
    /\ UNCHANGED <<exec, nonce, fresh, refused, turns, restarts, pres, lostWrite, oldOverNew>>
    /\ UNCHANGED ownVars

\* stream_tick.rs:663: the reader's 30 s "working" hook (normalized to turn_active).
Heartbeat ==
    /\ bridge = "alive" /\ ~(ReaderStops /\ lost) /\ ReaderWrite("turn_active")
    /\ UNCHANGED <<exec, bridge, refused, turns, restarts, pres, phase, lost, seen>>
    /\ UNCHANGED ownVars

\* post_loop_finalize.rs:450 TURN_ACTIVE once the reader's loop ends.
BridgeExit ==
    /\ bridge = "exiting" /\ bridge' = "final" /\ ReaderWrite("turn_active")
    /\ UNCHANGED <<exec, refused, turns, restarts, pres, phase, lost, seen>>
    /\ UNCHANGED ownVars

\* completion_postlude.rs:211 terminal IDLE under the turn's nonce, or skipped when its
\* channel-effect/status-panel conditions fail.
BridgeFinal ==
    /\ bridge = "final" /\ bridge' = "gone"
    /\ \/ nonce = 1 /\ Write("idle", 0)
       \/ UNCHANGED <<row, nonce, fresh, ended>>
    /\ UNCHANGED <<exec, refused, turns, restarts, pres, phase, readerVars>>
    /\ UNCHANGED ownVars

\* STALE_TURN_GRACE passes only while no accepted heartbeat lands; a running new turn's own
\* reader keeps its row fresh.
StaleTick ==
    /\ fresh /\ ~(bridge = "alive" /\ ~(ReaderStops /\ lost) /\ ~Blocked("turn_active", 0))
    /\ ~(nonce = 2 /\ pres = "busy")
    /\ fresh' = FALSE
    /\ UNCHANGED <<exec, row, nonce, bridge, ended, refused, turns, restarts, pres, phase, readerVars>>
    /\ UNCHANGED ownVars

\* stale_turn_reconciler.rs:699/766: no dispatch, stale heartbeat, no tmux pane -> idle.
Sweep ==
    /\ row = "turn_active" /\ ~fresh /\ row' = "idle"
    /\ ended' = (ended \/ exec = "retired")
    /\ UNCHANGED <<exec, nonce, fresh, bridge, refused, turns, restarts, pres, phase, readerVars>>
    /\ UNCHANGED ownVars

\* A new incarnation's Pending record (hosted_execution.rs:407 install_pending_pg).
Install ==
    /\ NewTurns > 0 /\ phase = "done" /\ exec = "retired" /\ exec' = "pending"
    /\ UNCHANGED <<row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase, readerVars>>
    /\ UNCHANGED ownVars

\* adk_session.rs:252 / session_locator.rs:429 turn start under a new nonce, before or after Install.
NewTurn ==
    /\ phase = "done" /\ turns < NewTurns /\ turns' = turns + 1
    /\ refused' = (refused \/ Blocked("turn_active", 2)) /\ Write("turn_active", 2)
    /\ pres' = "busy"
    /\ UNCHANGED <<exec, bridge, restarts, phase, readerVars>>
    /\ UNCHANGED ownVars

\* The new turn's own terminal IDLE under nonce 2.
NewTurnEnd ==
    /\ nonce = 2 /\ row = "turn_active" /\ pres = "busy" /\ pres' = "idle"
    /\ Write("idle", 0)
    /\ UNCHANGED <<exec, bridge, refused, turns, restarts, phase, readerVars>>
    /\ UNCHANGED ownVars

\* dcserver restart: the reader dies with the process; the inflight row the cancel removed is not
\* restored, so boot posts no status for the retired turn. An inflight row kept by a terminate
\* (Owner # "off") is now a prior process's row and waits for boot restore.
Restart ==
    /\ phase = "done" /\ restarts < Restarts /\ restarts' = restarts + 1 /\ bridge' = "gone"
    /\ UNCHANGED <<exec, row, nonce, fresh, ended, refused, turns, pres, phase, readerVars>>
    /\ igen' = IF inf = "present" THEN "prior" ELSE igen
    /\ boot' = IF inf = "present" THEN "pending" ELSE boot
    /\ UNCHANGED <<inf, shape, committed, notices, gone, reader, inq>>

\* turn_presence/activity.rs:733-735: an open transcript quiet for STALE_OPEN_AFTER leaves busy.
PresenceStale ==
    /\ pres = "busy" /\ exec # "bound" /\ nonce = 1 /\ pres' = "unknown"
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, phase, readerVars>>
    /\ UNCHANGED ownVars


\* ---- Inflight row ownership after a terminate and a restart (Owner # "off") ----
\* Code b04ea56650. inf: the turn's inflight row; igen: its born_generation against this process
\* (reconcile_gate.rs:29); shape: the row fields that pick its restart owner (Shapes):
\*   pathless  herdr_row_end.rs:53-72 target() holds
\*   path      output_path set, no kind/receipt/hand-off/rebind/replay (owner.rs:127-141 false)
\*   kind      tui_terminal_kind set (herdr_admitted_restart.rs:23-45)
\*   receipt   herdr_before_start_stop or receipt file (herdr_before_start_restart.rs:9-24)
\*   replay    replay_rerun_blocked (model/identity.rs:107; restore_inflight.rs:165-168)
\* boot: restore_inflight.rs:120-229 one pass; committed: the row's own notice ack
\* (terminal_delivery_committed / receipt notice_pending false / replay debt acknowledged);
\* notices: terminal notices shown (capped at 2); gone: "settled" by an owner, "dropped" by age.
\* reader: a wrapper-restored reader of this row's episode, "live" while its watcher handle is
\* live (relay_state.rs:476-483), "mapping" once only the channel mapping is left (:544-559).
\* inq: an owner input that arrived after the restart; "asleep" is Wait::Event, which returns
\* before the claim (owner_input.rs:978-980) and no timer re-reads (presence wakes an owned
\* queue only on its Idle transition, turn_presence/supervisor.rs:681-700).
\* Current: hold owner.rs:110-123 (every shape), end pass target() only, unpinned on a new nonce.
\* Target: one owner predicate; the end pass picks every held shape, settles it, then wakes.
\* Flaws turn one Target repair off each: split (pass filter stays target()), redeliver (re-run
\* ignores committed), stale_drop (stale-age drops Herdr rows), path_refused (Restored clear keeps
\* the path-less refusal, clear_store/herdr_row_end.rs:43), nowake (no closed() after a re-run
\* settle), replay_keep (Replay only logs a hold), mapping_gate (the reader gate reads the channel
\* mapping), pin_strict (a newer execution nonce reads as unpinned, herdr_row_end.rs:541-549).
AllShapes == {"pathless", "path", "kind", "receipt", "replay"}
Cur == Owner = "current"
Fl(f) == ~Cur /\ f \in Flaws
PickSet == IF Cur \/ Fl("split") THEN {"pathless"} ELSE AllShapes
Held == On /\ inf = "present" /\ igen = "prior" /\ shape \in AllShapes
Show(c) == IF c THEN notices ELSE notices + 1
Bump == IF notices < 2 THEN notices + 1 ELSE 2
\* The row's own execution is gone: Retired, or a newer nonce was installed over it
\* (install_pending_pg only over a retired older nonce, hosted_execution.rs:417-425).
ExecGone == exec = "retired" \/ (exec = "pending" /\ ~Cur /\ ~Fl("pin_strict"))
ReaderHolds == IF Fl("mapping_gate") THEN reader # "none" ELSE reader = "live"

\* herdr_terminate.rs settle_and_release -> settle_captured: the HostTerminated finalize either
\* clears the row with its terminal or its actor CAS refuses and the row stays (#8435 (b)).
TermSettle ==
    /\ On /\ phase = "settle" /\ phase' = "done"
    /\ \/ inf' = "none" /\ gone' = "settled" /\ notices' = notices + 1
       \/ UNCHANGED <<inf, gone, notices>>
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres,
                   readerVars, igen, shape, boot, committed, reader, inq>>

\* restore_inflight.rs: a pathless row skips (held_at_restore, :215); kind/receipt run their one
\* settle (:143,:209,:224) and may keep the row "for the next boot"; replay delivers its debt and
\* always keeps the row (:165-168, replay_hold.rs:113-175); a path row takes the wrapper restore,
\* which may attach a reader and may leave the row (carried_turn_close.rs:197-201).
BootRestore ==
    /\ On /\ boot = "pending" /\ boot' = "done"
    /\ CASE shape = "pathless" -> UNCHANGED <<inf, committed, notices, gone, reader>>
         [] shape = "path" ->
              \/ inf' = "none" /\ gone' = "settled" /\ notices' = notices + 1
                 /\ UNCHANGED <<committed, reader>>
              \/ reader' \in {"none", "live"} /\ UNCHANGED <<inf, committed, notices, gone>>
         [] shape = "replay" ->
              \/ UNCHANGED <<inf, committed, notices, gone, reader>>
              \/ committed' = TRUE /\ notices' = Show(committed)
                 /\ UNCHANGED <<inf, gone, reader>>
         [] OTHER ->
              \/ UNCHANGED <<inf, committed, notices, gone, reader>>
              \/ committed' = TRUE /\ notices' = Show(committed)
                 /\ UNCHANGED <<inf, gone, reader>>
              \/ inf' = "none" /\ gone' = "settled" /\ committed' = TRUE
                 /\ notices' = Show(committed) /\ UNCHANGED reader
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
                   readerVars, igen, shape, inq>>

\* The restored reader's watcher stops or goes stale; the channel mapping it left stays.
ReaderExit ==
    /\ On /\ reader = "live" /\ reader' = "mapping"
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
                   readerVars, inf, igen, shape, boot, committed, notices, gone, inq>>

\* A new incarnation's Pending record over the retired nonce (/restart, clear recovery).
Reincarnate ==
    /\ On /\ igen = "prior" /\ exec = "retired" /\ exec' = "pending"
    /\ UNCHANGED <<row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
                   readerVars, ownVars>>

\* An owner input after the restart: a held channel puts it to sleep (Wait::Event).
InputArrive ==
    /\ On /\ igen = "prior" /\ boot = "done" /\ inq = "none"
    /\ inq' = IF Held THEN "asleep" ELSE "run"
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
                   readerVars, inf, igen, shape, boot, committed, notices, gone, reader>>

\* queue_io/input_owner/closed.rs:22-34 kickoff: the woken owner reads the front again.
OwnerReeval ==
    /\ On /\ inq = "woken" /\ inq' = IF Held THEN "asleep" ELSE "run"
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
                   readerVars, inf, igen, shape, boot, committed, notices, gone, reader>>

\* One 5 s pass over the row (herdr_row_end.rs:112-181, after reconcile_done). Its result follows
\* from the state: a picked row whose own execution is gone and no live reader of its episode
\* holds is settled (notice once, clear, then closed() wakes the owner); a Restored clear that
\* keeps the path-less refusal shows its notice and is refused; anything else changes nothing.
EndTick ==
    /\ On /\ boot = "done" /\ inf = "present" /\ igen = "prior"
    /\ shape \in PickSet /\ ExecGone /\ ~(shape = "path" /\ ReaderHolds)
    /\ ~(shape = "replay" /\ Fl("replay_keep"))
    /\ IF shape = "path" /\ Fl("path_refused")
       THEN notices' = Bump /\ UNCHANGED <<inf, gone, committed, inq>>
       ELSE /\ inf' = "none" /\ gone' = "settled" /\ committed' = TRUE
            /\ notices' = IF Fl("redeliver") THEN Bump ELSE Show(committed)
            /\ inq' = IF inq = "asleep" /\ ~(Fl("nowake") /\ shape # "pathless")
                      THEN "woken" ELSE inq
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
                   readerVars, igen, shape, boot, reader>>

\* inflight/removal.rs:417-509 with the Herdr preservation removed: age alone drops the row,
\* whatever the execution or its terminal.
StaleDrop ==
    /\ Fl("stale_drop") /\ inf = "present" /\ inf' = "none" /\ gone' = "dropped"
    /\ UNCHANGED <<exec, row, nonce, fresh, bridge, ended, refused, turns, restarts, pres, phase,
                   readerVars, igen, shape, boot, committed, notices, reader, inq>>

Next ==
    \/ Retire \/ ForcePostlude \/ LiveEnd \/ Heartbeat \/ BridgeExit \/ BridgeFinal
    \/ StaleTick \/ Sweep \/ Install \/ NewTurn \/ NewTurnEnd \/ Restart \/ PresenceStale
    \/ NextPreflight \/ ToolUse
    \/ TermSettle \/ BootRestore \/ EndTick \/ StaleDrop
    \/ ReaderExit \/ Reincarnate \/ InputArrive \/ OwnerReeval

Fair ==
    /\ WF_vars(Retire) /\ WF_vars(ForcePostlude) /\ WF_vars(LiveEnd) /\ WF_vars(Heartbeat)
    /\ WF_vars(BridgeExit) /\ WF_vars(BridgeFinal) /\ WF_vars(StaleTick) /\ WF_vars(Sweep)
    /\ WF_vars(NewTurnEnd) /\ WF_vars(PresenceStale) /\ WF_vars(Restart) /\ WF_vars(NextPreflight)
    /\ WF_vars(TermSettle) /\ WF_vars(BootRestore) /\ WF_vars(EndTick)
    /\ WF_vars(ReaderExit) /\ WF_vars(OwnerReeval)

Spec == Init /\ [][Next]_vars /\ Fair

\* The retired turn's row, once ended, never reads as an active turn of that turn again.
NoRevival == ~(ended /\ row = "turn_active" /\ nonce = 1)
\* Once the cancel or row end finished, the cancel selection (status = 'turn_active') skips the
\* retired turn's row.
EndedTurnNotSelectable == (phase = "done" /\ nonce = 1) => row # "turn_active"
\* A turn start under a new nonce is never refused.
NewTurnNotRefused == ~refused
\* The reader whose guarded save read its inflight row as gone never writes the session row again.
LostReaderNeverWrites == ~lostWrite
\* The retired turn's reader never overwrites the new incarnation turn's row.
NewTurnRowNotOverwritten == ~oldOverNew

\* A row leaves only after its turn's terminal notice was shown, and that notice shows once.
RowGoneWithTerminal == gone # "no" => notices >= 1
NoticeOnce == notices <= 1

RowSettles == <>[](row \in Ended)
\* A Herdr row whose execution is gone is eventually ended or no longer holds new inputs.
ExeclessRowResolves == (exec # "bound" /\ inf = "present") ~> (inf = "none" \/ ~Held)
\* An owner input that went to sleep behind the row runs once the row no longer holds it.
HeldInputRuns == (inq = "asleep") ~> (inq = "run")
PresenceLeavesBusy == (exec = "retired" /\ nonce = 1) ~> (pres # "busy")
====
