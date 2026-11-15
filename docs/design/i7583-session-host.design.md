# #7583 Herdr Windows와 공통 세션 호스트 — 설계 1

<!-- doc-path-refs: external 2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01/ crates/herdr-core/ src/SKILL.md -->
<!-- doc-path-refs: proposed src/services/campaign_execution/tui_runtime.rs -->

기준 코드: **origin/main `1974cff8a67bf5d1db041abfdd4d615c518ee104`**.
브랜치 `design/i7583`, 2026-11-15, win-desktop WSL 조사. 이 문서와 상태 모델만 작성했다.
Rust·운영 DB/store·설정·설치·서비스는 바꾸지 않았다. [이슈 원문](https://github.com/itismyfield/AgentDesk/issues/7583)과
11-02 보류 댓글을 읽었고, 이번 착수 브리프의 선행 작업 완료 결정을 입력으로 삼았다.

## 결정

**A: 기존 Herdr TUI를 Windows 관리 세션 호스트로 확장한다.** Unix tmux 선택과 현재 입출력 소유권은 유지한다.
`InteractiveSessionHost`는 실제 호스트 효과를 내는 공통 경계로 확장하며, provider 준비·큐 원문 소유·
실행 신원·O 게시 권위는 해당 기존 소유자에게 둔다. Core는 같은 포트의 별도 구현체로 F4에 연결한다.
Windows TUI 서버에 대한 dcserver 재연결은 **terminal attach를 호출하지 않는다**. 살아 있는 같은
서버의 API에 새 연결을 열어 저장된 pane/root/provider/nonce를 확인하고 native source와 O writer를 재채택한다.
이 순서는 기존 Unix 경로에 이미 있으며, Windows OS 증거와 부팅 배선을 실측해야 활성화할 수 있다.

| 안 | 흐름·비용 | 위험·판정 |
|---|---|---|
| A: Herdr TUI + Windows named pipe | 기존 Pending→create→location→expected→Bound, 입력 gate·native JSONL·재시작 reader를 재사용. 전송·프로세스 신원·Windows launch/hook·boot 포트 필요 | 기존 전체 세션 계약과 가장 가깝다. terminal attach/실행 중 서버 교체가 없어도 dcserver만 재시작하면 API 재채택 가능. **권고**; Job 수명/nonce 증명이 U04/U05에서 안 되면 활성화 보류 |
| B: Windows를 바로 Herdr Core로 | `hc run`/terminal id 저장→send/wait/read→F4 결과 수집. named pipe·ConPTY·holder 분리의 이점 | Discord의 Claude/Codex native source/세션 이어 열기·입력 receipt·root 증거를 새로 결합해야 한다. preview API와 상태 보고만으로 native turn/O 완료를 대신할 수 없음. Windows TUI 대체의 선행 선택으로 쓰지 않고 F4 전용 후속 구현 |

새 운영자 명령·수동 전환 스위치·운영자 런북을 만들지 않는다. 실측용 임시 인스턴스 명령은 제품 운용 조건이 아니다.
불확실한 송신을 자동 재전송하거나, 모르는 PID를 정리하는 권한도 추가하지 않는다.

## 이미 main에 있는 것과 남은 수용 조건

| DoD | main 증거 | 남은 것 |
|---|---|---|
| Windows Claude/Codex Herdr 및 재시작 복구 | Unix 실행은 `src/services/claude/herdr_turn.rs`, `src/services/codex/herdr_turn.rs:149`; 재채택 `src/services/discord/tui_prompt_relay/herdr_source.rs:274` | Windows OS/pipe/launch/native hook/source/부팅/취소를 실제로 통과시키고 Windows CI 및 win 실측. Qwen은 U10 뒤 지원 가능 범위를 결정 |
| ProcessBackend/Host/Kind 삭제 | `src/services/session_backend.rs:166`, `src/services/session_host/process_host.rs:12`, `src/domain/host_observation.rs:6`에 남음 | 호출자·대역부터 삭제한 뒤 registry/enum 삭제. 실행 소스/시험에 구체 구현이나 pipe mode 0; 역사 문서의 문자열은 삭제 대상 아님 |
| 공통 trait, 우회 호출 0 | `src/services/session_host/traits.rs:9`에는 이미 관찰·send_text·send_keys·interrupt 있음. `src/services/session_host/legacy_collapse.rs`와 `src/services/provider/session_probe.rs`는 일부 경유 | create/recover/terminate 추가, 실제 입력·종료·복구와 잔여 소비자 이관. 아래 전수표의 모든 관리 세션 행을 처리해야 함 |
| 채널별 tmux/Herdr, 기본값 | `src/config/session_hosts.rs:350`의 채널 endpoint와 `src/services/turn_host.rs:155`, 두 live turn switch 있음 | 기존 `runtime.herdr_turn_enabled` 자리에서 선택 표현 확장. 기존 bool/미지정 동작 보존, 새 운영 명령 0 |
| Core F4 실행/결과 수집 | 현행 `HostKind`는 Tmux/Process/Herdr뿐(`src/domain/host_observation.rs:4`) | Core adapter, F4 소비자, 첫 지시·OSC·parent wait·재부팅 소멸 실측. **아직 충족 아님** |

이슈 본문의 `3c598ff` 좌표를 사용하지 않는다. 본문에서 상대적으로 적은 소스 경로는 아래 표의
`src/services/` 접두사 기준이다. 모든 TSV coordinate는 저장소 루트에서 시작한다.

## 조사 원본과 전수표 사용법

[진입점/도메인 표](i7583/entrypoints.md)와 네 TSV를 함께 검토한다. Process 계열 **603행/107파일**,
HostKind/RuntimeHandoffKind/turn 선택·marker **2079행/498파일**, 호스트 호출 후보 **2932행/655파일**,
Windows gate/OS 가정 **1446행/227파일**을 저장했다. 단순 문자열 grep의 숫자를 실제 prod 호출 수나 시험 수라고
보고하지 않는다. 정의·import·inline 시험을 남긴 원본이고, 관리 세션 효과 표와 대조해 누락을 막는다.
[audited-bypasses.tsv](i7583/audited-bypasses.tsv)는 후보에서 주석·선언·명명된 시험을 걸러낸 보수적인
효과 지점 표다. adapter 내부와 trait을 이미 경유하는 소비자는 따로 판정하며, 비관리 실행·시험 도구는 제외 근거를 붙인다.
DoD 3은 이 표의 `BYPASS`를 단순 삭제해 0으로 만드는 것이 아니라 **호출 체인이 공통 trait에 닿는 증거**로 닫는다.

### ProcessBackend: 지울 것과 남길 것

| 효과 | 현재 main 진입점(file:line) | 처리 |
|---|---|---|
| Claude 선택/생성/재사용 | `src/services/claude.rs:754`, `src/services/claude.rs:794`, `src/services/claude.rs:810`; `src/services/claude/process_session_launch.rs:50`, `src/services/claude/process_session_launch.rs:131`, `src/services/claude/process_session_launch.rs:148` | Windows host 선택이 먼저 완성된 뒤 direct child 생성·레지스트리 재사용 경로 삭제 |
| Codex 선택/생성/재사용 | `src/services/codex/command_streaming.rs:184`; `src/services/codex/process_session_launch.rs:45`, `src/services/codex/process_session_launch.rs:137`, `src/services/codex/process_session_launch.rs:147`; `src/services/codex.rs:107` | Process runtime label과 fallback 삭제, direct headless 실행은 구분해 유지 |
| Qwen 선택/생성/재사용 | `src/services/qwen.rs:341`, `src/services/qwen.rs:357`; `src/services/qwen/session_lifecycle.rs:322`, `src/services/qwen/session_lifecycle.rs:352`, `src/services/qwen/session_lifecycle.rs:441` | process 절반만 삭제. tmux wrapper/session lifecycle은 남김. Windows Qwen 미지원 시 입력 이전 명시적 오류 |
| pipe spawn·write·alive | `src/services/session_backend.rs:56`, `src/services/session_backend.rs:78`, `src/services/session_backend.rs:153`, `src/services/session_backend.rs:166`, `src/services/session_backend.rs:277`, `src/services/session_backend.rs:341`, `src/services/session_backend.rs:548` | SessionConfig/Handle/Backend와 registry·active guard·stopped ring 삭제 |
| 관찰/capture | `src/services/session_host/process_host.rs:21`; `src/services/session_backend/output.rs:15`; `src/services/provider/session_probe.rs:264`; `src/server/routes/agents_crud.rs:1262`; `src/services/discord/turn_presence/activity/unbound/live.rs:62` | ProcessHost/캡처 삭제, 공통 가짜 호스트로 시험 교체. 단순 살아 있음 대역에 실제 child spawn을 쓰지 않음 |
| 중단/강제 종료 | `src/services/discord/turn_bridge/tmux_runtime.rs:606`; `src/services/discord/turn_bridge/tmux_runtime/process_backend_cancel.rs:13`, `src/services/discord/turn_bridge/tmux_runtime/process_backend_cancel.rs:123`; `src/services/discord/turn_bridge/tmux_runtime/process_table.rs:34`; `src/services/session_backend.rs:398`, `src/services/session_backend.rs:478`, `src/services/session_backend.rs:575` | registry/PID 전용 분기 삭제. tmux·direct headless와 공유한 process-tree 식별 유틸은 호출자를 확인해 유지 |
| clear/reset/cleanup | `src/services/discord/commands/control/managed_reset.rs:57`, `src/services/discord/commands/control/managed_reset.rs:74`, `src/services/discord/commands/control/managed_reset.rs:104`; `src/services/discord/commands/control/clear_teardown.rs:99`; `src/services/discord/commands/control/provider_reset.rs:53`; `src/services/discord/admin_host_guard.rs:283`, `src/services/discord/admin_host_guard.rs:381`; `src/services/session_backend/auth_profiles.rs:12` | 검증된 호스트 대상으로 이관 후 Process 전용 정리 삭제. API 의미/기존 권한 유지 |
| 결과/재시작 분기 | `src/services/agent_protocol.rs:72`, `src/services/agent_protocol.rs:129`, `src/services/agent_protocol.rs:308`; `src/services/discord/turn_bridge/runtime_handoff_loop.rs:831`, `src/services/discord/turn_bridge/runtime_handoff_loop.rs:871`; `src/services/discord/recovery_engine.rs:408`; `src/services/discord/inflight/model/serde_adapters.rs:25`; `src/services/tui_turn_state.rs:231` | ProcessReady/handoff/variant 삭제. 오래된 raw runtime을 tmux로 추정하지 않음 |
| wrapper CLI pipe | `src/cli/args/command_args.rs:222`, `src/cli/args/command_args.rs:268`, `src/cli/args/command_args.rs:305`; `src/cli/run.rs:421`, `src/cli/run.rs:452`, `src/cli/run.rs:495`; `src/services/tmux_wrapper.rs:375`; `src/services/codex_tmux_wrapper/input.rs:67`; `src/services/qwen_tmux_wrapper.rs:128`; `scripts/smoke-tmux-wrapper-scenarios.sh:387` | `InputMode::Pipe`/argument와 전용 시험 삭제. Unix FIFO와 wrapper child stdin은 유지 |
| 시험 대역 | `src/services/session_backend.rs:93`; `src/services/discord/admin_host_guard_tests.rs`; `commands/control/codex_clear_relaunch_tests*`; `src/services/discord/turn_bridge/tmux_runtime/process_force_kill_tests.rs`; `headless_turn/process_mailbox/*tests.rs`; 원본 TSV 전 행 | old registry를 살아 있음 증거로 쓰는 대역을 injected HostEvidence/FakeHost/가짜 OS stamp로 치환. Process 기능 자체 시험만 삭제하고 해당 consumer의 거절·보존 시험은 유지 |

**공유 코드는 지우지 않는다.** `src/services/session_backend.rs:26` ReadOutputFailure, `src/services/session_backend.rs:855` stream_line 파서,
`src/services/session_backend.rs:868` terminal_usage, `src/services/session_backend.rs:887` 이후 JSONL tail/harvest는 tmux wrapper도 사용한다.
`src/services/session_backend/stream_line.rs`, `src/services/session_backend/terminal_usage.rs`와 공통 read-output은 남기고 모듈 이름 정리는 별도 필요가 없으면 하지 않는다.
`src/services/session_backend/auth_profiles.rs`는 registry 의존을 제거하되 tmux auth marker 비교/재시작 준비 계약을 해당 provider 소유에 남긴다.
`src/services/tmux_diagnostics.rs:279` FIFO 오류·output source 추적과 stdout parsing을 Process 전용 stdin 오류(`src/services/tmux_diagnostics.rs:305`)와 구분한다.
`src/services/claude/process_session_launch.rs:183`의 `without_resume_arg`는 `src/services/claude.rs:2430`에서 공유하므로 provider argv 준비로 먼저 옮긴다.
직접 실행(headless 일회성), process identity/signal, native JSONL codec를 이름에 process가 있다는 이유로 지우지 않는다.

### 공통 trait 우회 경로의 기준표

아래는 기능별 소유 진입점이고, 각 소유자의 leaf와 형제 호출 좌표는 TSV의 개별 행으로 펼쳐져 있다.
`InteractiveSessionHost` 구현과 그 안의 OS primitive는 우회가 아니다. 상위 façade는 남아도 물리 효과가 trait을
통과해야 한다. 예컨대 `InputTransport`는 plan executor port로 남되 구현이 독자 OS write를 해서는 안 된다.

| 구역 | 현재 우회/소비자 좌표 | 최종 경계와 수용 시험 |
|---|---|---|
| tmux 생성/재생성 | `src/services/claude/tui_session_launch.rs:100`; `src/services/claude.rs:2699`; `src/services/codex/tui_session_launch/first_input.rs:53`, `src/services/codex/tui_session_launch/first_input.rs:127`; `src/services/codex.rs:1613`; `src/services/qwen/session_lifecycle.rs:220`; `src/services/opencode.rs:1084`; `src/services/discord/tmux_watcher/placeholder_reclaim.rs:169` | host.create；provider argv·cwd·identity는 준비된 값으로 전달. 원래 이름/범위, recreate 전 dead/identity 증거 유지 |
| Herdr 생성 | `src/services/claude/herdr_turn/handoff.rs:127`; `src/services/claude/herdr_turn/never_attached.rs:21`; `src/services/codex/herdr_turn.rs:223`, `src/services/codex/herdr_turn.rs:636`; `src/services/herdr_launch.rs:378`, `src/services/herdr_launch.rs:515` | host.create→기존 CAS/nonce/permit 프로토콜. 생성 ACK와 Bound를 합치지 않음 |
| FIFO 입력 | `src/services/claude.rs:2795`; `src/services/codex.rs:1732`, `src/services/codex.rs:1785`; `src/services/qwen/followup_reader.rs:15` | wrapper transport를 TmuxHost의 typed submit으로 이관. FIFO 포맷과 partial-write 오분류 금지 |
| Claude 키/붙여넣기 | `src/services/claude_tui/host_input.rs:164`, `src/services/claude_tui/host_input.rs:252`, `src/services/claude_tui/host_input.rs:548`, `src/services/claude_tui/host_input.rs:559`; `src/services/claude_tui/busy_inject.rs:357`; `src/services/claude_tui/busy_inject/stash.rs:46`; `src/services/claude_tui/input.rs:259`; `src/services/tui_input/actor/pane.rs:109`; `src/services/tui_input/submission_tmux.rs:82` | 기존 plan·draft 보존·first-key/owner gate 이후 host.submit. stash 이후 오류는 WriteUncertain 유지 |
| Codex 키/붙여넣기 | `src/services/codex_tui/host_input.rs:59`, `src/services/codex_tui/host_input.rs:315`, `src/services/codex_tui/host_input.rs:597`; `src/services/codex_tui/busy_inject.rs:216`; `src/services/discord/queue_io/input_owner/effect/codex.rs:192`; `src/services/codex/herdr_turn/busy.rs:129` | tmux/Herdr 동일 포트, provider별 Enter/receipt 규칙 유지 |
| 설정·passthrough·handoff | `src/services/claude/session_handoff.rs:110`, `src/services/claude/session_handoff.rs:123`; `src/services/discord/commands/tui_passthrough/herdr_settings.rs:122`, `src/services/discord/commands/tui_passthrough/herdr_settings.rs:245`, `src/services/discord/commands/tui_passthrough/herdr_settings.rs:318`; `src/services/discord/tmux_watcher/terminal_token_update.rs:113`; `src/services/discord/health/turn_deliver/inject/herdr.rs:111` | control/system 출처로 명시하고 user原文으로 재분류하지 않음. `/exit`·`/clear`도 같은 gated submit |
| 관찰의 외부 소비자 | `src/services/tmux_diagnostics.rs:12`, `src/services/tmux_diagnostics.rs:37`, `src/services/tmux_diagnostics.rs:134`; `src/services/tmux_common.rs:543`; `src/services/session_activity.rs:57`; `src/services/provider.rs:847`; `src/services/discord/session_runtime.rs:360`; `src/services/discord/turn_presence/activity.rs:1083`; `src/server/routes/agents.rs:415`; 전체 TSV | presence/liveness/cwd/pid/screen은 host를 통과. 각 현재 bool collapse의 ProbeError 처리를 보존하는 adapter만 허용 |
| 사용자 Cancel/Stop | `src/services/discord/commands/stop.rs:67`; `src/services/discord/commands/text_commands/native_stop.rs:38`; `src/services/discord/turn_bridge/tmux_runtime.rs:541`, `src/services/discord/turn_bridge/tmux_runtime.rs:553`; `src/services/discord/turn_bridge/tmux_runtime/claude_stop_delivery.rs:415`, `src/services/discord/turn_bridge/tmux_runtime/claude_stop_delivery.rs:549`; `src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs:801`; `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:411` | host.interrupt. cancel token/queue/native stop/O 소유자 결론을 먼저 확보. session row를 retire하거나 pane을 닫지 않음 |
| Herdr Cancel/HTTP | `src/services/queue/herdr_stop.rs:13`; `src/services/turn_orchestrator/herdr_user_stop.rs`; `src/services/codex_tui/input/herdr_cancel.rs`; `src/services/discord/herdr_terminate.rs:192` | Escape/취소 결과와 pane-close를 구별. 기존 cancel_enabled·terminate_enabled 정책 확장 금지 |
| 종료/강제 종료 | `src/services/termination_audit/host_terminate.rs:52`; `src/services/provider_teardown.rs:84`; `src/services/turn_lifecycle.rs:478`, `src/services/turn_lifecycle.rs:488`; `src/services/discord/herdr_terminate.rs:576`; `src/services/dispatched_sessions/herdr_kill/effects.rs` | warrant를 받는 host.terminate. root/provider가 남으면 Unknown/보존, 성공으로 retire하지 않음 |
| clear/reset/종료 후처리 | `src/services/discord/commands/control/managed_reset.rs:104`; `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:160`, `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:380`; `src/services/discord/commands/control/clear_teardown/clear_end_record.rs:74`; `src/services/discord/admin_host_guard.rs:283`; `src/services/discord/router/message_handler/provider_isolation.rs:410`, `src/services/discord/router/message_handler/provider_isolation.rs:546` | 같은 incarnation을 재관측. turn/input원문/receipt/O debt의 기존 end 순서 유지 |
| 재시작/부팅 복구 | `src/services/discord/runtime_bootstrap/queued_recovery.rs:210`; `src/services/discord/runtime_bootstrap/recovery_flush.rs:73`; `src/services/discord/recovery_engine/restore_inflight.rs:98`; `src/services/discord/watchers/lifecycle/restore_retry.rs:83`; `src/services/discord/tui_prompt_relay/rehydration.rs:270`, `src/services/discord/tui_prompt_relay/rehydration.rs:668`; `src/services/discord/recovery_engine/herdr_reader.rs:235`, `src/services/discord/recovery_engine/herdr_reader.rs:501` | host.recover는 read/re-adopt. 생존 실행에 create/입력 재전송 0. active reconcile 뒤 큐 wake, standby는 주입 0 |
| reaper/idle/종료 정리 | `src/services/discord/tmux_reaper.rs:329`, `src/services/discord/tmux_reaper.rs:361`, `src/services/discord/tmux_reaper.rs:553`; `src/services/discord/tmux_watcher/post_stream_exit.rs:245`, `src/services/discord/tmux_watcher/post_stream_exit.rs:273`; `src/services/discord/tmux_watcher/terminal_abort_exits.rs:148`; `src/services/discord/runtime_bootstrap/owner_cleanup.rs:203`; `src/services/dispatched_sessions/tmux_cleanup.rs:338`; `src/services/maintenance/jobs/worktree_orphan_sweep.rs:619` | 관찰을 공통화하되 existing guard와 대상 범위를 유지. Unix 이름 나열을 Herdr/전체 Windows 프로세스 열거·kill로 확대하지 않음 |
| 기타 managed caller | `src/engine/ops/exec_ops.rs:418`; `src/services/agents/turn.rs:481`; `src/services/routines/agent_executor/reliability.rs:259`; `src/services/provider_cli/session_guard.rs:206`; `src/services/tui_prompt_dedupe/binding_context.rs:466` | 주어진 managed target의 효과만 trait화. F4는 Core로 연결할 소비자. 비관리 auth/login·자기 dcserver launcher는 아래 제외 |
| 시험·운영 도구 | `src/services/session_host/test_support.rs`; `src/services/session_host/herdr_socket_rig_tests.rs`; `scripts/e2e/tui_relay/tmux.py`; `scripts/e2e/tui_relay/herdr.py` | 시험은 계약 대역/실제 임시 서버 두 종류 유지. Python e2e는 외부 클라이언트로 API를 시험하므로 Rust trait 직접 구현 대상 아님 |

비관리 제외: `src/cli/dcserver.rs:831` 자기 런처, `src/server/routes/provider_auth_profiles.rs:339` 로그인 터미널,
`src/cli/utils.rs` CLI 진단/자기 세션, `scripts/check_*` 감사기의 문자열. 이들까지 기존 관리 채널의 terminate 권한으로 묶지 않는다.
TSV의 `ADAPTER_PRIMITIVE`는 adapter의 raw I/O, `THROUGH_TRAIT`는 이미 host 경유,
`TEST_OR_HELPER`/`NON_MANAGED`는 제외다. guard/refusal/queue façade는 물리 효과 지점이 아니다.

### HostKind·RuntimeHandoffKind·선택 상태의 입구

| 상태 | 생성/변경 | 읽는 진입점·삭제/확장 방침 |
|---|---|---|
| HostKind 3종 | `src/domain/host_observation.rs:4`, `src/domain/host_observation.rs:13`, `src/domain/host_observation.rs:23`; `src/services/session_host/model.rs:12`; marker `src/services/tmux_common/host_marker.rs:12`, `src/services/tmux_common/host_marker.rs:26`; `src/services/herdr_launch.rs:774` 부근 기록 | `src/services/session_host/resolve.rs:31`, `src/services/session_host/resolve.rs:70`; `src/services/session_host/consumer_guard.rs`; `src/services/discord/inflight/host_locator.rs:39`; `src/services/discord/inflight/host_recovery_guard.rs:58`; `src/db/dispatched_sessions/hosted_execution.rs:47`；전체 usage는 state-entrypoints.tsv |
| RuntimeHandoffKind 4종 | `src/services/agent_protocol.rs:68`, `src/services/agent_protocol.rs:75`, `src/services/agent_protocol.rs:86`; provider runtime handoff；`src/services/discord/turn_bridge/runtime_handoff_loop.rs:214`, `src/services/discord/turn_bridge/runtime_handoff_loop.rs:842`, `src/services/discord/turn_bridge/runtime_handoff_loop.rs:889`; `tmux_common` marker writer；`src/services/herdr_launch/runtime_marker.rs` | `src/services/discord/recovery_engine.rs:389`; `src/services/tui_turn_state.rs`; `src/services/discord/inflight/model/serde_adapters.rs:14`; `src/services/discord/inflight/save_store/identity_gate/runtime_stamp.rs`; `src/services/discord/turn_finalizer/completion_signal.rs`; `tui_prompt_dedupe/runtime_binding/codex_policy/*` |
| `herdr_turn_enabled` | `src/config/runtime_settings.rs:90` deserialize；live config.current；`src/config/reload_classes/table.rs:143`; 시험 setter `src/services/turn_host.rs:254` | `src/services/turn_host.rs:238`만 actual live gate. 옵션 false/None 유지. live 값 검사는 운영 설정을 열지 않고 코드로만 조사 |
| Codex 별도 gate | `src/config/runtime_settings.rs:94`; `src/services/turn_host.rs:220`, `src/services/turn_host.rs:270` 시험 setter | bool false/None이면 기존 unsupported 유지. Claude의 새 선택 표현이 Codex 동의를 암묵적으로 true로 바꾸지 않음 |
| 채널 endpoint | `src/config/session_hosts.rs:134`, `src/config/session_hosts.rs:177`, `src/config/session_hosts.rs:212`, `src/config/session_hosts.rs:325`; migration/approval_store/projection；user_default | `src/services/turn_host.rs:155`, `src/services/turn_host.rs:316`; registry；recovery local endpoint. snapshot·reload·동의/migration 경로도 선택을 소비하므로 새 resolver의 입력으로 일원화 |
| 시험 writer/reader | state-entrypoints.tsv의 `test-named`와 mixed-file inline 시험 | 강제 switch, raw JSON/marker, host evidence 대역도 새 선택 타입의 bool 호환성을 시험. 이름만 바꿔 검사를 무력화하지 않음 |

호스트와 provider 실행 형태는 직교한다. Herdr의 Claude/Codex도 현재처럼 `ClaudeTui`/`CodexTui`를 쓴다.
`RuntimeHandoffKind::Herdr`를 추가해 두 축을 다시 섞지 않는다. Process 삭제 뒤 3종 실행 형태와 2종 호스트가 남으며,
Core는 F4 host tag로 추가한다. Core를 Discord native runtime으로 가장하지 않는다. V1 `sessions.hosted_execution`의 Herdr 전용 host 검증에 Core 문자열을 밀어 넣지 않고, F4의 기존 SessionIdentity/SessionRecord에 terminal id·incarnation을 연결한다.

## 공통 추상화의 단위

단일 정본은 `services/session_host`다. 객체 안전한 `InteractiveSessionHost`를 다음 효과 단위로 확장한다.
모든 구현체에 default unsupported를 추가하고 caller 없는 dormant prod 모듈을 만들지 않는다.
첫 기반 조각은 cfg(test)의 계약 대역과 타입부터 착지하거나 **기존 호출자가 즉시 쓰는 포트만** 추가한다.

| 포트 | 입력·결과 | 소유 경계 |
|---|---|---|
| create | 준비된 argv/cwd/env, canonical owner/nonce, 이미 발급한 launch permit → NotSent / Created / Indeterminate와 위치 | provider가 명령을 준비하고 기존 DB CAS/permit 소유자가 Pending 설치. host는 실제 생성만. 필요한 비동기 I/O는 boxed future/기존 runtime blocking 경계로 감쌈 |
| submit | `AuthorizedHostTarget` + 출처·typed payload/keys/wrapper frame → Confirmed / Refused / Indeterminate | raw `HostSessionRef(name)`가 쓰기 권한이 되지 않음. 기존 Herdr mutation gate와 tmux first-key gate를 adapter가 소비. InputTransport는 plan executor adapter로 남김 |
| observe | ref/expected → presence/liveness/screen/cwd/pid, read-only execution evidence | ProbeError/Unknown을 Missing으로 합치지 않음. 기존 bool collapse는 같은 장소·의미로 유지하고 새 경로에서 재사용하지 않음 |
| interrupt | existing native stop/cancel owner가 허용한 exact execution target → 기존 outcome | Escape/C-c/기존 signal fallback. turn 중단을 host session 종료로 승격하지 않음 |
| terminate | existing identity Match + permitting stop verdict로 발급한 warrant → 확인된 Gone / Refused / Indeterminate | `src/services/termination_audit/host_terminate.rs:35` warrant 발급 규칙 유지. close ACK만으로 provider 소멸이라고 보고하지 않음 |
| recover | durable owner/location/expected/source + 현재 endpoint witness → SameExecution / Gone / Withheld / Unknown | 저장된 같은 실행을 read-only 재관측. source/claim/O publish는 기존 소유자가 수행하고 마지막에 admission. recover 내부에서 create·blind replay·추정 kill 없음 |

`HerdrLaunchHost`의 create/evidence는 Herdr adapter의 내부 구현으로 흡수한다. `InputTransport`와
`EarlierExecution`은 계획/핸드오프의 도메인 port로 남아도 모든 physical effect가 `InteractiveSessionHost`를 통과해야 한다.
목적은 trait 이름 하나로 DB/queue/O를 합치는 것이 아니라 호스트의 생성부터 재채택까지 동일한 대상과 증거를 쓰게 하는 것이다.
이미 main의 `src/services/claude_tui/host_input.rs:7` BusyOperation과 `src/services/discord/queue_io/input_owner/effect/claude.rs:23`가 소유한 공통 Claude operation을 재사용한다. `src/services/discord/queue_io/input_owner/effect/herdr.rs`는 현재 compatibility 시험 fixture이므로 별도 production writer로 재구현하지 않는다. 새 transition lock·새 수동 recovery 상태계를 추가하지 않는다. 기존 SessionTransitionLease, nonce CAS, source authority,
writer claim, notice fence를 그대로 전달한다. traits에 일반 `kill_pid`/`attach_terminal` 권한을 넣지 않는다.

## Windows 전송·실행·신원

확인한 공개 primary source는 Herdr TUI 태그 v0.9.5
[`2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01/src/ipc.rs`](https://github.com/herdrdev/herdr/blob/2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01/src/ipc.rs#L35)다.
Windows는 `interprocess::local_socket::Stream`, `GenericNamespaced`로 논리 socket path를 변환한다(:45).
서버도 같은 변환과 DACL을 사용하고(:68), socket path 파일에는 pid:시각 표식만 쓴다(:322).
따라서 AF_UNIX/실제 파일 open이나 `\\.\pipe\herdr`라는 추정 이름을 쓰지 않는다.
실제 pipe name 변환은 같은 라이브러리 규약/태그를 고정해 U01에서 왕복 확인한다.
기존 wire framing/typed reply/id/partial-write 구별은 `src/services/session_host/herdr/wire.rs`와 `src/services/session_host/herdr/contract.rs:21`을 재사용한다.

[공식 Windows 문서](https://herdr.dev/tui/docs/windows-beta/)에서 ConPTY의 로컬 지속 세션과 API 기반 pane 사용,
전체 앱 접속은 지원하지만 direct terminal attach, live server handoff, Unix FD handoff/foreground group,
서명 바이너리/SmartScreen 회피는 미지원임을 확인했다. 이 설계는 그 기능에 기대지 않는다.
현재 AgentDesk 호환 상수 `src/services/session_host/herdr/model.rs:13`의 protocol 22, 최소 0.9.3과 Windows 설치 0.9.5의
실제 ping/schema 일치는 아직 U01이다. 최신 문서와 설치 바이너리의 기능을 같은 것으로 단정하지 않는다.

| Unix 가정 위치 | Windows 포트 요구 |
|---|---|
| `src/services/session_host.rs:6`, `src/services/session_host.rs:10`, `src/services/session_host.rs:19`, `src/services/session_host.rs:51`; `src/services/session_host/herdr_registry.rs` cfg branches | Transport factory를 LocalEndpoint으로 분리. Unix path와 Windows logical pipe address를 태그로 구분. 무작정 모든 unix cfg를 제거하지 않음 |
| `src/services/session_host/herdr/model.rs:44` absolute socket PathBuf, `src/services/turn_host.rs:316` socket_addr 문자열 | 논리 주소와 실제 pipe 변환 분리. 기존 persisted socket_addr는 endpoint 해석 adapter에서 호환하며 다른 문자열을 tmux로 default하지 않음 |
| `src/services/session_host/herdr/transport.rs:6`, `src/services/session_host/herdr/transport.rs:43`, `src/services/session_host/herdr/transport.rs:83` UnixStream/peer | overlapped I/O나 interprocess pipe + 총 deadline/프레임 제한. write 시작 전 실패=NotSent, write 이후 timeout/끊김=Indeterminate, mutation retry 0 |
| `src/services/session_host/herdr/provenance.rs:20`, socket_peer·/proc/sysctl; `src/services/session_host/herdr/pane_probe.rs:145` not(unix) 미지원 | GetNamedPipeServerProcessId + process handle/creation FILETIME + root/provider tree/nonce 검증. 실제 프로세스 시작값과 별도 OS boot identity 보존 |
| `src/services/session_host/herdr/policy/loaded_image.rs:19` Linux/macOS만 지원; `src/services/session_host/herdr/policy/peer.rs:183` | Windows loaded image/file identity adapter는 U03에서 검증. 서명 요구를 추가하지 않고 기존 policy 동등성 입증. 증명 못한 user_default는 활성화하지 않음 |
| `src/services/session_host/herdr/observe.rs:107` dedicated config E7 | `resume_agents_on_restore=false`의 실제 실행 서버 증명 필요. 설치 바이너리 버전/파일 경로만으로 Off가 아님. owned launch 기록 + 실제 server identity/config publication을 결합하고 U03 검사 |
| `src/services/session_host/herdr/autostart/spawn.rs:48`, `src/services/session_host/herdr/autostart/spawn.rs:238`, `src/services/session_host/herdr/autostart/spawn.rs:259` macOS 지원/setsid/pre_exec/passwd | Windows용 프로세스 수명·현재 계정 환경/로그 경계. dcserver job에서 별도 수명 확보 여부 U04. 기존 서버를 발견하면 재시작/업그레이드하지 않음 |
| `src/services/herdr_launch.rs:353`, `src/services/herdr_launch.rs:368`, `src/services/herdr_launch.rs:495`, `src/services/herdr_launch.rs:733`; provider launch shell_escape/unset | Windows argv/cwd/env 준비, cmd/npm shim 인용, marker/hook/native paths. `/bin/sh`, export/unset, Unix self-report 설치 형식을 가져오지 않음 |
| `src/services/claude.rs`/`src/services/codex.rs:27`; `src/services/discord/recovery_engine.rs:118`~`src/services/discord/recovery_engine.rs:139`; `src/services/discord/runtime_bootstrap.rs:15`; `rehydration` | tmux-only 부분은 cfg 유지, native provider/source/recovery는 OS-neutral 모듈로 분리. Windows 경로가 호출되도록 boot/periodic rehydrate까지 함께 연결 |

권한·수명: dcserver와 Herdr는 같은 Windows 계정/맞는 elevation의 기존 전제를 쓴다. 임의 사용자 서버에
붙으려고 `allow_unelevated_clients`를 켜지 않는다. 이미 설치된 0.9.5는 사용자 경로 아래 있으므로 실제 서비스 계정에서
발견·접속 가능한지 U06이다. 없는 설치·권한 불일치·불가한 Job 분리이면 생성 전에 명시적 오류와 기존 원문 보존.
`DETACHED_PROCESS`만 주면 job kill에서 벗어난다고 가정하지 않는다. 기존 Windows service 정의/운영 설정 변경 없이
새 임시 dcserver와 임시 Herdr가 독립 수명을 갖는 구현이어야 한다. 보장할 수 없으면 A 활성화를 보류한다.

## 재시작·부팅·incarnation·종료 경로

1. **새 실행**: canonical channel/session owner를 구하고 현재 선택·기존 host witness를 함께 읽는다.
   nonce/incarnation을 준비(`src/services/herdr_launch.rs:466`, `src/services/herdr_launch.rs:733`)하고 Pending CAS(`src/services/herdr_launch.rs:482`) 후에만 host.create.
   location CAS(`src/services/herdr_launch.rs:554`)와 root/provider ExpectedExecution 저장 뒤 native SessionStart/rollout source를 Bound로 연결한다.
   생성 reply 유실은 nonce와 작업공간 표식을 가진 Pending으로 남아 기존 unplaced census가 해결한다.
   sender timeout을 실패 확정으로 해석해 두 번째 create를 하지 않는다.
2. **dcserver만 종료/재시작**: Herdr 서버와 pane/ConPTY는 종료하지 않는다. 메모리 registry는 비워져도
   `sessions.hosted_execution`의 owner/location/expected/nonce/source_ref, spawn/native markers와 O cursor는 남는다.
   active boot에서 endpoint를 **접속**하고 server PID+creation/boot, pane/root/provider+nonce를 재관측한다.
   동일 실행이면 `host.recover`→`attach_restarted_herdr_source`의 기존 source 재게시·admission으로 돌아온다.
   provider spawn 0, terminal attach 0, 기존 user payload 재송신 0. 중간/완료 턴은 원래 receipt/O cursor로 계속한다.
3. **부팅 gate/인접 recovery**: catch-up이 입력을 보존하는 동안 queued/inflight 복구와 binding rehydrate를 수행한다.
   active reconcile complete의 기존 wake를 유지(`src/services/discord/runtime_bootstrap.rs:35`); standby complete는 주입하지 않는다(:472).
   한 번의 boot에서 Unknown이면 health/rehydration의 기존 재시도가 재관측한다. Windows에서 함수가 컴파일되는 것뿐 아니라
   실제 `src/services/discord/tui_prompt_relay/rehydration.rs:270`, `src/services/discord/tui_prompt_relay/rehydration.rs:668` 호출에 도달하는지 검증한다.
4. **Herdr 서버 교체/Windows 재부팅**: 같은 프로세스가 살아남는다고 주장하지 않는다. TUI Windows는 live handoff가 없고
   서버 재시작에 provider 재실행이 개입하면 다른 incarnation이다. E7 Off를 유지하고 이전 실행 root/provider의 실제 소멸을
   확인한 뒤 기존 `src/services/discord/recovery_engine/herdr_server_replaced.rs:172`, `src/services/discord/recovery_engine/herdr_server_replaced.rs:240`와 root-replaced 경로로 재조정한다.
   boot identity는 dcserver incarnation과 구별한다. PID 재사용/pipe 이름 재사용/권한 오류를 absent로 치환하지 않는다.
   새 실행이 필요하면 기존 입력 소유자가 별도 nonce로 시작하고 old O debt를 먼저 정리하는 현행 정책을 쓴다.
5. **Cancel/Stop**: 현재 turn owner만 native interrupt를 요청한다. queued 원문/Held/WriteUncertain를 정책대로 남기고
   completion/native terminal evidence를 기다린다. provider의 Escape ACK는 세션 종료/turn 완료/O 게시 완료가 아니다.
6. **Terminate/clear/reset**: transition lease→동일 identity와 stop verdict→host.terminate→exact target의 pane/root/provider
   부재 재확인→기존 retire/source/O cleanup. close reply가 불명확하면 기록과 원문을 남겨 재관측한다.
   crash가 close 앞/뒤 어느 단계에 와도 row를 먼저 지워 고아를 만들지 않는다. 기존 reaper/stop/clear의 권한을 합집합으로 넓히지 않는다.

오래된 Process 실행의 전환은 별도 경계다. 새 바이너리가 memory-only registry를 이미 잃은 과거 child를
자동으로 찾아 입양할 수 있다고 약속하지 않는다(기존 결함 `src/services/session_backend.rs:341`, `src/services/claude/process_session_launch.rs:49`).
전환 기반이 살아 있는 Process registry를 가진 동안에는 **진행 턴의 자연 종료 후**, 기존 managed end 권한으로만
그 실행을 끝내고 저장된 native session id를 Herdr launch에 넘긴다. 불명확한 이전 실행은 원문 보존/명시적 거절로 남기며
두 번째 provider를 띄우지 않는다. 이 일시 호환 경로는 Process 삭제 조각 전까지 있어야 한다.
이전 프로세스가 살아 있는지 증명할 수 없는데 운영자의 수동 정리 명령으로 해결하는 안은 내지 않는다.
이미 registry를 잃은 legacy row의 장기 유실 수리는 이번 범위 밖이며, 새로운 Herdr 세션은 처음부터 durable identity를 가진다.

## 입력 출처와 소유자

| 입력 출처 | 원문/권위 소유자 | 실제 효과와 이번 제한 |
|---|---|---|
| Discord live user 입력 | #7272 queue/input_owner, committed ticket/front | 기존 owner→provider plan→host.submit. 전달 후 uncertainty를 새 원문으로 enqueue하지 않음 |
| Discord catch-up/queued follower/Held | 같은 durable queue/front/reservation | 재시작/boot가 새 출처가 되지 않음. source ready와 실제 wake 뒤 같은 owner가 한 번 제출 |
| routine/goal/campaign 지시 | 각 run/attempt/parent owner | 목적/결과 식별자를 보존, host가 임의 user turn을 발명하지 않음. Core F4 첫 지시는 run argv로 |
| 사람이 TUI/Windows Terminal에서 직접 친 입력 | 사람/native provider 기록 | AgentDesk 제출 receipt로 소유하지 않음. busy/draft 관측은 입력 허가가 아님. Core 3초 quiet도 AgentDesk owner를 대신하지 않음 |
| hook/OSC/JSONL/native queued-input 관측 | provider/source binding, 기존 O reader | 관측 자료이며 재송신 지시 아님. OSC done으로 Discord native turn 완료를 합성하지 않음 |
| `/clear`, `/resume`, model/effort 설정, `/exit` | control transition·provider settings·handoff owner | user payload와 분리된 control/system 입력. 기존 lineages/nonce/clear 기대값과 read-before-effect 유지 |
| Cancel/Escape/C-c | cancel token/native stop owner | 세션 종료 권한과 다름. actor owner·execution identity를 그대로 전달 |
| Terminate/HTTP force/cleanup | existing warrant/termination/source owners | 허용한 exact owned target만. 다른 프로세스·다른 channel·replaced incarnation에 전파하지 않음 |
| Core child→parent 결과 알림 | F4 attempt/parent terminal/run owner | parent message는 결과 도착 신호. 저장된 attempt id·산출물/exit를 확인한 후 결과로 수용 |
| retry/recovery 합성 입력 | 기존 원문 소유자 | control 재조정과 사용자의 original replay를 구분. write 이후 uncertainty에는 자동 paste 재시도 0 |

#5071 대조: 정본 `docs/relay-state-contract.md`의 현재/목표 구분을 읽었다. `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:113`의 원본 source id·mailbox, `src/services/tui_o/writer/mod.rs:135`의 delivery lease, `src/services/discord/turn_bridge/runtime_handoff_loop/guarded_save.rs:62`의 identity-gated row 저장에서 각 결정 권위를 가져온다. 이 설계는 그 계약의 아직 구현되지 않은 범용 durable disposition/ACK를 완료했다고 주장하지 않는다. 호스트는 terminal bytes/API 결과만 소유한다. output completion/게시와 원문 settlement는 기존 단일 권위를
유지하고 adapter가 직접 Discord 응답을 게시하거나 inflight row를 독자 삭제하지 않는다.

## 채널 선택과 기본값

현재 `runtime.herdr_turn_enabled: Option<bool>` 자리에 bool 호환 타입을 넣는다. 예시 표현은
`true`/`false` 또는 `{channels: {<channel>: "tmux" | "herdr"}}`이며, 미지정 channel은 기존 호환 규칙을 따른다.
serde는 untagged `Legacy(bool) | Channels { channels: BTreeMap<ChannelId, HostChoice> }`로 고정한다. 미지의 host/필드는 검증 오류다. 별도 top-level 스위치를 추가하지 않는다.
기존 `session_hosts` map은 endpoint의 위치를, 이 runtime map은 desired host를 소유한다. endpoint/boot 승인·user_default 전제도 새 resolver가 읽으며 두 번째 desired-host 원장이나 별도 동의 경로를 만들지 않는다.
이 표현은 채널의 desired host이고, 살아 있는 current host는 durable witness가 정한다. 설정 변경만으로 생존 실행을 덮어쓰지 않는다.

| 입력/플랫폼 | 유지할 기본 의미 |
|---|---|
| Unix, endpoint 없는 채널, 키 미지정/옛 bool | 현재처럼 tmux. tmux 부재시 Process fallback은 제거되고 시작 전 오류/원문 보존 |
| Unix, endpoint 있는 Claude, true | 현재처럼 Herdr |
| Unix, endpoint 있는 Claude, false/None | 현재처럼 입력 전 거절; 자동 tmux demotion 없음 |
| Codex endpoint + 별도 gate false/None | 현재 unsupported 유지. gate true이면 선택한 Herdr/현재 host와 일치 검사 |
| 명시한 channel tmux/Herdr | 기존 handoff/return의 same-session/stop/nonce 검사를 통해 다음 안전 경계에서 이동. Windows tmux 요청은 명시적 unavailable |
| Windows, endpoint/명시 선택 없는 예전 관리 세션 | Process 제거라는 요구에 따른 **호스트 구현의 유일한 의도적 기본 변경: Herdr**. 사용자 채널·provider/native session/입력 정책은 유지 |
| Windows, 명시 false/기존 Herdr 거절 설정 | 몰래 true로 바꾸지 않음. 지원 가능한 host가 없으면 명시적 거절 |

Windows의 암묵 기본 대체는 **endpoint도 명시 선택도 없는** 기존 관리 Claude/Codex에만 적용한다. 기존 configured endpoint의 false/None과 Codex 별도 gate는 그대로 거절한다. 이 내부 기본 host는 provider별 legacy pipe 경로 대신 native TUI 경로를 선택하며, 사용자가 binding/운영 설정을 다시 작성해야 하는 조건을 두지 않는다. Windows implicit endpoint는 runtime root+execution node에서 결정하는 내부 키 `windows-managed-default/v1`, 고정 named-session 식별자로 boot마다 재구성한다. 동일 dedicated server를 재사용하며 config에 endpoint 행을 자동 저장하지 않는다. 같은 endpoint factory가 생성과 복구를 소비하므로 registry 초기화가 기존 row를 잃지 않는다. 이 대체와 explicit false의 경계는 S08/S12 및 U10의 필수 시험이다.

“기본값이 같다”는 Unix의 tmux/Herdr 선택·옛 bool 의미를 보존한다는 뜻이다. Windows Process 기본 구현까지 같게
두면서 Process를 삭제하는 것은 불가능하다. 그 예외를 숨기지 않고 Windows activation 수용 시험에 넣는다.
tmux가 없는 Unix에서 무조건 Herdr를 자동 설치하거나 새 서버로 넘어가지 않는다. 선택한 host가 없다는 오류는
payload 전/기존 start-failure 경로로 한 번 드러내고 큐 원문을 보존한다. Unknown을 unavailable 확정으로 보지 않는다.

## Herdr Core 제약과 F4 계약

이슈 버전은 0.1.14다. 조사 시 최신 공개 태그는 **0.1.21**이므로 구현 조각에서 실측 버전을 고정해야 한다.
0.1.14 primary source는 [`29e6d375623de40cb4820e0894aa131ba057b796`](https://github.com/herdrdev/herdr-core/tree/29e6d375623de40cb4820e0894aa131ba057b796)다.
[`crates/herdr-core/src/input.rs:65` (상류 고정 태그)](https://github.com/herdrdev/herdr-core/blob/29e6d375623de40cb4820e0894aa131ba057b796/crates/herdr-core/src/input.rs#L65)의 기본 quiet는
3000ms이고 `src/services/discord/turn_bridge/runtime_handoff_loop/guarded_save.rs:154` send는 paste 뒤 기본 200ms 후 Enter다. 물리 입력은 PTY 경로이며 키/interrupt API도 존재한다.
`src/SKILL.md:32`는 첫 지시를 run에 넣으라고 한다. 부팅 중 send가 drop될 수 있고 실패 후 blind resend도 금지한다.
`src/SKILL.md:148`은 OSC 7501 idle/working/done/blocked/error와 live/finished를 구분한다.

CoreHost는 install/upgrade나 사용자의 core stop을 하지 않는다. 첫 task를 provider argv에 넣어 실행하고 terminal id,
attempt id, parent id, host/core identity를 저장한다. 후속 지시는 같은 owner target으로 submit한다. OSC는 진행 상태이며
결과 수용은 child의 exit/result artifact/attempt id로 확인한다. parent/children wait의 message/timeout/not_started는
완료가 아니다. children은 기본 parent 종료에 종속되므로 `--outlive-parent`를 무조건 켜서 고아를 만들지 않는다.
반대로 dcserver 프로세스를 parent terminal로 잘못 연결해 재시작 때 lane을 죽이지 않아야 한다(U12).

Core가 재시작/업데이트돼도 holder가 프로그램을 유지할 수 있는 것과 provider 대화의 `--resume`는 별개다.
Core에는 Claude/Codex session 이어 열기 API가 없으므로 adapter capability로 Unsupported를 표한다.
OS 재부팅/프로그램 loss 후 keep 복구는 shell을 만들 수 있지만 같은 agent가 살아난 것이 아니다.
그 terminal의 이전 attempt는 기존 lost/cancelled 결론으로 끝내고 새 lane은 새 attempt다. 사용자가 요구하지 않은
자동 provider 재실행/기존 task 재송신을 추가하지 않는다. F4 한 lane의 end-to-end 기록까지 있어야 Core DoD가 끝난다.

F4의 현재 정본도 조사했다. `src/domain/campaign/model.rs:154` SessionIdentity와 `src/domain/campaign/model.rs:162` Alive/Gone/Unknown, `src/domain/campaign/model.rs:177` SessionRecord가 attempt를 연결한다. `src/services/campaign_execution/control.rs:514`의 Gone 증거·등록 계약, `host_gate.rs:542`의 admit_and_initiate, `src/services/campaign_execution/control.rs:707` indeterminate 대사, `src/services/campaign_execution/effect_envelope.rs:117` action envelope를 C02가 소비한다. 따라서 새 runtime adapter는 이 gate의 nonce/attempt를 받아 공통 host를 호출하고 결과를 기존 command/store로 돌려준다. `src/services/campaign_execution/verification_runner.rs:109`는 검사 프로세스 실행기이므로 LLM TUI adapter로 바꾸지 않는다. 구현 착수 때 이 좌표의 main drift를 다시 확인한다.

## 독립 착지 조각

현재 cap은 스킬 `scripts/pr_cap_prod.py:15`의 **prod 30파일 / 순 코드 +1500**, 설계 재분할 선은 **+1200**다.
모든 수치는 코드 줄(주석/공백/시험 제외)의 상한 추정이다. giant 길이로 분할하지 않는다.
공유 파일은 아래 순서의 단일 작성자가 이어 수정한다. 병렬 독립 구현을 요구하지 않는다.
각 기반은 실행 경로를 바꾸기 전 기존 consumer와 연결하거나 cfg(test)로 검증해 dead_code/unused warning을 늘리지 않는다.

| 조각 | 소유 파일/범위 | prod 상한·순증 예상 | 수용 시험 | Windows CI / win 실측 |
|---|---|---|---|---|
| S01 포트 계약 | `session_host/{traits,model,test_support}.rs`, `src/services/termination_audit/host_terminate.rs`; fake contract | 6 / +650 | host 종류·warrant·NotSent/uncertain·raw name 쓰기 거절; 신규 production 동작 0 | compile / 불필요 |
| S02 tmux 관찰 포트 | `session_host/{tmux_host,legacy_collapse,resolve}.rs`, `src/services/tmux_diagnostics.rs`, `src/services/provider/session_probe.rs`, `src/services/session_activity.rs` | 12 / +350 | 3값/기존 bool collapse/지연·dead pane·동일 target | compile / 불필요 |
| S03 입력 물리 효과 | `claude_tui/host_input*`, `src/services/codex_tui/host_input.rs`, `tui_input/{actor/pane,submission_tmux*}`, `src/services/session_host/tmux_host.rs`, gated Herdr adapter | 18 / +850 | 원문 owner·first-key cancel·draft stash·partial write·receipt; 전송 spy가 trait만 관측 | compile / 후속 U07/U08 |
| S04 생성/핸드오프 포트 | `src/services/claude/tui_session_launch.rs`, `claude/herdr_turn/{handoff,never_attached}.rs`, `src/services/codex/tui_session_launch/first_input.rs`, `herdr_launch*`, provider별 launch | 18 / +850 | Pending crash/ACK 유실/2개 동시 create·same-session Return·argv | compile / 후속 U04/U05 |
| S05 종료/중단 포트 | `src/services/termination_audit/host_terminate.rs`, `discord/turn_bridge/tmux_runtime/{judged_stop,native_stop,*stop_delivery}.rs`, `src/services/provider_teardown.rs`, `src/services/turn_lifecycle.rs`, `src/services/dispatched_sessions/herdr_kill/effects.rs` | 18 / +700 | Cancel은 세션 생존; close 전/후 crash; mismatched/Unknown warrant 무효; 기존 signal fallback 동등 | compile / 후속 U09 |
| S06 복구/소유자 포트 | `discord/recovery_engine/{host_reconcile,herdr_reader,herdr_server_replaced,restore_inflight}.rs`, `tui_prompt_relay/{herdr_source,rehydration}.rs`, runtime_bootstrap/복구 파일 | 18 / +850 | source cursor·old format·standby·boot+health 재채택·reconcile wake | compile / 후속 U08 |
| S07a~S07f 잔여 consumer 6묶음 | [consumer-slices.tsv](i7583/consumer-slices.tsv)의 정확한 파일 목록. 관찰/cleanup·설정·reaper·routines·HTTP caller | **각 현재 16~17파일, cap ≤24 / +250** | leaf trait spy + source scan; 각 기존 guard/bool/동일 provider 결과 동등. 이전 조각과 겹치는 파일은 해당 조각 완료 후 실제 잔여만 수정 | 각 compile / tmux 회귀 외 불필요 |
| S08 선택 표현 호환 | `src/config/runtime_settings.rs`, `config/session_hosts*`, `src/services/turn_host.rs`, reload_classes/validation 시험 | 14 / +700 | bool/None과 channel 선택 표 전 칸, stale current host/승인 snapshot, unsupported platform | compile / 활성화 전 U06 |
| S09 Windows named pipe | `session_host/herdr/{transport,model,wire,contract}.rs`, registry/factory, 새 pipe adapter, dependency feature | 10 / +950 | 실제 ephemeral pipe·framing·timeout·byte partial-write·동시 mutation serial·endpoint 교체 | **native 실행 job** / U01/U02 |
| S10 Windows OS witness | `herdr/{provenance,pane_probe,observe}.rs`, policy/{peer,loaded_image}, Windows OS leaf | 12 / +1100 | PID 재사용/새 server·boot·provider tree/nonce·E7 config publication·loaded image 동일성 | **native 실행 job** / U03/U05/U06 |
| S11 Windows launch/native source | `herdr/autostart/{spawn,candidate,record}.rs`, provider launch args, hook/source 경로, incarnation marker | 18 / +1100 | native Windows argv 공백/한글/npm shim·UTF-8/newline·SessionStart/rollout/Hook·server 독립 수명 | **native 실행 job** / U04/U05/U07 |
| S12 Windows recovery/활성화 | `src/services/discord/recovery_engine.rs`, runtime_bootstrap/rehydration cfg, selected provider dispatch, ci-pr/path selector/Windows exact tests | 18 / +1000 | 부팅/중간턴/dcserver crash·같은 session/pane/nonce·no duplicate/O cursor·cancel/terminate; missing tmux 명시 오류 | **실행·CI 선택 감사** / **U01~U10 일괄** |
| D01 provider/pipe 제거 | Claude/Codex process_session_launch, qwen/session_lifecycle process 부분, provider routing, wrapper args/run/3 wrapper와 pipe input | **20 / -700 이하** | Windows Herdr와 Unix tmux 모두 spawn; no-tmux 실패 명시; shared parser/FIFO 유지 | required / U07~U09 증거 재사용 가능 |
| D02 시험 대역 치환 | process-uses.tsv의 test-named/inline 대역, session_host/test_support, consumer evidence injection | **8 / +250 이하** (시험 파일 수 별도 기록) | 기존 negative identity/clear/stop 시험 수·assert 의미 보존. 실제 ProcessBackend가 살아 있음의 대역인 시험 0 | required / 실제 서비스 불필요 |
| D03 소비자 분기 제거 | admin/control/reset/health/replay/finalizer/runtime consumers, body_census, tui_turn_state; Process kind variant는 아직 유지 | **24 / -250 이하** | 오래된 raw kind는 unknown 보존; 삭제한 match/import caller warning 0; 인접 cancel/clear/recovery 동등 | required / 불필요 |
| D04 registry/Host/enum 최종 삭제 | session_backend core/process output/auth 의존, ProcessHost, domain HostKind, agent_protocol kind/handoff, inflight parser/locator, runtime_handoff_loop, runtime_marker | **24 / -1000 이하** | executable source의 ProcessBackend/ProcessHost/Process kind/pipe mode 0; JSONL parsing과 native runtime 3종 유지 | required / U08/U09 회귀 |
| C01 Core host adapter | 새 `session_host/herdr_core/*`, model/factory/contract; F4의 기존 host 선택 소비 준비 | 12 / +1100 | hc schema/version pin·실제 임시 Core의 run/send/read/interrupt/owned terminate/recover·unsupported resume | **native 실행 job** / U11/U12 |
| C02 F4 소비자 실측 | 새 `src/services/campaign_execution/tui_runtime.rs`, `campaign_execution/{mod,host_gate,effect_envelope,control}.rs`, `src/domain/campaign/model.rs`, existing server-campaign command/store와 targeted harness | 10 / +800 | 독립 TUI lane 1개 첫 지시/OSC/parent wait/결과/재부팅 loss; 다른 terminal 정리 0 | required / **U11~U14** |

총 **23 착지 조각**(S01~S06 6개 + S07 여섯 + S08~S12 5개 + D 네 개 + C 두 개) 상한 계획이다.
S07 파일 수를 실측해 6묶음으로 고정했다. +1200을 넘는 조각은 구현 전에 다시 나눈다.
삭제는 음수 줄 수여도 **파일 상한**으로 판단한다. D04에 25번째 파일이 필요하면 caller 삭제를 D03으로 먼저 옮겨
컴파일 가능한 중간 head를 만들고 상한을 맞춘다. variant 삭제를 일부 match만 남겨 깨진 head로 착지하지 않는다.
각 조각은 대상 시험·원인 guard mutant(상태 변경 조각)·경고 0·필수 selector·static gates를 통과한 독립 head여야 한다.

순서: S01→S02/S03/S04/S05→S06→S07 전부→S08, Windows S09→S10→S11→S12.
S12 전에는 Windows 기본 전환과 Process 경로 제거를 활성화하지 않는다. D01→D02→D03→D04,
Core는 공통 포트가 안정된 뒤 C01→C02다. C01은 runtime 호출자가 없으면 cfg(test)로 두고 C02에서 활성화한다.
DoD 3 최종 검사는 S07 이후와 D04/C02 이후 각각 수행한다. 소스/의존성이 바뀌면 이전 head 실행 증거를 재사용하지 않는다.

## [U] 실측 항목 — 전부 win-desktop 측정 패킷으로 전달

이번 레그에서 한 실측은 설치/버전의 읽기뿐이다. `/mnt/c/Windows/System32/cmd.exe /d /c where herdr`는
`C:\Users\itism\.herdr\packages\standalone\releases\0.9.5-x86_64-pc-windows-msvc\herdr.exe`를 찾았고
그 바이너리 `--version`은 **herdr 0.9.5**다. `where hc`는 미발견이다. PATH에 cmd.exe가 없어 절대 경로를 썼다.
raw log는 `logs/design-1/windows-where-{herdr,hc}.log`, `windows-herdr-version.log`다.
서버 접속/시작, 설치·설정 변경·native cargo 실행·서비스 재시작은 하지 않았다.

아래 명령은 **후속 격리 실측의 방법**이다. `$sandbox`는 새 임시 경로, 이름 `i7583-<nonce>`의 전용 Herdr/Core,
child/daemon은 실측 harness가 직접 시작한 것만 대상이다. 운영 dcserver/AgentDesk-* 세션에는 적용하지 않는다.
필요한 미설치 Core 제공은 코디네이터가 승인한 실측 환경에서만 한다. 지금 설치를 요청하거나 변경하지 않았다.

| ID | win-desktop에서 잴 명령/방법 | 관측값·기대값 / 차단 범위 |
|---|---|---|
| U01 pipe 주소·schema·framing | 설치 바이너리 `herdr api schema --json` 저장; 전용 `HERDR_SESSION=i7583-<nonce>` 서버를 harness로 띄워 logical socket→GenericNamespaced 연결, ping/session.snapshot/pane.process_info. `cargo test <named_pipe_contract>` native | protocol/version·pipe 실제 이름·DACL·프레임/ID·screen 일치. AgentDesk protocol22와 같아야 S09/S12 활성화 |
| U02 송신 경계 | 임시 pipe proxy가 byte0 이전 실패, 첫 write 뒤 timeout, ACK 유실, malformed id/oversize를 주입. native `cargo test <pipe_partial_write>` | NotSent/Indeterminate 분리·create/paste 재송신 0·총 deadline. S09 필수 |
| U03 서버/E7/loaded image 증거 | `GetNamedPipeServerProcessId`, `GetProcessTimes`, `QueryFullProcessImageNameW`, file-id/creation/boot 기록; fresh canonical Off 서버/이후 config 교체/바이너리 교체/서버 PID 교체 각각 임시 harness | 같은 handle server witness만 승인; stale/mismatched/unproved Off는 Withheld. Windows 환경 읽기 동등성이 안 되면 user_default/그 launch mode 활성화 금지. S10 차단 |
| U04 dcserver와 Herdr의 독립 수명 | 운영 launch 인자를 **코드 기준**으로 재현한 임시 native parent/Job Object→Herdr 서버/pane 실행; parent 정상 종료·강제 종료·job close 후 `Get-Process -Id <owned-server>,<owned-provider>`와 pipe snapshot | server/root/provider PID+FILETIME·pane id 그대로, 창/terminal attach 없이 API 재연결. `DETACHED_PROCESS`의 추정으로 PASS 금지. 서비스 형태 동등 harness에서 실패하면 Windows 활성화 차단 |
| U05 root/provider/nonce와 셸 인용 | 임시 `workspace.create`로 Claude/Codex 실행; 공백·한글 cwd, `.exe`/`.cmd`/npm shim argv/env, marker와 process tree 조회, root만 교체/같은 pane id 교체 | expected nonce+root/provider 창조시각·native session id 일치; 다른 incarnation은 거절. Windows foreground group 부재를 빈 목록/Dead로 치환하지 않음 |
| U06 실제 실행 계정/elevation | native 임시 harness에서 현재 서비스 launch **코드와 동등한 계정 경계**로 candidate 발견/pipe 접속·동일 SID 확인. `whoami /user`, owned process token SID/elevation만 기록(운영 설정 값 금지) | 설치된 사용자 바이너리를 실행 계정이 찾고 같은 허용 계정으로 접속. 불일치는 생성 전 오류; 공유 설정·승격·설치 자동 변경 0 |
| U07 native 첫/후속 입력·출력 | 격리 native `cargo test <windows_herdr_claude_codex_roundtrip>` 또는 harness；첫 지시·후속·다중행/한글·사람 draft·busy·modal, JSONL/Hook/rollout 수집 | 입력 owner 유지, 실제 user/native 결과 1회, source/receipt/O cursor 정합, hook 주소·파일 path가 Windows에서 성립. S11/S12 |
| U08 재시작/복구 crash 행렬 | 임시 dcserver만 host create 앞/ACK 유실/위치 CAS 앞/Bound idle/turn 중간/완료 뒤/O 게시 앞에 종료하고 같은 sandbox로 재기동 | 같은 server/pane/root/provider/session/nonce, live 실행 수1, 누락 원문0, output 중복0; boot/periodic wake·standby injection0. terminal attach 호출0. 해당 native 로그 없으면 DoD1 미충족 |
| U09 Cancel/Terminate/clear/stop | 임시 native roundtrip 중 기존 Cancel/Stop/HTTP force/clear/reset 요청·close 앞/뒤 daemon crash·missing marker·foreign nonce 수행 | cancel 뒤 pane 생존, 종료 뒤 owned root/provider/child0, 오류면 row/원문 보존. 생존 foreign pane0변경. S05/S12/D04 |
| U10 Qwen·tmux 없음·기본값 | native Qwen discovery/첫 입력/후속/source 포맷 검증; Unix harness PATH에서 tmux 제거 후 기본/명시 Herdr·bool/None 선택 표 실행 | Qwen 가능하면 same contract; 불가 시 Windows Qwen 명확한 unsupported와 원문보존. Unix tmux 없음 start refusal1·silent loss0. 기본값 표 모든 칸 실제 기대와 일치 |
| U11 Core 제공/버전·API | 현재 `where hc` 미발견. 준비된 격리 Windows Core에서 `hc --version`, `hc schema`, `hc --skill` 저장하고 named pipe/ConPTY run/read/interrupt/kill/rm exact owned terminal 시험 | 측정한 버전·schema 해시 고정, 입력 quiet/ack/필드·권한 경계 확인. 0.1.14와 최신 0.1.21을 섞지 않음. C01 차단 |
| U12 Core holder·parent 수명 | `HC_SESSION=i7583-<nonce>` 임시 core와 parent/child를 `hc run --bg --name ...`로 실행; 직접 만든 core만 재시작, parent wait/message/timeout 및 parent종료 실측 | core 재시작은 같은 program/terminal을 재채택; parent 종속/child 취소·outlive-default 정확. daemon 재시작이 잘못 parent 종료로 전파되면 C02 차단 |
| U13 F4 실제 결과/OSC | 준비된 Core에서 `hc run --bg --name lane-<nonce> --cwd <fixture> <provider> <first-task>`; `hc wait <id>`/`--children`, `hc read <id>`, attempt artifact 검증 | 부팅 중 send0, 첫 지시1, OSC 7501은 상태만, 결과 ID·exit/본문 일치, message/timeout/not_started를 완료로 취급0. F4 lane1 실제 기록 필요 |
| U14 Core OS 재부팅/loss | 코디네이터가 제공한 **격리 Windows VM**에서 Core owned fixture terminal 실행→VM reboot→`hc ls`/read/recovery event와 F4 attempt 확인. win-desktop 운영 호스트 재부팅 금지 | 기존 program 소멸이면 같은 agent 복구 성공으로 보고0; keep가 shell을 만들어도 old attempt Lost/취소. task 자동 재송신0·고아0. C02 DoD 차단 |

U01~U14를 골라 일부만 발사하지 않는다. 패킷 전체에 PASS/FAIL/NOT_RUN과 raw 로그·버전·해시·PID 생성시각을 기록한다.
실측 불가를 성공이나 빈 실패 0으로 보고하지 않는다. U03/U04/U05 중 핵심 계약이 불가능하면 A 활성화 전에
DESIGN_DELTA로 돌아오며, 설치/서비스 변경이나 수동 kill 런북으로 우회하지 않는다.

## 모델·검증·리뷰 집중 항목

[모델 보고서](../statecharts/session-host/session-host.report.md): 새 10 cfg, 최종 checker rc0,
4 GREEN/6 기대 RED. `RestartEventuallyReconnect`는 PROPERTY이고 applied WF를 사용한다.
no-loss/no-double/no-orphan을 검사했다. Windows OS 계약은 가정이며 네이티브 실행 PASS가 아니다.
기존 session-lifecycle/host-migration의 역사 모델/기대 RED는 그대로 두고 이번 모델에 현재 좌표와 변화시킬 가정을 연결했다.

| 리뷰 집중 항목 | 막은 방법/남은 반례 |
|---|---|
| terminal attach 없이 dcserver 재시작 재채택 | 기존 API/source 재채택 경로 사용. Windows peer/Job/native source는 U03~U08 미측정; CurrentWindowsHerdr cfg는 이를 드러내는 기대 RED |
| Unix tmux 부재 fallback 삭제 | 선택 host unavailable를 payload 전 명시 오류/원문 보존; Herdr 자동 demotion·Process fallback0. U10 및 selector/StartFailure 회귀 필요 |
| 부팅·정리·시험 대역 진입점 누락 | 4 raw 전수 TSV + 효과 기준표/consumer-slices + FIFO 보충. source-scan과 final bypass 재검색을 각 slice 수용 조건으로 고정 |
| 채널 선택 기본값 | bool/None/endpoint/Codex 별도 gate/Unix tmux 표를 고정. Windows Process→Herdr는 요구된 예외를 명시 |
| 경계 시각 | create/위치 CAS, close/retire를 모델에서 분리. input ACK≠native receipt≠O 완료 |
| 동시 경합 | existing Pending CAS/transition/source authority/claim/first-key gate 유지. replay mutant는 이중 세션 반례 |
| 옛 형식 복원 | unknown raw kind/host locator 보존·fail closed. Process 삭제 후 None을 tmux로 해석하는 회귀를 D03/D04에서 차단 |
| 문서로만 선언한 guard | Windows 활성화는 실제 pipe/identity/Job/hook/recovery 시험에 묶고 cfg 해제만으로 승인하지 않음 |
| 잘못된 0/PASS | raw grep 후보와 실제 prod call 수, 모델과 OS 관측, 설계와 구현을 구분. cargo/native Windows/PG는 이번 레그 NOT_RUN |
| 기존 정리 권한 확대 | `src/services/provider_teardown.rs:24` clearance, `src/services/claude/herdr_return.rs:83` end-before-retire, `src/services/discord/tmux_reaper.rs` 범위 유지. 모르는 PID/foreign root kill이나 config/service 쓰기 추가0 |

커밋 전 `scripts/ci/statechart_index.py`로 번호 650 색인을 생성하고 diff/static/크기 검사를 기록한다.
이 문서의 검증 결과는 구현의 REVIEW_READY가 아니다. 독립 설계 리뷰와 U 패킷 결과를 받은 다음에만 구현 레그로 간다.
