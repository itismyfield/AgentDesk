# #8280 Herdr 채널 세션의 dcserver 재시작 후 이어 쓰기

기준 소스 `8470a8b911849b2570e00103fb9bee33c5d9c7eb`. #8260, #8261, #8222의 머지 결과가 포함되어 있다. 11월 20일 08:49 KST의 dcserver 재시작 뒤, 09:15 첫 Discord 메시지는 작업 중인 Claude 세션에 도달했으나 복원 배너, 복구 안내, 지난 대화 재주입도 따라왔다. 이 사건의 근거는 #8280에 기록된 로그 순서와 관찰이다. Registry 경고는 출력 라우팅에서 나고 DB selector는 입력 준비에서 읽힌다. 두 로그의 시간 순서만으로 전자가 후자를 호출했다고 볼 수 없다.

## 진입점 전수표

행은 호출 단위다. 경로와 줄 번호는 기준 소스에 고정한다. `git grep -n`으로 `restart_pending|reconcile_done|restore_inflight_turns|reconnect_restarted_herdr|attach_restarted_herdr_source|restore_tmux_watchers|load_hosted_execution_pg|list_local_herdr_rows_pg|resolve_session_target|owner_channel_for_tmux_session|register_tmux_channel|register_restored_core|restore_provider_session|db_provider_session_restored|session-restore` 및 복원 문구, 제어 명령, 하위 writer/receipt 호출을 찾고 선언과 호출 양쪽을 추적했다. 같은 hosted 행의 별도 소비자까지 표에 넣었다.

| ID | file:line | 현재 동작과 경계 |
|---|---|---|
| E01 | `src/cli/dcserver_restart_wait.rs:208`, `src/cli/dcserver_restart_marker.rs:93`, `src/services/discord/runtime_bootstrap/deferred_restart.rs:89`, `src/services/discord/mod.rs:699` | marker 요청, drain/종료 허가와 nonce 확인. `restart_pending`은 dcserver 종료의 증거다. |
| E02 | `src/services/discord/runtime_bootstrap/shared_data.rs:172`, `src/services/discord/runtime_bootstrap.rs:315`, `src/services/discord/runtime_bootstrap/framework_setup.rs:107` | 새 프로세스는 reconcile을 미완료로 시작하고, TUI relay와 Discord 복구 작업을 각각 기동한다. |
| E03 | `src/services/discord/runtime_bootstrap/recovery_flush.rs:43`, `src/services/discord/runtime_bootstrap/queued_recovery.rs:26`, `src/services/discord/runtime_bootstrap/queued_recovery.rs:210` | downtime catch-up → durable queue 및 inflight 복원. |
| E04 | `src/services/discord/runtime_bootstrap/recovery_flush.rs:73`, `src/services/discord/runtime_bootstrap/recovery_flush.rs:120`, `src/services/discord/runtime_bootstrap/recovery_flush.rs:135`, `src/services/discord/mod.rs:2625` | watcher 복원 시도 → reconcile 완료 → reconcile 중 도착한 입력 재구동. |
| E05 | `src/services/discord/tui_prompt_relay.rs:310`, `src/services/discord/tui_prompt_relay.rs:323`, `src/services/discord/tui_prompt_relay/rehydration.rs:250`, `src/services/discord/tui_prompt_relay/rehydration.rs:665` | Claude/Codex idle reader, pending synthetic 복원, provider별 Herdr 재연결 패스. 입력 reconcile과 독립적으로 진행된다. |
| E06 | `src/services/discord/recovery_engine/herdr_reader.rs:235`, `src/services/discord/recovery_engine/herdr_reader.rs:258`, `src/services/discord/recovery_engine/herdr_reader.rs:302`, `src/services/discord/recovery_engine/herdr_reader.rs:456` | 로컬 Pending/Bound/Retired 조회. 동일 행·nonce의 기존 관찰을 재사용하고, 미배치와 RootReplaced를 별도 처리한다. |
| E07 | `src/db/dispatched_sessions/hosted_execution.rs:237`, `src/db/dispatched_sessions/hosted_execution.rs:309`, `src/db/dispatched_sessions/hosted_execution.rs:333` | canonical provider/identity 또는 정확한 session key 조회. 로컬 행 열거와 미배치 Pending 열거. |
| E08 | `src/services/discord/recovery_engine/herdr_reader.rs:38`, `src/services/discord/recovery_engine/herdr_reader.rs:53`, `src/services/session_host/herdr_gate.rs:806`, `src/services/discord/recovery_engine/host_reconcile.rs:103` | 저장된 endpoint/pane을 읽어 root와 nonce가 확인된 provider 프로세스를 대조한다. 명시적 Missing과 Unknown을 구별한다. |
| E09 | `src/services/discord/tui_prompt_relay/herdr_source.rs:127`, `src/services/discord/tui_prompt_relay/herdr_source.rs:305`, `src/services/discord/tui_prompt_relay/herdr_source.rs:321`, `src/services/discord/tui_prompt_relay/herdr_source.rs:363` | nonce의 transcript/rollout을 복구하고 행을 재확인한다. Claude 자체 clear 대기, baseline 부재, 등록 거절을 구별한다. |
| E10 | `src/services/tui_prompt_dedupe/runtime_binding/pane_registration.rs:227`, `src/services/tui_prompt_dedupe/runtime_binding/pane_registration.rs:249`, `src/services/tui_prompt_dedupe/runtime_binding.rs:362` | source authority 아래 채널·binding mirror를 발행한다. 같은 source의 기존 reader cursor를 보존하는 호출 경로는 E09에 있다. |
| E11 | `src/services/tui_prompt_dedupe/runtime_binding.rs:189`, `src/services/tui_prompt_dedupe/runtime_binding.rs:449`, `src/services/discord/tui_prompt_relay.rs:746`, `src/services/discord/tui_prompt_relay.rs:794` | 관측 프롬프트/idle poll의 reverse lookup, mirror 등록·조회, drift 경고. Herdr 설정이 있는 관측 프롬프트에는 좁은 후보 예외가 있다. |
| E12 | `src/services/discord/idle_relay_drift.rs:458`, `src/services/discord/idle_relay_drift.rs:488`, `src/services/discord/idle_relay_drift.rs:537`, `src/db/dispatched_sessions.rs:82` | 레거시 idle 라우팅 drift 수리. DB/설정 후보와 pane/mirror를 검증한 뒤 권위 registry에 복구한다. |
| E13 | `src/services/discord/tmux_watcher_registry.rs:318`, `src/services/discord/tmux_watcher_registry.rs:331`, `src/services/engine_runtime/relay_state.rs:396`, `src/services/engine_runtime/relay_state.rs:562` | restored reverse owner 등록·해제·조회. live watcher claim이 우선하며 이 owner만으로 입력 권한을 얻지 않는다. |
| E14 | `src/services/discord/tui_prompt_relay/rehydration.rs:118`, `src/services/discord/tui_prompt_relay/rehydration.rs:501`, `src/services/discord/tui_prompt_relay/rehydration.rs:854`, `src/services/discord/tui_prompt_relay/rehydration/codex_marker.rs:235` | 레거시 marker/설정에 따른 mirror 복구·퇴거 및 Codex owner 복구. Herdr nonce 증거와 독립. |
| E15 | `src/services/discord/watchers/lifecycle/restore_retry.rs:83`, `src/services/discord/watchers/lifecycle/restore.rs:21`, `src/services/discord/watchers/lifecycle/restore_core.rs:3`, `src/services/discord/watchers/lifecycle/restore_support.rs:193` | boot watcher 후보를 골라 registry/core/저장 selector를 등록하는 별도 경로. |
| E16 | `src/services/discord/watchers/lifecycle/claims.rs:203`, `src/services/discord/watchers/lifecycle/claims.rs:247`, `src/services/discord/watchers/lifecycle/claims.rs:342`, `src/services/discord/watchers/lifecycle/claims.rs:370` | hosted admission, 재사용·교체·신규 watcher claim의 nonce 경계. |
| E17 | `src/services/discord/recovery_engine/restore_inflight.rs:98`, `src/services/discord/recovery_engine/restore_inflight.rs:120`, `src/services/discord/inflight/host_recovery_guard.rs:134`, `src/services/discord/recovery_engine/host_reconcile.rs:157` | 부팅 inflight 출력 이어 읽기와 세대 대조. 현재 턴의 출력 복구가 새 메시지 제출을 소유하지 않는다. |
| E18 | `src/services/discord/health.rs:712`, `src/services/discord/recovery_engine/manual_rebind/watcher_claim.rs:22`, `src/services/discord/health/recovery/watchdog_pass.rs:267`, `src/services/discord/health/relay_dead_reattach.rs:126` | 수동 rebind와 watchdog watcher 재부착. 기존 claim 유지/교체 규칙을 거친다. |
| E19 | `src/services/discord/router/intake_gate.rs:120`, `src/services/discord/router/intake_gate.rs:1139`, `src/services/discord/router/intake_gate/busy_inject.rs:232`, `src/services/discord/queue_io/input_owner/ingress.rs:181` | 첫 Discord intake, Busy 우회 차단, reconcile/restart 중 hold. |
| E20 | `src/services/discord/router/intake_queue_transaction.rs:43`, `src/services/discord/router/intake_queue_transaction.rs:339`, `src/services/discord/queue_io/input_owner/ingress.rs:396`, `src/services/discord/queue_io/input_owner/ingress.rs:466` | 사람·허용 봇 분류와 admission ticket, 같은 메시지의 소유권 등록. |
| E21 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:966`, `src/services/discord/herdr_terminate/root_replaced/preclaim.rs:14`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1011` | owner Idle 진입. RootReplaced preclaim → 동일 front claim → 사람 intake 또는 owner headless. |
| E22 | `src/services/discord/router/message_handler/intake_turn.rs:161`, `src/services/discord/router/message_handler/intake_turn.rs:968`, `src/services/discord/router/message_handler/intake_turn.rs:983`, `src/services/discord/router/message_handler/intake_turn.rs:1008` | route와 reset/clear를 처리한 뒤 채널 transition에서 early owner 판단 → 이어 쓰기 또는 DB 복원 준비 → mailbox claim. |
| E23 | `src/services/discord/router/message_handler/intake_turn/session_preparation.rs:16`, `src/services/discord/router/message_handler/intake_turn/session_preparation.rs:52`, `src/services/discord/adk_session.rs:376`, `src/services/discord/adk_session.rs:435` | cache miss의 DB selector 경로. DB가 비어야 live runtime binding fallback을 탄다. selector 발견을 resume 사유로 곧바로 기록한다. |
| E24 | `src/services/discord/session_runtime.rs:41`, `src/services/discord/session_runtime.rs:92`, `src/services/discord/session_runtime.rs:225`, `src/services/discord/session_runtime.rs:484` | provider selector cache, rebind, 채널 자동 복구, thread bootstrap. 채널에 row가 준비되지 않은 순간이 있다. |
| E25 | `src/services/discord/router/message_handler/headless_turn.rs:159`, `src/services/discord/router/message_handler/headless_turn.rs:660`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1049` | API·루틴·예약 등 owner headless 경로. 같은 pane을 대상으로 할 때 기존 실행의 진행 lease를 새 claim에 앞서 인수한다. |
| E26 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:72`, `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:326`, `src/services/discord/router/message_handler/intake_turn/owned_busy/missing_watch.rs:11`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:394` | Busy/Idle/Unknown 재관측, 기존 owner의 Busy 제출, Missing 전환과 Held/wake. |
| E27 | `src/services/discord/router/message_handler/provider_dispatch.rs:553`, `src/services/claude/herdr_turn.rs:267`, `src/services/claude/herdr_turn.rs:699`, `src/services/codex/herdr_turn.rs:131`, `src/services/codex/herdr_turn.rs:323` | provider별 Herdr executor가 실제 row를 다시 읽어 Bound 이어 쓰기와 launch 후보를 고른다. |
| E28 | `src/services/claude/herdr_turn.rs:320`, `src/services/claude/herdr_turn.rs:403`, `src/services/claude/herdr_turn.rs:434`, `src/services/codex/herdr_turn.rs:538`, `src/services/codex/herdr_turn.rs:639` | nonce/source가 유지된 follow-up, Claude 자체 clear/Pending, Codex follow-up과 컴포저 제출. |
| E29 | `src/services/discord/queue_io/input_owner/drive.rs:36`, `src/services/discord/queue_io/input_owner/drive.rs:88`, `src/services/discord/queue_io/input_owner/pump.rs:43`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1591` | 동일 원본/front와 frozen followers claim, wake, 물리 제출의 reserve/receipt 경계. |
| E30 | `src/services/discord/router/message_handler/intake_turn.rs:1325`, `src/services/discord/router/message_handler/intake_turn/retry_prompt.rs:22`, `src/services/discord/router/response_format.rs:58`, `src/services/discord/router/message_handler/intake_turn.rs:1374`, `src/services/discord/prompt_builder/channel_recent_context.rs:208` | durable retry 원문과 렌더링된 복원 문맥, 새 세션용 recent context의 별도 출처. |
| E31 | `src/services/discord/router/message_handler/intake_turn.rs:1356`, `src/services/discord/router/turn_start.rs:480`, `src/services/discord/router/turn_start.rs:535`, `src/services/discord/placeholder_live_events/session_panel.rs:21` | DB 사유 또는 launch 힌트에 따라 session lifecycle/panel 복원 표시가 발행되는 위치. |
| E32 | `src/services/discord/tmux_watcher/pre_emit_guard.rs:357`, `src/services/discord/turn_bridge/terminal_outcome_delivery.rs:419`, `src/services/discord/turn_bridge/terminal_outcome_delivery.rs:485`, `src/services/discord/turn_bridge/terminal_outcome_delivery/recovery_retry.rs:91`, `src/services/discord/turn_bridge/recovery_text.rs:898` | restart/retry 복구 안내·재주입의 별도 경로. 같은 문구를 사용하는 emitter별로 launch 확정과 연결한다. |
| E33 | `src/services/discord/tui_prompt_relay/session_rotation_settle/selector.rs:48`, `src/services/discord/tui_prompt_relay/session_rotation_settle/selector.rs:171`, `src/services/discord/tui_prompt_relay/session_rotation_settle/selector.rs:197`, `src/services/discord/tmux_watcher/provider_session_persistence.rs:60`, `src/services/discord/turn_bridge/completion_postlude/channel_writeback.rs:270` | #8261 in-pane clear의 현재 selector/후계 source를 따르는 cache/DB 쓰기 경로. 늦은 기존 source 발행 경합의 상대. |
| E34 | `src/services/discord/commands/control.rs:223`, `src/services/discord/commands/control.rs:544`, `src/services/discord/commands/control.rs:832`, `src/services/discord/commands/control/clear_teardown.rs:28`, `src/services/discord/commands/control/clear_teardown.rs:47` | reset 또는 clear 경계, provider selector 삭제와 clear epoch. |
| E35 | `src/services/discord/commands/control/clear_teardown.rs:63`, `src/services/discord/commands/control/clear_teardown.rs:148`, `src/services/discord/commands/control/clear_teardown/resume_fence.rs:15`, `src/services/discord/commands/control/herdr_recovery.rs:21` | 정확한 hosted 실행 clear 종료, resume fence, RootReplaced 전용 clear. |
| E36 | `src/services/discord/commands/control.rs:671`, `src/services/discord/commands/control.rs:743`, `src/services/discord/commands/control/clear_teardown.rs:243`, `src/services/discord/health/recovery/stop_judgement.rs:56`, `src/services/discord/health/recovery/runtime_stop.rs:65` | Discord stop과 런타임 stop 경계. 취소된 grant 뒤 늦은 reattach가 종료 상태를 부활시키면 안 된다. |
| E37 | `src/services/discord/commands/control.rs:769`, `src/server/routes/queue_api.rs:209`, `src/server/routes/agents_stop.rs:4`, `src/services/discord/health/recovery/runtime_lookup.rs:160`, `src/services/discord/health/recovery/channel_runtime.rs:265` | queued cancel/HTTP cancel/agent stop, preserve-watcher와 정확한 진행 턴 조회. |
| E38 | `src/services/discord/herdr_terminate/http_force.rs:32`, `src/services/discord/herdr_terminate.rs:147`, `src/services/discord/herdr_terminate.rs:218`, `src/services/discord/herdr_terminate.rs:225`, `src/services/discord/herdr_terminate.rs:412`, `src/services/discord/herdr_terminate.rs:692` | 강제 취소/Terminate exact target, 종료 증거 및 nonce CAS, 기존 lease 정산. |
| E39 | `src/services/discord/commands/restart/herdr.rs:188`, `src/services/discord/commands/restart/herdr.rs:392`, `src/services/discord/commands/restart/herdr.rs:471`, `src/services/herdr_launch.rs:405`, `src/services/herdr_launch.rs:785` | 명시적 provider `/restart` 종료→후계 launch와 nonce 생성. dcserver 재시작과 별도. |
| E40 | `src/services/discord/host_key_derivation.rs:60`, `src/services/discord/host_teardown_gate.rs:159`, `src/services/discord/host_teardown_gate.rs:218`, `src/services/session_host/resolve.rs:239`, `src/services/turn_host.rs:146` | canonical/legacy 키 후보와 teardown/host 분류. 이름, migration marker, inflight 단서만으로 입력을 열지 않는다. |
| E41 | `src/services/discord/recovery_engine/herdr_row_end.rs:112`, `src/services/discord/recovery_engine/herdr_row_end.rs:503`, `src/services/discord/recovery_engine/herdr_server_replaced.rs:62`, `src/services/discord/herdr_terminate/root_replaced.rs:282` | 행 종료와 RootReplaced의 증거·settlement·복구 gate. 기존 행으로의 키 전송을 금지한다. |
| E42 | `src/services/discord/tui_prompt_relay/herdr_source.rs:154`, `src/services/discord/tui_prompt_relay/herdr_source.rs:210`, `src/services/discord/tui_prompt_relay/synthetic_start.rs:2046`, `src/services/discord/tui_prompt_relay/synthetic_start/admission_fence.rs:98`, `src/services/discord/tui_prompt_relay/synthetic_start/pending_claim.rs:240` | 첫 launch 소스 발행, TUI 직접 입력 관찰 및 pending synthetic claim. Discord 첫 메시지의 재송신 경로가 아니다. |
| E43 | `src/services/discord/host_defer_gate.rs:193`, `src/services/discord/router/herdr_consent/admission.rs:140`, `src/services/discord/health/o_transition.rs:34`, `src/services/discord/health/o_transition/automatic.rs:184`, `src/services/discord/health/o_transition/codex_context.rs:332` | 같은 행을 쓰는 host 입장 gate, consent와 O-writer cutover. O reader 증거를 계속 존중한다. |
| E44 | `src/services/discord/turn_presence/activity/unbound/herdr_bound.rs:30`, `src/services/discord/turn_presence/activity/unbound/live.rs:394`, `src/services/tui_prompt_dedupe/hosted_source.rs:365`, `src/services/discord/queue_io/input_owner/claude_handoff.rs:546`, `src/services/discord/queue_io/input_owner/claude_handoff.rs:742` | active turn/source 판정, handoff의 exact hosted lookup. 같은 실행행이 있어도 진행 턴과 입력 효과는 별도 증거가 필요하다. |
| E45 | `src/services/session_host/session_record.rs:174`, `src/services/session_host/session_record.rs:193`, `src/services/session_resume.rs:1403`, `src/services/herdr_launch.rs:778` | CLI/서비스 resume 배치, 실행 기록, 후계 launch의 exact lookup. |
| E46 | `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:17`, `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:199`, `src/engine/ops/timeouts_ops/host_repair.rs:62`, `src/services/tmux_turn_liveness/cleanup_host.rs:57` | 디스패치 정리·timeout·유휴 청소도 같은 행을 보므로 분류 직후 죽음/퇴거를 제출 전에 다시 본다. |
| E47 | `src/services/discord/catch_up.rs:445`, `src/services/discord/catch_up/classification.rs:110`, `src/services/discord/catch_up/phase2.rs:274`, `src/services/discord/queue_io/input_owner/origin.rs:33` | 부팅 catch-up과 원본 메시지 중복 판정, 예약·루틴 등의 출처 복원. |
| E48 | `src/services/discord/session_transition.rs:23`, `src/services/discord/session_transition.rs:88`, `src/services/herdr_launch/clear_epoch.rs:57`, `src/services/herdr_launch/clear_epoch.rs:176`, `src/services/session_host/herdr_gate.rs:806`, `src/services/session_host/herdr_gate.rs:884` | transition lease, clear epoch와 effect permit, 동일 서버의 읽기와 종료 확인. |
| E49 | `src/services/herdr_launch/retire.rs:36`, `src/services/herdr_launch/retire.rs:71`, `src/services/herdr_launch/retire.rs:113`, `src/services/discord/recovery_engine/herdr_reader.rs:487` | Missing read 이후 exact retire와 hold 해제, 프로세스 로컬 Missing 표시. |
| E50 | `src/services/claude/herdr_turn/handoff.rs:49`, `src/services/codex/herdr_turn.rs:196`, `src/services/codex/herdr_turn/restart_intent.rs:29`, `src/services/herdr_launch/codex_resume.rs:11` | Claude 실제 resume selector/clear fence, Codex의 명시적 종료 후 resume intent와 native read boundary. 일반 Codex Herdr launch는 fresh다. |
| E51 | `src/services/claude/herdr_turn/first_input.rs:27`, `src/services/codex/herdr_turn/first_input.rs:28`, `src/services/discord/queue_io/input_owner/effect.rs:96`, `src/services/discord/queue_io/input_owner/held_watch.rs:92` | 소유자의 첫 입력 effect, 확정·미실행·불확실 결과 구분, 관찰 변화 후 재구동. |
| E52 | `src/services/discord/router/response_format/retry_prompt_context.rs:27`, `src/services/discord/router/response_format/retry_prompt_context.rs:59`, `src/services/discord/router/response_format/retry_prompt_context.rs:84`, `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:67` | 오래된 retry envelope와 소비 CAS, 렌더링, idle poll에서의 owner 소비. |

## 부팅과 첫 메시지가 만나는 경계

- **restart_pending과 일반 재시작.** E01의 marker/nonce는 dcserver 종료 요청에 속한다. 다음 부팅은 두 경우 모두 같은 hosted owner 조회로 들어간다. dcserver 세대만 바뀌고 Herdr 서버와 pane이 그대로라면 실행 nonce를 유지한다. 동시에 Herdr가 교체됐거나 OS가 재부팅됐다면 현재 서버 witness와 root/provider의 시작 증거를 다시 대조한다. 이전 provider `/restart` greeting과 새 PID의 존재는 각각 생존 판정의 근거가 될 수 없다.
- **첫 메시지가 먼저 도착.** reconcile이 닫혀 있는 동안 기존 ticket/front에 보관하고 boot wake 뒤 같은 원본으로 재개한다(E19–E21, E47). 일반 reconcile 완료 뒤에도 E05의 reader가 늦을 수 있다. 이때 inbox owner가 채널 transition lease 아래 한 개의 canonical 행을 조회해 기존 read-only probe와 재부착으로 합류한다. 같은 observation의 첫 성공 발행을 재사용한다. peer/standby와 현재 gateway 소유권을 잃은 경로에는 새 grant를 내지 않는다.
- **incarnation.** 키는 provider, bot namespace, 채널/스레드, logical key, DB 행 ID, 노드와 endpoint, pane ID, execution nonce, 서버 witness, root/provider process stamp다. 채널 transition 아래 clear epoch와 종료 gate를 확인한다. 조회나 source pin이 늦어지면 행·nonce·routing을 다시 읽고 CAS 전에 옛 결과를 버린다. 확인된 hosted 증거를 가진 reverse owner는 E13에 발행한다. dedupe mirror는 E10의 source 발행과 진단에 사용한다. 실제 watcher는 E16–E18의 claim으로 인수하며 이미 게시한 출력과 기록된 offset 및 진행 턴 lease를 보존한다.

## 생존 증거와 첫 메시지 판정

1. **실행 행 조회.** provider와 채널이 맞는 canonical `Bound` observation을 선택한다(E06–E08, E27, E40). 별칭은 exact session key와 canonical owner가 합의할 때 쓴다. `Pending`은 기존 startup/unplaced resolver로 넘긴다. `Retired`는 종료 출처와 clear 경계 확인을 거친다.
2. **workspace 이름과 pane.** 같은 로컬 endpoint의 snapshot에서 `logical_key#*` 이름이 정확히 하나이고 이름 nonce와 DB nonce가 일치하며 agent pane이 살아 있으면 현재 pane ID를 같은 실행 행에 기록하고 재부착한다(E06–E09). 이름이 복수이거나 nonce가 다르면 기존 endpoint·pane, PID/시작 stamp, transcript inode 및 binding log의 정밀 판정으로 내려간다. 행만 읽히거나 socket 관찰이 실패하면 같은 원본을 보관한다. 실행 정리에는 기존 Missing/Ended 증명과 nonce CAS를 사용한다. RootReplaced와 missing marker는 기존 회복 포트에 위임한다(E38–E41, E48–E49).
3. **transcript/rollout과 readiness.** 정밀 경로에서는 해당 nonce의 세션 ID와 파일 dev/inode, binding generation, current cursor를 대조한다. 재부착은 같은 실행의 현재 source와 cursor를 이어받는다(E09–E10, E28, E33, E44). mtime, 오래된 tail, DB selector는 생존을 증명하지 못한다. 현재 transcript와 화면으로 Idle/Busy/Unknown을 분류한다. Claude 자체 clear의 `AwaitingClear`는 #8261의 현재 selector와 #8260의 source 준비를 따른다. #8222의 압축 metadata 뒤 Idle 복원과 기존 composer guard를 사용한다. Codex는 현재 rollout과 native read boundary를 보존한다.
4. **이어 쓰기(사용자 11-20 20:1x 기준 개정).** 판정 기준은 'tmux와 같은 결과'다. 생존을 확인하면 곧바로 재부착(watcher claim·registry·진행 턴 lease 인수)하고, 진행 중 턴의 Busy owner grant도 새 세대가 다시 세운다. 그 뒤 첫 입력은 평소 herdr 채널과 같은 입력 규칙을 탄다(작업 중이면 평소 busy 주입, 유휴면 바로 제출). 'Idle 전용' 조건은 두지 않는다. 대기는 재부착이 끝나기 전과, 평소 경로도 막는 composer 상태(권한 모달 등)에서만 허용한다. 사용자에게 '보류 중'을 표시하는 것은 pane identity를 끝내 확인할 수 없는 경우(행만 있음·socket 오류)로 한정하고, 그 경우도 관찰이 바뀌면 재부착이나 복원으로 끝나야 한다. 증명 요구는 '죽었다고 판정해 정리할 때'에만 둔다. 이번 입력을 보내려고 살아 있는 실행의 종료나 교체를 시작하지 않는다.
5. **종료와 모호함.** Exact 종료 확인과 nonce CAS로 정산하고 현재 clear/reset/stop 및 queue custody를 다시 확인한다. boot가 동일 종료를 먼저 정산했으면 그 결과를 재사용한다. 행만 있고 위치·identity를 확인할 수 없는 경우, socket 오류, 부분 관찰, 충돌은 같은 원본의 Held를 유지한다. 관찰 변화 또는 기존 backstop으로 재평가하며 기한을 넘기면 사용자에게 보류 상태를 표시한다. 불확실한 pane에 키를 보내거나 타임아웃만으로 후계를 열면 유실 또는 중복 실행이 가능하다.

| 시작 상태 | 첫 입력 결과 | 복원 배너 / 복구 안내 / 지난 대화 재주입 |
|---|---|---|
| 살아 있는 유휴 pane | 같은 nonce 및 현재 source에 owner가 한 번 제출 | 0 / 0 / 0 |
| 살아 있는 작업 중 pane | 같은 pane에 즉시 재부착하고 평소 Busy 주입으로 제출. 평소 composer 차단을 적용 | 0 / 0 / 0 |
| 죽은 pane | 종료를 확인한 뒤 검증한 native source로 새 프로세스 resume, 새 nonce에 한 번 제출 | 1 / 1 / 1. 재주입은 보존된 이전 대화가 있는 수용 시험 기준 |
| 실행 행만 확인됨 | 키 전송 없이 Pending resolver 또는 exact 재관측. 생존이나 종료 확인을 거쳐 위 경로로 합류 | 판정 전 0 / 0 / 0 |

### 실제 resume에서만 복원 표시

`Continue`에는 기존 실행과 선택한 현재 source를, `EndedRecovery`에는 predecessor의 종료 증거와 같은 front를 실어 보낸다. 현재 clear/goal/reset, worktree 격리 및 route veto를 먼저 적용한다. 단순 selector 조회로 배너를 결정하던 E23/E31 분기를 실제 native resume 결과의 소비로 교체한다. provider가 새 프로세스로 resume했다는 launch evidence와 새 nonce를 확인하면 해당 front에 배너와 복구 안내를 한 번씩 연결한다. 지난 대화 층은 launch의 세션·파일 경계를 검증한 다음 첫 물리 prompt를 준비하기 직전에 기존 builder로 합친다. 라이브 이어 쓰기에 오래된 retry envelope가 따라오면 복원 층만 제외하고 원문, 답장 인용, 첨부, frozen followers를 전달한다. E30·E32·E52는 실제 적용이 확인될 때 복원 문맥 소비를 CAS로 정산한다. 이전 턴의 실제 실패와 O-writer 출력은 각 소유자가 정산한다.

Claude는 E50의 retired native source 확인과 clear fence를 사용한다. Codex의 일반 Herdr launch는 DB selector가 있어도 fresh이며 현재 resume는 명시적 `/restart` greeting에 한정된다. #8280은 exact predecessor의 종료 증거, 현재 owner의 단일 rollout, 검증된 inode/offset과 현재 front를 가진 경우에만 별도의 좁은 허가를 추가한다. 기존 `ResumeStart`의 캡처·재검사 로직을 공유한다. 새 greeting을 만들지 않으며 일반 Codex fresh와 명시적 clear/reset을 그대로 적용한다. `Retired`만으로 종료 경위를 증명할 수 없고 `recovered_missing`은 프로세스 로컬이다. 두 번째 재시작에도 이어 가려면 기존 queue custody에 predecessor/front의 exact 종료 증거를 보존한다. 읽히는 옛 형식은 현재 경계와 과거 종료를 재검증할 수 있을 때만 허가한다. 경위를 증명할 수 없는 generic Retired/오래된 selector와 유효한 source가 없는 경우에는 기존 fresh 또는 명시적 보류를 따른다. 실제 resume이 발생한 경우에 복원 신호를 게시한다.

### 정확히 한 번의 효과

E20의 동일 ticket과 E29의 동일 front·frozen followers를 사용한다. Boot reader와 watcher는 관찰·등록·wake를 맡는다. writer lane의 reserve → 현재 행과 epoch 재검사 → provider first-key gate → 기존 효과 결산 순서를 적용한다. 첫 키 전 죽음이면 `NotSent`로 판정해 동일 원본을 후계가 받는다. stop/clear/cancel이 먼저 확정되면 기존 제어 소유자가 결산한다. 제출이 확인된 원본은 재전송하지 않는다. Enter 결과를 모를 때는 기존 hold/receipt, nonce, prompt 해시와 source offset에서 제출 증거를 확인한다. 기존 Herdr 포트는 Enter와 내구성 receipt의 원자성을 보장하지 않는다. 증거가 영구히 사라지는 경우 사용자에게 보류를 표시한다. 모델의 liveness는 판독·wake·결산이 언젠가 진행되는 첫 입력에 해당한다.

## 입력 출처와 소유자

| 입력/관측 | 출처와 소유자 | #8280 경계 |
|---|---|---|
| Discord 사람의 live·답장·첨부·thread | 원 작성자, Discord message ID와 채널의 input owner(E19–E22) | 현재 front에 이어 쓰기/복원 판정 |
| 허용 bot/announce, 차단 bot | bot과 원래 메시지 ID, 허용된 입력은 기존 mailbox 정책(E20, E47) | 허용분만 적용. 출력 relay 재수락 금지 |
| Catch-up, durable 큐/재시도, frozen followers | 원 작성자와 정규화된 admission ticket, 기존 큐 owner(E03, E29–E30, E47, E51–E52) | 같은 원본·순서 유지. 복원 문맥은 실제 resume에서 적용 |
| API/HTTP, iMessage, 루틴·예약 시작 | 호출·발화·실행 ID, 기존 ticket과 owner headless(E21, E25, E47) | 해당 Herdr 채널의 TUI로 라우팅할 때 공통 판정 |
| TUI 직접 제출, 로컬 slash, 미제출 draft | pane의 사람 또는 provider, synthetic/output owner가 관찰(E11, E42) | 관찰 턴 재송신 금지. draft는 readiness blocker |
| 부팅 inflight / watcher 재부착 | 이미 제출한 턴의 기존 watcher/O-writer(E16–E18, E43–E44) | 출력 연결을 회복. 새 입력의 소유권은 mailbox에 있음 |
| provider `/restart` greeting과 복구 wake | lifecycle operation/seed 및 기존 enqueue 원본(E39, E47–E51) | greeting은 기존 허가. wake는 같은 front만 구동 |
| Cancel·Terminate·clear·stop | 사용자/API의 제어 operation, exact turn/host owner(E34–E41) | 같은 transition/epoch를 소비하고 이전 관찰을 무효화 |

## 두 안 비교

| 안 | 흐름 | 비용과 위험 |
|---|---|---|
| 부팅 때 registry/core 선충전 | provider별 전체 행을 순회해 endpoint/process/source를 검증하고 reverse owner와 selector를 발행 | 정상 부팅 뒤 첫 입력에 추가 조회가 적다. 채널별 probe 비용과 startup 지연을 부팅에서 부담한다. 첫 입력이 앞서거나 warm cache가 낡으면 별도 판정이 필요하다. |
| 첫 메시지에서 확인 | 기존 boot reader를 유지하고 miss 또는 stale cache에서 exact 행을 찾아 같은 재부착을 호출 | 사용할 채널에서만 추가 조회를 부담한다. 첫 메시지 지연과 경합은 transition/nonce와 wake로 처리한다. 이 안을 선택한다. 부팅이 이미 발행한 현재 증거는 재사용하고 첫 키에서 다시 확인한다. |

## 구현 조각과 수용 시험

prod 수와 순증은 구현 계획 추정이다. giant의 새 코드는 인자 전달과 호출 교체 또는 기존 블록의 child 이동으로 제한한다.

| 조각 | 경계와 배치 | 운영 배선 | prod 파일 / 순증 추정 | 수용 시험 |
|---|---|---|---:|---|
| 1. 정확한 실행 판정·발행 | `recovery_engine` child가 E06–E13, E48–E49의 read/attach/registry를 판단. 기존 transition 및 source authority 재사용 | 비활성 착지. 기존 부팅 결과와 side effect 순서를 보존한다. 첫 입력의 판정 호출과 확인된 Herdr reverse owner 발행, 종료 증거의 입력 소비는 꺼져 있다. | 6 / +330 | Claude/Codex idle, busy, Missing, unplaced/Unknown, provider exited, endpoint/root/nonce 교체, boot와 intake 두 순서, 이전 nonce publish 0 |
| 2. 같은 입력의 owner 경로 | `session_preparation`과 owner child에서 E19–E29, E51을 연결한다. 이름 기반 Bound 위치 갱신, registry·watcher·진행 턴 lease 인수, Unproven 원본 보존을 배선한다. | 살아 있는 경로를 활성화한다. 새 headless claim보다 기존 lease를 먼저 인수한다. 같은 원본을 평소 Busy 주입 또는 유휴 제출로 보내고 이어 쓰기에서 복원 문맥을 선택하지 않는다. | 승인 산정 누적 약 22 / +895 | 유휴/Busy 같은 pane에 1회 전달·답 게시, headless claim 경합, Unproven, 이름 충돌, 첫 키 전 사망/stop/reset, AckUnknown 무재전송 |
| 3. 종료 resume과 표시 | 종료 provenance와 launch receipt는 child에 둔다. Claude 현재 resume와 Codex `ResumeStart` 검증 재사용, E30–E33/E50/E52의 render와 emit에 연결 | 종료를 확인한 뒤 새 프로세스 resume과 그에 따른 복원 문맥·안내·배너를 연결한다. 경계 없는 구형 retry 문맥의 복원 층도 여기서 다룬다. | 8 / +470 | 확인된 종료 뒤 Claude/Codex 새 nonce와 표시 각 1회, 옛 retry envelope, old selector만 있는 행, 두 번째 boot, 명시적 clear/ordinary fresh/provider restart 대조 |
| **착지** | 조각 1, 조각 2 및 Unproven 흡수를 같은 묶음으로 착지 | 이 묶음에서 살아 있는 실행을 이어 쓴다. | 조각당 PLAN_READY +1200, 상한 30 prod 파일 / +1500 | 종료 후 실제 resume과 표시를 조각 3에서 검증 |

E2E 칸은 가능하다. `src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_e2e_tests.rs:457`의 실제 kickoff를 `RelayE2eHarness`, isolated PG, mock Discord 및 exact Herdr rig로 확장한다. 같은 pane과 내구성 큐/source를 새 shared runtime에 연결한다. Busy는 boot reader와 첫 intake의 순서를 barrier로 뒤집고 이전 턴 종료 전후를 관찰한다. Codex는 mock rollout/hook fixtures를 재사용한다. Discord 캡처, provider 입력 프레임, receipt와 새 launch frame을 함께 확인한다. 죽은 pane은 old key send 0, 새 nonce에 1회, 세 복원 효과를 각각 검사한다. Native Herdr 교차 프로세스 시험은 격리 래퍼와 throwaway PG를 쓰는 별도 프로필이다. Rust/PG/E2E는 구현 조각의 수용 시험이다.

## 리뷰 집중 항목

- **경계 시각.** shutdown nonce, 새 dcserver 세대, Herdr peer/server witness, process start, native source offset을 비교한다. reconcile 완료 전후와 첫 key 경계를 함께 본다.
- **동시성.** boot/reader/watcher/intake의 publication과 clear/stop/Terminate fence를 확인한다. 조회 I/O를 기다리는 동안 source lock을 잡지 않는다. CAS 패배는 같은 원본의 재관측으로 처리한다.
- **옛 형식.** canonical/legacy row, 미배치 Pending, inode 미기록, 옛 retry envelope, process-local Missing 기록과 두 번째 재시작을 확인한다. 늦은 watcher가 clear/fresh marker를 덮지 않게 한다.
- **잘못된 생존 판정.** shell만 남은 pane, PID·숫자 pane·socket 재사용, 이전 transcript mtime, unreadable screen, 사용자 draft, Busy 증거 뒤 첫 키 전 사망을 검토한다.

## 상태 차트

[기존 HerdrMissingBound 모델의 #8280 결과](../../statecharts/session-lifecycle/herdr-live-reattach.report.md)
