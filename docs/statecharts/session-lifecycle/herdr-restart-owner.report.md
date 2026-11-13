# Herdr `/restart` r3 상태 모델: seed 적재 → 정확 종료 → owner 턴이 launch, clear가 이긴다

기준 코드: `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba` (main). 생산 코드 변경은 없다.
설계는 [`../../herdr-parity-restart.design.md`](../../herdr-parity-restart.design.md) 3절(r3)이다.
r1 모델 [`HerdrRestart`](herdr-restart.report.md)는 restart가 직접 launch하는 방향을 검사하며 그대로 두었다.
A 레인이 그 파일들에 Codex resume 경계를 반영하는 중이라 제자리에서 고치지 않고, 채택안은 모듈 `HerdrRestartOwner`로 검사한다.
r3에서 이 모듈을 사용자 결정 4건과 설계 리뷰 d2의 P2 5건에 맞춰 갱신했다(r2는 42 cfg였다).

아래에서 `src/` 없이 쓴 짧은 파일 이름(`owner_input.rs`, `restart.rs`, `control.rs`, `herdr_terminate.rs`, `headless_turn.rs` 등)은 설계 3절 머리의 대응표를 따른다.

**검증 종류:** TLC가 설계 포트를 가정했을 때의 불변식·진행성 검사다. Rust/DB/실제 herdr 시험, 실제 경합 재현, 운영 수용 PASS를 뜻하지 않는다.

## 진입점에서 가져온 값 범위

| 모델 값 | 코드의 값·행동과 좌표 | 축약·제외 |
|---|---|---|
| `Policy={before,after}` | 현재 거절 `src/services/discord/commands/restart.rs:219`; after는 설계 3.1 | before는 아무것도 바꾸지 않고 거절 |
| `StartKind` 일곱 값 | `src/db/dispatched_sessions/hosted_execution.rs:27`, `:171`, `:228`; 종료 서비스의 거절 조건 `src/services/discord/herdr_terminate.rs:450`~`:463`, `:512` | Conflict·다른 owner/node는 Unknown의 거절에 합쳤다. Legacy의 살아 있는 tmux(이주 분기)는 모델 밖 |
| `Dispatch`, `rowDispatch`, `retries`, `inherit` | 묶인 종료 `src/services/discord/herdr_terminate/http_force.rs:23`, `:90`; disconnect 조건 `src/db/dispatched_sessions/hosted_execution/http_force.rs:44`, `:55`; tmux의 취소 턴은 dispatch를 쓰지 않음 `src/services/discord/turn_bridge/post_loop_finalize.rs:437`, `:439` | fence 전 dispatch 교체와 row·턴 dispatch 불일치는 모델 밖(설계 3.3 표) |
| `SeedMode={ok,none}` | 적재 거절 사유: 큐 여유 `src/services/turn_orchestrator/queued_head/v2/effort_continue.rs:200`, V2 미채택·commit 미확인 `v2/source_admission.rs:918`~`:981` | none은 "넣지 못함" 한 갈래. commit 미확인의 재확인은 모델 밖 |
| `HeadBusy`, `q`의 `inflight` | 전송 중 단위가 큐 맨 앞에 있어야 결산됨 `src/services/turn_orchestrator/queued_head/v2/retained.rs:405`, `v2.rs:259` | 단위는 입력 하나. 수락 전에 옛 실행으로 시작된 전달이다 |
| `seed` 아홉 값 | 적재 `v2/source_admission.rs:918`; front 시작 `owner_input.rs:835`, `:912`; 결산 `v2/source_admission.rs:609` | `dropped`는 시작 실패의 retained + 알림, `uncertain`은 제출 뒤·receipt 전 crash, `stripped`는 변형 전용 |
| `lphase={begun,pending,created,sent,settled}` | launch `src/services/herdr_launch.rs:378`, Pending CAS `:482`; 물리 제출 `src/services/discord/queue_io/input_owner/effect.rs:96`, receipt `:130`; source bind는 그 뒤 | Codex의 attach·시작점 기록은 A 범위라 bind 한 단계로 묶었다 |
| `epoch`, `lEpoch`, `clearing` | 설계 3.4의 clear epoch(새 포트). clear 시작 `src/services/discord/commands/control.rs:518`, 기존 취소 확인 `src/services/claude/herdr_turn.rs:252`, `src/services/codex/herdr_turn.rs:358` | clear는 한 번. epoch은 프로세스 메모리라 crash에서 사라진다 |
| `ClearOn={off,serial,overtake,launch,any}` | `control.rs:524`, `:540`, `:552`, `:599`; `control/herdr_recovery.rs:58`, `:115`, `:158`; `control/clear_teardown.rs:161`, `:183`, `:202` | clear의 종료 단계는 "row 읽기 + 처리" 한 단계. clear와 겹친 crash는 제외 |
| `CrashOn`, `Rollback`, `binary` | 부팅 순서 `src/services/discord/runtime_bootstrap/queued_recovery.rs:48`, census `src/services/turn_orchestrator/queued_head/compat.rs:33`; 옛 판정 `headless_turn.rs:159` | crash는 수락 뒤 아무 단계에서나 한 번 |
| `oldInputs`, `RestartOpen` | 설계 3.9-2. 옛 실행으로 가는 모든 전달을 관측한다 | 수락 전에 시작한 전달과 실패 확정 뒤의 전달은 허용 |

## 수정 전/후 비교

| 항목 | Before: main | After: r3 설계 |
|---|---|---|
| healthy herdr `/restart` | 취소 전에 거절 (`Before`: `RestartAvailable` RED) | seed 적재 → exact close → owner 턴이 launch, seed가 첫 입력 (GREEN) |
| dispatch 실행 중 | 거절 | 재시작. dispatch 상태를 쓰지 않고 재시도 0, successor가 물려받지 않음 |
| launch 중 `!clear` | 보류만 보고하고 launch가 계속된다 (`MainClear`: `NoStaleCreate`·`NoSeedAfterClear`·`NoSessionAfterClear`·`ClearForgets` RED) | clear가 이긴다 (`ClearDuringLaunch`, `ClearAny` GREEN) |
| seed를 넣을 수 없음 | 해당 없음 | seed 없이 재시작, 대기 입력이 successor를 띄움 (`Seedless` GREEN) |
| 전송 중 단위 | 해당 없음 | seed는 그 뒤. 단위의 결산이 막히지 않음 (`HeadBusy` GREEN) |
| crash | 해당 없음 | seed가 적재된 요청은 Ready 또는 보이는 실패. 제출 뒤·receipt 전 crash의 seed는 재전송 0 |
| 진행성 | `RestartAvailable` RED | 여섯 진행성 모두 GREEN (남는 계약 cfg 제외) |

## 검사할 규칙

불변식 17개:
- `InputCustody`, `InputAtMostOnce`: 접수 원본은 큐·전달·receipt 있는 폐기·알림 있는 보류 중 하나이고 effect는 최대 1회다.
- `NoOldPaneInput`: 재시작이 열려 있는 동안(수락부터 종료 또는 실패 확정까지) 옛 실행으로 시작하는 전달 0. 정상 전달 경로도 센다.
- `NoFalseReady`: 인사말은 끝내려던 실행으로 가지 않는다(gate가 사라진 부팅 뒤에도).
- `SeedAtMostOnce`, `NoBlindCreate`: seed와 create는 불명 뒤에도 반복하지 않는다.
- `SingleSession`: 옛 pane과 새 pane의 합은 언제나 1 이하다.
- `InflightSettledBeforeReady`, `ReadyEvidence`: Ready 전에 옛 턴이 결산됐고, Ready는 seed의 receipt로만 말한다.
- `NoInheritedDispatch`, `NoDispatchRepost`: successor가 옛 dispatch를 물려받지 않고, restart가 재시도 dispatch를 만들지 않는다.
- `NoSeedAfterClear`: clear가 시작된 뒤 seed 제출 0.
- `NoStaleCreate`: clear보다 먼저 시작한 launch는 clear 뒤에 create하지 않는다.
- `NoSessionAfterClear`: clear가 끝났을 때 그런 launch가 만든 세션이 남아 있지 않다.
- `NoResumeAfterClear`: clear 뒤에 시작한 launch는 그 clear가 끝낸 대화를 resume하지 않는다.
- `NoSeedRevival`, `NoFalseSuperseded`: clear가 버린 seed를 다시 적재하지 않고, clear가 없었는데 superseded를 말하지 않는다.

진행성 6개:
- `RequestResolves`: seed가 적재된 요청은 결국 Ready, 보이는 실패, clear의 폐기 중 하나로 끝난다.
- `CommandAnswers`: 명령은 결국 답하거나 보이는 실패를 낸다(crash 제외).
- `RestartAvailable`: healthy면 새 Ready여야 한다.
- `AcceptedInputResolves`: 접수 원본마다 결국 전달·receipt 있는 폐기·알림 있는 보류로 끝난다.
- `ClearForgets`: 큐를 폐기한 clear 뒤 selector는 결국 지워진다.
- `ClearCompletes`: 시작한 clear는 결국 끝난다(launch 대기가 멈추지 않는다).

## 결과

저장소 checker `scripts/ci/statecharts_tlc.py`로 63 cfg를 돌렸다: **GREEN 28, 기대 RED 35, 예상 위반 집합 전부 일치**.
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
| [DispatchRestart](HerdrRestartOwnerDispatchRestart.cfg) | dispatch 실행 중 재시작, N=2 | — | GREEN |
| [DispatchCrash](HerdrRestartOwnerDispatchCrash.cfg) | dispatch 실행 중 + 아무 단계 crash | — | GREEN |
| [CloseIndeterminate](HerdrRestartOwnerCloseIndeterminate.cfg) | close 불명 | — | GREEN |
| [CloseRefused](HerdrRestartOwnerCloseRefused.cfg) | close 거절 | — | GREEN |
| [WithdrawFallback](HerdrRestartOwnerWithdrawFallback.cfg) | 철회가 빠져도 permit이 종결 | — | GREEN |
| [LaunchNotSent](HerdrRestartOwnerLaunchNotSent.cfg) | create 미전송 | — | GREEN |
| [LaunchUnknown](HerdrRestartOwnerLaunchUnknown.cfg) | create 응답 불명 | — | GREEN |
| [ClearSerial](HerdrRestartOwnerClearSerial.cfg) | transition을 쥔 clear, N=2 | — | GREEN |
| [ClearOvertake](HerdrRestartOwnerClearOvertake.cfg) | restart exclusion 중 지나간 clear, N=2 | — | GREEN |
| [ClearDuringLaunch](HerdrRestartOwnerClearDuringLaunch.cfg) | owner가 launch하는 중의 clear, N=2 | — | GREEN |
| [ClearAny](HerdrRestartOwnerClearAny.cfg) | 임의 시점의 clear, N=2 | — | GREEN |
| [Seedless](HerdrRestartOwnerSeedless.cfg) | seed를 넣지 못한 재시작, N=2 | — | GREEN |
| [SeedlessClear](HerdrRestartOwnerSeedlessClear.cfg) | seed 없음 + 임의 시점 clear | — | GREEN |
| [SeedlessCrash](HerdrRestartOwnerSeedlessCrash.cfg) | seed 없음 + crash | — | GREEN |
| [HeadBusy](HerdrRestartOwnerHeadBusy.cfg) | 전송 중 단위가 큐 맨 앞, N=2 | — | GREEN |
| [HeadBusyCrash](HerdrRestartOwnerHeadBusyCrash.cfg) | 전송 중 단위 + crash | — | GREEN |
| [Crash](HerdrRestartOwnerCrash.cfg) | 아무 단계 crash(제출 세 단계 사이 포함), N=2 | — | GREEN |
| [CrashRollback](HerdrRestartOwnerCrashRollback.cfg) | crash 뒤 옛 바이너리 | — | GREEN |
| [CrashCloseRefused](HerdrRestartOwnerCrashCloseRefused.cfg) | 실패 응답 전후 crash | — | GREEN |
| [CrashLaunchUnknown](HerdrRestartOwnerCrashLaunchUnknown.cfg) | create 불명 전후 crash | — | GREEN |
| [Before](HerdrRestartOwnerBefore.cfg) | main의 거절 | `RestartAvailable` | 기대 RED |
| [MainClear](HerdrRestartOwnerMainClear.cfg) | **main의 clear**: launch 중 턴에 보류만 보고 | `ClearForgets`, `NoSeedAfterClear`, `NoSessionAfterClear`, `NoStaleCreate` | 기대 RED |
| [DropQueue](HerdrRestartOwnerDropQueue.cfg) | retire가 큐를 purge | `AcceptedInputResolves`, `InputCustody` | 기대 RED |
| [OldInput](HerdrRestartOwnerOldInput.cfg) | fence 중 옛 pane 입력 | `NoOldPaneInput` | 기대 RED |
| [SeedBehind](HerdrRestartOwnerSeedBehind.cfg) | seed를 대기 입력 뒤에 둠(tmux 순서) | `NoOldPaneInput` | 기대 RED |
| [PrependOverHead](HerdrRestartOwnerPrependOverHead.cfg) | 전송 중 단위 앞에 seed 삽입 | `AcceptedInputResolves`, `RequestResolves`, `RestartAvailable` | 기대 RED |
| [NoGate](HerdrRestartOwnerNoGate.cfg) | gate 없이 적재 | `NoOldPaneInput`, `RestartAvailable` | 기대 RED |
| [NoPermit](HerdrRestartOwnerNoPermit.cfg) | permit 없이 부팅 | `NoFalseReady`, `RequestResolves` | 기대 RED |
| [SkipClose](HerdrRestartOwnerSkipClose.cfg) | 종료 관측 없이 retire | `SingleSession` | 기대 RED |
| [NoSettle](HerdrRestartOwnerNoSettle.cfg) | 옛 턴 결산 생략 | `InflightSettledBeforeReady` | 기대 RED |
| [ReplayInput](HerdrRestartOwnerReplayInput.cfg) | 입력 receipt 망각 | `InputAtMostOnce` | 기대 RED |
| [RequeueUncertain](HerdrRestartOwnerRequeueUncertain.cfg) | 제출 뒤·receipt 전 crash의 seed를 다시 queued로 | `SeedAtMostOnce` | 기대 RED |
| [ReadyOnAdmit](HerdrRestartOwnerReadyOnAdmit.cfg) | admission에서 Ready | `ReadyEvidence` | 기대 RED |
| [ReplayUnknown](HerdrRestartOwnerReplayUnknown.cfg) | 불명 create 재전송 | `AcceptedInputResolves`, `NoBlindCreate`, `SingleSession` | 기대 RED |
| [NoWake](HerdrRestartOwnerNoWake.cfg) | 해제 뒤 wake 없음 | `AcceptedInputResolves`, `RequestResolves`, `RestartAvailable` | 기대 RED |
| [NoNotice](HerdrRestartOwnerNoNotice.cfg) | 시작 실패 알림 없음 | `RequestResolves` | 기대 RED |
| [NoNoticeRefusal](HerdrRestartOwnerNoNoticeRefusal.cfg) | 거절 응답 없음 | `CommandAnswers`, `RestartAvailable` | 기대 RED |
| [UnboundDispatch](HerdrRestartOwnerUnboundDispatch.cfg) | dispatch에 묶지 않은 종료(seed 없음) | `NoInheritedDispatch` | 기대 RED |
| [RetryDispatch](HerdrRestartOwnerRetryDispatch.cfg) | postlude가 재시도 dispatch 생성 | `NoDispatchRepost` | 기대 RED |
| [NoPermitDetach](HerdrRestartOwnerNoPermitDetach.cfg) | permit이 남은 disconnect를 안 함 + crash | `NoInheritedDispatch` | 기대 RED |
| [NoSupersedeCheck](HerdrRestartOwnerNoSupersedeCheck.cfg) | postlude의 epoch 대조 없음 | `ClearForgets`, `NoResumeAfterClear`, `NoSeedAfterClear` | 기대 RED |
| [SeedAbsentIsClear](HerdrRestartOwnerSeedAbsentIsClear.cfg) | 응답이 seed 부재를 clear로 판단 | `NoFalseSuperseded` | 기대 RED |
| [ClearSkipsRetired](HerdrRestartOwnerClearSkipsRetired.cfg) | clear가 Retired row의 selector를 안 지움 | `ClearForgets`, `NoResumeAfterClear` | 기대 RED |
| [ReloadSeed](HerdrRestartOwnerReloadSeed.cfg) | clear 뒤 seed 재적재 | `ClearForgets`, `NoResumeAfterClear`, `NoSeedAfterClear`, `NoSeedRevival` | 기대 RED |
| [R1Launch](HerdrRestartOwnerR1Launch.cfg) | r1: exclusion 안에서 launch | `ClearForgets`, `NoSeedAfterClear`, `NoSessionAfterClear` | 기대 RED |
| [NoLaunchCheck](HerdrRestartOwnerNoLaunchCheck.cfg) | launch가 create 전 epoch을 안 봄 | `NoStaleCreate` | 기대 RED |
| [NoSubmitCheck](HerdrRestartOwnerNoSubmitCheck.cfg) | 제출 전 epoch을 안 봄 | `NoSeedAfterClear` | 기대 RED |
| [ClearNoWait](HerdrRestartOwnerClearNoWait.cfg) | clear가 launch 종료를 안 기다림 | `ClearForgets`, `NoResumeAfterClear` | 기대 RED |
| [ClearKeepsClaim](HerdrRestartOwnerClearKeepsClaim.cfg) | clear가 launch 중 입력을 결산하지 않음 | `NoSeedAfterClear` | 기대 RED |
| [LeakLaunching](HerdrRestartOwnerLeakLaunching.cfg) | 끝난 launch가 등록을 안 풂 | `ClearCompletes`, `ClearForgets` | 기대 RED |
| [IntentField](HerdrRestartOwnerIntentField.cfg) | 별도 V2 필드 + 롤백 | `RequestResolves` | 기대 RED |
| [WithdrawFirst](HerdrRestartOwnerWithdrawFirst.cfg) | 응답 전에 철회 + crash | `RequestResolves` | 기대 RED |
| [GapClearCloseFailure](HerdrRestartOwnerGapClearCloseFailure.cfg) | **남는 계약**: 종료 실패 + clear timeout | `ClearForgets` | 기대 RED |
| [GapClearCreateUnknown](HerdrRestartOwnerGapClearCreateUnknown.cfg) | **남는 계약**: create 응답 불명 + clear | `ClearForgets` | 기대 RED |
| [GapSeedlessDispatchCrash](HerdrRestartOwnerGapSeedlessDispatchCrash.cfg) | **남는 것**: seed 없음 + dispatch + crash (tmux와 같은 상태) | `NoInheritedDispatch` | 기대 RED |

규칙마다 그것을 깨는 cfg: `InputCustody`(DropQueue), `InputAtMostOnce`(ReplayInput), `NoOldPaneInput`(OldInput, SeedBehind, NoGate), `NoFalseReady`(NoPermit),
`SeedAtMostOnce`(RequeueUncertain), `NoBlindCreate`(ReplayUnknown), `SingleSession`(SkipClose), `InflightSettledBeforeReady`(NoSettle), `ReadyEvidence`(ReadyOnAdmit),
`NoInheritedDispatch`(UnboundDispatch, NoPermitDetach), `NoDispatchRepost`(RetryDispatch), `NoSeedAfterClear`(NoSubmitCheck, ClearKeepsClaim),
`NoStaleCreate`(NoLaunchCheck), `NoSessionAfterClear`(R1Launch, MainClear), `NoResumeAfterClear`(NoSupersedeCheck, ClearNoWait, ClearSkipsRetired),
`NoSeedRevival`(ReloadSeed), `NoFalseSuperseded`(SeedAbsentIsClear), `RequestResolves`(NoWake, NoNotice, IntentField, WithdrawFirst, PrependOverHead),
`CommandAnswers`(NoNoticeRefusal), `RestartAvailable`(Before, NoGate), `AcceptedInputResolves`(DropQueue, NoWake, PrependOverHead),
`ClearForgets`(NoSupersedeCheck 외), `ClearCompletes`(LeakLaunching).

`ClearSerial`·`ClearAny`·`SeedlessClear`는 상태가 500개를 넘어 차트 파일이 없다(개별 보고서와 TLC 판정은 있다).

## 모델이 r3 설계를 바꾼 곳

- **superseded restart가 자기 seed를 철회한다.** clear가 epoch만 올리고 아직 큐를 폐기하기 전에 restart가 exclusion을 놓으면, seed가 새 epoch으로 시작해 clear 뒤에 제출된다
  (첫 실행의 `ClearOvertake`·`ClearAny`에서 `NoSeedAfterClear` RED). postlude가 epoch 이동을 보면 selector 삭제와 함께 남은 seed를 철회하도록 설계 3.1 7단계에 넣었다.
- **permit이 "끝났지만 아직 Retired가 아닌" 실행에서는 기다린다.** close 뒤·retire 전 crash에서 seed를 거절해 버리면 그 뒤 대기 입력이 dispatch가 남은 row 위에 launch한다
  (`DispatchCrash`에서 `NoInheritedDispatch` RED). 거절은 실행이 아직 도는 경우로 좁히고, 끝난 경우는 기존 복구를 기다린 뒤 seed가 남은 disconnect를 끝내고 launch한다(3.1 8단계).
- **launch 시작의 "clear 진행 중" 확인은 넣지 않았다.** 넣은 모델과 뺀 모델의 결과가 같았다. restart가 transition을 쥔 동안 지나간 clear는 postlude의 epoch 대조가 닫는다.
  restart가 아닌 holder 아래의 같은 경합은 기존 계약으로 남긴다(설계 3.4의 남는 것 3).
- **clear의 Retired selector 삭제는 launch 중 clear에서 필요하다.** r2에서는 overtake 경합용이었지만 그 경우는 이제 epoch이 닫는다.
  launch가 스스로 멈춰 unsent Pending을 retire한 뒤의 row가 Retired이고, 그때 selector를 지울 주체가 clear뿐이다(`ClearSkipsRetired`).

## 반례와 실제 코드 대조

- [Before](herdr-restart-owner-Before.report.md): `restart.rs:219`의 거절이 취소보다 먼저다. 정적 추적으로 확인한 현재 동작이다.
- [MainClear](herdr-restart-owner-MainClear.report.md): Retired row + 진행 중 턴에서 clear는 보류를 보고하고(`control/herdr_recovery.rs:158`) launch는 취소 확인 없이 create한다
  (`src/services/herdr_launch.rs`에 취소 확인이 없다). 일반 메시지의 launch에서도 같은 현재 동작이다.
- [SeedAbsentIsClear](herdr-restart-owner-SeedAbsentIsClear.report.md): wake 뒤 seed는 제출되어 큐에서 빠진다(`v2/source_admission.rs:609`). 부재를 clear로 읽으면 정상 재시작에 superseded를 말하고 selector를 지운다.
- [PrependOverHead](herdr-restart-owner-PrependOverHead.report.md): 전송 중 단위 앞에 seed를 넣으면 그 단위의 결산이 `queue first item differs from selected head`로 실패한다(`v2/retained.rs:405`).
- [RequeueUncertain](herdr-restart-owner-RequeueUncertain.report.md): 물리 제출(`queue_io/input_owner/effect.rs:96`)과 receipt(`:130`) 사이 crash의 seed를 다시 queued로 되돌리면 인사말이 두 번 나간다.
- [R1Launch](herdr-restart-owner-R1Launch.report.md): exclusion 안에서 띄운 successor를 지나간 clear가 끝낼 수 없다(리뷰 d1의 P2-1).
- [IntentField](herdr-restart-owner-IntentField.report.md): `SnapshotV2`에는 `deny_unknown_fields`가 없어(`v2_types.rs:19`) 옛 바이너리가 새 필드를 읽지 않고 다시 쓸 때 지운다.

변형은 생산 코드를 바꾸거나 실행한 결과가 아니다. 구현 레그에서 같은 진입점과 fault 주입으로 RED→GREEN→원인 뮤턴트 RED를 확인해야 한다.

## 포트 가정과 한계

1. seed의 단위 뒤 적재·gate·permit·철회, 묶인 종료, clear epoch과 launch의 두 확인, clear의 launch 대기와 Retired selector 삭제는 아직 없는 구현이다. 모델은 그것들이 설계대로 있다고 가정한다.
2. seed가 owner front에서 headless 진입을 거쳐 launch에 닿는다는 것은 모델이 증명하지 않는다. herdr에서 그 진입은 지금 거절된다(`headless_turn.rs:159`).
3. clear가 launch 중 턴의 미제출 입력을 terminal로 결산한다는 것은 가정이다(`ClearKeepsClaim`이 그 가정을 뺀 변형). 기존 clear가 이미 그렇게 하는지는 B-clear 첫 시험이 확인한다.
4. 제출 직전의 epoch 확인과 제출을 한 단계로 묶었다. 실제로는 확인과 제출 사이에 clear가 시작할 수 있고, 그때는 "clear 전에 제출됨"으로 세며 clear가 그 세션을 끝낸다.
5. tmux에서 restart 뒤 dispatch가 최종적으로 어떻게 끝나는지는 모델하지 않았다. 모델은 restart가 dispatch 상태를 쓰지 않는다는 것만 본다.
6. clear와 겹친 crash, 두 번 이상의 crash·restart·clear, 다중 endpoint, restart가 아닌 holder 아래의 overtake는 제외했다.
7. 각 I/O가 결국 답하고 기존 복구(끝난 pane의 row 정리와 그 뒤의 owner wake, unplaced resolver, Pending 재확인)가 결국 돈다는 약한 공정성을 가정한다.
8. Codex resume 시작점과 출력 중복은 A의 범위다. 이 모델은 selector가 남았는지만 본다.

## 다시 실행

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py --base 33d1f4b5a9 --jobs 8 --java "$JAVA"
```

[정상 흐름](herdr-restart-owner-After.chart.mmd), [dispatch 실행 중](herdr-restart-owner-DispatchRestart.chart.mmd),
[launch 중 clear](herdr-restart-owner-ClearDuringLaunch.chart.mmd), [clear overtake](herdr-restart-owner-ClearOvertake.chart.mmd),
[seed 없는 재시작](herdr-restart-owner-Seedless.chart.mmd), [전송 중 단위](herdr-restart-owner-HeadBusy.chart.mmd),
[crash](herdr-restart-owner-Crash.chart.mmd), [현재 거절](herdr-restart-owner-Before.chart.mmd).
cfg마다 `herdr-restart-owner-<suffix>.report.md`가 같은 모델·[view](herdr-restart-owner.view.mjs)에서 생성됐다.
