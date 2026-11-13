# Herdr restart B1/B2 포트 모델: clear 허가 시각 계약

조사 기준 `588241083ccf4c2cda6d147e89e1d9fd10725387`(A+r3 병합). 정본은 [설계3.1~3.9](../../herdr-parity-restart.design.md)와 [B 분할 계획](../../herdr-parity-restart-b-split.plan.md)이다.

**PLAN_READY용 모델이며 B 생산 코드·Rust/cargo·PG/herdr 실실행·production PASS는 각각0이다.** After GREEN은 아래 제안 포트를 코드가 제공한다는 가정의 결과다. 현행 `/restart` 거절은 Before의 `RestartAvailable` 기대 RED로 남는다. A offset 계약의 TLA/cfg는 바꾸지 않았으며 r1 direct-launch의 s 영역은 B 수용 근거가 아니다.

## 수정 전/후와 원인 대조

| 경계 | 수정 전/생략 | 정정 After 포트 | 원인 대조 |
|---|---|---|---|
| P2-1 / B2 | idle Bound/placed Pending의 close 후 crash를 주체 없는 fair action이 복구 | inflight 복구 또는 durable seed owner가 exact ended row를 CAS하고 seed-only disconnect·wake | NoPermit/NoPermitDetach에서 `EndedSeedRecovers` RED |
| P2-2 / B2 | 재호출·conflict·완료 ticket을 신규 capacity 실패로 취급 | 종료 대상 선택 전에 existing-first ticket 조회·기존 결과 공유, terminal ticket 재종료0 | known-ticket seedless는 `NoOperationSeedless`, 재적재는 `NoOperationDuplicateSeed` RED |
| P2-4 / B1 | historical0f 모델은 live permission 동안 epoch 상승을 늦춤 | ClearBegin은 즉시 epoch++/clearing; old grant 유지·완료 허용, 새 BeginEffect 거절; 완료는 old startup/effect 배수+exact teardown 후 | 동일 semantic witness가 old RED/new GREEN; check-only create/early-release submit mutant RED |
| clear 완료 | 순간 launcher=0 또는 물리 발생시각만 검사 | 완료 순간 old startup/grant 배수 여부를 latch, 완료 뒤 새 epoch 입력은 허용 | ClearNoWait의 `NoSessionAfterClear` RED, post-clear fresh create9개 도달 |
| 복원 successor·head | seed 제출을 원자화하거나 head만 결산 | exact successor claim→CheckEpoch→BeginEffect→physical submit→receipt→binding, N=2 head+follower 결산 | 기존 관련 crash/PrependOverHead 기대 RED 유지 |

### 허가 시각 계약 정정의 RED → GREEN

동일 `witness/assert_grant_alignment.py`를 두 보존 DOT에 적용했다. old TLC가 당시 규칙으로 GREEN이었던 기록은 historical로 남기며 RED로 바꿔 부르지 않는다.

| 같은 관측 기준 | old `0f92c4de…` | new `e1ffe3de…` |
|---|---:|---:|
| graph states | 617 | 683 |
| live old grant를 유지한 ClearBegin/epoch 상승 edge | 0 | 8 |
| semantic checker | RED, exit1 | GREEN, exit0 |
| 새 source에서 clear 중 새 grant | — | 0 |
| ClearEnd의 old startup/grant 배수 edge | — | 96 |
| clear 완료 뒤 fresh create | — | 9 |

새 graph는 create와 submit 각각 `grantEpoch=0, permission=true → ClearBegin(epoch=1, clearing=true, old permission 유지) → 기존 physical effect 완료`에 도달한다. 새 BeginEffect는 live epoch/clearing을 다시 검사한다. AbortUnsent/AbortSubmit은 effectBegan인 grant를 조기 반납하지 않는다. NoStaleCreate/NoSeedAfterClear는 **물리 I/O 완료 시각이 아니라 새 grant 발급 시각**을 판정한다. 이미 허가된 I/O가 clear 시작 뒤 끝나는 것은 허용되며 clear 완료 뒤까지 남는 것은 금지다.

### expected.tsv의 정확한 델타

기존63cfg/행을 유지했다. 신규행0·예외추가0, 기존3행만 정정했다.

| cfg | 변경 | 이유 |
|---|---|---|
| ClearNoWait | +`NoSessionAfterClear` | clear 완료 순간 old grant/startup가 남는 경로를 새 latch가 잡음 |
| MainClear | +`EffectHasLivePermission` | 기다리지 않는 clear가 old worker 상태를 정리하는 조기반납 경로 검출 |
| NoSupersedeCheck | −`NoSeedAfterClear` | 이전 물리시각 규칙의 위반 제거. clear 전에 허가된 seed의 사후 완료는 정정 계약상 허용; `NoResumeAfterClear`·`ClearForgets` RED 유지 |

마지막 삭제는 기존 기대 위반을 숨기려는 완화가 아니라 사용자 P2-4의 허가 시각 계약 정정이다. 이전 위반집합63개 보존 주장은 폐기했다. 다른60행은 그대로다.

## 실제 코드가 제공해야 할 precondition

1. **idle repair 소유:** close 후·retire 전 crash에도 durable seed owner가 exact old nonce/owner/dispatch를 재검사해 복구한다. `src/services/discord/herdr_terminate.rs:186` read_end, :196 provider_absent, :217 execution_end, :224 retire_confirmed를 사용하고 seed-only disconnect→wake까지 맡는 B2 포트다. 기존 background job이 이를 이미 보장한다는 주장이 아니다.
2. **existing-first:** canonical 종료 대상 선정/gate/close 전에 operation·ticket을 조회한다. same/conflict 및 완료·retained·clear-disposed ticket은 기존 결과를 공유하고 새 seed/seedless 종료를 만들지 않는다. 실제 durable bind/receipt 조회와 정확한 outcome 분류가 필요하다.
3. **live effect permission:** 같은 mutex에서 begin_clear가 즉시 epoch++/clearing on하고 BeginEffect가 live epoch/clearing을 재검사한다. clear 전에 발급한 EffectPermit을 blocking backend 호출 반환까지 보유한다. clear는 이후 새 grant를 막고 기존 effect/startup 배수 뒤 exact teardown한다. CheckEpoch bool 전달만으로는 이 계약이 안 된다.
4. **startup registration:** clearing 중 신규 launch 등록은 같은 mutex에서 거절/보류하고 counter를 늘리지 않는다. 모델 OwnerFree/LaunchBegin의 clearing gate가 이 선형화의 추상화다. clear 완료 뒤 새 입력은 정상 허용한다. 새 giant owner 동작이 이미 존재한다는 주장이 아니다.
5. **exact successor/dispatch:** SeedJoin은 ticket/attempt가 소유한 successor nonce만 허용한다. 복원 Bound도 submit permission을 소비한다. disconnect는 restart postlude 또는 같은 seed permit만 하고, 일반 입력은 old dispatch를 지우지 않는다. dispatch status/history는 PG snapshot 시험이 필요하다.
6. **실제 effect 경계:** headless_turn.rs:147~165는 claim/reset 전이다. seed 설치 뒤 전체 terminate를 재호출하지 않는다. leaf는 queue_io/input_owner/effect.rs:96, receipt :130; Pending CAS는 herdr_launch.rs:498, registry 연결 :508, prepared create :531이다. TLA의 historical :482 주석은 실행 증거 좌표로 쓰지 않는다. TUI claim은 synthetic_start/claim.rs:54 및 wiring :618이다.
7. **sticky startup/followers:** clear가 관측한 startup은 먼저 끝나도 sticky해야 한다. 실제 PreparedPayload.followers(queued_head/v2_types.rs:147)의 snapshot/claim/clear receipt/upload/bind index는 B1 Rust 시험이 검증한다. 모델은 head+follower 단위이며 임의 merge payload 직렬화는 증명하지 않는다.

## 값 범위·가정과 한계

기존 r3 row7종·seed9상태·launch5단계·clear epoch·dispatch·receipt는 canonical 진입점 표의 코드 값이다. RecoveryPolicy=owned/missing, ReplayMode=off/same/conflict, OpPolicy=strict/seedless/duplicate, EffectPolicy=serialized/check_only_create/early_release_submit은 제안 포트·원인 mutant의 선택값이다. grantEpoch/grantValid·checked/permission/effectKind·clearRequested/clearing·완료 latch는 새 B1 계약의 모델 표시값이며 현재 생산 guard가 아니다.

한 clear·한 crash·한 알려진 operation 재호출, N=1/2를 검사한다. backend I/O가 결과/timeout으로 반환하고 owner/notice/clear scheduler가 계속 실행한다는 fairness를 쓴다. 영구 network 단절·반환하지 않는 backend·ticket eviction/unknown commit·데이터 손상·native source 검증은 증명하지 않는다. clear와 겹친 crash는 기존 계약대로 제외한다.

**startup 범위 차이:** 모델 launcher는 receipt 뒤 Source binding까지 보유하는 추상화다. 실제 B1 계획의 startup counter는 물리 첫 submit 반환 때 끝나고 AI reply/Source까지 기다리지 않는다. 따라서 clear 뒤 늦은 stale Bind CAS가 session/selector를 되살리지 못함을 실제 Rust 시험에서 별도로 확인해야 한다. 모델 GREEN으로 그 시험을 대체하지 않는다. Source/Ready 자체는 owner admission/Launched와 구별한다.

A immutable offset·Resume cause·ordinary fresh TLA SHA256 `11be1251b5e3854547c95dcecce55ffd26ee599fce748c39bc609871f5106f5d`는 그대로다. actual `/restart`/seed owner 연결은 B 생산 구현으로 아직 제공되지 않는다.

## 검증 결과와 증거

20 invariant/8 liveness 및 NoDeadlock 자동 검사를 쓴다. 새 TLA SHA256 `e1ffe3de64e0c6a01d3b84b7a1e07a580471a1e72f9a06b657bf97e117fabe63`.

- 전체 새 source scan: **63cfg/608 TLC 하위 실행, GREEN28·기대RED35, unchecked0**, 284.1초. old expected로 실행해 위3행 mismatch 때문에 **exit1**이었다. 이를 PASS로 세지 않는다.
- 기대3행을 정정한 delta: **3cfg/34 TLC 하위 실행, 23.6초, exit0**, 최종 기대집합과 일치. 전체 scan의 나머지60행 결과와 최종63행 집합도 대조했다. 같은 source에 대한 불필요 전체 로컬 반복은 하지 않았고, committed head 공식 repo e2e63은 부모 레인이 별도 기록한다.
- After/ClearAny/ClearDuringLaunch/ClearOvertake/ClearSerial positive5: 45 TLC, exit0, 46.9초. 체크가 끝나지 않은 규칙0.
- old0f 모델607 runs와 이전 repo e2e는 historical 계약 진단으로 보존한다. 새 source의 최종 성공으로 재사용하지 않는다. 첫 clear-grant 진단 실패도 최종 증거가 아니다.

새 로그 root는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8128/logs/impl-b/model/clear-grant/`다. `checker-grant-v2-all.log`, `checker-grant-v2-delta3.log`, `checker-grant-v2-positive.log`, `witness/{old,new}-result.json`·`assert_grant_alignment.py`, wrapper `render-v2-final/`, 최종 입력 `inputs-final/`로 구별한다. old 아카이브 `../inputs-final/`는 덮어쓰지 않았다.

환경 OpenJDK27.0.1.0.3/Node26.13.0/TLC2 2.19(tla2tools1.7.4), SHELL=/bin/zsh. Cargo/PG/운영 실행0. 모델 반례는 제안 guard를 제거한 추상 경로이며 생산 mutation/실재현이 아니다. 코드 추적은 위 좌표의 포트 공백 확인이고 B1/B2 실제 진입·PG·원인 mutation13시험을 대체하지 않는다.

최종 wrapper63개도 새 source/최종 기대집합과 각각 대조했다. GREEN 중500-state 그림 상한을 넘는5개는 보고서만 제공하고 stale 그림을 두지 않는다. 해당 cfg를 CI에서 뺀 것이 아니며 states는 관측값, steps 미제공은 미제공으로 적었다. 아래 각 RED 보고서 링크에는 규칙별 최단 반례의 번호와 전체 경로가 있다. 그림은 같은 라벨 안의 self-step을 숨기며 그 수를 개별 보고서가 기록한다.

| cfg | TLC states/steps | 실제 깨진 규칙 | 결과 |
|---|---:|---|---|
| [After](herdr-restart-owner-After.report.md) | 65/190 | — | GREEN |
| [Before](herdr-restart-owner-Before.report.md) | 5/10 | `RestartAvailable` | 기대 RED |
| [ClearAny](herdr-restart-owner-ClearAny.report.md) | 904/미제공(그림상한) | — | GREEN |
| [ClearDuringLaunch](herdr-restart-owner-ClearDuringLaunch.report.md) | 683/미제공(그림상한) | — | GREEN |
| [ClearKeepsClaim](herdr-restart-owner-ClearKeepsClaim.report.md) | 298/890 | `NoSeedAfterClear` | 기대 RED |
| [ClearNoWait](herdr-restart-owner-ClearNoWait.report.md) | 884/미제공(그림상한) | `NoSessionAfterClear`, `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [ClearOvertake](herdr-restart-owner-ClearOvertake.report.md) | 799/미제공(그림상한) | — | GREEN |
| [ClearSerial](herdr-restart-owner-ClearSerial.report.md) | 904/미제공(그림상한) | — | GREEN |
| [ClearSkipsRetired](herdr-restart-owner-ClearSkipsRetired.report.md) | 704/미제공(그림상한) | `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [CloseIndeterminate](herdr-restart-owner-CloseIndeterminate.report.md) | 14/31 | — | GREEN |
| [CloseRefused](herdr-restart-owner-CloseRefused.report.md) | 14/31 | — | GREEN |
| [Concurrent](herdr-restart-owner-Concurrent.report.md) | 133/454 | — | GREEN |
| [Crash](herdr-restart-owner-Crash.report.md) | 188/551 | — | GREEN |
| [CrashCloseRefused](herdr-restart-owner-CrashCloseRefused.report.md) | 20/54 | — | GREEN |
| [CrashLaunchUnknown](herdr-restart-owner-CrashLaunchUnknown.report.md) | 75/182 | — | GREEN |
| [CrashRollback](herdr-restart-owner-CrashRollback.report.md) | 192/562 | — | GREEN |
| [DispatchCrash](herdr-restart-owner-DispatchCrash.report.md) | 92/225 | — | GREEN |
| [DispatchRestart](herdr-restart-owner-DispatchRestart.report.md) | 67/195 | — | GREEN |
| [DropQueue](herdr-restart-owner-DropQueue.report.md) | 31/75 | `InputCustody`, `AcceptedInputResolves` | 기대 RED |
| [GapClearCloseFailure](herdr-restart-owner-GapClearCloseFailure.report.md) | 87/238 | `ClearForgets` | 기대 RED |
| [GapClearCreateUnknown](herdr-restart-owner-GapClearCreateUnknown.report.md) | 222/681 | `ClearForgets` | 기대 RED |
| [GapSeedlessDispatchCrash](herdr-restart-owner-GapSeedlessDispatchCrash.report.md) | 97/238 | `NoInheritedDispatch` | 기대 RED |
| [HeadBusy](herdr-restart-owner-HeadBusy.report.md) | 124/320 | — | GREEN |
| [HeadBusyCrash](herdr-restart-owner-HeadBusyCrash.report.md) | 336/844 | — | GREEN |
| [IntentField](herdr-restart-owner-IntentField.report.md) | 92/226 | `RequestResolves` | 기대 RED |
| [LaunchNotSent](herdr-restart-owner-LaunchNotSent.report.md) | 35/85 | — | GREEN |
| [LaunchUnknown](herdr-restart-owner-LaunchUnknown.report.md) | 27/65 | — | GREEN |
| [LeakLaunching](herdr-restart-owner-LeakLaunching.report.md) | 217/665 | `ClearForgets`, `ClearCompletes`, `ClearRequestCompletes` | 기대 RED |
| [MainClear](herdr-restart-owner-MainClear.report.md) | 394/1291 | `NoSessionAfterClear`, `NoStaleCreate`, `EffectHasLivePermission`, `NoSeedAfterClear`, `ClearForgets` | 기대 RED |
| [NoGate](herdr-restart-owner-NoGate.report.md) | 70/169 | `NoOldPaneInput`, `RestartAvailable` | 기대 RED |
| [NoLaunchCheck](herdr-restart-owner-NoLaunchCheck.report.md) | 271/857 | `EffectHasLivePermission`, `NoStaleCreate` | 기대 RED |
| [NoNotice](herdr-restart-owner-NoNotice.report.md) | 91/223 | `RequestResolves` | 기대 RED |
| [NoNoticeRefusal](herdr-restart-owner-NoNoticeRefusal.report.md) | 3/5 | `CommandAnswers`, `RestartAvailable` | 기대 RED |
| [NoPermit](herdr-restart-owner-NoPermit.report.md) | 91/222 | `NoFalseReady`, `EffectHasLivePermission`, `RequestResolves`, `AcceptedInputResolves`, `EndedSeedRecovers` | 기대 RED |
| [NoPermitDetach](herdr-restart-owner-NoPermitDetach.report.md) | 205/590 | `NoOperationSeedless`, `NoInheritedDispatch`, `RequestResolves`, `AcceptedInputResolves`, `EndedSeedRecovers` | 기대 RED |
| [NoSettle](herdr-restart-owner-NoSettle.report.md) | 33/80 | `InflightSettledBeforeReady` | 기대 RED |
| [NoSubmitCheck](herdr-restart-owner-NoSubmitCheck.report.md) | 271/839 | `EffectHasLivePermission`, `NoSeedAfterClear` | 기대 RED |
| [NoSupersedeCheck](herdr-restart-owner-NoSupersedeCheck.report.md) | 799/미제공(그림상한) | `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [NoWake](herdr-restart-owner-NoWake.report.md) | 35/83 | `RequestResolves`, `RestartAvailable`, `AcceptedInputResolves` | 기대 RED |
| [OldInput](herdr-restart-owner-OldInput.report.md) | 61/150 | `NoOldPaneInput` | 기대 RED |
| [PrependOverHead](herdr-restart-owner-PrependOverHead.report.md) | 11/25 | `RequestResolves`, `RestartAvailable`, `AcceptedInputResolves` | 기대 RED |
| [R1Launch](herdr-restart-owner-R1Launch.report.md) | 263/799 | `EffectHasLivePermission`, `NoSeedAfterClear`, `NoSessionAfterClear`, `ClearForgets` | 기대 RED |
| [ReadyOnAdmit](herdr-restart-owner-ReadyOnAdmit.report.md) | 35/85 | `ReadyEvidence` | 기대 RED |
| [ReloadSeed](herdr-restart-owner-ReloadSeed.report.md) | 947/미제공(그림상한) | `NoSeedRevival`, `NoResumeAfterClear`, `NoSeedAfterClear`, `ClearForgets` | 기대 RED |
| [ReplayInput](herdr-restart-owner-ReplayInput.report.md) | 73/214 | `NoOperationDuplicateSeed`, `InputAtMostOnce` | 기대 RED |
| [ReplayUnknown](herdr-restart-owner-ReplayUnknown.report.md) | 53/158 | `NoOperationSeedless`, `NoBlindCreate`, `SingleSession`, `EffectHasLivePermission`, `AcceptedInputResolves` | 기대 RED |
| [RequeueUncertain](herdr-restart-owner-RequeueUncertain.report.md) | 103/247 | `SeedAtMostOnce` | 기대 RED |
| [RetryDispatch](herdr-restart-owner-RetryDispatch.report.md) | 33/80 | `NoDispatchRepost` | 기대 RED |
| [SeedAbsentIsClear](herdr-restart-owner-SeedAbsentIsClear.report.md) | 36/85 | `NoFalseSuperseded` | 기대 RED |
| [SeedBehind](herdr-restart-owner-SeedBehind.report.md) | 93/231 | `NoOldPaneInput` | 기대 RED |
| [Seedless](herdr-restart-owner-Seedless.report.md) | 106/295 | — | GREEN |
| [SeedlessClear](herdr-restart-owner-SeedlessClear.report.md) | 1338/미제공(그림상한) | — | GREEN |
| [SeedlessCrash](herdr-restart-owner-SeedlessCrash.report.md) | 238/694 | — | GREEN |
| [SkipClose](herdr-restart-owner-SkipClose.report.md) | 33/80 | `SingleSession` | 기대 RED |
| [StartLegacy](herdr-restart-owner-StartLegacy.report.md) | 25/60 | — | GREEN |
| [StartMissing](herdr-restart-owner-StartMissing.report.md) | 25/60 | — | GREEN |
| [StartPendingPlaced](herdr-restart-owner-StartPendingPlaced.report.md) | 91/223 | — | GREEN |
| [StartPendingUnplaced](herdr-restart-owner-StartPendingUnplaced.report.md) | 26/63 | — | GREEN |
| [StartRetired](herdr-restart-owner-StartRetired.report.md) | 25/60 | — | GREEN |
| [StartUnknown](herdr-restart-owner-StartUnknown.report.md) | 5/10 | — | GREEN |
| [UnboundDispatch](herdr-restart-owner-UnboundDispatch.report.md) | 106/295 | `NoInheritedDispatch` | 기대 RED |
| [WithdrawFallback](herdr-restart-owner-WithdrawFallback.report.md) | 13/30 | — | GREEN |
| [WithdrawFirst](herdr-restart-owner-WithdrawFirst.report.md) | 18/46 | `RequestResolves` | 기대 RED |

## 핵심 반례와 남긴 계약

- [Before: `RestartAvailable`](herdr-restart-owner-Before.report.md): 0 → 1 → 2 → 3. Submit an input to the still-current session: Visible failure; inputs settled; old session lives

- [NoPermitDetach: `EndedSeedRecovers`](herdr-restart-owner-NoPermitDetach.report.md): 0 → 1 → 2 → 3 → 4 → 5. Treat known operation as seedless capacity failure: Idle ended execution; the seed owner must repair the row; known ticket entered the seedless path; original 1 waiting

- [NoPermitDetach: `NoOperationSeedless`](herdr-restart-owner-NoPermitDetach.report.md): 0 → 1 → 2. Treat known operation as seedless capacity failure: Seed committed; gate holds it; known ticket entered the seedless path; original 1 waiting

- [ReplayInput: `NoOperationDuplicateSeed`](herdr-restart-owner-ReplayInput.report.md): 0 → 1 → 2. Insert a duplicate operation seed: Seed committed; gate holds it; known operation inserted another seed; original 1 waiting

- [NoLaunchCheck: `EffectHasLivePermission`](herdr-restart-owner-NoLaunchCheck.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10. Complete the previously granted create: Create answered; first input not submitted; physical effect used no live permission; original 1 waiting

- [NoLaunchCheck: `NoStaleCreate`](herdr-restart-owner-NoLaunchCheck.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11. Compare live epoch and retain create permission: A new create grant ignored the clear epoch

- [NoSubmitCheck: `EffectHasLivePermission`](herdr-restart-owner-NoSubmitCheck.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13. Complete the previously granted first input: First input submitted; receipt not committed; physical effect used no live permission; original 1 waiting

- [NoSubmitCheck: `NoSeedAfterClear`](herdr-restart-owner-NoSubmitCheck.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13 → 14. Compare live epoch and retain input permission: A new seed grant was issued after clear

- [ClearNoWait: `NoSessionAfterClear`](herdr-restart-owner-ClearNoWait.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10. Clear drains earlier grants and tears down their exact session: Owner turn began a launch (after a clear); clear completed before an older startup or grant drained; original 1 explicitly dropped, original 2 not yet arrived

- [NoSupersedeCheck: `NoResumeAfterClear`](herdr-restart-owner-NoSupersedeCheck.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11. Owner starts the front: register the launch: A launch resumed the conversation a clear ended

- [LeakLaunching: `ClearRequestCompletes`](herdr-restart-owner-LeakLaunching.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13. Clear epoch moved: retire the unsent Pending: Clear discarded the queue; a finished launch never released its registration

GapClearCloseFailure/GapClearCreateUnknown/GapSeedlessDispatchCrash의 기대 RED는 남긴 공백이다. owner Accepted/Launched는 Ready 증거가 아니며 receipt+source binding이 필요하다. named action/fairness 자체가 생산 guard는 아니다. reserved-source ingress 거절·actor 교체·실제 dispatch 경합·V1/V2/unknown schema 복원은 분할 계획의13개 실제 시험이 필요하다.

## 재실행

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py --root /tmp/i8128-owner-cleargrant-v2 --jobs 2
/Users/itismyfield/.codex/skills/statechart/scripts/statechart \
  /tmp/i8128-owner-cleargrant-v2/HerdrRestartOwner.tla \
  --config /tmp/i8128-owner-cleargrant-v2/HerdrRestartOwnerAfter.cfg \
  --view /tmp/i8128-owner-cleargrant-v2/herdr-restart-owner.view.mjs \
  --out /tmp/i8128-owner-cleargrant-replay-new
```

same witness 재현은 로그 root의 `witness/assert_grant_alignment.py DOT --source TLA --out NEW.json`이다. old는 `../coverage-final/ClearDuringLaunch.dot` + `../inputs-final/HerdrRestartOwner.tla`(예상 exit1), new는 `witness/new.dot` + `inputs-final/HerdrRestartOwner.tla`(예상 exit0)를 쓴다. old/new JSON의 states617/683와 source SHA도 함께 확인한다. 빈 graph나 parser 실패는 기대 RED로 세지 않는다.

[정상](herdr-restart-owner-After.chart.mmd), [idle Pending crash](herdr-restart-owner-StartPendingPlaced.chart.mmd), [BeginEffect 제거](herdr-restart-owner-NoLaunchCheck.chart.mmd), [허가 조기반납](herdr-restart-owner-NoSubmitCheck.chart.mmd), [clear 배수 생략](herdr-restart-owner-ClearNoWait.chart.mmd). 새 source TLA/cfg/view는 scratch·저장소·새 inputs-final의 바이트 동일성을 별도 검사한다.
