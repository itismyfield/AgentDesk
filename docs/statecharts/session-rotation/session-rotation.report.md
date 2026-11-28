# 전사 크기 세션 회전 (#8472 design-2)

코드 기준은 `5f0f2803e8`이다. `adeb172ff4`와 `src/` 차이가 없어 design-1의 좌표가 그대로 맞는다.
설계 문서는 이 폴더 밖(레인 산출물)에 있고, 여기에는 모델과 검사 결과만 둔다.

## 무엇을 모델링하나

한 채널의 Claude 세션 하나를 Old에서 New로 한 번 회전한다. 흐름은 다음과 같다.

1. 회전 예약(`Reserve`)
2. 인수인계 요청 전송(`AskDispatch`, 이때 래치 소진)
3. 인수인계 API(`Stage`)
4. 요청 턴 끝
5. 입력 배리어(`Barrier`)
6. `/exit`(`StopDispatch`·`StopResolve`)
7. 새 UUID 기동(`LaunchDispatch`·`LaunchResolve`)
8. seed(`SeedDispatch`·`SeedDone`)

배리어 뒤 실패는 단계마다 자동 해제(`Release`)로 끝난다.

- `Gens = {Old, New, Other}`: Other는 회전이 고르지 않은 제3 UUID다. warm follow-up 재생성, in-pane `/clear`, prompt-too-long 새 세션이 여기로 간다.
- 회전 밖 경로도 행동으로 넣었다.
  - `ExternalKill`: kill-tmux, force-kill, idle-kill, Terminate, 실행 lease 상실
  - `ExternalReplace`: 제3 UUID 교체
  - `Deliver`의 재생성 분기
  - `DirectInput`: `/tui/send`, slash passthrough
  - `FireSchedule`·`ToggleSchedule`: ScheduleWakeup·/loop·CronCreate
  - `UserControl`: Cancel·stop·clear·Terminate·effort·resume
  - `Crash`·`Boot`
- `owned`는 채널 출력을 O가 맡는지(Committed이고 Undelivered가 게시되는지)를 나타낸다. 아니면 Legacy watcher가 출력을 맡는다.
- 공정성은 회전 자신의 단계와 이미 보낸 효과의 결과 도착(성공이든 실패든)에만 둔다. 사람·에이전트·provider의 선택과 외부 kill에는 두지 않는다.

`Fix*` 상수 여섯 개가 각각 design-2 수리 하나를 켠다. 모두 FALSE면 design-1의 동작이다.

| 상수 | 수리 | 리뷰 결함 |
|---|---|---|
| FixRelease | 배리어 뒤 포기는 효과가 끝난 뒤 단계별 목적지(Old/New/Other)로 배리어를 푼다 | P1-1 |
| FixLatch | 래치는 요청을 보내는 순간 소진한다. 미지원·도착 포기는 소진하지 않는다 | P1-2 |
| FixRecreate | 예약 출처(요청·seed)는 재생성 분기 대신 미전송으로 끝난다 | P1-3 |
| FixDirect | `/tui/send`·passthrough는 배리어 동안 거절한다 | P1-3 |
| FixSched | 미발화 CLI 예약이 있으면 그 세대는 자격이 없다 | P1-4 |
| FixOwner | O Committed이고 Undelivered가 게시되는 채널만 자격이 있다 | P1-5 |

## 결과 (`scripts/ci/statecharts_tlc.py`, workers 1)

| cfg | 수리 | 깨진 규칙 | 시간 |
|---|---|---|---|
| D1Held (design-1 원문 + 증인) | — | W_HeldWithInput, W_HeldNoSession, W_StopPermitLost (도달, 기대) | 160s |
| Before | 모두 끔 | LatchOnlyIfAsked, InputExactlyOneSession, NoUnflushedCut, NoScheduleLoss, NoTurnCut, SeedFirst, BarrierReleases, QueuedLeaves | 40s |
| NoRelease | P1-1만 끔 | BarrierReleases, QueuedLeaves | 150s |
| LatchAtReserve | P1-2만 끔 | LatchOnlyIfAsked | 10s |
| ReservedRecreate | P1-3 재생성만 끔 | InputExactlyOneSession | 10s |
| DirectUngated | P1-3 `/tui/send`만 끔 | NoTurnCut, SeedFirst | 26s |
| ScheduleIgnored | P1-4만 끔 | NoScheduleLoss, NoTurnCut | 19s |
| LegacyOwned | P1-5만 끔 | NoUnflushedCut | 23s |
| **Fixed** | 모두 켬, crash 1·kill 1 | 없음 (규칙 11개 + liveness 3개) | 169s |
| **Small** | 모두 켬, crash 0·kill 0 | 없음 | 22s |
| Witness | 모두 켬 | W_AbortBeforeAsk, W_ReleasedToOld, W_ReleasedToOther (도달, 기대) | 12s |
| WitnessSeed | 모두 켬 | W_NewOrdinaryAfterSeed (완료 뒤 New가 일반 입력 수락, 기대) | 17s |
| WitnessNotice | 모두 켬 | W_ReleasedToNew (seed 불명 → 안내 1건 → New 해제, 기대) | 16s |

변형 cfg는 그 수리가 지키는 규칙만 검사한다. 전체 규칙은 Fixed와 Before가 본다.
Fixed는 313,896 상태, 깊이 31이다.
일반 입력 두 건(Ordinary = {o1, o2})으로는 레인에서만 돌렸고 커밋하지 않았다. 안전 규칙 11개는 3,458,412 상태에서 위반이 없었다. liveness는 10분 제한 안에 끝나지 않았다.

## 반례와 코드 대조

- **NoRelease BarrierReleases (19단계)**: 배리어 커밋 → 명시 명령 → `Held`. design-1의 `Abandoned(held)`와 같은 상태다.
  코드에는 이 상태를 푸는 행동이 없다. design-1 §1.5 "명시 복구 처분"은 어느 조각에도 정의되지 않았다.
- **NoRelease QueuedLeaves (15단계)**: 배리어 → 외부 종료 → 입력 도착 → 영구 보류.
  외부 종료 경로는 `src/server/routes/dispatched_sessions.rs:116-139`와 `policies/timeouts/idle-kill.js:124`다.
- **ReservedRecreate (3단계)**: 요청 전달이 재생성 분기를 타서 Other에 들어간다.
  `src/services/claude_tui/hosting/warm_followup.rs:168-183`에서 `LegacyRecreateEligible`이면 `host.retire` 뒤 새 UUID를 만든다.
- **DirectUngated NoTurnCut (10단계)**: `/exit` 대기 중에 `/tui/send`가 Old 턴을 연다.
  `src/services/claude_tui/tui_relay.rs:219-240`은 composer lock만 잡는다.
- **ScheduleIgnored**: 예약이 남은 Old를 끝내면 예약이 사라지고, `/exit` 창에서 발화하면 턴이 끊긴다.
  CronCreate는 세션 전용(메모리)이고 REPL이 유휴일 때만 발화한다(현 Claude Code 도구 설명 기준, 버전에 따라 다를 수 있다).
  발화한 행은 `src/services/tui_prompt_dedupe/extract.rs:106-113`이 표시를 보고 안다.
- **LegacyOwned (8단계)**: Legacy 채널은 O의 부채가 0이라 배리어를 통과하고 `/exit`가 Legacy 꼬리를 끊는다.
  `src/services/tui_o/channel_policy/adoption.rs:25-44`, `src/services/tui_o/writer/actor.rs:458-474`.
- **LatchAtReserve (1단계)**: 예약만으로 래치가 소진된다. design-1 `ReserveUnsupported`/`Reserve`와 같다.

## 가정과 남는 위험

- **in-pane 사람 입력은 모델에 없다.** 사람이 `/exit` 대기 창(최대 `EXIT_WAIT` 20초, `src/services/claude/session_handoff.rs:15`)에 직접 타이핑하면 그 턴은 끊길 수 있다. 배리어 전 입력은 Old 턴을 열어 `StopDispatch` 가드에 걸린다. 설계 문서에 남은 위험으로 적었다.
- Owner는 입력 시도를 한 번에 하나만 둔다(`NoFlight`). 기존 V2 attempt 직렬화를 추상화한 것이다.
- `/tui/send` 배리어 판정은 composer lock 아래에서 읽는다. 부팅 때는 회전 카탈로그를 읽기 전까지 거절한다고 가정한다(설계 [D]).
- 출력은 부채 플래그 두 개(O·Legacy)로만 본다. 게시 단위의 정확히 한 번은 기존 tui-o-writer 모델이 맡는다. 회전은 새 게시 정책을 만들지 않는다.
- 외부 kill·교체는 한 번, crash는 한 번이 경계다. kill이 무한히 반복되면 진행을 주장하지 않는다.
