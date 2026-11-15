# SessionHost (#7583 최종 설계)

AgentDesk `origin/main` `1974cff8a67bf5d1db041abfdd4d615c518ee104` 기준. 한 Discord 채널의 관리 provider 세션이
플랫폼·채널 선택에 따라 고른 호스트(tmux, Herdr, process) 위에서 dcserver 재시작·호스트 서버 소실·종료 요청을 지나는 모델이다.
`Design = "proposed"`는 최종 설계(ProcessBackend 삭제, Windows 관리 Claude/Codex는 Herdr, 호스트가 없으면 provider 호출 전 intake admission에서 거절하고 원문을 남김)다.

설계 1 모델(`refs/review/i7583-design-1`, 10 cfg)의 Pending CAS·생성 효과·위치 기록·재채택·닫기 분리를 기반으로 하고,
Fable 모델(`refs/review/i7583-design-fable-1`, 14 cfg)에만 있던 플랫폼별 호스트 선택·tmux 부재·Windows kill-on-close job·
호스트 서버 소실과 재기동·입력 거절과 그 뮤턴트(`serverInJob`, `silentFallback`)를 옮겨 넣었다.
Fable의 `BootOpensEarly`(재조정 없이 intake가 열림)는 설계 1의 `NoWake`(재시작 패스에 닿지 못함)와 같은 결함이라 `NoWake` 하나로 합쳤다.
Fable의 `RetireBeforeClose`는 설계 1의 `EarlyRetire`와 같다. 변수 값 범위는 [진입점 표](../../design/i7583/entrypoints.md)에서만 가져왔다.

개정 2(r2)는 독립 리뷰를 반영해 다음을 더했다: 위치가 기록된 Pending(`RecordLocation` 뒤 행이 Pending으로 남음, provider는 뜰 수도 안 뜰 수도 있음)과
그 자동 탈출(`AdoptPlaced`·`ResolvePlacedGone`·`ResidueClose`), endpoint와 스위치의 분리(`Endpoint`·`Switch`), Codex Escape 스위치(`CancelSwitch`)와 Cancel 전달,
호스트 없음 거절이 원문을 남기는지(`kept`·`lost`, `Unavailable`·`Requeue`). 뮤턴트 4개(`skipPlaced`, `gateRun`, `gateCancel`, `errConsumes`)와 cfg 6개를 더했다.

TLC 1.7.4(`statechart` 스킬), 23 cfg: 규칙이 모두 성립 9개, 아래에 적은 규칙만 깨짐 14개(모두 예상). `expected.tsv`가 같은 행을 가진다.
Rust는 빌드·실행하지 않았다. 반례는 코드를 읽어 대조했다.

**보장 범위.** 이 모델이 보이는 것은 (1) 고른 호스트 위에서 재시작·Pending·입력·종료 계약이 성립하는가, (2) Windows PlatformDefault 실행이 bool 스위치 때문에
거절되거나 Escape가 버려지지 않는가다. 채널 선택 표(설계 §5) 전체가 main과 같다는 것은 보이지 않는다 — 그것은 조각 W01의 표 기반 시험이 정본이다(설계 §5.2).
r1 보고서의 "Unix 쌍 상태 수가 같으므로 기본값이 같다"는 문장은 철회한다(상태 수 동일은 선택 결과 동일을 뜻하지 않는다).

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| `NoSessionLoss` | INVARIANT | dcserver 재시작만으로 provider가 끝나지 않고, 살아 있는 실행은 내구 행(레지스트리·위치·호스트 census)으로 닿을 수 있다 |
| `NoDoubleSession` | INVARIANT | 한 채널에 살아 있는 실행은 하나 이하 |
| `NoOrphanAfterTerminate` | INVARIANT | Retired 행에는 살아 있는 실행이 없다(종료 뒤 고아 0) |
| `NoProcessHost` | INVARIANT | 최종 설계는 process 호스트를 띄우지 않는다 |
| `NoInputLoss` | INVARIANT | 호스트 없음 거절이 원문을 소비하지 않는다 |
| `NoDefaultRefusal` | INVARIANT | 정책 거절은 endpoint가 설정된 채널에서만 난다(PlatformDefault는 bool로 거절되지 않음) |
| `NoDefaultCancelDrop` | INVARIANT | Escape가 버려지는 것은 endpoint 채널의 스위치 off뿐이다 |
| `RestartEventuallyReconnect` | PROPERTY | 재시작 뒤 같은 실행을 다시 잡거나, 정말 사라진 실행의 행을 닫는다 |
| `InputAnswered` | PROPERTY | 모든 입력은 시작되거나 거절·보존된다 |
| `TerminateEventuallyGone` | PROPERTY | 종료 요청은 소유 실행의 소멸 확인으로 끝난다 |
| `PendingEventuallyResolved` | PROPERTY | Pending(위치 유무 무관)은 사람 개입 없이 Bound나 Retired로 정리된다 |
| `HostRecovers` | PROPERTY | 잃은 호스트 서버는 사람 손 없이 다시 뜬다 |

공정성은 dcserver와 호스트가 스스로 하는 일(생성·위치 기록·재조정·재채택·census·placed resolver·닫기·서버 재기동·재대기열)에만 건다.
입력·재시작·호스트 소실·종료 요청·Cancel 요청과 provider 기동은 강제하지 않는다.

## 결과

| cfg | 플랫폼 / 설계 / 조건 | 상태 수 | 깨지는 규칙 |
|---|---|---|---|
| `SessionHostUnixTmuxCurrent` | unix, main, tmux | 4,434 | 없음 |
| `SessionHostUnixTmux` | unix, 최종, tmux | 4,434 | 없음 |
| `SessionHostUnixHerdrCurrent` | unix, main, endpoint + 스위치 on | 3,647 | `PendingEventuallyResolved`, `RestartEventuallyReconnect` |
| `SessionHostUnixHerdr` | unix, 최종, endpoint + 스위치 on | 8,299 | 없음 |
| `SessionHostUnixEndpointOffCurrent` | unix, main, endpoint + 스위치 off | 40 | 없음(입력 전 거절) |
| `SessionHostUnixEndpointOff` | unix, 최종, endpoint + 스위치 off | 40 | 없음(같은 거절) |
| `SessionHostUnixNoTmuxCurrent` | unix tmux 없음, main(process 폴백) | 680 | `NoSessionLoss`, `NoDoubleSession`, `NoOrphanAfterTerminate`; `RestartEventuallyReconnect`, `InputAnswered`, `TerminateEventuallyGone` |
| `SessionHostUnixNoTmux` | unix tmux 없음, 최종 | 24 | 없음(admission 거절, 원문 보존) |
| `SessionHostSilentFallback` | 뮤턴트: 최종 설계가 process 폴백을 남김 | 680 | 위 6개 + `NoProcessHost` |
| `SessionHostErrConsumes` | 뮤턴트: 호스트 없음 오류가 원문을 소비(provider 함수 `Err`) | 40 | `NoInputLoss` |
| `SessionHostWindowsCurrent` | windows, main(process) | 682 | `NoSessionLoss` |
| `SessionHostWindowsSelectedCurrent` | windows, main, endpoint + 스위치 on | 40 | 없음(`cfg!(unix)` 스위치가 거절) |
| `SessionHostWindowsHerdr` | windows, 최종, endpoint + 스위치 on | 8,299 | 없음 |
| `SessionHostWindowsImplicit` | windows, 최종, 선택 없음·모든 bool off → 내부 기본 Herdr | 8,299 | 없음 |
| `SessionHostGateRun` | 뮤턴트: PlatformDefault에 실행 bool 적용 | 40 | `NoDefaultRefusal` |
| `SessionHostGateCancel` | 뮤턴트: PlatformDefault에 Cancel bool 적용 | 8,299 | `NoDefaultCancelDrop` |
| `SessionHostWindowsServerInJob` | 뮤턴트: Herdr 서버가 dcserver job 안에서 뜸 | 9,169 | `NoSessionLoss` |
| `SessionHostWindowsNoAutostart` | windows, 최종, 서버 재기동 수단 없음 | 4,226 | `HostRecovers` |
| `SessionHostSkipPlaced` | 뮤턴트: placed resolver 없음(main처럼 건너뜀) | 3,647 | `PendingEventuallyResolved`, `RestartEventuallyReconnect` |
| `SessionHostReplayPending` | 뮤턴트: 잃은 생성 응답을 재생성 허가로 읽음 | 4,095 | `NoSessionLoss`, `NoDoubleSession`, `NoOrphanAfterTerminate` |
| `SessionHostEarlyRetire` | 뮤턴트: 닫기 전에 행을 Retired로 | 8,470 | `NoSessionLoss`, `NoOrphanAfterTerminate` |
| `SessionHostNoWake` | 뮤턴트: 부팅이 재시작 패스에 닿지 못함 | 1,113 | `RestartEventuallyReconnect`, `InputAnswered`, `PendingEventuallyResolved` |
| `SessionHostForgetIdentity` | 뮤턴트: 재시작에서 위치·census 신원을 잃음 | 774 | `NoSessionLoss`; `RestartEventuallyReconnect`, `InputAnswered`, `PendingEventuallyResolved` |

리뷰 항목의 수정 전/후: P1-1 `UnixHerdrCurrent`(main)·`SkipPlaced` RED → `UnixHerdr`·`WindowsImplicit` GREEN. P1-2 `GateRun`·`GateCancel` RED → `WindowsImplicit` GREEN.
P1-3 `ErrConsumes` RED → `UnixNoTmux` GREEN. Unix Herdr 쌍의 상태 수가 다른 것(3,647 / 8,299)은 placed Pending 자동 탈출이 Unix에도 도는 의도한 동작 변화다.
`UnixEndpointOff*`는 endpoint + 스위치 off 거절이 main과 최종에서 같은 결과임을 보인다(표 전체가 아니라 이 칸만).

차트(500 상태 이하이거나 반례가 짧은 것): [Herdr 서버가 job 안](session-host-windows-server-in-job.chart.mmd),
[닫기 전 Retired](session-host-early-retire.chart.mmd), [Pending 재생성](session-host-replay-pending.chart.mmd),
[서버 재기동 수단 없음](session-host-windows-no-autostart.chart.mmd), [tmux 없는 Unix, 최종](session-host-unix-no-tmux.chart.mmd),
[placed resolver 없음](session-host-skip-placed.chart.mmd), [실행 bool 적용](session-host-gate-run.chart.mmd), [Cancel bool 적용](session-host-gate-cancel.chart.mmd),
[오류가 원문 소비](session-host-err-consumes.chart.mmd). 나머지 cfg의 차트와 TLC 원본은 레인 폴더 `sc-out/`에 있다(커밋하지 않음).

## 반례와 코드 대조

- `WindowsCurrent` `NoSessionLoss`: 입력 → process 실행 생성 → dcserver 종료. dcserver는 Windows에서 자기 자신을
  kill-on-close job에 넣으므로(`src/services/platform/windows_job.rs:35`, `:52`, 부팅 `src/cli/dcserver.rs:882`) 자식 provider가 함께 끝난다.
  레지스트리는 메모리 static이라(`src/services/session_backend.rs:341`) 새 dcserver가 다시 찾을 것도 없다.
- `WindowsServerInJob` `NoSessionLoss`: 최종 설계라도 Herdr 서버를 dcserver의 일반 자식으로 띄우면 같은 job이 서버와 모든 pane을 끝낸다.
  최종 설계 조각 W04가 서버를 job 밖에서 띄우는 이유다([U] U04).
- `UnixNoTmuxCurrent` 세 불변식: process 실행 → dcserver 종료 → 자기 process group이라 살아남지만(`src/services/session_backend.rs:240`) 레지스트리가 비어
  죽은 것으로 읽혀 행이 닫히고(고아) → 다음 입력이 두 번째 provider를 띄운다(이중 세션). 최종 설계는 admission 거절로 이 경로를 없앤다(`UnixNoTmux` GREEN).
- `ErrConsumes` `NoInputLoss`: 호스트 없음을 provider 함수 `Err`로만 알리면 owner 없는 턴은 `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:284-286`에서
  곧바로 실행 경로로 가고 원문이 큐로 돌아오지 않는다. 최종 설계는 `src/services/discord/router/message_handler/intake_turn/host_refusal.rs:6-63`의 복원 경계에서 거절한다.
- `UnixHerdrCurrent`·`SkipPlaced` `PendingEventuallyResolved`: 생성 → 위치 기록 → 실행 명령 확인 끊김(provider 미기동) → 재시작. 복구 패스는 Pending을 건너뛰고
  (`src/services/discord/recovery_engine/herdr_reader.rs:463-464`) unplaced resolver는 위치 없는 행만 받으므로(`src/services/herdr_launch/unplaced.rs:353`) 행이 영영 남고
  입력이 그 뒤에 묶인다. 최종 설계는 placed resolver가 adopt·retire·residue close 중 하나로 끝낸다.
- `GateRun` `NoDefaultRefusal`, `GateCancel` `NoDefaultCancelDrop`: Windows 기본 실행에 `herdr_codex_turn_enabled`(`src/services/turn_host.rs:220-232`)나
  `herdr_cancel_enabled`(`src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs:257-262`)를 그대로 적용하면 기본 설정에서 실행이 거절되거나 Escape가 버려진다.
- `ReplayPending`: 생성 효과 뒤 응답·위치 기록 전에 재시작 → 같은 Pending에서 다시 생성 → 두 실행(이중), 하나를 정리하며 고아·유실. 최종 설계는 Pending을 census로만 정리한다
  (`src/services/herdr_launch.rs:482`, `src/services/herdr_launch/unplaced.rs`).
- `EarlyRetire`: 닫기 요청을 보내자마자 행을 Retired로 바꾸면 소멸 확인 전 Retired + 살아 있는 실행. 최종 설계는 소유 실행 소멸 확인 뒤 retire한다
  (`src/services/claude/herdr_return.rs:83`의 end-before-retire와 같은 순서). residue close도 같은 순서다.
- `NoWake`: 재시작 패스(`src/services/discord/tui_prompt_relay/rehydration.rs:270`, `:668` → `src/services/discord/recovery_engine/herdr_reader.rs:220`)가
  Windows에서 불리지 않으면 같은 실행이 살아 있어도 다시 잡지 못하고 입력이 그 행 뒤에 묶인다. 조각 W08의 수용 시험이 생산 부팅 진입점에서 이 호출 도달을 단언한다.
- `ForgetIdentity`: 재시작에서 위치와 nonce census를 잃으면 Pending이 영영 정리되지 않는다. 최종 설계는 내구 행의 위치·nonce를 재채택의 유일한 근거로 쓴다.
- `WindowsNoAutostart` `HostRecovers`: 서버가 끝나면 입력은 거절·보존으로 답하지만(조용한 유실은 없음) 서버는 돌아오지 않는다. W04가 자동 기동을 Windows에 연다.

## 범위 밖

Herdr launch의 세부 단계(`session-lifecycle/HerdrSession.tla`), tmux↔Herdr 채널 이동(`host-migration/HostMigration.tla`), tmux 부팅 지연 probe,
provider가 스스로 끝나는 경우, Herdr Core(F4 레인 전용, 채널 세션 호스트가 아님). Windows OS 계약(named pipe 서버 PID·FILETIME, job 탈출,
ConPTY 입력)은 가정이며 실측 PASS가 아니다.
