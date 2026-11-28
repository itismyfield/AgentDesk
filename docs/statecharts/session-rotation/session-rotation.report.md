# 전사 크기 세션 회전 (#8472 design-3)

코드 기준은 `30053c6f23`(origin/main `826a01da0e` 병합)이다.
설계 문서는 이 폴더 밖(레인 산출물)에 있고, 여기에는 모델과 검사 결과만 둔다.
`SessionRotationD1.tla`·`D1Held.cfg`는 design-1 원문 모델이고 바뀌지 않았다.

## 무엇을 모델링하나

한 채널의 Claude 세션 하나를 Old에서 New로 한 번 회전한다.

1. 회전 예약(`Reserve`)
2. 인수인계 요청 전송(`AskDispatch`, 이때 래치 소진)
3. 인수인계 API(`Stage`)와 요청 턴 끝
4. 입력 배리어(`Barrier`)
5. 협조 종료(`StopDispatch`·`StopResolve`·`StopSettle`)
6. 고정 UUID 새 기동(`LaunchDispatch`·`LaunchResolve`·`LaunchSettle`)
7. 연결과 seed(`Bind`·`SeedDispatch`·`SeedDone`)

배리어 뒤 실패는 단계마다 자동 해제(`Release`)로 끝난다. design-3에서 더한 것은 다음과 같다.

- 효과 불명: `/exit`와 기동은 Unknown으로 끝날 수 있다(`stopPending`·`launchPending`). 호스트 증거가 정산(`StopSettle`·`LaunchSettle`)하기 전에는 풀지 않는다.
- 호스트 입력 보류: seed가 보류에 걸리면 `HoldResolve`가 판정한 뒤에만 푼다.
- 안내: seed 유실 안내는 Owner 의도(`DeclareLoss`), outbox 게시(`NoticePost`), 기록(`NoticeRecord`)의 세 단계다.
- clear: `/clear`는 `ClearBegin`(clear epoch 증가, 회전 취소)과 `ClearCommit`(Owner 대기 입력 정리, 실행 종료)으로 나뉜다.
- 자기 선택 이동: `SelfPublish`는 New의 SessionStart가 `Bind` 전에 선택을 New로 옮기는 동작이다.
- 회전 밖 경로
  - `DirectInput`: `/tui/send`, slash passthrough, Herdr 설정 키
  - `NativeInput`: pane 직접 입력. 거절할 수 없다.
  - `FireSchedule`·`ToggleSchedule`: CLI 예약
  - `ExternalKill`·`ExternalSelect`: 외부 종료, 다른 UUID 선택
  - `UserStop`: Cancel·stop·Terminate·effort
  - `Crash`·`Boot`
- `Rearm`은 끝난 회전 뒤 다음 회전을 연다. 같은 UUID를 다시 요청하는지는 래치가 정한다.
- `EnvBypass`·`EnvControl`은 회전 밖 경로 묶음을 켠다. 상태 수 때문에 Fixed를 셋으로 나눴다.
- 공정성은 회전 자신의 단계와 이미 보낸 효과의 결과·정산에만 둔다. 사람·에이전트·provider의 선택과 외부 kill에는 두지 않는다.

`Fix*`와 두 호스트 상수가 각각 수리 하나를 켠다. 모두 FALSE면 design-1의 동작이다.

| 상수 | 수리 | 리뷰 결함 |
|---|---|---|
| FixRelease | 배리어 뒤 포기는 단계별 목적지로 배리어를 푼다 | design-2 P1-1 |
| FixLatch | 래치는 요청을 보내는 순간 소진한다 | design-2 P1-2 |
| FixRecreate | 예약 출처는 재생성 분기 대신 미전송으로 끝난다 | design-2 P1-3 |
| FixDirect | `/tui/send`·passthrough·Herdr 설정 키는 배리어 동안 거절한다 | design-2 P1-3, A2 |
| FixSched | 미발화 CLI 예약이 있으면 그 세대는 자격이 없다 | design-2 P1-4, P2-1 |
| FixOwner | O가 출력을 맡는 채널만 자격이 있다 | design-2 P1-5 |
| FixUnknown | 불명 `/exit`·기동은 정산까지 배리어를 유지한다 | P1-1 |
| FixHold | seed의 호스트 입력 보류는 판정 뒤에만 푼다 | P1-3 |
| FixNotice | 안내는 Owner 의도와 영구 dedupe 키로 남긴다 | P1-7 |
| FixClear | clear는 시작한 효과를 기다리고 한 Owner 커밋으로 회전과 대기 입력을 지운다 | A1, P1-2 |
| FixSelf | 고정 New UUID의 자기 선택 이동은 외부 변경이 아니다 | A3 |
| FixCas | 마지막 선택 게시는 자기 집합에서만 CAS한다 | P1-2 |
| FixLatchKey | 래치는 요청한 UUID마다 남는다 | P1-5 |
| NativeGate·HostQueuesExit | 실행 중 턴에 친 slash 명령을 CLI가 턴 뒤로 미루는 호스트만 자격이 있다 | P1-4 |

## 결과 (`scripts/ci/statecharts_tlc.py`, workers 1)

| cfg | 수리 | 깨진 규칙 | 시간 |
|---|---|---|---|
| Before | 모두 끔, 명시 명령·clear·외부 선택 켬 | ClearedNotDelivered, InputExactlyOneSession, LatchDurable, LatchOnlyIfAsked, NoOverwriteExternal, NoScheduleLoss, NoStrayEffect, NoUnflushedCut, SingleExecution | 49s |
| NoRelease | FixRelease만 끔 | BarrierReleases, QueuedLeaves | 15s |
| LatchAtReserve | FixLatch만 끔 | LatchOnlyIfAsked | 4s |
| ReservedRecreate | FixRecreate만 끔 | InputExactlyOneSession | 3s |
| DirectUngated | FixDirect만 끔 | SeedFirst | 26s |
| ScheduleIgnored | FixSched만 끔 | NoScheduleLoss | 14s |
| LegacyOwned | FixOwner만 끔 | NoUnflushedCut | 4s |
| EffectUnknownReleased | FixUnknown만 끔 | NoStrayEffect, SingleExecution | 8s |
| HoldIgnored | FixHold만 끔 | NoHoldBypass | 5s |
| NoticeBestEffort | FixNotice만 끔 | NoDuplicatePost, SeedFirst | 4s |
| ClearSplit | FixClear만 끔 | ClearedNotDelivered, SingleExecution | 11s |
| SelfCancel | FixSelf만 끔 | NoSelfCancel | 4s |
| SelectorOverwrite | FixCas만 끔 | NoOverwriteExternal | 11s |
| LatchCurrentOnly | FixLatchKey만 끔 | NoRepeatedRequest | 41s |
| NativeUnsafe | NativeGate·HostQueuesExit 끔 | NoTurnCut | 17s |
| **FixedCore** | 모두 켬, 회전 밖 경로 끔, crash 1·kill 1 | 없음 (규칙 16개 + liveness 3개) | 37s |
| **FixedBypass** | 모두 켬, 입력 우회 켬 | 없음 | 100s |
| **FixedControl** | 모두 켬, 명시 명령·clear·외부 선택 켬, kill 1 | 없음 | 136s(kill 2 때), 부하 중 kill 1 244s |
| LatchKeyFixed | 모두 켬, 외부 선택 2회(A → B → A) | 없음 (NoRepeatedRequest) | 49s |
| **Small** | 모두 켬, crash 0·kill 0 | 없음 | 11s |
| Witness | 모두 켬 | W_ClearDuringRotation, W_ReleasedToOther, W_StopUnknownThenOld (도달, 기대) | 13s |
| WitnessSeed | 모두 켬 | W_NewOrdinaryAfterSeed (도달, 기대) | 4s |
| WitnessNotice | 모두 켬 | W_ReleasedToNewAnnounced (도달, 기대) | 4s |
| D1Held | design-1 원문 | W_HeldWithInput, W_HeldNoSession, W_StopPermitLost (도달, 기대) | 바뀌지 않음 |

변형 cfg는 그 수리가 지키는 규칙만 검사한다. 전체 규칙은 Fixed 셋과 Small이 본다.
상태 수는 FixedCore 25,020(깊이 31), FixedBypass 118,312(38), FixedControl 100,384(34), Small 3,133이다. design-2 Fixed(313,896)보다 작다.
Before는 깨지는 규칙만 나열하고 입력 우회 경로를 끈다. 둘 다 켜면 상태가 300만을 넘어 300초 안에 끝나지 않는다. 우회 입력의 턴 절단·seed 전 입력은 NativeUnsafe·DirectUngated가, liveness는 NoRelease가 본다.

## 반례와 코드 대조

- **EffectUnknownReleased NoStrayEffect**: `/exit` 결과가 불명인데 배리어를 풀고 Old를 돌려준다. 뒤늦게 `/exit`가 도착해 이미 넘긴 Old를 끝낸다.
  `src/services/claude/session_handoff.rs:30-35`의 Unclear와 `EXIT_WAIT` 초과는 결과를 모르는 상태다. 정산하기 전에는 풀면 안 된다.
- **EffectUnknownReleased SingleExecution**: 기동 결과가 불명인데 Old로 풀어 재기동한다. 늦게 생긴 New pane과 Old가 함께 돈다.
- **HoldIgnored NoHoldBypass**: seed가 Herdr 입력 보류에 걸렸는데 해제가 보류를 건너뛰고 다음 입력을 넘긴다.
  `src/services/claude/herdr_turn/first_input.rs:298-364`의 보류는 `hold_resolve` 판정 전까지 다른 입력을 받지 않는다.
- **NoticeBestEffort**: 안내를 TTL 키로 보내면 재시작 뒤 같은 안내가 다시 나가고(NoDuplicatePost), 게시 전에 풀면 안내 없이 New가 입력을 받는다(SeedFirst).
- **ClearSplit ClearedNotDelivered**: `/clear`가 시작한 seed·기동을 기다리지 않고 지우면, 지운 입력이 새 세션에 들어간다.
  clear는 전환 잠금을 3초 기다린 뒤 넘어서고(`src/services/discord/commands/control.rs:566-574`), Owner clear(`:597`)를 효과 대기(`control/clear_teardown.rs:79`)보다 먼저 커밋한다. 그래서 회전 효과는 clear epoch로 허가받고, 결과를 커밋하기 전에 epoch를 다시 본다.
- **SelfCancel NoSelfCancel**: 고정 New UUID의 SessionStart가 `Bind` 전에 선택을 옮기면, 외부 변경으로 보고 회전을 스스로 버린다.
- **SelectorOverwrite NoOverwriteExternal**: 다른 경로가 고른 제3 UUID를 마지막 게시가 New로 덮는다.
- **LatchCurrentOnly NoRepeatedRequest**: 래치가 현재 UUID 하나만 기억하면 A → B → A에서 A에 요청이 두 번 간다.
- **NativeUnsafe NoTurnCut**: 실행 중 턴에 친 `/exit`를 CLI가 바로 처리하면 사람의 pane 직접 입력 턴이 끊긴다.
- **DirectUngated SeedFirst**: `/tui/send`가 배리어를 무시하면 seed 전에 New가 입력을 받는다. 턴 절단은 HostQueuesExit가 막아 더는 나오지 않는다.
- **ScheduleIgnored NoScheduleLoss**: 예약이 남은 Old를 끝내면 세션 전용 예약이 사라진다.
- **NoRelease**: design-2와 같다. 배리어 뒤 외부 종료가 `Held`로 영구 보류된다.

## 모델을 돌리며 고친 설계

1. 사람 턴이 `/exit` 창에서 열리면 O 부채가 남는다. O 출력은 종료 뒤에도 파일에서 배출되므로 막지 않는다. Legacy 부채만 위반으로 센다.
2. 사람 턴이 `/exit` 창에서 예약을 만들면 사라진다. 막을 수 없어 남는 위험으로 두고, 규칙은 회전이 고른 종료만 본다.
3. 기동 직전 선택을 다시 읽지 않으면 외부 선택과 New가 함께 돈다. `LaunchDispatch`가 선택을 다시 읽는다.
4. 불명 `/exit`가 clear·외부 선택 뒤 재기동한 Old에 도착한다. kill·선택·clear가 대기 중인 `/exit`를 무효로 만든다(같은 프로세스가 아니면 도착하지 않는다).
5. clear 중 seed 완료가 지운 입력을 새 세션에 넘긴다. clear 중에는 seed 완료·해제·기동·seed 전송을 거절한다.
6. 자기 SessionStart로 회전을 버린다. 고정 UUID 게시는 자기 집합(OwnSel)으로 본다.

## 가정과 남는 위험

- 사람이 `/exit` 창에서 직접 친 턴은 끊기지 않지만(HostQueuesExit), 그 턴이 만든 세션 전용 예약·배경 작업은 사라질 수 있다.
- HostQueuesExit는 가정이다. 이 성질이 없는 호스트·CLI 버전은 자격이 없다(설계 [U] 스파이크).
- 외부 선택은 `/exit`·기동이 진행 중이거나 기동이 불명일 때 일어나지 않는다고 본다. 세션 전환 잠금이 이를 보장한다는 가정이다.
- Owner는 입력 시도를 한 번에 하나만 둔다(`NoFlight`).
- 외부 kill·선택은 cfg마다 1~2회, crash는 1회가 경계다. 무한 반복 kill에서는 진행을 주장하지 않는다.
