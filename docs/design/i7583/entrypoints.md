# #7583 현재 main 진입점과 모델 도메인

기준은 `1974cff8a67bf5d1db041abfdd4d615c518ee104`이다. 아래 좌표는 이 SHA의 좌표다.
`*.tsv`는 설계 1(WSL 레인)이 `git ls-files src scripts .github`에 대해 정규식으로 전수 검색한 원본이다. 합류 레그는 바이트 그대로 옮겼고,
개정 2(r2)에서 audited-bypasses.tsv의 두 행만 판정을 고쳤다(아래). 나머지 파일·행은 원본 그대로다.
`nearest_function_lexical`은 줄 앞의 마지막 함수 선언이며 Rust AST의 소유 함수 판정이 아니다.
`production-or-inline-test`는 inline 시험을 포함한다. 후보 수를 실제 우회 호출 수나 시험 실행 수로 보고하지 않는다.

| 원본 | 범위 | 후보 행/파일 |
|---|---|---|
| [process-uses.tsv](process-uses.tsv) | ProcessBackend/Host, registry API/handle/config, wrapper pipe 입력 | 603 / 107 |
| [state-entrypoints.tsv](state-entrypoints.tsv) | HostKind, RuntimeHandoffKind, 두 turn switch와 시험 setter, host/runtime marker | 2079 / 498 |
| [host-call-sites.tsv](host-call-sites.tsv) | 생성·재생성·입력·관찰·중단·종료·복구의 구체 API와 tmux 명령 문자열 | 2932 / 655 |
| [windows-gates.tsv](windows-gates.tsv) | 호스트·provider·복구·설정의 cfg와 Unix OS/주소/셸 가정 | 1446 / 227 |
| [audited-bypasses.tsv](audited-bypasses.tsv) | 위 후보에서 주석·선언·명명 시험을 거른 효과 지점 | 441 / 161 |
| [consumer-slices.tsv](consumer-slices.tsv) | 설계 1 S01~S06 소유를 뺀 잔여 소비자 101파일의 6묶음 | 101 파일 |

audited-bypasses.tsv의 판정 분포(r2 재분류 뒤 `awk`로 다시 셈): BYPASS 320행(관찰 153·입력 89·종료 43·생성 16·중단 12·복구 7),
ADAPTER_PRIMITIVE 48, THROUGH_TRAIT 16, TEST_OR_HELPER 21, NON_MANAGED 28, LIFECYCLE_ENTRY 6, DOMAIN_INTENT 1, DOMAIN_CLASSIFIER 1.
BYPASS 행이 걸친 파일은 126개다(관찰 89·입력 27·종료 27·생성 12·중단 11·복구 4, 겹침 포함).

r2 재분류: `src/cli/utils.rs:168`(관찰)·`:186`(종료)을 NON_MANAGED → BYPASS. 함수 `kill_agentdesk_tmux_sessions_local`(`src/cli/utils.rs:166-194`)은
`AgentDesk-*` 이름의 관리 세션을 나열해 종료하는 reset-tmux 경로다(독립 리뷰 P1-4). `:186`은 S01, `:168`은 S02가 옮긴다.
남은 src NON_MANAGED 종료 호출은 `src/cli/dcserver.rs:831`, `src/server/routes/provider_auth_profiles.rs:341`, `:490` 세 곳이다(자기 fallback·로그인·인증 프로필 세션).

## 합류 레그 보충 좌표 (전수표 밖, main에서 직접 확인)

설계 1 전수표의 검색 범위 밖이라 빠졌던 진입점이다. 모델의 값 범위는 바꾸지 않고, 행동의 근거로만 쓴다.

| 좌표 | 내용 | 쓰는 곳 |
|---|---|---|
| `src/services/platform/windows_job.rs:35`, `:52` | dcserver가 자기 자신을 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` job에 넣는다 | 모델 `Crash`의 Windows 자식 종료, 조각 W04 |
| `src/cli/dcserver.rs:882` | Windows 부팅에서 위 job을 만든다 | 같음 |
| `src/services/discord/recovery_engine/restore_inflight.rs:226` | 디스크의 모르는 `runtime_kind` 행을 조용히 건너뛴다 | 조각 D03의 옛 `process_backend` 행 처리 |
| `src/services/discord/inflight/model.rs:1367`, `:1390` | 종류·tmux 이름 없는 행의 복구 기본값이 `ProcessBackend`다 | 조각 D03 0단계 |
| `src/services/cluster/readiness.rs:339` | 모든 노드가 `process` backend를 광고한다 | 조각 D04, [U] U14 |
| `src/config/session_hosts.rs:350` | 채널 → Herdr endpoint 조회 | 채널 선택 표 |
| `src/services/platform/tmux/availability.rs:13`, `:14` | tmux 가용성 캐시 45초·연속 실패 3회 | 조각 D01 |
| `src/services/herdr_launch.rs:533-569` | 위치 CAS 뒤 확인 못 한 실행 명령은 위치를 남긴 Pending + Indeterminate | 모델 `RecordLocation`, 조각 S06b |
| `src/services/discord/recovery_engine/herdr_reader.rs:463-464`, `src/services/herdr_launch/unplaced.rs:353` | 복구 패스는 Pending을 건너뛰고, unplaced resolver는 위치 없는 행만 | 모델 `PlacedFix`·`SkipPlaced` |
| `src/services/discord/recovery_engine/herdr_server_replaced.rs:26-31`, `:238-262` | server-replaced의 placed Pending 처리는 user_default·재부팅 뒤만 | 조각 W08 |
| `src/services/provider/cancel_token_claude_interrupt.rs:535-541`, `:544-563`; `src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs:257-262` | settlement `cfg!(unix)`, Codex Escape는 Cancel bool | 모델 `CancelSwitch`·`CancelOk` |
| `src/services/discord/router/message_handler/intake_turn/host_refusal.rs:6-63`, `src/services/discord/router/intake_dispatch/queued.rs:353-357`, `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:284-286` | admission 거절의 원문 복원과, owner 없는 턴의 provider `Err`가 복원 경계를 지나지 않음 | 모델 `Unavailable`·`kept`/`lost`·`ErrConsumes` |

## 모델 값의 유일한 출처

`docs/statecharts/session-host/SessionHost.tla`는 이 표의 유한 추상값만 쓴다. 설계 1 모델(Pending·생성·위치·재채택·종료)을 기반으로
Fable 모델의 플랫폼별 호스트 선택·tmux 부재·Windows job·호스트 서버 소실·입력 거절을 옮겨 넣었다.
식별자 1·2·3은 같은 채널 실행을 구별하는 대칭 추상값이다. 채널 1, dcserver 재시작 1, 호스트 서버 소실 1, 입력 2, 종료 요청 1,
일시 관찰 실패 1은 TLC의 탐색 경계이며 운영 상한을 뜻하지 않는다.

| 모델 변수/상수 | 값 | 전수표 좌표 | 추상화 |
|---|---|---|---|
| `Platform` | unix, windows | windows-gates/state-entrypoints `src/services/turn_host.rs:228`, `src/services/turn_host.rs:243` | `cfg!(unix)` 게이트 |
| `Design` | current, proposed | 검사 시나리오 | main과 최종 설계 |
| `Tmux` | BOOLEAN | `src/services/platform/tmux/availability.rs:6`, `src/services/claude.rs:1650` | tmux 사용 가능 |
| `Endpoint` | BOOLEAN | `src/config/session_hosts.rs:109`, `src/config/session_hosts.rs:230`, `src/config/session_hosts.rs:350` | 채널에 Herdr endpoint(또는 명시 선택)가 있음 |
| `Switch` | BOOLEAN | `src/config/runtime_settings.rs:87-94`, `src/services/turn_host.rs:220-247` | 실행 bool(Claude/Codex를 하나로 추상화) |
| `CancelSwitch` | BOOLEAN | `src/config/runtime_settings.rs:95-97`, `src/services/provider/cancel_token_claude_interrupt.rs:544-563` | Codex Escape bool |
| `AllowStop`, `MaxInputs` | BOOLEAN, 2 | 검사 시나리오 | 종료 요청 허용, 입력 수 경계 |
| `Autostart` | BOOLEAN | `src/services/session_host/herdr/autostart/spawn.rs:48` | dcserver 수명 밖에서 Herdr 서버를 다시 띄울 수 있음 |
| `Mutant` | none, replay, earlyRetire, noWake, forget, serverInJob, silentFallback, skipPlaced, gateRun, gateCancel, errConsumes | `src/services/session_host/traits.rs:9`; 바꿀 포트의 가드 제거 | 제품 상태가 아닌 검사 시나리오 |
| `host` | none, tmux, process, herdr | `src/domain/host_observation.rs:4` | Core는 채널 세션 호스트가 아니므로 모델에 넣지 않음 |
| `hostSrv` | up, down | `src/services/discord/recovery_engine/herdr_server_replaced.rs:246`, `src/services/discord/recovery_engine/herdr_server_replaced.rs:261` | tmux/Herdr 서버 프로세스 |
| `row` | Legacy, Pending, Bound, Retired | `src/db/dispatched_sessions/hosted_execution.rs:27`, `src/db/dispatched_sessions/hosted_execution.rs:171` | legacy/없음과 serde 상태 3개 |
| `prov` | BOOLEAN | `src/services/herdr_launch.rs:553-569`, `src/services/discord/router/message_handler/provider_dispatch_herdr_tests.rs:766-781` | pane 안에 provider가 떴는가(셸만 남은 placed Pending 구별) |
| `live`, `record`, `locator` | {1,2,3}의 부분집합; 0..3 | `src/services/herdr_launch.rs:482`, `src/services/herdr_launch.rs:515`, `src/db/dispatched_sessions/hosted_execution.rs:47`, `src/db/dispatched_sessions/hosted_execution.rs:68` | pane/root/provider/nonce 묶음의 동일성 |
| `serial` | 0..1 | 위와 같음 | replay 뮤턴트가 한 번만 재생성하게 세는 값 |
| `flight` | idle, creating, created, closing | `src/services/herdr_launch.rs:474`, `src/services/termination_audit/host_terminate.rs:52` | 생성·닫기 중간 효과 |
| `registry`, `adopted`, `recoverOwed` | BOOLEAN | `src/services/session_backend.rs:341`, `src/services/discord/recovery_engine/herdr_reader.rs:235`, `src/services/discord/tui_prompt_relay/herdr_source.rs:274` | 메모리 재구축과 같은 실행 재채택 |
| `stop`, `terms` | BOOLEAN, 0..1 | `src/services/discord/commands/control/clear_teardown.rs:99`, `src/services/queue/herdr_stop.rs:13` | 종료·clear의 끝내기 요청. Cancel은 세션 종료가 아님 |
| `killed` | BOOLEAN | `src/services/session_backend.rs:341` (+보충 `windows_job.rs:35`) | dcserver 재시작만으로 provider가 끝남 |
| `want`, `answer`, `inputs` | BOOLEAN; none, started, refused, unavailable, failed; 0..`MaxInputs` | `src/services/discord/router/message_handler/provider_dispatch.rs:559`, `src/services/claude.rs:794`, `src/services/claude.rs:810`, `src/services/codex/command_streaming.rs:184`, `src/services/qwen.rs:357` | 입력이 시작되거나 거절됨 |
| `kept`, `lost` | BOOLEAN | `src/services/discord/router/message_handler/intake_turn/host_refusal.rs:6-63`, `src/services/discord/router/intake_dispatch/queued.rs:353-357` | 호스트 없음 거절 뒤 원문이 큐에 남음 / 소비됨 |
| `cancel` | none, requested, delivered, dropped | `src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs:257-262`, `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:478-490` | 턴 Escape 요청과 전달 결과 |
| `srv`, `crashes` | up, down, boot; 0..1 | `src/cli/dcserver.rs:771`, `src/services/discord/runtime_bootstrap.rs:35`, `src/services/discord/runtime_bootstrap.rs:472` | 실행·종료·재조정 gate |
| `losses`, `unknowns` | 0..1 | `src/services/discord/recovery_engine/herdr_server_replaced.rs:261`, `src/services/session_host/model.rs:52`, `src/services/discord/recovery_engine/host_reconcile.rs:81` | 호스트 서버 소실 1회, ProbeError/Unknown 1회 뒤 정상화 |

모델의 discoverable witness는 Herdr nonce 작업공간 census·원본 실행 확인 계약
(`src/services/herdr_launch/unplaced.rs`, `src/services/session_host/herdr/launch_host.rs:633`)과 tmux 세션 이름 조회를 추상화한다.
Windows에서 그 계약이 성립하는지는 설계 [U] U03·U04·U05·U11 실측 전까지 가정이다.
