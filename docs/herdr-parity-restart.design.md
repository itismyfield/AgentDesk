# Herdr `/restart` 사용자 동작 일치 설계

기준: `origin/main` = `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`, 2026-11-13 KST.
대상 이슈: [#8128](https://github.com/itismyfield/AgentDesk/issues/8128).
**설계·문서·모델만 작성했다. 생산 코드 변경, Rust 시험, 운영 실행은 0이다.**

이 문서의 목표는 인증된 `/restart`와 같은 구현을 쓰는 `!restart`에서 기존 실행을 끝내고,
가능하면 같은 대화를 새 incarnation으로 이어 시작하는 것이다. 진행 중인 턴은 기존 tmux와 같이
손실 가능성을 알리고 중단한다. 이미 접수한 대기 입력은 보존하며, 종료한 실행으로 다시 보내지 않는다.
새 incarnation의 준비/인사말 시작이 확인되거나, 확인하지 못한 이유가 사용자에게 보이는 것으로 요청을 끝낸다.

`/model`의 live 적용 → `/compact` → 세션 리셋/`tmux_recreate` → `!clear` 종료 기록 → 진단 화면은 후속 순서다.
일반 API·루틴·iMessage 시작은 #8125, busy 합류는 #8114, 양쪽 실제 시나리오 수용은 #8120에서 추적한다.
이 문서는 #8128 전체 DoD를 완료했다고 주장하지 않는다. 불가피한 사용자 차이로 새로 승인받은 항목도 없다.

**r2(2026-11-13):** 설계 리뷰 d1의 방향 제안을 판정해 3·4절을 다시 썼다. restart는 인사말 seed를 owner 큐 head에 먼저 적재하고
옛 실행을 정확히 끝낸 뒤 owner를 깨우는 데까지만 하며, successor launch는 owner의 기존 herdr 턴 경로가 한다.
1절의 진입점 표는 r1 그대로이고, 사용자 결정이 필요한 동작 차이는 3.7에 모았다.

## 1. 기준선과 진입점 전수표

선행 O-a의 shared child reader/store 준비는 main의 `aa630a5bbf9179452de42704d60a9b6167aeddcc`에 있으며
`src/services/tui_o/store/thread_children.rs:15`의 verified surface/activation guard를 그대로 사용한다.
S3의 continuation custody/barrier는 main의 `51508b1f8ec75f0cb6254ce49a3d46442ec7e6bc`에 있으며
`src/services/turn_orchestrator/queued_head/v2/effort_continue.rs:17`은 verified caller만 intent를 만들게 한다.
두 커밋은 기준 HEAD의 ancestor임을 확인했다. 준비된 store·owner original/ticket·barrier 기구를 재구현하지 않는다.
O-a는 child 활성화 전체가 아니고 S3는 command launch/취소 hook 자체가 아니다.
따라서 이 선행 착지만으로 herdr `/restart`가 충족됐다고 표시하지 않는다.

아래 좌표는 모두 위 기준 SHA의 코드다. `D`는 `src/services/discord/`, `S`는 `src/services/`,
`DB`는 `src/db/`를 뜻한다. 경로를 생략한 두 번째 좌표도 같은 행의 파일이다.
설계 모델의 운영 값 범위는 이 표에서만 가져온다. 모델의 단계명은 이 호출 순서의 투영이며 새 생산 enum이 아니다.

| 경로/소비자 | file:line | 현재 동작·값·이번 범위 |
|---|---|---|
| 슬래시 등록·인증 | `D/commands/restart.rs:352`, `:312`, `:315`, `:318` | `cmd_restart` → auth → slash policy → 공통 `restart_channel` |
| 텍스트 명령 | `D/commands/text_commands.rs:129`, `D/commands/restart.rs:336` | 인증된 intake의 `!restart`, 원 명령 메시지에 reply(`:343`) |
| 공통 restart | `D/commands/restart.rs:259`, `:272`, `:293` | 진행 알림 → reset → seed → 결과. host별 orchestration을 나눌 위치 |
| 메모리 snapshot | `D/commands/restart.rs:17`, `:45` | `NoActiveSession / ActiveSessionWithoutSessionId / ActiveSessionResumable`; 표시용 snapshot이지 실행 권한이 아님 |
| reset 전 herdr 거절 | `D/commands/restart.rs:218` | `managed_reset_refusal(...true,true,None)`가 cancel보다 먼저 실행됨 |
| 공통 관리 가드 | `D/admin_host_guard.rs:93`, `:124`, `:143` | `configured_refusal(:114)`로 herdr 설정 채널 거절. **이 가드를 전역 완화하지 않음** |
| row·marker 가드 | `D/admin_host_guard.rs:15`, `:151`, `:164`, `:334` | legacy tmux 확인, unknown/conflict/hosted 세션의 이름 기반 조작 차단 |
| 현재 active 취소 | `D/commands/restart.rs:223`, `:225`, `:227` | 경고 → mailbox cancel → `stop_active_turn(PreserveSession)`. herdr branch에서 이 선취소를 쓰면 exact terminate의 captured token 증거를 잃을 수 있음 |
| tmux reset 인자 | `D/commands/restart.rs:239` | `reset_provider_state=!supports_resume(:245)`, history 보존(`:246`), recreate 요청(`:247`) |
| 기존 인사말 | `D/commands/restart.rs:13`, `:133`, `:151` | `안녕하세요. 세션 재시작 완료.`를 `start_headless_turn`으로 제출 |
| seed 재시도·결과 | `D/commands/restart.rs:55`, `:166` | `Started/Busy/Failed/RetryFailed`; pre-spawn Conflict만 1초 뒤 한 번 재시도. accepted 요청 재전송 없음 |
| 일반 headless 거절 | `D/router/message_handler/headless_turn.rs:147`, `:159`, `S/turn_host.rs:177` | 현재 herdr `executor_not_wired`; 기존 seed를 그대로 부르면 종료 뒤 시작 실패 |
| provider reset | `D/commands/control/provider_reset.rs:3`, `:16`, `:24`, `:38`, `:49`, `:58` | guard 뒤 memory/DB selector·history·process·tmux reset. 다른 명령까지 열리므로 전체 재사용 금지 |
| tmux recreate | `D/commands/tmux_recreate.rs:9`, `:38`, `:57` | inflight admission 뒤 kill, 성공 때만 temp cleanup |
| 세션 보존 relaunch | `D/commands/tmux_recreate.rs:80`, `:91`, `:102` | 관리 guard, account marker 보존, kill/cleanup/복원. herdr 후속 범위 |
| pending 설정 소비 | `D/commands/control.rs:225`, `:246`, `:334` | herdr로 바뀐 relaunch는 해제/다음 기동 안내; guarded provider reset 호출. 후속 범위 |
| `/stop` | `D/commands/stop.rs:84`, `:91`, `:75` | Herdr stop 결과에 답한 뒤 별도 cancel/cleanup 안 함 |
| Herdr stop | `D/turn_bridge/tmux_runtime/judged_stop.rs:345`, `:370` | interrupt intent + Escape. close 권한과 다름 |
| 비강제 Cancel | `S/queue/herdr_stop.rs:13`, `:24`, `:42` | holder stop, provider End 대기; 완료나 inflight 삭제로 위조하지 않음 |
| 강제 Cancel | `S/queue/http_force.rs:33`, `:86` | exact owner/dispatch/nonce terminate + **queue purge**, restart에 purge 재사용 금지 |
| session force-kill | `S/dispatched_sessions/herdr_kill.rs:21`, `:60`, `:96` | 같은 terminate 서비스, queue 보존. retry dispatch 등 HTTP 후처리 전체는 가져오지 않음 |
| 명시 Terminate API | `src/server/routes/herdr_terminate.rs:40`, `:88`, `:93`, `:110` | 인증/switch, session_key+nonce, 선택한 pane proof, Retired만 HTTP 성공 |
| exact 종료 서비스 | `D/herdr_terminate.rs:401`, `:437` | owner/local node/location/nonce(`:453`), captured turn(`:477`), transition(`:482`), same actor/token(`:486`), fence(`:519`), row 재검사(`:548`), exact close(`:590`), end read(`:594`), retire CAS(`:596`), settlement(`:603`) |
| 종료의 결과·postlude | `D/herdr_terminate.rs:38`, `:61`, `:412`, `:224` | 기존 `after_retire`에서만 보유 exclusion 사용. retire CAS stale은 Indeterminate이며 성공 아님 |
| exact close 권한 | `S/termination_audit/host_terminate.rs:116`, `:126`, `:132` | live switch + terminate warrant, 일반 cancel warrant를 close로 승격하지 않음 |
| pane·server 정체성 | `S/session_host/herdr_gate.rs:141`, `:455`, `:473`, `:496`, `:658` | endpoint/session/pane/nonce fence, 같은 witness 서버에만 close 전달. 숫자 pane 재조회/다른 연결 fallback 금지 |
| exclusion 보유 범위 | `D/herdr_terminate/retired_exclusion.rs:16`, `:99` | transition + old execution fence + actor/token; `disconnect_session_keeping_provider` 사용 가능 |
| 보존형 disconnect CAS | `DB/dispatched_sessions/hosted_execution/http_force.rs:34`, `:45`, `:47` | retired/owner/nonce/dispatch 조건 아래 provider selector 유지. **successor Pending 설치 전에만 적용** |
| `/clear`, `!clear` | `D/commands/control.rs:510`, `:523`, `:552`, `:562`, `:573`, `:599` | transition 3초 timeout에도 계속 수행; queue/history/selector 폐기. restart와 의미가 다름 |
| Herdr clear teardown | `D/commands/control/clear_teardown.rs:70`, `:137`, `:179`, `:192` | 동일 `terminate_explicit_herdr` 서비스가 재사용 대상 |
| clear 전용 부작용 | `D/commands/control/clear_teardown.rs:175`, `:183`, `:186` | retired selector 삭제 postlude. `end_herdr` 전체를 그대로 호출하면 resume 보존 위반 |
| clear의 기존 fallback·기록 | `D/commands/control/clear_teardown.rs:150`, `:167`, `:80`, `:94`, `:97` | unknown/pool 없음은 end_unconfirmed 후 managed fallback 가능. tmux 전용 end record 차이는 후속으로 남기고 확대하지 않음 |
| 소유 입력 종료 cleanup | `D/commands/control/clear_teardown/execution_cleanup.rs:9`, `:42`, `:63`, `:112` | nonce/receipt/cleanup owner 전용 계약. restart에서 terminal 입력을 되살리는 용도로 쓰지 않음 |
| hosted 상태·읽기 | `DB/dispatched_sessions/hosted_execution.rs:27`, `:171`, `:228` | `Pending/Bound/Retired`; `Legacy/Known/Unknown`, lookup `Missing/Found/Unknown/Conflict` |
| herdr 새 launch | `S/herdr_launch.rs:378`, `:395`, `:433`, `:466`, `:492`, `:515` | RuntimeHintFence → row → Retired hold 검사 → 새 nonce → Pending CAS/marker → create |
| create/launch 결과 | `S/herdr_launch.rs:111`, `:210`, `:226` | `Created/NotSent/Indeterminate/CreatedUnconfirmed`; `Launched{evidence Ok/Err}/Indeterminate`; 불명 ACK는 Pending 유지, 재생성 금지 |
| incarnation 준비 | `S/herdr_launch.rs:732`, `:758` | 새 UUID, provider/channel/logical/runtime/native session/launch mode를 묶음 |
| Claude resume 준비 | `S/claude/herdr_turn/handoff.rs:6`, `:17`, `:29`, `:118` | account·transcript·O cursor·최신 binding·inode 검증, core session_id만으로 resume 안 함 |
| Claude 시작 순서 | `S/claude/herdr_turn/handoff.rs:99`, `:103`, `:107`, `:108` | hook 사전 구독 → launch → 자기 SessionStart → attach → prompt |
| Claude migration 종료 | `S/claude/herdr_turn/handoff.rs:60`, `:63` | `earlier_tmux`를 종료하는 migration 분기. Herdr→Herdr restart에서는 실행하지 않을 부분 |
| Codex 현재 시작 | `S/codex/herdr_turn.rs:188`, `:200`, `:277`, `:434` | fresh native selector=None/resume=false, 자기 Startup source, first input 뒤 Source attach |
| Codex argv 가드 | `S/codex/tui_session_launch/launch_policy.rs:170`, `:178`, `:210`, `:256` | 준비 전과 최종 argv 모두 fresh-only 검증. resume 모드 지원을 따로 추가해야 함 |
| Codex 출력 경계 | `S/codex/herdr_turn/first_input.rs:195`, `:208`, `:211`, `:225` | 첫 입력 뒤 hook/source/attach, 새 rollout 가정 offset=0. resume에 그대로 쓰면 과거 출력 중복 위험 |
| transition 중 intake | `D/router/message_handler/intake_turn/runtime_transition.rs:37`, `:43`, `:78` | 새 입력 durable tail, 기존 owner front는 원본 유지 + release wake |
| busy owner 입력 | `D/router/message_handler/provider_dispatch/owner_input.rs:216`, `:239` | transition 실패는 `NotSent(session_transition_busy)` |
| 기존 owner synthetic 시작 | `D/router/message_handler/provider_dispatch/owner_input.rs:621`, `D/queue_io/input_owner/start.rs:13`, `:59` | retry original/ticket 보존·admission. idle context는 `owner_input.rs:909` → headless 거절로 돌아가므로 전용 연결 필요 |
| 기존 transition lease/wake | `D/session_transition.rs:38`, `:120`, `:145` | 실제 shared/channel mutex에만 borrow 허용, Drop 이후 installed-owner wake |
| owner 내구 상태 | `S/turn_orchestrator/queued_head/v2_types.rs:20`, `:38`, `:81`, `:93` | V2 optional handoff/effort intent, 원본/attempt/target epoch. r2는 새 필드를 더하지 않고 seed를 `items`와 admission bind context에 둔다(3.5) |
| 입력 없는 boot debt | `S/turn_orchestrator/queued_head/compat.rs:23` | `holds_input`는 items/head/retained/effort만 셈. r2의 seed는 items의 한 항목이라 추가 연결이 필요 없다 |
| boot census 필터 | `S/turn_orchestrator/pending_queue_persistence/queued_recovery.rs:88`, `:92` | `v2_channels`가 holds_input/unfinished Claude handoff만 선택. r2의 seed가 든 파일은 holds_input으로 이미 선택된다 |
| 부팅 큐/actor | `D/runtime_bootstrap/queued_recovery.rs:48`, `:65`, `:129`, `:155`, `:189`, `:210` | 큐 먼저 복원, actor dedup·persist 실패는 원본 Held, 그 뒤 inflight |
| 부팅 intake gate | `D/runtime_bootstrap.rs:34`, `:41` | 복원이 끝난 뒤 intake 오픈, installed-owner wake |
| inflight 재수화 | `D/recovery_engine/restore_inflight.rs:136`, `:198`, `:219` | receipt/Herdr settlement, runtime_kind absent legacy 허용, unknown/newer는 추정 복원 금지 |
| TUI pending 재수화 | `D/tui_prompt_relay/synthetic_start.rs:2044`, `:2073` | pending index/worker 복원, prompt 자체 재전송 아님 |
| auto-queue·진단 | `S/auto_queue/runtime.rs:485`, `D/commands/diagnostics/reports.rs:46` | herdr reset 건너뜀/화면 unsupported. 후속 조각에서 처리 |
| 루틴 reset·runtime clear | `S/routines/session_control.rs:105`, `D/health/recovery.rs:267` | 같은 managed guard를 소비. 큐를 clear하는 후처리도 있으므로 restart에서 통째 호출하지 않음 |
| 에이전트 상태·출력 API | `S/agents/turn.rs:309`, `src/server/routes/agents.rs:186`, `S/dispatched_sessions/output.rs:171` | row/session-key guard로 tmux capture/probe를 거절하거나 unsupported로 표시. 이후 진단 조각 |
| 에이전트 CRUD·auth | `src/server/routes/agents_crud.rs:1037`, `src/server/routes/provider_auth_profiles.rs:518` | legacy-tmux/auth-login target 가드. restart의 별도 경로가 이를 전역 개방하지 않음 |
| 명시 resume rebind | `S/session_resume.rs:404` | herdr 설정이면 transcript 복사·binding·teardown 전에 HostUnsupported. restart 준비의 검증 포트와 구별 |
| stale/relay 복구 | `D/router/intake_gate/stale_turn.rs:191`, `D/relay_recovery/apply.rs:100` | turn release 거절 시 token/state 보존. 새 incarnation 때문에 old token을 무조건 삭제하지 않음 |
| watcher·idle·inflight 복구 | `D/tmux_watcher/host_gate.rs:153`, `D/session_idle_cleanup.rs:75`, `D/inflight/host_recovery_guard.rs:103` | herdr 설정 채널의 이름 기반 teardown/clear 건너뜀. restart activation으로 이 보호를 약화하지 않음 |
| CLI·provider cleanup·활성 판정 | `src/cli/utils.rs:176`, `S/provider_cli/session_guard.rs:199`, `S/provider/cancel_token_cleanup/executor.rs:162`, `S/session_activity.rs:49` | marker가 다른 host면 이름 기반 kill/probe를 거절/건너뜀. old nonce settlement와 별도 보호 |
| TUI passthrough | `D/commands/tui_passthrough.rs:176` | channel guard로 tmux passthrough 거절. `/model`·`/compact` 후속과 guard를 공유하므로 전역 우회 금지 |
| I/O 관측의 대기 한계 | `S/session_host/herdr/transport.rs:30`, `:84`, `:159`, `S/claude/herdr_turn.rs:63`, `S/codex/herdr_turn.rs:56` | 기존 socket read/write timeout·read deadline, Claude SessionStart/Codex composer/source wait를 재사용. DB·알림/작업 scheduler의 진행은 별도 liveness 전제 |

### 입력 출처와 불변식의 수락 경계

| 출처 | file:line | 수락·재시작 중 처리 |
|---|---|---|
| Discord 사람의 일반 메시지 | `D/router/intake_queue_transaction.rs:35`, `:121`, `D/router/intake_gate.rs:366` | Human, 원본 message/source IDs·queued generation 유지. 등록 slash는 일반 메시지 큐에 중복 투입하지 않음 |
| Discord 허용 봇 | `D/router/intake_gate.rs:347`, `D/router/intake_queue_transaction.rs:43` | AllowedBot/announce만 기존 정책대로 접수. 재시작 중 입력 custody는 사람과 동일, 명령 권한은 기존 auth 그대로 |
| 기타 봇 | `D/router/intake_queue_transaction.rs:39`, `D/router/intake_gate.rs:347` | OtherBot의 기존 비접수 정책 유지. 수락하지 않은 입력을 loss=0 계수에 넣어 가짜 성공으로 만들지 않음 |
| TUI에서 시작된 실제 턴 | `D/tui_prompt_relay/synthetic_start.rs:2044`, `D/herdr_terminate.rs:477` | O/합성 start로 관측한 old inflight를 captured token 기준 결산. 이미 시작된 턴은 중단 경고 대상 |
| TUI의 아직 제출하지 않은 수동 draft/직접 키 | `S/session_host/herdr_gate.rs:555`, `D/commands/restart.rs:276` | AgentDesk가 수락한 queued original과 구별. 종료할 pane의 draft·진행 중 턴은 기존 tmux restart처럼 중단될 수 있음. 직접 키 입력 자체를 무손실 접수했다고 주장하지 않음 |
| HTTP·루틴·iMessage 시작 | `D/router/message_handler/headless_turn.rs:159`, `S/turn_host.rs:177` | 현재 herdr 거절 유지(#8125). 일반 ingress는 restart seed source를 가진 시작을 만들 수 없어야 함(3.1 8단계) |
| restart 인사말 | `D/commands/restart.rs:13`, `D/queue_io/input_owner/start.rs:13` | 인증된 명령의 stable operation ID에서 유도한 한 ticket/original. restart 명령만 owner 큐 head에 적재하고, 종료 대상이 Retired일 때만 successor의 첫 입력으로 제출 |

수락한 입력은 owner의 durable original/attempt로 보유한 입력이다. 재시작 요청의 수락 선형화는
**seed 원본이 owner 큐 head에 내구 commit된 시점**이다(r2). 그 뒤 대기 입력은 seed를 앞지르지 못하고,
pane 수준의 쓰기 차단은 뒤이은 old channel transition + old nonce termination fence가 맡는다.
수락 전 이미 시작된 send는 기존 receipt로 `NotSent / acknowledged / uncertain`을 결산한다.
`NotSent`만 새 target에서 시도할 수 있고, uncertain은 보존·표시하며 맹목 재전송하지 않는다.
명령 텍스트가 도착한 시각부터 모든 native 키를 막았다는 더 강한 보장은 현재 코드에 없다.

## 2. 사용자 수준 비교

| 기능/상황 | tmux 현재 | herdr 현재 | 같음·다름 / 맞추는 방법 |
|---|---|---|---|
| 인증된 `/restart`, `!restart` | reset 뒤 즉시 seed | cancel 전 거절 | 다름 → restart 전용 host 분기: seed 적재 → exact terminate → owner 턴이 launch(3.1) |
| 살아 있는 실행 중 재시작 | 턴 손실 가능 경고, 취소 | 턴/프로세스 유지 | 다름 → 경고 후 exact captured old turn만 retire/settle |
| 대화 이력 | 보존 | 동작 없음 | 다름 → clear/history purge를 호출하지 않음 |
| 지원 provider의 native session | selector 보존, resume 시도 | 동작 없음 | 다름 → Claude 검증 재사용, Codex resume/source 경계 선행 지원 |
| native session 없음 | fresh로 시작, 이유 안내 | 거절 | 다름 → 동일한 fresh 결정, 확인된 부재에서만 시작 |
| 활성 실행 없음 | seed가 workspace 해석 후 시작 | 거절 | 다름 → 종료 없이 seed 적재 + wake, row 상태별 처리는 3.2 |
| 대기 입력 | reset에서 history/queue clear 요청 안 함 | 기존 큐 그대로 | 부분 같음 → herdr restart도 original/receipt/순서 보존, release wake 필요 |
| 재시작 중 새 Discord 입력 | 기존 transition/queue 정책 | 기존 transition/queue 정책 | 기구는 같음 → owner 큐에서 seed 뒤에 선다. exclusion은 종료와 disconnect 동안만 |
| 옛 incarnation으로 입력 | tmux 종료/재생성 | restart 없음 | 다름 → old fence 유지, 새 nonce/target epoch를 재판정한 owner만 제출 |
| 인사말 exactly once | accepted seed 재시도 안 함 | headless start 거절 | 다름 → operation에서 유도한 ticket 하나 + seed permit + 기존 owner receipt, uncertain seed 재전송 금지 |
| 성공 표시 | `Started`는 start 반환, Ready 증거 아님 | 거절 설명 | 다름 → 명령 응답은 종료 확인 + 인사말 등록까지. Ready는 인사말 턴의 응답, 실패는 owner 알림(3.7 U2). admission만으로 Ready를 말하지 않음 |
| close/create 미확인 | 일부 기존 결과가 요청 결과만 표시 | 명령 거절 | 다름 → herdr unknown ACK/identity는 Pending/custody 보존 + 사용자 실패. 이름 fallback/임의 kill 없음 |
| 동시 `!clear` | clear가 명시적으로 큐 폐기 | clear가 명시적으로 큐 폐기 | 같음 유지 → clear 우선. clear가 seed를 폐기한 것을 restart가 보고 selector까지 지운 뒤 superseded 실패(3.4). launch 중 clear의 기존 차이는 3.7 U3 |
| provider 설정 적용 | 기존 launch 인자 사용 | fresh launch 설정 경로 사용 | 새 launch도 같은 검증을 사용. `/model` live 변경 수용은 후속이며 이 조각으로 완료 표시 안 함 |
| 일반 API/루틴 시작 | headless 시작 가능 | `executor_not_wired` | 범위 밖 다름: #8125. restart seed만 좁게 연결하며 전체 허용 금지 |
| dispatch 실행 중 재시작 | 턴을 끊고 진행 | 거절 | 다름으로 남김 → 종료 전 거절 + `/stop` 안내(3.3). 사용자 결정 U1 |

위 행의 herdr 미구현은 불가피한 차이로 인정하지 않는다. 오류·identity 미확인은 성공과 구별한 실패이며,
tmux의 기존 오판까지 복제해 parity라고 부르지 않는다.

## 3. 구현 방향과 경합 처리 (r2)

새 host trait 또는 Core를 만들지 않는다. 기존 `terminate_explicit_herdr`, `RetiredExclusion`,
owner original/ticket/receipt, 그리고 **owner의 기존 herdr 턴 경로**(Discord 메시지가 지금 쓰는 launch)를 연결한다.
`clear_teardown::end_herdr`가 쓰는 종료 서비스는 재사용하고 clear의 selector 삭제 postlude는 재사용하지 않는다.

3~5절의 짧은 파일 이름은 다음을 뜻한다: `owner_input.rs` = `D/router/message_handler/provider_dispatch/owner_input.rs`,
`headless_turn.rs` = `D/router/message_handler/headless_turn.rs`, `intake_turn.rs` = `D/router/message_handler/intake_turn.rs`,
`provider_dispatch.rs` = `D/router/message_handler/provider_dispatch.rs`, `restart.rs` = `D/commands/restart.rs`,
`control.rs` = `D/commands/control.rs`, `clear_teardown.rs`·`herdr_recovery.rs` = `D/commands/control/` 아래,
`herdr_terminate.rs` = `D/herdr_terminate.rs`, `retired_exclusion.rs` = `D/herdr_terminate/retired_exclusion.rs`,
`http_force.rs` = `DB/dispatched_sessions/hosted_execution/http_force.rs`, `handoff.rs` = `S/claude/herdr_turn/handoff.rs`,
`first_input.rs` = `S/codex/herdr_turn/first_input.rs`, `herdr_launch.rs` = `S/herdr_launch.rs`, `turn_host.rs` = `S/turn_host.rs`,
`effects.rs` = `S/dispatched_sessions/herdr_kill/effects.rs`, `v2.rs`·`v2/…`·`v2_types.rs`·`compat.rs` = `S/turn_orchestrator/queued_head/` 아래.

### 3.0 방향 판정: owner 큐 seed 방향 채택

r1(현 방향)은 restart 명령이 exclusion 안에서 lower `launch_herdr_session`을 직접 부르고 그 뒤 seed를 넣는다.
리뷰 d1의 새 방향은 restart가 종료와 seed 적재까지만 하고, successor launch는 owner의 턴 경로가 seed를 첫 입력으로 처리하며 한다.
**새 방향을 채택한다.** 다만 리뷰 제안의 순서(종료 → 적재)를 **적재 → 종료**로 바꾼다. 이유는 3.5의 crash 표에 있다.

| 항목 | 현 방향(r1): restart가 직접 launch | 새 방향(채택): seed를 큐 head에, launch는 owner 턴 |
|---|---|---|
| 흐름 | 종료(`D/herdr_terminate.rs:401`) → disconnect(`retired_exclusion.rs:100`) → exclusion 안에서 provider 준비·`launch_herdr_session`(`S/herdr_launch.rs:378`)·attach → seed permit → 해제·wake | seed 원본을 큐 head에 내구 적재 → 종료 → disconnect → 해제·wake. launch는 owner 턴이 한다: `owner_input.rs:835` start_idle → `provider_dispatch.rs:337` drive → `S/claude/herdr_turn.rs:275` 또는 `S/codex/herdr_turn.rs:200` → `S/herdr_launch.rs:378` |
| 이미 있는 선례 | 없음. launch를 턴 밖에서 부르는 생산 호출자가 없다 | 세션 강제 종료가 같은 모양이다: 종료 + selector 보존 disconnect(`S/dispatched_sessions/herdr_kill/effects.rs:131`), 큐 보존, 해제 뒤 wake(`effects.rs:141`). 그 뒤 첫 입력이 Retired row 위에 successor를 띄운다(`S/herdr_launch.rs:433`~`:449`) |
| 새로 만들 것 | lower launch용 lease accessor, Claude `launched_source`(`handoff.rs:40`) 분해, Codex composer→seed→attach 순서 재현, V2 restart intent + 부팅 census 연결, seed permit | seed 적재·철회·gate·permit 작은 child 1개, typed 연결 3곳. launch·attach·unplaced 해결·Pending 재사용은 기존 경로 그대로(`S/codex/herdr_turn.rs:228`~`:264`, `S/claude/herdr_turn.rs:395`~`:427`) |
| B 비용 추정 | 리뷰 P2 반영 뒤 prod 14~18파일, 순증 +1050~1400. +1200을 넘겨 2분할 필요. `herdr_terminate.rs`(949줄)에 held-guard 진입점을 더하면 giant 경계(1,000줄)를 넘는다 | prod 13~15파일, 순증 +480~620. 한 리뷰 단위 |
| clear 경합 | exclusion이 launch·seed까지 길다. clear는 3초 뒤 지나가고(`control.rs:524`) 자기 종료는 `transition busy`로 거절된다(`herdr_terminate.rs:483`). successor가 clear 뒤에도 살아남는다(P2-1). 고치려면 restart가 자기 successor를 exact 종료하는 경로가 새로 필요하다 | exclusion은 종료와 disconnect 동안만이다. 해제 뒤에는 기존 clear가 Retired row를 직접 처리한다(`control/herdr_recovery.rs:58`, `:83`, `:115`). exclusion 중 clear가 지나가면 **seed가 폐기된 것**이 신호가 되어 postlude가 selector를 clear 뜻대로 지운다(3.4). successor가 아직 없으므로 종료할 대상이 없다 |
| crash 시점별 결과 | intent의 stage와 row를 대조하는 부팅 복구가 새로 필요(종료 뒤·Pending 뒤·seed 뒤 각각) | seed가 큐에 있으므로 기존 부팅이 그대로 몬다(`compat.rs:33`, `pending_queue_persistence/queued_recovery.rs:92`). 적재 뒤·종료 전 crash는 permit이 보이게 거절한다(3.5) |
| 부팅 census | 입력 0·intent만 있는 파일을 찾도록 `holds_input`·census 연결 필요 | 연결 0. seed는 `items`의 한 항목이다 |
| 롤백 | 새 V2 필드를 옛 바이너리가 조용히 지운다(P2-6). 버전 표지가 필요하다 | 새 필드·새 serde variant 0. 옛 바이너리는 seed를 headless start로 읽어 `executor_not_wired`로 보이게 거절한다(`headless_turn.rs:159`~`:165`, `provider_dispatch/start_failure.rs:21`) |
| Ready 표시 | 명령이 launch를 기다리지만 Ready는 여전히 seed receipt로 판정(r1 10단계) | 명령 응답은 "옛 실행 종료 + 인사말 등록"까지. Ready는 인사말 턴의 응답으로 보이고, launch 실패는 owner의 기존 시작 실패 알림으로 보인다 |
| 해제~launch 사이 경쟁 | 해당 없음(exclusion 안에서 launch) | owner는 채널당 단일 actor이고 seed가 큐 front다(FIFO). HTTP·루틴 시작은 herdr에서 거절(`turn_host.rs:191`), 열려도 owner 큐 뒤로 들어간다(`owner_input.rs:621`). 동시 launch는 `install_pending_pg` CAS가 하나만 남긴다(`herdr_launch.rs:482`~`:487`). 종료 API는 nonce가 정확히 맞아야 한다(`herdr_terminate.rs:453`) |
| 남는 위험 | P2-1 수리 경로(자기 successor 종료)와 lock 순서 | seed의 시작 경로가 headless다. herdr에서 headless 시작은 지금 전부 거절이라(`headless_turn.rs:159`) 그 뒤 구간이 생산에서 돈 적이 없다. B 첫 커밋에서 실제 진입 시험으로 확인한다(3.1 8단계) |

채택 근거는 세 가지다. (1) launch를 새로 조립하지 않고 매일 도는 경로를 쓴다. (2) P2-1의 원인(긴 exclusion 안의 launch)이 구조에서 사라지고,
P2-6의 원인(새 내구 필드)도 사라진다. (3) 비용이 절반 아래이고 분할이 필요 없다. 같은 모델에서 r1의 순서를 변형(`R1Launch`)으로 넣으면
`NoResumeAfterClear`가 깨진다(6절).

### 3.1 채택 흐름

1. 기존 명령 auth/policy를 통과한 뒤 `restart_channel`(`D/commands/restart.rs:259`)에서 herdr 설정 채널이면 전용 분기로 간다.
   legacy tmux 분기와 `managed_reset_refusal`(`:218`)은 그대로 둔다. 전역 가드를 완화하지 않는다.
2. canonical row를 읽어 3.2의 표로 분기한다. 거절 행은 여기서 보이는 실패로 끝나고 아무것도 바꾸지 않는다.
3. 종료가 필요한 행은 active dispatch를 본다(3.3). 있으면 여기서 거절한다.
4. **seed 적재(수락 시점).** operation은 interaction ID 또는 `!restart` message ID에 token/provider/channel namespace를 결합한 값이다.
   여기서 ticket과 합성 원본(기존 인사말 문구, `restart.rs:13`)을 유도하고, admission context에 `source = agentdesk_restart_seed`,
   operation, 종료 대상 nonce(없으면 null)를 담아 owner 큐 **head**에 내구 적재한다. 기존 continuation의 prepend(`v2/source_admission.rs:971`)와
   head 여유 한 칸(`:748`)을 같은 방식으로 쓴다. 같은 operation의 재호출은 같은 ticket이라 중복 적재되지 않는다.
   큐에 이미 restart seed가 있거나 `effort_restart` intent가 미결이면(`v2_types.rs:41`, `v2/effort_continue.rs:126`) Busy로 거절한다.
   적재 실패(V2 미채택·큐 가득·commit 미확인)는 종료 전의 보이는 거절이다.
5. 종료가 필요한 행에서는 적재와 함께 채널의 **in-flight gate**(프로세스 메모리, RAII)를 건다. gate가 걸린 동안 owner는 seed를 시작하지 않고 기다린다
   (`owner_input.rs:859`~`:872`의 기존 대기 분기에 한 갈래). seed가 front에 있는 동안 사람 입력을 앞으로 옮기지 않는다(`v2.rs:125`의 기존 가드와 같은 자리).
   따라서 수락 뒤에는 어떤 대기 입력도 seed를 앞질러 옛 pane으로 가지 않는다.
   옛 턴이 도는 중이면 기존 synthetic 대기 규칙(`owner_input.rs:214`, `:245`)도 seed를 그 턴이 끝날 때까지 잡아 둔다.
   gate는 Drop에서 owner를 깨운다. 명령 future가 취소되거나 panic해도 seed가 깨움 없이 남지 않고, 그때의 결과는 permit이 row로 정한다.
6. 기존 exact terminate를 부른다. tmux 분기의 `mailbox_cancel_active_turn` 선취소는 쓰지 않는다. 진행 중 턴이 있으면 먼저 손실 경고를 보인다.
   서비스가 transition(`herdr_terminate.rs:482`)과 fence(`:519`)를 잡고 row·actor·token을 다시 본 뒤 exact close → 종료 관측 → retire CAS → captured 턴 결산을 한다.
   close 불명·provider 잔존·stale CAS·nonce 불일치는 실패다(9단계).
   close부터 postlude까지는 운영자 Terminate 경로처럼 요청 future와 분리해 돌린다(`herdr_terminate.rs:679`). 끊긴 요청이 close와 결산 사이에서 멈추면 안 된다.
7. `after_retire` postlude(exclusion 보유 중): seed가 아직 큐에 있는지 본다. 없으면 clear가 앞선 것이다(3.4). 있으면
   `disconnect_session_keeping_provider`(`retired_exclusion.rs:100`)를 실행한다. clear의 queue purge·history erase·catch-up discard는 부르지 않는다.
   postlude가 끝나면 exclusion과 gate를 놓고 owner를 깨운다(`owner_input.rs:602`). HostTerminated 결산은 큐를 스스로 깨우지 않는다(`turn_finalizer/finalize_context.rs:45`).
8. owner가 front인 seed를 시작한다. context가 보존된 시작이라 headless 진입(`owner_input.rs:912`)을 탄다. 여기에 **seed permit**을 둔다:
   origin이 owner의 내구 bind context에서 읽은 `RestartSeed`일 때만, `headless_turn.rs:159`의 판정을 intake와 같은 판정(`turn_host.rs:198`)으로 바꾸고,
   context의 종료 대상 nonce가 지금 row의 살아 있는(Retired 아닌) execution이면 거절한다. 그 밖의 headless 시작은 지금처럼 `executor_not_wired`다(#8125).
   일반 ingress(`queue_io/input_owner/start.rs:13`)는 이 source를 가진 시작을 만들지 못하게 거절한다. seed는 restart 명령만 적재한다.
   B 첫 커밋에서 seed가 이 경로로 `launch_herdr_session`에 닿고 `headless_turn.rs:159`~`:926` 사이에서 tmux 이름 기반 reset·reconcile·clear가
   herdr 채널에 실행되지 않음을 실제 진입 시험으로 확인한다. 걸리는 단계가 있으면 그 가드는 두고 seed를 intake 형 시작(`owner_input.rs:922`)으로 돌린다.
9. 실패 응답. 종료가 거절·불명으로 끝나면 **먼저 보이는 실패를 답하고 그 뒤 seed를 철회**한다(기존 대기 입력 취소와 같은 terminal receipt).
   답과 철회 사이에 crash가 나면 남은 seed를 permit이 거절해 알림이 한 번 더 나갈 뿐이다. 반대 순서는 crash 때 결과 없이 끝난다.
10. 성공 응답은 "옛 실행 종료 확인, 인사말을 대기열 맨 앞에 등록"까지다. resume 여부는 launch 때의 검증이 정하므로 "가능하면 이어서 시작"으로 안내한다.
    `Admission::Accepted`나 launch 반환을 Ready로 말하지 않는다. Ready는 seed ticket의 Settled receipt, successor nonce의 Bound, source binding으로 판정한다.
    launch 실패·create 응답 불명은 owner의 기존 시작 실패 처리(retained + 알림)로 보이며 seed를 맹목 재전송하지 않는다.

Claude는 `launched_source`(`handoff.rs:40`)가 account·transcript·cursor·inode를 검증해 resume하거나 fresh로 간다. 이 경로를 분해하지 않는다.
같은 이름 tmux가 살아 있을 때만 도는 `earlier_tmux` 종료(`handoff.rs:60`~`:69`)는 일반 메시지에서와 같은 기존 동작이고 restart가 새로 얹는 권한이 아니다.

### 3.2 시작 시 row 상태별 처리 (P2-4)

| canonical row | 종료 | 처리 |
|---|---|---|
| Unknown / Conflict / 다른 owner·node | 없음 | 보이는 거절. 큐·row 변화 0 |
| Bound, location 이 node | exact terminate | 3.1의 4~10단계 전부 |
| Pending, location 있음 | exact terminate | Bound와 같다. launch evidence가 없어 서비스가 거절하면(`herdr_terminate.rs:512`) 9단계 |
| Pending, unplaced(location·expected 없음) | 없음 | 보이는 거절 + 안내. 이 상태는 다음 입력의 기존 resolver가 푼다(`S/claude/herdr_turn.rs:403`, `S/codex/herdr_turn.rs:240`~`:253`). restart가 정체 불명 pane을 닫는 권한은 더하지 않는다 |
| Retired | 없음 | seed 적재 + wake. 옛 실행은 이미 끝났으므로 terminate를 다시 부르지 않는다(retired probe 실패 경우가 없어진다). gate 없음 |
| Missing / Legacy | 없음 | seed 적재 + wake. owner 턴이 fresh 또는 검증된 resume으로 띄운다(`S/claude/herdr_turn.rs:275`). gate 없음 |

r1의 "terminate가 준 exclusion 위에서만 launch" 전제가 없어져, 종료할 것이 없는 행에서 exclusion을 구할 필요가 없다.

### 3.3 dispatch 실행 중 (P2-3)

`terminate_explicit_herdr`는 dispatch를 묶지 않아(`herdr_terminate.rs:412`, `:608`) row에 `active_dispatch_id`가 있으면 disconnect가 0행을 쓴다
(`DB/dispatched_sessions/hosted_execution/http_force.rs:55`). 그대로 두면 successor가 옛 dispatch를 물려받는다.

- 종료 전에 `active_dispatch_pg`(`http_force.rs:21`)를 읽어 dispatch가 있으면 "dispatch 실행 중, `/stop` 뒤 다시"로 거절한다. close 0, seed 0.
- 사전 검사 뒤 fence 사이에 dispatch가 붙는 경합에서는 postlude의 disconnect가 `Ok(false)`다. 이때는 seed를 철회하고 보이는 실패로 끝낸다.
  옛 실행은 이미 Retired이며, 다음 입력은 운영자 Terminate 뒤와 같은 기존 경로로 시작한다. `Ok(false)`를 성공으로 세지 않는다.
- HTTP 강제 종료처럼 `Expected{owner, dispatch}`로 묶고 dispatch를 failed 처리하는 대안은 범위가 커서 택하지 않는다(3.7 U1).

### 3.4 restart 중 `!clear` (P2-1)

clear는 transition을 3초 기다린 뒤 지나가고(`control.rs:524`), 자기 종료는 다시 3초 뒤 `transition busy`로 끝날 수 있다(`herdr_terminate.rs:483`, `clear_teardown.rs:202`).
채택안에서 "clear 우선"은 다음으로 성립한다.

| clear가 큐를 폐기한 시점 | 일어나는 일 | 결과 |
|---|---|---|
| seed 적재 전 | 평범한 clear. 그 뒤 restart는 clear 뒤 상태에서 시작 | selector 삭제, fresh successor가 인사말 |
| 적재 뒤, restart exclusion 전·중 | clear가 seed를 폐기한다(`control.rs:552` 또는 `herdr_recovery.rs:76`). restart postlude가 "seed 없음"을 보고 keep 대신 `clear_retired_selector_pg`(`http_force.rs:75`)를 실행하고 superseded 실패를 답한다. 응답 직전에 한 번 더 본다 | seed 0, selector 삭제, successor 0 |
| exclusion 해제 뒤, owner가 seed를 잡기 전 | 기존 clear가 Retired row를 처리(`herdr_recovery.rs:58`, `:83`, `:115`) | seed 폐기, selector 삭제 |
| clear가 Bound를 읽고 지나간 뒤 restart가 retire, 그 뒤 clear가 폐기 | `end_herdr`가 Retired row에서 아무것도 하지 않는다(`clear_teardown.rs:161`). **B에서 이 분기에 같은 CAS 삭제를 더한다** | seed 폐기, selector 삭제 |
| owner가 seed 턴을 시작한 뒤(launch 중) | 일반 메시지의 launch 중 clear와 같다. Retired + 활성 턴이면 보류 보고(`herdr_recovery.rs:158`), unplaced Pending이면 종료 미확인(`clear_teardown.rs:202`) | **기존 결함. 넓히지 않음**(3.7 U3) |
| restart의 종료가 실패했고 clear의 종료도 timeout | 둘 다 경고로 끝나고 옛 세션이 남는다 | clear의 기존 timeout 계약 그대로. 사용자가 다시 시도 |

seed가 종료보다 먼저 큐에 있으므로, 수락 뒤 폐기를 커밋한 clear는 반드시 seed를 지운다. 폐기와 restart의 확인은 owner actor가 직렬화한다.
별도 clear 세대 계수기를 만들지 않는다. restart는 clear가 버린 입력을 되살리지 않고 seed를 다시 적재하지 않는다.
clear가 guard 없이 지나가는 동안의 폐기~selector 삭제 사이(수 ms)에 새 입력이 launch하는 일반 경합은 clear의 기존 overtake 계약에 속하며 이 설계가 바꾸지 않는다.

### 3.5 crash·부팅·롤백 (P2-6)

seed 원본과 그 admission context가 이 요청의 **유일한 내구 기록**이다. 새 V2 필드, 새 serde enum variant, 버전 상향은 없다.
부팅은 기존 순서(큐 → inflight → intake 오픈, `runtime_bootstrap/queued_recovery.rs:48`, `runtime_bootstrap.rs:34`) 그대로이고 gate는 메모리라 사라진다.

| crash 시점 | 부팅 때 상태 | 결과 |
|---|---|---|
| 적재 전 | 변화 없음 | 요청이 수락되지 않았다. 옛 세션 그대로 |
| 적재 뒤, close 전 | seed가 head, row는 옛 nonce Bound | permit 거절 → seed는 알림과 함께 종결("재시작이 끝나지 않았어요"). 대기 입력은 살아 있는 옛 세션으로 간다. 세션 1개 |
| close 뒤, retire CAS 전 | row는 Bound인데 pane은 끝남 | 기존 복구가 row를 Retired로 만들면 seed가 launch(Ready). 그 전에 seed가 먼저 시작되면 permit 거절(보이는 실패)이고 입력은 복구 뒤 launch |
| retire 뒤(disconnect 전후), wake 전 | seed가 head, row Retired | owner가 seed로 launch → Ready. disconnect가 빠졌어도 launch는 hosted record만 본다(`herdr_launch.rs:433`) |
| launch 뒤(Pending 설치·create 전후) | seed attempt + Pending | 기존 owner attempt 복구와 Pending resolver. create를 보낸 적 없으면 다시 시작, 응답 불명이면 Pending 유지 + 보이는 실패, create 재전송 0 |
| 실패 응답 뒤, seed 철회 전 | seed가 남음 | permit 거절 알림이 한 번 더. 무응답 종료 없음 |

적재를 종료 뒤에 두면(리뷰 제안의 순서) "retire 뒤·적재 전" crash가 흔적 없이 끝난다(옛 세션 종료, 인사말 0, 결과 0).
이를 막으려면 별도 내구 intent나 알림 debt가 다시 필요하고 그것이 P2-6의 롤백 문제를 되살린다. 그래서 적재를 먼저 한다.

- **롤백**: 옛 바이너리는 seed를 context가 있는 headless 시작으로 읽고(`queue_io/input_owner/origin.rs:42`), herdr 채널이라 `executor_not_wired`로 거절해
  기존 typed 시작 실패(retained + 알림)로 종결한다. 큐 항목이라 다시 쓸 때 지워지지 않는다. 사람 입력은 영향이 없다.
  시작 실패 사유를 새 `StartFailureKind` variant로 만들지 않는다(`v2_types.rs:200`). 옛 바이너리가 스냅샷을 못 읽게 된다. 기존 kind + 사유 문자열을 쓴다.
- **기존 intent와의 공존**: `effort_restart`가 미결이면 restart를 Busy로 거절하고, seed가 큐에 있으면 effort intent 준비를 Busy로 거절한다(같은 owner 판정).
  effort 장벽(`v2.rs:236`, `:372`)이 seed의 시작을 막아 서로 기다리는 일을 없앤다. `unfinished_handoff`(Claude handoff journal)는 기존대로 owner가 먼저 끝낸다.
- 옛 V1/V2·`runtime_kind` 없는 inflight 복원은 기존 정책 그대로다. unknown/newer 형식을 내려 읽거나 큐 파일을 지우지 않는다.

진행성은 scheduler가 owner drive와 알림 작업을 계속 돌리고 I/O가 답하거나 deadline으로 분류되는 조건 아래 보장한다.
전송 불능인 Discord에 메시지가 도착했다는 보장은 만들지 않는다.

### 3.6 A 조각과의 접점 (P2-2·P2-5)

A(Codex resume 경계)는 다른 레인이 구현 중이다. 채택안이 A에 요구하는 것은 다음뿐이다.

- **resume 요청의 출처**: `HerdrLaunch.resume`은 seed 턴에서만 true가 된다. 일반 Codex 재기동은 지금처럼 fresh다(`S/codex/herdr_turn.rs:188`~`:197`).
  B는 owner가 내구 context에서 읽은 `RestartSeed` origin을 Codex 턴의 resume 요청으로 넘기는 연결 한 줄을 갖고, A는 그 요청을 받는 포트를 준다.
  selector가 있다는 것만으로 resume을 켜지 않는다.
- **시작점**: A가 Pending CAS와 같은 row 기록에 고정하는 resume 시작점을 그대로 쓴다. 채택안에서는 seed가 **일반 첫 입력 경로**(`first_input.rs:195`~`:225`)로
  들어가므로 그 경로가 row의 시작점을 읽으면 충분하다. restart가 EOF를 재서 넘기지 않는다. launch 시점에는 옛 provider가 이미 끝나 있다(Retired).
- **부팅**: seed와 launch 사이 crash 뒤에도 시작점은 row에 있고 seed는 큐에 있다. V2 intent에 시작점을 둘 일이 없다.
- Claude는 접점이 없다. 기존 `resumed_session`(`handoff.rs:6`)과 `o_reads`(`:29`)가 그대로 판정한다.

### 3.7 사용자 결정이 필요한 제품 동작 차이

| # | 사용자에게 보이는 차이 | 기본값(이 설계) | 대안 |
|---|---|---|---|
| U1 | dispatch 실행 중 `/restart`: tmux는 진행, herdr는 거절하고 `/stop`을 안내 | 거절 | dispatch를 failed 처리하며 진행(HTTP 강제 종료와 같은 묶음, 범위 큼) |
| U2 | 결과 표시: tmux는 한 응답에 "인사말 turn 시작됨/실패". herdr는 응답이 "종료 확인 + 인사말 등록"이고, 새 세션의 준비는 인사말 턴의 응답으로, launch 실패는 뒤따르는 알림으로 보인다 | 비동기 표시 | 명령이 seed receipt를 일정 시간 기다렸다가 한 응답으로 합침(대기 상한·timeout 문구가 새로 필요) |
| U3 | `/restart` 직후 `!clear`: 인사말 턴이 launch 중이면 clear가 보류 경고로 끝나고 새 세션이 남을 수 있다. tmux는 clear가 이긴다. herdr의 일반 메시지에서도 지금 같은 동작이다 | 기존 동작 유지, 후속 후보 | clear 쪽을 고쳐 launch 중 턴도 끝내게 함(모든 메시지에 영향, 별도 이슈) |
| U4 | unplaced Pending(생성 응답 유실)에서 `/restart` 거절 + "메시지를 보내면 기존 경로가 해결" 안내 | 거절 | 후속에서 exact 정리 수단을 정한 뒤 허용 |

U2는 r1에서도 Ready가 receipt 기준이었으므로 문구 차이에 가깝다. U1·U3은 tmux와 결과가 달라지는 항목이다.

### 3.8 수용 기준 문장 (B가 표로 대조할 것)

1. herdr 채널의 인증된 `/restart`·`!restart`는 옛 exact 실행을 끝내고 인사말을 첫 입력으로 한 새 incarnation 하나를 띄운다.
2. 수락(seed 적재) 뒤 어떤 대기 입력도 옛 pane으로 가지 않고, 접수 원본은 한 번만 전달되거나 명시 receipt로 종결된다.
3. seed는 restart 명령만 만들 수 있고, 종료 대상이 살아 있는 동안에는 전달되지 않는다. 일반 headless 시작은 계속 거절된다.
4. 3.2의 각 행은 표의 결과를 내고, 거절 행은 close·seed·row 변화가 0이다.
5. dispatch가 있으면 종료 전에 거절하고, disconnect `Ok(false)`는 보이는 실패다.
6. 수락 뒤 큐를 폐기한 clear가 있으면 seed 0·selector 삭제·successor 0으로 끝난다(3.4의 넷째 행 포함).
7. 3.5의 각 crash 시점은 표의 결과를 내고, 수락된 요청은 Ready 또는 보이는 실패로 끝난다.
8. 옛 바이너리로 돌아가도 seed는 보이는 거절로 종결되고 스냅샷은 읽힌다.
9. create 응답 불명·seed 불명은 재전송 0, Pending/원본 보존, 보이는 실패다.
10. tmux 채널의 `/restart`와 herdr 채널의 인접 관리 가드는 변하지 않는다.

## 4. 구현 분할과 크기 (r2)

A는 그대로다(다른 레인). B는 채택안 기준 **prod 13~15파일, 순증 +480~620**으로 추정하며 리뷰 단위 하나다(상한 30파일/+1500, 분할 기준 +1200).
첫 production WIP 커밋 직후 `pr_cap_prod.py`로 다시 잰다. +900을 넘으면 아래 경계로 나눈다. 함수·시험을 옮겨 수치를 맞추지 않는다.

| 조각 | 예정 생산 파일/연결 | 순증 추정 | 수용 경계 |
|---|---|---|---|
| A: Codex resume launch/source 경계 | `S/codex/herdr_turn.rs`, 새 `herdr_turn/restart_resume.rs`, `first_input.rs`, `tui_session_launch/launch_policy.rs`, 필요 시 기존 launch options/source child (4~6파일) | +350~500 | 기존 fresh 검사 유지, resume native/argv/source EOF/nonce 검증. 기존 생산 prepare/first-input 경로의 소비되는 기능으로 연결하고 미사용 dormant API/allow를 만들지 않음 |
| B: restart 명령·seed·permit·clear 보강 | 아래 표 | +480~620 | 3.8의 1~10 |

| B 파일 | 넣는 것 | 순증 |
|---|---|---|
| `D/commands/restart.rs` | herdr 분기 한 갈래, 응답 문구 연결 | +20~30 |
| 새 `D/commands/restart/herdr.rs` | row 분기표, dispatch 사전 검사, 적재·종료·postlude·응답·철회 순서 | +180~230 |
| 새 `D/queue_io/input_owner/restart_seed.rs` (+`mod.rs` 한 줄) | operation→ticket·원본·context, head 적재, 존재 확인, 철회, in-flight gate, permit | +130~170 |
| `D/queue_io/input_owner/origin.rs`, `start.rs` | `RestartSeed` origin, 일반 ingress의 source 거절 | +15~20 |
| 새 `S/turn_orchestrator/queued_head/v2/restart_seed.rs`, `v2.rs` | seed 판별, head 적재 허용, 사람 선행 이동 가드 | +35~50 |
| `S/turn_orchestrator/queued_head/v2/source_admission.rs` **(giant 1,028줄)** | head 적재 판별 연결 2~4줄만 | +4 |
| `D/router/message_handler/provider_dispatch/owner_input.rs` **(giant 1,567줄)** | gate 대기 분기, 기존 대기 예외에 origin 한 갈래 | +8~12 |
| `D/router/message_handler/headless_turn.rs` **(giant 1,545줄)** | `:159`의 판정 선택 분기만. 검증은 child에 둔다 | +6~10 |
| `S/turn_host.rs` | seed용 진입 판정(intake 판정 재사용) | +10~15 |
| `D/commands/control/clear_teardown.rs` | Retired row에서의 selector CAS 삭제(3.4 넷째 행) | +8~12 |
| A의 포트를 부르는 연결(`provider_dispatch.rs`의 Codex 턴 조립 등 1~2파일) | seed origin → Codex resume 요청 | +10~20 |

분할이 필요해지면 경계는 **B-clear**(`clear_teardown.rs` 보강과 그 시험, 단독으로 안전)와 **B-main**(나머지)이다.
giant 세 파일에는 선언·typed 연결·거절 분기만 넣는다. `herdr_terminate.rs`(949줄)와 `herdr_gate.rs`는 건드리지 않는다.
시험 추가 시 공식 inventory/선택 경로 동기화로 required PR job 선택을 확인한다. baseline·allowlist·감사 임계값은 이 설계의 완화 대상이 아니다.

## 5. 검증 계획과 운영 관측

설계 모델의 GREEN은 생산 실행의 GREEN이 아니다. 이번 레그에서는 아래 Rust/PG/E2E를 실행하지 않았다.

| 검증 | 실제 진입점·RED → GREEN | 원인 제거 뮤턴트 / 필수 단언 |
|---|---|---|
| 명령 end-to-end fixture | 기존 `commands/restart.rs:374`/`:423` host-guard PG seam과 `restart_channel`, slash/text auth 뒤 공통 실행. seed가 owner drive를 거쳐 `launch_herdr_session`에 닿는 데까지 | herdr branch 제거 → 정상 herdr 재시작 거절 RED. auth 거절은 close/create/seed 0이며 실제 명령 결과 포함 |
| seed 시작 경로 | owner front → headless 진입 → `herdr::execute`. B 첫 커밋의 확인 시험(3.1 8단계) | permit 분기 제거 → `executor_not_wired` RED. 일반 headless 시작과 source를 흉내 낸 ingress는 계속 거절(대조군). `:159`~`:926` 사이 tmux 이름 기반 효과 0 단언 |
| row 상태표 | 3.2의 여섯 행마다 PG fixture로 실제 명령 실행 | Retired·Missing 행에서 terminate를 부르는 변형 → 거절 RED. 거절 행은 close·seed·row 변화 0 |
| dispatch | `active_dispatch_id`가 있는 row에서 실제 명령 | 사전 검사 제거 + `Ok(false)` 무시 → successor row에 옛 dispatch가 남아 RED. 정상은 close 0·거절 문구 |
| 종료의 정확성 | `terminate_explicit_herdr` + witnessed fake transport/PG row + actual command branch | exact fence 또는 identity 재검사 제거 → old/server/pane 교체에서 foreign close/old input RED. close ACK만 성공 처리 mutant → provider 잔존 RED |
| 입력 custody·동시성 | restart의 적재/fence/close/retire/postlude/release 각각 barrier에서 실제 Discord intake·owner drive 실행 | purge 연결, gate 제거, permit 제거, release wake 제거 → 원본 ID/본문/upload 손상·중복, 옛 pane 인사말, 이른 거절 또는 영구 대기 RED. 두 입력의 실제 count=각1 |
| uncertain 경계 | close/create/seed effect 직후 reply 유실·phase commit 전 crash fixture | blind create/seed retry → 물리 create 또는 seed 효과 2 RED. uncertain은 Pending/retained+보이는 실패이며 성공 count에서 제외 |
| Codex resume | A의 실제 `prepare_codex_herdr_launch`/first-input/attach 경로, 기존 transcript가 있는 fake-provider round trip | fresh validator 삭제만 한 변형, offset=0, foreign SessionStart 허용 → native mismatch/과거 출력 재배달 RED. fresh control도 계속 GREEN |
| Claude resume | command→준비→SessionStart/attach, account/transcript/inode/cursor fixture | latest binding 검사 제거 → stale selector RED. migration 종료 호출 mutant → 관계없는 tmux close 1 RED(정상 0) |
| clear 경합 | restart를 fence 뒤 barrier에 세우고 실제 `!clear`를 보내 3초를 넘김. 3.4의 행마다 교차 | postlude의 seed 확인 제거, `end_herdr` Retired 보강 제거 → selector가 남아 다음 입력이 clear 전 대화를 resume RED. 끝난 뒤 seed 0, selector 컬럼 NULL, 원본 drop receipt, successor 0 |
| 중복·동시 요청 | 같은 interaction/message operation을 prepare/close/seed/결과 단계에서 다시 호출, 다른 요청과도 교차 | durable operation/ticket 대신 새 ID 생성 → close/create/seed 중복 RED. 다른 진행 중 요청의 Busy는 사용자에게 표시하며 기존 요청 custody를 덮어쓰지 않음 |
| boot·옛 형식 | 3.5의 crash 시점마다 스냅샷·row fixture로 실제 부팅 복원, Pending lost ACK, old V1/V2/runtime_kind absent | permit 제거 → 살아 있는 옛 세션에 인사말 RED. 응답 전 철회 순서 변형 → 결과 없는 종료 RED. unknown/newer는 추정 복원 0 |
| 롤백 | seed가 든 스냅샷을 기준 SHA의 owner 코드 경로(typed 연결 없는 판정)로 읽는 golden | seed를 새 V2 필드·새 variant로 옮긴 변형 → 필드 소실 또는 parse 실패 RED. 정상은 `executor_not_wired` 종결 + 알림, 사람 입력 보존 |
| 결과 정확성 | close 실패·evidence gap·seed Pending·source bind timeout·Discord 전송 실패 | admission=Ready/0시험=PASS 변형을 실패로 검출. owner 시작 실패 알림과 실제 메시지 도착을 분리 |

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
| herdr server·정책 | `S/herdr_launch.rs:385` LegacyDedicated restore-off fresh witness, UserDefault grant `:456`; `S/session_host/herdr_gate.rs:496` 같은 서버. 버전 문자열이 아닌 기존 프로토콜/권한 판정을 사용하며 런타임 설정값은 이 레그에서 확인하지 않음 |
| pane identity | `D/herdr_terminate.rs:453`, `:548`, `S/session_host/herdr_gate.rs:455` old nonce/owner/location + witnessed exact close. 같은 숫자 pane 재사용은 새 권한 아님 |
| launch 인자 | Claude `handoff.rs:17/29/85`, Codex `launch_policy.rs:178/210/256`, `herdr_launch.rs:732`. 기존 설정·계정·source 검증을 통과시켜야 하며 resume-only 가드 완화 금지 |
| 경계 시각 | request 도착·seed commit(수락)·fence·close effect·retire·해제/wake·owner의 Pending commit·seed submit·source attach를 분리. 이전 uncertain 입력은 replay하지 않음. 직접 native draft는 접수 원본 보장 밖임을 명시 |
| 동시성 경합 | exclusion은 old fence→retire→old disconnect까지. clear는 timeout 후 진행하므로 postlude와 응답 직전의 seed 존재 확인이 필요(3.4). 해제 뒤 launch까지는 owner 단일 actor·FIFO·Pending CAS가 막는다(3.0). 두 번째 요청은 큐의 seed를 보고 Busy, Cancel/Terminate/actor 교체는 nonce·token 불일치로 보이는 실패 |
| 옛 형식 복원 | 새 V2 필드·variant 0이라 스냅샷 형식이 바뀌지 않는다. seed는 큐 항목이므로 기존 census가 찾는다(`compat.rs:33`). legacy inflight 호환 유지, unknown/newer 오인 복원 금지 |
| 문서로만 선언한 가드 | seed head 적재·철회, in-flight gate, seed permit, ingress의 source 거절, postlude의 seed 확인, `end_herdr` Retired 보강은 **새 구현·시험이 필요한 포트**다. 기존 transition/fence가 이것까지 제공한다고 주장하지 않음 |
| seed의 headless 진입 | herdr에서 headless 시작은 지금 전부 거절이라 `headless_turn.rs:159` 뒤 구간이 herdr 채널로 생산에서 돈 적이 없다. intake와 공유하는 reconcile(`intake_turn.rs:1494`, `headless_turn.rs:808`)은 확인했으나 구간 전체는 B 첫 커밋의 실제 진입 시험으로 확인해야 한다. #8125와 같은 문을 좁게 여는 것이므로 그 일감과 순서를 맞춘다 |
| 잘못된 0/PASS | 모델/current refusal의 안전 GREEN≠parity; owner Accepted/launch Launched≠Ready; PG skip/시험0≠PASS. 실제 물리 효과·선택 수·원본 receipt와 결과 메시지로 판정 |
| 기존 결함 넓히지 않음 | `D/commands/control/provider_reset.rs:58`의 tmux recreate bool을 소비하지 않고 `:70` Applied로 끝나는 기존 가능성, clear teardown unknown fallback은 그대로 두고 herdr 성공 판단에 복제하지 않음. launch 중인 턴에 대한 clear의 보류(`control/herdr_recovery.rs:158`)·unplaced Pending 종료 미확인(`clear_teardown.rs:202`)은 일반 메시지에도 있는 기존 동작이며 모델에 기대 RED로 남겼다(후속 후보, 3.7 U3). 새 이슈/PR은 이 레그에서 쓰지 않음 |

## 6. 상태 차트 산출물

r1 모델 `HerdrRestart`(restart가 직접 launch하는 방향)는 그대로 두었다. A 레인이 같은 파일들에 Codex resume 경계를 반영하는 중이라 제자리에서 고치지 않았다.
채택안은 같은 영역의 새 모듈 `HerdrRestartOwner`로 검사한다. 값 범위·수정 전/후 표·규칙·TLC 결과·Mermaid·재현 명령은
[`session-lifecycle/herdr-restart-owner.report.md`](statecharts/session-lifecycle/herdr-restart-owner.report.md)에 있다.
r1 보고서는 [`session-lifecycle/herdr-restart.report.md`](statecharts/session-lifecycle/herdr-restart.report.md)다.

- 42 cfg: GREEN 20, 기대 RED 22(main 1, 원인 변형 19, 기존 결함 표시 2). 불변식 13개와 진행성 5개 모두 그것을 깨는 cfg가 있다.
- restart 중 clear(직렬·overtake), dispatch 실행 중, crash 시점 전부(비결정 1회), 옛 바이너리로의 롤백, 시작 row 여섯 가지를 포함한다.
- r1의 순서를 변형으로 넣은 `R1Launch`는 `NoResumeAfterClear`·`ClearForgets`가 깨진다. 별도 내구 필드를 쓰는 `IntentField`는 롤백에서 `RequestResolves`가 깨진다.
- 모델은 새 코드가 아닌 설계 포트를 검사한다. seed의 headless 진입, provider별 I/O, 실제 부팅 복원은 5절의 시험으로 증명해야 한다.

`docs/statecharts/expected.tsv`에 r1 23행과 r2 42행을 등록했다(행 추가는 코디네이터 사전 허용). r1 23행은 A 레인도 같은 내용을 추가하므로
병합 때 같은 자리에서 겹친다. A 쪽 행을 남기고 r2 42행을 그 뒤에 두면 된다.

## 변경 이력

- r1 (2026-11-13, `2c88733814`): 진입점 전수표, restart가 직접 launch하는 방향, `HerdrRestart` 모델 23 cfg.
- r2 (2026-11-13): 리뷰 d1의 방향 제안을 판정해 owner 큐 seed 방향을 채택(적재 → 종료 순서). P2-1·P2-3·P2-4·P2-6 반영, P2-2·P2-5는 A 접점만 기록, 사용자 결정 항목 U1~U4, B 크기 재추정, `HerdrRestartOwner` 모델 42 cfg.
