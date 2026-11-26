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
cfgs set `Owner = "off"` and `Flaws = {}`, so the ownership variables stay at their initial values
and every existing rule result is unchanged; Before and NoNonce draw a different shortest
counterexample with the same broken rules, so their charts were regenerated. Design and
entrypoints: [herdr-restart-row-owner.design.md](herdr-restart-row-owner.design.md).
w2 adds the review's P1 paths: the Restored clear refusal, the owner wake, Replay, plus a reader
that leaves only its channel mapping and a newer incarnation over the row's execution nonce.

| Action | Code |
|---|---|
| TermSettle | `herdr_terminate.rs:327-391` `settle_and_release` → `settle_captured`: HostTerminated clears the row, or the actor CAS refuses and the row stays (#8435 (b), refusal cause out of range) |
| Restart (Owner on) | `born_generation` is stamped once (`inflight/model.rs:1068`), so a kept row turns prior-generation (`reconcile_gate.rs:29-34`); the mailbox actor dies with the process |
| BootRestore | `restore_inflight.rs:120-229` one pass: pathless skips (`:215`); kind (`:224`) and receipt (`:143,:209`) run their settle once and may keep the row "for the next boot" (`herdr_admitted_restart.rs:193`, `herdr_before_start_restart.rs:114`); replay delivers its debt and keeps the row (`:165-168`, `replay_hold.rs:114-175`); an `output_path` row takes the wrapper restore, which may attach a reader and may leave the row (`carried_turn_close.rs:197-201`) |
| ReaderExit | the restored watcher stops or goes stale (`relay_state.rs:476-483`) while its channel mapping stays (`:544-559`) |
| Reincarnate | a newer Pending record over the retired nonce (`hosted_execution.rs:417-425`) |
| InputArrive / OwnerReeval | an owner input after the restart sleeps on `Wait::Event` (`owner_input.rs:978-980`); presence wakes an owned queue only on its Idle transition (`turn_presence/supervisor.rs:681-700`); `closed()` (`queue_io/input_owner/closed.rs:22-34`) makes it read the front again |
| EndTick | one 5 s pass (`herdr_row_end.rs:112-181`, after `reconcile_done`), fair as a tick; its result follows from the state: a picked row whose execution is gone and that no live reader of its episode holds is settled once, cleared, and the owner is woken |
| StaleDrop | design alternative only: `inflight/removal.rs:417-509` without the Herdr preservation |

Shapes (`Shapes`, chosen at init): `pathless` = `target()` holds (`herdr_row_end.rs:53-72`);
`path` = `output_path` set and no kind/receipt/hand-off/rebind/replay; `kind` = `tui_terminal_kind`;
`receipt` = before-start stop or receipt; `replay` = `replay_rerun_blocked`. Held = prior-generation
row of any shape (`owner.rs:110-123`). Current picks only `pathless` and reads a newer nonce as
unpinned. Target picks every held shape and runs that shape's settle again, treats a superseded
nonce as an ended execution, gates Restored on a live same-episode reader, clears Restored with a
shape-checked clear, archives Replay after its debt, and wakes the owner after every settle.
`Flaws` turns one of those repairs off per cfg.

Rules: `ExeclessRowResolves` (PROPERTY) — a row whose execution is not Bound is eventually gone or
no longer held. `HeldInputRuns` (PROPERTY) — an owner input asleep behind the row eventually runs.
`RowGoneWithTerminal` — a row leaves only after its terminal notice was shown. `NoticeOnce` — that
notice shows once.

| cfg | Flaws | Breaks | Holds |
|---|---|---|---|
| OwnerCurrent | (current code) | ExeclessRowResolves, HeldInputRuns | RowGoneWithTerminal, NoticeOnce |
| OwnerW1 | path_refused, nowake, replay_keep, mapping_gate, pin_strict (the w1 design) | NoticeOnce, ExeclessRowResolves, HeldInputRuns | RowGoneWithTerminal |
| OwnerTarget | — | — | all (695 states, over the chart budget: report only) |
| OwnerPathRefused (P1-1) | path_refused | NoticeOnce, ExeclessRowResolves, HeldInputRuns | RowGoneWithTerminal |
| OwnerNoWake (P1-2) | nowake | HeldInputRuns | RowGoneWithTerminal, NoticeOnce, ExeclessRowResolves |
| OwnerReplayKeep (P1-3) | replay_keep | ExeclessRowResolves, HeldInputRuns | RowGoneWithTerminal, NoticeOnce |
| OwnerMappingGate (P2) | mapping_gate | ExeclessRowResolves, HeldInputRuns | RowGoneWithTerminal, NoticeOnce |
| OwnerPinStrict (P2) | pin_strict | ExeclessRowResolves, HeldInputRuns | RowGoneWithTerminal, NoticeOnce |
| OwnerMutantSplit | split | ExeclessRowResolves, HeldInputRuns | RowGoneWithTerminal, NoticeOnce |
| OwnerRedeliver | redeliver | NoticeOnce | RowGoneWithTerminal, ExeclessRowResolves, HeldInputRuns |
| OwnerStaleDrop | stale_drop | RowGoneWithTerminal, HeldInputRuns | NoticeOnce, ExeclessRowResolves |

Counterexamples, replayed against the code:

- OwnerCurrent: retire (`herdr_terminate.rs:623`); the settle is refused and the row stays
  (`:395-411`); restart makes it prior-generation; boot restore leaves it; from then on
  `restarted_owner` holds inputs, `row_end_pass` skips a non-pathless row (`:122`),
  `request_row_end` returns without a log (`:161`), and stale-age keeps it (`removal.rs:417-421`).
- OwnerPathRefused: an `output_path` row, Retired, no reader; each pass shows the notice
  (`herdr_row_end.rs:270-276`) and the clear refuses on `clear_store/herdr_row_end.rs:43`
  (`:286-291` keeps the row), so the next pass shows it again.
- OwnerNoWake: the row is cleared but the owner input asleep on `Wait::Event` is never read again.
- OwnerReplayKeep: the replay row stays held after its boot debt (`restore_inflight.rs:165-168`).
- OwnerMappingGate: the restored reader stops, its channel mapping stays, and a gate on
  `channel_binding` holds forever.
- OwnerPinStrict: a newer incarnation replaces the row's execution nonce and `hosted_record`
  answers `execution_unpinned` (`herdr_row_end.rs:541-549`) on every pass.

The case log's 14:48:35 "dedicated owner" line with no row end diagnostics matches the
kind/receipt/replay variant; which field the row carried is not confirmed.

Out of range: the CAS refusal cause (#8435 (b)), the live row end notice repeat (#8435 (c)),
HotSwap/rebind rows, current-generation rows before a restart, and a notice dedupe that would have
to survive a process (design [U5]).
