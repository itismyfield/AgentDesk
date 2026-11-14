# Herdr `/restart` 사용자 동작 일치 설계

기준: `origin/main` = `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`, 2026-11-13 KST.
대상 이슈: [#8128](https://github.com/itismyfield/AgentDesk/issues/8128).
**r3 목표 계약과 선행 A/B1 증거를 구별한다. B2a 종료·postlude·Gate와 B2b seed 적재·permit·Codex resume 배선은 구현됐고, 실행 증거는 각 조각의 구현 보고서가 head별로 적는다. 아래 1.1절의 "B2b 미완" 표기는 B2a 시점의 기록이다.**

이 문서의 목표는 인증된 `/restart`와 같은 구현을 쓰는 `!restart`에서 기존 실행을 끝내고,
가능하면 같은 대화를 새 incarnation으로 이어 시작하는 것이다. 진행 중인 턴은 기존 tmux와 같이
손실 가능성을 알리고 중단한다. 이미 접수한 대기 입력은 보존하며, 종료한 실행으로 다시 보내지 않는다.
새 incarnation의 준비/인사말 시작이 확인되거나, 확인하지 못한 이유가 사용자에게 보이는 것으로 요청을 끝낸다.

`/model`의 live 적용 → `/compact` → 세션 리셋/`tmux_recreate` → `!clear` 종료 기록 → 진단 화면은 후속 순서다.
일반 API·루틴·iMessage 시작은 #8125, busy 합류는 #8114, 양쪽 실제 시나리오 수용은 #8120에서 추적한다.
이 문서는 #8128 전체 DoD를 완료했다고 주장하지 않는다. 불가피한 사용자 차이로 새로 승인받은 항목도 없다.

**r2(2026-11-13):** 설계 리뷰 d1의 방향 제안을 판정해 3·4절을 다시 썼다. restart는 인사말 seed를 owner 큐 head에 먼저 적재하고
옛 실행을 정확히 끝낸 뒤 owner를 깨우는 데까지만 하며, successor launch는 owner의 기존 herdr 턴 경로가 한다.
1절의 r1 기준선 표는 보존하고, 현재 B2a 연결은 뒤의 1.1절과 입력 출처 표에서 갱신한다.

**r3(2026-11-13):** 사용자 결정 4건(#8128 댓글, 20:15)과 설계 리뷰 d2의 P2 5건을 반영했다.
dispatch 실행 중에도 재시작하고(3.3), `!clear`는 새 세션이 뜨는 중에도 이기며(3.4), seed를 넣을 수 없어도 종료 전에 거절하지 않는다(3.6).
인사말이 대기 메시지보다 앞서는 순서는 불가피한 차이로 인정됐다. tmux와의 차이 전수와 남은 결정은 3.8에 있다. 3.1~4절이 r3 본문이다.

## 1. 기준선과 진입점 전수표

선행 O-a의 shared child reader/store 준비는 main의 `aa630a5bbf9179452de42704d60a9b6167aeddcc`에 있으며
`src/services/tui_o/store/thread_children.rs:15`의 verified surface/activation guard를 그대로 사용한다.
S3의 continuation custody/barrier는 main의 `51508b1f8ec75f0cb6254ce49a3d46442ec7e6bc`에 있으며
`src/services/turn_orchestrator/queued_head/v2/effort_continue.rs:17`은 verified caller만 intent를 만들게 한다.
두 커밋은 기준 HEAD의 ancestor임을 확인했다. 준비된 store·owner original/ticket·barrier 기구를 재구현하지 않는다.
O-a는 child 활성화 전체가 아니고 S3는 command launch/취소 hook 자체가 아니다.
따라서 이 선행 착지만으로 herdr `/restart`가 충족됐다고 표시하지 않는다.

아래 첫 표는 위 기준 SHA의 **r1 역사적 기준선**이며, 현재 B2a 동작은 1.1절이 우선한다. 표의 herdr 거절을 현재 `/restart` 전체의 동작으로 읽지 않는다. `D`는 `src/services/discord/`, `S`는 `src/services/`,
`DB`는 `src/db/`를 뜻한다. 경로를 생략한 두 번째 좌표도 같은 행의 파일이다.
설계 모델의 운영 값 범위는 이 표에서만 가져온다. 모델의 단계명은 이 호출 순서의 투영이며 새 생산 enum이 아니다.

| 경로/소비자 | file:line | r1 기준선 동작·값·범위 |
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
| owner 내구 상태 | `src/services/turn_orchestrator/queued_head/v2_types.rs:20`, `:38`, `:81`, `:93` | V2 optional handoff/effort intent, 원본/attempt/target epoch. r2부터 새 필드를 더하지 않고 seed를 `items`와 admission bind context에 둔다(3.5) |
| 입력 없는 boot debt | `src/services/turn_orchestrator/queued_head/compat.rs:23` | `holds_input`는 items/head/retained/effort만 셈. r2의 seed는 items의 한 항목이라 추가 연결이 필요 없다 |
| boot census 필터 | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs:88`, `:92` | `v2_channels`가 holds_input/unfinished Claude handoff만 선택. r2의 seed가 든 파일은 holds_input으로 이미 선택된다 |
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

### 1.1 B2a 현재 연결 전수표

조사 기준은 `5b8811fd4efd57f1de15df806002afc868a9523d` 뒤 **미커밋 B2a source가 적용된 checkout**이다. 아래 child는 proposed가 아니라 실제 파일이다. 연결 유무를 읽기 전용으로 확인했으며 Rust/PG PASS는 아직0이다. [B2a 모델 보고서](statecharts/session-lifecycle/herdr-restart-owner-b2a.report.md)의 모듈·액션 대응과 입력 분류가 같은 경계를 설명한다.

| 진입점/모듈 | 현재 file:line | B2a 실제 동작·후속 범위 |
|---|---|---|
| slash 등록·인증 | `src/services/discord/commands/restart.rs:380`, `:334`, `:337`, `:346` | interaction ID를 operation으로 사용. 기존 auth/slash policy 유지 |
| `!restart` | `src/services/discord/commands/text_commands.rs:129`; `src/services/discord/commands/restart.rs:355`, `:371` | 기존 intake 뒤 message ID로 같은 공통 restart, 원 명령 메시지에 reply |
| Herdr 전용 분기 | `src/services/discord/commands/restart.rs:262`, `:283`, `:298`; `src/services/discord/commands/restart/herdr.rs:28` | cfg(unix) Herdr 설정 채널은 기존 tmux reset/seed 전에 child로 분기·return. global managed guard는 유지 |
| Operation·Gate | `src/services/turn_orchestrator/restart_operation.rs:3`, `:10`; `src/services/discord/commands/restart/herdr.rs:39`, `:43`, `:46`, `:54` | provider/token namespace+channel+요청 ID, 완료 결과 먼저 조회/획득 뒤 재확인. 첫 프로세스 메모리만이며 seedless crash 뒤 영속 같은op 보장0 |
| detached worker·결과 cache | `src/services/discord/commands/restart/herdr.rs:57`, `:58`, `:59`, `:65`, `:66`; `src/services/discord/queue_io/input_owner/restart_gate.rs:22`, `:76` | snapshot→warn→spawn→종료 결과 저장→Gate 해제. caller 취소에도 worker를 보존하는 범위는 spawn 뒤, warn await 전후까지 무조건 완료라고 주장하지 않음 |
| 선택·dispatch 결합 | `src/services/discord/commands/restart/herdr.rs:85`, `:105`, `:108`, `:124`, `:130`; `src/services/discord/herdr_terminate/restart_recovery.rs:20` | Known/placed/owner 동일만, row dispatch와 captured dispatch equality를 별도로 보유. Missing/unknown/unplaced는 seed 없이 보이는 실패 |
| live exact 종료 | `src/services/discord/commands/restart/herdr.rs:156`; `src/services/discord/herdr_terminate/http_force.rs:62`; `src/services/discord/herdr_terminate.rs:478`, `:483`, `:520`, `:549`, `:590`, `:595`, `:597` | 기존 captured actor/token·transition·nonce fence·row 재확인·exact close·종료 확인·retire를 소비. 선취소·queue purge·직접 launch0 |
| idle close 직후 crash의 명시 재진입 | `src/services/discord/commands/restart/herdr.rs:141`; `src/services/discord/herdr_terminate/restart_recovery.rs:32`, `:74`, `:90`, `:111`, `:134`, `:140`, `:149`, `:152` | 새 명시 요청이 Ended Bound/placed Pending을 fence 아래 다시 확인하고 retire/settle, close 재전송0. Retired는 proof(`:118`, `:125`)로 정리 재개. autonomous boot repair가 아님 |
| captured turn 결산 | `src/services/discord/herdr_terminate.rs:315`, `:338`, `:355`, `:604`; `src/services/discord/herdr_terminate/restart_recovery.rs:152` | HostTerminated finalizer로 captured snapshot/token을 결산. restart child가 retry dispatch를 만들지 않음. tmux와 dispatch 최종 상태·재시도·게시 수 동등성은 PG 미검증 |
| clear 우선 postlude | `src/services/discord/commands/restart/herdr.rs:206`, `:207`, `:215`, `:223`, `:229`; `src/services/herdr_launch/clear_epoch.rs:46`, `:57` | snapshot epoch을 disconnect 전후 확인. 겹친 clear면 exact retired selector CAS 삭제; 아니면 selector 보존. CAS=false만으로 종료를 실패로 돌리지 않음 |
| selector 보존 disconnect | `src/services/discord/herdr_terminate/retired_exclusion.rs:100`, `:113`; `src/db/dispatched_sessions/hosted_execution/http_force.rs:34`, `:44`, `:45`, `:55` | exclusion 아래 row owner/nonce/dispatch 조건으로 disconnect. 새 세션·다른 dispatch를 지우는 권한을 추가하지 않음 |
| owner hold·release wake | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:175`, `:181`, `:613`; `src/services/discord/queue_io/input_owner/restart_gate.rs:76`, `:82`, `:83` | Gate 중 front hold, 결과 cache 뒤 Gate Drop의 notify/owner wake. 원본과 대기 큐를 재생성·폐기하지 않음 |
| `!clear`·`/clear` | `src/services/discord/commands/text_commands.rs:100`; `src/services/discord/commands/control.rs:523`, `:530`; `src/services/herdr_launch/clear_epoch.rs:65`; `src/services/discord/commands/control/clear_teardown.rs:73` | 즉시 epoch 증가/clearing, transition timeout 후에도 진행, B1 startup/effect 대기. B2a postlude는 이를 재확인 |
| 모듈 연결 | `src/services/discord/commands/restart.rs:14`; `src/services/discord/herdr_terminate.rs:4`; `src/services/discord/queue_io/input_owner/mod.rs:15`; `src/services/turn_orchestrator.rs:115` | 작은 child 선언. 새 V2/serde·trait·Core0 |
| seed·일반 boot 미완 | `src/services/discord/commands/restart.rs:298`, `:312`; `src/services/discord/commands/restart/herdr.rs:26`; `src/services/discord/router/message_handler/headless_turn.rs:159`; `src/services/discord/runtime_bootstrap/queued_recovery.rs:50` | Herdr는 기존 tmux seed 전에 return하여 인사말 미시작을 응답. seed/admission/우선순위·일반 boot의 restart debt·successor Ready는 B2b |

### 입력 출처와 불변식의 수락 경계

| 출처 | 현재 file:line | B2a 수락·재시작 중 처리 |
|---|---|---|
| Discord 사람의 `/restart`·`!restart` | `src/services/discord/commands/restart.rs:334`, `:337`, `:346`, `:355`, `:371`; `src/services/discord/router/intake_gate.rs:516` | 기존 auth/policy 뒤 operation 고정, 같은 Herdr 종료 분기. 명령 자체를 인사말 seed로 접수하지 않음 |
| Discord 사람의 일반 메시지 | `src/services/discord/router/intake_queue_transaction.rs:36`, `:43`, `:121`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:175` | owner에 접수된 original/receipt는 Gate 동안 hold, 해제 뒤 wake. 기존 captured 진행 턴은 중단 경고·HostTerminated 결산 대상 |
| Discord 허용 봇/announce | `src/services/discord/router/intake_gate.rs:355`, `:491`, `:496`, `:514`, `:526`; `src/services/discord/router/intake_queue_transaction.rs:38`, `:45` | 기존 turn 필터·허용 봇 인증 예외·텍스트 명령 정책 유지. owner에 이미 접수한 original custody만 포함; 기존 cancel-preservation 차이를 사람과 같다고 확대하지 않음 |
| 기타 봇 | `src/services/discord/router/intake_gate.rs:361`; `src/services/discord/router/intake_queue_transaction.rs:47` | 기존 비접수 정책 유지. 미접수 입력을 loss=0 계수에 넣지 않음 |
| TUI 관측·claim된 실제 턴 | `src/services/discord/tui_prompt_relay/synthetic_start/claim.rs:54`; `src/services/discord/tui_prompt_relay/synthetic_start_wiring.rs:618`; `src/services/discord/herdr_terminate.rs:478` | old inflight를 captured actor/token/snapshot으로 결산. TUI가 친 문자 `/restart`를 Discord 명령으로 새로 해석하는 경로0 |
| TUI 미제출 draft/직접 키 | `src/services/claude/herdr_turn/composer.rs:59`; `src/services/session_host/herdr_gate.rs:549`; `src/services/discord/commands/restart.rs:291` | durable queued original과 별개. pane 종료 때 중단될 수 있으며 직접 키 전체를 무손실 접수했다고 주장하지 않음 |
| API·HTTP·루틴·iMessage 일반 시작 | `src/services/discord/router/message_handler/headless_turn.rs:159`, `:165`; `src/services/turn_host.rs:191` | 기존 executor_not_wired 거절 유지. 명시 Terminate API는 기존 별도 endpoint이며 restart seed 권한으로 확대하지 않음 |
| restart 인사말·일반 boot | `src/services/discord/commands/restart.rs:298`, `:312`; `src/services/discord/commands/restart/herdr.rs:26`; `src/services/discord/runtime_bootstrap/queued_recovery.rs:50` | B2a 인사말 생성0. seed·admission·우선순위와 일반 boot의 restart 복구는 B2b 미완 |

수락 입력은 owner durable original/attempt로 보유한 입력이다. **B2a의 old pane 입력 차단 선형화는 transition+old nonce termination fence 획득**이며, 명령 수신·Gate 획득·종료 확인을 seed 내구 수락과 같게 취급하지 않는다. B2a에는 seed 내구 수락 자체가 없다. 이미 시작된 send는 기존 receipt로 `NotSent / acknowledged / uncertain`을 결산하며 uncertain은 맹목 재전송하지 않는다. queue/history purge는 추가하지 않는다.

r3 목표의 **seed 내구 commit 수락**, 전송 중 단위 바로 뒤 적재·우선 실행·successor Ready 및 seed를 통한 crash 복구는 아래3절의 B2b 미완 계약이다. 기존 일반 boot 큐/inflight 복원이 그 새 동작을 이미 제공한다고 주장하지 않는다.

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
| 인사말과 대기 입력의 순서 | 인사말이 대기 입력 뒤 | 동작 없음 | **불가피한 차이(사용자 인정)** → herdr는 인사말이 대기 입력 앞(3.8 U5) |
| 재시작 중 새 Discord 입력 | 기존 transition/queue 정책 | 기존 transition/queue 정책 | 기구는 같음 → owner 큐에서 seed 뒤에 선다. exclusion은 종료와 disconnect 동안만 |
| 옛 incarnation으로 입력 | tmux 종료/재생성 | restart 없음 | 다름 → old fence 유지, 새 nonce/target epoch를 재판정한 owner만 제출 |
| 인사말 exactly once | accepted seed 재시도 안 함 | headless start 거절 | 다름 → operation에서 유도한 ticket 하나 + seed permit + 기존 owner receipt, uncertain seed 재전송 금지 |
| 성공 표시 | `Started`는 start 반환, Ready 증거 아님 | 거절 설명 | 다름 → 명령 응답은 종료 확인 + 인사말의 ticket 상태. Ready는 인사말 턴의 응답, 실패는 owner 알림(3.8 U2). admission만으로 Ready를 말하지 않음 |
| close/create 미확인 | 일부 기존 결과가 요청 결과만 표시 | 명령 거절 | 다름 → herdr unknown ACK/identity는 Pending/custody 보존 + 사용자 실패. 이름 fallback/임의 kill 없음 |
| 동시 `!clear` | clear가 명시적으로 큐 폐기 | clear가 명시적으로 큐 폐기 | 같게 맞춤 → clear 우선. 채널 clear epoch으로 seed가 큐에 있든 새 세션이 뜨는 중이든 clear가 이긴다(3.4) |
| provider 설정 적용 | 기존 launch 인자 사용 | fresh launch 설정 경로 사용 | 새 launch도 같은 검증을 사용. `/model` live 변경 수용은 후속이며 이 조각으로 완료 표시 안 함 |
| 일반 API/루틴 시작 | headless 시작 가능 | `executor_not_wired` | 범위 밖 다름: #8125. restart seed만 좁게 연결하며 전체 허용 금지 |
| dispatch 실행 중 재시작 | 턴을 끊고 진행, dispatch 상태는 쓰지 않음 | 거절 | 같게 맞춤 → 거절 없이 재시작, dispatch 상태를 쓰지 않고 successor가 물려받지 않게 함(3.3) |
| 인사말을 대기열에 넣을 수 없을 때 | reset 뒤 인사말 실패를 알림 | 거절 | 같게 맞춤 → 종료 전 거절 없음, 인사말 실패 문구(3.6) |

위 행의 herdr 미구현은 불가피한 차이로 인정하지 않는다. 오류·identity 미확인은 성공과 구별한 실패이며,
tmux의 기존 오판까지 복제해 parity라고 부르지 않는다.

## 3. 구현 방향과 경합 처리 (r3)

새 host trait 또는 Core를 만들지 않는다. 기존 `terminate_explicit_herdr`, `RetiredExclusion`,
owner original/ticket/receipt, 그리고 **owner의 기존 herdr 턴 경로**(Discord 메시지가 지금 쓰는 launch)를 연결한다.
`clear_teardown::end_herdr`가 쓰는 종료 서비스는 재사용하고 clear의 selector 삭제 postlude는 재사용하지 않는다.

3~5절의 짧은 파일 이름은 다음을 뜻한다: `owner_input.rs` = `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs`,
`headless_turn.rs` = `src/services/discord/router/message_handler/headless_turn.rs`, `intake_turn.rs` = `src/services/discord/router/message_handler/intake_turn.rs`,
`provider_dispatch.rs` = `src/services/discord/router/message_handler/provider_dispatch.rs`, `restart.rs` = `src/services/discord/commands/restart.rs`,
`control.rs` = `src/services/discord/commands/control.rs`, `clear_teardown.rs`·`herdr_recovery.rs` = `src/services/discord/commands/control/` 아래,
`herdr_terminate.rs` = `src/services/discord/herdr_terminate.rs`, `retired_exclusion.rs` = `src/services/discord/herdr_terminate/retired_exclusion.rs`,
`http_force.rs` = `src/db/dispatched_sessions/hosted_execution/http_force.rs`, `handoff.rs` = `src/services/claude/herdr_turn/handoff.rs`,
`first_input.rs` = `src/services/codex/herdr_turn/first_input.rs`, `herdr_launch.rs` = `src/services/herdr_launch.rs`, `turn_host.rs` = `src/services/turn_host.rs`,
`effects.rs` = `src/services/dispatched_sessions/herdr_kill/effects.rs`, `v2.rs`·`v2/…`·`v2_types.rs`·`compat.rs` = `src/services/turn_orchestrator/queued_head/` 아래.

### 3.0 방향 판정: owner 큐 seed 방향 채택

(r3 주: 아래 표는 r2의 방향 비교다. 채택 열의 clear·dispatch·비용 칸은 r3에서 3.3·3.4·4절이 대체한다. seed의 위치는 "큐 head"에서 "전송 중 단위 바로 뒤"로 정밀해졌다. B 구현 전 재조사한 최신 비용과 분할 경계는 [B 분할 계획](herdr-parity-restart-b-split.plan.md)이 대체한다.)

r1(현 방향)은 restart 명령이 exclusion 안에서 lower `launch_herdr_session`을 직접 부르고 그 뒤 seed를 넣는다.
리뷰 d1의 새 방향은 restart가 종료와 seed 적재까지만 하고, successor launch는 owner의 턴 경로가 seed를 첫 입력으로 처리하며 한다.
**새 방향을 채택한다.** 다만 리뷰 제안의 순서(종료 → 적재)를 **적재 → 종료**로 바꾼다. 이유는 3.5의 crash 표에 있다.

| 항목 | 현 방향(r1): restart가 직접 launch | 새 방향(채택): seed를 큐 head에, launch는 owner 턴 |
|---|---|---|
| 흐름 | 종료(`src/services/discord/herdr_terminate.rs:401`) → disconnect(`retired_exclusion.rs:100`) → exclusion 안에서 provider 준비·`launch_herdr_session`(`src/services/herdr_launch.rs:378`)·attach → seed permit → 해제·wake | seed 원본을 큐 head에 내구 적재 → 종료 → disconnect → 해제·wake. launch는 owner 턴이 한다: `owner_input.rs:835` start_idle → `provider_dispatch.rs:337` drive → `src/services/claude/herdr_turn.rs:275` 또는 `src/services/codex/herdr_turn.rs:200` → `src/services/herdr_launch.rs:378` |
| 이미 있는 선례 | 없음. launch를 턴 밖에서 부르는 생산 호출자가 없다 | 세션 강제 종료가 같은 모양이다: 종료 + selector 보존 disconnect(`src/services/dispatched_sessions/herdr_kill/effects.rs:131`), 큐 보존, 해제 뒤 wake(`effects.rs:141`). 그 뒤 첫 입력이 Retired row 위에 successor를 띄운다(`src/services/herdr_launch.rs:433`~`:449`) |
| 새로 만들 것 | lower launch용 lease accessor, Claude `launched_source`(`handoff.rs:40`) 분해, Codex composer→seed→attach 순서 재현, V2 restart intent + 부팅 census 연결, seed permit | seed 적재·철회·gate·permit 작은 child 1개, typed 연결 3곳. launch·attach·unplaced 해결·Pending 재사용은 기존 경로 그대로(`src/services/codex/herdr_turn.rs:228`~`:264`, `src/services/claude/herdr_turn.rs:395`~`:427`) |
| B 비용 추정 | 리뷰 P2 반영 뒤 prod 14~18파일, 순증 +1050~1400. +1200을 넘겨 2분할 필요. `herdr_terminate.rs`(949줄)에 held-guard 진입점을 더하면 giant 경계(1,000줄)를 넘는다 | prod 13~15파일, 순증 +480~620. 한 리뷰 단위 |
| clear 경합 | exclusion이 launch·seed까지 길다. clear는 3초 뒤 지나가고(`control.rs:524`) 자기 종료는 `transition busy`로 거절된다(`herdr_terminate.rs:483`). successor가 clear 뒤에도 살아남는다(P2-1). 고치려면 restart가 자기 successor를 exact 종료하는 경로가 새로 필요하다 | exclusion은 종료와 disconnect 동안만이다. 해제 뒤에는 기존 clear가 Retired row를 직접 처리한다(`control/herdr_recovery.rs:58`, `:83`, `:115`). exclusion 중 clear가 지나가면 **seed가 폐기된 것**이 신호가 되어 postlude가 selector를 clear 뜻대로 지운다(3.4). successor가 아직 없으므로 종료할 대상이 없다 |
| crash 시점별 결과 | intent의 stage와 row를 대조하는 부팅 복구가 새로 필요(종료 뒤·Pending 뒤·seed 뒤 각각) | seed가 큐에 있으므로 기존 부팅이 그대로 몬다(`compat.rs:33`, `pending_queue_persistence/queued_recovery.rs:92`). 적재 뒤·종료 전 crash는 permit이 보이게 거절한다(3.5) |
| 부팅 census | 입력 0·intent만 있는 파일을 찾도록 `holds_input`·census 연결 필요 | 연결 0. seed는 `items`의 한 항목이다 |
| 롤백 | 새 V2 필드를 옛 바이너리가 조용히 지운다(P2-6). 버전 표지가 필요하다 | 새 필드·새 serde variant 0. 옛 바이너리는 seed를 headless start로 읽어 `executor_not_wired`로 보이게 거절한다(`headless_turn.rs:159`~`:165`, `provider_dispatch/start_failure.rs:21`) |
| Ready 표시 | 명령이 launch를 기다리지만 Ready는 여전히 seed receipt로 판정(r1 10단계) | 명령 응답은 "옛 실행 종료 + 인사말 등록"까지. Ready는 인사말 턴의 응답으로 보이고, launch 실패는 owner의 기존 시작 실패 알림으로 보인다 |
| 해제~launch 사이 경쟁 | 해당 없음(exclusion 안에서 launch) | owner는 채널당 단일 actor이고 seed가 큐 front다(FIFO). HTTP·루틴 시작은 herdr에서 거절(`turn_host.rs:191`), 열려도 owner 큐 뒤로 들어간다(`owner_input.rs:621`). 동시 launch는 `install_pending_pg` CAS가 하나만 남긴다(`herdr_launch.rs:482`~`:487`). 종료 API는 nonce가 정확히 맞아야 한다(`herdr_terminate.rs:453`) |
| 남는 위험 | P2-1 수리 경로(자기 successor 종료)와 lock 순서 | seed의 시작 경로가 headless다. herdr에서 headless 시작은 지금 전부 거절이라(`headless_turn.rs:159`) 그 뒤 구간이 생산에서 돈 적이 없다. B 첫 커밋에서 실제 진입 시험으로 확인한다(3.1 8단계) |

채택 근거는 세 가지다. (1) launch를 새로 조립하지 않고 매일 도는 경로를 쓴다. (2) P2-1의 원인(긴 exclusion 안의 launch)이 구조에서 사라지고,
P2-6의 원인(새 내구 필드)도 사라진다. (3) r2 당시 비용 추정은 직접 launch보다 작았다. 최신 조사에서는 clear 효과 허가와 idle 복구 비용을 포함해 두 조각으로 나눈다(4절). 같은 모델에서 r1의 순서를 변형(`R1Launch`)으로 넣으면
`NoResumeAfterClear`가 깨진다(6절).

### 3.1 채택 흐름 (r3)

r3는 사용자 결정(2026-11-13 20:15)에 따라 "종료 전 거절"을 두 경우(읽을 수 없는 row, unplaced Pending)만 남기고 모두 없앴다.
seed 적재는 종료의 전제 조건이 아니다. 넣을 수 있으면 넣고, 못 넣으면 seed 없이 재시작한다(3.6).

1. 기존 명령 auth/policy를 통과한 뒤 `restart_channel`(`src/services/discord/commands/restart.rs:259`)에서 herdr 설정 채널이면 전용 분기로 간다.
   legacy tmux 분기와 `managed_reset_refusal`(`:218`)은 그대로 둔다. 전역 가드를 완화하지 않는다.
2. canonical row를 읽어 3.2의 표로 분기한다. 거절 행은 여기서 보이는 실패로 끝나고 아무것도 바꾸지 않는다.
3. 채널의 **clear epoch**(3.4)을 읽어 둔다. 종료가 필요한 행이면 **in-flight gate**(프로세스 메모리, RAII)를 건다.
   gate는 적재 결과와 무관하게 postlude가 끝날 때까지 유지한다. gate는 Drop에서 owner를 깨운다.
4. **기존 operation ticket 선조회 후 seed 적재 시도.** operation은 interaction ID 또는 `!restart` message ID에 token/provider/channel namespace를 결합한 값이다.
   현재 row로 새 context를 만들기 전에 기존 ticket을 읽는다. 진행 중이면 그 작업에 합류하고, 완료됐으면 기존 결과를 재응답한다. 이 경우 close·create·seed 수를 늘리지 않는다.
   receipt만 남은 만료 operation도 새 요청으로 바꾸지 않는다. 상세 상태를 복원할 수 없으면 이미 처리됐음을 표시하고 종료 효과를 금지한다.
   identity/context Conflict는 큐 포화·미채택 같은 admission 불가와 다른 결과다. Conflict를 seedless로 내려 종료해서는 안 된다.
   여기서 ticket과 합성 원본(기존 인사말 문구, `restart.rs:13`)을 유도하고, admission context에 `source = agentdesk_restart_seed`,
   operation, 종료 대상 nonce, row의 active dispatch, drop receipt용 origin(`src/services/turn_orchestrator/queued_head/v2/origin.rs:14`가 읽는 자리)을 담는다.
   - **위치.** owner가 이미 전송 중이거나 claim한 단위(head와 그 followers)가 있으면 seed는 **그 단위 바로 뒤**, 없으면 맨 앞이다.
     전송 중 단위는 큐 맨 앞에 그대로 있어야 결산된다(`v2/retained.rs:405`, `v2.rs:259`). 기존 continuation의 prepend(`v2/source_admission.rs:971`)는
     미결 head가 있으면 거절하므로(`v2/effort_continue.rs:168`) 그대로 쓰지 못한다. 새 owner child가 단위 길이 뒤에 넣고 Queued bind index를 그만큼 민다.
     head 여유 한 칸(`v2/effort_continue.rs:200`)은 continuation과 같은 방식으로 쓴다.
   - **적재됨:** 이 commit이 요청의 수락 시점이다. seed 뒤의 대기 입력은 seed를 앞지르지 못한다(`v2.rs:125`의 사람 선행 이동도 seed 앞으로는 막는다).
   - **적재 못 함**(큐 여유 없음, V2 미채택, `effort_restart` 미결, 그 밖의 admission 거절): seed 없이 계속한다(3.6). 수락 시점은 6단계의 fence다.
   - **commit 미확인:** 적재됐을 수도 있다고 보고 계속한다. gate가 걸려 있어 그 seed는 시작되지 않는다. 7단계에서 같은 ticket으로 다시 확인한다.
   같은 operation의 ticket은 같은 결과를 가리킨다. 큐의 다른 restart seed에 합류하면 alias operation의 receipt와 bind도 같은 commit에 기록한다.
   합류한 요청은 공유 seed를 다시 닫거나 독자적으로 철회하지 않는다. alias를 재호출해도 종료 효과는 0이다.
5. gate가 걸린 동안 owner는 seed를 시작하지 않고 기다린다(`owner_input.rs:859`~`:872`의 기존 대기 분기에 한 갈래).
   옛 턴이 도는 중이면 기존 synthetic 대기 규칙(`owner_input.rs:214`, `:245`)도 seed를 그 턴이 끝날 때까지 잡아 둔다.
6. 기존 exact terminate를 **row의 owner와 active dispatch에 묶어** 부른다(3.3). tmux 분기의 `mailbox_cancel_active_turn` 선취소는 쓰지 않는다.
   진행 중 턴이 있으면 먼저 손실 경고를 보인다. 서비스가 transition(`herdr_terminate.rs:482`)과 fence(`:519`)를 잡고 row·actor·token·dispatch를 다시 본 뒤
   exact close → 종료 관측 → retire CAS → captured 턴 결산을 한다. close 불명·provider 잔존·stale CAS·nonce 불일치는 실패다(9단계).
   close부터 postlude까지는 운영자 경로처럼 요청 future와 분리해 돌린다(`src/services/discord/herdr_terminate/http_force.rs:30`의 방식). 끊긴 요청이 close와 결산 사이에서 멈추면 안 된다.
7. `after_retire` postlude(exclusion 보유 중):
   - clear epoch이 3단계의 값과 다르거나 seed ticket이 clear 폐기로 끝났으면 **clear가 앞선 것**이다. selector를 clear 뜻대로 지우고
     (`clear_retired_selector_pg`, `http_force.rs:75`), 아직 큐에 있는 seed를 철회하고, superseded를 답한다(3.4).
   - 아니면 `disconnect_session_keeping_provider`(`retired_exclusion.rs:100`)를 dispatch 조건과 함께 실행한다. clear의 queue purge·history erase·catch-up discard는 부르지 않는다.
   - commit 미확인이었던 seed는 같은 ticket으로 다시 적재를 시도해 상태를 확정한다.
   postlude가 끝나면 exclusion과 gate를 놓고 owner를 깨운다(`owner_input.rs:602`). HostTerminated 결산은 큐를 스스로 깨우지 않는다(`src/services/discord/turn_finalizer/finalize_context.rs:45`).
8. owner가 front인 seed를 시작한다. context가 보존된 시작이라 headless 진입(`owner_input.rs:912`)을 탄다. 여기에 **seed permit**을 둔다.
   origin이 owner의 내구 bind context에서 읽은 `RestartSeed`일 때만 `headless_turn.rs:159`의 판정을 intake와 같은 판정(`turn_host.rs:198`)으로 바꾸고, row를 context의 종료 대상 nonce와 대조한다.
   - 그 nonce의 실행이 **아직 돌고 있으면** 거절한다(재시작이 끝나지 않았다). 알림과 함께 종결하고, 그 뒤 대기 입력은 살아 있는 세션으로 간다.
   - 그 nonce의 실행이 **끝났는데 row가 아직 Retired가 아니면** permit이 복구 주체가 된다. inflight 파일 없는 idle Bound·location 있는 Pending도 대상이다.
     채널 transition과 그 nonce의 termination fence 아래 owner·nonce·dispatch와 종료 증거를 다시 고정하고, 기존 `execution_end`/`retire_confirmed` 및 disconnect CAS를 끝낸다.
     이 복구는 새 close를 보내지 않는다. live·unconfirmed·row 이동이면 보이는 실패로 seed를 종결한다. 기존 inflight 복구만 공정하게 실행된다고 가정하지 않는다.
   - row가 그 nonce의 Retired이고 context의 dispatch가 아직 row에 남아 있으면, 같은 selector 보존 disconnect CAS를 먼저 끝낸다(postlude 전 crash의 마무리).
   - 그 밖의 headless 시작은 지금처럼 `executor_not_wired`다(#8125). 일반 ingress(`src/services/discord/queue_io/input_owner/start.rs:13`)는 이 source를 가진 시작을 만들지 못하게 거절한다.
   B-main 첫 커밋에서 seed가 이 경로로 `launch_herdr_session`에 닿고, permit 이전의 `start_waits` 구간과 `headless_turn.rs:159`~`:926` 사이에서
   tmux 이름 기반 reset·reconcile·clear가 herdr 채널에 실행되지 않음을 실제 진입 시험으로 확인한다. 걸리는 단계가 있으면 그 가드는 두고 seed를 intake 형 시작(`owner_input.rs:922`)으로 돌린다.
9. 실패 응답. 종료가 거절·불명으로 끝나면 **먼저 보이는 실패를 답하고 그 뒤 seed를 철회**한다(기존 대기 입력 취소와 같은 terminal receipt).
   답과 철회 사이에 crash가 나면 남은 seed를 permit이 거절해 알림이 한 번 더 나갈 뿐이다. 실패가 확정된 뒤에는 옛 세션을 다시 쓴다(3.9-2).
10. 성공 응답은 "옛 실행 종료 확인"과 인사말의 상태다. 상태는 seed의 **ticket 상태**(Queued / Attempt / Settled / Retained / clear 폐기 / 철회 / 없음)로 읽는다.
    기존 admission bind(`v2_types.rs:63`~`:73`)와 retained·drop receipt로 판정하며 새 V2 필드는 없다. "큐에 seed가 없다"만으로 clear라고 판단하지 않는다.
    wake 뒤 seed가 이미 제출됐거나(`v2/source_admission.rs:609`) 시작 실패로 retained가 됐을 수 있다.
    - Queued/Attempt: "인사말을 대기열에 등록". Settled: "인사말 turn이 시작됐습니다". Retained: 기존 실패 문구. seed 없음: `restart.rs:99`의 "인사말 turn 시작 실패: 사유".
    `Admission::Accepted`나 launch 반환을 Ready로 말하지 않는다. Ready는 seed ticket의 Settled receipt, successor nonce의 Bound, source binding으로 판정한다.

**seed 제출의 세 단계와 crash.** owner의 첫 입력은 물리 제출(`src/services/discord/queue_io/input_owner/effect.rs:96`), receipt 결산(`:130`), source bind가 따로 일어난다.
- 제출 전 crash: seed는 unsent로 큐에 돌아간다. 부팅 뒤 row가 여전히 종료 대상 nonce의 Retired면 다시 시작한다. launch가 이미 다른 nonce의 Pending·Bound를 남겼으면 **합류하지 않고 보이는 거절로 끝낸다**(B2b 판정 2026-11-14: reserve 전에는 그 successor가 이 seed의 것이라는 내구 증거가 없다. 사용자는 `/restart`를 다시 하면 된다). 남은 Pending은 다음 입력의 기존 Pending 확인이 푼다.
- 제출 뒤·receipt 전 crash: seed는 **불명**이다. 다시 보내지 않고 retained + 알림으로 끝낸다. Ready를 말하지 않는다.
- receipt 뒤·bind 전 crash: seed는 전달로 확정이고, 부팅 뒤 기존 Pending 확인이 Bound를 만든 때 Ready다.
Codex의 attach와 시작점 기록(`first_input.rs:208`, `:219`)은 A의 범위다.

Claude는 `launched_source`(`handoff.rs:40`)가 account·transcript·cursor·inode를 검증해 resume하거나 fresh로 간다. 이 경로를 분해하지 않는다.
같은 이름 tmux가 살아 있을 때만 도는 `earlier_tmux` 종료(`handoff.rs:60`~`:69`)는 일반 메시지에서와 같은 기존 동작이고 restart가 새로 얹는 권한이 아니다.

### 3.2 시작 시 row 상태별 처리 (P2-4)

| canonical row | 종료 | 처리 |
|---|---|---|
| Unknown / Conflict / 다른 owner·node | 없음 | 보이는 거절. 큐·row 변화 0 |
| Bound, location 이 node | exact terminate | 3.1의 3~10단계 전부 |
| Pending, location 있음 | exact terminate | Bound와 같다. launch evidence가 없어 서비스가 거절하면(`herdr_terminate.rs:512`) 9단계 |
| Pending, unplaced(location·expected 없음) | 없음 | 보이는 거절 + 안내(3.8 U4). 이 상태는 다음 입력의 기존 resolver가 푼다(`src/services/claude/herdr_turn.rs:403`, `src/services/codex/herdr_turn.rs:240`~`:253`) |
| Retired | 없음 | seed 적재 시도 + wake. 옛 실행은 이미 끝났으므로 terminate를 다시 부르지 않는다. gate 없음 |
| Missing / Legacy | 없음 | seed 적재 시도 + wake. owner 턴이 fresh 또는 검증된 resume으로 띄운다(`src/services/claude/herdr_turn.rs:275`). gate 없음 |

### 3.3 dispatch 실행 중 재시작 (사용자 결정 1)

**tmux가 하는 일.** `/restart`는 진행 중 턴을 취소하고(`restart.rs:223`~`:227`, `src/services/discord/mailbox_stop.rs:5`) 세션을 reset한다.
취소된 턴은 자기 dispatch를 완료로도 실패로도 만들지 않는다(`src/services/discord/turn_bridge/post_loop_finalize.rs:437`, `:439`). restart는 dispatch 상태를 쓰지 않고 재시도 dispatch도 만들지 않는다.
사용자에게 보이는 것은 손실 경고, 재시작 결과, 인사말이다.

**herdr r3.** 거절하지 않고 같은 결과를 낸다.
- terminate를 row의 owner와 active dispatch에 묶는다(`src/services/discord/herdr_terminate/http_force.rs:23`의 `Expected`). 서비스가 captured 턴의 dispatch와 fence 아래 row의 dispatch를 대조한다(`herdr_terminate.rs:499`~`:505`, `:549`).
- 결산은 HostTerminated다. captured hosted episode만 끝내고 dispatch 상태는 쓰지 않는다(`src/services/discord/turn_finalizer/finalize_context.rs:41`). 출력 소유는 기존 reader에 남는다.
- postlude의 selector 보존 disconnect가 그 dispatch 조건으로 row에서 dispatch를 떼어낸다(`http_force.rs:44`, `:55`). 새 seed 턴은 옛 dispatch를 물려받지 않는다.
- 상속 금지는 새 seed 턴에 한정한다. 이후 backlog 입력의 thread/card dispatch 재발견은 `src/services/dispatches/thread_reuse.rs:247`, `src/services/discord/router/message_handler/intake_turn.rs:1518`의 tmux 기준선을 그대로 따른다.
- HTTP 강제 종료의 `fail_or_retry`(`src/services/dispatched_sessions/force_kill.rs:325`)는 부르지 않는다. 재시도 dispatch 0, 중복 게시 0.

| 경합·경계 | 처리 |
|---|---|
| 읽은 뒤 fence 전에 row의 dispatch가 바뀜 | 서비스가 `row changed under fence`로 거절한다. close 전이다. row를 다시 읽어 한 번만 다시 시도하고, 또 바뀌면 보이는 실패(9단계) |
| row의 dispatch와 진행 중 턴의 dispatch가 다름(row에 낡은 값) | 묶지 않고 종료한다. disconnect가 `Ok(false)`여도 실패가 아니다. row는 그 값을 계속 가리킨다. tmux도 row를 고치지 않는다 |
| retire 뒤·disconnect 전 crash | seed permit이 context의 owner·nonce·dispatch와 종료 증거를 transition·fence 아래 고정해 같은 CAS를 끝낸 뒤 launch한다(3.1 8단계) |
| 위 crash + seed 없음(3.6) | row가 dispatch를 계속 가리킨 채 다음 입력이 launch한다. tmux와 같은 상태다. 모델에 기대 RED로 남겼다(`GapSeedlessDispatchCrash`) |

**확인하지 못한 것.** tmux에서 restart 뒤 그 dispatch가 최종적으로 어떻게 끝나는지(감시·시간 초과 경로)는 추적하지 않았다.
herdr는 row에서 dispatch를 떼어내고 tmux row는 계속 가리키므로, 감시가 row를 근거로 삼으면 최종 상태가 갈릴 수 있다.
B-main의 첫 PG 시험이 tmux 기준선(dispatch status, 재시도 수, 게시 수)을 기록하고 herdr 값을 그것과 같게 단언한다. 다르면 같은 writer를 쓰도록 이 절을 고친다.

### 3.4 `!clear`가 이긴다 (사용자 결정 2, r2 리뷰 P2-2)

clear는 seed가 큐에 있든, owner가 seed로 새 세션을 띄우는 중이든, 인사말 턴이 돌고 있든 이긴다. 끝난 뒤 seed 실행 0, 그 clear보다 먼저 시작한 launch가 만든 세션 0, selector 삭제다.

**기구: 채널 clear epoch**(프로세스 메모리, 새 작은 child). clear와 그것이 이겨야 하는 턴은 같은 프로세스에 있으므로 내구 기록이 필요 없다.

`CheckEpoch`는 관찰이고 효과 허가가 아니다. epoch 갱신과 `BeginEffect`는 같은 채널 상태 mutex 아래 직렬화한다.
`BeginEffect`는 epoch와 clearing 상태를 다시 검사한 뒤 효과 카운터를 올려 허가를 발급한다. 허가는 실제 create·submit이 끝날 때까지 살아 있어야 한다.
clear보다 먼저 허가된 I/O는 clear 시작 뒤에도 끝날 수 있다. clear는 startup과 허가된 I/O를 기다린 뒤 그 실행을 exact 종료하고 selector를 지운 다음 완료한다.
clear 중에는 새 startup을 active count에 더하지 않고 새 효과 허가도 발급하지 않는다. clear 완료 뒤 owner의 다음 입력은 새 epoch에서 정상 처리한다.
등록이 먼저 끝나도 startup과 겹친 사실은 clear guard에 sticky로 남겨 Retired 특수 보류 경로가 다시 선택되지 않게 한다.
async task의 TLS를 await 너머 설치하지 않는다. sync execute의 등록 Arc를 create worker와 별도 blocking thread의 CreatedSubmit에 명시 전달한다.

| 누가 | 하는 일 | 좌표(붙일 자리) |
|---|---|---|
| clear | 시작하자마자 epoch을 올린다. 그 뒤 큐 폐기·턴 취소를 한다 | `control.rs:518` 다음 |
| herdr launch 턴 | 시작할 때 `Launching`을 등록하고 그때의 epoch을 기억한다(RAII, 끝나면 대기자를 깨움) | `src/services/claude/herdr_turn.rs:275`, `src/services/codex/herdr_turn.rs:200`의 launch 직전 |
| launch | Pending CAS 뒤 CheckEpoch, worker의 실제 create 직전 BeginEffect. 허가 실패면 자기 unsent Pending을 retire하고 끝낸다 | `herdr_launch.rs:482` 뒤, `:588`의 기존 retire |
| launch 턴 | 첫 입력 CheckEpoch 후 실제 composer mutation 직전 BeginEffect. 허가 없으면 제출하지 않는다 | 기존 취소 확인 옆(`src/services/claude/herdr_turn.rs:252`, `src/services/codex/herdr_turn.rs:358`) |
| clear | 폐기에서 launch 중 턴의 미제출 입력도 terminal로 결산한다(다시 시작 0) | `src/services/discord/mailbox_finish.rs:362` 경로. 기존 동작인지 B-clear 첫 시험이 확인 |
| clear | 종료 단계에서 등록된 launch가 끝나기를 기다린 뒤(기존 launch I/O 상한) row를 읽어 처리한다 | `clear_teardown.rs:137` |
| restart | 3단계에서 읽은 epoch을 postlude와 응답에서 대조한다 | 3.1 7·10단계 |

clear 종료 단계의 row 처리(row가 움직였으면 다시 읽는다):

| 읽은 row | 처리 |
|---|---|
| Bound / location 있는 Pending | 기존 exact terminate + selector 삭제 postlude(`clear_teardown.rs:183`) |
| Retired | 그 nonce 조건의 selector CAS 삭제. 지금은 그냥 지나간다(`clear_teardown.rs:161`). `herdr_recovery.rs:115`가 Retired row에 이미 하는 일과 같다 |
| unplaced Pending, launch가 끝남(create 응답 불명) | 기존대로 종료 미확인 경고(`clear_teardown.rs:202`). 남는 계약이다 |
| Missing / Legacy | 없음 |

launch 중 턴이 있는 Retired row에서 clear는 지금 보류를 보고하고 그 턴을 그대로 둔다(`herdr_recovery.rs:158`).
B-clear는 `Launching`이 등록된 턴을 "retire된 실행의 원래 턴"과 구별해, 그 경우 일반 clear 경로(취소 → 폐기 → 위 종료 단계)로 보낸다.

restart와 만나는 시점별 결과:

| clear가 시작한 시점 | 일어나는 일 | 결과 |
|---|---|---|
| restart epoch 캡처 전 | 평범한 clear. 그 뒤 restart는 clear 뒤 상태에서 시작 | selector 삭제, fresh successor가 인사말 |
| epoch 캡처 뒤, seed 적재 전 | 종료할 실행이 없는 Retired·Vacant 요청도 마지막 적재·재적재 뒤 epoch을 확인하고 자기 seed의 철회를 확인할 때까지 gate를 유지 | seed 0, selector 삭제 유지, 새 create·재시작 성공 응답 0 |
| 적재 뒤, restart exclusion 전(clear가 transition 보유) | clear가 seed를 폐기하고 옛 실행을 끝낸다. restart의 종료는 nonce 불일치로 실패하고 응답은 epoch을 보고 superseded | seed 0, selector 삭제, 세션 0 |
| restart exclusion 중(clear는 3초 뒤 지나감) | clear가 큐를 폐기한다. 자기 종료는 `transition busy`로 끝날 수 있다(`herdr_terminate.rs:483`). postlude가 epoch을 보고 selector를 지우고 남은 seed를 철회한다 | seed 0, selector 삭제, 세션 0 |
| exclusion 해제 뒤, owner가 seed를 잡기 전 | 기존 clear가 Retired row를 처리(`herdr_recovery.rs:58`, `:83`, `:115`) | seed 폐기, selector 삭제 |
| owner가 seed 턴을 띄우는 중(Pending 전·create 중·제출 전) | clear가 미제출 입력을 terminal로 결산한다. CheckEpoch 뒤 BeginEffect는 새 허가를 거절하며, 이미 허가된 I/O는 완료까지 추적한다. clear는 startup·I/O 종료를 기다린 뒤 row를 정리한다 | 새 효과 허가 0, clear 완료 시 세션 0·selector 삭제 |
| BeginEffect 뒤·물리 create/submit 전 | 허가가 물리 I/O 완료까지 살아 있고 clear가 기다린다. 허가 전 clear였으면 여기로 올 수 없다 | 먼저 허가된 I/O 완료 가능, clear 완료 시 해당 세션 0·selector 삭제 |
| seed 제출 뒤(인사말 턴 진행 중) | Bound + 진행 중 턴의 기존 clear: 턴 취소, exact terminate | 인사말은 clear 전에 실행됨. 세션 0, selector 삭제 |
| restart의 종료가 실패했고 clear의 종료도 timeout | 둘 다 경고로 끝나고 옛 세션이 남는다 | clear의 기존 timeout 계약 그대로(`GapClearCloseFailure`) |

- **"seed 없음"은 clear의 증거가 아니다**(P2-2). selector 삭제는 epoch 이동 또는 seed ticket의 clear 폐기 기록(`v2/retained.rs:416`)에만 묶는다.
- restart는 clear가 버린 입력을 되살리지 않고 seed를 다시 적재하지 않는다.
- epoch은 일반 herdr launch 전부에 적용된다. launch 중 clear가 보류로 끝나던 기존 결함(r2의 U3)을 restart에 한정하지 않고 고친다. 일반 메시지의 launch도 같은 창을 갖기 때문이다.
- 남는 것: (1) create 응답이 불명인 채 clear가 끝난 경우의 종료 미확인 경고, (2) clear와 겹친 crash(epoch이 사라진다), (3) restart가 아닌 다른 작업이 transition을 쥔 동안 지나간 clear의 폐기~삭제 사이 경합. 셋 다 기존 계약이며 넓히지 않는다.

### 3.5 crash·부팅·롤백 (P2-6)

seed가 적재된 요청에서는 seed 원본과 그 admission context가 **유일한 내구 기록**이다. 새 V2 필드, 새 serde enum variant, 버전 상향은 없다.
부팅은 기존 순서(큐 → inflight → intake 오픈, `src/services/discord/runtime_bootstrap/queued_recovery.rs:48`, `src/services/discord/runtime_bootstrap.rs:34`) 그대로이고 gate와 epoch은 메모리라 사라진다.

| crash 시점 | 부팅 때 상태 | 결과 |
|---|---|---|
| 적재 전 | 변화 없음 | 요청이 수락되지 않았다. 옛 세션 그대로 |
| 적재 뒤, close 전 | seed가 큐에, row는 옛 nonce Bound, 실행은 살아 있음 | permit 거절 → seed는 알림과 함께 종결. 대기 입력은 살아 있는 옛 세션으로 간다. 세션 1개 |
| close 뒤, retire CAS 전 | idle Bound/placed Pending 포함, row는 옛 nonce인데 실행은 끝남 | permit이 transition·fence 아래 종료 증거를 고정해 retire/disconnect를 마무리. 새 close 0, 확인 불가면 보이는 실패 |
| retire 뒤(disconnect 전후), wake 전 | seed가 큐에, row Retired | permit이 남은 disconnect를 끝내고 owner가 seed로 launch → Ready |
| launch 중·seed 제출 전후 | seed attempt + Pending | 3.1 끝의 세 단계 규칙. create 재전송 0, 불명 seed 재전송 0. 제출 전 crash로 다른 nonce의 Pending·Bound가 남았으면 seed는 합류 없이 보이는 거절 |
| 실패 응답 뒤, seed 철회 전 | seed가 남음 | permit 거절 알림이 한 번 더. 무응답 종료 없음 |
| seed 없는 재시작(3.6)의 어느 시점 | 내구 기록 없음 | tmux와 같다. 옛 실행이 끝났으면 다음 입력이 새 세션을 띄우고, 재시작 결과 메시지는 없을 수 있다 |

- **롤백**: 옛 바이너리는 seed를 context가 있는 headless 시작으로 읽고(`src/services/discord/queue_io/input_owner/origin.rs:42`), herdr 채널이라 `executor_not_wired`로 거절해
  기존 typed 시작 실패(retained + 알림)로 종결한다. 큐 항목이라 다시 쓸 때 지워지지 않는다. 사람 입력은 영향이 없다.
  시작 실패 사유를 새 `StartFailureKind` variant로 만들지 않는다(`v2_types.rs:200`). 기존 kind + 사유 문자열을 쓴다.
- **기존 intent와의 공존**: `effort_restart`가 미결이면 seed를 넣지 않고 재시작한다(3.6). restart는 그 intent를 건드리지 않는다.
  `unfinished_handoff`(Claude handoff journal)는 기존대로 owner가 먼저 끝낸다.
- 옛 V1/V2·`runtime_kind` 없는 inflight 복원은 기존 정책 그대로다. unknown/newer 형식을 내려 읽거나 큐 파일을 지우지 않는다.

진행성은 scheduler가 owner drive와 알림 작업을 계속 돌리고 I/O가 답하거나 deadline으로 분류되는 조건 아래 보장한다.
전송 불능인 Discord에 메시지가 도착했다는 보장은 만들지 않는다.

### 3.6 seed를 넣을 수 없을 때 (사용자 결정 4, r2 리뷰 P2-1)

종료 전에 거절하지 않는다. tmux가 reset 뒤 seed를 시도하고 실패를 알리는 것(`restart.rs:239`, `:293`, `:99`)과 같은 결과를 낸다.

| 상황 | herdr r3 | 사용자에게 보이는 결과 |
|---|---|---|
| 전송 중·claim된 단위가 큐 맨 앞에 있음 | seed를 그 단위 바로 뒤에 적재(3.1 4단계). 거절 없음 | 정상 재시작. 그 단위가 진행 중 턴이면 손실 경고 |
| 큐가 가득(30) | seed는 head 여유 한 칸을 쓴다. 그 칸도 없으면 seed 없이 재시작 | 대개 tmux보다 낫다(tmux는 `Full`로 인사말 실패). 칸이 없으면 tmux와 같은 "인사말 turn 시작 실패" |
| V2 미채택 | seed 없이 재시작 | "인사말 turn 시작 실패: 사유" |
| commit 미확인 | gate를 건 채 재시작하고 postlude에서 같은 ticket으로 확정 | 확정되면 정상, 아니면 "인사말 미확인" |
| `effort_restart` 미결 | seed 없이 재시작. intent는 그대로 | "인사말 turn 시작 실패: 사유". effort 흐름의 수렴은 #8109 소관 |

- **유실 0.** seed를 못 넣어도 대기 입력은 큐에 그대로 있고, postlude의 wake 뒤 맨 앞 입력이 successor를 띄운다(세션 강제 종료 뒤와 같은 기존 경로).
- seed가 없으면 수락 시점은 종료의 fence다. 그 전에 시작한 전달은 옛 세션으로 가는 정상 전달이다.
- seed가 없으면 gate·permit·철회가 쓰이지 않는다. clear 우선은 epoch이 그대로 맡는다(3.4).
- seed가 없으면 crash 뒤 내구 기록이 없다(3.5 마지막 행). tmux의 kill 뒤·seed 전 crash와 같다.
- `effort_restart` intent를 만드는 생산 호출자는 지금 없다(`prepare_effort`의 생산 호출은 조회용 `None`뿐, `src/services/discord/queue_io/input_owner/effort_continue.rs:19`). 이 행은 #8109가 들어온 뒤에 도달한다.

### 3.7 A 조각과의 접점 (P2-2·P2-5 of d1)

A(Codex resume 경계)는 `0060c4dd50f68a7509217c29986dc58ff5e64d16`에 구현되어 독립 리뷰 중이다. B는 아래 기존 계약을 소비한다. A 리뷰가 계약을 바꾸면 코디의 통보를 받아 DESIGN_DELTA로 멈춘다.

- **resume 요청의 출처**: `HerdrLaunch.resume`은 seed 턴에서만 true가 된다. 일반 Codex 재기동은 지금처럼 fresh다(`src/services/codex/herdr_turn.rs:188`~`:197`).
  origin은 `OwnerTurn`까지 보존된다. B-main은 그 `RestartSeed` origin을 Codex 턴의 typed resume 요청으로 넘기는 배선을 갖고, A는 그 요청을 받는 포트를 준다.
  selector가 있다는 것만으로 resume을 켜지 않는다. seed 없는 재시작(3.6) 뒤의 첫 입력은 일반 입력이므로 Codex는 fresh로 시작한다.
- **시작점**: A가 Pending CAS 전에 immutable incarnation context에 내구 기록하고 Pending의 binding nonce로 연결하는 resume 시작점을 그대로 쓴다. seed가 일반 첫 입력 경로(`first_input.rs:195`~`:225`)로
  들어가므로 그 경로가 같은 nonce의 incarnation context에서 시작점을 읽어야 한다. 통합 시험은 owner seed 경로에서 `:216`·`:225`의 시작점 소비와 일반 재기동의 `resume=false`를 함께 본다.
- **부팅**: seed와 launch 사이 crash 뒤에도 시작점은 같은 incarnation context에 있고 seed는 큐에 있다. Pending/Bound successor 복원은 저장된 시작점을 쓰며 startup 뒤 EOF로 다시 측정하지 않는다.
- Claude는 접점이 없다. 기존 `resumed_session`(`handoff.rs:6`)과 `o_reads`(`:29`)가 그대로 판정한다.

### 3.8 tmux와의 차이 전수와 남은 결정

| # | 항목 | tmux | herdr r3 | 상태 |
|---|---|---|---|---|
| U1 | dispatch 실행 중 `/restart` | 턴을 끊고 진행. dispatch 상태는 쓰지 않음 | 같다(3.3) | 사용자 결정 1 반영. tmux의 최종 dispatch 상태 기준선은 B 시험이 확정 |
| U2 | 결과 표시 | 한 응답에 인사말 시작/실패 | 응답은 종료 확인 + 인사말의 ticket 상태. 새 세션 준비는 인사말 턴의 응답, launch 실패는 뒤따르는 알림 | **남는 차이**. r2 기본값 유지 |
| U3 | 재시작 직후 `!clear` | clear가 이김 | 같다(3.4) | 사용자 결정 2 반영 |
| U4 | unplaced Pending(생성 응답 유실)에서 `/restart` | 해당 상태 없음 | 거절 + "메시지를 보내면 기존 경로가 해결" 안내 | **유지**(아래 사유). 사용자 질문 범위 밖이라 확인 필요 |
| U5 | 인사말의 순서 | 대기 메시지 **뒤**(`v2/source_admission.rs:752`의 append) | 대기 메시지 **앞**(전송 중 단위 바로 뒤) | **불가피한 차이로 인정**(사용자 결정 3) |
| U6 | 큐 가득·V2 미채택·commit 미확인·effort 미결 | reset 뒤 seed 시도, 실패를 알림 | 같다(3.6). 큐 가득에서는 여유 칸 덕에 인사말이 성공할 수 있다 | 사용자 결정 4 반영 |
| U7 | 재시작 도중 daemon crash | kill 뒤·seed 전 crash는 흔적 없음 | seed가 적재됐으면 Ready 또는 보이는 실패로 끝남. seed 없으면 tmux와 같음 | herdr가 더 강함 |
| U8 | 재시작 실패(종료 거절·불명) | reset은 대개 성공으로 보고(`provider_reset.rs:70`) | 보이는 실패, 옛 세션 유지, 인사말 0 | **남는 차이**. tmux의 오판을 복제하지 않음(2절) |

- **U5가 불가피한 이유.** herdr에서는 인사말이 새 세션을 띄우는 첫 입력이고 요청의 내구 기록이다. 대기 메시지 뒤에 두면 수락 뒤에도 대기 메시지가 옛 세션으로 가거나
  (모델 `SeedBehind`가 `NoOldPaneInput`을 깬다), 대기 메시지가 permit·dispatch 정리 없이 새 세션을 먼저 띄운다.
- **U4를 유지하는 이유.** 이 상태는 pane이 생겼는지 모른다. 닫을 정확한 대상이 없고, 종료 없이 seed만 넣으면 기존 resolver가 그 pane을 입양했을 때
  재시작되지 않은 세션에 "세션 재시작 완료"가 들어간다. 거절하고 일반 입력의 resolver에 맡기는 쪽이 거짓 결과를 만들지 않는다.

### 3.9 수용 기준 문장 (B가 표로 대조할 것)

1. herdr 채널의 인증된 `/restart`·`!restart`는 옛 exact 실행을 끝내고, seed가 적재됐으면 인사말을 첫 입력으로 한 새 incarnation 하나를 띄운다.
2. 수락 시점부터 재시작이 끝나거나 **실패로 확정될 때까지** 새로 시작하는 전달은 옛 nonce로 가지 않는다. 수락 전에 시작한 전달과 실패 확정 뒤의 전달은 옛 세션으로 갈 수 있다.
   실패 확정은 실패 응답, 또는 부팅 뒤 permit의 거절이다. 접수 원본은 한 번만 전달되거나 명시 receipt로 종결된다.
3. seed는 restart 명령만 만들 수 있고, 종료 대상이 돌고 있는 동안에는 전달되지 않는다. 일반 headless 시작은 계속 거절된다.
4. 3.2의 각 행은 표의 결과를 내고, 거절 행은 close·seed·row 변화가 0이다.
5. dispatch 실행 중에도 재시작한다. restart는 dispatch 상태를 쓰지 않고 재시도 dispatch를 만들지 않으며 새 seed 턴은 그 dispatch를 물려받지 않는다. 이후 입력의 재발견·최종 상태·게시 수는 tmux 기준선과 같다.
6. clear의 epoch 갱신 뒤 create·seed 제출의 새 효과 허가 0이다. 먼저 허가된 I/O는 끝날 수 있으나, clear가 끝날 때 그 launch가 만든 세션 0·selector 삭제다(3.4의 남는 것 세 가지 제외). CheckEpoch만으로 효과를 허가하지 않는다.
7. seed를 넣을 수 없어도 종료 전에 거절하지 않는다. 대기 입력 유실 0, 응답은 tmux의 인사말 실패 문구다.
8. 전송 중·claim된 단위가 있으면 seed는 그 뒤에 들어가고 그 단위의 결산은 실패하지 않는다. claim 당시 followers는 이후 입력 도착으로 바뀌지 않는다.
9. 3.5의 각 crash 시점은 표의 결과를 내고, seed가 적재된 요청은 Ready 또는 보이는 실패로 끝난다. 제출 뒤·receipt 전 crash의 seed는 다시 보내지 않는다.
10. 응답은 seed의 ticket 상태로 정하고, superseded는 clear epoch 이동 또는 ticket의 clear 폐기 기록이 있을 때만 말한다. 같은 operation은 기존 ticket을 먼저 조회해 합류·기존 결과만 응답하며 close·create·seed를 반복하지 않는다. identity/context Conflict는 종료를 금지한다.
11. 옛 바이너리로 돌아가도 seed는 보이는 거절로 종결되고 스냅샷은 읽힌다.
12. create 응답 불명은 재전송 0, Pending 보존, 보이는 실패다.
13. tmux 채널의 `/restart`와 herdr 채널의 인접 관리 가드는 변하지 않는다.

## 4. 구현 분할과 크기

**B 구현 전 재조사:** 생산 순증 +1124~1463(중심 +1294)로 +1200 PLAN_READY 선을 넘는다.
생산 코드를 쓰기 전에 B1(clear 효과 허가·claimed followers, prod11~13/+334~493)과
B2(seed·명령·복구 활성화, prod16~22/+790~970, B1 head 기준)로 나눈다.
파일별 비용·누락되었던 worker/idle 복구 비용·수용 문장별 시험 대응은 [B 분할 계획](herdr-parity-restart-b-split.plan.md)에 있다.
아래 r3 추정 표는 이전 설계의 비교 기록이며 최신 크기 판단에는 쓰지 않는다.

A는 `0060c4dd`에서 구현됐고 리뷰 중이다. r3도 B를 **두 리뷰 단위**로 나눴다. 당시 합계 추정은 prod 22~26파일, 순증 +715~980이었다(상한 30파일/+1500, 분할 기준 +1200).
당시 합계는 분할 기준 아래였지만 clear 수리가 restart와 독립이고 모든 herdr launch에 영향을 주므로 따로 리뷰하기로 했다. 순서는 B-clear → B-main이었다.
조각마다 첫 production WIP 커밋 직후 `pr_cap_prod.py`로 다시 잰다. 함수·시험을 옮겨 수치를 맞추지 않는다.

| 조각 | 범위 | 순증 추정 | 수용 경계 |
|---|---|---|---|
| A: Codex resume launch/source 경계 | `src/services/codex/herdr_turn.rs`, 새 `herdr_turn/restart_resume.rs`, `first_input.rs`, `tui_session_launch/launch_policy.rs`, 필요 시 기존 launch options/source child (4~6파일) | +350~500 | 기존 fresh 검사 유지, resume native/argv/source EOF/nonce 검증 |
| B-clear: launch 중 herdr 턴에 대한 clear 우선 | 아래 표 (7~9파일) | +190~290 | 3.9의 6 |
| B-main: restart 명령·seed·permit | 아래 표 (15~17파일) | +525~690 | 3.9의 1~5, 7~13 |

| B-clear 파일 | 넣는 것 | 순증 |
|---|---|---|
| 새 `src/services/herdr_launch/clear_epoch.rs` | 채널 epoch, `Launching` RAII, launch 종료 대기 | +80~110 |
| `src/services/herdr_launch.rs` (781줄) | Pending CAS 뒤 epoch 확인과 기존 unsent retire 호출 | +12~18 |
| `src/services/claude/herdr_turn.rs`(905줄), `src/services/claude/herdr_turn/handoff.rs`, `src/services/codex/herdr_turn.rs`(852줄) | `Launching` 등록, 첫 입력 직전 확인 | +25~40 |
| `src/services/discord/commands/control.rs` **(940줄, giant 경계 근처)** | clear 시작의 epoch 올림 한 줄과 전달 | +4~6 |
| `src/services/discord/commands/control/clear_teardown.rs` | launch 종료 대기, row 재읽기 처리, Retired selector CAS | +45~65 |
| `src/services/discord/commands/control/herdr_recovery.rs` | launch 중 턴을 일반 clear 경로로 | +8~12 |
| owner의 clear 폐기(`src/services/turn_orchestrator/queued_head/v2/control.rs`) | claim된 미제출 front의 terminal 결산. 기존 동작이면 변경 0 | +0~25 |

| B-main 파일 | 넣는 것 | 순증 |
|---|---|---|
| `src/services/discord/commands/restart.rs` | herdr 분기 한 갈래, 응답 문구 연결 | +20~30 |
| 새 `src/services/discord/commands/restart/herdr.rs` | row 분기표, epoch 읽기, 묶인 종료 호출과 1회 재시도, postlude, ticket 상태별 응답, 철회 | +190~240 |
| 새 `src/services/discord/herdr_terminate/restart.rs` (+부모 `mod` 한 줄) | owner·dispatch에 묶고 요청과 분리해 도는 종료 진입점. `herdr_terminate.rs`(949줄) 본문은 건드리지 않는다 | +50~70 |
| 새 `src/services/discord/queue_io/input_owner/restart_seed.rs` (+`mod.rs` 한 줄) | operation→ticket·원본·context, 적재 시도, ticket 상태, 철회, in-flight gate, permit(거절·대기·남은 disconnect) | +150~200 |
| `src/services/discord/queue_io/input_owner/origin.rs`, `start.rs` | `RestartSeed` origin, 일반 ingress의 source 거절 | +15~20 |
| 새 `src/services/turn_orchestrator/queued_head/v2/source_admission/restart_seed.rs`, `v2.rs` | 단위 뒤 삽입과 bind index 이동, ticket 상태 조회, 사람 선행 이동 가드 | +50~70 |
| `src/services/turn_orchestrator/queued_head/v2/source_admission.rs` **(giant 1,028줄)** | 삽입 위치 판별 연결 2~4줄만 | +4 |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs` **(giant 1,567줄)** | gate 대기 분기, 기존 대기 예외에 origin 한 갈래 | +8~12 |
| `src/services/discord/router/message_handler/headless_turn.rs` **(giant 1,545줄)** | `:159`의 판정 선택 분기만. 검증은 child에 둔다 | +6~10 |
| `src/services/turn_host.rs` | seed용 진입 판정(intake 판정 재사용) | +10~15 |
| A의 포트를 부르는 연결(`provider_dispatch.rs`의 Codex 턴 조립 등 1~2파일) | seed origin → typed resume 요청 | +10~20 |

giant 세 파일에는 선언·typed 연결·거절 분기만 넣는다. `herdr_terminate.rs`와 `herdr_gate.rs`의 본문은 건드리지 않는다. `control.rs`는 +6을 넘기지 않는다.
시험 추가 시 공식 inventory/선택 경로 동기화로 required PR job 선택을 확인한다. baseline·allowlist·감사 임계값은 이 설계의 완화 대상이 아니다.

## 5. 검증 계획과 운영 관측

설계 모델의 GREEN은 생산 실행의 GREEN이 아니다. 이번 레그에서는 아래 Rust/PG/E2E를 실행하지 않았다.

| 검증 | 실제 진입점·RED → GREEN | 원인 제거 뮤턴트 / 필수 단언 |
|---|---|---|
| 명령 end-to-end fixture | 기존 `commands/restart.rs:374`/`:423` host-guard PG seam과 `restart_channel`, slash/text auth 뒤 공통 실행. seed가 owner drive를 거쳐 `launch_herdr_session`에 닿는 데까지 | herdr branch 제거 → 정상 herdr 재시작 거절 RED. auth 거절은 close/create/seed 0이며 실제 명령 결과 포함 |
| seed 시작 경로 | owner front → headless 진입 → `herdr::execute`. B 첫 커밋의 확인 시험(3.1 8단계) | permit 분기 제거 → `executor_not_wired` RED. 일반 headless 시작과 source를 흉내 낸 ingress는 계속 거절(대조군). `:159`~`:926` 사이 tmux 이름 기반 효과 0 단언 |
| row 상태표 | 3.2의 여섯 행마다 PG fixture로 실제 명령 실행 | Retired·Missing 행에서 terminate를 부르는 변형 → 거절 RED. 거절 행은 close·seed·row 변화 0 |
| dispatch | `active_dispatch_id`가 있는 row에서 dispatch 턴 진행 중 실제 명령. 같은 fixture를 tmux 채널에도 돌려 기준선(dispatch status·재시도 수·게시 수) 기록 | 묶지 않은 종료 → successor row에 옛 dispatch가 남아 RED. `fail_or_retry` 호출 변형 → 재시도 dispatch 1 RED. fence 전 dispatch 교체는 1회 재시도, retire 뒤 crash는 permit이 마무리 |
| seed를 넣을 수 없을 때 | 큐 30개·31개, V2 미채택, commit 미확인 주입, effort 미결 fixture에서 실제 명령. tmux와 입력 순서·close 수·응답 문구 비교 | 종료 전 거절 변형 → close 0으로 RED. 대기 입력 유실 0, backlog 2개에서 herdr 첫 입력이 인사말(append 변형 RED) |
| 전송 중 단위 | Reserved·MayHaveSent·Unknown head와 claim된 front 각각에서 실제 명령 | 무조건 prepend 변형 → head 결산이 `queue first item differs`로 실패해 RED. 정상은 단위 결산 성공 뒤 seed가 front |
| 종료의 정확성 | `terminate_explicit_herdr` + witnessed fake transport/PG row + actual command branch | exact fence 또는 identity 재검사 제거 → old/server/pane 교체에서 foreign close/old input RED. close ACK만 성공 처리 mutant → provider 잔존 RED |
| 입력 custody·동시성 | restart의 적재/fence/close/retire/postlude/release 각각 barrier에서 실제 Discord intake·owner drive 실행 | purge 연결, gate 제거, permit 제거, release wake 제거 → 원본 ID/본문/upload 손상·중복, 옛 pane 인사말, 이른 거절 또는 영구 대기 RED. 두 입력의 실제 count=각1 |
| uncertain 경계 | close/create/seed effect 직후 reply 유실·phase commit 전 crash fixture | blind create/seed retry → 물리 create 또는 seed 효과 2 RED. uncertain은 Pending/retained+보이는 실패이며 성공 count에서 제외 |
| Codex resume | A의 실제 `prepare_codex_herdr_launch`/first-input/attach 경로, 기존 transcript가 있는 fake-provider round trip | fresh validator 삭제만 한 변형, offset=0, foreign SessionStart 허용 → native mismatch/과거 출력 재배달 RED. fresh control도 계속 GREEN |
| Claude resume | command→준비→SessionStart/attach, account/transcript/inode/cursor fixture | latest binding 검사 제거 → stale selector RED. migration 종료 호출 mutant → 관계없는 tmux close 1 RED(정상 0) |
| clear 우선(B-clear) | 일반 메시지와 seed 각각의 launch를 Pending 전·create 중·제출 직전 barrier에 세우고 실제 `!clear` | launch의 epoch 확인 제거 → clear 뒤 pane 생성 RED. 제출 직전 확인 제거 → clear 뒤 입력 제출 RED. clear의 launch 대기 제거·Retired CAS 제거 → selector 잔존 RED. 끝난 뒤 pane 0, selector 컬럼 NULL, 입력 재시작 0 |
| clear와 restart | restart를 fence 뒤 barrier에 세우고 실제 `!clear`를 보내 3초를 넘김. 3.4의 행마다 교차 | postlude의 epoch 대조 제거 → selector가 남아 다음 입력이 clear 전 대화를 resume RED. gate 해제 직후 seed를 정상 결산 또는 시작 실패시킨 뒤 응답을 재개: `items.contains` 판정 변형 → 거짓 superseded RED |
| seed 제출 경계 | 물리 제출 뒤·receipt 전, receipt 뒤·bind 전에 crash를 주입해 실제 부팅 복원 | 불명 seed를 queued로 되돌리는 변형 → 인사말 2회 RED. admission에서 Ready를 말하는 변형 RED |
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
| herdr server·정책 | `src/services/herdr_launch.rs:385` LegacyDedicated restore-off fresh witness, UserDefault grant `:456`; `src/services/session_host/herdr_gate.rs:496` 같은 서버. 버전 문자열이 아닌 기존 프로토콜/권한 판정을 사용하며 런타임 설정값은 이 레그에서 확인하지 않음 |
| pane identity | `src/services/discord/herdr_terminate.rs:453`, `:548`, `src/services/session_host/herdr_gate.rs:455` old nonce/owner/location + witnessed exact close. 같은 숫자 pane 재사용은 새 권한 아님 |
| launch 인자 | Claude `handoff.rs:17/29/85`, Codex `launch_policy.rs:178/210/256`, `herdr_launch.rs:732`. 기존 설정·계정·source 검증을 통과시켜야 하며 resume-only 가드 완화 금지 |
| 경계 시각 | request 도착·seed commit(수락)·fence·close effect·retire·해제/wake·owner의 Pending commit·seed submit·source attach를 분리. 이전 uncertain 입력은 replay하지 않음. 직접 native draft는 접수 원본 보장 밖임을 명시 |
| 동시성 경합 | exclusion은 old fence→retire→old disconnect까지. clear는 timeout 후 진행하므로 postlude와 응답에서 clear epoch을 대조한다(3.4). 해제 뒤 launch까지는 owner 단일 actor·FIFO·Pending CAS가 막는다(3.0). 두 번째 요청은 큐의 seed를 자기 seed로 쓰고, Cancel/Terminate/actor 교체는 nonce·token 불일치로 보이는 실패 |
| 옛 형식 복원 | 새 V2 필드·variant 0이라 스냅샷 형식이 바뀌지 않는다. seed는 큐 항목이므로 기존 census가 찾는다(`compat.rs:33`). legacy inflight 호환 유지, unknown/newer 오인 복원 금지 |
| 문서로만 선언한 가드 | seed의 단위 뒤 적재·ticket 상태·철회, in-flight gate, seed permit(거절·대기·남은 disconnect), ingress의 source 거절, 묶인 종료 진입점, clear epoch과 launch의 두 확인, clear의 launch 대기와 Retired CAS는 **새 구현·시험이 필요한 포트**다. 기존 transition/fence가 이것까지 제공한다고 주장하지 않음 |
| seed의 headless 진입 | herdr에서 headless 시작은 지금 전부 거절이라 `headless_turn.rs:159` 뒤 구간이 herdr 채널로 생산에서 돈 적이 없다. intake와 공유하는 reconcile(`intake_turn.rs:1494`, `headless_turn.rs:808`)은 확인했으나 구간 전체는 B 첫 커밋의 실제 진입 시험으로 확인해야 한다. #8125와 같은 문을 좁게 여는 것이므로 그 일감과 순서를 맞춘다 |
| 잘못된 0/PASS | 모델/current refusal의 안전 GREEN≠parity; owner Accepted/launch Launched≠Ready; PG skip/시험0≠PASS. 실제 물리 효과·선택 수·원본 receipt와 결과 메시지로 판정 |
| 기존 결함 넓히지 않음 | `src/services/discord/commands/control/provider_reset.rs:58`의 tmux recreate bool을 소비하지 않고 `:70` Applied로 끝나는 기존 가능성, clear teardown unknown fallback은 그대로 두고 herdr 성공 판단에 복제하지 않음. launch 중인 턴에 대한 clear의 보류(`control/herdr_recovery.rs:158`)는 r3의 B-clear가 고친다(main 동작은 모델의 `MainClear`). create 응답이 불명인 Pending의 종료 미확인(`clear_teardown.rs:202`)은 기존 계약으로 남는다. 새 이슈/PR은 이 레그에서 쓰지 않음 |

## 6. 상태 차트 산출물

`HerdrRestart`의 a 영역은 A의 Codex resume 경계다. s 영역은 r1 direct-launch 제안의 역사적 음성 대조이며 B의 정본으로 재사용하지 않는다.
채택안은 같은 영역의 모듈 `HerdrRestartOwner`로 검사하며 r3에서 사용자 결정대로 갱신했다. 값 범위·수정 전/후 표·규칙·TLC 결과·Mermaid·재현 명령은
[`session-lifecycle/herdr-restart-owner.report.md`](statecharts/session-lifecycle/herdr-restart-owner.report.md)에 있다.
r1 보고서는 [`session-lifecycle/herdr-restart.report.md`](statecharts/session-lifecycle/herdr-restart.report.md)다.

- 63 cfg: GREEN 28, 기대 RED 35(main의 거절 1, main의 clear 동작 1, 원인 변형 30, 남는 계약 3). 불변식 20개와 진행성 8개 모두 그것을 깨는 cfg가 있다.
- dispatch 실행 중 재시작, clear 우선(직렬·overtake·launch 중·임의 시점), seed 없는 재시작, 전송 중 단위, seed 제출의 세 단계와 각 사이의 crash, 롤백, 시작 row 여섯 가지를 포함한다.
- 모델은 새 코드가 아닌 설계 포트를 검사한다. seed의 headless 진입, provider별 I/O, 실제 부팅 복원, tmux의 dispatch 기준선은 5절의 시험으로 증명해야 한다.

`docs/statecharts/expected.tsv`에 r1 23행과 r3 63행을 등록했다(행 추가는 코디네이터 사전 허용). r1 23행은 A 레인도 같은 내용을 추가하므로
병합 때 같은 자리에서 겹친다. A 쪽 행을 남기고 `HerdrRestartOwner` 63행을 그 뒤에 두면 된다.

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

## 변경 이력

- r1 (2026-11-13, `2c88733814`): 진입점 전수표, restart가 직접 launch하는 방향, `HerdrRestart` 모델 23 cfg.
- r2 (2026-11-13, `f3c745ebc9`): 리뷰 d1의 방향 제안을 판정해 owner 큐 seed 방향을 채택(적재 → 종료 순서). P2-1·P2-3·P2-4·P2-6 반영, P2-2·P2-5는 A 접점만 기록, 사용자 결정 항목 U1~U4, B 크기 재추정, `HerdrRestartOwner` 모델 42 cfg.
- B2b 판정 (2026-11-14): 소유 증거 없는 successor 합류 금지(3.1·3.5), unplaced Pending 거절 안내 문구, Retired row는 종료 없이 seed만 적재.
- r3 (2026-11-13): 사용자 결정 4건 반영(dispatch 중 재시작, clear 우선, 인사말 순서는 불가피 차이, seed 불가 시에도 재시작)과 리뷰 d2의 P2 5건(전송 중 단위 뒤 적재, ticket 상태와 clear epoch, seed 제출 세 단계, 옛 pane 전달 기준, tmux 차이 전수). B를 B-clear·B-main으로 분할, 모델 63 cfg.

<!-- doc-path-refs: proposed herdr_turn/restart_resume.rs src/services/discord/herdr_terminate/restart.rs -->
