# Superseded carried row and owner observer rebind (#7896 B)

Code SHA: `b5e0f2115f` plus this repair. Models: `InputOwnershipConsumedWaiting.tla` (extended) and `ManagedObserverRebind.tla` (new).

## What the code does

- `health/recovery/carried_turn_close.rs`: an earlier generation's managed row that was not readopted and has no verified native pin used to be kept forever (`unverified_carried_input`). It now closes when its `session_id` names a session other than the one the binding names for the source the pane ended on (`ClosedBoundary.session`; an empty binding session keeps the row), after the same native end, O drain/capture, lock and receipt proof as every other carried close. Before archiving it enqueues one outbox notice keyed by the turn (`queue_io/retained_notice.rs`), so a repeated close posts once. A failed receipt commit on this close leaves no settlement mark, so no generic settlement can remove the row without the notice. A row naming the current session is still kept (#7828).
- `claude_tui/hosting/warm_followup.rs`: `owner_observer` resolves its session before the submit. After the submit it now polls for up to 10 s: this submit's own receipt pin in that session reads it as before, and the pane's verified session (hook adoption + runtime binding) changing reads that one from the submit's time. Growth of the old file proves nothing, because `/clear` appends its cost-state there. When the grace expires the old session is read, and a session adopted later still takes the read over once (see "Late adoption after the grace" below).

## Entry points the models take their values from

| Variable | Writer / transition | file:line (base) |
|---|---|---|
| `row = "carried"` | busy paste row; shutdown restamp | `provider_isolation.rs:153-166`, inflight shutdown restamp |
| `readopt` | restore readoption, O-only marker consumption | `recovery_engine/restore_inflight.rs:1922-1937` |
| `pinVerified` | busy observer pin only | `warm_followup.rs` `busy_owner_observer`, `inflight/native_stop_pin.rs:463` |
| `aSession` | row `session_id` vs `ClosedBoundary.session` (binding source session) | `carried_turn_close.rs` `session_superseded` |
| `seed`, `consumed` | busy paste or idle managed start (consumption unknown) | `provider_isolation.rs:153-166`, `inflight/managed_submission.rs:59-84` |
| `failures`, `marked`, `aGone` | receipt commit failure mark, generic settlement removal, archive | `carried_turn_close.rs` `commit_receipt`, `receipt_settle.rs:147-176` (`settle_marked`, `settle_marked_row`, `settle_boot`) |
| `close`, `told` | carried close, outbox dedupe | `carried_turn_close.rs` `try_close`, `retained_notice.rs` `carried_unverified_closed` |
| `adopt` | hook adoption after the transcript exists | `hook_server/adoption_retry.rs`, `tui_prompt_dedupe::record_hook_adopted_claude_session_id` |
| `observer` (`pollingA`/`fixedA`/`fixedB`) | `owner_observer` build, `rebound_source` poll/grace | `warm_followup.rs` |
| `aMeta` | `/clear` cost-state appended to the old transcript | `tests/fixtures/hook_payload/claude-2.1.316.transcript-prefix.jsonl` case `suffix_after_cli_clear` |
| `receiptA` | the writer's receipt pin for this submit | `provider/native_stop.rs:116-140`, [writer receipt scope (5bcbb82297 기준) L333–L345](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L333-L345) |

## Results (plain TLC 1.7.4, `scripts/ci/statecharts_tlc.py --base origin/main`)

| cfg | before this change | after |
|---|---|---|
| ConsumedWaitingAfter / AfterStamped / InvalidPin / NonO | holds | holds (unchanged; `RepairSupersededClose = FALSE`, `ASessions = {"same"}`, `ASeeds = {"busy"}`, `ReceiptFailures = 0`) |
| ConsumedWaitingBefore / PinSupplyBefore | `PendingBEventuallyStarts` breaks | same |
| ConsumedWaitingOutcomeBefore | `OrphanAReceiptCompleted` breaks | same |
| ConsumedWaitingSupersededBefore (new, repair off, old session, no pin) | — | `PendingBEventuallyStarts` breaks (defect D) |
| ConsumedWaitingSupersededAfter (new) | — | holds, incl. `UnprovenCloseToldOnce` |
| ConsumedWaitingSupersededSameSession (new) | — | `PendingBEventuallyStarts` breaks (#7828 gap, kept on purpose) |
| ConsumedWaitingSupersededNoDedupe (new) | — | `UnprovenCloseToldOnce` breaks (a lost archive repeats the close) |
| ConsumedWaitingSupersededIdleBefore (new, idle seed, repair off) | — | `PendingBEventuallyStarts` breaks |
| ConsumedWaitingSupersededIdle (new, idle seed, one receipt failure) | — | holds, incl. `UnprovenRemovalTold` |
| ConsumedWaitingSupersededIdleSameSession (new) | — | `PendingBEventuallyStarts` breaks (#7828 gap) |
| ConsumedWaitingSupersededIdleMarkUnproven (new, Mut) | — | `UnprovenRemovalTold` breaks (settlement removes the row without the notice) |
| ManagedObserverRebindBefore (no rebind) | `SubmittedEventuallyCloses` breaks | same |
| ManagedObserverRebindAfter | holds (r1 model) | holds with polling, metadata growth and grace |
| ManagedObserverRebindMetaIsInput (new, Mut = r1 head behaviour) | — | `FixedAOnlyOnEvidenceOrGrace` and `SubmittedEventuallyCloses` break |
| ManagedObserverRebindSinglePoll (new, Mut) | — | `FixedAOnlyOnEvidenceOrGrace` and `SubmittedEventuallyCloses` break |
| ManagedObserverRebindGraceBeforeAdoption (new) | — | `SubmittedEventuallyCloses` breaks: liveness holds only while the grace outlasts the adoption |

## Late adoption after the grace (#7889)

Code SHA: `31389fb471` plus this repair. `RepairLate` adds `LateHandoff`: once the grace has expired and the observer reads A, a verified adoption of B while A holds no receipt of this submit moves the reader to B once (`left`), read from the submit's time. In code the A reader's liveness probe asks `late_rebind` (`warm_followup.rs`), the read ends as a death would, and `observe_submitted` reads B only when A relayed nothing; a receipt seen in A keeps the read there for good.

| cfg | before | after |
|---|---|---|
| ManagedObserverRebindGraceBeforeAdoption (`RepairLate = TRUE`) | `SubmittedEventuallyCloses` breaks | holds |
| ManagedObserverRebindGraceBeforeAdoptionNoHandoff (new, `RepairLate = FALSE`) | — | `SubmittedEventuallyCloses` breaks (the old gap) |
| Before / MetaIsInput / SinglePoll (`RepairLate = FALSE`, so the poll mutants keep their meaning) | unchanged | unchanged |
| After (`RepairLate = TRUE`) | holds | holds |

The receipt guard is not exercised: in this model the input reaches A only without a `/clear`, and then B is never adopted. It is covered by the code's receipt test instead.

Liveness is a PROPERTY with weak fairness on every progress action; `Tail` and `HealthKeepsA` are the stutter steps that make a permanent wait a lasso.

## Out of the models

- The turn-presence admission `Rejected(Superseded)` is an observation fence (lock contention, watcher installed) and proves nothing about a row; it is not a model input.
- The watcher O-evidence backstop is not implemented (an anchorless row's nonce is self-authenticating), so it is not modelled.
- Codex owner observer and the busy observer: unchanged; `/clear` mid-busy does not reach them.
- Herdr panes and output locks: unchanged paths.
