# HerdrRetiredRow — sessions row status after a Herdr execution is retired under a turn

Code SHA: `8caf0e5d8c80b7ebc29a62111b36b4fac7decf03` (Before); After adds `retired_status_fence.rs`
(hook update fence) and the live row end disconnect in `herdr_row_end_live.rs`.

## Actions and code
| Action | Code |
|---|---|
| Retire | `herdr_terminate.rs` `retire_confirmed` via `queue.rs` force cancel or `herdr_row_end_live.rs` |
| ForcePostlude | `cancel_effects.rs` `herdr_force_steps` → `hosted_execution/http_force.rs` `disconnect_retired_pg` |
| LiveEnd | `herdr_row_end_live.rs` `end_live`: settle, then (After) `disconnect_ended`, then cancel the reader |
| Heartbeat | `turn_bridge/stream_tick.rs` 30 s `working` hook; a force cancel settles the reader mutely (`turn_finalizer/finalize.rs` keeps the token uncancelled), so it keeps posting |
| BridgeExit / BridgeFinal | `post_loop_finalize.rs` `TURN_ACTIVE`, `completion_postlude.rs` terminal `IDLE` (may be skipped) |
| StaleTick / Sweep | `stale_turn_reconciler.rs` candidate (no dispatch, heartbeat older than `STALE_TURN_GRACE`) → `idle` |
| Install / NewTurn / NewTurnEnd | `hosted_execution.rs` `install_pending_pg`; `adk_session.rs` / `session_locator.rs` turn start with a new nonce |
| Restart | the reader dies with the process; the removed inflight row is not restored |
| PresenceStale | `turn_presence/activity.rs` open transcript quiet for `STALE_OPEN_AFTER` → `unknown` |

## Results (statechart, plain TLC)
| cfg | Breaks | Holds |
|---|---|---|
| Before (force) | NoRevival, EndedTurnNotSelectable, RowSettles | NewTurnNotRefused, PresenceLeavesBusy |
| After (force) | — | all |
| LiveEndBefore | NoRevival, EndedTurnNotSelectable | NewTurnNotRefused, RowSettles (stale sweep), PresenceLeavesBusy |
| LiveEndAfter | — | all |
| NewTurn (After + one new turn) | NoRevival, EndedTurnNotSelectable, RowSettles | NewTurnNotRefused, PresenceLeavesBusy |
| NoNonce (fence without the new-nonce exemption) | NewTurnNotRefused, NoRevival, EndedTurnNotSelectable, RowSettles | PresenceLeavesBusy |
| Restart (Before + one restart) | NoRevival, EndedTurnNotSelectable | RowSettles, PresenceLeavesBusy |

The Before counterexample is the observed order: retire, force postlude `disconnected`, the reader's
next `working` hook writes `turn_active`, and its heartbeat keeps the row fresh so the stale sweep
never qualifies it.

NewTurn is a known gap left open: once a new execution is installed the fence no longer applies, so
the force-cancelled reader's heartbeat can still mark the row active. The fence cannot tell that
heartbeat from the new turn's; stopping it needs the reader itself to stop (follow-up).
Presence is computed from the transcript, not the row, and leaves busy in every cfg.
