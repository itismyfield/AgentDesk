# Superseded carried row and owner observer rebind (#7896 B)

Code SHA: `b5e0f2115f` plus this repair. Models: `InputOwnershipConsumedWaiting.tla` (extended) and `ManagedObserverRebind.tla` (new).

## What the code does

- `health/recovery/carried_turn_close.rs`: an earlier generation's managed row that was not readopted and has no verified native pin used to be kept forever (`unverified_carried_input`). It now closes when its `session_id` names a session other than the transcript the pane ended on (`ClosedBoundary.source`), after the same native end, O drain/capture, lock and receipt proof as every other carried close. Before archiving it enqueues one outbox notice keyed by the turn (`queue_io/retained_notice.rs`), so a repeated close posts once. A row naming the current session is still kept (#7828).
- `claude_tui/hosting/warm_followup.rs`: `owner_observer` resolves its session before the submit. After the submit it now waits up to 10 s for either its own transcript to grow (read it as before) or the pane's verified session (hook adoption + runtime binding) to change (read that one from the submit's time).

## Entry points the models take their values from

| Variable | Writer / transition | file:line (base) |
|---|---|---|
| `row = "carried"` | busy paste row; shutdown restamp | `provider_isolation.rs:153-166`, inflight shutdown restamp |
| `readopt` | restore readoption, O-only marker consumption | `recovery_engine/restore_inflight.rs:1922-1937` |
| `pinVerified` | busy observer pin only | `warm_followup.rs` `busy_owner_observer`, `inflight/native_stop_pin.rs:463` |
| `aSession` | row `session_id` vs `ClosedBoundary.source` | `carried_turn_close.rs` `session_superseded` |
| `close`, `told` | carried close, outbox dedupe | `carried_turn_close.rs` `try_close`, `retained_notice.rs` `carried_unverified_closed` |
| `adopt` | hook adoption after the transcript exists | `hook_server/adoption_retry.rs`, `tui_prompt_dedupe::record_hook_adopted_claude_session_id` |
| `observer` | `owner_observer` build, `rebound_source` | `warm_followup.rs` |

## Results (plain TLC 1.7.4, `scripts/ci/statecharts_tlc.py --base origin/main`)

| cfg | before this change | after |
|---|---|---|
| ConsumedWaitingAfter / AfterStamped / InvalidPin / NonO | holds | holds (unchanged, `RepairSupersededClose = FALSE`, `ASessions = {"same"}`) |
| ConsumedWaitingBefore / PinSupplyBefore | `PendingBEventuallyStarts` breaks | same |
| ConsumedWaitingOutcomeBefore | `OrphanAReceiptCompleted` breaks | same |
| ConsumedWaitingSupersededBefore (new, repair off, old session, no pin) | — | `PendingBEventuallyStarts` breaks (defect D) |
| ConsumedWaitingSupersededAfter (new) | — | holds, incl. `UnprovenCloseToldOnce` |
| ConsumedWaitingSupersededSameSession (new) | — | `PendingBEventuallyStarts` breaks (#7828 gap, kept on purpose) |
| ConsumedWaitingSupersededNoDedupe (new) | — | `UnprovenCloseToldOnce` breaks (a lost archive repeats the close) |
| ManagedObserverRebindBefore (new) | — | `SubmittedEventuallyCloses` breaks |
| ManagedObserverRebindAfter (new) | — | holds |

Liveness is a PROPERTY with weak fairness on every progress action; `Tail` and `HealthKeepsA` are the stutter steps that make a permanent wait a lasso.

## Out of the models

- The turn-presence admission `Rejected(Superseded)` is an observation fence (lock contention, watcher installed) and proves nothing about a row; it is not a model input.
- The watcher O-evidence backstop is not implemented (an anchorless row's nonce is self-authenticating), so it is not modelled.
- Codex owner observer and the busy observer: unchanged; `/clear` mid-busy does not reach them.
- Herdr panes and output locks: unchanged paths.
