# SessionHost (#7583 최종 설계)

AgentDesk `origin/main` `1974cff8a67bf5d1db041abfdd4d615c518ee104` 기준. 한 Discord 채널의 관리 provider 세션이
플랫폼·채널 선택에 따라 고른 호스트(tmux, Herdr, process) 위에서 dcserver 재시작·호스트 서버 소실·종료 요청을 지나는 모델이다.
`Design = "proposed"`는 최종 설계(ProcessBackend 삭제, Windows 관리 Claude/Codex는 Herdr, 호스트가 없으면 provider 호출 전 intake admission에서 거절하고 원문을 남김)다.

설계 1 모델(`refs/review/i7583-design-1`, 10 cfg)의 Pending CAS·생성 효과·위치 기록·재채택·닫기 분리를 기반으로 하고,
Fable 모델(`refs/review/i7583-design-fable-1`, 14 cfg)에만 있던 플랫폼별 호스트 선택·tmux 부재·Windows kill-on-close job·
호스트 서버 소실과 재기동·입력 거절과 그 뮤턴트(`serverInJob`, `silentFallback`)를 옮겨 넣었다.
Fable의 `BootOpensEarly`(재조정 없이 intake가 열림)는 설계 1의 `NoWake`(재시작 패스에 닿지 못함)와 같은 결함이라 `NoWake` 하나로 합쳤다.
Fable의 `RetireBeforeClose`는 설계 1의 `EarlyRetire`와 같다. 변수 값 범위는 [진입점 표](../../design/i7583/entrypoints.md)에서만 가져왔다.

개정 2(r2)는 독립 리뷰를 반영해 위치가 기록된 Pending과 그 자동 탈출, endpoint와 스위치의 분리, Cancel 스위치와 전달, 호스트 없음 거절의 원문 보존을 더했다.
개정 3(r3)은 r2 리뷰를 반영해 다음을 더했다: provider 증명과 source 증명의 분리(`src`, Codex는 첫 prompt와 함께 첫 hook을 보냄),
Codex 첫 입력(`FirstPrompt`·`HookArrives`·`TurnAttach`)과 source 없는 Bound의 거절(`FollowupNoSource`), 첫 입력 전 provider의 확인만 하는 resolver 행동(`ConfirmReady`),
Herdr Escape가 Claude·Codex 모두 Cancel 스위치를 따른다는 사실(`CancelOk`), 바뀌는 tmux 가용성(`tmuxUp`, `TmuxReturns`)과 재제출 횟수(`requeues`).
상수 `Codex`, 뮤턴트 3개(`bindProven`, `escapeNoSwitch`, `noAvailWake`), cfg 7개를 더했다.
개정 4(r4)는 r3 리뷰를 반영해 `src`를 둘로 나눴다: 내구 source 기록이 가리키는 실행(`srcOf`)과 재시작이 비우는 이 프로세스의 등록(`reg`: 실행 목록·provider 매핑·placeholder·runtime binding).
hook은 등록된 실행에서만 source를 남기고(아니면 425 재시도), attach·placed resolver의 Bound는 등록 ∧ 자기 source를 요구한다. 재시작 뒤 등록을 되살리는 `RestoreReg`와
입력 한 건의 수탁(`owed`, `wasKept`, `resub`)을 더했다. 뮤턴트 4개(`noRestore`, `hookConsumed`, `foreignSource`, `dropOnWake`)와 cfg 4개를 더했다.

TLC 1.7.4(`statechart` 스킬), 34 cfg: 규칙이 모두 성립 11개, 아래에 적은 규칙만 깨짐 23개(모두 예상). `expected.tsv`가 같은 행을 가진다.
Rust는 빌드·실행하지 않았다. 반례는 코드를 읽어 대조했다.

**보장 범위.** 이 모델이 보이는 것은 (1) 고른 호스트 위에서 재시작·Pending·입력·종료 계약이 성립하는가, (2) Windows PlatformDefault 실행이 bool 스위치 때문에
거절되거나 Escape가 버려지지 않는가, (3) Unix endpoint의 Escape가 Cancel 스위치 off에서 꺼진 채인가, (4) 도착한 입력이 답 전에는 늘 실행 대기 또는 보존 중이고(폐기 0), 보존된 입력이 호스트가 돌아오면 답에 닿으며 그 답이 실제 시작일 수 있는가,
(5) 재시작한 Codex Pending이 사람 없이 Bound 또는 기존 정리로 끝나고 다른 실행의 source로 Bound가 되지 않는가다.
채널 선택 표(설계 §5) 전체가 main과 같다는 것은 보이지 않는다 — 그것은 조각 W01의 표 기반 시험이 정본이다(설계 §5.2).
첫 write 뒤 hold가 남았는데 hook이 끝내 오지 않는 경우(설계 §7.3)는 모델에 넣지 않았다(hold 기록과 write를 한 걸음으로 묶음, main hold 계약 그대로).

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| `NoSessionLoss` | INVARIANT | dcserver 재시작만으로 provider가 끝나지 않고, 살아 있는 실행은 내구 행(레지스트리·위치·호스트 census)으로 닿을 수 있다 |
| `NoDoubleSession` | INVARIANT | 한 채널에 살아 있는 실행은 하나 이하 |
| `NoOrphanAfterTerminate` | INVARIANT | Retired 행에는 살아 있는 실행이 없다(종료 뒤 고아 0) |
| `NoProcessHost` | INVARIANT | 최종 설계는 process 호스트를 띄우지 않는다 |
| `NoInputLoss` | INVARIANT | 호스트 없음 거절이 원문을 소비하지 않는다 |
| `CustodyKept` | INVARIANT | 도착한 입력은 답(시작·실패·거절) 전까지 실행 대기 또는 보존 중이다(wake가 보존을 풀며 버리지 않음) |
| `NoDefaultRefusal` | INVARIANT | 정책 거절은 endpoint가 설정된 채널에서만 난다(PlatformDefault는 bool로 거절되지 않음) |
| `NoDefaultCancelDrop` | INVARIANT | Escape가 버려지는 것은 endpoint 채널의 스위치 off뿐이다 |
| `BoundOwnSource` | INVARIANT | Bound 행은 자기 실행의 source를 가진다(source 없는 Bound는 모든 입력을 거절하고, 다른 실행의 source면 다른 transcript를 이 세션으로 읽는다) |
| `UnixCancelAsMain` | INVARIANT | Unix endpoint 채널의 Herdr Escape는 Cancel 스위치 off에서 꺼진 채다(main과 같음) |
| `NeverResubmitted` | INVARIANT(증인) | `RequeueReach` cfg에만 둔다. 깨지면 보존됐던 입력이 호스트 복귀 뒤 실제로 시작되는 것이 정상 설계에서 도달 가능하다는 뜻 |
| `RestartEventuallyReconnect` | PROPERTY | 재시작 뒤 같은 실행을 다시 잡거나, 정말 사라진 실행의 행을 닫는다 |
| `InputAnswered` | PROPERTY | 모든 입력은 시작되거나 거절·보존된다 |
| `TerminateEventuallyGone` | PROPERTY | 종료 요청은 소유 실행의 소멸 확인으로 끝난다 |
| `PendingEventuallyResolved` | PROPERTY | Pending은 사람 개입 없이 Bound·Retired, 또는 "provider가 살아 있고 첫 입력 전이며 대기 입력이 없는" 상태에 닿는다 |
| `CodexFirstInputDelivered` | PROPERTY | 첫 입력 전 Codex Pending에 입력이 기다리면 같은 실행에 첫 prompt가 전달된다(또는 실행이 사라짐) |
| `HeldInputSubmitted` | PROPERTY | 보존된 입력은 호스트를 쓸 수 있게 되면 답(시작·실패·거절)에 닿는다(보존 해제만으로는 성립하지 않음) |
| `RestartedPendingResolved` | PROPERTY | 첫 prompt를 받은 뒤 재시작한 Codex Pending은 Bound가 되거나 기존 정리로 Retired가 된다 |
| `HostRecovers` | PROPERTY | 잃은 호스트 서버는 사람 손 없이 다시 뜬다 |

공정성은 dcserver와 호스트가 스스로 하는 일(생성·위치 기록·재조정·재채택·census·placed resolver·첫 prompt·hook 뒤 attach·닫기·서버 재기동·재대기열)과
Codex가 첫 prompt와 함께 보내는 첫 hook과 재시작 뒤 등록 복원(`RestoreReg`)에 건다. 입력·재시작·호스트 소실·tmux 복귀·종료 요청·Cancel 요청과 provider 기동은 강제하지 않는다.

## 결과

| cfg | 플랫폼 / 설계 / 조건 | 상태 수 | 깨지는 규칙 |
|---|---|---|---|
| `SessionHostUnixTmuxCurrent` | unix, main, tmux | 3,481 | 없음 |
| `SessionHostUnixTmux` | unix, 최종, tmux | 3,481 | 없음 |
| `SessionHostUnixHerdrCurrent` | unix, main, endpoint + 스위치 on, Claude | 4,204 | `PendingEventuallyResolved`, `RestartEventuallyReconnect` |
| `SessionHostUnixHerdr` | unix, 최종, endpoint + 스위치 on, Claude, Cancel 스위치 off | 6,034 | 없음 |
| `SessionHostCodexHerdrCurrent` | unix, main, endpoint, Codex | 5,915 | `PendingEventuallyResolved`, `RestartEventuallyReconnect`, `RestartedPendingResolved` |
| `SessionHostCodexHerdr` | unix, 최종, endpoint, Codex(재시작 복원 포함) | 8,340 | 없음 |
| `SessionHostCodexNoRestore` | 뮤턴트(r3 설계): 재시작 뒤 등록 복원 없음 | 8,112 | `PendingEventuallyResolved`, `RestartEventuallyReconnect`, `RestartedPendingResolved` |
| `SessionHostHookConsumed` | 뮤턴트: 등록 전 hook이 무기록으로 소비 | 8,570 | `PendingEventuallyResolved`, `RestartEventuallyReconnect`, `RestartedPendingResolved` |
| `SessionHostForeignSource` | 뮤턴트: resolver가 다른 실행의 source로 Bound | 8,394 | `BoundOwnSource` |
| `SessionHostUnixEndpointOffCurrent` | unix, main, endpoint + 스위치 off | 40 | 없음(입력 전 거절) |
| `SessionHostUnixEndpointOff` | unix, 최종, endpoint + 스위치 off | 40 | 없음(같은 거절) |
| `SessionHostUnixNoTmuxCurrent` | unix tmux 없음, main(process 폴백) | 5,000 | `NoSessionLoss`, `NoDoubleSession`, `NoOrphanAfterTerminate`; `RestartEventuallyReconnect`, `InputAnswered`, `TerminateEventuallyGone` |
| `SessionHostUnixNoTmux` | unix tmux 없음, 최종 | 6,886 | 없음(admission hold, tmux 복귀 뒤 재제출) |
| `SessionHostRequeueReach` | 위와 같고 증인 규칙 `NeverResubmitted` 추가 | 6,886 | `NeverResubmitted`(도달 증인) |
| `SessionHostNoAvailWake` | 뮤턴트: 가용성 복귀 wake 없음 | 3,513 | `HeldInputSubmitted` |
| `SessionHostDropOnWake` | 뮤턴트: wake가 보존을 풀고 실행 대기에 넣지 않음 | 3,687 | `CustodyKept`; `HeldInputSubmitted` |
| `SessionHostSilentFallback` | 뮤턴트: 최종 설계가 process 폴백을 남김 | 5,000 | 위 6개 + `NoProcessHost` |
| `SessionHostErrConsumes` | 뮤턴트: 호스트 없음 오류가 원문을 소비 | 4,600 | `NoInputLoss`, `CustodyKept` |
| `SessionHostWindowsCurrent` | windows, main(process) | 1,384 | `NoSessionLoss` |
| `SessionHostWindowsSelectedCurrent` | windows, main, endpoint + 스위치 on | 80 | 없음(`cfg!(unix)` 스위치가 거절) |
| `SessionHostWindowsHerdr` | windows, 최종, endpoint + 스위치 on | 12,068 | 없음 |
| `SessionHostWindowsImplicit` | windows, 최종, 선택 없음·모든 bool off, Claude | 12,068 | 없음 |
| `SessionHostCodexImplicit` | windows, 최종, 선택 없음·모든 bool off, Codex | 16,680 | 없음 |
| `SessionHostBindProven` | 뮤턴트(r2 처방): provider 증명만으로 Bound, Codex | 35,462 | `BoundOwnSource`; `CodexFirstInputDelivered` |
| `SessionHostEscapeNoSwitch` | 뮤턴트(r2 표): Unix Claude Escape를 settlement만으로 허가 | 6,034 | `UnixCancelAsMain` |
| `SessionHostGateRun` | 뮤턴트: PlatformDefault에 실행 bool 적용 | 80 | `NoDefaultRefusal` |
| `SessionHostGateCancel` | 뮤턴트: PlatformDefault에 Cancel bool 적용 | 12,068 | `NoDefaultCancelDrop` |
| `SessionHostSkipPlaced` | 뮤턴트: placed resolver 없음(main처럼 건너뜀) | 8,408 | `PendingEventuallyResolved`, `RestartEventuallyReconnect` |
| `SessionHostWindowsServerInJob` | 뮤턴트: Herdr 서버가 dcserver job 안에서 뜸 | 13,170 | `NoSessionLoss` |
| `SessionHostWindowsNoAutostart` | windows, 최종, 서버 재기동 수단 없음 | 5,926 | `HostRecovers` |
| `SessionHostReplayPending` | 뮤턴트: 잃은 생성 응답을 재생성 허가로 읽음 | 9,232 | `NoSessionLoss`, `NoDoubleSession`, `NoOrphanAfterTerminate` |
| `SessionHostEarlyRetire` | 뮤턴트: 닫기 전에 행을 Retired로 | 12,200 | `NoSessionLoss`, `NoOrphanAfterTerminate` |
| `SessionHostNoWake` | 뮤턴트: 부팅이 재시작 패스에 닿지 못함 | 2,522 | `RestartEventuallyReconnect`, `InputAnswered`, `PendingEventuallyResolved` |
| `SessionHostForgetIdentity` | 뮤턴트: 재시작에서 위치·census 신원을 잃음 | 1,804 | `NoSessionLoss`; `RestartEventuallyReconnect`, `InputAnswered`, `PendingEventuallyResolved` |

리뷰 항목의 수정 전/후: r3 P1-1 `BindProven` RED → `CodexImplicit`·`CodexHerdr` GREEN. r3 P1-2 `EscapeNoSwitch` RED → `UnixHerdr` GREEN.
r3 P1-3·P2 `NoAvailWake`·`ErrConsumes` RED → `UnixNoTmux` GREEN, 재제출 도달은 `RequeueReach`가 증인이다(`HeldInputSubmitted`가 공허하게 성립하지 않음).
r2 항목: `UnixHerdrCurrent`·`SkipPlaced` RED → `UnixHerdr`·`WindowsImplicit` GREEN, `GateRun`·`GateCancel` RED → `WindowsImplicit` GREEN.
r4 P1 `CodexNoRestore`(수정 전 설계)·`HookConsumed` RED → `CodexHerdr` GREEN, `ForeignSource` RED → `CodexHerdr` GREEN(`BoundOwnSource`). r4 P2-3 `DropOnWake` RED → `UnixNoTmux` GREEN(`CustodyKept`),
`RequeueReach`의 증인은 이제 보존 해제가 아니라 보존됐던 입력의 실제 시작이다. 다른 cfg의 판정은 r3과 같다(`ErrConsumes`는 `CustodyKept`도 깬다).
`UnixEndpointOff*`는 endpoint + 스위치 off 거절이 main과 최종에서 같은 결과임을 보인다(표 전체가 아니라 이 칸만).

차트(반례가 짧은 것): [Herdr 서버가 job 안](session-host-windows-server-in-job.chart.mmd),
[닫기 전 Retired](session-host-early-retire.chart.mmd), [Pending 재생성](session-host-replay-pending.chart.mmd),
[서버 재기동 수단 없음](session-host-windows-no-autostart.chart.mmd), [placed resolver 없음](session-host-skip-placed.chart.mmd),
[실행 bool 적용](session-host-gate-run.chart.mmd), [Cancel bool 적용](session-host-gate-cancel.chart.mmd), [오류가 원문 소비](session-host-err-consumes.chart.mmd),
[provider 증명만으로 Bound](session-host-bind-proven.chart.mmd), [Unix Escape가 스위치 없이 켜짐](session-host-escape-no-switch.chart.mmd),
[가용성 wake 없음](session-host-no-avail-wake.chart.mmd), [보존 → tmux 복귀 → 재제출(증인)](session-host-requeue-reach.chart.mmd),
[재시작 복원 없음](session-host-codex-no-restore.chart.mmd), [등록 전 hook 소비](session-host-hook-consumed.chart.mmd), [남의 source로 Bound](session-host-foreign-source.chart.mmd),
[wake가 보존 입력을 버림](session-host-drop-on-wake.chart.mmd).
나머지 cfg의 차트와 TLC 원본은 레인 폴더 `sc-out-r4/`에 있다(커밋하지 않음).

## 반례와 코드 대조

- `BindProven` `BoundOwnSource`·`CodexFirstInputDelivered`: 생성 → 위치 기록 → (턴 실패) → Codex provider 기동 → resolver가 provider 증명만으로 Bound → 다음 입력이
  source 없음으로 거절(`src/services/codex/herdr_turn.rs:515-531`)되고, 첫 입력 함수는 Pending만 받으므로(`src/services/codex/herdr_turn/first_input.rs:90-103`) 첫 prompt가 영영 가지 않는다.
  Codex 첫 hook은 첫 prompt와 함께 오고(`src/services/codex/herdr_turn.rs:266-274`) source attach는 baseline이 없으면 보류된다(`src/services/discord/tui_prompt_relay/herdr_source.rs:294-302`).
  최종 설계는 첫 입력 전 Pending을 지키고 기존 첫 입력 경로로 잇는다(`CodexImplicit` GREEN).
- `CodexNoRestore` `RestartedPendingResolved`(수정 전 설계, 이번 P1): 생성 → 위치 기록 → Codex 기동 → 첫 prompt → 첫 hook(source 기록) → dcserver 재시작 → 재채택 대기에서 영영 멈춤.
  재시작은 실행 목록(`src/services/tui_prompt_dedupe/runtime_binding/adopt_skip.rs:102`)과 runtime binding을 비우고, Codex attach는 live binding이 그 source여야 하며
  (`src/services/discord/tui_prompt_relay/herdr_source.rs:231-238`, 아니면 NotPublished), 재시작 복원은 Bound만 받는다(`src/services/discord/recovery_engine/host_reconcile.rs:96-98`).
  첫 입력 함수는 hook이 기록됐으면 거절한다(`src/services/codex/herdr_turn.rs:235-239`). 최종 설계는 resolver가 attach 전에 같은 nonce의 등록을 쓰기 없이 되살린다(설계 §7.3 재시작 복원).
- `HookConsumed`: 첫 prompt 뒤 hook 전에 재시작하고, 등록이 되살아나기 전에 온 hook이 목록에 없어 이 실행으로 기록되지 않은 채 넘어가면
  (`src/services/tui_prompt_dedupe/runtime_binding/codex_hook.rs:333-335`, `src/services/claude_tui/hook_server/observation_ingress.rs:187-211`) Codex는 hook을 다시 보내지 않아 source가 영영 없다.
  최종 설계는 판정 대기 동안 이 hook을 425로 재시도시킨다.
- `ForeignSource` `BoundOwnSource`: resolver가 "source가 있다"만 보고 Bound하면 앞 실행이 남긴 source로 새 실행을 Bound한다. 최종 설계는 nonce baseline·spawn marker·live binding이 모두 같은 실행일 때만 기존 attach가 Bound를 쓴다.
- `DropOnWake` `CustodyKept`: 가용성 wake가 보존을 풀면서 입력을 실행 대기에 넣지 않으면 답 없이 사라진다. r3 모델의 `HeldInputSubmitted`(보존 해제가 목표)와 증인은 이 폐기를 구별하지 못했다.
- `CodexHerdrCurrent` `PendingEventuallyResolved`: main Codex의 첫 prompt 뒤 provider가 사라지면 Pending은 복구 패스가 건너뛰어 남는다(같은 원인, `UnixHerdrCurrent`와 같은 결함).
- `EscapeNoSwitch` `UnixCancelAsMain`: Claude Herdr stop도 `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:389-402`에서 `codex_stop_delivery`로 가고
  `herdr_command_eligible`(`:250-262`)이 Cancel 스위치를 본다. r2 표처럼 Unix Claude Escape를 settlement만으로 허가하면 스위치 off인 채널에서 Escape가 나간다.
- `NoAvailWake` `HeldInputSubmitted`: 보존된 입력이 tmux가 돌아온 뒤에도 깨어나지 않으면 큐에 영영 남는다. 최종 설계는 가용성 신호가 drain을 깨운다(설계 §10.2).
- `WindowsCurrent` `NoSessionLoss`: 입력 → process 실행 생성 → dcserver 종료. dcserver는 Windows에서 자기 자신을
  kill-on-close job에 넣으므로(`src/services/platform/windows_job.rs:35`, `:52`, 부팅 `src/cli/dcserver.rs:882`) 자식 provider가 함께 끝난다.
  레지스트리는 메모리 static이라(`src/services/session_backend.rs:341`) 새 dcserver가 다시 찾을 것도 없다.
- `WindowsServerInJob` `NoSessionLoss`: 최종 설계라도 Herdr 서버를 dcserver의 일반 자식으로 띄우면 같은 job이 서버와 모든 pane을 끝낸다.
  최종 설계 조각 W04가 서버를 job 밖에서 띄우는 이유다([U] U04).
- `UnixNoTmuxCurrent` 세 불변식: process 실행 → dcserver 종료 → 자기 process group이라 살아남지만(`src/services/session_backend.rs:240`) 레지스트리가 비어
  죽은 것으로 읽혀 행이 닫히고(고아) → 다음 입력이 두 번째 provider를 띄운다(이중 세션). 최종 설계는 admission hold로 이 경로를 없앤다(`UnixNoTmux` GREEN).
- `ErrConsumes` `NoInputLoss`: 호스트 없음을 provider 함수 `Err`로만 알리면 owner 없는 턴은 `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:284-286`에서
  곧바로 실행 경로로 가고 원문이 큐로 돌아오지 않는다. live 메시지는 `host_refusal`의 비queued 분기(`src/services/discord/router/message_handler/intake_turn/host_refusal.rs:46-63`)도 본문을 남기지 않는다. 최종 설계는 router admission(`src/services/discord/router/intake_dispatch.rs:229`)에서 내구 hold로 분류한다.
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
