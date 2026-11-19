# Native completion records

`NativeCompletionRecord.tla` models the recording step between a durable native result and the existing row cleanup. The live boundary is `turn_finalizer/actor_state.rs::submit_native`. Boot reaches the same recorder from `turn_finalizer/native_restore.rs::BootInputs::restore` and `RestoredInput::settle_saved`. The recording step ends after a successful write, a skipped row, or a failed read or write.

The table covers the completion entry points audited for this slice. `D/` denotes `src/services/discord/`, `TB/` denotes its `turn_bridge/` directory, and `TF/` denotes its `turn_finalizer/` directory.

| Entry | Bridge or watcher coordinate | Native disposition and coordinate |
|---|---|---|
| P1 | `TB/mod.rs:688` before-start stop | Existing control intent in `TF/native_policy.rs` and `TF/terminal_handler.rs:266`. |
| F1 | `TB/post_loop_finalize.rs:184` placeholder | Bridge display cleanup. O owns native publishing. |
| F2 | `TB/post_loop_finalize.rs:244` child-session abort | Bridge transport error handling. |
| F3 | `TB/post_loop_finalize.rs:263` orphan tool line | Bridge status display. |
| F4, C18 | `TB/post_loop_finalize.rs:281`, `TB/completion_postlude.rs:635` API friction | Follow-up with memory effects. |
| F5 | `TB/post_loop_finalize.rs:341` retry body | Bridge requeue presentation. |
| F6 | `TB/post_loop_finalize.rs:346` review dispatch guard | Follow-up with review dispatch completion. |
| F7 | `TB/post_loop_finalize.rs:409` empty response telemetry | Bridge observation. C15 records a native empty scheduled response. |
| F8, C5 | `TB/post_loop_finalize.rs:450`, `TB/completion_postlude.rs:204` session statuses | S2 session status work. |
| F9, C25 | `TB/post_loop_finalize.rs::finalize_bridge_terminal` queue release | Existing native wake in `TF/terminal_handler.rs::NativeCleanup::drive`. |
| F10, C4, C23 | `TB/post_loop_finalize.rs:673` finalizer and inflight cleanup | Existing native mailbox and row CAS and cleanup acknowledgement in `TF/terminal_handler.rs:680` and `TF/native_restore.rs::RestoredInput::drive`. |
| T1, T14 | `D/outbound/delivery_record.rs` completion ledger | O publishing retains delivery custody. |
| T2, T11, T12 | `D/tmux_watcher.rs:2251` turn-view reactions | S2. `TF/cleanup.rs:425` has a finalizer reaction hook. Native reachability remains open. |
| T3, T4, T5 | `TB/terminal_outcome_delivery.rs` recovery and interruption branches | Native disposition and O notices retain custody. |
| T6 | `TB/terminal_outcome_delivery.rs` delivery and lease | O writer owns native Discord delivery. |
| T7 | `D/tmux_watcher.rs:1445` scheduled outcome mirror | `D/tmux_watcher/commit_decisions.rs:39` excludes native pins before the row mutation. C15 classifies the proven range independently. |
| T8 | `TB/terminal_outcome_delivery.rs` rollover prefix cleanup | Bridge streaming display. |
| T9 | `TB/terminal_outcome_delivery.rs` TUI quiescence and requeue | Existing native closer proof in `TF/native_terminal.rs`. |
| T10 | `TB/completion_guard.rs:550` work dispatch | `TF/native_completion.rs::write` reuses bridge work completion. |
| T13 | `TB/terminal_outcome_delivery/` last-turn time | Follow-up display state. |
| C1, C2 | `TB/completion_postlude.rs` context and footer | O's `tui_o/writer/turn_summary.rs` owns the footer. |
| C3 | `TF/completion_admission.rs` admission release | Existing native custody in `TF/actor_state.rs::handle_native` and `TF/native_restore.rs::restore`. |
| C6 | `TB/completion_postlude.rs:256` isolated session close | S1 keeps the existing session lifecycle. Native holds can include an isolated scheduled row. |
| C7 | `TB/completion_postlude.rs` watcher resume | O retains its native channel. |
| C8 | `TB/completion_postlude/channel_writeback.rs:86` completion scope | `TF/native_completion.rs::write` uses existing scheduled/legacy scope and routine ownership. Per-run presentation determines `channel_context`. |
| C9, C10 | `TB/completion_postlude.rs:342` memory history and session identity | Follow-up session writeback. |
| C11 | `TB/completion_postlude/prelude_lifecycle.rs:166` prelude consumption | S2. A confirmed prelude can be injected again before that work lands. |
| C12, C13 | `TB/completion_postlude.rs:420` feedback and WIP reminders | Follow-up intake context. |
| C14 | `TB/completion_postlude.rs:504` completion telemetry | Follow-up observations. |
| C15 | `TB/completion_postlude.rs:551` scheduled outcome | `TF/native_completion.rs::write` records the exact range, including `EmptyResponse`. `D/tmux_watcher/path_b_record.rs::settle_committed_range` excludes its own native pin. `D/receipt_settle.rs:186` excludes native-owned mirrors from boot settlement. |
| C16 | `TB/completion_postlude.rs:565` transcript | `TF/native_completion.rs::read_run` and `write` persist the non-empty range for the current row. `D/tmux_watcher/path_b_record.rs::persist_transcript` checks custody on its selected row. |
| C17 | `TB/recall_feedback.rs:151` memento stats | S2 with common-parser `ToolResult` translation. |
| C19 | `TB/completion_postlude.rs:672` analytics | S2. |
| C20 | `TB/completion_postlude.rs` memory capture and reflect | Follow-up backend effects. |
| C21, C22 | `TB/completion_postlude.rs` metrics and restart report | Existing bridge observations and restart handling. |
| C24 | `TF/completion_admission.rs` rejected handoff | Existing native actor admission and custody checks. |

## TLC configurations

The following pairs exercise the five before modes against the matching fixed mode. The `.tla` and all `.cfg` files retain their reviewed contents.

| Before suffix | Fixed suffix | Rule violated before |
|---|---|---|
| `Before` | `FixedNormal` | `ScheduledEventuallySettles`, `TranscriptEventuallyRecorded` |
| `CapturedBefore` | `FixedClearBeforeRecord` | `NoResurrectionAfterClear` |
| `EmptyGateBefore` | `FixedEmpty` | `ScheduledEventuallySettles` |
| `ForeignBefore` | `FixedNormal` | `NoForeignBody` |
| `OrderingBefore` | `FixedNormal` | `NoCleanupBeforeRecordStage`, `ScheduledEventuallySettles`, `TranscriptEventuallyRecorded` |

`FixedUnreadable`, `FixedWriteFail`, `FixedCrashAfterRecord`, `FixedCrashAfterDelete`, `FixedCancelled`, and `FixedFollower` cover the remaining terminal scenarios.
