# Native input attribution — 진입점 전수표

최초 설계 전수표의 기준 코드: `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`. 아래 기존 mutation 표는 그 기준을 유지한다. A/B1/B2a 추가 포트는 마지막 절에 구분한다.

`git grep -n -E <pattern> -- src`의 전 저장소 결과에서 시험 파일·`cfg(test)` 구간·주석을 제외한 호출 및 선언 전수다. 선언/공통 wrapper도 생략하지 않았다. 같은 함수의 여러 호출은 각각 남겼다. 각 표의 모델 행동은 원래 경로의 의미이며 이번 설계가 모든 경로를 변경한다는 뜻이 아니다. 본문의 핵심 actor 분기 표와 함께 읽는다. 메모리/DB getter는 상태를 바꾸지 않아 제외한다. `GuardedClearOutcome::for_snapshot*` 등 별칭의 실제 제거 구현도 inflight 표의 leaf에서 덮는다.

재현 스크립트·원시 `git grep` 결과: `/Users/itismyfield/.adk/lane-runs/i8148/logs/d1-build-inventory.py`, `d1-census-{lease,inflight,kick,anchor}.txt`. 기준 mutation 표는 이 스크립트가 생성했고, 뒤의 A/B 포트 표는 각 변경 시점의 소스 좌표로 덧붙였다. `hard_stop` 등 이름이 같은 타 subsystem도 과잉 포함하며 모델의 AdjacentEnd는 해당 mailbox 경로만 뜻한다. 실제 상태 값은 아래 핵심 행에서만 정한다.

## 모델 값의 근거

| 값 | 코드 좌표 | 모델 행동 |
|---|---|---|
| 대기열 원본 1·2, lease 없음/있음, actor token·nonce | `src/services/turn_orchestrator.rs:1936`, `:1989`, `:2015`, `:2028`, `:2042`, `:2061`; `episode_identity.rs:142` | Inject, BootRestore, Settle |
| native Unknown/Open/Idle 및 record range | `src/services/tui_o/writer/input_facts.rs:22`, `:39`, `:552`; `shadow/seal.rs:98` | ExternalStart, OwnStart, Duration, Reconstruct |
| Waiting/Dequeued/Attached/Running; enqueue/dequeue/remove/absorbed/command | `src/services/tui_o/writer/input_facts/queued.rs:9`, `:60`; `src/services/tui_o/writer/input_facts/native.rs:13`, `:33` (B1에서 기존 classifier만 이동) | Enqueue, Dequeue, Withdraw, MidTurnJoin |
| assistant end_turn은 strict idle과 다름 | `src/services/tui_o/shadow/identity.rs:70`; `src/services/tui_turn_state.rs:15` | AssistantEnd, Duration |
| 원본/attempt·source inode·실행 nonce·receipt | `src/services/tui_input/human_frame/history/v2.rs:12`, `:323`, `:395`; `history.rs:622`, `:706` | Inject, Enqueue, Reconstruct, Bind |
| inflight 없음/같은 episode/후속 episode, 세대 old/current | `src/services/discord/inflight/model.rs`의 `InflightTurnState`; `save_store/identity_gate/guarded_read.rs:7`; `clear_store/snapshot.rs` | ProjectionSave, ProjectionClear, StaleEvidence |
| 입력 결과 미확정/완료/철회/중단/미검증/운영자 해제 | 기존 `TerminalEvent` (`turn_finalizer.rs:270`), V2 retained 원본 (`queued_head/v2_types.rs`); proposed NativeInputSettlement는 설계 확장 | ObserveProof, Settle, AdjacentEnd, OperatorRelease |
| boot/process incarnation과 binding execution incarnation은 별개 | `recovery_engine/restore_inflight.rs:2378`; `turn_presence/activity.rs:70`; `tui_o/shadow/mod.rs:111` | Restart, BootRestore, Reconstruct, StaleEvidence |

아래 표의 `Projection*`는 모델에서 row 집합 변경으로, `Settle`은 durable 결과→lease CAS→row CAS→wake 네 단계로 확장한다. `AdjacentEnd`는 기존 Cancel/Terminate/clear/stop의 명시적 결과이며 남의 native 종료와 구분한다.

## lease: 142행 (시험·주석 제외 723행)

| 코드 좌표 | 포함 함수/진입점 | 모델 행동 |
|---|---|---|
| `src/services/discord/admin_host_guard.rs:256` | `apply` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/catch_up/retry_state.rs:538` | `clear_channel_discarding_catch_up_backlog` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/health/recovery.rs:285` | `clear_provider_channel_runtime` | BootRestore / Reconstruct |
| `src/services/discord/health/recovery/inflight_repair.rs:188` | `clear_idle_tmux_stale_turn` | BootRestore / Reconstruct |
| `src/services/discord/health/recovery/runtime_lookup.rs:211` | `finish_cancelled_provider_channel_mailbox` | BootRestore / Reconstruct |
| `src/services/discord/health/recovery/stop_judgement.rs:170` | `finish_judged_turn` | BootRestore / Reconstruct |
| `src/services/discord/health/recovery/stop_judgement.rs:191` | `finish_found_turn` | BootRestore / Reconstruct |
| `src/services/discord/health/recovery/stop_judgement.rs:193` | `finish_found_turn` | BootRestore / Reconstruct |
| `src/services/discord/health/recovery/stop_judgement.rs:194` | `finish_found_turn` | BootRestore / Reconstruct |
| `src/services/discord/health/watcher_respawn.rs:440` | `release_stale_mailbox_ownership_after_force_clean` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/live_bridge.rs:286` | `defer_unstarted_turn` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:17` | `unavailable_finish_turn_result` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:54` | `mailbox_finish_judged_turn` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:72` | `mailbox_finish_stopped_turn` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:106` | `finish_judged` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:126` | `mailbox_finish_cancelled_turn_on` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:132` | `mailbox_finish_cancelled_turn_on` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:135` | `mailbox_finish_cancelled_turn_on` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:151` | `mailbox_finish_turn` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:158` | `mailbox_finish_turn` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:188` | `mailbox_finish_turn_if_matches` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:196` | `mailbox_finish_turn_if_matches` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:236` | `unwind_unstarted_turn` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:265` | `mailbox_finish_turn_if_matches_episode_started_before_inner` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:276` | `mailbox_finish_turn_if_matches_episode_started_before_inner` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:294` | `mailbox_finish_turn_if_matches_episode_started_before` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:302` | `mailbox_finish_turn_if_matches_episode_started_before` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:321` | `mailbox_finish_turn_if_matches_episode_started_before_without_completion` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:329` | `mailbox_finish_turn_if_matches_episode_started_before_without_completion` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:341` | `mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:350` | `mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mailbox_finish.rs:362` | `mailbox_clear_channel` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mod.rs:1255` | `record_tmux_watcher_reconnect` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/mod.rs:1565` | `mailbox_try_start_turn` | Inject(배타 lease claim) |
| `src/services/discord/mod.rs:1572` | `mailbox_try_start_turn` | Inject(배타 lease claim) |
| `src/services/discord/mod.rs:1587` | `mailbox_try_start_turn_kinded` | Inject(배타 lease claim) |
| `src/services/discord/mod.rs:1595` | `mailbox_try_start_turn_kinded` | Inject(배타 lease claim) |
| `src/services/discord/placeholder_live_events/mod.rs:129` | `clear_channel` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/placeholder_live_events/mod.rs:134` | `clear_channel` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/placeholder_live_events/panel_cache_invalidation.rs:51` | `clear_channel` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/placeholder_sweeper/abandon_guard.rs:187` | `abandoned_mailbox_finish_actions` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/placeholder_sweeper/abandon_guard.rs:366` | `finalize_abandoned_mailbox_admitted` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/placeholder_sweeper/abandon_guard.rs:376` | `finalize_abandoned_mailbox_admitted` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/queue_dispatch.rs:345` | `mailbox_take_next_automatic_intervention` | BootRestore / Reconstruct |
| `src/services/discord/queue_dispatch.rs:487` | `mailbox_restore_dequeued_head` | BootRestore / Reconstruct |
| `src/services/discord/queue_dispatch/kickoff.rs:280` | `kickoff_queue_channel_inner` | BootRestore / Reconstruct |
| `src/services/discord/queue_io/turn_admission.rs:7` | `mailbox_try_start_turn_kinded_with_feedback` | Inject(배타 lease claim) |
| `src/services/discord/queue_io/turn_admission.rs:15` | `mailbox_try_start_turn_kinded_with_feedback` | Inject(배타 lease claim) |
| `src/services/discord/queue_io/turn_admission.rs:30` | `mailbox_try_start_turn_behind_queue` | Inject(배타 lease claim) |
| `src/services/discord/queue_io/turn_admission.rs:44` | `mailbox_try_start_turn_behind_queue` | Inject(배타 lease claim) |
| `src/services/discord/queue_io/turn_admission.rs:59` | `mailbox_try_start_turn_unless_released` | BootRestore / Reconstruct |
| `src/services/discord/queue_io/turn_admission.rs:67` | `mailbox_try_start_turn_unless_released` | BootRestore / Reconstruct |
| `src/services/discord/queue_io/turn_admission.rs:81` | `mailbox_try_start_turn_adopting` | BootRestore / Reconstruct |
| `src/services/discord/queue_io/turn_admission.rs:90` | `mailbox_try_start_turn_adopting` | BootRestore / Reconstruct |
| `src/services/discord/queue_io/turn_admission.rs:111` | `mailbox_try_start_turn_ordered` | Inject(배타 lease claim) |
| `src/services/discord/queue_io/turn_admission.rs:148` | `mailbox_try_start_turn_ordered` | Inject(배타 lease claim) |
| `src/services/discord/queue_io/turn_admission.rs:153` | `mailbox_try_start_turn_ordered` | Inject(배타 lease claim) |
| `src/services/discord/queue_io/turn_admission.rs:165` | `mailbox_try_start_turn_ordered` | BootRestore / Reconstruct |
| `src/services/discord/recovery_engine/runtime.rs:325` | `reregister_active_turn_from_inflight_inner` | BootRestore / Reconstruct |
| `src/services/discord/relay_recovery/apply.rs:126` | `apply_relay_recovery_decision_admitted` | BootRestore / Reconstruct |
| `src/services/discord/relay_recovery/apply.rs:265` | `apply_relay_recovery_decision_admitted` | BootRestore / Reconstruct |
| `src/services/discord/router/intake_dispatch/queued.rs:51` | `restore` | BootRestore / Reconstruct |
| `src/services/discord/router/intake_dispatch/queued.rs:175` | `admit_queued_intake` | BootRestore / Reconstruct |
| `src/services/discord/router/intake_gate/stale_turn.rs:258` | `release_queue_blocked_stale_active_turn` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/router/message_handler.rs:110` | `try_start_turn_with_stale_busy_heal` | Inject(배타 lease claim) |
| `src/services/discord/router/message_handler.rs:120` | `try_start_turn_with_stale_busy_heal` | Inject(배타 lease claim) |
| `src/services/discord/router/message_handler.rs:142` | `try_start_turn_with_stale_busy_heal` | Inject(배타 lease claim) |
| `src/services/discord/router/message_handler/busy_retry.rs:51` | `defer_owned_front` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/router/message_handler/intake_turn.rs:997` | `handle_text_message` | Inject(배타 lease claim) |
| `src/services/discord/router/message_handler/intake_turn/stale_dispatch_guard.rs:48` | `abort_terminal_dispatch_at_turn_start` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/router/message_handler/tui_followup.rs:679` | `defer_promoted_dispatch_if_hosted_tui_busy` | BootRestore / Reconstruct |
| `src/services/discord/router/message_handler/turn_lifecycle.rs:14` | `mailbox_try_start_turn_with_terminal_marker_cleanup` | Inject(배타 lease claim) |
| `src/services/discord/router/message_handler/turn_lifecycle.rs:31` | `mailbox_try_start_turn_with_terminal_marker_cleanup` | Inject(배타 lease claim) |
| `src/services/discord/router/turn_start.rs:723` | `release_mailbox_after_placeholder_post_failure` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/router/turn_start.rs:742` | `release_mailbox_after_hosted_tui_busy_pre_submit` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/tmux.rs:651` | `start_monitor_auto_turn_when_available` | Inject(배타 lease claim) |
| `src/services/discord/tmux_restart_handoff.rs:463` | `release_relinquished_turn_mailbox` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/tmux_watcher/turn_stream_collector.rs:239` | `collect_turn_stream_body` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/tui_prompt_relay/bridge_gateway.rs:191` | `dispatch_queued_turn` | BootRestore / Reconstruct |
| `src/services/discord/tui_prompt_relay/relay_e2e/mod.rs:787` | `headless_turn_setup_failure_releases_only_its_slot_and_active_count` | Inject(배타 lease claim) |
| `src/services/discord/tui_prompt_relay/relay_e2e/mod.rs:872` | `headless_turn_provider_mismatch_refuses_before_mailbox_or_session_mutation` | Inject(배타 lease claim) |
| `src/services/discord/tui_prompt_relay/synthetic_start/bridge_handoff.rs:196` | `admit_kinded` | Inject(배타 lease claim) |
| `src/services/discord/tui_prompt_relay/synthetic_start/bridge_handoff.rs:201` | `admit_kinded` | BootRestore / Reconstruct |
| `src/services/discord/tui_prompt_relay/synthetic_start/bridge_handoff.rs:374` | `release_unrecorded_actor` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/tui_prompt_relay/synthetic_start/bridge_handoff.rs:888` | `capture_dormant` | Inject(배타 lease claim) |
| `src/services/discord/turn_bridge/finalize_epilogue.rs:156` | `finalize_and_drain_queued_turns` | BootRestore / Reconstruct |
| `src/services/discord/turn_bridge/mod.rs:521` | `spawn_turn_bridge_with_pin` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/turn_finalizer/cleanup/already_finalized.rs:55` | `already_finalized_active_state` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/turn_finalizer/cleanup/terminal_release.rs:67` | `release_restored_watcher_active_turn_before_panel_edit` | BootRestore / Reconstruct |
| `src/services/discord/turn_finalizer/episode.rs:89` | `claim_episode` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/turn_finalizer/finalize.rs:181` | `do_finalize_with_probe` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/turn_finalizer/finalize.rs:189` | `do_finalize_with_probe` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/turn_finalizer/guarded_finish_residue.rs:164` | `release` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/discord/turn_lease.rs:299` | `claim` | OperatorRelease |
| `src/services/discord/zombie_foreground_release.rs:405` | `release_zombie_foreground_turn_guarded` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/queue/cancel_effects.rs:212` | `purge_on` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator.rs:634` | `try_start_turn` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:641` | `try_start_turn` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:654` | `try_start_turn_with_persistence` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:661` | `try_start_turn_with_persistence` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:677` | `try_start_turn_kinded` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:684` | `try_start_turn_kinded` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:696` | `try_start_turn_kinded_with_persistence` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:705` | `try_start_turn_kinded_with_persistence` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:716` | `try_start_turn_kinded_result` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:741` | `try_start_owner_front` | Inject(배타 lease claim) |
| `src/services/turn_orchestrator.rs:764` | `restore_active_turn` | BootRestore / Reconstruct |
| `src/services/turn_orchestrator.rs:771` | `restore_active_turn` | BootRestore / Reconstruct |
| `src/services/turn_orchestrator.rs:782` | `restore_active_turn_kinded` | BootRestore / Reconstruct |
| `src/services/turn_orchestrator.rs:908` | `finish_turn` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator.rs:1611` | `finalize_turn_state` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator.rs:1617` | `finalize_turn_state` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator.rs:2384` | `input_mailbox_step` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator.rs:2426` | `input_mailbox_step` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator.rs:2452` | `input_mailbox_step` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator.rs:2484` | `input_mailbox_step` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/checked_purge.rs:7` | `purge_queue` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/checked_purge.rs:32` | `purge_queue` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/checked_purge.rs:85` | `try_purge_queue` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/clear_channel.rs:161` | `clear_channel_state` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:71` | `try_start_turn_unless_released` | BootRestore / Reconstruct |
| `src/services/turn_orchestrator/episode_identity.rs:79` | `try_start_turn_unless_released` | BootRestore / Reconstruct |
| `src/services/turn_orchestrator/episode_identity.rs:92` | `try_start_turn_adopting` | BootRestore / Reconstruct |
| `src/services/turn_orchestrator/episode_identity.rs:117` | `release_turn_lease_if_matches` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:145` | `finish_turn_if_matches_episode_started_before` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:152` | `finish_turn_if_matches_episode_started_before` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:162` | `finish_turn_if_matches_episode_and_actor_started_before` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:194` | `finish_turn_if_matches` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:263` | `finish_turn_if_matches` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:280` | `finish_turn_if_matches` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:303` | `finish_turn_if_matches` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:323` | `finish_turn_identity_matches` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:390` | `finish_turn_if_token` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:408` | `finish_turn_if_token` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/episode_identity.rs:428` | `finish_turn_if_token` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/lease_release.rs:21` | `release_active_turn_anchor` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/lease_release.rs:26` | `release_active_turn_anchor` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/lease_release.rs:40` | `detach_active_turn_anchor` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/policy_close.rs:302` | `close` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/policy_close.rs:348` | `detach` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/queued_head/v2/control.rs:111` | `route` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |
| `src/services/turn_orchestrator/witnessed_release.rs:93` | `answer` | Settle / AdjacentEnd(nonce/actor 조건은 본문 표) |

## inflight: 254행 (시험·주석 제외 1462행)

| 코드 좌표 | 포함 함수/진입점 | 모델 행동 |
|---|---|---|
| `src/server/routes/health_api.rs:812` | `stale_mailbox_repair_handler` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/commands/control/clear_teardown.rs:229` | `stop_released_turn` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:471` | `archive_completed_identity` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/health/recovery.rs:845` | `maybe_recover_completed_stale_leak` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/carried_turn_close.rs:279` | `try_close` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/inflight_repair.rs:73` | `clear_idle_tmux_stale_turn_inflight_if_pinned` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/inflight_repair.rs:81` | `clear_idle_tmux_stale_turn_inflight_if_pinned` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/inflight_repair.rs:154` | `clear_idle_tmux_stale_turn` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/inflight_repair.rs:241` | `clear_recovered_leak_inflight` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/inflight_repair.rs:249` | `clear_recovered_leak_inflight` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/inflight_repair.rs:254` | `clear_recovered_leak_inflight_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/inflight_repair.rs:261` | `clear_recovered_leak_inflight_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/inflight_repair.rs:268` | `clear_recovered_leak_inflight_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:52` | `clear_persistent_inflight_for_stop` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:57` | `clear_persistent_inflight_for_stop` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:80` | `stop_judged_channel_runtime` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:166` | `stop_judged_channel_runtime` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:174` | `stop_judged_channel_runtime` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:179` | `stop_judged_channel_runtime` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:194` | `stop_judged_channel_runtime` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:246` | `stop_judged_channel_runtime` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/runtime_stop.rs:252` | `stop_judged_channel_runtime` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/watchdog_pass.rs:95` | `revalidate_and_clear_explicit_background_inflight` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/watchdog_pass.rs:119` | `revalidate_and_clear_explicit_background_inflight` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/watchdog_pass.rs:363` | `run_stall_watchdog_pass` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/health/recovery/watchdog_pass.rs:579` | `run_stall_watchdog_pass` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight.rs:539` | `remove_inflight_row_for_policy_op` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:94` | `request_inflight_abandon_if_matches` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:103` | `request_inflight_abandon_if_matches` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:115` | `request_inflight_abandon_if_matches_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:122` | `request_inflight_abandon_if_matches_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:134` | `request_inflight_abandon_for_captured_episode` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:143` | `request_inflight_abandon_for_captured_episode` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:169` | `request_inflight_abandon_matching_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:261` | `request_inflight_abandon_if_matches_zero_owned` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:269` | `request_inflight_abandon_if_matches_zero_owned` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:273` | `request_inflight_abandon_if_matches_zero_owned_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/abandon.rs:279` | `request_inflight_abandon_if_matches_zero_owned_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:8` | `clear_inflight_state_if_matches_identity_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:14` | `clear_inflight_state_if_matches_identity_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:19` | `clear_inflight_state_if_matches_identity_returning_row_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:151` | `clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:210` | `clear_inflight_state_if_matches_identity_turn_nonce_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:217` | `clear_inflight_state_if_matches_identity_turn_nonce_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:233` | `clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:241` | `clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:252` | `clear_inflight_state_for_captured_episode` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:261` | `clear_inflight_state_for_captured_episode` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/identity.rs:444` | `clear_rebind_origin_inflight_state_if_matches_identity_impl_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/clear_store/identity.rs:515` | `clear_rebind_origin_inflight_state_if_matches_identity_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/clear_store/identity.rs:522` | `clear_rebind_origin_inflight_state_if_matches_identity_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/clear_store/identity.rs:537` | `clear_rebind_origin_inflight_state_if_matches_identity_for_reconcile_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/clear_store/identity.rs:545` | `clear_rebind_origin_inflight_state_if_matches_identity_for_reconcile_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/clear_store/mod.rs:64` | `clear_inflight_state` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:125` | `clear_inflight_state_if_matches` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:133` | `clear_inflight_state_if_matches` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:136` | `clear_inflight_state_if_matches_identity` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:141` | `clear_inflight_state_if_matches_identity` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:144` | `clear_inflight_state_if_matches_identity_returning_row` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:152` | `clear_inflight_state_if_matches_identity_returning_row` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:157` | `clear_inflight_state_if_matches_identity_turn_nonce` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:166` | `clear_inflight_state_if_matches_identity_turn_nonce` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:175` | `clear_inflight_state_if_matches_identity_generation` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:186` | `clear_inflight_state_if_matches_identity_generation` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:198` | `clear_inflight_state_for_snapshot` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:205` | `clear_rebind_origin_inflight_state_if_matches_identity` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/clear_store/mod.rs:214` | `clear_rebind_origin_inflight_state_if_matches_identity` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/clear_store/mod.rs:223` | `archive_inflight_state_if_matches_identity_generation` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:234` | `archive_inflight_state_if_matches_identity_generation` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:245` | `archive_inflight_state_if_matches_identity_generation_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:333` | `clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:343` | `clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:353` | `clear_inflight_state_if_matches_identity_after_delivery` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:365` | `clear_inflight_state_if_matches_identity_after_delivery` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:378` | `refresh_inflight_last_offset_if_matches_identity` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/clear_store/mod.rs:391` | `refresh_inflight_last_offset_if_matches_identity` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/clear_store/mod.rs:406` | `clear_inflight_state_if_matches_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:507` | `clear_inflight_state_if_matches_zero_owned` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:514` | `clear_inflight_state_if_matches_zero_owned` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:519` | `clear_inflight_state_if_matches_zero_owned_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:571` | `clear_inflight_state_if_matches_identity_generation_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:603` | `clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:672` | `clear_inflight_state_if_matches_identity_after_delivery_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:767` | `clear_inflight_state_if_matches_tmux_response_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/mod.rs:831` | `refresh_inflight_last_offset_if_matches_identity_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/clear_store/reconcile_gate.rs:138` | `clear_inflight_state_for_reconcile` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/reconcile_gate.rs:146` | `clear_inflight_state_for_reconcile` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/reconcile_gate.rs:306` | `clear_inflight_state_for_reconcile_with_allocation_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/reconcile_gate.rs:312` | `clear_inflight_state_for_reconcile_with_allocation_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/reconcile_gate.rs:324` | `clear_inflight_state_for_reconcile_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/reconcile_gate.rs:330` | `clear_inflight_state_for_reconcile_in_root` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/clear_store/reconcile_gate.rs:346` | `clear_rebind_origin_for_reconcile_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/episode_guard.rs:218` | `adopt_and_lock_inflight_episode` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store.rs:1105` | `save_inflight_state_create_new` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1113` | `save_inflight_state_create_new` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1119` | `save_inflight_state_create_new_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1123` | `save_inflight_state_create_new_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1126` | `save_inflight_state_create_new_in_root_with_write` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1131` | `save_inflight_state_create_new_in_root_with_write` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1152` | `save_inflight_state_create_new_fenced` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1195` | `save_inflight_state_create_new_in_root_with_io` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1301` | `save_inflight_state_if_absent` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1307` | `save_inflight_state_if_absent` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1310` | `save_inflight_state_if_absent_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1314` | `save_inflight_state_if_absent_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store.rs:1322` | `save_inflight_state_if_absent_in_root_with_io` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/delivery_rewind.rs:16` | `save_inflight_delivery_rewind_if_matches_identity` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/delivery_rewind.rs:28` | `save_inflight_delivery_rewind_if_matches_identity` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/delivery_rewind.rs:31` | `save_inflight_delivery_rewind_if_matches_identity_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate.rs:53` | `save_inflight_state_if_identity_unchanged` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate.rs:64` | `save_inflight_state_if_identity_unchanged_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate.rs:95` | `save_inflight_state_if_identity_matches_allow_output_restamp` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate.rs:108` | `save_inflight_state_if_identity_matches_allow_output_restamp_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate.rs:802` | `save_existing_inflight_rebind_adoption_if_matches_identity_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/identity_gate.rs:808` | `save_existing_inflight_rebind_adoption_if_matches_identity_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/identity_gate.rs:818` | `save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/identity_gate.rs:825` | `save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/identity_gate.rs:835` | `save_existing_inflight_rebind_adoption_impl_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/identity_gate.rs:843` | `save_existing_inflight_rebind_adoption_impl_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/identity_gate.rs:854` | `lock_and_save_existing_inflight_rebind_adoption_impl_in_root` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:22` | `patch_bridge_entry_state_if_identity_unchanged` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:30` | `patch_bridge_entry_state_if_identity_unchanged` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:33` | `patch_bridge_entry_state_tracking_placeholder_clear` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:43` | `patch_bridge_entry_state_tracking_placeholder_clear` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:52` | `patch_bridge_entry_state_if_identity_unchanged_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:58` | `patch_bridge_entry_state_if_identity_unchanged_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:61` | `patch_bridge_entry_state_if_identity_unchanged_in_root_impl` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/completion_preserve.rs:27` | `save_inflight_state_if_matches_identity` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:35` | `save_stream_tick_state_preserving_current_message_races` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:46` | `save_stream_tick_state_preserving_current_message_races` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:61` | `save_stream_tick_state_if_bridge_authority` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:72` | `save_stream_tick_state_if_bridge_authority` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:89` | `save_stream_tick_state_preserving_current_message_races_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:98` | `save_stream_tick_state_preserving_current_message_races_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:109` | `save_stream_tick_state_if_bridge_authority_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:118` | `save_stream_tick_state_if_bridge_authority_in_root` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:129` | `save_stream_tick_state_preserving_current_message_races_in_root_with_mode` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/inflight/save_store/rebind_adoption.rs:5` | `save_existing_inflight_rebind_adoption_if_matches_identity` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/rebind_adoption.rs:13` | `save_existing_inflight_rebind_adoption_if_matches_identity` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/rebind_adoption.rs:21` | `save_existing_inflight_rebind_adoption_if_matches_episode` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/rebind_adoption.rs:30` | `save_existing_inflight_rebind_adoption_if_matches_episode` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/rebind_adoption.rs:40` | `save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/rebind_adoption.rs:49` | `save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/rebind_adoption.rs:58` | `save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_episode` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight/save_store/rebind_adoption.rs:68` | `save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_episode` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/inflight_heartbeat_sweeper.rs:151` | `run_heartbeat_sweep_pass_inner` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/mod.rs:604` | `clear_inflight_state_for_channel` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/mod.rs:605` | `clear_inflight_state_for_channel` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/placeholder_sweeper/abandon_guard.rs:279` | `delete_state_if_allowed` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/receipt_settle.rs:155` | `settle_marked` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/receipt_settle.rs:323` | `stopped` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/recovery_engine/completion_delivery.rs:84` | `commit` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/idle_captured_response.rs:438` | `retire_captured_ready_response` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/coordinate_adoption.rs:86` | `adopt_coordinates` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/coordinate_adoption.rs:93` | `adopt_coordinates` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:101` | `apply` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:109` | `apply` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:117` | `apply` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:124` | `apply` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:139` | `apply` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:202` | `rebind_inflight_for_channel_inner` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:756` | `rebind_inflight_for_channel_inner` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight.rs:247` | `restore_inflight_states` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight.rs:648` | `restore_inflight_states` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight.rs:1274` | `restore_inflight_states` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight.rs:1551` | `restore_inflight_states` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight.rs:1733` | `restore_inflight_states` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight.rs:1804` | `restore_inflight_states` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight.rs:1906` | `restore_inflight_states` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight.rs:2179` | `restore_inflight_states` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/restore_inflight/replay_hold.rs:122` | `deliver_held_debt_checked` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_engine/runtime.rs:251` | `reregister_active_turn_from_inflight_inner` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_paths/restart.rs:116` | `dispose_recovery_relay_outcome` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/recovery_paths/restart.rs:820` | `apply_undeliverable_relay_disposition` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/relay_recovery/apply.rs:231` | `apply_relay_recovery_decision_admitted` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/relay_recovery/apply.rs:474` | `apply_relay_recovery_decision_admitted` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/relay_recovery/idle_tmux.rs:49` | `clear_idle_tmux_reattach_inflight_if_pinned` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/relay_recovery/idle_tmux.rs:57` | `clear_idle_tmux_reattach_inflight_if_pinned` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/router/intake_gate/thread_guard_release.rs:137` | `thread_guard_force_clean_stale_thread` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/router/message_handler/foreign_row.rs:21` | `held_by_another_turn` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/foreign_row.rs:47` | `admit_headless` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/headless_turn.rs:942` | `start_reserved_headless_turn_admitted` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/intake_turn.rs:2037` | `handle_text_message` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/intake_turn/inflight_create_log.rs:21` | `log_create_new_inflight_outcome` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/provider_isolation.rs:253` | `save_intake_inflight` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/provider_isolation.rs:259` | `save_intake_inflight` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/provider_isolation.rs:261` | `save_intake_inflight` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/provider_isolation.rs:807` | `ensure_provider_worktree_isolation` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/router/message_handler/watchdog.rs:284` | `attach_paused_turn_watcher_for_inflight` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/session_runtime/worktree.rs:602` | `sync_inflight_worktree_context` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/standby_relay.rs:230` | `run_standby_relay` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/standby_relay.rs:593` | `refresh_standby_inflight_heartbeat` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/standby_relay.rs:602` | `refresh_standby_inflight_heartbeat` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/standby_relay.rs:708` | `complete_standby_inflight_state` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tmux/monitor_auto_turn_inflight.rs:54` | `ensure_monitor_auto_turn_inflight` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/tmux_restart_handoff.rs:420` | `start_restart_handoff_from_state` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tmux_restart_handoff.rs:424` | `start_restart_handoff_from_state` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tmux_restart_handoff.rs:426` | `start_restart_handoff_from_state` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/tmux_watcher/liveness.rs:529` | `reacquire_watcher_inflight_for_active_stream` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/tmux_watcher/liveness.rs:557` | `discard_restored_response_seed_before_no_inflight_terminal_relay` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/tmux_watcher/no_result_exits.rs:447` | `handle_no_result_exits` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tmux_watcher/stall_exit.rs:50` | `finalize_pinned_watcher_exit` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tmux_watcher/terminal_commit_epilogue.rs:309` | `run_terminal_commit_epilogue` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tmux_watcher/terminal_preflight.rs:524` | `run_terminal_preflight_prepare` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/tmux_watcher/turn_identity.rs:24` | `emit_explicit_inflight_cleanup_signal` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tmux_watcher/turn_identity.rs:146` | `emit_explicit_inflight_cleanup_signal_pane_dead` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tui_direct_pending_start/foreign_recovery.rs:198` | `submit_stale_foreign_inflight_cancel` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/tui_direct_pending_start/foreign_recovery.rs:289` | `submit_committed_foreign_inflight_complete` | BootRestore / ProjectionSave / ProjectionClear |
| `src/services/discord/tui_direct_pending_start/state.rs:403` | `save_row_unless_retired` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/tui_prompt_relay/codex_idle_rollout.rs:223` | `replay_and_relay_codex_sources` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/tui_prompt_relay/relay_e2e/registered_bootstrap_e2e.rs:144` | `seed` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/tui_prompt_relay/relay_e2e/thread_guard_host_e2e.rs:456` | `thread_guard_keeps_a_successor_admitted_after_the_witness_pg` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tui_prompt_relay/relay_e2e/thread_guard_host_e2e.rs:477` | `thread_guard_keeps_a_successor_admitted_after_the_witness_pg` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/tui_prompt_relay/relay_e2e/thread_guard_host_e2e.rs:618` | `thread_guard_keeps_a_successor_route_published_after_the_release_pg` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/tui_prompt_relay/synthetic_start.rs:2138` | `finish_tui_direct_synthetic_turn_if_current` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/tui_prompt_relay/synthetic_start/bridge_handoff.rs:277` | `refresh_existing` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/bridge_entry_persist.rs:300` | `persist_bridge_entry_inflight_state` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/bridge_entry_persist.rs:321` | `persist_bridge_entry_inflight_state` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/bridge_entry_persist.rs:329` | `persist_bridge_entry_inflight_state` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/bridge_entry_persist.rs:373` | `establish_bridge_entry_authority` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/completion_postlude.rs:781` | `run_completion_postlude` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/completion_postlude.rs:812` | `run_completion_postlude` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/completion_postlude.rs:871` | `run_completion_postlude` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/completion_postlude.rs:944` | `run_completion_postlude` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/completion_postlude.rs:976` | `run_completion_postlude` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/guards.rs:318` | `drop` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/post_loop_finalize.rs:186` | `run_post_loop_finalize` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/post_loop_finalize.rs:206` | `run_post_loop_finalize` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/post_loop_finalize.rs:239` | `run_post_loop_finalize` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/post_loop_finalize.rs:274` | `run_post_loop_finalize` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/retry_state.rs:321` | `reset_session_for_auto_retry` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/retry_state/delivery_rewind.rs:106` | `persist_delivery_rewind` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/stream_loop/tool_arms/authority.rs:59` | `strict_tool_fence_save` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/stream_tick/guarded_persist.rs:161` | `persist_stream_tick_state_with_authority_mode` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/stream_tick/guarded_persist.rs:170` | `persist_stream_tick_state_with_authority_mode` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/terminal_delivery.rs:543` | `bridge_epilogue_clears_inflight` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs:104` | `handle_cancel_prompt_replace` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs:131` | `handle_cancel_prompt_replace` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs:148` | `handle_cancel_prompt_replace` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs:163` | `handle_cancel_prompt_replace` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs:180` | `handle_cancel_prompt_replace` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue.rs:100` | `handle_delivery_epilogue` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_bridge/tmux_runtime.rs:89` | `should_clear_inflight` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/tmux_runtime/stop_host.rs:93` | `may_clear_inflight` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_bridge/two_message_panel.rs:314` | `reanchor_bridge_two_message_status_panel_below_answer` | ProjectionSave(Inject / native observation, 기존 identity fence) |
| `src/services/discord/turn_finalizer/cleanup/already_finalized.rs:53` | `already_finalized_active_state` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_finalizer/episode.rs:136` | `claim_episode` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_lease.rs:261` | `clear_late_projection` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/turn_lease.rs:309` | `claim` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/discord/watchers/dispatched_origin_ghost.rs:103` | `consume_dispatched_origin_ghost_if_current` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/turn_lifecycle.rs:522` | `stop_turn_with_policy` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/turn_lifecycle.rs:524` | `stop_turn_with_policy` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/turn_lifecycle.rs:978` | `clear_inflight_by_channel` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |
| `src/services/turn_lifecycle.rs:979` | `clear_inflight_by_channel` | ProjectionClear(Settle / OperatorRelease / AdjacentEnd) |

## inflight_alias: 64행 (시험·주석 제외 20행)

| 코드 좌표 | 포함 함수/진입점 | 모델 행동 |
|---|---|---|
| `src/engine/ops/exec_ops.rs:337` | `register_exec_ops_with_runner` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/herdr_row_end.rs:64` | `clear_restarted_herdr_row` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:42` | `clear_inflight_state_if_matches_identity_returning_row_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:94` | `remove_identity_matched_state` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:105` | `remove_identity_matched_state` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:108` | `remove_identity_matched_state_unchecked` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:198` | `clear_inflight_state_if_matches_identity_turn_nonce_impl_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:279` | `clear_admitted_restart_terminal` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:324` | `clear_admitted_restart_terminal` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:394` | `clear_before_start_receipt_locked` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/identity.rs:503` | `clear_rebind_origin_inflight_state_if_matches_identity_impl_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/mod.rs:202` | `clear_inflight_state_for_snapshot` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/mod.rs:580` | `clear_inflight_state_if_matches_identity_generation_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/mod.rs:593` | `remove_row` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/operator_disposition.rs:6` | `operator_disposition_remove_pinned` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/operator_disposition.rs:33` | `remove_pinned_under_lock` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/snapshot.rs:14` | `for_snapshot_with_detail` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/snapshot.rs:21` | `for_snapshot_with_detail` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/snapshot.rs:83` | `clear_if_matches_generation_and_episode_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/clear_store/snapshot.rs:148` | `clear_if_matches_generation_and_episode_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/episode_guard.rs:126` | `bind_synthetic_anchor_under_guard` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/episode_guard.rs:148` | `persist_verified_under_guard` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/finalizer_identity.rs:52` | `backfill_finalizer_turn_id_under_lock` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/orphan_relay_reclaim.rs:251` | `downgrade_orphan_owner_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/orphan_relay_reclaim.rs:391` | `mark_session_bound_relay_delivered_locked_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/ownership_ops.rs:175` | `bind_status_panel_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/ownership_ops.rs:254` | `clear_status_panel_if_current_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/ownership_ops.rs:362` | `clear_current_msg_if_matches_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/removal/scoped_restore.rs:99` | `backfill_scoped_restore` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate.rs:131` | `save_inflight_state_identity_gated_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate.rs:218` | `save_inflight_state_identity_gated_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate.rs:278` | `patch_restart_full_response_if_identity_unchanged_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate.rs:357` | `patch_restart_full_response_if_identity_unchanged_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate.rs:626` | `persist_leak_recovery_response_offset_if_matches_identity_locked_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate.rs:688` | `persist_recovery_output_path_if_matches_identity_locked_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:90` | `patch_bridge_entry_state_if_identity_unchanged_in_root_impl` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:193` | `patch_bridge_entry_state_if_identity_unchanged_in_root_impl` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/captured_nonce.rs:64` | `save_admitted_nonce_transition` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/captured_nonce.rs:76` | `read_owned_row_for_guarded_write` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/completion_preserve.rs:129` | `save_inflight_state_if_matches_identity_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/heartbeat.rs:61` | `touch_inflight_state_if_matches_identity_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/runtime_stamp.rs:254` | `persist_terminal_range` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/runtime_stamp.rs:585` | `stamp_runtime_handoff_if_matches_identity_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/runtime_stamp.rs:688` | `stamp_runtime_handoff_if_matches_identity_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/runtime_stamp/herdr_terminal_admission.rs:151` | `admit` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/runtime_stamp/herdr_terminal_admission.rs:207` | `take_drain_restart` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:153` | `save_stream_tick_state_preserving_current_message_races_in_root_with_mode` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:389` | `save_stream_tick_state_preserving_current_message_races_in_root_with_mode` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:503` | `patch_restart_mode_if_matches_identity_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:579` | `clear_long_running_placeholder_if_matches_identity_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/store.rs:621` | `persist_under_lock_inner` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/store.rs:696` | `persist_under_lock` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/store.rs:702` | `persist_under_lock` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/store.rs:721` | `persist_under_lock_with_snapshot` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/store.rs:727` | `persist_under_lock_with_snapshot` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/store.rs:745` | `persist_readopted_under_lock` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/store.rs:753` | `persist_under_lock_preserving_updated_at` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/store.rs:759` | `persist_under_lock_preserving_updated_at` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/watcher_state.rs:209` | `persist_watcher_stream_progress_locked_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/watcher_state.rs:323` | `commit_watcher_terminal_delivery_locked_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/inflight/watcher_state.rs:426` | `persist_watcher_relay_watermark_locked_in_root` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/recovery_engine/herdr_admitted_restart.rs:178` | `settle_checked` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/tui_direct_pending_start/turn_retirement.rs:115` | `retire_channel` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |
| `src/services/discord/turn_finalizer/episode/row_retirement.rs:105` | `retire_drifted_row` | ProjectionSave / ProjectionClear (공통 lock·identity 구현과 alias) |

## kick: 173행 (시험·주석 제외 293행)

| 코드 좌표 | 포함 함수/진입점 | 모델 행동 |
|---|---|---|
| `src/dispatch/dispatch_channel.rs:36` | `dispatch_type_skips_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/dispatch/dispatch_status/review_skip.rs:31` | `restore_auto_queue_mainline_after_review_skip_on_pg` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/engine/transition.rs:595` | `decide_dispatch_attached` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/pipeline.rs:850` | `kickoff_for` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/pipeline.rs:996` | `is_dispatch_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/auto_queue/phase_gate.rs:290` | `create_activate_dispatch_pg_inner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/auto_queue/phase_gate.rs:295` | `create_activate_dispatch_pg_inner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/commands/stop.rs:189` | `after_native_publish` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/commands/text_commands.rs:54` | `schedule_text_stop_pending_queue_drain` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/health/recovery.rs:158` | `apply_runtime_hard_stop_cleanup` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/health/recovery.rs:188` | `apply_runtime_hard_stop_cleanup` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/health/recovery.rs:208` | `apply_runtime_hard_stop_finalizer_cleanup_pre_release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/health/recovery/channel_runtime.rs:234` | `schedule_pending_queue_drain_after_cancel` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/health/recovery/watchdog_pass.rs:414` | `run_stall_watchdog_pass` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/health/recovery/zombie_reclaim.rs:41` | `release_zombie_foreground_turn_by_tmux_name` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/live_bridge.rs:337` | `defer_unstarted_turn` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/mailbox_finish.rs:255` | `unwind_unstarted_turn` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/mod.rs:1467` | `cleanup_retry_inflight_blocks_idle_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/mod.rs:1540` | `idle_queue_snapshot_has_kickable_backlog` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/mod.rs:1546` | `idle_queue_snapshot_has_kickable_backlog` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/mod.rs:2105` | `enqueue_internal_followup` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/mod.rs:2159` | `maybe_schedule_catch_up_retry_after_queue_drain` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/mod.rs:2186` | `idle_queue_take_next_soft_if_ready` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/mod.rs:2228` | `idle_queue_take_next_soft_if_ready` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/placeholder_sweeper/abandon_guard.rs:390` | `finalize_abandoned_mailbox_admitted` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch.rs:88` | `log_kickoff_rejected_restore` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch.rs:264` | `mailbox_take_next_automatic_intervention` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch.rs:317` | `mailbox_take_next_automatic_intervention` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:92` | `kickoff_idle_queue_channel` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:97` | `kickoff_idle_queue_channel` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:100` | `kickoff_queue_channel` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:107` | `kickoff_queue_channel` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:113` | `kickoff_queue_channel_inner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:123` | `kickoff_queue_channel_inner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:241` | `kickoff_queue_channel_inner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:250` | `kickoff_queue_channel_inner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:357` | `kickoff_idle_queues` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:363` | `kickoff_idle_queues` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:376` | `kickoff_idle_queues_with_deps` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:398` | `kickoff_idle_queues_with_deps` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:419` | `kickoff_idle_queues_with_deps` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_dispatch/kickoff.rs:431` | `kickoff_idle_queues_with_deps` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io.rs:319` | `schedule_race_loss_requeue_post_enqueue_idle_recheck` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io.rs:379` | `schedule_writer_ready_queue_kick` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io.rs:506` | `kick_queue_channel_if_context_available` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io.rs:508` | `kick_queue_channel_if_context_available` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/auto_queue_eligibility.rs:37` | `held` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/auto_queue_eligibility.rs:38` | `held` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/auto_queue_eligibility.rs:102` | `run_queued_lane` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:214` | `schedule_deferred_idle_queue_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:220` | `schedule_deferred_idle_queue_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:232` | `schedule_deferred_idle_queue_kickoff_immediate` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:238` | `schedule_deferred_idle_queue_kickoff_immediate` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:371` | `arm_slow_idle_queue_backstop_if_queue_nonempty` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:428` | `wake_coalesced` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:669` | `reconcile_all_ready_idle_queues` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:786` | `schedule_deferred_idle_queue_kickoff_inner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/backstop.rs:834` | `schedule_deferred_idle_queue_kickoff_inner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/input_owner/claude_handoff.rs:303` | `schedule` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/input_owner/closed.rs:28` | `closed` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/input_owner/ingress.rs:458` | `admit_raw` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/input_owner/routine.rs:380` | `owner_input_wake` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/turn_admission.rs:219` | `mailbox_recovery_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_io/turn_admission.rs:239` | `mailbox_recovery_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/queue_marker.rs:190` | `start_and_drain_kickoff_markers` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/recovery_engine/herdr_reader.rs:366` | `reconnect_pass` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/recovery_engine/herdr_server_replaced.rs:162` | `request` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/recovery_engine/restore_inflight.rs:2090` | `restore_inflight_states` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/recovery_engine/restore_inflight.rs:2238` | `restore_inflight_states` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/recovery_engine/restore_inflight/kickoff_identity.rs:14` | `recovery_kickoff_identity` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/relay_recovery/apply.rs:79` | `apply_relay_recovery_decision_admitted` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/relay_recovery/apply.rs:162` | `apply_relay_recovery_decision_admitted` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/relay_recovery/apply.rs:274` | `apply_relay_recovery_decision_admitted` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_dispatch.rs:940` | `defer_live_submission` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_dispatch/queued.rs:73` | `restore` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_dispatch/queued.rs:196` | `admit_queued_intake` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_gate.rs:1066` | `handle_event` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_gate/queue_effects.rs:183` | `schedule_idle_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_gate/queue_effects.rs:184` | `schedule_idle_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_gate/stale_turn.rs:271` | `release_queue_blocked_stale_active_turn` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_gate/thread_guard_release.rs:165` | `thread_guard_force_clean_stale_thread` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_gate/thread_guard_release.rs:178` | `thread_guard_force_clean_stale_thread` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_queue_transaction.rs:326` | `schedule_idle_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/intake_queue_transaction.rs:468` | `commit_soft_intervention_transaction` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/message_handler/intake_turn/adk_thread.rs:74` | `publish` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/message_handler/intake_turn/race_loss.rs:120` | `enqueue_race_loss_requeued_intervention` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/message_handler/intake_turn/stale_dispatch_guard.rs:66` | `abort_terminal_dispatch_at_turn_start` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:611` | `wake` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/message_handler/tui_followup.rs:701` | `defer_promoted_dispatch_if_hosted_tui_busy` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/turn_start.rs:725` | `release_mailbox_after_placeholder_post_failure` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/router/turn_start.rs:751` | `release_mailbox_after_hosted_tui_busy_pre_submit` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/runtime_bootstrap/recovery_flush.rs:95` | `run_bot_spawn_recovery_and_flush_restart_reports` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/runtime_bootstrap/recovery_flush.rs:136` | `run_bot_spawn_recovery_and_flush_restart_reports` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/session_transition.rs:153` | `wake_installed_owner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tmux_restart_handoff.rs:481` | `release_relinquished_turn_mailbox` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tmux_restart_handoff.rs:541` | `resume_aborted_restart_turn` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tmux_restart_handoff.rs:545` | `resume_aborted_restart_turn` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tmux_watcher/terminal_commit_epilogue.rs:539` | `run_terminal_commit_epilogue` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_direct_pending_start.rs:592` | `promote_queued_follow_up_after_abort` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_direct_pending_start/state.rs:258` | `pending_synthetic_start_blocks_idle_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay.rs:330` | `relay_observed_prompt` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/bridge_gateway.rs:209` | `dispatch_queued_turn` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs:20` | `module` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs:41` | `validate_promoted_b_completion_lifecycle` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs:54` | `validate_promoted_b_completion_lifecycle` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs:73` | `drain_promoted_b_completion_lifecycle` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs:84` | `drain_promoted_b_completion_lifecycle` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs:172` | `promoted_b_completion_lifecycle_validates_order_and_cardinality` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs:184` | `promoted_b_completion_lifecycle_validates_order_and_cardinality` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/rehydration.rs:459` | `wake_observed_source` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/relay_e2e/registered_bootstrap_e2e.rs:250` | `kick_off` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/tui_prompt_relay/relay_e2e/thread_guard_host_e2e.rs:372` | `thread_guard_leaves_an_unanchored_thread_turn_alone_pg` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_bridge/finalize_epilogue.rs:120` | `finalize_and_drain_queued_turns` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_bridge/finalize_epilogue.rs:168` | `finalize_and_drain_queued_turns` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_bridge/finalize_epilogue.rs:217` | `finalize_and_drain_queued_turns` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_bridge/followup_requeue.rs:79` | `schedule_retry_if_eligible` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_bridge/followup_requeue/draft_release.rs:145` | `watch_draft_release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_bridge/followup_requeue/draft_release.rs:149` | `watch_draft_release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_completion_events.rs:19` | `module` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_completion_events.rs:66` | `queue_eligible` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_completion_events.rs:71` | `queue_is_eligible` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_completion_events.rs:111` | `publish_queue_eligible_completion_event` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer.rs:686` | `actor_loop` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer.rs:691` | `actor_loop` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup.rs:102` | `kickoff_thread_parents_after_finalize` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup.rs:108` | `kickoff_thread_parents_after_finalize` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup.rs:130` | `kick_thread_parents_after_turn_release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup.rs:136` | `kick_thread_parents_after_turn_release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup/already_finalized.rs:73` | `already_finalized_active_state` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup/already_finalized.rs:89` | `already_finalized_active_state` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup/terminal_release.rs:97` | `release_restored_watcher_active_turn_before_panel_edit` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup/terminal_release.rs:101` | `release_restored_watcher_active_turn_before_panel_edit` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup/terminal_release.rs:119` | `rearm_queue_backstop_after_mailbox_release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/cleanup/terminal_release.rs:131` | `rearm_queue_backstop_after_mailbox_release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/completion_admission.rs:90` | `publish_claimed_queue_eligible` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/completion_admission.rs:98` | `publish_claimed_queue_eligible` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/completion_admission_actor.rs:86` | `update_completion_admission` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/completion_admission_actor.rs:151` | `note_mailbox_release_after_finalize` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/finalize.rs:319` | `do_finalize_with_probe` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/finalize.rs:331` | `do_finalize_with_probe` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/guarded_finish_residue.rs:44` | `handle_idle_queue_guard_skip` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/guarded_finish_residue.rs:184` | `release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/guarded_finish_residue.rs:191` | `release` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/policy_commit.rs:216` | `run_tail` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/policy_commit.rs:227` | `run_tail` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/reconcile.rs:63` | `run_backstop_finalize` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_finalizer/reconcile.rs:353` | `reconcile` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_presence/supervisor.rs:295` | `kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_presence/supervisor.rs:525` | `act` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_presence/supervisor.rs:857` | `kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_presence/supervisor.rs:858` | `kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_presence/supervisor.rs:869` | `wake_owner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/turn_presence/supervisor.rs:874` | `wake_owner` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/discord/zombie_foreground_release.rs:437` | `release_zombie_foreground_turn_guarded` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator.rs:130` | `module` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator.rs:817` | `recovery_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator.rs:1145` | `wait` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator.rs:1166` | `turn_finished` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator.rs:2053` | `input_mailbox_step` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator.rs:2064` | `input_mailbox_step` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/recovery_kickoff.rs:39` | `kickoff_refusal` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/recovery_kickoff.rs:48` | `kickoff_refusal` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/recovery_kickoff.rs:58` | `occupied_kickoff_outcome` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/registry_purge.rs:290` | `recovery_kickoff_with_closed_retry` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/registry_purge.rs:300` | `recovery_kickoff_with_closed_retry` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/remint_fence.rs:58` | `note_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/remint_fence.rs:119` | `note_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/remint_fence.rs:125` | `note_kickoff` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/turn_finished_signal.rs:13` | `module` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/turn_finished_signal.rs:18` | `module` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/turn_finished_signal.rs:49` | `wait` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/turn_finished_signal.rs:52` | `turn_finished_signal` | Kick(wake/event/queue admission, 같은 head 재검증) |
| `src/services/turn_orchestrator/turn_finished_signal.rs:56` | `turn_finished_signal` | Kick(wake/event/queue admission, 같은 head 재검증) |

## anchor: 8행 (시험·주석 제외 2행)

| 코드 좌표 | 포함 함수/진입점 | 모델 행동 |
|---|---|---|
| `src/services/turn_orchestrator.rs:1989` | `input_mailbox_step` | Inject / BootRestore / Settle(본문 actor 분기) |
| `src/services/turn_orchestrator.rs:1993` | `input_mailbox_step` | Inject / BootRestore / Settle(본문 actor 분기) |
| `src/services/turn_orchestrator.rs:2028` | `input_mailbox_step` | Inject / BootRestore / Settle(본문 actor 분기) |
| `src/services/turn_orchestrator.rs:2032` | `input_mailbox_step` | Inject / BootRestore / Settle(본문 actor 분기) |
| `src/services/turn_orchestrator.rs:2061` | `input_mailbox_step` | Inject / BootRestore / Settle(본문 actor 분기) |
| `src/services/turn_orchestrator.rs:2065` | `input_mailbox_step` | Inject / BootRestore / Settle(본문 actor 분기) |
| `src/services/turn_orchestrator/lease_release.rs:43` | `detach_active_turn_anchor` | Inject / BootRestore / Settle(본문 actor 분기) |
| `src/services/turn_orchestrator/lease_release.rs:50` | `detach_active_turn_anchor` | Inject / BootRestore / Settle(본문 actor 분기) |


## boot 및 직접 제거 진입점 보강

| 코드 좌표 | 행동/소유 |
|---|---|
| `src/services/discord/runtime_bootstrap.rs:308` | BootRestore: mint 전 public boot reaper |
| `src/services/discord/inflight/removal/boot_reaper.rs:61` | PrepareBoot: 기존 prepare_before_reap 포트 |
| `src/services/discord/inflight/removal/boot_reaper.rs:95` | PrepareBoot→custody→Reap 순서 |
| `src/services/discord/inflight/removal/boot_reaper.rs:131` | Reap: provider row census |
| `src/services/discord/inflight/removal/boot_reaper.rs:218` | ProjectionClear: lock-held 직접 unlink |
| `src/services/discord/inflight/circuit_removal.rs:55` `delete` | AdjacentEnd/ProjectionClear: circuit 제거 |
| `src/services/discord/inflight/rebind_reap.rs:311` | AdjacentEnd/ProjectionClear: abandoned rebind |
| `src/services/discord/inflight/rebind_reap.rs:422` | AdjacentEnd/ProjectionClear: dead watcher rebind |

모델의 review record는 registration 있음·owning_episode None follower와 boot 준비/보존/행 존재/restore-head를 이 진입점의 두 값으로 구분한다. 실패한 준비는 reaper를 허가하지 않는다.


## A reader 구현 좌표

위 전수표는 d1 기준 main 좌표이며 A는 다음 기존 포트에만 연결한다. runtime root는 `runtime_store::runtime_root()`다(그 아래 `discord_inflight` row root와 구별한다).

| 함수 | A 행동 | B에 남는 소비 |
|---|---|---|
| `src/services/tui_input/human_frame/history/v2/native.rs:99` `boot_records` | v3 live/planned/passive의 미정리 ticket·결과·원본 registration 읽기 | native proof fold 및 저장 결과에 따른 cleanup 재개 |
| `src/services/discord/inflight/removal/boot_reaper.rs:68` `prepare_before_reap` | 기존 boot port에서 먼저 native custody 준비; 실패 시 pass 예약·행 삭제 보류 | 보존 자료 restore census |
| `src/services/discord/inflight/removal/boot_custody.rs:434` `preserve_native_before_boot_reap` | 기존 retention lock·durable custody publication 재사용 | source incarnation 인증 후 최종 결과 확정 |
| `src/services/tui_input/human_frame/history/v2/native.rs:174` `record_native_outcome` | cfg(test) 계약 API: 등록+전체 ticket 일치, None follower 포함 | 운영 producer 연결 |
| `src/services/turn_orchestrator/episode_identity.rs:162`, `src/services/discord/turn_finalizer/episode.rs:47`, `src/services/discord/inflight/clear_store/identity.rs:252` | 기존 exact episode/actor/row CAS를 유지 | Some episode만 이 경로 호출; None follower는 정리 권한 없음 |
| `src/services/discord/queue_io/input_owner/drive.rs:88` `wake` | 기존 owner wake·claim 계약을 통합 fixture에서 실제 호출 | 결과/cleanup 소비 후 운영 wake 연결 |
| `src/services/tui_input/human_frame/history/v2/native.rs:152` `prepare_native` | Inject 준비: cfg(test) 최초 ticket과 envelope3를 같은 transaction으로 저장 | 운영 호출 연결은 B |
| `src/services/tui_input/human_frame/history/v2/native.rs:200` `ack_native_withdrawal_notice` | NoticeAck: cfg(test) 영속 outbox enqueue 뒤 철회 안내 ack | 운영 호출 연결은 B |

A의 boot fixture는 실제 prepare→custody→reaper 뒤 시험 소비자가 등록 결과·기존 exact mailbox finish·owner drive를 연결한다. 자동 restore/결과 producer가 운영에 연결되었다는 주장은 하지 않는다.

## B1 읽기·준비 포트 (비활성)

| file:line / 함수 | 행동·모델 값의 근거 |
|---|---|
| `src/services/tui_o/writer/input_facts.rs:570` `observe_native`; `:579` `native_evidence`; `:536` 같은 capture의 record 연결 | 첫 record 전 opt-in, authenticated prefix만. `NativeInputPreparation.source`의 PreSource/Bound는 아래 bind_source에서 오며 capture 재시작의 물리 SourceId는 기존 SourceBinding을 사용한다. |
| `src/services/tui_o/writer/input_facts/native.rs:117` `Stage`; `:150` `fold`; `:104` `Evidence` | Waiting/Taken/Attached/Running/Removed, 실제 source 순서·FIFO·membership/strict closer·철회. namespace/전체 ticket과 native range를 소비자에게 반환할 뿐 row·lease를 변경하지 않는다. |
| `src/services/tui_input/human_frame/history/v2/native.rs:20` `Intent`; `:31` `episode` | Primary/Follower, Some/None. primary 원본 id 일치·nonce 비어 있지 않음·generation>0. 모델 pin은 이 명시적 intent에서만 온다. |
| `src/services/discord/queue_io/input_owner/pump.rs:102` `submit_native_front` → 공통 reserve/effect | 모델 Intent→Prepared는 attribution을 담은 기존 durable reserve, AnyKey는 그 뒤 기존 effect leaf, NotSent는 leaf 전 typed 거절이다. 생산 caller는 아직 없다. |
| `src/services/turn_orchestrator/queued_head/v2/sources.rs:14` `native_intent`; `prepare_history/bind_source/restore` | 원본 primary를 기준으로 decode. RuntimeReady와 Unknown/Reserved 복원은 기존 source·phase 형식을 보존한다. NotSent 원본은 기존 unarmed 복원으로 queued에 남고 결과를 만들지 않는다. |
| `src/services/tui_input/human_frame/history/v2/native.rs:183` `native_evidence_registrations` | 기존 abandoned no-key 등록과 결과/cleanup 완료를 새 membership 후보에서 제외. 저장된 결과·boot custody 소비는 B2다. |

B1에는 mailbox turn lease acquire/release, inflight create/delete, owner kick의 새 진입점이 없다. 기존 전수표는 유지한다. `NativeInputPreparation.phase`의 Intent/Prepared/Unknown/NotSent는 위 준비 경계의 추상 이름이며 별도 운영 상태 파일·전역 guard가 아니다. 이 모델의 result=None은 B1이 native finalization을 켜지 않는 범위이며 B2 결과/CAS/row/wake 진행성을 대신하지 않는다.


## B2a 추가 포트 (B1 head 이후)

아래 두 표는 B2a head8698df14b의 역사적 접점이다. Watch 연결은 2a에서 제거했으며 현재 접점은 마지막 절을 따른다.

| 코드 접점 | 모델 행동·권한 | 이번 변경 |
|---|---|---|
| `src/services/turn_orchestrator/queued_head/v2/sources.rs::Owner::prepare_history` → `history/v2/native.rs::Registry::prepare_native_intent` | Prepare. 기존 cold effect의 prepare-before-arm 경계 | 명시 intent가 있을 때만 v3 ticket 보존. 운영 intent producer는 아직 없음. lease/inflight/kick 변경0 |
| `src/services/tui_o/writer/input_facts.rs::{observe_native,native_evidence}` 및 `input_facts/native.rs::fold` | Enqueue/Dequeue/Withdraw/MidTurnJoin/OwnStart/Duration | 동일 full capture의 후보만; receipt·source·FIFO 유일성, popAll은 debt 없음 |
| `src/services/discord/turn_presence/activity.rs::{read_from,read_tail}` → `activity/native_input.rs::prepare` | Reconstruct. full prefix 없으면 native 후보 없음 | worker/tail 설치. aggregate activity 판정·lease 변화 없음 |
| `activity.rs::{judge_with_policy,install}` → `activity/native_input.rs::{observe,snapshot}` | ObserveProof 전 read-only 후보 | 현재 binding/generation/seq/revision/through만 stamp. result writer/actor/wake 연결0 |
| `src/services/discord/turn_finalizer/terminal_handler.rs::NativeAdmission::capture` | typed admission의 비활성 정밀화 | cfg(all(test,unix)). source/range/target/episode/actor 검증만, mutator 없음 |

새 lease 취득/해제·inflight 생성/삭제·queue kick 진입점은 없다. `activity::Watch`의 generation/revision과 `Key.seq`, `InputFacts`의 complete-line through, 기존 `SourceBinding/Source` 및 A ticket/episode 필드만 모델 값으로 사용한다. source 분류와 Cancel/Terminate/clear/stop/turn-lease release 전수는 d1 설계 표와 위 baseline mutation 표를 유지하며 B2b에서 공통 결과 CAS에 연결한다.

### B2a 좌표 고정

| file:line (수리 후 정본) | 함수 |
|---|---|
| `src/services/turn_orchestrator/queued_head/v2/sources.rs:178` | `prepare_history` |
| `src/services/tui_input/human_frame/history/v2/native.rs:226` | `prepare_native_intent` |
| `src/services/tui_o/writer/input_facts.rs:569` | `observe_native` |
| `src/services/tui_o/writer/input_facts.rs:581` | `native_evidence` |
| `src/services/tui_o/writer/input_facts/native.rs:173` | `fold` |
| `src/services/discord/turn_presence/activity.rs:970` | `read_from` |
| `src/services/discord/turn_presence/activity.rs:949` | `read_tail` |
| `src/services/discord/turn_presence/activity.rs:583` | `judge_with_policy` |
| `src/services/discord/turn_presence/activity.rs:885` | `install(watch` |
| `src/services/discord/turn_presence/activity/native_input.rs:24` | `snapshot` |
| `src/services/discord/turn_presence/activity/native_input.rs:7` | `prepare` |
| `src/services/discord/turn_finalizer/terminal_handler.rs:321` | `capture(` |

## B2b-1 비활성 정리 포트 (기준 8698df14b)

아래는 `cfg(all(test, unix))` 검증 경계다. 운영 진입점은 아직 없다. 기존 baseline 전수표의 production 소유를 바꾸지 않는다.

| 포트 | 모델 행동 | 권한/변수 값 출처 |
|---|---|---|
| `terminal_handler.rs::NativePrepared::capture` | Prepare / explicit primary pin | 기존 full Ticket, TurnKey, 현재 generation 및 producer가 준 actor Arc. follower None은 cleanup 권한 없음 |
| `native.rs::Registry::note_native_any_key` | AnyKey | 등록·full ticket CAS 뒤 settlement의 확장 metadata `any_key=true`. marker 없음은 NotSent/Unknown 원본 유지 |
| `actor_state.rs::TurnFinalizer::submit_native` → `handle_native` | ObserveProof / DebtCommit | 검증된 same-capture Evidence만 Result. 기존 finalizer actor 메시지 순서, panic은 debt 보존 |
| `terminal_handler.rs::NativeCleanup::drive` → `mailbox_finish.rs::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion` | Settle / DebtRelease | actor Arc·nonce·message ID·pre-key monotonic cutoff exact CAS. 실제 removed_token만 global_active 차감 |
| `NativeCleanup::drive` → `inflight/clear_store/snapshot.rs::GuardedClearOutcome::for_snapshot_with_detail` | ProjectionClear / DebtRow | captured episode/runtime/start/offset, 최신 save_generation은 기존 lock-held CAS. read/parse 오류는 Missing 성공이 아님 |
| `native.rs::Registry::ack_native_cleanup` → `turn_completion_events::publish_queue_eligible_completion_event` | cleanup admission / DebtWake | full ticket + 동일 outcome + 철회 안내 ack. row barrier 뒤 first cleanup만 wake. 기존 ledger admission plan 연결은 B2b-2 |

결과·cleanup metadata를 저장하는 leaf는 기존 frame registry lock/transaction을 쓴다. 독립 DB·전역 guard·새 실행 소유자는 없다. boot/custody-only 소비 및 Cancel/Terminate/clear/stop/release의 production 연결은 본 전수표의 기존 경로 그대로 B2b-2 잔여다.

Start의 기존 production wake 호출은 `turn_finalizer.rs::actor_loop`에서 `actor_state.rs::handle_start`로 같은 동작을 옮겼다. `publish_claimed_queue_eligible` 두 호출은 기존 admission의 Kick이며 새 큐 정책이 아니다. Native cleanup 포트는 row runtime 선확인→actor CAS→row 재확인→lock-held snapshot CAS 순서이고, generic pin이 담지 않는 execution nonce의 ABA는 B2b-2 잔여다.


## B2b-2a P2 대체 포트 (기준 25d22a24a)

| 현재 file:line / 함수 | 모델 행동·값 출처 / 기존 접점 대체 |
|---|---|
| `src/services/discord/turn_presence/activity/native_input.rs:13,31` `Reader::new/poll` | Reconstruct/ObserveProof. Watch를 받지 않는 ticket 전용 full-prefix reader. byte budget은 IO 진행만 제한하며 native 결과를 시간으로 만들지 않는다. `activity.rs`의 prepare/observe/snapshot 호출은 제거했다. |
| `src/services/tui_o/writer/input_facts/native.rs:89,122` `registrations_fingerprint` / `Observation` | namespace·전체 ticket·frame_hash·receipts에서 fingerprint를 만든다. Preparation의 version/readVersion {0,1}은 변경 전/후 fingerprint의 유한 추상화이며 운영 카운터 신설이 아니다. |
| `src/services/discord/turn_finalizer/terminal_handler.rs:338,521` `NativeAdmission::capture/NativePrepared::admit` | 관측/현재 및 registry의 fingerprint가 다르면 typed 거절. source transcript의 revision이 같아도 늦은 receipt가 기존 후보를 취소한다. |
| `src/services/tui_input/human_frame/history/v2/native.rs:291,502` `record_native_outcome_at_stamp/native_transaction_checked` | Settle 결과 publish 전에 동일 history transaction 안에서 fingerprint를 비교하고 Executed membership row의 receipt를 확인한다. 기존 같은 ticket/outcome의 cleanup 재시도는 멱등이다. |

추가 reader·증거·actor-result 포트는 `cfg(all(test,unix))`다. 기존 schema reader/prepare/bind/project는 production에 남고 새 production native producer/actor/lease/row/kick 진입점은 없다. AnyKey 실제 source의 읽기 포트(`claude_tui/owner_input.rs`)가 설계 목록 밖이므로 FILES 경계에서 producer/custody/witness/bridge 연결을 시작하지 않았다.


## B2b-2 ① 연결 포트 (기준 458610721)

기본 운영 선택은 `native_prepare::enabled=false`다. 아래 production 선언/연결을 추가했으나 ticket 발행과 bridge 전이는 켜지 않는다. native 결과 producer/boot/인접 종료는 후속 경계다. 위 B2b-1 표의 runtime nonce ABA 잔여는 잠금 안 검증 leaf로 보강했고, 정리 actor 자체의 production 활성화는 여전히 없다.

| file:line / 진입점 | 모델 행동 / 유한 값 근거 |
|---|---|
| `src/services/claude_tui/owner_input.rs:52` `key_attempted` | AnyKey/NotSent. 기존 scoped `keyed` AtomicBool의 None/false/true를 읽는다. setter와 저장 위치는 기존 것뿐이다. |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input/native_input.rs:40,64,88` `wrap/for_front/submit` | Prepare/Inject. 명시 primary Episode 또는 follower None을 기존 pump에 운반하고 등록 ack 전 leaf를 금지한다. |
| `src/services/discord/turn_finalizer/native_prepare.rs:19,108` `Handle::prepare/handle` | PortRegister. actor-local prepared HashMap의 없음/있음, 기존 ledger Pending/Finalized, oneshot 등록 ack의 실패/성공이 review.prepared의 값 근거다. |
| `native_prepare.rs:53,155` `Handle::witness/NativeKeyed` | AnyKey/NotSent. 키 true만 history receipt를 기록, false는 actor-local 준비만 제거한다. 원본 결과 None을 바꾸지 않는다. |
| `src/services/discord/turn_finalizer/completion_admission.rs:13,74` `native_input/claimable` | NativePortBarrier. None/Some(false)/Some(true)는 기존 배송 관문과 분리된 입력 준비/정리 상태다. PortRow의 nativeAck bool은 cleanup_complete와 admission의 완료 확정 추상화이며 새 durable flag가 아니다. |
| `src/services/discord/inflight/clear_store/snapshot.rs:14,131` `for_native_snapshot_with_detail/clear_snapshot_checked` | DebtRow/ProjectionClear. 기존 파일 잠금 아래 nonempty hosted execution nonce의 일치/불일치를 검사한다. row None/own/foreign 값은 기존 inflight snapshot에서만 가져온다. native wrapper는 test-only다. |
| `src/services/discord/turn_bridge/native_input.rs:19,50,64` `state/reject_legacy_body/neutral_exit` | PortNeutral. exact episode와 보관된 같은 actor Arc에 한해 Legacy/Pending/Settled를 반환한다. review.neutral bool은 중립 stream exit의 실행 여부다. actor 주소는 Arc가 살아 있는 동안의 query 비교에만 사용하며 정리 권한으로 저장하지 않는다. |
| `src/services/discord/turn_finalizer/terminal_handler.rs:68` `handle_terminal` | NativePortBarrier. 등록된 primary의 legacy Complete/GateTimeout은 Deferred이며 result/cancel로 위조하지 않는다. |

PortTransferred는 durable 결과+내 lease 없음+foreign row의 조합에서 debt만 닫는 ② 목표다. 모델은 안전성/진행성 목표를 비교하지만 이 조각의 production drive는 해당 종결을 구현하지 않는다. WARN/새 observer/새 ticket replay/lost wake는 사용자 11-14 범위 축소이며 과거 cfg를 운영 수용 증거로 세지 않는다.


## B2b-2 ② P2 수리 (기준 2baf0806f)

| 진입점 | 행동·값 근거 |
|---|---|
| `actor_state.rs::submit_native` → `NativeCleanup::prepare_notice` | Result durable → outbox enqueue → withdrawal ack를 actor 메시지 제출 전에 수행한다. 2초 제한은 PG 대기만 Err로 돌리고 lease/result 완료를 만들지 않는다. |
| `actor_state.rs::contain_panic` | Terminal과 Native가 같은 helper로 panic payload를 &str/String으로 보존한다. 실패는 ack의 Err/기존 Terminal outcome으로 돌아가며 actor 수명과 분리한다. |
| `terminal_handler.rs::NativeCleanup::drive` | durable exact outcome이 없으면 거절. captured actor가 lease를 보유하지 않고 row가 captured episode/runtime/start/offset과 다르면 cleanup debt만 ack한다. row 삭제·wake·global_active 변경0이다. |
| `NativeCleanup::row_snapshot` | readable foreign row는 None, unreadable은 Err. successor lease가 있고 row가 없으면 foreign으로 취급한다. successor가 없는 단순 row 부재는 기존 captured fallback과 normal clear/wake를 유지한다. 모델 parentLease0/2와 row false는 앞의 no-own-lease/foreign proof를 추상화하며 독립 저장 값을 신설하지 않는다. |

기본 `native_prepare::enabled=false`는 유지한다. boot/custody-only/old generation/인접 종료/e2e 및 운영 증거 공급 포트는 이 ②에 포함하지 않는다. 기존 ①의 barrier와 drive 소유 계약을 바꾸지 않았다.


## B2b 조각 1 — saved outcome / custody / boot (비활성)

기준 부모 `f8c80664eae4955f477e51ffbf86d554eb3980d5`. 기존 mutation 전수표를 대체하지 않는다. `native_prepare::enabled()` false이며 control/supply/activation은 이 조각 밖이다. `NativeInputAttribution`의 Consumer 전이와 기존 PortTransfer unreadable 보강은 아래 값·진입점에서만 가져온다.

| 코드 좌표 | 소비/변경과 값 | 모델 행동 |
|---|---|---|
| `src/services/tui_input/human_frame/history/v2/native.rs:248,274` | namespace/full registration/ticket, outcome None/Some, cleanup/notice false/true. 같은 원본 결과의 단조 병합; custody-only는 passive retained/settled, live slot 생성 0 | ConsumerAdopt |
| `src/services/discord/inflight/removal/native_input_custody.rs:5,37` | canonical + 기존 custody complete manifest/copy digest; unreadable/partial/contradictory는 Err, terminal record 보존 | ConsumerAdopt / ConsumerRepair |
| `src/services/discord/turn_finalizer/native_restore.rs:29,62` | 원본 generation>0 및 current 미만, saved proof/source/UUID/AnyKey true. 현재 fixture는 old=1,current=2, parentLease=0; 결과 미존재를 유지 | ConsumerAdopt / NativeBootNoFabrication / NativeBootNoLeaseRemint |
| `src/services/discord/runtime_bootstrap/native_input.rs:4`, `runtime_bootstrap.rs:299` | enabled 조건 아래 준비→기존 reaper→restore; 오류면 intake 전 복귀 | ConsumerAdopt→ConsumerReap→ConsumerSettle |
| `src/services/discord/inflight/removal/boot_reaper.rs:24,75,108` | 기존 prepare_before_reap 보존 오류/incomplete면 restore admission 보류. 기존 reaper의 exact loader row 삭제 소유 유지 | ConsumerReap / NativeBootFailureKeepsRow |
| `src/services/discord/turn_finalizer/native_restore.rs:131,154` | read-only current row/lease 재확인, original exact started/offset snapshot CAS만; successor row이면 cleanup debt만 닫음, row 삭제/wake/global_active 변경 0 | ConsumerSettle / PortTransferred |
| `src/services/discord/turn_finalizer/native_restore.rs:146,258,299` | Register barrier→actor 밖 PG 안내→Settle→cleanup ack→terminal custody 성공 확인. PG/보존 실패는 admission Pending | ConsumerSettle / NativeBootAdmission |
| `src/services/discord/turn_finalizer/native_prepare.rs:123`, `native_restore.rs:346,357` | channel의 모든 restored input settled 여부를 actor-local 조회. fixed switch false면 기존 경로 유지 | ConsumerOwnerDrive admission |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input/native_input.rs:121,139`, `owner_input.rs:664,677` | 기존 owner recover/drive 연결; boot admission 뒤 wake. own lease 추가 claim/전역 gate 없음 | ConsumerOwnerDrive |

기존 inflight age/root helper·active reconcile·owner recover·ledger key resolver의 의미 동일 leaf 이동은 소유 이전이 아니다. 결과 1회·옛 row만 삭제·후속 head 1회 fixture는 기존 reaper와 V2 owner Drive의 실제 진입점을 쓴다. unreadable은 실제 자료가 복구되어야 진행하며 시간으로 자료를 고치거나 결과를 합성하지 않는다. native idle WARN·ticket replay·lost-wake 감시는 사용자 범위 축소로 추가하지 않는다.


## 조각1 리뷰 P2 — receipt/source 채택 보강

| 코드/모델 값의 근거 | 행동 |
|---|---|
| `history/v2/native.rs::BootRecord,RestoredRecord,boot_records,native_boot_record` | namespace/full registration/full ticket에 붙은 optional entry/event facts 보존; 구형 없음은 None |
| 같은 파일 `RestoredRecord::decode,same_entry,merge_entry` | entry의 attempt nonce/frame/source identity 검증, 실제 key/UUID/kind event 집합 단조 병합·다른 owner 충돌 거절 |
| 같은 파일 `merge_source,adopt_native_records` | source None/Some, 다른 Some/Some; target/incarnation/pre_source 기대값. 오래된 None으로 bound 사실을 지우지 않음 |
| `NativeAttributionCustodyReceipt{Before,Fixed}.cfg` | preserved false/true에 따른 다음 동일 frame의 receipt 확인/진행; concrete events는 Rust에서 검증 |
| `NativeAttributionCustodySource{Before,Fixed}.cfg` | 건강한 None/Some copy 재채택 충돌/단조 채택; 기존 bool/전이만 재사용 |

이 보강은 lease/row/global_active 변경점을 추가하지 않는다. control의 실제 mutation cut point에 넣을 nonblocking storage commit 포트는 파일 승인 전이며, 기존 `Registry::transaction`의 blocking mutex/flock을 actor에 연결하지 않았다.


### 조각2 독립 보호 경계

| 진입점 | 모델 행동 | 실제 범위 |
|---|---|---|
| `turn_bridge/native_input.rs::state` → `turn_finalizer/native_prepare.rs::native_settled` → `handle(NativeSettled)` | PortNeutral | exact key·captured actor의 승계 종결 조회. successor row/lease/global_active 변경·wake 없음 |
| `turn_finalizer/reconcile.rs::reconcile` → `actor_state.rs::native_debt_pending` | ConsumerGc | 일반 phase/TTL만으로 미확정 boot debt 삭제 금지. 미결 barrier 또는 unsettled boot registration이 있으면 보존 |

이 두 입구는 Cancel/Terminate/clear/stop/operator release 권한 commit과 직접 row 삭제 custody를 대체하지 않는다. 해당 control 입구는 기존 전수표대로 남으며 아직 native policy 연결을 하지 않았다. 운영 활성화는 false다.
