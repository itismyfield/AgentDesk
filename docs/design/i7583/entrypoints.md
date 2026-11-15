# #7583 현재 main 진입점과 모델 도메인

기준은 `1974cff8a67bf5d1db041abfdd4d615c518ee104`이다. 아래 좌표는 이 SHA의 좌표다.
`*.tsv`는 `git ls-files src scripts .github`에 대한 정규식 전수 검색 원본이다.
`nearest_function_lexical`은 줄 앞의 마지막 함수 선언이며 Rust AST의 소유 함수 판정이 아니다.
`production-or-inline-test`는 inline 시험을 포함한다. 후보 수를 실제 우회 호출 수나 시험 실행 수로 보고하지 않는다.
정의·import·시험·호스트 내부 구현도 보존해, 구현자가 삭제 후 검색으로 빠진 호출을 찾을 수 있게 한다.

| 원본 | 범위 | 현재 후보 행/파일 |
|---|---|---|
| [process-uses.tsv](process-uses.tsv) | ProcessBackend/Host, registry API/handle/config, wrapper pipe 입력 | 603 / 107 |
| [state-entrypoints.tsv](state-entrypoints.tsv) | HostKind, RuntimeHandoffKind, 두 turn switch와 시험 setter, host/runtime marker | 2079 / 498 |
| [host-call-sites.tsv](host-call-sites.tsv) | 생성·재생성·입력·관찰·중단·종료·복구의 구체 API 및 tmux 명령 문자열 | 2932 / 655 |
| [windows-gates.tsv](windows-gates.tsv) | 호스트·provider·복구·설정의 cfg와 Unix OS/주소/셸 가정 | 1446 / 227 |

audited-bypasses.tsv는 **441행/161파일**이며 보수적 BYPASS 후보 318, adapter primitive 48, 기존 trait 경유 16, 시험/helper 21, 비관리 30, lifecycle entry 6, domain intent/classifier 각 1이다. 후보 수는 실제 우회 확정 호출 수가 아니다. 실제 syscall/transport leaf와 기존 wrapper를 포함해 구현자가 한 행씩 call chain을 닫는 기준표다. consumer-slices.tsv는 S01~S06 소유를 제외한 101파일을 17/17/17/17/17/16으로 나눈다.

추가 검색은 ProcessReady/execute_streaming_local_process/InputModeArg::Pipe와 herdr_turn_switched_on의 간접 reader까지 포함했다. 파일·행·원본 source를 함께 저장했으므로 정의/import/test를 줄인 결과만 보고 누락을 추정하지 않는다.

## 모델 값의 유일한 출처

`docs/statecharts/session-host/SessionHost.tla`는 이 표의 유한 추상값만 사용한다.
식별자 1·2는 두 번의 같은 채널 실행을 구별하는 대칭 추상값이다. 채널 수 1, 재시작 수 1,
실행 수 2, 일시 관찰 실패 수 1은 TLC의 탐색 경계이며 운영 상한을 뜻하지 않는다.

| 모델 변수/상수 | 값 | 현재 코드/진입점 | 추상화 |
|---|---|---|---|
| Host | tmux, process, herdr | `src/domain/host_observation.rs:4`; parser `src/domain/host_observation.rs:23` | Core는 아직 main 값이 아니므로 모델에 추가하지 않음 |
| Mode | current, proposed, replay, earlyRetire, noWake, forget | `src/services/session_host/traits.rs:9`; 변경할 포트의 비교/가드 제거 | 제품 상태가 아닌 검사 시나리오 |
| row | Legacy, Pending, Bound, Retired | `src/db/dispatched_sessions/hosted_execution.rs:27`; Legacy는 `src/db/dispatched_sessions/hosted_execution.rs:171` 부근 HostedRecord | legacy/missing와 serde state 3개 |
| live, locator, serial | {}, {1}, {2}, {1,2}; 0,1,2 | `src/services/herdr_launch.rs:482` Pending, `src/services/herdr_launch.rs:515` create; `src/db/dispatched_sessions/hosted_execution.rs:47` location, `src/db/dispatched_sessions/hosted_execution.rs:68` expected | pane/root/provider/session nonce 묶음의 동일성; 실제 PID 번호를 모델 값으로 쓰지 않음 |
| srv | up, down, boot | `src/cli/dcserver.rs:771`; `src/services/discord/runtime_bootstrap.rs:480`; `src/services/discord/runtime_bootstrap/recovery_flush.rs:36` | 프로세스 실행/종료/재조정 gate |
| flight | idle, creating, created, closing | `src/services/herdr_launch.rs:474`; `src/services/termination_audit/host_terminate.rs:52`; `src/services/discord/herdr_terminate.rs` | 기존 모델 HerdrSession의 launch/close 중간 효과 |
| adopted, registry, recoverOwed | BOOLEAN | `src/services/session_backend.rs:341`; `src/services/discord/recovery_engine/herdr_reader.rs:235`; `src/services/discord/tui_prompt_relay/herdr_source.rs:274` | 메모리 재구축과 동일 실행 재채택 |
| stop, terminated | BOOLEAN | `src/services/discord/commands/control/clear_teardown.rs:99`; `src/services/queue/herdr_stop.rs:13`; `src/services/discord/herdr_terminate.rs` | cancel은 세션 종료가 아님; 여기 stop은 명시적 종료/clear의 끝내기 요청 |
| unknowns | 0,1 | `src/services/session_host/model.rs:52`; `src/services/discord/recovery_engine/host_reconcile.rs:81` | ProbeError/Unknown이 한 번 나타난 뒤 정상화; 영구 실패는 별도 liveness 한계 |

모델의 discoverable witness는 현재 Herdr의 nonce 작업공간 census/원본 실행 확인 계약
(`src/services/herdr_launch/unplaced.rs`, `src/services/session_host/herdr/launch_host.rs:633`)을 추상화한다.
Windows에서 그 계약이 성립하는지는 설계의 U03/U04/U05/U08 실측 전까지 가정이다.
