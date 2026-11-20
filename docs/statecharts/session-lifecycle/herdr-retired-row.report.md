# HerdrRetiredRow — sessions row status after a Herdr execution is retired under a turn

Code SHA: `8caf0e5d8c80b7ebc29a62111b36b4fac7decf03` (Before). `Fence`/`Cleanup` model the r4 fix
(`retired_status_fence.rs`, the live row end disconnect); `ReaderStops` models the r5 fix based on
`62a3523d8910bca77b92e1e42100a9a57a671b33` (`turn_bridge/session_row_writer.rs`): a reader whose owned
inflight row is gone or held by a successor stops its heartbeat and post-loop `turn_active` writes.
`FlushObserves`/`ToolGuard` model r5 fix 1: the tick's dirty flush also releases the writer, and a
released reader's late tool frame records no tool mark or background child. `seen` (a guarded save
read the owned row as gone) is split from `lost` (the writer released); the rule checks `seen`.

## Actions and code
| Action | Code |
|---|---|
| Retire | `herdr_terminate.rs` `retire_confirmed` via `queue.rs` force cancel or `herdr_row_end_live.rs` |
| ForcePostlude | `cancel_effects.rs` `herdr_force_steps` → `hosted_execution/http_force.rs` `disconnect_retired_pg` |
| LiveEnd | `herdr_row_end_live.rs` `end_live`: (After) `disconnect_ended`, then `settle_captured`, then cancel the reader. The model takes these as one step and does not check their order; the live end PG test asserts the row is `disconnected` while the captured token is still held just before settlement |
| Heartbeat | `turn_bridge/stream_tick.rs` 30 s `working` hook; a force cancel settles the reader mutely (`turn_finalizer/finalize.rs` keeps the token uncancelled), so it keeps posting until `ReaderStops` ends it once `lost` |
| NextPreflight / ToolUse | the next tick's preflight releases a writer the dirty flush did not; `stream_loop/tool_arms.rs` tool-use mark and background child registration (`session_activity.rs`) |
| (lost) | the force cancel clears the reader's inflight row; a live row end settles and clears it; the reader's guarded save then reads `RowAbsent`/`SuccessorOwned` (`stream_tick/runtime.rs`) |
| BridgeExit / BridgeFinal | `post_loop_finalize.rs` `TURN_ACTIVE`, `completion_postlude.rs` terminal `IDLE` (may be skipped) |
| StaleTick / Sweep | `stale_turn_reconciler.rs` candidate (no dispatch, heartbeat older than `STALE_TURN_GRACE`) → `idle` |
| Install / NewTurn / NewTurnEnd | `hosted_execution.rs` `install_pending_pg`; `adk_session.rs` / `session_locator.rs` turn start with a new nonce |
| Restart | the reader dies with the process; the removed inflight row is not restored |
| PresenceStale | `turn_presence/activity.rs` open transcript quiet for `STALE_OPEN_AFTER` → `unknown` |

## Results (statechart, plain TLC)
| cfg | Breaks | Holds |
|---|---|---|
| Before (force) | LostReaderNeverWrites, NoRevival, EndedTurnNotSelectable, RowSettles | NewTurnNotRefused, NewTurnRowNotOverwritten, PresenceLeavesBusy |
| After (force, r4+r5) | — | all |
| LiveEndBefore | LostReaderNeverWrites, NoRevival, EndedTurnNotSelectable | NewTurnNotRefused, NewTurnRowNotOverwritten, RowSettles (stale sweep), PresenceLeavesBusy |
| LiveEndAfter | — | all |
| NewTurnBefore (r4 only + one new turn) | LostReaderNeverWrites, NoRevival, EndedTurnNotSelectable, NewTurnRowNotOverwritten, RowSettles | NewTurnNotRefused, PresenceLeavesBusy |
| NewTurn (r4+r5 + one new turn) | — | all |
| NoNonce (fence without the new-nonce exemption, no reader stop) | LostReaderNeverWrites, NewTurnNotRefused, NoRevival, EndedTurnNotSelectable, NewTurnRowNotOverwritten, RowSettles | PresenceLeavesBusy |
| NewTurnFlushBefore (NewTurn without FlushObserves) | LostReaderNeverWrites, NoRevival, EndedTurnNotSelectable, NewTurnRowNotOverwritten | NewTurnNotRefused, RowSettles, PresenceLeavesBusy |
| NewTurnToolBefore (NewTurn without ToolGuard) | LostReaderNeverWrites, NewTurnRowNotOverwritten, RowSettles | NoRevival, EndedTurnNotSelectable, NewTurnNotRefused, PresenceLeavesBusy |
| Restart (Before + one restart) | LostReaderNeverWrites, NoRevival, EndedTurnNotSelectable | RowSettles, PresenceLeavesBusy, NewTurnNotRefused, NewTurnRowNotOverwritten |

The Before counterexample is the observed order: retire, force postlude `disconnected`, the reader's
next `working` hook writes `turn_active`, and its heartbeat keeps the row fresh so the stale sweep
never qualifies it.

NewTurnBefore keeps the gap r4 left: once a new execution is installed the fence no longer
applies, and the force-cancelled reader's heartbeat marks the new turn's row active. With the
reader stop (NewTurn) the lost reader writes nothing, so the new turn's row is its own.
Presence is computed from the transcript, not the row, and leaves busy in every cfg.

The eight earlier cfgs set `FlushObserves` and `ToolGuard` TRUE so they keep checking what they
checked before; the two new Before cfgs each drop one of them. The P2 that post-loop removal is not
caught by these cfgs is not addressed here.
