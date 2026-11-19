# Native completion records

`NativeCompletionRecord.tla` models the recording step between a durable native result and the existing row cleanup. The live boundary is `turn_finalizer/actor_state.rs::submit_native`. Boot reaches the same recorder from `turn_finalizer/native_restore.rs::BootInputs::restore` and `RestoredInput::settle_saved`. The recorder uses the settlement proven by its caller. Cleanup revalidates the result before removing the row. A startup recovery can verify the durable channel identity with an empty in-memory session map.

The table covers the completion entry points audited for this slice.

| Entry | Bridge or watcher coordinate | Native disposition and coordinate |
|---|---|---|
| P1 | `src/services/discord/turn_bridge/mod.rs:689` before-start stop | Existing control intent in `src/services/discord/turn_finalizer/native_policy.rs` and `src/services/discord/turn_finalizer/terminal_handler.rs:266`. |
| F1 | `src/services/discord/turn_bridge/post_loop_finalize.rs:184` placeholder | Bridge display cleanup. O owns native publishing. |
| F2 | `src/services/discord/turn_bridge/post_loop_finalize.rs:244` child-session abort | Bridge transport error handling. |
| F3 | `src/services/discord/turn_bridge/post_loop_finalize.rs:263` orphan tool line | Bridge status display. |
| F4, C18 | `src/services/discord/turn_bridge/post_loop_finalize.rs:281`, `src/services/discord/turn_bridge/completion_postlude.rs:635` API friction | Follow-up with memory effects. |
| F5 | `src/services/discord/turn_bridge/post_loop_finalize.rs:341` retry body | Bridge requeue presentation. |
| F6 | `src/services/discord/turn_bridge/post_loop_finalize.rs:346` review dispatch guard | Follow-up with review dispatch completion. |
| F7 | `src/services/discord/turn_bridge/post_loop_finalize.rs:409` empty response telemetry | Bridge observation. C15 records a native empty scheduled response. |
| F8, C5 | `src/services/discord/turn_bridge/post_loop_finalize.rs:450`, `src/services/discord/turn_bridge/completion_postlude.rs:204` session statuses | S2 session status work. |
| F9, C25 | `src/services/discord/turn_bridge/post_loop_finalize.rs::finalize_bridge_terminal` queue release | Existing native wake in `src/services/discord/turn_finalizer/terminal_handler.rs::NativeCleanup::drive`. |
| F10, C4, C23 | `src/services/discord/turn_bridge/post_loop_finalize.rs:673` finalizer and inflight cleanup | Existing native mailbox and row CAS and cleanup acknowledgement in `src/services/discord/turn_finalizer/terminal_handler.rs:680` and `src/services/discord/turn_finalizer/native_restore.rs::RestoredInput::drive`. |
| T1, T14 | `src/services/discord/outbound/delivery_record.rs` completion ledger | O publishing retains delivery custody. |
| T2, T11, T12 | `src/services/discord/tmux_watcher.rs:2251` turn-view reactions | S2. `src/services/discord/turn_finalizer/cleanup.rs:425` has a finalizer reaction hook. Native reachability remains open. |
| T3, T4, T5 | `src/services/discord/turn_bridge/terminal_outcome_delivery.rs` recovery and interruption branches | Native disposition and O notices retain custody. |
| T6 | `src/services/discord/turn_bridge/terminal_outcome_delivery.rs` delivery and lease | O writer owns native Discord delivery. |
| T7 | `src/services/discord/tmux_watcher.rs:1445` scheduled outcome mirror | `src/services/discord/tmux_watcher/commit_decisions.rs:39` excludes native pins before the row mutation. C15 classifies the proven range independently. |
| T8 | `src/services/discord/turn_bridge/terminal_outcome_delivery.rs` rollover prefix cleanup | Bridge streaming display. |
| T9 | `src/services/discord/turn_bridge/terminal_outcome_delivery.rs` TUI quiescence and requeue | Existing native closer proof in `src/services/discord/turn_finalizer/native_terminal.rs`. |
| T10 | `src/services/discord/turn_bridge/completion_guard.rs:550` work dispatch | `src/services/discord/turn_finalizer/native_completion.rs::write` reuses bridge work completion. |
| T13 | `src/services/discord/turn_bridge/terminal_outcome_delivery/` last-turn time | Follow-up display state. |
| C1, C2 | `src/services/discord/turn_bridge/completion_postlude.rs` context and footer | O's `tui_o/writer/turn_summary.rs` owns the footer. |
| C3 | `src/services/discord/turn_finalizer/completion_admission.rs` admission release | Existing native custody in `src/services/discord/turn_finalizer/actor_state.rs::handle_native` and `src/services/discord/turn_finalizer/native_restore.rs::restore`. |
| C6 | `src/services/discord/turn_bridge/completion_postlude.rs:256` isolated session close | S1 keeps the existing session lifecycle. Native holds can include an isolated scheduled row. |
| C7 | `src/services/discord/turn_bridge/completion_postlude.rs` watcher resume | O retains its native channel. |
| C8 | `src/services/discord/turn_bridge/completion_postlude/channel_writeback.rs:86` completion scope | `src/services/discord/turn_bridge/completion_postlude/channel_writeback.rs::native_completion_scope` recognizes the durable channel key and its promoted aliases, then applies the existing scheduled/legacy scope and routine ownership. Per-run presentation determines `channel_context`. |
| C9, C10 | `src/services/discord/turn_bridge/completion_postlude.rs:342` memory history and session identity | Follow-up session writeback. |
| C11 | `src/services/discord/turn_bridge/completion_postlude/prelude_lifecycle.rs:166` prelude consumption | S2. A confirmed prelude can be injected again before that work lands. |
| C12, C13 | `src/services/discord/turn_bridge/completion_postlude.rs:420` feedback and WIP reminders | Follow-up intake context. |
| C14 | `src/services/discord/turn_bridge/completion_postlude.rs:504` completion telemetry | Follow-up observations. |
| C15 | `src/services/discord/turn_bridge/completion_postlude.rs:551` scheduled outcome | `src/services/discord/turn_finalizer/native_completion.rs::write` records the exact range, including `EmptyResponse`. `src/services/discord/tmux_watcher/path_b_record.rs::settle_committed_range` excludes its own native pin. `src/services/discord/receipt_settle.rs:186` excludes native-owned mirrors from boot settlement. |
| C16 | `src/services/discord/turn_bridge/completion_postlude.rs:565` transcript | `src/services/discord/turn_finalizer/native_completion.rs::read_run` and `write` persist the non-empty range for the current row. The structured row ID also controls transcript attribution. `src/services/discord/tmux_watcher/path_b_record.rs::persist_transcript` checks custody on its selected row. |
| C17 | `src/services/discord/turn_bridge/recall_feedback.rs:151` memento stats | S2 with common-parser `ToolResult` translation. |
| C19 | `src/services/discord/turn_bridge/completion_postlude.rs:672` analytics | S2. |
| C20 | `src/services/discord/turn_bridge/completion_postlude.rs` memory capture and reflect | Follow-up backend effects. |
| C21, C22 | `src/services/discord/turn_bridge/completion_postlude.rs` metrics and restart report | Existing bridge observations and restart handling. |
| C24 | `src/services/discord/turn_finalizer/completion_admission.rs` rejected handoff | Existing native actor admission and custody checks. |

## Watcher, recovery and dispatch boundaries

| Entry | Coordinate | Custody decision |
|---|---|---|
| Watcher pre-panel release | `src/services/discord/tmux_watcher.rs:1734`, `src/services/discord/turn_finalizer/cleanup/terminal_release.rs:53` | The pinned native row keeps its mailbox and managed inflight file. |
| Watcher committed cleanup | `src/services/discord/tmux_watcher/terminal_commit_epilogue.rs:262`, `src/services/discord/tmux_watcher/terminal_commit_epilogue.rs:312` | Checks the late row's custody before clearing. An independent legacy late row keeps its usual identity and stale-range checks. |
| Watcher fresh-idle cleanup | `src/services/discord/tmux_watcher/no_result_exits.rs:398`, `src/services/discord/tmux_watcher/no_result_exits.rs:453` | Checks the pinned row's custody before receipt, row clear and finalizer submission. |
| Watcher stall and boot stale reaping | `src/services/discord/tmux_watcher/stall_exit.rs`, `src/services/discord/inflight/removal/boot_reaper.rs` | Adjacent cleanup retains existing authority and is outside the recording liveness premise. |
| Recovery completion | `src/services/discord/recovery_engine/restore_inflight.rs:421`, `src/services/discord/recovery_engine/restore_inflight.rs:1088`, `src/services/discord/recovery_engine/restore_inflight.rs:1354`, `src/services/discord/recovery_engine/restore_inflight.rs:1563` | `receipt_settle.rs::settle_boot` excludes native-held rows before these legacy branches can infer work or remove a row. |
| Watcher work dispatch | `src/services/discord/tmux_watcher.rs:2319`, `src/services/discord/tmux_watcher/path_b_record.rs:295` | Suppresses a watcher completion if the pin or late row has native custody and a structured dispatch ID. Otherwise the watcher fallback completes implementation and rework. Its existing plan and plan-review branches emit no completion event. |
| Native work dispatch | `src/services/discord/turn_finalizer/native_completion.rs:192` | Uses the structured row ID. The reused helper covers implementation, rework, plan and plan-review. Exactly-once completion assumes successful lookup, write, and retention of the native row. |
| Intake dispatch selection | `src/services/discord/router/message_handler/intake_turn/dispatch_runtime.rs:54`, `src/services/discord/router/message_handler/intake_turn.rs:1423`, `src/services/discord/router/message_handler/intake_turn.rs:1521` | A first lookup hit pins its protected decision for prompt, row and webhook. A miss preserves the late lookup, prefix fallback and protection check. A verified ID with no details appears in the prompt without a generic completion contract. |

## Startup, contention and dispatch evidence

| Entry | Coordinate | Completion boundary |
|---|---|---|
| Process startup, new restart generation | `src/services/discord/runtime_bootstrap.rs:299` calls `runtime_bootstrap/native_input.rs::prepare_native_boot`, then `turn_finalizer/native_restore.rs::BootInputs::restore` | The sessions map starts empty. Bulk restore registers custody and sends cleanup only for inputs whose durable result reached recording. |
| Late boot result, restored source and incarnation retry | `src/services/discord/turn_finalizer/native_supply.rs::saved_live`, `Job::Boot`, and the boot offer | Saved live work re-enters `submit_native`. Boot work enters `RestoredInput::settle_saved` and shares `prepare_completion` with bulk restore. |
| Ordinary channel identity | `src/services/discord/session_locator.rs::owns_completion_key` | A single PG read checks the Claude provider, bot token hash, Discord channel and primary key or same-session alias. A missing canonical row uses the legacy key resolver. Lookup failure or a different identity keeps the native turn isolated. |
| Live prepare and contention | `src/services/discord/turn_finalizer/actor_state.rs::submit_native`, `terminal_handler.rs::NativeCleanup::prepare_notice` | The result may be recorded with the same verified settlement while another channel holds the registry lock. A failed prepare or cleanup verification returns an error with the row still pending. |
| Boot prepare and contention | `src/services/discord/turn_finalizer/native_restore.rs::RestoredInput::prepare_completion` and `drive` | Preparation obtains and validates the current result. The shared recorder uses that settlement. The actor revalidates before its CAS. |
| Native supply preflight | `src/services/discord/turn_finalizer/native_supply.rs::durable` | A failed lock probe returns a retryable error before saving or submitting. |
| Fenced operator teardown | `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs::read_native_completion` | An exact withheld routing receipt and a physical-source observation can authorize archive of the routing owner. |
| Watcher receipt and admission retry | `src/services/discord/tmux_watcher/path_b_record.rs::record_receipt`, `src/services/discord/receipt_settle.rs::settle_marked_row`, `router/message_handler/foreign_row.rs` | The watcher can commit a terminal receipt. Admission and boot share the custody check before deleting a row with a committed receipt. |
| Boot receipt, committed mirror and fallback | `src/services/discord/receipt_settle.rs::settle_boot` and `BootReceipts::settle_cleared` | Native-held rows stay on disk for native recovery. The ordinary restore input and its cleared-row receipt fallback take rows outside native custody. |
| ID-less input producers | `src/services/discord/headless_turn.rs` and `router/message_handler/intake_turn/dispatch_runtime.rs` | The headless row can have no structured ID even when its text includes a header. Protected intake can intentionally omit the ID. The shared native writer preserves either absence. |
| Native work and transcript consumers | `src/services/discord/turn_finalizer/native_completion.rs::write`, `src/db/session_transcripts.rs::dispatch_has_assistant_response_db`, `src/dispatch/dispatch_status.rs` | Both native writes use the row ID. Initial and retried work evidence checks query that exact stored ID. |
| Legacy watcher and recovery attribution | `src/services/discord/tmux_watcher/path_b_record.rs::persist_transcript` and `finalize_dispatch`, `src/services/discord/recovery_engine/analytics_transcript.rs::persist_recovered_transcript` | Path B blocks transcript writes on a native-held target. The approved S1 ID-less watcher work fallback stays available. Boot gives legacy restore only rows outside native custody. |

## TLC configurations

The original 27 configurations cover the reviewed S1 design. Four repair configurations exercise startup before the sessions map is populated and registry contention between prepare and cleanup. The full list of expected violations is in `docs/statecharts/expected.tsv`.

| Before suffix | Fixed suffix | Failure before |
|---|---|---|
| `Before` | `FixedNormal` | Recording progress |
| `BootEmptySessionMapBefore` | `FixedBootEmptySessionMap` | Transcript progress with no session map entry |
| `RegistryContendedBefore` | `FixedRegistryContended` | Transcript, scheduled and work progress while the registry is held |
| `CapturedBefore` | `FixedClearBeforeRecord` | Resurrection after clear |
| `EmptyGateBefore` | `FixedEmpty` | Scheduled completion progress |
| `ForeignBefore` | `FixedNormal` | Foreign body |
| `OrderingBefore`, `WatcherClearBefore` | `FixedNormal`, `FixedWatcherRace` | Cleanup before recording |
| `WatcherWorkBefore`, `WatcherCountBefore` | `FixedWatcherRace` | Foreign body or duplicate work completion |
| `GateAllBefore`, `NativeWithoutIdBefore` | `FixedNoDispatchId` | Work progress or duplicate work completion |
| `TwoLookupsBefore` | `FixedPromptOnlyId` | Work progress after a lost row ID |
| `SingleDecisionBefore`, `EarlyRetryBefore` | `FixedFirstMissLateHit` | Work progress after a late lookup hit |

`BaseFirstMissLateHit` confirms that the baseline completes work when its second lookup hits after the first miss.
