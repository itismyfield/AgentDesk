# SessionHost (#7583 최종 설계)

AgentDesk `origin/main` `1974cff8a67bf5d1db041abfdd4d615c518ee104` 기준. 한 Discord 채널의 관리 provider 세션이
플랫폼·채널 선택에 따라 고른 호스트(tmux, Herdr, process) 위에서 dcserver 재시작·호스트 서버 소실·종료 요청을 지나는 모델이다.
`Design = "proposed"`는 최종 설계(ProcessBackend 삭제, Windows 관리 Claude/Codex는 Herdr, tmux 없는 Unix는 spawn 전 거절)다.

설계 1 모델(`refs/review/i7583-design-1`, 10 cfg)의 Pending CAS·생성 효과·위치 기록·재채택·닫기 분리를 기반으로 하고,
Fable 모델(`refs/review/i7583-design-fable-1`, 14 cfg)에만 있던 플랫폼별 호스트 선택·tmux 부재·Windows kill-on-close job·
호스트 서버 소실과 재기동·입력 거절과 그 뮤턴트(`serverInJob`, `silentFallback`)를 옮겨 넣었다.
Fable의 `BootOpensEarly`(재조정 없이 intake가 열림)는 설계 1의 `NoWake`(재시작 패스에 닿지 못함)와 같은 결함이라 `NoWake` 하나로 합쳤다.
Fable의 `RetireBeforeClose`는 설계 1의 `EarlyRetire`와 같다. 변수 값 범위는 [진입점 표](../../design/i7583/entrypoints.md)에서만 가져왔다.

TLC 1.7.4(`statechart` 스킬), 17 cfg: 규칙이 모두 성립 8개, 아래에 적은 규칙만 깨짐 9개(모두 예상). `expected.tsv`가 같은 행을 가진다.
Rust는 빌드·실행하지 않았다. 반례는 코드를 읽어 대조했다.

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| `NoSessionLoss` | INVARIANT | dcserver 재시작만으로 provider가 끝나지 않고, 살아 있는 실행은 내구 행(레지스트리·위치·호스트 census)으로 닿을 수 있다 |
| `NoDoubleSession` | INVARIANT | 한 채널에 살아 있는 실행은 하나 이하 |
| `NoOrphanAfterTerminate` | INVARIANT | Retired 행에는 살아 있는 실행이 없다(종료 뒤 고아 0) |
| `NoProcessHost` | INVARIANT | 최종 설계는 process 호스트를 띄우지 않는다 |
| `RestartEventuallyReconnect` | PROPERTY | 재시작 뒤 같은 실행을 다시 잡거나, 정말 사라진 실행의 행을 닫는다 |
| `InputAnswered` | PROPERTY | 모든 입력은 시작되거나 거절된다 |
| `TerminateEventuallyGone` | PROPERTY | 종료 요청은 소유 실행의 소멸 확인으로 끝난다 |
| `PendingEventuallyResolved` | PROPERTY | Pending은 Bound나 Retired로 정리된다 |
| `HostRecovers` | PROPERTY | 잃은 호스트 서버는 사람 손 없이 다시 뜬다 |

공정성은 dcserver와 호스트가 스스로 하는 일(생성·위치 기록·재조정·재채택·census·닫기·서버 재기동)에만 건다.
입력·재시작·호스트 소실·종료 요청은 강제하지 않는다.

## 결과

| cfg | 플랫폼 / 설계 / 채널 | 상태 수 | 깨지는 규칙 |
|---|---|---|---|
| `SessionHostUnixTmuxCurrent` | unix, main, tmux | 991 | 없음 |
| `SessionHostUnixTmux` | unix, 최종, tmux | 991 | 없음 |
| `SessionHostUnixHerdrCurrent` | unix, main, Herdr 선택 | 991 | 없음 |
| `SessionHostUnixHerdr` | unix, 최종, Herdr 선택 | 991 | 없음 |
| `SessionHostUnixNoTmuxCurrent` | unix tmux 없음, main(process 폴백) | 231 | `NoSessionLoss`, `NoDoubleSession`, `NoOrphanAfterTerminate`; `RestartEventuallyReconnect`, `InputAnswered`, `TerminateEventuallyGone` |
| `SessionHostUnixNoTmux` | unix tmux 없음, 최종 | 40 | 없음(입력이 spawn 전 거절됨) |
| `SessionHostSilentFallback` | 뮤턴트: 최종 설계가 process 폴백을 남김 | 231 | 위 6개 + `NoProcessHost` |
| `SessionHostWindowsCurrent` | windows, main(process) | 238 | `NoSessionLoss` |
| `SessionHostWindowsSelectedCurrent` | windows, main, Herdr 선택 | 40 | 없음(`cfg!(unix)` 스위치가 거절) |
| `SessionHostWindowsHerdr` | windows, 최종, Herdr 선택 | 991 | 없음 |
| `SessionHostWindowsImplicit` | windows, 최종, 선택 없음 → 내부 기본 Herdr | 991 | 없음 |
| `SessionHostWindowsServerInJob` | 뮤턴트: Herdr 서버가 dcserver job 안에서 뜸 | 1,113 | `NoSessionLoss` |
| `SessionHostWindowsNoAutostart` | windows, 최종, 서버 재기동 수단 없음 | 528 | `HostRecovers` |
| `SessionHostReplayPending` | 뮤턴트: 잃은 생성 응답을 재생성 허가로 읽음 | 634 | `NoDoubleSession` |
| `SessionHostEarlyRetire` | 뮤턴트: 닫기 전에 행을 Retired로 | 1,004 | `NoSessionLoss`, `NoOrphanAfterTerminate` |
| `SessionHostNoWake` | 뮤턴트: 부팅이 재시작 패스에 닿지 못함 | 364 | `RestartEventuallyReconnect`, `InputAnswered`, `PendingEventuallyResolved` |
| `SessionHostForgetIdentity` | 뮤턴트: 재시작에서 위치·census 신원을 잃음 | 238 | `NoSessionLoss`; `RestartEventuallyReconnect`, `InputAnswered`, `PendingEventuallyResolved` |

Unix tmux·Unix Herdr 쌍은 main과 최종 설계의 도달 상태 수가 같다(991). 채널 기본값이 바뀌지 않는다는 모델 근거다.
Windows 최종 설계(선택·암묵 기본)는 Unix Herdr와 같은 상태 공간(991)이다.

차트(500 상태 이하이거나 반례가 짧은 것): [Herdr 서버가 job 안](session-host-windows-server-in-job.chart.mmd),
[닫기 전 Retired](session-host-early-retire.chart.mmd), [Pending 재생성](session-host-replay-pending.chart.mmd),
[서버 재기동 수단 없음](session-host-windows-no-autostart.chart.mmd), [tmux 없는 Unix, 최종](session-host-unix-no-tmux.chart.mmd).
나머지 cfg의 차트와 TLC 원본은 레인 폴더 `sc-out/`에 있다(커밋하지 않음, 화살표 76~189개로 읽기 어려움).

## 반례와 코드 대조

- `WindowsCurrent` `NoSessionLoss`: 입력 → process 실행 생성 → dcserver 종료. dcserver는 Windows에서 자기 자신을
  kill-on-close job에 넣으므로(`src/services/platform/windows_job.rs:35`, `:52`, 부팅 `src/cli/dcserver.rs:882`) 자식 provider가 함께 끝난다.
  레지스트리는 메모리 static이라(`src/services/session_backend.rs:341`) 새 dcserver가 다시 찾을 것도 없다.
- `WindowsServerInJob` `NoSessionLoss`: 최종 설계라도 Herdr 서버를 dcserver의 일반 자식으로 띄우면 같은 job이 서버와 모든 pane을 끝낸다.
  최종 설계 조각 W04가 서버를 job 밖에서 띄우는 이유다([U] U04).
- `UnixNoTmuxCurrent` 세 불변식: process 실행 → dcserver 종료 → 자기 process group이라 살아남지만(`src/services/session_backend.rs:240`) 레지스트리가 비어
  죽은 것으로 읽혀 행이 닫히고(고아) → 다음 입력이 두 번째 provider를 띄운다(이중 세션). 최종 설계는 spawn 전 거절로 이 경로를 없앤다(`UnixNoTmux` GREEN).
- `ReplayPending` `NoDoubleSession`: 생성 효과 뒤 응답·위치 기록 전에 재시작 → 같은 Pending에서 다시 생성. 최종 설계는 Pending을 census로만 정리한다
  (`src/services/herdr_launch.rs:482`, `src/services/herdr_launch/unplaced.rs`).
- `EarlyRetire`: 닫기 요청을 보내자마자 행을 Retired로 바꾸면 소멸 확인 전 Retired + 살아 있는 실행. 최종 설계는 소유 실행 소멸 확인 뒤 retire한다
  (`src/services/claude/herdr_return.rs:83`의 end-before-retire와 같은 순서).
- `NoWake`: 재시작 패스(`src/services/discord/tui_prompt_relay/rehydration.rs:270`, `:668` → `src/services/discord/recovery_engine/herdr_reader.rs:220`)가
  Windows에서 불리지 않으면 같은 실행이 살아 있어도 다시 잡지 못하고 입력이 그 행 뒤에 묶인다. 조각 W06·W07의 수용 시험이 생산 부팅 진입점에서 이 호출 도달을 단언한다.
- `ForgetIdentity`: 재시작에서 위치와 nonce census를 잃으면 Pending이 영영 정리되지 않는다. 최종 설계는 내구 행의 위치·nonce를 재채택의 유일한 근거로 쓴다.
- `WindowsNoAutostart` `HostRecovers`: 서버가 끝나면 입력은 거절로 답하지만(조용한 유실은 없음) 서버는 돌아오지 않는다. W04가 자동 기동을 Windows에 연다.

## 범위 밖

Herdr launch의 세부 단계(`session-lifecycle/HerdrSession.tla`), tmux↔Herdr 채널 이동(`host-migration/HostMigration.tla`), tmux 부팅 지연 probe,
provider가 스스로 끝나는 경우, Herdr Core(F4 레인 전용, 채널 세션 호스트가 아님). Windows OS 계약(named pipe 서버 PID·FILETIME, job 탈출,
ConPTY 입력)은 가정이며 실측 PASS가 아니다.
