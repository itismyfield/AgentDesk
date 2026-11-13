# Herdr restart B1/B2 구현 전 포트 모델

조사 기준 `588241083ccf4c2cda6d147e89e1d9fd10725387`(A+r3 병합). 정본은 [설계3.1~3.9](../../herdr-parity-restart.design.md)와 [B 분할 계획](../../herdr-parity-restart-b-split.plan.md)이다.

**PLAN_READY용 모델이며 B 생산 코드·Rust/cargo·PG/herdr 실실행·production PASS는 각각0이다.** After는 아래 새 포트를 실제 코드가 제공한다는 가정 아래의 GREEN이다. 현행 `/restart`의 거절은 Before에서 그대로 RED다. A offset 계약을 바꾸지 않았으며 r1 direct-launch 모델의 s 영역을 B 수용 근거로 재사용하지 않는다.

## 수정 전/후와 원인 대조

| 경계/조각 | r3의 생략/Before | After 포트 | 원인 mutant |
|---|---|---|---|
| P2-1 / B2 | 주체 없는 fair RowEndRecovery가 idle Bound/placed Pending도 저절로 정리 | 기존 inflight 복구가 있거나 durable seed owner/permit이 exact ended row를 CAS하고 wake | NoPermit·NoPermitDetach의 RecoveryPolicy=missing: EndedSeedRecovers 및 입력/seed 진행성 RED |
| P2-2 / B2 | 재호출·다른 operation·완료 ticket 조회 없음 | same/conflict는 existing-first ticket 반환/공유, terminal ticket 재종료0 | known-ticket→seedless는 NoOperationSeedless RED; 재적재는 NoOperationDuplicateSeed RED |
| P2-4 / B1 | epoch 확인과 physical effect가 한 step | CheckEpoch 관찰 → live BeginEffect 비교/허가 → 허가를 보유한 physical create/first-input | check-only create·조기반납 submit에서 EffectHasLivePermission 및 stale create/seed RED |
| 복원 successor | SeedJoin이 원자적 제출 | exact successor의 seed도 claim→CheckSubmitEpoch→BeginSubmitEffect→effect→receipt→binding | 관련 crash cfg 재검사 |
| 전송 중 단위 | head 하나 | HeadBusy N=2는 head+1 follower의 이미 진행 중인 단위. 모두 결산되기 전 seed는 앞지르지 않음 | 기존 PrependOverHead RED 유지 |

기존63cfg를 유지했다. 신규 cfg/expected 행0, 기존 위반 집합63개 삭제0, 예외 추가0이다. capacity/V2/effort admission 실패의 seedless 재시작은 사용자 결정대로 허용하고, **known replay/conflict를 capacity 실패로 바꾸는 것만 금지**한다.

## 실제 코드가 제공해야 할 precondition

1. **idle repair 소유:** close 뒤·retire 전 crash에서 gate/명령 future가 사라져도 durable seed owner가 처리한다. `src/services/discord/herdr_terminate.rs:217` execution_end와 :224 retire_confirmed를 exact old nonce/owner/dispatch 재검사 아래 사용하고 seed-only disconnect→wake까지 맡는다. 새 idle 분기는 B2 포트이며 main의 background job이 이미 보장한다는 주장이 아니다.
2. **existing-first:** canonical 종료 대상 선정/gate/close 전에 operation·ticket을 조회한다. same/conflict 및 완료/retained/clear-disposed ticket은 기존 결과를 반환/공유하고 새 seed나 seedless 종료를 만들지 않는다. 모델의 ReplayRequest는 이 early-return 포트이며 실제 durable bind/receipt 조회·정확한 existing outcome 분류가 필요하다.
3. **live effect permission:** clear epoch 갱신과 BeginEffect가 같은 serializer를 사용한다. CheckEpoch bool만 넘기지 말고 권한을 blocking thread의 backend 호출 선형화까지 보유한다. scope는 create/seed first-input이며, 수락 전에 이미 시작된 native 입력의 receipt는 별개다. clear 요청은 기록되고 수락된 bounded I/O가 권한을 돌려준 뒤 epoch을 올린다. 사용자 도착과 clear epoch 선형화는 구별한다.
4. **exact successor:** SeedJoin은 해당 ticket/attempt가 소유한 정확한 successor nonce에만 허용한다. 임의 새 Bound를 자기 successor로 보지 않는다. 복원된 Bound에서도 같은 submit permission을 소비한다.
5. **dispatch:** disconnect는 restart postlude 또는 같은 seed permit만 한다. 일반 입력은 old dispatch를 지우지 않는다. GapSeedlessDispatchCrash 기대 RED는 유지한다. 실제 dispatch status/history 불변은 PG snapshot 시험이 필요하고 bool proxy만으로 증명하지 않는다.
6. **실제 진입:** headless_turn.rs:147~165는 claim/reset 전이다. 새 seed token 설치 뒤 전체 terminate를 재호출해 seed 자신을 captured-turn settlement 대상으로 삼지 않는다. 물리 leaf는 queue_io/input_owner/effect.rs:96, receipt는 :130; Pending/create는 herdr_launch.rs:498/:531이다.
7. **sticky startup/followers:** clear가 관측한 startup은 먼저 끝나도 sticky해야 한다. 순간 Launching=0만 보고 Retired 특수 보류로 돌아가지 않는다. 실제 PreparedPayload.followers(queued_head/v2_types.rs:147)의 snapshot/claim/clear receipt/upload/bind index는 B1 Rust 시험이 검증한다. 모델은 이미 진행 중인 head+follower의 일반화이며 임의 merge payload 직렬화까지 증명하지 않는다.

## 값 범위·가정

기존 r3 row7종/seed9상태/launch5단계/clear epoch/dispatch/receipt는 canonical 진입점 표 그대로다. RecoveryPolicy=owned/missing은 seed owner의 idle repair 소유, ReplayMode=off/same/conflict는 알려진 ticket 조회, OpPolicy=strict/seedless/duplicate는 classifier guard와 cause mutant다. EffectPolicy=serialized/check_only_create/early_release_submit은 live epoch 허가와 그 제거 변형이다. checked/permission/effectKind/clearRequested는 새 B1 메모리 포트의 표시값이다.

한 clear·한 crash·한 알려진 operation 재호출, N=1/2를 검사한다. 실제 I/O가 결과/timeout으로 돌아오고 scheduler가 owner/notice/clear를 계속 실행한다는 fairness를 쓴다. 영구 network 단절·반환하지 않는 backend·ticket eviction/unknown commit·데이터 손상·native source 검증은 이 모델의 증명이 아니다. clear와 겹친 crash는 기존 계약대로 제외한다.

## 새 규칙과 실제 결과

기존17 invariant/6 liveness에 NoOperationSeedless, NoOperationDuplicateSeed, EffectHasLivePermission, EndedSeedRecovers, ClearRequestCompletes를 추가해 **20 invariant/8 liveness**다(NoDeadlock 자동 검사 별도).

**63 cfg /607 TLC 하위 실행, GREEN28·기대RED35, 미검사0, 기대집합 전부 일치(exit0)**. 최종 checker 총289.2초, cfg 최대14.0초로 300초/cfg 예산 이내다. 기존63개 위반 집합을 전부 보존한 것을 별도로 assert했다. 새 guard의 비활성 값이 만드는 추가 진행성은 초기 실행에서 확인했고 최종 기대집합을 강화했다.

모델 SHA256 `0f92c4defbb01df50696fb87b67e5119320f9663e556a13393c4699762f248f4`. 정식 로그는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8128/logs/impl-b/model/checker-b-ports-final.log`, wrapper는 render-b-ports-final/, DOT는 coverage-final/coverage.json, 최종 입력은 inputs-final/이다. scratch·저장소·archive의 TLA/cfg/view는 바이트 동일하다. 초기 로그나 partial packaging을 최종 증거로 쓰지 않았다.

최종 wrapper63개도 모두 검사했다. GREEN ClearAny, ClearDuringLaunch, ClearOvertake, ClearSerial, SeedlessClear는 500-state 그림 예산 때문에 Mermaid를 만들지 않았다. 해당 stale r3 그림5개를 제거하고 TLC 보고서/state 수를 유지했다. 이 cfg를 CI에서 빼거나 범위를 줄인 것이 아니다. 500-state 초과 negative cfg는 wrapper가 생성한 반례 경로 그림을 제공한다.

환경 OpenJDK27.0.1.0.3/Node26.13.0/TLC2 2.19(tla2tools1.7.4), SHELL=/bin/zsh. Cargo/PG/운영 실행0, 검증 process는 모두 종료했다.

| cfg | TLC states/steps | 실제 깨진 규칙 | 결과 |
|---|---:|---|---|
| [After](HerdrRestartOwnerAfter.cfg) | 65/190 | — | GREEN |
| [Before](HerdrRestartOwnerBefore.cfg) | 5/10 | `RestartAvailable` | 기대 RED |
| [ClearAny](HerdrRestartOwnerClearAny.cfg) | 1030/미제공(그림상한) | — | GREEN |
| [ClearDuringLaunch](HerdrRestartOwnerClearDuringLaunch.cfg) | 617/미제공(그림상한) | — | GREEN |
| [ClearKeepsClaim](HerdrRestartOwnerClearKeepsClaim.cfg) | 268/792 | `NoSeedAfterClear` | 기대 RED |
| [ClearNoWait](HerdrRestartOwnerClearNoWait.cfg) | 761/미제공(그림상한) | `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [ClearOvertake](HerdrRestartOwnerClearOvertake.cfg) | 925/미제공(그림상한) | — | GREEN |
| [ClearSerial](HerdrRestartOwnerClearSerial.cfg) | 1030/미제공(그림상한) | — | GREEN |
| [ClearSkipsRetired](HerdrRestartOwnerClearSkipsRetired.cfg) | 644/미제공(그림상한) | `NoResumeAfterClear`, `ClearForgets` | 기대 RED |
| [CloseIndeterminate](HerdrRestartOwnerCloseIndeterminate.cfg) | 14/31 | — | GREEN |
| [CloseRefused](HerdrRestartOwnerCloseRefused.cfg) | 14/31 | — | GREEN |
| [Concurrent](HerdrRestartOwnerConcurrent.cfg) | 133/454 | — | GREEN |
| [Crash](HerdrRestartOwnerCrash.cfg) | 172/511 | — | GREEN |
| [CrashCloseRefused](HerdrRestartOwnerCrashCloseRefused.cfg) | 20/54 | — | GREEN |
| [CrashLaunchUnknown](HerdrRestartOwnerCrashLaunchUnknown.cfg) | 67/166 | — | GREEN |
| [CrashRollback](HerdrRestartOwnerCrashRollback.cfg) | 172/512 | — | GREEN |
| [DispatchCrash](HerdrRestartOwnerDispatchCrash.cfg) | 84/209 | — | GREEN |
| [DispatchRestart](HerdrRestartOwnerDispatchRestart.cfg) | 67/195 | — | GREEN |
| [DropQueue](HerdrRestartOwnerDropQueue.cfg) | 31/75 | `InputCustody`, `AcceptedInputResolves` | 기대 RED |
| [GapClearCloseFailure](HerdrRestartOwnerGapClearCloseFailure.cfg) | 92/249 | `ClearForgets` | 기대 RED |
| [GapClearCreateUnknown](HerdrRestartOwnerGapClearCreateUnknown.cfg) | 250/745 | `ClearForgets` | 기대 RED |
| [GapSeedlessDispatchCrash](HerdrRestartOwnerGapSeedlessDispatchCrash.cfg) | 89/222 | `NoInheritedDispatch` | 기대 RED |
| [HeadBusy](HerdrRestartOwnerHeadBusy.cfg) | 124/320 | — | GREEN |
| [HeadBusyCrash](HerdrRestartOwnerHeadBusyCrash.cfg) | 304/780 | — | GREEN |
| [IntentField](HerdrRestartOwnerIntentField.cfg) | 82/206 | `RequestResolves` | 기대 RED |
| [LaunchNotSent](HerdrRestartOwnerLaunchNotSent.cfg) | 35/85 | — | GREEN |
| [LaunchUnknown](HerdrRestartOwnerLaunchUnknown.cfg) | 27/65 | — | GREEN |
| [LeakLaunching](HerdrRestartOwnerLeakLaunching.cfg) | 193/581 | `ClearForgets`, `ClearCompletes`, `ClearRequestCompletes` | 기대 RED |
| [MainClear](HerdrRestartOwnerMainClear.cfg) | 364/1181 | `NoStaleCreate`, `NoSessionAfterClear`, `NoSeedAfterClear`, `ClearForgets` | 기대 RED |
| [NoGate](HerdrRestartOwnerNoGate.cfg) | 70/169 | `NoOldPaneInput`, `RestartAvailable` | 기대 RED |
| [NoLaunchCheck](HerdrRestartOwnerNoLaunchCheck.cfg) | 247/775 | `EffectHasLivePermission`, `NoStaleCreate` | 기대 RED |
| [NoNotice](HerdrRestartOwnerNoNotice.cfg) | 83/207 | `RequestResolves` | 기대 RED |
| [NoNoticeRefusal](HerdrRestartOwnerNoNoticeRefusal.cfg) | 3/5 | `CommandAnswers`, `RestartAvailable` | 기대 RED |
| [NoPermit](HerdrRestartOwnerNoPermit.cfg) | 83/206 | `NoFalseReady`, `EffectHasLivePermission`, `RequestResolves`, `AcceptedInputResolves`, `EndedSeedRecovers` | 기대 RED |
| [NoPermitDetach](HerdrRestartOwnerNoPermitDetach.cfg) | 189/550 | `NoOperationSeedless`, `NoInheritedDispatch`, `RequestResolves`, `AcceptedInputResolves`, `EndedSeedRecovers` | 기대 RED |
| [NoSettle](HerdrRestartOwnerNoSettle.cfg) | 33/80 | `InflightSettledBeforeReady` | 기대 RED |
| [NoSubmitCheck](HerdrRestartOwnerNoSubmitCheck.cfg) | 241/740 | `EffectHasLivePermission`, `NoSeedAfterClear` | 기대 RED |
| [NoSupersedeCheck](HerdrRestartOwnerNoSupersedeCheck.cfg) | 923/미제공(그림상한) | `NoResumeAfterClear`, `NoSeedAfterClear`, `ClearForgets` | 기대 RED |
| [NoWake](HerdrRestartOwnerNoWake.cfg) | 35/83 | `RequestResolves`, `RestartAvailable`, `AcceptedInputResolves` | 기대 RED |
| [OldInput](HerdrRestartOwnerOldInput.cfg) | 61/150 | `NoOldPaneInput` | 기대 RED |
| [PrependOverHead](HerdrRestartOwnerPrependOverHead.cfg) | 11/25 | `RequestResolves`, `RestartAvailable`, `AcceptedInputResolves` | 기대 RED |
| [R1Launch](HerdrRestartOwnerR1Launch.cfg) | 273/814 | `EffectHasLivePermission`, `NoSeedAfterClear`, `NoSessionAfterClear`, `ClearForgets` | 기대 RED |
| [ReadyOnAdmit](HerdrRestartOwnerReadyOnAdmit.cfg) | 35/85 | `ReadyEvidence` | 기대 RED |
| [ReloadSeed](HerdrRestartOwnerReloadSeed.cfg) | 1107/미제공(그림상한) | `NoSeedRevival`, `NoResumeAfterClear`, `NoSeedAfterClear`, `ClearForgets` | 기대 RED |
| [ReplayInput](HerdrRestartOwnerReplayInput.cfg) | 73/214 | `NoOperationDuplicateSeed`, `InputAtMostOnce` | 기대 RED |
| [ReplayUnknown](HerdrRestartOwnerReplayUnknown.cfg) | 53/158 | `NoOperationSeedless`, `NoBlindCreate`, `SingleSession`, `EffectHasLivePermission`, `AcceptedInputResolves` | 기대 RED |
| [RequeueUncertain](HerdrRestartOwnerRequeueUncertain.cfg) | 95/231 | `SeedAtMostOnce` | 기대 RED |
| [RetryDispatch](HerdrRestartOwnerRetryDispatch.cfg) | 33/80 | `NoDispatchRepost` | 기대 RED |
| [SeedAbsentIsClear](HerdrRestartOwnerSeedAbsentIsClear.cfg) | 36/85 | `NoFalseSuperseded` | 기대 RED |
| [SeedBehind](HerdrRestartOwnerSeedBehind.cfg) | 93/231 | `NoOldPaneInput` | 기대 RED |
| [Seedless](HerdrRestartOwnerSeedless.cfg) | 106/295 | — | GREEN |
| [SeedlessClear](HerdrRestartOwnerSeedlessClear.cfg) | 1458/미제공(그림상한) | — | GREEN |
| [SeedlessCrash](HerdrRestartOwnerSeedlessCrash.cfg) | 214/638 | — | GREEN |
| [SkipClose](HerdrRestartOwnerSkipClose.cfg) | 33/80 | `SingleSession` | 기대 RED |
| [StartLegacy](HerdrRestartOwnerStartLegacy.cfg) | 25/60 | — | GREEN |
| [StartMissing](HerdrRestartOwnerStartMissing.cfg) | 25/60 | — | GREEN |
| [StartPendingPlaced](HerdrRestartOwnerStartPendingPlaced.cfg) | 83/207 | — | GREEN |
| [StartPendingUnplaced](HerdrRestartOwnerStartPendingUnplaced.cfg) | 26/63 | — | GREEN |
| [StartRetired](HerdrRestartOwnerStartRetired.cfg) | 25/60 | — | GREEN |
| [StartUnknown](HerdrRestartOwnerStartUnknown.cfg) | 5/10 | — | GREEN |
| [UnboundDispatch](HerdrRestartOwnerUnboundDispatch.cfg) | 106/295 | `NoInheritedDispatch` | 기대 RED |
| [WithdrawFallback](HerdrRestartOwnerWithdrawFallback.cfg) | 13/30 | — | GREEN |
| [WithdrawFirst](HerdrRestartOwnerWithdrawFirst.cfg) | 18/46 | `RequestResolves` | 기대 RED |

## 실제 경로 coverage·원인 control

최종 DOT5개는 위607 checker 실행과 별도다. coverage-final/coverage.json의 B_PORT_PRECONDITIONS_COVERAGE_OK:

- idle Bound172 states/repair edge2, idle PendingPlaced83 states/edge1. oldLive=false/inflight=false/crashed에서 Retired+wake로 감.
- same replay32개 중 terminal ticket6, conflict66개 중 terminal14. seed/row/creates/panes/oldLive/gate/excl/dispatch 불변, seedless/duplicate0.
- clear launch617 states: BeginEffect34·physical effect34. CheckEpoch 뒤 epoch 이동8개에서는 BeginEffect 금지. permission 보유 중 요청8개는 effect 뒤 epoch bump.

causal-controls/는 NoPermit/NoPermitDetach의 다른 조건을 그대로 두고 RecoveryPolicy만 missing→owned로 바꾼 control2개다. EndedSeedRecovers가 GREEN으로 돌아왔고, 이 control에서 다른 기존 규칙도 전부 PASS라고 주장하지 않는다.

반례 번호는 generated report를 따른다. 실제 생산 mutation/실재현이 아니며 B1/B2의 실제 진입·PG·원인 mutation 시험을 대체하지 않는다.

- [LeakLaunching: `ClearRequestCompletes`](herdr-restart-owner-LeakLaunching.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13 → 14 → 15 → 16. Clear discards the queue and the launching input: Clear discarded the queue; a finished launch never released its registration

- [NoLaunchCheck: `EffectHasLivePermission`](herdr-restart-owner-NoLaunchCheck.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10. Create once under live effect permission: Create answered; first input not submitted; physical effect used no live permission; original 1 waiting

- [NoPermit: `EffectHasLivePermission`](herdr-restart-owner-NoPermit.report.md): 0 → 1 → 2 → 3. Seed typed into the old execution: Greeting reached the execution the restart should end; physical effect used no live permission; original 1 waiting

- [NoPermit: `EndedSeedRecovers`](herdr-restart-owner-NoPermit.report.md): 0 → 1 → 2 → 3 → 4. Daemon dies and boots: Idle ended execution; the seed owner must repair the row

- [NoPermitDetach: `NoOperationSeedless`](herdr-restart-owner-NoPermitDetach.report.md): 0 → 1 → 2. Treat known operation as seedless capacity failure: Seed committed; gate holds it; known ticket entered the seedless path; original 1 waiting

- [NoPermitDetach: `EndedSeedRecovers`](herdr-restart-owner-NoPermitDetach.report.md): 0 → 1 → 2 → 3 → 4 → 5. Treat known operation as seedless capacity failure: Idle ended execution; the seed owner must repair the row; known ticket entered the seedless path; original 1 waiting

- [NoSubmitCheck: `EffectHasLivePermission`](herdr-restart-owner-NoSubmitCheck.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13. Submit the first input under live permission: First input submitted; receipt not committed; physical effect used no live permission; original 1 waiting

- [R1Launch: `EffectHasLivePermission`](herdr-restart-owner-R1Launch.report.md): 0 → 1 → 2 → 3 → 4 → 5. Launch the successor inside the exclusion: One new Ready session; inputs await the owner; physical effect used no live permission; original 1 waiting, original 2 not yet arrived

- [ReplayInput: `NoOperationDuplicateSeed`](herdr-restart-owner-ReplayInput.report.md): 0 → 1 → 2. Insert a duplicate operation seed: Seed committed; gate holds it; known operation inserted another seed; original 1 waiting

- [ReplayUnknown: `NoOperationSeedless`](herdr-restart-owner-ReplayUnknown.report.md): 0 → 1 → 2. Treat known operation as seedless capacity failure: Seed committed; gate holds it; known ticket entered the seedless path; original 1 waiting

- [ReplayUnknown: `EffectHasLivePermission`](herdr-restart-owner-ReplayUnknown.report.md): 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11. Create again after an unknown reply: Unknown create acknowledgement was replayed; physical effect used no live permission; original 1 waiting

## 남긴 계약과 A 경계

GapClearCloseFailure/GapClearCreateUnknown/GapSeedlessDispatchCrash의 기대 RED는 그대로다. owner Accepted/Launched는 Ready 증거가 아니며 receipt+source binding이 필요하다. named action/fairness 자체가 생산 guard는 아니다. reserved-source ingress 거절·actor 교체·실제 dispatch 경합·V1/V2/unknown schema 복원은 분할 계획의13개 실제 시험이 필요하다.

A immutable offset·Resume cause·ordinary fresh의23cfg/바이트는 변경하지 않았다. HerdrRestart.s의 r1 direct-launch는 역사적 투영이며 현재 B 구현/수용 정본은 이 Owner 모델이다.

## 재실행

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py --root /tmp/i8128-restart-owner-b --jobs 2
/Users/itismyfield/.codex/skills/statechart/scripts/statechart \
  /tmp/i8128-restart-owner-b/HerdrRestartOwner.tla \
  --config /tmp/i8128-restart-owner-b/HerdrRestartOwnerAfter.cfg \
  --view /tmp/i8128-restart-owner-b/herdr-restart-owner.view.mjs \
  --out /tmp/i8128-restart-owner-b-render-new
```

[정상](herdr-restart-owner-After.chart.mmd), [idle Pending crash](herdr-restart-owner-StartPendingPlaced.chart.mmd), [BeginEffect 제거](herdr-restart-owner-NoLaunchCheck.chart.mmd), [허가 조기반납](herdr-restart-owner-NoSubmitCheck.chart.mmd), [idle recovery 제거](herdr-restart-owner-NoPermitDetach.chart.mmd). expected.tsv의 기존63행을 강화해 동기화했다(신규행0·기존위반삭제0·예외추가0).
