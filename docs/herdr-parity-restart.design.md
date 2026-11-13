# Herdr `/restart` 사용자 동작 일치 설계

기준: `origin/main` = `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`, 2026-11-13 KST.
대상 이슈: [#8128](https://github.com/itismyfield/AgentDesk/issues/8128).
**초기 설계(1~6절)는 설계·문서·모델만 다뤘다. A 구현 범위와 갱신한 계약은 7절에 기록한다.**

이 문서의 목표는 인증된 `/restart`와 같은 구현을 쓰는 `!restart`에서 기존 실행을 끝내고,
가능하면 같은 대화를 새 incarnation으로 이어 시작하는 것이다. 진행 중인 턴은 기존 tmux와 같이
손실 가능성을 알리고 중단한다. 이미 접수한 대기 입력은 보존하며, 종료한 실행으로 다시 보내지 않는다.
새 incarnation의 준비/인사말 시작이 확인되거나, 확인하지 못한 이유가 사용자에게 보이는 것으로 요청을 끝낸다.

`/model`의 live 적용 → `/compact` → 세션 리셋/`tmux_recreate` → `!clear` 종료 기록 → 진단 화면은 후속 순서다.
일반 API·루틴·iMessage 시작은 #8125, busy 합류는 #8114, 양쪽 실제 시나리오 수용은 #8120에서 추적한다.
이 문서는 #8128 전체 DoD를 완료했다고 주장하지 않는다. 불가피한 사용자 차이로 새로 승인받은 항목도 없다.

## 1. 기준선과 진입점 전수표

선행 O-a의 shared child reader/store 준비는 main의 `aa630a5bbf9179452de42704d60a9b6167aeddcc`에 있으며
`src/services/tui_o/store/thread_children.rs:15`의 verified surface/activation guard를 그대로 사용한다.
S3의 continuation custody/barrier는 main의 `51508b1f8ec75f0cb6254ce49a3d46442ec7e6bc`에 있으며
`src/services/turn_orchestrator/queued_head/v2/effort_continue.rs:17`은 verified caller만 intent를 만들게 한다.
두 커밋은 기준 HEAD의 ancestor임을 확인했다. 준비된 store·owner original/ticket·barrier 기구를 재구현하지 않는다.
O-a는 child 활성화 전체가 아니고 S3는 command launch/취소 hook 자체가 아니다.
따라서 이 선행 착지만으로 herdr `/restart`가 충족됐다고 표시하지 않는다.

아래 좌표는 모두 위 기준 SHA의 코드이며 경로는 저장소 루트 기준이다.
경로를 생략한 두 번째 좌표도 같은 행의 파일이다.
설계 모델의 운영 값 범위는 이 표에서만 가져온다. 모델의 단계명은 이 호출 순서의 투영이며 새 생산 enum이 아니다.

| 경로/소비자 | file:line | 현재 동작·값·이번 범위 |
|---|---|---|
| 슬래시 등록·인증 | `src/services/discord/commands/restart.rs:352`, `:312`, `:315`, `:318` | `cmd_restart` → auth → slash policy → 공통 `restart_channel` |
| 텍스트 명령 | `src/services/discord/commands/text_commands.rs:129`, `src/services/discord/commands/restart.rs:336` | 인증된 intake의 `!restart`, 원 명령 메시지에 reply(`:343`) |
| 공통 restart | `src/services/discord/commands/restart.rs:259`, `:272`, `:293` | 진행 알림 → reset → seed → 결과. host별 orchestration을 나눌 위치 |
| 메모리 snapshot | `src/services/discord/commands/restart.rs:17`, `:45` | `NoActiveSession / ActiveSessionWithoutSessionId / ActiveSessionResumable`; 표시용 snapshot이지 실행 권한이 아님 |
| reset 전 herdr 거절 | `src/services/discord/commands/restart.rs:218` | `managed_reset_refusal(...true,true,None)`가 cancel보다 먼저 실행됨 |
| 공통 관리 가드 | `src/services/discord/admin_host_guard.rs:93`, `:124`, `:143` | `configured_refusal(:114)`로 herdr 설정 채널 거절. **이 가드를 전역 완화하지 않음** |
| row·marker 가드 | `src/services/discord/admin_host_guard.rs:15`, `:151`, `:164`, `:334` | legacy tmux 확인, unknown/conflict/hosted 세션의 이름 기반 조작 차단 |
| 현재 active 취소 | `src/services/discord/commands/restart.rs:223`, `:225`, `:227` | 경고 → mailbox cancel → `stop_active_turn(PreserveSession)`. herdr branch에서 이 선취소를 쓰면 exact terminate의 captured token 증거를 잃을 수 있음 |
| tmux reset 인자 | `src/services/discord/commands/restart.rs:239` | `reset_provider_state=!supports_resume(:245)`, history 보존(`:246`), recreate 요청(`:247`) |
| 기존 인사말 | `src/services/discord/commands/restart.rs:13`, `:133`, `:151` | `안녕하세요. 세션 재시작 완료.`를 `start_headless_turn`으로 제출 |
| seed 재시도·결과 | `src/services/discord/commands/restart.rs:55`, `:166` | `Started/Busy/Failed/RetryFailed`; pre-spawn Conflict만 1초 뒤 한 번 재시도. accepted 요청 재전송 없음 |
| 일반 headless 거절 | `src/services/discord/router/message_handler/headless_turn.rs:147`, `:159`, `src/services/turn_host.rs:177` | 현재 herdr `executor_not_wired`; 기존 seed를 그대로 부르면 종료 뒤 시작 실패 |
| provider reset | `src/services/discord/commands/control/provider_reset.rs:3`, `:16`, `:24`, `:38`, `:49`, `:58` | guard 뒤 memory/DB selector·history·process·tmux reset. 다른 명령까지 열리므로 전체 재사용 금지 |
| tmux recreate | `src/services/discord/commands/tmux_recreate.rs:9`, `:38`, `:57` | inflight admission 뒤 kill, 성공 때만 temp cleanup |
| 세션 보존 relaunch | `src/services/discord/commands/tmux_recreate.rs:80`, `:91`, `:102` | 관리 guard, account marker 보존, kill/cleanup/복원. herdr 후속 범위 |
| pending 설정 소비 | `src/services/discord/commands/control.rs:225`, `:246`, `:334` | herdr로 바뀐 relaunch는 해제/다음 기동 안내; guarded provider reset 호출. 후속 범위 |
| `/stop` | `src/services/discord/commands/stop.rs:84`, `:91`, `:75` | Herdr stop 결과에 답한 뒤 별도 cancel/cleanup 안 함 |
| Herdr stop | `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:345`, `:370` | interrupt intent + Escape. close 권한과 다름 |
| 비강제 Cancel | `src/services/queue/herdr_stop.rs:13`, `:24`, `:42` | holder stop, provider End 대기; 완료나 inflight 삭제로 위조하지 않음 |
| 강제 Cancel | `src/services/queue/http_force.rs:33`, `:86` | exact owner/dispatch/nonce terminate + **queue purge**, restart에 purge 재사용 금지 |
| session force-kill | `src/services/dispatched_sessions/herdr_kill.rs:21`, `:60`, `:96` | 같은 terminate 서비스, queue 보존. retry dispatch 등 HTTP 후처리 전체는 가져오지 않음 |
| 명시 Terminate API | `src/server/routes/herdr_terminate.rs:40`, `:88`, `:93`, `:110` | 인증/switch, session_key+nonce, 선택한 pane proof, Retired만 HTTP 성공 |
| exact 종료 서비스 | `src/services/discord/herdr_terminate.rs:401`, `:437` | owner/local node/location/nonce(`:453`), captured turn(`:477`), transition(`:482`), same actor/token(`:486`), fence(`:519`), row 재검사(`:548`), exact close(`:590`), end read(`:594`), retire CAS(`:596`), settlement(`:603`) |
| 종료의 결과·postlude | `src/services/discord/herdr_terminate.rs:38`, `:61`, `:412`, `:224` | 기존 `after_retire`에서만 보유 exclusion 사용. retire CAS stale은 Indeterminate이며 성공 아님 |
| exact close 권한 | `src/services/termination_audit/host_terminate.rs:116`, `:126`, `:132` | live switch + terminate warrant, 일반 cancel warrant를 close로 승격하지 않음 |
| pane·server 정체성 | `src/services/session_host/herdr_gate.rs:141`, `:455`, `:473`, `:496`, `:658` | endpoint/session/pane/nonce fence, 같은 witness 서버에만 close 전달. 숫자 pane 재조회/다른 연결 fallback 금지 |
| exclusion 보유 범위 | `src/services/discord/herdr_terminate/retired_exclusion.rs:16`, `:99` | transition + old execution fence + actor/token; `disconnect_session_keeping_provider` 사용 가능 |
| 보존형 disconnect CAS | `src/db/dispatched_sessions/hosted_execution/http_force.rs:34`, `:45`, `:47` | retired/owner/nonce/dispatch 조건 아래 provider selector 유지. **successor Pending 설치 전에만 적용** |
| `/clear`, `!clear` | `src/services/discord/commands/control.rs:510`, `:523`, `:552`, `:562`, `:573`, `:599` | transition 3초 timeout에도 계속 수행; queue/history/selector 폐기. restart와 의미가 다름 |
| Herdr clear teardown | `src/services/discord/commands/control/clear_teardown.rs:70`, `:137`, `:179`, `:192` | 동일 `terminate_explicit_herdr` 서비스가 재사용 대상 |
| clear 전용 부작용 | `src/services/discord/commands/control/clear_teardown.rs:175`, `:183`, `:186` | retired selector 삭제 postlude. `end_herdr` 전체를 그대로 호출하면 resume 보존 위반 |
| clear의 기존 fallback·기록 | `src/services/discord/commands/control/clear_teardown.rs:150`, `:167`, `:80`, `:94`, `:97` | unknown/pool 없음은 end_unconfirmed 후 managed fallback 가능. tmux 전용 end record 차이는 후속으로 남기고 확대하지 않음 |
| 소유 입력 종료 cleanup | `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:9`, `:42`, `:63`, `:112` | nonce/receipt/cleanup owner 전용 계약. restart에서 terminal 입력을 되살리는 용도로 쓰지 않음 |
| hosted 상태·읽기 | `src/db/dispatched_sessions/hosted_execution.rs:27`, `:171`, `:228` | `Pending/Bound/Retired`; `Legacy/Known/Unknown`, lookup `Missing/Found/Unknown/Conflict` |
| herdr 새 launch | `src/services/herdr_launch.rs:378`, `:395`, `:433`, `:466`, `:492`, `:515` | RuntimeHintFence → row → Retired hold 검사 → 새 nonce → Pending CAS/marker → create |
| create/launch 결과 | `src/services/herdr_launch.rs:111`, `:210`, `:226` | `Created/NotSent/Indeterminate/CreatedUnconfirmed`; `Launched{evidence Ok/Err}/Indeterminate`; 불명 ACK는 Pending 유지, 재생성 금지 |
| incarnation 준비 | `src/services/herdr_launch.rs:732`, `:758` | 새 UUID, provider/channel/logical/runtime/native session/launch mode를 묶음 |
| Claude resume 준비 | `src/services/claude/herdr_turn/handoff.rs:6`, `:17`, `:29`, `:118` | account·transcript·O cursor·최신 binding·inode 검증, core session_id만으로 resume 안 함 |
| Claude 시작 순서 | `src/services/claude/herdr_turn/handoff.rs:99`, `:103`, `:107`, `:108` | hook 사전 구독 → launch → 자기 SessionStart → attach → prompt |
| Claude migration 종료 | `src/services/claude/herdr_turn/handoff.rs:60`, `:63` | `earlier_tmux`를 종료하는 migration 분기. Herdr→Herdr restart에서는 실행하지 않을 부분 |
| Codex 현재 시작 | `src/services/codex/herdr_turn.rs:188`, `:200`, `:277`, `:434` | fresh native selector=None/resume=false, 자기 Startup source, first input 뒤 Source attach |
| Codex argv 가드 | `src/services/codex/tui_session_launch/launch_policy.rs:170`, `:178`, `:210`, `:256` | 준비 전과 최종 argv 모두 fresh-only 검증. resume 모드 지원을 따로 추가해야 함 |
| Codex 출력 경계 | `src/services/codex/herdr_turn/first_input.rs:195`, `:208`, `:211`, `:225` | 첫 입력 뒤 hook/source/attach, 새 rollout 가정 offset=0. resume에 그대로 쓰면 과거 출력 중복 위험 |
| transition 중 intake | `src/services/discord/router/message_handler/intake_turn/runtime_transition.rs:37`, `:43`, `:78` | 새 입력 durable tail, 기존 owner front는 원본 유지 + release wake |
| busy owner 입력 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:216`, `:239` | transition 실패는 `NotSent(session_transition_busy)` |
| 기존 owner synthetic 시작 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:621`, `src/services/discord/queue_io/input_owner/start.rs:13`, `:59` | retry original/ticket 보존·admission. idle context는 `owner_input.rs:909` → headless 거절로 돌아가므로 전용 연결 필요 |
| 기존 transition lease/wake | `src/services/discord/session_transition.rs:38`, `:120`, `:145` | 실제 shared/channel mutex에만 borrow 허용, Drop 이후 installed-owner wake |
| owner 내구 상태 | `src/services/turn_orchestrator/queued_head/v2_types.rs:20`, `:38`, `:81`, `:93` | V2 optional handoff/effort intent, 원본/attempt/target epoch. 새 restart intent 제안의 저장 위치 |
| 입력 없는 boot debt | `src/services/turn_orchestrator/queued_head/compat.rs:23` | `holds_input`는 items/head/retained/effort만 셈. restart intent만 남는 파일도 발견하도록 연결해야 함 |
| boot census 필터 | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs:88`, `:92` | `v2_channels`가 holds_input/unfinished Claude handoff만 선택. restart intent-only·알림 debt도 owner 복구에 올려야 함 |
| 부팅 큐/actor | `src/services/discord/runtime_bootstrap/queued_recovery.rs:48`, `:65`, `:129`, `:155`, `:189`, `:210` | 큐 먼저 복원, actor dedup·persist 실패는 원본 Held, 그 뒤 inflight |
| 부팅 intake gate | `src/services/discord/runtime_bootstrap.rs:34`, `:41` | 복원이 끝난 뒤 intake 오픈, installed-owner wake |
| inflight 재수화 | `src/services/discord/recovery_engine/restore_inflight.rs:136`, `:198`, `:219` | receipt/Herdr settlement, runtime_kind absent legacy 허용, unknown/newer는 추정 복원 금지 |
| TUI pending 재수화 | `src/services/discord/tui_prompt_relay/synthetic_start.rs:2044`, `:2073` | pending index/worker 복원, prompt 자체 재전송 아님 |
| auto-queue·진단 | `src/services/auto_queue/runtime.rs:485`, `src/services/discord/commands/diagnostics/reports.rs:46` | herdr reset 건너뜀/화면 unsupported. 후속 조각에서 처리 |
| 루틴 reset·runtime clear | `src/services/routines/session_control.rs:105`, `src/services/discord/health/recovery.rs:267` | 같은 managed guard를 소비. 큐를 clear하는 후처리도 있으므로 restart에서 통째 호출하지 않음 |
| 에이전트 상태·출력 API | `src/services/agents/turn.rs:309`, `src/server/routes/agents.rs:186`, `src/services/dispatched_sessions/output.rs:171` | row/session-key guard로 tmux capture/probe를 거절하거나 unsupported로 표시. 이후 진단 조각 |
| 에이전트 CRUD·auth | `src/server/routes/agents_crud.rs:1037`, `src/server/routes/provider_auth_profiles.rs:518` | legacy-tmux/auth-login target 가드. restart의 별도 경로가 이를 전역 개방하지 않음 |
| 명시 resume rebind | `src/services/session_resume.rs:404` | herdr 설정이면 transcript 복사·binding·teardown 전에 HostUnsupported. restart 준비의 검증 포트와 구별 |
| stale/relay 복구 | `src/services/discord/router/intake_gate/stale_turn.rs:191`, `src/services/discord/relay_recovery/apply.rs:100` | turn release 거절 시 token/state 보존. 새 incarnation 때문에 old token을 무조건 삭제하지 않음 |
| watcher·idle·inflight 복구 | `src/services/discord/tmux_watcher/host_gate.rs:153`, `src/services/discord/session_idle_cleanup.rs:75`, `src/services/discord/inflight/host_recovery_guard.rs:103` | herdr 설정 채널의 이름 기반 teardown/clear 건너뜀. restart activation으로 이 보호를 약화하지 않음 |
| CLI·provider cleanup·활성 판정 | `src/cli/utils.rs:176`, `src/services/provider_cli/session_guard.rs:199`, `src/services/provider/cancel_token_cleanup/executor.rs:162`, `src/services/session_activity.rs:49` | marker가 다른 host면 이름 기반 kill/probe를 거절/건너뜀. old nonce settlement와 별도 보호 |
| TUI passthrough | `src/services/discord/commands/tui_passthrough.rs:176` | channel guard로 tmux passthrough 거절. `/model`·`/compact` 후속과 guard를 공유하므로 전역 우회 금지 |
| I/O 관측의 대기 한계 | `src/services/session_host/herdr/transport.rs:30`, `:84`, `:159`, `src/services/claude/herdr_turn.rs:63`, `src/services/codex/herdr_turn.rs:56` | 기존 socket read/write timeout·read deadline, Claude SessionStart/Codex composer/source wait를 재사용. DB·알림/작업 scheduler의 진행은 별도 liveness 전제 |

### 입력 출처와 불변식의 수락 경계

| 출처 | file:line | 수락·재시작 중 처리 |
|---|---|---|
| Discord 사람의 일반 메시지 | `src/services/discord/router/intake_queue_transaction.rs:35`, `:121`, `src/services/discord/router/intake_gate.rs:366` | Human, 원본 message/source IDs·queued generation 유지. 등록 slash는 일반 메시지 큐에 중복 투입하지 않음 |
| Discord 허용 봇 | `src/services/discord/router/intake_gate.rs:347`, `src/services/discord/router/intake_queue_transaction.rs:43` | AllowedBot/announce만 기존 정책대로 접수. 재시작 중 입력 custody는 사람과 동일, 명령 권한은 기존 auth 그대로 |
| 기타 봇 | `src/services/discord/router/intake_queue_transaction.rs:39`, `src/services/discord/router/intake_gate.rs:347` | OtherBot의 기존 비접수 정책 유지. 수락하지 않은 입력을 loss=0 계수에 넣어 가짜 성공으로 만들지 않음 |
| TUI에서 시작된 실제 턴 | `src/services/discord/tui_prompt_relay/synthetic_start.rs:2044`, `src/services/discord/herdr_terminate.rs:477` | O/합성 start로 관측한 old inflight를 captured token 기준 결산. 이미 시작된 턴은 중단 경고 대상 |
| TUI의 아직 제출하지 않은 수동 draft/직접 키 | `src/services/session_host/herdr_gate.rs:555`, `src/services/discord/commands/restart.rs:276` | AgentDesk가 수락한 queued original과 구별. 종료할 pane의 draft·진행 중 턴은 기존 tmux restart처럼 중단될 수 있음. 직접 키 입력 자체를 무손실 접수했다고 주장하지 않음 |
| HTTP·루틴·iMessage 시작 | `src/services/discord/router/message_handler/headless_turn.rs:159`, `src/services/turn_host.rs:177` | 현재 herdr 거절 유지(#8125). 이 설계의 typed seed permit을 일반 API 사용자가 만들 수 없어야 함 |
| restart 인사말 | `src/services/discord/commands/restart.rs:13`, `src/services/discord/queue_io/input_owner/start.rs:13` | 인증된 명령의 stable operation ID에서 유도한 한 ticket/original. 일반 source 문자열과 다른 내부 권한으로만 successor에 제출 |

수락한 입력은 owner의 durable original/attempt로 보유한 입력이다. 재시작 수락 선형화는
**old channel transition + old nonce termination fence 획득 및 현재 row 재검사가 끝난 시점**이다.
그 전 이미 시작된 send는 기존 receipt로 `NotSent / acknowledged / uncertain`을 결산한다.
`NotSent`만 새 target에서 시도할 수 있고, uncertain은 보존·표시하며 맹목 재전송하지 않는다.
명령 텍스트가 도착한 시각부터 모든 native 키를 막았다는 더 강한 보장은 현재 코드에 없다.

## 2. 사용자 수준 비교

| 기능/상황 | tmux 현재 | herdr 현재 | 같음·다름 / 맞추는 방법 |
|---|---|---|---|
| 인증된 `/restart`, `!restart` | reset 뒤 즉시 seed | cancel 전 거절 | 다름 → restart 전용 host dispatch와 exact terminate/launch 연결 |
| 살아 있는 실행 중 재시작 | 턴 손실 가능 경고, 취소 | 턴/프로세스 유지 | 다름 → 경고 후 exact captured old turn만 retire/settle |
| 대화 이력 | 보존 | 동작 없음 | 다름 → clear/history purge를 호출하지 않음 |
| 지원 provider의 native session | selector 보존, resume 시도 | 동작 없음 | 다름 → Claude 검증 재사용, Codex resume/source 경계 선행 지원 |
| native session 없음 | fresh로 시작, 이유 안내 | 거절 | 다름 → 동일한 fresh 결정, 확인된 부재에서만 시작 |
| 활성 실행 없음 | seed가 workspace 해석 후 시작 | 거절 | 다름 → 기존 owner/workspace/host 검증을 거친 fresh branch |
| 대기 입력 | reset에서 history/queue clear 요청 안 함 | 기존 큐 그대로 | 부분 같음 → herdr restart도 original/receipt/순서 보존, release wake 필요 |
| 재시작 중 새 Discord 입력 | 기존 transition/queue 정책 | 기존 transition/queue 정책 | 기구는 같음 → close→Pending→seed custody까지 exclusion 연결 |
| 옛 incarnation으로 입력 | tmux 종료/재생성 | restart 없음 | 다름 → old fence 유지, 새 nonce/target epoch를 재판정한 owner만 제출 |
| 인사말 exactly once | accepted seed 재시도 안 함 | headless start 거절 | 다름 → typed permit + 기존 owner ticket/receipt, uncertain seed 재전송 금지 |
| 성공 표시 | `Started`는 start 반환, Ready 증거 아님 | 거절 설명 | 다름 → herdr는 current nonce·source binding·seed 실제 제출 증거로 표시. admission만으로 Ready를 말하지 않음 |
| close/create 미확인 | 일부 기존 결과가 요청 결과만 표시 | 명령 거절 | 다름 → herdr unknown ACK/identity는 Pending/custody 보존 + 사용자 실패. 이름 fallback/임의 kill 없음 |
| 동시 `!clear` | clear가 명시적으로 큐 폐기 | clear가 명시적으로 큐 폐기 | 같음 유지 → clear 우선, restart는 generation 변경을 보고 superseded 실패. 새 큐/옛 큐를 되살리지 않음 |
| provider 설정 적용 | 기존 launch 인자 사용 | fresh launch 설정 경로 사용 | 새 launch도 같은 검증을 사용. `/model` live 변경 수용은 후속이며 이 조각으로 완료 표시 안 함 |
| 일반 API/루틴 시작 | headless 시작 가능 | `executor_not_wired` | 범위 밖 다름: #8125. restart seed만 좁게 연결하며 전체 허용 금지 |

위 행의 herdr 미구현은 불가피한 차이로 인정하지 않는다. 오류·identity 미확인은 성공과 구별한 실패이며,
tmux의 기존 오판까지 복제해 parity라고 부르지 않는다.

## 3. 구현 방향과 경합 처리

새 host trait 또는 Core를 만들지 않는다. 기존 `terminate_explicit_herdr`,
`RetiredExclusion`, `launch_herdr_session`, provider 준비/attach, owner original/ticket/receipt를 연결한다.
`clear_teardown::end_herdr`가 사용하는 종료 서비스를 재사용하며 clear의 selector 삭제 postlude는 재사용하지 않는다.

### 요청과 종료

1. 기존 명령 auth/policy를 통과한 뒤 `/restart` 전용 host 분기로 들어간다. legacy tmux branch는 유지한다.
   canonical owner·provider·channel·local node·session_key·old nonce를 고정한다. Unknown/Conflict/foreign owner,
   지원되지 않은 launch/resume 조건은 종료 전에 보이는 실패로 끝낸다. source 문자열만으로 이 권한을 만들 수 없다.
2. destructive effect 전에 기존 V2 owner snapshot에 optional `restart` intent를 저장한다(제안).
   operation은 interaction ID 또는 `!restart` message ID에 token/provider/channel namespace를 결합한다.
   old key/nonce, clear generation, 검증할 native selector, successor nonce, seed ticket/original,
   stage와 결과 알림 debt를 보관한다. row/native binding은 재검사 대상이며 저장값 자체가 권한은 아니다.
   같은 진행 중 operation의 재호출은 같은 결과/seed를 사용한다. 다른 동시 요청은 명시적인 Busy 실패로 끝낼 수 있다.
3. 기존 exact terminate를 호출한다. 기존 tmux branch의 `mailbox_cancel_active_turn`을 먼저 호출하지 않는다.
   서비스가 old transition/fence를 잡고 row/actor/token을 다시 검사한 시점을 수락 경계로 삼는다.
   exact close → pane/provider 종료 관측 → retire CAS → captured old inflight/actor settlement를 그대로 사용한다.
   close 불명·provider 잔존·stale CAS는 새 launch 허가가 아니다.
4. `after_retire`가 받은 exclusion 안에서 `disconnect_session_keeping_provider`를 **새 Pending 전에** 실행한다.
   old exclusion의 CAS를 successor row에 다시 쓰지 않는다. clear queue purge/history erase/catch-up discard는 호출하지 않는다.

### successor와 seed

5. 같은 exclusion을 유지한 채 provider 준비와 lower `launch_herdr_session`을 호출한다.
   필요한 borrow는 기존 `SessionTransitionLease::borrow`로 shared/channel mutex를 확인하는 작은 accessor로 연결한다.
   lower launch 자체는 channel transition을 재취득하지 않고 RuntimeHintFence만 잠시 잡는다.
   기존 full owner/headless kickoff 또는 unplaced resolver를 이 안에서 기다리지 않는다(transition 재취득 교착/timeout).
6. Claude는 `resumed_session/current_session`의 account·transcript·cursor·inode 확인과 launch/SessionStart/attach를 재사용한다.
   `launched_source` 안의 migration `earlier_tmux` 종료는 실행하지 않는 준비 포트를 만든다.
   같은 이름 tmux가 관측되어도 이 조각에서 추가 kill하지 않고 충돌을 알린다.
7. Codex는 fresh 가드를 유지하면서 명시적 resume 준비를 추가한다. native session/계정·선택 transcript·inode·prelaunch EOF와
   O binding watermark, 자기 새 nonce의 SessionStart를 검증한다. resume argv만 허용하거나 resumed source를 offset 0부터 읽지 않는다.
   첫 입력 이후 source가 생기는 Codex에서는 seed 전에 Bound를 기다리면 교착한다. composer/evidence 확인 → seed → 자기 source attach 순서다.
8. 새 nonce Pending은 canonical CAS로 먼저 고정된다. `CreatedUnconfirmed`, evidence gap, lost ACK이면 기존 Pending을 유지하고 실패를 표시한다.
   요청 cancel/timeout 뒤 `create`를 재전송하거나 새 pane을 임의 정리하지 않는다. boot는 canonical Pending을 대조·기존 resolver로 판단한다.
9. successor에만 유효한 내부 `RestartSeedPermit`(제안)을 발급한다. 검증값은 operation/provider/channel,
   owner namespace, current successor nonce/target epoch, stable ticket, clear generation이다.
   기존 owner synthetic original/admission을 이용하되 restart seed를 일반 headless early-refusal 우회 문자열로 취급하지 않는다.
   giant `headless_turn.rs`/`owner_input.rs`에는 typed 연결·거절 분기만 두고 검증은 작은 child에 둔다.
10. seed original/intent의 내구 저장을 마친 뒤 exclusion을 놓고 기존 installed-owner wake를 호출한다.
    old prepared payload/old target을 복사하지 않고 successor의 실제 source/composer를 다시 판정한다.
    seed는 startup slot에서 먼저 결산하고 기존 사용자 original끼리 FIFO를 유지한다.
    실제 제출·새 source binding·current row를 확인한 뒤 Ready/인사말 시작을 표시한다.
    `Admission::Accepted` 또는 `Launched` 반환만으로 Ready를 표시하지 않는다.

### clear·crash·재수화

`!clear`는 3초 transition timeout 뒤에도 진행한다. 따라서 exclusion만으로 clear를 막았다고 선언하지 않는다.
launch 직전, seed custody commit/실제 제출 직전, Ready 게시 직전에 기존 clear/queued generation을 재검사한다.
clear가 앞섰으면 이전 restart intent는 superseded 실패이며, 이미 clear가 버린 입력은 그 terminal receipt로 결산한다.
restart가 clear 이후 세션을 임의로 살리거나 옛 queued original을 복원하지 않는다.
그 순간 존재하는 새 Pending/pane이 있으면 canonical row와 기존 clear/terminate 권한으로 처리하며,
정체성이 불명인 pane을 restart가 더 찾아 kill하는 권한은 추가하지 않는다.

intent만 남고 items가 0인 crash도 boot에서 발견해야 한다. `SnapshotV2::holds_input`/compat·queue census·owner recovery를 연결하고,
기존 부팅의 queue 먼저 → inflight → intake open 순서를 지킨다. stage·canonical old/successor row·seed receipt를 대조해 이어가거나
사용자 실패/알림 debt를 결산한다. `Prepared` intent만으로 close/create를 재실행하지 않는다.
create effect와 ACK/intent 반영 사이 crash는 canonical Pending/nonce로 조사하며, 불명 create를 새 create로 대체하지 않는다.
새 field가 없는 옛 V2는 `serde(default)=None`; legacy snapshot과 runtime_kind absent 복원은 기존 정책을 유지한다.
unknown/newer 형식을 legacy로 다운캐스팅하거나 큐 파일을 지우지 않는다.

진행성은 scheduler가 관측/알림 작업을 계속 수행하고 I/O가 답하거나 deadline으로 분류되는 조건 아래 보장한다.
전송 불능인 Discord에 메시지가 도착했다는 보장은 만들지 않는다. 결과 알림 실패는 durable debt로 남기고 부팅/기존 owner wake에서 재시도한다.
Ready timeout과 실패 표시는 session cleanup·seed replay 권한과 분리한다.

## 4. 구현 분할과 크기

현재 예상은 **prod 14~19파일, 순증 +1100~1450**이다. +1200을 넘을 가능성이 있으므로 두 리뷰 조각으로 나눈다.
각 조각은 prod 30파일/+1500 이하이며, 함수/시험을 단순 이동해 수치를 맞추지 않는다.
한 기능의 PR·활성화는 둘을 묶어 한 번 한다. 실제 구현 착수 전 main 드리프트와 추정을 다시 확인한다.

| 조각 | 예정 생산 파일/연결 | 순증 추정 | 수용 경계 |
|---|---|---|---|
| A: Codex resume launch/source 경계 | `src/services/codex/herdr_turn.rs`, 새 `herdr_turn/restart_resume.rs`, `first_input.rs`, `tui_session_launch/launch_policy.rs`, 필요 시 기존 launch options/source child (4~6파일) | +350~500 | 기존 fresh 검사 유지, resume native/argv/source EOF/nonce 검증. 기존 생산 prepare/first-input 경로의 소비되는 기능으로 연결하고 미사용 dormant API/allow를 만들지 않음 |
| B: restart 수명·owner seed·boot | `src/services/discord/commands/restart.rs`, 새 `commands/restart/herdr.rs`, 새 작은 seed/intent child, `src/services/discord/herdr_terminate/retired_exclusion.rs`, 기존 provider 준비 포트, `src/services/discord/queue_io/input_owner/start.rs`, `owner_input.rs`/`headless_turn.rs` 연결, V2 types/compat/actor child, `runtime_bootstrap/queued_recovery.rs` 연결 (10~13파일, 공통 파일 중복 가능) | +750~950 | 공개 실제 명령을 RED→GREEN, queue/seed/Ready/실패·복원까지. 일반 API 거절 및 인접 관리 guard 유지 |

`headless_turn.rs` 1545줄, `provider_dispatch/owner_input.rs` 1567줄은 giant다(기준선 실측).
여기에는 선언·typed 연결·거절 분기만 추가한다. `herdr_gate.rs` 같은 giant에 restart orchestration을 넣지 않는다.
시험 추가 시 공식 inventory/선택 경로 동기화로 required PR job 선택을 확인한다. baseline·allowlist·감사 임계값은 이 설계의 완화 대상이 아니다.

## 5. 검증 계획과 운영 관측

설계 모델의 GREEN은 생산 실행의 GREEN이 아니다. 이번 레그에서는 아래 Rust/PG/E2E를 실행하지 않았다.

| 검증 | 실제 진입점·RED → GREEN | 원인 제거 뮤턴트 / 필수 단언 |
|---|---|---|
| 명령 end-to-end fixture | 기존 `commands/restart.rs:374`/`:423` host-guard PG seam과 `restart_channel`, slash/text auth 뒤 공통 실행 | herdr branch 제거 → 정상 herdr 재시작 거절 RED. auth 거절은 close/create/seed 0이며 실제 명령 결과 포함 |
| 종료의 정확성 | `terminate_explicit_herdr` + witnessed fake transport/PG row + actual command branch | exact fence 또는 identity 재검사 제거 → old/server/pane 교체에서 foreign close/old input RED. close ACK만 성공 처리 mutant → provider 잔존 RED |
| 입력 custody·동시성 | restart의 fence/close/retire/Pending/seed/release 각각 barrier에서 실제 Discord intake·owner drive 실행 | purge 연결, old target 재사용, release wake 제거 → 원본 ID/본문/upload/generation 손상·중복 또는 영구 Held RED. 두 입력의 실제 count=각1 |
| uncertain 경계 | close/create/seed effect 직후 reply 유실·phase commit 전 crash fixture | blind create/seed retry → 물리 create 또는 seed 효과 2 RED. uncertain은 Pending/retained+보이는 실패이며 성공 count에서 제외 |
| Codex resume | A의 실제 `prepare_codex_herdr_launch`/first-input/attach 경로, 기존 transcript가 있는 fake-provider round trip | fresh validator 삭제만 한 변형, offset=0, foreign SessionStart 허용 → native mismatch/과거 출력 재배달 RED. fresh control도 계속 GREEN |
| Claude resume | command→준비→SessionStart/attach, account/transcript/inode/cursor fixture | latest binding 검사 제거 → stale selector RED. migration 종료 호출 mutant → 관계없는 tmux close 1 RED(정상 0) |
| clear 경합 | 실제 clear timeout 후 discard와 restart phase를 barrier로 교차 | generation 검사 제거 → clear 뒤 seed/Ready·옛 큐 부활 RED. 명시 drop receipt와 새 generation 입력을 각각 확인 |
| 중복·동시 요청 | 같은 interaction/message operation을 prepare/close/seed/결과 단계에서 다시 호출, 다른 요청과도 교차 | durable operation/ticket 대신 새 ID 생성 → close/create/seed 중복 RED. 다른 진행 중 요청의 Busy는 사용자에게 표시하며 기존 요청 custody를 덮어쓰지 않음 |
| boot·옛 형식 | items=0인 intent-only snapshot, seed receipt-only, Pending lost ACK, old V1/V2/runtime_kind absent | census/boot wake 제거 → 미발견 또는 미결 요청 RED. snapshot 필드 없음 control, unknown/newer는 추정 복원 0 |
| 결과 정확성 | close 실패·evidence gap·seed Pending·source bind timeout·Discord 전송 실패 | admission=Ready/0시험=PASS 변형을 실패로 검출. 결과 알림 debt와 실제 메시지 도착을 분리 |

각 구현 조각은 같은 head에서 대상 시험의 선택 수=실행 수=성공 수, 실패·skip, PG 실제 실행,
바이너리 경로/SHA256·원시 로그·SHELL/TMPDIR/cargo profile을 남긴다. 원인 뮤턴트는 바이트 원복을 확인한다.
Rust 빌드가 필요할 때만 지정 `wait_headroom`/`seed_target`/cargo-shim 및 debug=0/strip=none을 적용한다.
본 설계의 정적 검사와 모델 실행 외 cargo/clippy/PG/E2E는 **미실행**이다.

실제 운영 수용은 코디네이터가 허용된 E2E 채널에서 수행한다. 이 레그는 herdr server/운영 프로세스/설정/DB를 조작하지 않는다.
tmux와 herdr 각각 idle·busy·queue 2개·resume·실패·동시 clear를 같은 시나리오로 실행한다.
관측은 command ID/operation, provider/channel, old→new nonce, close/retire 관측, create 수, seed ticket/receipt,
원본 message IDs별 queued/held/delivered/explicit-drop, 새 source/dev/ino/offset, actual Ready 결과 메시지를 결합한다.
완료 조건은 old exact pane/provider 부재, 새 current incarnation 1개, seed 1회, 접수 원본별 중복/유실 0,
또는 보이는 실패와 보존 원본이다. `Started`, HTTP 200, 로그에 error 없음만으로 수용하지 않는다.
새 관측은 기존 tracing에 operation/nonce/phase/outcome을 추가하는 정도로 제한하고 본문·비밀값은 기록하지 않는다.

### 운영 전제와 독립 리뷰 집중 항목

| 전제/집중 항목 | 코드 증거와 막을 방법 또는 남은 반례 |
|---|---|
| herdr server·정책 | `src/services/herdr_launch.rs:385` LegacyDedicated restore-off fresh witness, UserDefault grant `:456`; `src/services/session_host/herdr_gate.rs:496` 같은 서버. 버전 문자열이 아닌 기존 프로토콜/권한 판정을 사용하며 런타임 설정값은 이 레그에서 확인하지 않음 |
| pane identity | `src/services/discord/herdr_terminate.rs:453`, `:548`, `src/services/session_host/herdr_gate.rs:455` old nonce/owner/location + witnessed exact close. 같은 숫자 pane 재사용은 새 권한 아님 |
| launch 인자 | Claude `handoff.rs:17/29/85`, Codex `launch_policy.rs:178/210/256`, `herdr_launch.rs:732`. 기존 설정·계정·source 검증을 통과시켜야 하며 resume-only 가드 완화 금지 |
| 경계 시각 | request 도착·수락 fence·close effect·Pending commit·seed submit·source attach·Ready 게시를 분리. 이전 uncertain 입력은 replay하지 않음. 직접 native draft는 접수 원본 보장 밖임을 명시 |
| 동시성 경합 | old fence→retire→old disconnect→새 Pending→seed custody까지 exclusion 유지. clear는 timeout 후 진행하므로 generation 검사 필요. 두 번째 요청/Cancel/Terminate/actor 교체는 expected row/token 불일치로 보이는 실패 |
| 옛 형식 복원 | optional intent 없는 V2·legacy inflight는 기존 호환 유지. intent-only debt census 연결, unknown/newer 오인 복원 금지. 이 연결은 현재 main에 구현돼 있지 않음 |
| 문서로만 선언한 가드 | typed seed permit, restart intent/census, generation 재검사, provider별 Ready proof는 **새 구현·시험이 필요한 포트**. 기존 transition/fence가 이것까지 제공한다고 주장하지 않음 |
| 잘못된 0/PASS | 모델/current refusal의 안전 GREEN≠parity; owner Accepted/launch Launched≠Ready; PG skip/시험0≠PASS. 실제 물리 효과·선택 수·원본 receipt와 결과 메시지로 판정 |
| 기존 결함 넓히지 않음 | `src/services/discord/commands/control/provider_reset.rs:58`의 tmux recreate bool을 소비하지 않고 `:70` Applied로 끝나는 기존 가능성, clear teardown unknown fallback은 그대로 두고 herdr 성공 판단에 복제하지 않음. 후속에 넘길 사실이며 새 이슈/PR은 이 레그에서 쓰지 않음 |

## 6. 상태 차트 산출물

기존 색인의 `session-lifecycle` Herdr incarnation/BusyHandoff 모델을 확인했다.
과거 모델의 기준 SHA·기존 반례는 유지하고 같은 영역에 `HerdrRestart`를 추가한다.
값 범위·수정 전/후 표·INVARIANT/PROPERTY·TLC 실행 결과·Mermaid·재현 명령은
[`session-lifecycle/herdr-restart.report.md`](statecharts/session-lifecycle/herdr-restart.report.md)에 둔다.
모델은 새 코드가 아닌 설계 포트를 검사하며, 부팅 intent 내구성과 provider별 I/O는 위 실제 시험으로 증명해야 한다.

새 cfg 23행의 `docs/statecharts/expected.tsv` 등록은 A 브리프에서 승인되었다.
기존 baseline 행은 바꾸지 않고 신규 모델을 등록하며, A의 새 위반 집합도 같은 23개 설정에서 검사한다.

## 7. A 조각의 재개 경계 계약

설계 리뷰 P2-2/P2-5는 A에서 다룬다. P2-1/3/4/6과 owner가 successor를 시작하는 대안은 B 전 r2 게이트에서 판정한다.
이 조각은 `/restart` 명령이나 V2 restart intent를 활성화하지 않는다.

- `ResumeStart{operation,path,dev,ino,offset}`를 immutable `BindingContext.resume_start`에 보관한다.
  incarnation 파일은 fsync/부모 flush 뒤 Pending CAS에 쓰는 nonce로 고정되므로, seed나 부팅의 다른 프로세스도 같은 시작점을 읽는다.
  메모리 registry나 seed metadata를 원본으로 삼지 않으며, optional 필드가 없는 옛 fresh context는 그대로 읽는다.
- Codex resume는 명시 `HerdrLaunch.resume_start`와 `resume=true`, 정확한 native selector,
  같은 owner의 확인된 Retired predecessor가 함께 있을 때만 준비한다. 일반 `launch_of`는 selector가 남아 있어도 fresh다.
  argv의 resume 모드는 이 기록과 selector/파일 정체성을 확인한 분기에서만 열고 기존 fresh 검사도 유지한다.
- 시작점은 옛 실행 종료가 확인된 뒤의 complete-line EOF다. 같은 source의 유일한 비-retired O cursor가
  EOF와 정확히 같고 prefix hash가 맞을 때만 capture한다. cursor<EOF, cursor>EOF, 파일 교체/손상은 명시 거절한다.
  min/max로 합치거나 O cursor를 움직이지 않는다. fresh로 바꿀지/다시 기다릴지는 B의 정책이며 A는 자동 전환하지 않는다.
- 새 resume 프로세스의 metadata가 seed 전에 EOF를 늘려도 부팅에서 시작점을 재측정하지 않는다.
  immutable offset을 유지하고 path/native/dev/ino 및 `len>=offset`을 확인한다. `len==offset` 재검사는 새 metadata를 잘못 거절하므로 하지 않는다.
- resume의 자기 binding은 정확한 durable context와 파일을 가진 `Resume` 원인만 허용한다. fresh의 `Startup` 제한은 유지한다.
  첫 key 전에 context를 확인하고, 첫 입력 reader·중단 토큰의 시작점·hook rollout marker·runtime binding 복원 offset이 같은 기록을 사용한다.
  값이 없거나 다른 source이면 0으로 대체하지 않는다. 이 경로를 verified policy로 승격시키지도 않는다.
- 실제 시험은 기존 rollout+O cursor, 저장된 새 incarnation/Pending, 메모리 registry 소실과 O 재부팅,
  실제 seed/Resume hook/attach/native reply/O delivery를 통과한다. 과거 assistant 출력은 다시 게시하지 않아야 하며,
  정상 resume를 `Startup`만 기다리다 SOURCE_WAIT 실패로 만들지 않아야 한다.

<!-- doc-path-refs: proposed herdr_turn/restart_resume.rs commands/restart/herdr.rs -->
