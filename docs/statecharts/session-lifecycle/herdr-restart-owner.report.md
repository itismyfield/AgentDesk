# Herdr `/restart` r2 상태 모델: seed를 큐 head에, launch는 owner 턴

기준 코드: `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba` (main). 생산 코드 변경은 없다.
설계는 [`../../herdr-parity-restart.design.md`](../../herdr-parity-restart.design.md) 3절(r2)이다.
r1 모델 [`HerdrRestart`](herdr-restart.report.md)는 restart가 직접 launch하는 방향을 검사하며 그대로 두었다.
A 레인이 그 파일들에 Codex resume 경계를 반영하는 중이라 제자리에서 고치지 않고, 채택안은 새 모듈 `HerdrRestartOwner`로 검사한다.

아래에서 `src/` 없이 쓴 짧은 파일 이름(`owner_input.rs`, `restart.rs`, `control.rs`, `herdr_terminate.rs`, `headless_turn.rs` 등)은 설계 3절 머리의 대응표를 따른다.

**검증 종류:** TLC가 설계 포트를 가정했을 때의 불변식·진행성 검사다. Rust/DB/실제 herdr 시험, 실제 경합 재현, 운영 수용 PASS를 뜻하지 않는다.

## 진입점에서 가져온 값 범위

| 모델 값 | 코드의 값·행동과 좌표 | 축약·제외 |
|---|---|---|
| `Policy={before,after}` | 현재 거절 `src/services/discord/commands/restart.rs:219`; after는 설계 3.1 | before는 아무것도 바꾸지 않고 거절 |
| `StartKind={Bound,PendingPlaced,PendingUnplaced,Retired,Missing,Legacy,Unknown}` | `src/db/dispatched_sessions/hosted_execution.rs:27`, `:171`, `:228`; 종료 서비스의 거절 조건 `src/services/discord/herdr_terminate.rs:450`~`:463`, `:512` | Conflict·다른 owner/node는 Unknown의 거절에 합쳤다. Legacy의 살아 있는 tmux(이주 분기)는 모델 밖 |
| `row`의 `NewPending/NewBound`, `created={no,yes,notsent,unknown}`, `creates`, `panes` | owner 턴의 launch `src/services/herdr_launch.rs:378`, Retired 위 설치 `:433`~`:449`, Pending CAS `:482`, 결과 `:111`, `:210` | 응답 불명은 pane이 생긴 최악으로 고정. evidence gap은 unknown에 합쳤다 |
| `Dispatch` | `active_dispatch_id`와 disconnect 조건 `src/db/dispatched_sessions/hosted_execution/http_force.rs:21`, `:55` | fence 뒤에 dispatch가 붙는 경합은 `IgnoreDispatch` 변형으로만 본다 |
| `CloseReply={confirmed,Indeterminate,Refused}` | `src/services/discord/herdr_terminate.rs:74`, 순서 `:482`, `:548`, `:590`, `:594`, `:596`, `:603`; nonce 고정 `:453` | 실패는 한 갈래. 불명 뒤 옛 pane은 살아 있다고 본다 |
| `seed={none,queued,claimed,delivered,dropped,cancelled,withdrawn,stripped}` | owner 큐 head 적재 `src/services/turn_orchestrator/queued_head/v2/source_admission.rs:918`, `:971`; front 시작 `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:835`, `:912` | `dropped`는 시작 실패의 retained + 알림, `stripped`는 변형 전용 |
| `gate`, `excl`, `awake` | 대기 분기 `owner_input.rs:859`, transition `src/services/discord/herdr_terminate.rs:482`, wake `owner_input.rs:602` | gate는 설계가 더하는 프로세스 메모리. 깨어난 drive가 막히면 다음 wake까지 쉰다 |
| `ClearOn={off,serial,overtake}`, `clear`, `clearSaw`, `selector` | `src/services/discord/commands/control.rs:524`, `:540`, `:552`, `:599`; `control/herdr_recovery.rs:54`, `:58`, `:115`, `:158`; `control/clear_teardown.rs:161`, `:183`, `:202` | serial은 transition을 쥔 clear, overtake는 restart가 쥔 동안 와서 3초 뒤 지나간 clear. guard 없는 폐기~삭제 사이는 한 단계로 묶었다 |
| `CrashOn`, `Rollback`, `binary` | 부팅 순서 `src/services/discord/runtime_bootstrap/queued_recovery.rs:48`, census `src/services/turn_orchestrator/queued_head/compat.rs:33`; 옛 판정 `src/services/discord/router/message_handler/headless_turn.rs:159` | crash는 수락 뒤 아무 단계에서나 한 번. 진행 중이던 clear와 겹친 crash는 제외 |
| `q={incoming,queued,delivered,cancelled,retained,lost}`, `effects`, `receipts` | `SnapshotV2.items/retained_inputs/owed_drop_receipts` (`src/services/turn_orchestrator/queued_head/v2_types.rs:20`) | `N=1/2`. `lost`와 두 번째 effect는 변형 검출용 |

## 수정 전/후 비교

| 항목 | Before: main | After: r2 설계 |
|---|---|---|
| healthy herdr `/restart` | 취소 전에 거절, 옛 세션 유지 | seed 적재 → exact close → owner 턴이 successor를 띄우고 seed가 첫 입력 |
| `RestartAvailable` | 새 Ready 없음: 기대 RED | GREEN |
| 대기 입력 | 그대로 | seed 뒤에서 기다렸다가 successor에 한 번씩. 실패로 끝난 재시작에서는 살아 있는 세션 또는 다음 launch로 간다 |
| 실패 뒤 입력 | 해당 없음 | "보이는 실패"만으로 결산된 것으로 세지 않는다. 입력마다 전달·receipt 있는 폐기·알림 있는 보류 중 하나여야 한다 |
| dispatch 실행 중 | 거절(모든 경우) | 종료 전 거절, close 0 |
| restart 중 `!clear` | 해당 없음 | 수락 뒤 폐기를 커밋한 clear는 seed를 지우고, selector는 clear 또는 restart postlude가 지운다 |
| crash | 해당 없음 | 수락된 요청은 Ready 또는 보이는 실패. 무응답 종료 없음 |
| 옛 바이너리로 롤백 | 해당 없음 | seed는 `executor_not_wired`로 보이게 종결 |

## 검사할 규칙

불변식:
- `InputCustody`, `InputAtMostOnce`: 접수 원본은 큐·전달·receipt 있는 폐기·알림 있는 보류 중 하나이고 effect는 최대 1회다.
- `NoOldPaneInput`: fence 뒤 옛 pane 전달 0.
- `NoFalseReady`: 인사말은 끝내려던 실행으로 가지 않는다(gate가 사라진 부팅 뒤에도).
- `SeedAtMostOnce`, `NoBlindCreate`: seed와 create는 응답 불명 뒤에도 반복하지 않는다.
- `SingleSession`: 옛 pane과 새 pane의 합은 언제나 1 이하다.
- `InflightSettledBeforeReady`: Ready 전에 옛 턴이 결산되고 busy 경고가 있었다.
- `NoCloseUnderDispatch`, `NoInheritedDispatch`: dispatch가 있으면 닫지 않고, successor가 옛 dispatch를 물려받지 않는다.
- `NoResumeAfterClear`: 큐 전체를 폐기한 clear 뒤에는 어떤 launch도 그 대화를 resume하지 않는다.
- `NoSeedRevival`: clear가 버린 seed를 다시 적재하지 않는다.
- `ClearNeverHeld`: 기존 결함을 드러내는 cfg에서만 검사한다.

진행성:
- `RequestResolves`: 수락된 요청은 결국 Ready, 보이는 실패, clear의 폐기 receipt 중 하나로 끝난다.
- `RefusalVisible`: 수락 전 거절은 결국 보인다.
- `RestartAvailable`: healthy면 새 Ready여야 한다. Before의 안전성 GREEN을 기능 PASS로 세지 않기 위한 규칙이다.
- `AcceptedInputResolves`: 접수 원본마다 결국 전달·receipt 있는 폐기·알림 있는 보류로 끝난다.
- `ClearForgets`: 큐 전체를 폐기한 clear 뒤 selector는 결국 지워진다.

## 결과

저장소 checker `scripts/ci/statecharts_tlc.py`로 42 cfg를 돌렸다: **GREEN 20, 기대 RED 22, 예상 위반 집합 전부 일치**.
cfg마다 깨진 규칙을 빼고 다시 돌려 전체 위반 집합을 얻는 방식이며 미검사 규칙은 없다.

| cfg | 조건 | 실제 깨진 규칙 | 판정 |
|---|---|---|---|
| [After](HerdrRestartOwnerAfter.cfg) | Bound, busy | — | GREEN |
| [Concurrent](HerdrRestartOwnerConcurrent.cfg) | N=2 도착 경합 | — | GREEN |
| [StartPendingPlaced](HerdrRestartOwnerStartPendingPlaced.cfg) | location 있는 Pending | — | GREEN |
| [StartRetired](HerdrRestartOwnerStartRetired.cfg) | 종료 없이 seed + wake | — | GREEN |
| [StartMissing](HerdrRestartOwnerStartMissing.cfg) | 종료 없이 seed + wake | — | GREEN |
| [StartLegacy](HerdrRestartOwnerStartLegacy.cfg) | 종료 없이 seed + wake | — | GREEN |
| [StartPendingUnplaced](HerdrRestartOwnerStartPendingUnplaced.cfg) | 보이는 거절 | — | GREEN |
| [StartUnknown](HerdrRestartOwnerStartUnknown.cfg) | 보이는 거절 | — | GREEN |
| [DispatchRefused](HerdrRestartOwnerDispatchRefused.cfg) | dispatch 실행 중 | — | GREEN |
| [CloseIndeterminate](HerdrRestartOwnerCloseIndeterminate.cfg) | close 불명 | — | GREEN |
| [CloseRefused](HerdrRestartOwnerCloseRefused.cfg) | close 거절 | — | GREEN |
| [WithdrawFallback](HerdrRestartOwnerWithdrawFallback.cfg) | 철회가 빠져도 permit이 종결 | — | GREEN |
| [LaunchNotSent](HerdrRestartOwnerLaunchNotSent.cfg) | create 미전송 | — | GREEN |
| [LaunchUnknown](HerdrRestartOwnerLaunchUnknown.cfg) | create 응답 불명 | — | GREEN |
| [ClearSerial](HerdrRestartOwnerClearSerial.cfg) | transition을 쥔 clear, N=2 | — | GREEN |
| [ClearOvertake](HerdrRestartOwnerClearOvertake.cfg) | 3초 뒤 지나간 clear, N=2 | — | GREEN |
| [Crash](HerdrRestartOwnerCrash.cfg) | 아무 단계 crash, N=2 | — | GREEN |
| [CrashRollback](HerdrRestartOwnerCrashRollback.cfg) | crash 뒤 옛 바이너리 | — | GREEN |
| [CrashCloseRefused](HerdrRestartOwnerCrashCloseRefused.cfg) | 실패 응답 전후 crash | — | GREEN |
| [CrashLaunchUnknown](HerdrRestartOwnerCrashLaunchUnknown.cfg) | create 불명 전후 crash | — | GREEN |
| [Before](HerdrRestartOwnerBefore.cfg) | main의 거절 | `RestartAvailable` | 기대 RED |
| [DropQueue](HerdrRestartOwnerDropQueue.cfg) | retire가 큐를 purge | `InputCustody`, `AcceptedInputResolves` | 기대 RED |
| [OldInput](HerdrRestartOwnerOldInput.cfg) | fence 중 옛 pane 입력 | `NoOldPaneInput` | 기대 RED |
| [NoGate](HerdrRestartOwnerNoGate.cfg) | gate 없이 적재 | `RestartAvailable` | 기대 RED |
| [NoPermit](HerdrRestartOwnerNoPermit.cfg) | permit 없이 부팅 | `NoFalseReady`, `RequestResolves` | 기대 RED |
| [SkipClose](HerdrRestartOwnerSkipClose.cfg) | 종료 관측 없이 retire | `SingleSession` | 기대 RED |
| [NoSettle](HerdrRestartOwnerNoSettle.cfg) | 옛 턴 결산 생략 | `InflightSettledBeforeReady` | 기대 RED |
| [ReplayInput](HerdrRestartOwnerReplayInput.cfg) | 입력 receipt 망각 | `InputAtMostOnce` | 기대 RED |
| [ReplaySeed](HerdrRestartOwnerReplaySeed.cfg) | 부팅 뒤 seed 재제출 | `SeedAtMostOnce` | 기대 RED |
| [ReplayUnknown](HerdrRestartOwnerReplayUnknown.cfg) | 불명 create 재전송 | `NoBlindCreate`, `SingleSession`, `AcceptedInputResolves` | 기대 RED |
| [NoWake](HerdrRestartOwnerNoWake.cfg) | 해제 뒤 wake 없음 | `RequestResolves`, `RestartAvailable`, `AcceptedInputResolves` | 기대 RED |
| [NoNotice](HerdrRestartOwnerNoNotice.cfg) | 시작 실패 알림 없음 | `RequestResolves` | 기대 RED |
| [NoNoticeRefusal](HerdrRestartOwnerNoNoticeRefusal.cfg) | 거절 응답 없음 | `RefusalVisible`, `RestartAvailable` | 기대 RED |
| [IgnoreDispatch](HerdrRestartOwnerIgnoreDispatch.cfg) | dispatch 검사 없음 | `NoCloseUnderDispatch`, `NoInheritedDispatch` | 기대 RED |
| [NoSupersedeCheck](HerdrRestartOwnerNoSupersedeCheck.cfg) | postlude의 seed 확인 없음 | `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [ClearSkipsRetired](HerdrRestartOwnerClearSkipsRetired.cfg) | `end_herdr`의 현재 Retired 분기 | `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [ReloadSeed](HerdrRestartOwnerReloadSeed.cfg) | clear 뒤 seed 재적재(종료 → 적재 순서) | `NoSeedRevival`, `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [R1Launch](HerdrRestartOwnerR1Launch.cfg) | r1: exclusion 안에서 launch | `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [IntentField](HerdrRestartOwnerIntentField.cfg) | 별도 V2 필드 + 롤백 | `RequestResolves` | 기대 RED |
| [WithdrawFirst](HerdrRestartOwnerWithdrawFirst.cfg) | 응답 전에 철회 + crash | `RequestResolves` | 기대 RED |
| [GapClearDuringLaunch](HerdrRestartOwnerGapClearDuringLaunch.cfg) | **기존 결함**: launch 중 clear | `ClearNeverHeld` | 기대 RED |
| [GapClearCloseFailure](HerdrRestartOwnerGapClearCloseFailure.cfg) | **기존 계약**: 종료 실패 + clear timeout | `ClearForgets` | 기대 RED |

규칙마다 그것을 깨는 cfg: `InputCustody`(DropQueue), `InputAtMostOnce`(ReplayInput), `NoOldPaneInput`(OldInput), `NoFalseReady`(NoPermit),
`SeedAtMostOnce`(ReplaySeed), `NoBlindCreate`(ReplayUnknown), `SingleSession`(SkipClose), `InflightSettledBeforeReady`(NoSettle),
`NoCloseUnderDispatch`·`NoInheritedDispatch`(IgnoreDispatch), `NoResumeAfterClear`·`NoSeedRevival`(ReloadSeed 외 3), `ClearNeverHeld`(Gap),
`RequestResolves`(NoWake, NoNotice, IntentField, WithdrawFirst), `RefusalVisible`(NoNoticeRefusal), `RestartAvailable`(Before, NoGate),
`AcceptedInputResolves`(DropQueue, NoWake), `ClearForgets`(NoSupersedeCheck, ClearSkipsRetired).

## 모델이 설계를 바꾼 곳

- **종료의 nonce 고정.** 첫 실행에서 `ClearSerial`의 `AcceptedInputResolves`가 깨졌다. clear가 옛 실행을 먼저 끝내고 새 입력이 successor를 띄운 뒤
  restart의 종료가 그 successor의 Pending을 retire하는 경로였다. 실제 서비스는 요청한 nonce와 row의 nonce가 다르면 거절한다
  (`src/services/discord/herdr_terminate.rs:453`). 모델의 종료를 그 조건에 맞췄고, 설계 3.1 6단계가 nonce 불일치를 실패로 적는다.
- **응답 뒤 철회.** 실패 때 seed를 먼저 지우고 답하면 그 사이 crash가 결과 없는 종료가 된다(`WithdrawFirst`). 설계 3.1 9단계는 답을 먼저 한다.
- **적재가 종료보다 먼저.** 종료 뒤에 적재하면 clear가 지나간 뒤 seed가 살아남는다(`ReloadSeed`). retire 뒤·적재 전 crash도 흔적 없이 끝난다.
- **clear 쪽 보강 한 곳.** `end_herdr`가 Retired row를 그냥 지나치면(`src/services/discord/commands/control/clear_teardown.rs:161`)
  clear가 Bound를 읽은 뒤 restart가 retire한 경우 selector가 남는다(`ClearSkipsRetired`).

## 반례와 실제 코드 대조

- [Before: `RestartAvailable`](herdr-restart-owner-Before.report.md): `restart.rs:219`의 거절이 취소보다 먼저다. 정적 추적으로 확인한 현재 동작이다.
- [NoSupersedeCheck](herdr-restart-owner-NoSupersedeCheck.report.md): restart가 transition을 쥔 동안 clear가 큐를 폐기하고, 자기 종료는 timeout으로 끝난다
  (`control.rs:524`, `herdr_terminate.rs:483`, `clear_teardown.rs:202`). postlude가 seed를 확인하지 않으면 selector가 남아 다음 입력이 clear 전 대화를 resume한다.
- [R1Launch](herdr-restart-owner-R1Launch.report.md): 리뷰 d1의 P2-1과 같은 반례다. exclusion 안에서 띄운 successor를 clear가 끝낼 수 없다.
- [NoPermit](herdr-restart-owner-NoPermit.report.md): 적재 뒤·close 전 crash에서 gate가 사라진다. permit이 없으면 인사말이 살아 있는 옛 세션으로 간다.
- [IntentField](herdr-restart-owner-IntentField.report.md): `SnapshotV2`에는 `deny_unknown_fields`가 없어(`v2_types.rs:19`) 옛 바이너리가 새 필드를 읽지 않고 다시 쓸 때 지운다.
- [GapClearDuringLaunch](herdr-restart-owner-GapClearDuringLaunch.report.md): Retired row + 활성 턴에서 clear는 보류를 보고한다(`control/herdr_recovery.rs:158`).
  일반 메시지의 launch에서도 같은 기존 동작이며 이 설계가 만들거나 넓히지 않는다.

변형은 생산 코드를 바꾸거나 실행한 결과가 아니다. 구현 레그에서 같은 진입점과 fault 주입으로 RED→GREEN→원인 뮤턴트 RED를 확인해야 한다.

## 포트 가정과 한계

1. seed의 head 적재·gate·permit·철회, postlude의 seed 확인, `end_herdr` Retired 보강은 아직 없는 구현이다. 모델은 그것들이 설계대로 있다고 가정한다.
2. seed가 owner front에서 headless 진입을 거쳐 launch에 닿는다는 것은 모델이 증명하지 않는다. herdr에서 그 진입은 지금 거절된다(`headless_turn.rs:159`).
3. clear가 guard 없이 지나갈 때 폐기와 selector 삭제 사이에 새 입력이 launch하는 경합은 한 단계로 묶어 제외했다. clear의 기존 overtake 계약에 속한다.
4. 진행 중이던 clear와 겹친 crash, 두 번 이상의 crash·restart·clear, 다중 endpoint는 제외했다.
5. launch 중 clear는 "보류"로만 모델했다. location 있는 Pending을 clear가 정확히 끝내는 경우와 unplaced의 종료 미확인을 나누지 않았다.
6. 각 I/O가 결국 답하고 기존 복구(끝난 pane의 row 정리, unplaced resolver, Pending 재확인)가 결국 돈다는 약한 공정성을 가정한다.
   Discord 전송이 영구히 실패하면 "보이는 실패"는 증명되지 않는다.
7. Codex resume 시작점과 출력 중복은 A의 범위다. 이 모델은 selector가 남았는지만 본다.

## 다시 실행

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py --base 33d1f4b5a9 --jobs 6 --java "$JAVA"
```

[정상 흐름](herdr-restart-owner-After.chart.mmd), [N=2](herdr-restart-owner-Concurrent.chart.mmd),
[clear overtake](herdr-restart-owner-ClearOvertake.chart.mmd), [crash](herdr-restart-owner-Crash.chart.mmd),
[롤백](herdr-restart-owner-CrashRollback.chart.mmd), [현재 거절](herdr-restart-owner-Before.chart.mmd).
cfg마다 `herdr-restart-owner-<suffix>.chart.mmd`와 `.report.md`가 같은 모델·[view](herdr-restart-owner.view.mjs)에서 생성됐다.
