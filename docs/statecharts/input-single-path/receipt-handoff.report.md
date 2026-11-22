# Receipt handoff across restart and same-id retry (#8289)

Code base: `e1ef00a08500e1e2b4e6b040bdd0446873e4ff06` (first repair) plus the second repair.

Two small models cover where an origin-carrying execution's turns receipt meets another owner.

## NativeReceiptRestart

A native completion whose receipt commit failed keeps its row marked (`terminal_receipt`). A
graceful shutdown then runs `inflight::mark_restart_mode_blocking`, and the next boot runs
`BootInputs::prepare`, the boot reaper and `BootInputs::restore` before the runtime starts.

| Config | Result |
|---|---|
| `NativeReceiptRestartBefore` | RED as expected: `NoBootHeld` (the marked row gets DrainRestart, so the native CAS refuses it after PG returns; a boot while PG is still down fails on the pending receipt) and `ReceiptEventuallySettled` |
| `NativeReceiptRestartFixed` | GREEN: restart marking skips a row awaiting its receipt (`inflight.rs::set_inflight_restart_mode_under_lock`), and a pending receipt at boot leaves the input unsettled for the writer's retry instead of failing restore (`native_restore.rs::RestoredInput::drive`) |

## WatcherRetryReceipt

A watcher stale-resume schedules a retry with the original message id, and the watcher-death
cleanup (`tmux_restart_handoff.rs::start_restart_handoff_from_state`) removes the dead row.
Receipts keep the first terminal and merge origins (`db/turns.rs::record_turn_receipt`).

| Config | Result |
|---|---|
| `WatcherRetryReceiptBefore` | RED as expected: `NoFailedAfterRetrySuccess` (cleanup commits failed before the retry runs) |
| `WatcherRetryReceiptFixed` | GREEN: the stale-resume branch carries the origins onto the execution row first (`pre_emit_guard.rs`), cleanup leaves a carried execution's terminal to the retry, and a refused retry fails it once (`recovery_text.rs::auto_retry_with_history`) |
| `WatcherRetryReceiptFixedCarryLost` | RED, known gap: when the carry write itself fails, cleanup cannot see the retry owner and commits failed first |

Rules: `NoBootHeld`, `NoRowLossWithoutReceipt`, `ReceiptEventuallySettled`,
`NoFailedAfterRetrySuccess`, `AtMostOneCard`, `CardEventuallyPublished`.
