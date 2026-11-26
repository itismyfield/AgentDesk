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

## Restart owner (#8435 S3)

Code SHA: `b04ea56650e5397989d33f1be199be21a83c8d0e`. `Owner # "off"` adds the turn's inflight row
after an HTTP terminate (`Path = "terminate"`), a dcserver restart, and who ends that row. Existing
cfgs set `Owner = "off"`, so the ownership variables stay at their initial values and every
existing rule result is unchanged. Before and NewTurnToolBefore draw a different shortest
counterexample with the same broken rules, so their chart and report were regenerated. Design and
entrypoints: [herdr-restart-row-owner.design.md](herdr-restart-row-owner.design.md).

| Action | Code |
|---|---|
| TermSettle | `herdr_terminate.rs:327-391` `settle_and_release` → `settle_captured`: HostTerminated clears the row, or the actor CAS refuses and the row stays (#8435 (b), refusal cause out of range) |
| Restart (Owner on) | `born_generation` is stamped once (`inflight/model.rs:1068`), so a kept row turns prior-generation (`reconcile_gate.rs:29-34`); the mailbox actor dies with the process |
| BootRestore | `restore_inflight.rs:120-229` one pass: pathless skips (`:215`); kind (`:224`) and receipt (`:143,:209`) run their settle once and may keep the row "for the next boot" (`herdr_admitted_restart.rs:193`, `herdr_before_start_restart.rs:114`); an `output_path` row takes the wrapper restore, whose reader can leave it (`carried_turn_close.rs:197-201`) |
| EndPass | `herdr_row_end.rs:112-181` after `reconcile_done`; on a Retired execution `observe` keeps retire None (`:413`), `commit` releases the hold (`:505-517`), notice, clear. Unclear gates retry every 5 s; the model takes the passing run |
| StaleDrop | design alternative only: `inflight/removal.rs:417-509` without the Herdr preservation |

Shapes (`Shapes`, chosen at init): `pathless` = `target()` holds (`herdr_row_end.rs:53-72`);
`path` = `output_path` set and no kind/receipt/hand-off/rebind/replay (`dedicated_cleanup_owner`
false, `owner.rs:127-141`); `kind` = `tui_terminal_kind`; `receipt` = before-start stop or receipt.
Held = prior-generation row of any shape (`owner.rs:110-123`). Current picks only `pathless`;
Target picks every held shape and runs that shape's settle again.

Rules: `ExeclessRowResolves` (PROPERTY) — a row whose execution is Retired is eventually gone or no
longer held. `RowGoneWithTerminal` — a row leaves only after its terminal notice was shown.
`NoticeOnce` — that notice shows once.

| cfg | Breaks | Holds |
|---|---|---|
| OwnerCurrent | ExeclessRowResolves (path, kind, receipt) | RowGoneWithTerminal, NoticeOnce |
| OwnerTarget | — | all |
| OwnerMutantSplit (Target, `target()` kept as the pass filter) | ExeclessRowResolves | RowGoneWithTerminal, NoticeOnce |
| OwnerRedeliver (Target re-run ignores the committed ack) | NoticeOnce | RowGoneWithTerminal, ExeclessRowResolves |
| OwnerStaleDrop (Target + stale-age drops Herdr rows) | RowGoneWithTerminal | NoticeOnce, ExeclessRowResolves |

Scratch runs with one shape each (not committed): Current and MutantSplit break only for `path`,
`kind` and `receipt`; `pathless` holds; Target holds for all four.

The OwnerCurrent counterexample, replayed against the code: retire (`herdr_terminate.rs:623`); the
settle is refused and the row stays (`:395-411` "captured turn still active"); restart makes it
prior-generation; boot restore leaves an `output_path` row (`target()` false at `:63`,
`admitted()` needs a kind); from then on `restarted_owner` holds inputs, `row_end_pass` skips it
(`:122`), `request_row_end` returns without a log (`:161`), and stale-age keeps it
(`removal.rs:417-421`). The kind/receipt variant keeps the row at boot, then the same three
consumers apply with `dedicated_cleanup_owner` true. The case log's 14:48:35 "dedicated owner" line
with no row end diagnostics matches that variant; which X field the row carried is not confirmed.

Out of range: the CAS refusal cause (#8435 (b)), the live row end notice repeat (#8435 (c)),
HotSwap/rebind/replay rows, and current-generation rows before a restart.
